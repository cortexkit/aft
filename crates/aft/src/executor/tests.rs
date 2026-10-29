use std::{
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    thread,
    time::{Duration, Instant},
};

use tempfile::TempDir;

use super::*;
use crate::{
    callgraph_store::{
        callgraph_refresh_worker_test_counts, clear_callgraph_refresh_worker_test_seam,
        flush_callgraph_store_refreshes_with_budget, set_callgraph_refresh_worker_test_seam,
        CallGraphStore,
    },
    config::Config,
    inspect::{InspectCache, InspectCacheError},
    parser::TreeSitterProvider,
    path_identity::ProjectRootId,
    protocol::Response,
};

fn ok(id: impl Into<String>) -> Response {
    Response::success(id, serde_json::json!({"ok": true}))
}

#[cfg(unix)]
#[test]
fn blocked_file_read_releases_root_reader_and_executor_slot() {
    let executor = test_executor(2, 1, 2, 1);
    let (_dir, root) = test_root("blocked-file");
    let external = tempfile::tempdir().unwrap();
    let path = external.path().join("blocked.txt");
    std::fs::write(&path, "content\n").unwrap();
    let fifo = external.path().join("consent-probe");
    crate::bounded_io::tests::fifo(&fifo);
    let probe = crate::bounded_io::tests::block_directory_probe(external.path(), &fifo);
    assert!(executor.register_actor(root.clone(), test_ctx()));
    let req = crate::protocol::RawRequest {
        id: "blocked-read".into(),
        command: "read".into(),
        lsp_hints: None,
        session_id: None,
        params: serde_json::json!({"file": path}),
    };
    let (started_tx, started_rx) = crossbeam_channel::bounded(1);
    let read = executor.submit_async(
        root.clone(),
        Lane::PureRead,
        req.id.clone(),
        Box::new(move |ctx| {
            started_tx.send(()).unwrap();
            crate::bounded_io::with_budget(
                Some(crate::bounded_io::Budget::after(Duration::from_millis(100))),
                || crate::commands::read::handle_read(&req, ctx),
            )
        }),
    );
    started_rx.recv_timeout(Duration::from_secs(2)).unwrap();
    let writer = executor.submit_async(
        root,
        Lane::Mutating,
        "after-blocked-read".into(),
        Box::new(|_| ok("writer")),
    );
    let (tx, rx) = crossbeam_channel::bounded(1);
    let waiter = thread::spawn(move || {
        tx.send((
            recv_async(read, "blocked read"),
            recv_async(writer, "writer"),
        ))
        .unwrap()
    });
    let before_writer = rx.recv_timeout(Duration::from_secs(1));
    crate::bounded_io::tests::release_fifo(&fifo);
    waiter.join().unwrap();
    let (response, writer_response) =
        before_writer.expect("root reader or executor slot stayed held by a blocked open");
    assert!(!response.success);
    assert_eq!(probe.fired.load(Ordering::Relaxed), 1);
    assert_eq!(response.data["code"], "read_blocked");
    assert!(writer_response.success);
}

fn test_ctx() -> Arc<AppContext> {
    Arc::new(AppContext::new(
        Box::new(TreeSitterProvider::new()),
        Config::default(),
    ))
}

fn test_root(label: &str) -> (TempDir, ProjectRootId) {
    let dir = tempfile::Builder::new()
        .prefix(&format!("aft-executor-{label}-"))
        .tempdir()
        .expect("create temp actor root");
    let root = ProjectRootId::from_path(dir.path()).expect("canonicalize actor root");
    (dir, root)
}

fn test_executor(
    pool_size: usize,
    read_cap: usize,
    actor_cap: usize,
    heavy_permits: usize,
) -> Executor {
    Executor::with_config(ExecutorConfig {
        pool_size,
        read_cap,
        actor_cap,
        heavy_permits,
        drr_quantum: 1,
    })
}

#[test]
fn lane_paused_six_cold_roots_leave_slots_and_maintenance_available() {
    use crate::semantic_index::{
        retry_lane_pause, with_lane_pause_clock, with_lane_pause_wait_hook, PauseRetryScope,
        SemanticIndex, SemanticIndexLock,
    };
    use crate::synapse_embed::{pause_test_support::*, LanePause, SynapseEmbeddingError};
    const ROOTS: usize = 6;
    let limiter = crate::cold_build_limiter::isolated_limiter(2);
    let storage = tempfile::tempdir().unwrap();
    let (entered_tx, entered_rx) = crossbeam_channel::unbounded();
    let resume = Arc::new((std::sync::Mutex::new(false), std::sync::Condvar::new()));
    let advanced = Arc::new(AtomicUsize::new(0));
    let writer_reached = Arc::new(AtomicUsize::new(0));
    let mut directories = Vec::new();
    let mut workers = Vec::new();
    for number in 0..ROOTS {
        let (directory, root) = test_root(&format!("paused-{number}"));
        let file = directory.path().join("lib.rs");
        std::fs::write(&file, "pub fn prepared() {}\n").unwrap();
        let key = format!("paused-{number}");
        crate::root_cache::configure_artifact_access(root.as_path(), &key, false);
        let storage = storage.path().to_path_buf();
        let limiter = limiter.clone();
        let entered = entered_tx.clone();
        let resume = resume.clone();
        let advanced = advanced.clone();
        let writer_reached = writer_reached.clone();
        directories.push((directory, key.clone()));
        workers.push(thread::spawn(move || {
            let _policy = PauseRetryScope::new(Duration::from_secs(15), Duration::from_secs(60), || true);
            with_lane_pause_clock(START, |clock| {
                let clock_wait = clock.clone();
                with_lane_pause_wait_hook(move |delay| {
                    entered.send(number).unwrap();
                    let (lock, wake) = &*resume;
                    let mut released = lock.lock().unwrap();
                    while !*released { released = wake.wait(released).unwrap(); }
                    clock_wait.fetch_add(delay.as_millis() as u64, Ordering::SeqCst);
                    advanced.fetch_add(1, Ordering::SeqCst);
                }, || retry_lane_pause(|| {
                    let _permit = crate::cold_build_limiter::acquire_blocking_while_for_root_with_limiter(
                        &limiter, "semantic build", root.as_path(), || true).unwrap();
                    let _writer =
                        SemanticIndexLock::acquire(&storage, &key, root.as_path()).unwrap();
                    if storage.join("semantic").join(&key).join("cache.lock").exists() {
                        writer_reached.fetch_add(1, Ordering::SeqCst);
                    }
                    SemanticIndex::build(root.as_path(), std::slice::from_ref(&file), &mut |texts: Vec<String>| {
                        if clock.load(Ordering::SeqCst) < START + PAUSE_MS {
                            Err(SynapseEmbeddingError::LanePaused(LanePause {
                                lane_id: MODEL.into(), fingerprint: Some("pinned".into()), retry_after_ms: Some(PAUSE_MS),
                                paused_until_ms: START + PAUSE_MS, reason: "maintenance".into(), cause: Default::default(), safe_to_retry_same_request: true, record_id: 0,
                            }).to_string())
                        } else { Ok(vec![vec![1.0, 0.0]; texts.len()]) }
                    }, 100)
                })).map(|index| index.len())
            })
        }));
    }
    // Each notification follows the attempt's permit and writer-lock drops.
    // Allow slow roots to arrive before probing resources; no pause is resumed yet.
    let pause_deadline = Instant::now() + Duration::from_secs(60);
    let mut paused = 0;
    while paused < ROOTS && entered_rx.recv_deadline(pause_deadline).is_ok() {
        paused += 1;
    }
    let writer_locks_free = directories
        .iter()
        .filter(|(_, key)| {
            let path = storage.path().join("semantic").join(key).join("cache.lock");
            crate::fs_lock::try_acquire(&path, Duration::ZERO).is_ok()
        })
        .count();
    let executor = test_executor(2, 1, 2, 1);
    let (_other_dir, other) = test_root("unrelated-during-pause");
    executor.register_actor(other.clone(), test_ctx());
    let admitted = Arc::new(AtomicUsize::new(0));
    let counted = admitted.clone();
    let other_limiter = limiter.clone();
    let unrelated = executor.submit_maintenance_async(
        other,
        Lane::HeavyInit,
        "unrelated-cold".into(),
        Box::new(move |_| {
            if let Some(_permit) = other_limiter.try_acquire() {
                counted.fetch_add(1, Ordering::SeqCst);
                ok("unrelated-cold")
            } else {
                Response::error(
                    "unrelated-cold",
                    "admission_blocked",
                    "all cold slots retained by paused roots",
                )
            }
        }),
    );
    let unrelated = recv_async(unrelated, "unrelated maintenance during pause");
    let before_advance = advanced.load(Ordering::SeqCst);
    let writer_attempts_before_advance = writer_reached.load(Ordering::SeqCst);
    {
        let (lock, wake) = &*resume;
        *lock.lock().unwrap() = true;
        wake.notify_all();
    }
    for worker in workers {
        assert!(worker.join().unwrap().unwrap() > 0);
    }
    assert_eq!(
        paused, ROOTS,
        "not all roots entered the pause before the deadline"
    );
    assert_eq!(writer_locks_free, ROOTS, "a pause retained a writer lock");
    assert_eq!(
        writer_attempts_before_advance, ROOTS,
        "the writer-lock control did not reach each real lock"
    );
    assert!(unrelated.success);
    assert_eq!(admitted.load(Ordering::SeqCst), 1);
    assert_eq!(
        before_advance, 0,
        "unrelated work only ran after virtual pause expiry"
    );
}

#[test]
fn lane_paused_executor_jobs_return_without_pinning_a_worker() {
    use crate::semantic_index::{retry_lane_pause, with_lane_pause_wait_hook, PauseRetryScope};
    use crate::synapse_embed::{pause_test_support::*, LanePause, SynapseEmbeddingError};
    let executor = test_executor(2, 1, 2, 1);
    let mut directories = Vec::new();
    let mut replies = Vec::new();
    for number in 0..6 {
        let (directory, root) = test_root(&format!("paused-executor-{number}"));
        executor.register_actor(root.clone(), test_ctx());
        directories.push(directory);
        replies.push(executor.submit_maintenance_async(root, Lane::HeavyInit, format!("paused-{number}"), Box::new(move |_| {
            let _policy = PauseRetryScope::new(Duration::from_secs(15), Duration::from_secs(60), || true);
            with_lane_pause_wait_hook(|_| panic!("executor worker attempted an announced-pause wait"), || {
                let result: Result<(), String> = retry_lane_pause(|| Err(SynapseEmbeddingError::LanePaused(LanePause {
                    lane_id: MODEL.into(), fingerprint: Some("pinned".into()), retry_after_ms: Some(PAUSE_MS),
                    paused_until_ms: START + PAUSE_MS, reason: "maintenance".into(), cause: Default::default(), safe_to_retry_same_request: true, record_id: 0,
                }).to_string()));
                Response::success(format!("paused-{number}"), serde_json::json!({"pause": LanePause::from_error(&result.unwrap_err()).is_some()}))
            })
        })));
    }
    for reply in replies {
        let response = recv_async(reply, "paused executor job");
        assert!(response.success, "{}", response.data);
        assert_eq!(response.data["pause"], true);
    }
    let (_other, root) = test_root("after-paused-executor");
    executor.register_actor(root.clone(), test_ctx());
    assert!(
        recv_async(
            executor.submit_maintenance_async(
                root,
                Lane::HeavyInit,
                "after-pause".into(),
                Box::new(|_| ok("after-pause"))
            ),
            "maintenance after pauses"
        )
        .success
    );
}

#[test]
fn last_executor_owner_can_drop_on_worker() {
    let owner = Arc::new(parking_lot::Mutex::new(Some(test_executor(2, 1, 1, 1))));
    let (_dir, root) = test_root("last-owner");
    owner
        .lock()
        .as_ref()
        .unwrap()
        .register_actor(root.clone(), test_ctx());
    let worker_owner = Arc::clone(&owner);
    let (finished_tx, finished_rx) = crossbeam_channel::bounded(1);
    let response = owner.lock().as_ref().unwrap().submit_async(
        root,
        Lane::PureRead,
        "last-owner".into(),
        Box::new(move |_| {
            let last = worker_owner.lock().take().unwrap();
            drop(last);
            finished_tx.send(()).unwrap();
            ok("last-owner")
        }),
    );
    finished_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("dropping on a worker must return without self-join");
    assert!(recv_async(response, "last owner drop").success);
}

fn observe_max(max_seen: &AtomicUsize, value: usize) {
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
fn recv_async(rx: tokio::sync::oneshot::Receiver<Response>, awaited: &str) -> Response {
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
fn scheduler_event_batch_leaves_excess_wakes_for_the_next_lock_turn() {
    let (event_tx, event_rx) = crossbeam_channel::unbounded();
    for _ in 0..=SCHEDULER_EVENT_BATCH_CAP {
        event_tx.send(SchedulerEvent::Wake).expect("queue wake");
    }
    let config = ExecutorConfig::default().effective();
    let mut state = SchedulerState::new(config);
    let completed_interactive = AtomicU64::new(0);
    let completed_maintenance = AtomicU64::new(0);
    let first = event_rx.recv().expect("first wake");

    assert!(!process_scheduler_event_batch(
        first,
        &event_rx,
        &mut state,
        &completed_interactive,
        &completed_maintenance,
    ));
    assert_eq!(event_rx.len(), 1);
}

#[test]
fn dispatch_loop_liveness_pending_survives_receive_handoff() {
    let (event_tx, event_rx) = crossbeam_channel::unbounded();
    let liveness = DispatchLoopLiveness::new(event_rx.clone());
    assert!(!liveness.has_pending_work());
    event_tx.send(SchedulerEvent::Wake).expect("queued wake");
    assert!(liveness.has_pending_work());

    assert!(liveness.wait_for_event(None));
    let _event = event_rx.recv().expect("receive wake");
    assert!(
        liveness.has_pending_work(),
        "dequeue must not hide work before the scheduler takes its state lock"
    );
    liveness.end_event();
    assert!(!liveness.has_pending_work());
}

#[test]
fn dispatch_loop_liveness_receiving_queued_work_advances_progress() {
    let (event_tx, event_rx) = crossbeam_channel::unbounded();
    let mut liveness = DispatchLoopLiveness::new(event_rx.clone());
    liveness.origin = Instant::now() - Duration::from_secs(1);
    event_tx.send(SchedulerEvent::Wake).expect("queued wake");
    assert!(liveness.has_pending_work());
    assert_eq!(liveness.last_progress_ms.load(Ordering::Relaxed), 0);

    // Receiving is progress, even if the scheduler next blocks on its mutex.
    // Age measured from observing a queued wake is not the blocked-loop age.
    assert!(liveness.wait_for_event(None));
    assert!(liveness.last_progress_ms.load(Ordering::Relaxed) >= 1000);
    let _event = event_rx.recv().expect("receive wake");
    assert!(liveness.has_pending_work());
    liveness.end_event();
    assert!(!liveness.has_pending_work());
}

#[test]
fn dispatch_loop_liveness_due_retry_is_pending_until_processed() {
    let (_event_tx, event_rx) = crossbeam_channel::unbounded();
    let liveness = DispatchLoopLiveness::new(event_rx);
    assert!(!liveness.has_pending_work());
    assert!(!liveness.wait_for_event(Some(Instant::now())));
    assert!(liveness.has_pending_work());
    liveness.end_event();
    assert!(!liveness.has_pending_work());
}

#[test]
fn dispatch_loop_liveness_is_pending_only_while_the_loop_owes_work() {
    let executor = test_executor(2, 1, 1, 2);
    let liveness = executor.dispatch_loop_liveness();
    assert!(
        !liveness.has_pending_work(),
        "an idle scheduler parked in recv() owes no work"
    );

    let (release_tx, release_rx) = crossbeam_channel::bounded(1);
    let holder = executor.hold_dispatch_loop_for_test(release_rx);
    let deadline = Instant::now() + Duration::from_secs(10);
    // Queued work is pending too, but its receipt advances the progress clock.
    // Wait for both the active marker and dequeue before measuring the
    // mutex-blocked interval; a spurious readiness wake owes no event.
    while !liveness.in_event.load(Ordering::Acquire) || !liveness.event_rx.is_empty() {
        assert!(
            Instant::now() < deadline,
            "held scheduler never read as pending"
        );
        thread::sleep(Duration::from_millis(5));
    }
    assert!(liveness.has_pending_work());
    // Age calculation does not require sleeping on the scheduler's clock.
    // Backdate an isolated clock so host timer resolution cannot shorten it.
    let (_events, events_rx) = crossbeam_channel::unbounded();
    let mut aged = DispatchLoopLiveness::new(events_rx);
    aged.origin -= Duration::from_secs(1);
    assert!(aged.progress_age() >= Duration::from_secs(1));

    release_tx.send(()).expect("release hold");
    holder.join().expect("hold thread");
    let deadline = Instant::now() + Duration::from_secs(10);
    while liveness.has_pending_work() {
        assert!(
            Instant::now() < deadline,
            "released scheduler stayed pending"
        );
        thread::sleep(Duration::from_millis(5));
    }
    assert!(!liveness.in_event.load(Ordering::Acquire));
}

#[test]
fn actor_contexts_returns_registered_contexts() {
    let executor = test_executor(2, 1, 1, 2);
    let (_dir_a, root_a) = test_root("contexts-a");
    let (_dir_b, root_b) = test_root("contexts-b");
    let ctx_a = test_ctx();
    let ctx_b = test_ctx();

    assert!(!Arc::ptr_eq(&ctx_a, &ctx_b));
    assert!(executor.register_actor(root_a, Arc::clone(&ctx_a)));
    assert!(executor.register_actor(root_b, Arc::clone(&ctx_b)));

    let contexts = executor.actor_contexts();

    assert_eq!(contexts.len(), 2);
    assert!(contexts.iter().any(|ctx| Arc::ptr_eq(ctx, &ctx_a)));
    assert!(contexts.iter().any(|ctx| Arc::ptr_eq(ctx, &ctx_b)));
}

#[test]
fn actor_entries_return_roots_and_contexts() {
    let executor = test_executor(2, 1, 1, 2);
    let (_dir_a, root_a) = test_root("entries-a");
    let (_dir_b, root_b) = test_root("entries-b");
    let ctx_a = test_ctx();
    let ctx_b = test_ctx();

    assert!(executor.register_actor(root_a.clone(), Arc::clone(&ctx_a)));
    assert!(executor.register_actor(root_b.clone(), Arc::clone(&ctx_b)));

    let entries = executor.actor_entries();

    assert_eq!(entries.len(), 2);
    assert!(entries
        .iter()
        .any(|(root, ctx)| root == &root_a && Arc::ptr_eq(ctx, &ctx_a)));
    assert!(entries
        .iter()
        .any(|(root, ctx)| root == &root_b && Arc::ptr_eq(ctx, &ctx_b)));
}

#[test]
fn parked_maintenance_root_stat_leaves_submit_and_scheduler_lock_available() {
    let (_dir, root) = test_root("parked-root-stat");
    let executor = Arc::new(test_executor(2, 2, 2, 1));
    executor.register_actor(root.clone(), test_ctx());
    let (parked_tx, parked_rx) = crossbeam_channel::bounded(1);
    let (release_tx, release_rx) = crossbeam_channel::bounded(1);
    *executor.inner.maintenance_root_check_hook.lock() = Some(Box::new(move || {
        parked_tx.send(()).unwrap();
        release_rx.recv().unwrap();
    }));

    let maintenance_executor = executor.clone();
    let maintenance_root = root.clone();
    let maintenance_thread = thread::spawn(move || {
        maintenance_executor.submit_maintenance_async(
            maintenance_root,
            Lane::HeavyInit,
            "parked-stat".into(),
            Box::new(|_| ok("parked-stat")),
        )
    });
    parked_rx.recv_timeout(Duration::from_secs(10)).unwrap();

    let (submitted_tx, submitted_rx) = crossbeam_channel::bounded(1);
    let interactive_executor = executor.clone();
    let interactive_thread = thread::spawn(move || {
        let response = interactive_executor.submit_async(
            root,
            Lane::PureRead,
            "interactive".into(),
            Box::new(|_| ok("interactive")),
        );
        submitted_tx.send(response).unwrap();
    });
    let (locked_tx, locked_rx) = crossbeam_channel::bounded(1);
    let lock_executor = executor.clone();
    let lock_thread = thread::spawn(move || {
        let _state = lock_executor.inner.state.lock();
        locked_tx.send(()).unwrap();
    });

    let interactive = submitted_rx.recv_timeout(Duration::from_secs(5));
    let lock_available = locked_rx.recv_timeout(Duration::from_secs(5));
    // Release and join before asserting so a regression cannot strand the test's threads.
    release_tx.send(()).unwrap();
    let maintenance = maintenance_thread.join().unwrap();
    interactive_thread.join().unwrap();
    lock_thread.join().unwrap();

    assert!(
        interactive.is_ok(),
        "another submit waited behind the parked maintenance root stat"
    );
    assert!(
        lock_available.is_ok(),
        "scheduler lock was held across the parked maintenance root stat"
    );
    assert!(recv_async(interactive.unwrap(), "interactive beside parked stat").success);
    assert!(recv_async(maintenance, "maintenance after stat release").success);
}

#[test]
fn parked_requeue_root_probe_leaves_submit_and_scheduler_lock_available() {
    let (dir, root) = test_root("parked-requeue-stat");
    let executor = Arc::new(test_executor(2, 2, 2, 1));
    executor.register_actor(root.clone(), test_ctx());
    // An in-flight epoch reader makes the bind start shared and request a requeue.
    let reader = StuckReader::start(&executor, &root, "requeue-reader", "read");
    let (parked_tx, parked_rx) = crossbeam_channel::bounded(1);
    let (release_tx, release_rx) = crossbeam_channel::bounded(1);
    let (bind, _token) =
        submit_repeatable_bind(&executor, &root, "subc-bind-parked-requeue", move |_| {
            assert!(
                request_exclusive_rerun(),
                "only the shared attempt should run"
            );
            let token = current_job_cancellation().expect("bind cancellation token");
            *token.inner.root_probe_barrier.lock() = Some((parked_tx.clone(), release_rx.clone()));
            ok("shared-attempt")
        });
    parked_rx.recv_timeout(Duration::from_secs(10)).unwrap();

    let (submitted_tx, submitted_rx) = crossbeam_channel::bounded(1);
    let interactive_executor = executor.clone();
    let interactive_thread = thread::spawn(move || {
        let response = interactive_executor.submit_async(
            root,
            Lane::PureRead,
            "interactive".into(),
            Box::new(|_| ok("interactive")),
        );
        submitted_tx.send(response).unwrap();
    });
    let (locked_tx, locked_rx) = crossbeam_channel::bounded(1);
    let lock_executor = executor.clone();
    let lock_thread = thread::spawn(move || {
        let _state = lock_executor.inner.state.lock();
        locked_tx.send(()).unwrap();
    });

    let interactive = submitted_rx.recv_timeout(Duration::from_secs(5));
    let lock_available = locked_rx.recv_timeout(Duration::from_secs(5));
    // Deletion while the probe is parked must still cancel the requeue.
    dir.close().unwrap();
    release_tx.send(()).unwrap();
    interactive_thread.join().unwrap();
    lock_thread.join().unwrap();
    let reader_response = reader.finish();

    assert!(
        interactive.is_ok(),
        "another submit waited behind the parked requeue root probe"
    );
    assert!(
        lock_available.is_ok(),
        "scheduler lock was held across the parked requeue root probe"
    );
    assert!(
        recv_async(
            interactive.unwrap(),
            "interactive beside parked requeue probe"
        )
        .success
    );
    let response = recv_async(bind, "requeue after root deletion");
    assert!(!response.success);
    assert_eq!(response.data["code"], "request_cancelled");
    assert!(reader_response.success);
}

#[test]
fn missing_maintenance_root_preserves_admission_refusal() {
    let (dir, root) = test_root("missing-maintenance-root");
    let executor = test_executor(2, 2, 2, 1);
    executor.register_actor(root.clone(), test_ctx());
    dir.close().unwrap();
    let response = executor.submit_maintenance_async(
        root,
        Lane::HeavyInit,
        "missing-root".into(),
        Box::new(|_| panic!("missing root admitted maintenance")),
    );
    let response = recv_async(response, "missing root refusal");
    assert!(!response.success);
    assert_eq!(response.data["code"], "maintenance_cancelled");
    assert_eq!(
        response.data["message"],
        "maintenance root is unbound or missing"
    );
}

#[test]
fn cancel_queued_maintenance_preserves_interactive_work_and_actor() {
    let executor = test_executor(1, 1, 1, 1);
    let (_dir, root) = test_root("cancel-maintenance");
    executor.register_actor(root.clone(), test_ctx());

    let (interactive_started_tx, interactive_started_rx) = crossbeam_channel::bounded(1);
    let (release_interactive_tx, release_interactive_rx) = crossbeam_channel::bounded(1);
    let interactive = executor.submit(
        root.clone(),
        Lane::Mutating,
        "interactive-blocker".to_string(),
        Box::new(move |_| {
            interactive_started_tx
                .send(())
                .expect("signal interactive start");
            release_interactive_rx
                .recv_timeout(Duration::from_secs(2))
                .expect("release interactive blocker");
            ok("interactive-blocker")
        }),
    );
    interactive_started_rx
        .recv_timeout(Duration::from_secs(2))
        .expect("interactive blocker starts");

    let executed = Arc::new(AtomicUsize::new(0));
    let mutating_executed = Arc::clone(&executed);
    let cancelled_mutating = executor.submit_maintenance_async(
        root.clone(),
        Lane::Mutating,
        "cancelled-mutating".to_string(),
        Box::new(move |_| {
            mutating_executed.fetch_add(1, Ordering::AcqRel);
            ok("cancelled-mutating")
        }),
    );
    let commit_executed = Arc::clone(&executed);
    let cancelled_commit = executor.submit_maintenance_async(
        root.clone(),
        Lane::MaintenanceCommit,
        "cancelled-commit".to_string(),
        Box::new(move |_| {
            commit_executed.fetch_add(1, Ordering::AcqRel);
            ok("cancelled-commit")
        }),
    );

    assert_eq!(executor.cancel_queued_maintenance(&root), 2);
    for response in [
        recv_async(
            cancelled_mutating,
            "cancelled mutating maintenance completion",
        ),
        recv_async(cancelled_commit, "cancelled commit maintenance completion"),
    ] {
        assert!(!response.success);
        assert_eq!(response.data["code"], "maintenance_cancelled");
    }
    assert_eq!(executed.load(Ordering::Acquire), 0);
    assert!(executor.actor_registered(&root));

    release_interactive_tx
        .send(())
        .expect("release interactive blocker");
    assert!(
        interactive
            .recv_timeout(Duration::from_secs(2))
            .unwrap()
            .success
    );

    let follow_up = executor.submit(
        root,
        Lane::PureRead,
        "interactive-follow-up".to_string(),
        Box::new(|_| ok("interactive-follow-up")),
    );
    assert!(
        follow_up
            .recv_timeout(Duration::from_secs(2))
            .unwrap()
            .success
    );
}

#[test]
fn identical_queued_watcher_drains_coalesce_and_settle_both_receivers() {
    let executor = test_executor(2, 1, 1, 1);
    let (_dir, root) = test_root("coalesce-watcher-drains");
    executor.register_actor(root.clone(), test_ctx());

    let (blocker_started_tx, blocker_started_rx) = crossbeam_channel::bounded(1);
    let (release_blocker_tx, release_blocker_rx) = crossbeam_channel::bounded(1);
    let blocker = executor.submit_maintenance_async(
        root.clone(),
        Lane::MaintenanceCommit,
        "maintenance-blocker".to_string(),
        Box::new(move |_| {
            blocker_started_tx.send(()).expect("signal blocker start");
            release_blocker_rx
                .recv_timeout(Duration::from_secs(2))
                .expect("release maintenance blocker");
            ok("maintenance-blocker")
        }),
    );
    blocker_started_rx
        .recv_timeout(Duration::from_secs(2))
        .expect("maintenance blocker starts");

    let executed = Arc::new(AtomicUsize::new(0));
    let first_executed = Arc::clone(&executed);
    let first = executor.submit_coalescable_maintenance_async(
        root.clone(),
        Lane::MaintenanceCommit,
        "watcher-drain-first".to_string(),
        MaintenanceCoalesceKey::WatcherDrain,
        Box::new(move |_| {
            first_executed.fetch_add(1, Ordering::AcqRel);
            ok("watcher-drain-first")
        }),
    );
    let second_executed = Arc::clone(&executed);
    let second = executor.submit_coalescable_maintenance_async(
        root,
        Lane::MaintenanceCommit,
        "watcher-drain-second".to_string(),
        MaintenanceCoalesceKey::WatcherDrain,
        Box::new(move |_| {
            second_executed.fetch_add(1, Ordering::AcqRel);
            ok("watcher-drain-second")
        }),
    );

    release_blocker_tx
        .send(())
        .expect("release maintenance blocker");
    assert!(recv_async(blocker, "maintenance blocker completion").success);
    let first_response = recv_async(first, "first watcher drain completion");
    let second_response = recv_async(second, "coalesced watcher drain completion");

    assert_eq!(
        executed.load(Ordering::Acquire),
        1,
        "only the queued drain that owns the coalesced work may execute"
    );
    assert!(first_response.success);
    assert!(!second_response.success);
    assert_eq!(second_response.data["code"], "maintenance_cancelled");
}

#[test]
fn maintenance_queue_cap_returns_typed_backpressure_without_silent_loss() {
    let executor = test_executor(2, 1, 1, 1);
    let (_dir, root) = test_root("maintenance-queue-cap");
    executor.register_actor(root.clone(), test_ctx());

    let (blocker_started_tx, blocker_started_rx) = crossbeam_channel::bounded(1);
    let (release_blocker_tx, release_blocker_rx) = crossbeam_channel::bounded(1);
    let blocker = executor.submit_maintenance_async(
        root.clone(),
        Lane::MaintenanceCommit,
        "maintenance-cap-blocker".to_string(),
        Box::new(move |_| {
            blocker_started_tx.send(()).expect("signal blocker start");
            release_blocker_rx
                .recv_timeout(Duration::from_secs(5))
                .expect("release maintenance cap blocker");
            ok("maintenance-cap-blocker")
        }),
    );
    blocker_started_rx
        .recv_timeout(Duration::from_secs(2))
        .expect("maintenance cap blocker starts");

    let executed = Arc::new(AtomicUsize::new(0));
    let mut admitted = Vec::with_capacity(MAINTENANCE_QUEUE_CAP);
    for index in 0..MAINTENANCE_QUEUE_CAP {
        let executed = Arc::clone(&executed);
        admitted.push(executor.submit_maintenance_async(
            root.clone(),
            Lane::MaintenanceCommit,
            format!("maintenance-cap-admitted-{index}"),
            Box::new(move |_| {
                executed.fetch_add(1, Ordering::AcqRel);
                ok(format!("maintenance-cap-admitted-{index}"))
            }),
        ));
    }
    let overflow_executed = Arc::clone(&executed);
    let overflow = executor.submit_maintenance_async(
        root,
        Lane::MaintenanceCommit,
        "maintenance-cap-overflow".to_string(),
        Box::new(move |_| {
            overflow_executed.fetch_add(1, Ordering::AcqRel);
            ok("maintenance-cap-overflow")
        }),
    );

    let overflow_response = recv_async(overflow, "maintenance backpressure completion");
    assert!(!overflow_response.success);
    assert_eq!(overflow_response.data["code"], "maintenance_backpressure");
    assert_eq!(
        overflow_response.data["queue_cap"],
        serde_json::json!(MAINTENANCE_QUEUE_CAP)
    );
    assert_eq!(executed.load(Ordering::Acquire), 0);

    release_blocker_tx
        .send(())
        .expect("release maintenance cap blocker");
    assert!(recv_async(blocker, "maintenance cap blocker completion").success);
    for receiver in admitted {
        assert!(recv_async(receiver, "admitted maintenance completion").success);
    }
    assert_eq!(
        executed.load(Ordering::Acquire),
        MAINTENANCE_QUEUE_CAP,
        "every admitted non-coalescable job must execute exactly once"
    );
}

#[test]
fn cross_actor_isolation() {
    let executor = test_executor(4, 2, 3, 2);
    let (_dir_a, root_a) = test_root("isolation-a");
    let (_dir_b, root_b) = test_root("isolation-b");
    executor.register_actor(root_a.clone(), test_ctx());
    executor.register_actor(root_b.clone(), test_ctx());

    let (a_started_tx, a_started_rx) = crossbeam_channel::bounded(1);
    let (release_a_tx, release_a_rx) = crossbeam_channel::bounded(1);
    let a_done = Arc::new(AtomicUsize::new(0));
    let a_done_job = Arc::clone(&a_done);

    let a_handle = executor.submit(
        root_a,
        Lane::HeavyInit,
        "test-request-0".to_string(),
        Box::new(move |_| {
            a_started_tx.send(()).expect("signal heavy start");
            release_a_rx
                .recv_timeout(Duration::from_secs(2))
                .expect("release heavy actor");
            a_done_job.store(1, Ordering::Release);
            ok("heavy-a")
        }),
    );
    a_started_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("actor A heavy job starts");

    let (b_done_tx, b_done_rx) = crossbeam_channel::bounded(1);
    let b_handle = executor.submit(
        root_b,
        Lane::PureRead,
        "test-request-1".to_string(),
        Box::new(move |_| {
            b_done_tx.send(()).expect("signal B read done");
            ok("read-b")
        }),
    );

    b_done_rx
        .recv_timeout(Duration::from_millis(300))
        .expect("actor B read completes while actor A heavy job is still running");
    assert_eq!(a_done.load(Ordering::Acquire), 0);
    b_handle
        .recv_timeout(Duration::from_secs(1))
        .expect("B completion response");

    release_a_tx.send(()).expect("release actor A heavy job");
    a_handle
        .recv_timeout(Duration::from_secs(1))
        .expect("A completion response");
}

#[test]
fn within_actor_read_concurrency() {
    let executor = test_executor(4, 2, 3, 2);
    let (_dir, root) = test_root("read-concurrency");
    executor.register_actor(root.clone(), test_ctx());

    let read_count = 6;
    let current_reads = Arc::new(AtomicUsize::new(0));
    let max_reads = Arc::new(AtomicUsize::new(0));
    let (started_tx, started_rx) = crossbeam_channel::bounded(read_count);
    let (release_tx, release_rx) = crossbeam_channel::bounded(read_count);
    let mut handles = Vec::new();

    for index in 0..read_count {
        let current_reads = Arc::clone(&current_reads);
        let max_reads = Arc::clone(&max_reads);
        let started_tx = started_tx.clone();
        let release_rx = release_rx.clone();
        handles.push(executor.submit(
            root.clone(),
            Lane::PureRead,
            "test-request-2".to_string(),
            Box::new(move |_| {
                let now = current_reads.fetch_add(1, Ordering::AcqRel) + 1;
                observe_max(&max_reads, now);
                started_tx.send(index).expect("signal read start");
                release_rx.recv().expect("release read job");
                current_reads.fetch_sub(1, Ordering::AcqRel);
                ok(format!("read-{index}"))
            }),
        ));
    }

    for _ in 0..executor.read_cap() {
        started_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("initial read admitted up to cap");
    }
    // Exercise admission while the first readers are held. A quiet receive
    // window alone cannot prove the scheduler considered the remaining reads.
    {
        let mut state = executor.inner.state.lock();
        let actor = state.actors.get_mut(&root).unwrap();
        assert_eq!(actor.read_inflight, executor.read_cap());
        assert!(actor.has_queued_jobs());
        assert!(
            try_admit_actor(
                &root,
                actor,
                AdmissionPass::Class(JobClass::Interactive),
                &executor.inner.config,
                &Arc::new(HeavySemaphore::new(executor.heavy_permits())),
                None,
            )
            .is_none(),
            "a queued read must be refused while the read cap is occupied"
        );
    }

    for _ in 0..read_count {
        release_tx.send(()).expect("release read token");
    }
    for handle in handles {
        handle
            .recv_timeout(Duration::from_secs(1))
            .expect("read completion response");
    }

    assert_eq!(max_reads.load(Ordering::Acquire), executor.read_cap());
}

#[test]
fn drr_fairness() {
    let executor = test_executor(4, 3, 3, 2);
    let (_dir_a, root_a) = test_root("drr-a");
    let (_dir_b, root_b) = test_root("drr-b");
    executor.register_actor(root_a.clone(), test_ctx());
    executor.register_actor(root_b.clone(), test_ctx());

    let flood_count = 20;
    let (a_started_tx, a_started_rx) = crossbeam_channel::bounded(flood_count);
    let (release_a_tx, release_a_rx) = crossbeam_channel::bounded(flood_count);
    let mut a_handles = Vec::new();

    for index in 0..flood_count {
        let a_started_tx = a_started_tx.clone();
        let release_a_rx = release_a_rx.clone();
        a_handles.push(executor.submit(
            root_a.clone(),
            Lane::PureRead,
            "test-request-3".to_string(),
            Box::new(move |_| {
                a_started_tx.send(index).expect("signal A flood start");
                release_a_rx
                    .recv_timeout(Duration::from_secs(2))
                    .expect("release A flood job");
                ok(format!("a-{index}"))
            }),
        ));
    }

    for _ in 0..executor.actor_cap() {
        a_started_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("actor A fills only its actor cap");
    }

    let (b_started_tx, b_started_rx) = crossbeam_channel::bounded(1);
    let b_handle = executor.submit(
        root_b,
        Lane::PureRead,
        "test-request-4".to_string(),
        Box::new(move |_| {
            b_started_tx.send(()).expect("signal B start");
            ok("b")
        }),
    );

    b_started_rx
        .recv_timeout(Duration::from_millis(300))
        .expect("actor B is scheduled within a bounded DRR round despite A flood");
    b_handle
        .recv_timeout(Duration::from_secs(1))
        .expect("B completion response");

    for _ in 0..flood_count {
        release_a_tx.send(()).expect("release A flood token");
    }
    for handle in a_handles {
        handle
            .recv_timeout(Duration::from_secs(1))
            .expect("A completion response");
    }
}

#[test]
fn heavy_bound() {
    let executor = test_executor(6, 3, 5, 2);
    let job_count = 6;
    let mut roots = Vec::new();
    let mut dirs = Vec::new();
    for index in 0..job_count {
        let (dir, root) = test_root(&format!("heavy-{index}"));
        executor.register_actor(root.clone(), test_ctx());
        dirs.push(dir);
        roots.push(root);
    }

    let current_heavy = Arc::new(AtomicUsize::new(0));
    let max_heavy = Arc::new(AtomicUsize::new(0));
    let (started_tx, started_rx) = crossbeam_channel::bounded(job_count);
    let (release_tx, release_rx) = crossbeam_channel::bounded(job_count);
    let mut handles = Vec::new();

    for (index, root) in roots.into_iter().enumerate() {
        let current_heavy = Arc::clone(&current_heavy);
        let max_heavy = Arc::clone(&max_heavy);
        let started_tx = started_tx.clone();
        let release_rx = release_rx.clone();
        handles.push(executor.submit(
            root,
            Lane::HeavyInit,
            "test-request-5".to_string(),
            Box::new(move |_| {
                let now = current_heavy.fetch_add(1, Ordering::AcqRel) + 1;
                observe_max(&max_heavy, now);
                started_tx.send(index).expect("signal heavy start");
                release_rx.recv().expect("release heavy job");
                current_heavy.fetch_sub(1, Ordering::AcqRel);
                ok(format!("heavy-{index}"))
            }),
        ));
    }

    for _ in 0..executor.heavy_permits() {
        started_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("heavy job admitted up to semaphore bound");
    }
    // Exhaust a controlled semaphore and ask the real admission function about
    // an already-queued job; worker scheduling speed is not the oracle.
    {
        let heavy = Arc::new(HeavySemaphore::new(executor.heavy_permits()));
        let _permits: Vec<_> = (0..executor.heavy_permits())
            .map(|_| heavy.try_acquire().unwrap())
            .collect();
        let mut state = executor.inner.state.lock();
        let (root, actor) = state
            .actors
            .iter_mut()
            .find(|(_, actor)| actor.has_queued_jobs())
            .unwrap();
        assert!(
            try_admit_actor(
                root,
                actor,
                AdmissionPass::Class(JobClass::Interactive),
                &executor.inner.config,
                &heavy,
                None,
            )
            .is_none(),
            "a queued heavy job must be refused without a heavy permit"
        );
    }

    for _ in 0..job_count {
        release_tx.send(()).expect("release heavy token");
    }
    for handle in handles {
        handle
            .recv_timeout(Duration::from_secs(1))
            .expect("heavy completion response");
    }

    assert_eq!(max_heavy.load(Ordering::Acquire), executor.heavy_permits());
    assert_eq!(dirs.len(), job_count);
}

#[test]
fn heavy_init_storm_leaves_a_worker_for_a_fresh_route_bind() {
    let executor = test_executor(2, 1, 1, 2);
    assert_eq!(
        executor.heavy_permits(),
        1,
        "HeavyInit must leave a worker available for RouteBind/configure"
    );

    let mut dirs = Vec::new();
    let mut roots = Vec::new();
    for label in ["heavy-a", "heavy-b", "fresh-bind"] {
        let (dir, root) = test_root(label);
        executor.register_actor(root.clone(), test_ctx());
        dirs.push(dir);
        roots.push(root);
    }

    let (heavy_started_tx, heavy_started_rx) = crossbeam_channel::bounded(2);
    let (release_heavy_tx, release_heavy_rx) = crossbeam_channel::bounded(2);
    let mut heavy_jobs = Vec::new();
    for (index, root) in roots[..2].iter().cloned().enumerate() {
        let started_tx = heavy_started_tx.clone();
        let release_rx = release_heavy_rx.clone();
        heavy_jobs.push(executor.submit(
            root,
            Lane::HeavyInit,
            format!("heavy-storm-{index}"),
            Box::new(move |_| {
                started_tx.send(index).expect("signal injected heavy delay");
                release_rx.recv().expect("release injected heavy delay");
                ok(format!("heavy-storm-{index}"))
            }),
        ));
    }
    heavy_started_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("one HeavyInit job starts");
    // The fresh bind's start below is positive evidence of a spare worker:
    // neither heavy job can finish until this test releases its channel.

    let (bind_started_tx, bind_started_rx) = crossbeam_channel::bounded(1);
    let bind = executor.submit(
        roots[2].clone(),
        Lane::Mutating,
        "subc-bind-fresh-root".to_string(),
        Box::new(move |_| {
            bind_started_tx
                .send(())
                .expect("signal fresh RouteBind start");
            ok("fresh-route-bind")
        }),
    );
    bind_started_rx
        .recv_timeout(Duration::from_millis(300))
        .expect("fresh RouteBind starts during the HeavyInit storm");
    bind.recv_timeout(Duration::from_secs(1))
        .expect("fresh RouteBind acknowledgement");

    for _ in 0..heavy_jobs.len() {
        release_heavy_tx
            .send(())
            .expect("release injected heavy delay");
    }
    for heavy in heavy_jobs {
        heavy
            .recv_timeout(Duration::from_secs(1))
            .expect("HeavyInit completion response");
    }
    assert_eq!(executor.nonrunnable_dispatch_count(), 0);
    assert_eq!(dirs.len(), 3);
}

#[test]
fn bind_blocker_snapshot_attributes_queue_reader_maintenance_and_worker_pressure() {
    let executor = test_executor(2, 1, 1, 2);
    let (_reader_dir, reader_root) = test_root("blocker-reader");
    executor.register_actor(reader_root.clone(), test_ctx());
    let (reader_started_tx, reader_started_rx) = crossbeam_channel::bounded(1);
    let (release_reader_tx, release_reader_rx) = crossbeam_channel::bounded(1);
    let reader = executor.submit(
        reader_root.clone(),
        Lane::PureRead,
        "reader".to_string(),
        Box::new(move |_| {
            reader_started_tx.send(()).expect("reader starts");
            release_reader_rx.recv().expect("release reader");
            ok("reader")
        }),
    );
    reader_started_rx
        .recv_timeout(Duration::from_secs(12))
        .expect("reader starts before queued configure jobs");
    let first_bind = executor.submit(
        reader_root.clone(),
        Lane::Mutating,
        "subc-bind-first".to_string(),
        Box::new(|_| ok("first-bind")),
    );
    let second_bind = executor.submit(
        reader_root.clone(),
        Lane::Mutating,
        "subc-bind-second".to_string(),
        Box::new(|_| ok("second-bind")),
    );
    // The snapshot API is try-lock-only and may lose repeatedly to the dispatcher's
    // scheduler windows under load. Poll its observable result while the channel barrier
    // keeps the reader in place, then settle every worker before making assertions.
    let deadline = Instant::now() + Duration::from_secs(12);
    let reader_snapshot = loop {
        if let Some(snapshot) = executor.try_bind_blocker_snapshot(&reader_root, "subc-bind-second")
        {
            break Some(snapshot);
        }
        if Instant::now() >= deadline {
            break None;
        }
        std::thread::sleep(Duration::from_millis(5));
    };

    release_reader_tx.send(()).expect("release reader");
    reader
        .recv_timeout(Duration::from_secs(12))
        .expect("reader completion");
    first_bind
        .recv_timeout(Duration::from_secs(12))
        .expect("first configure completion");
    second_bind
        .recv_timeout(Duration::from_secs(12))
        .expect("second configure completion");

    let reader_snapshot = reader_snapshot.expect("bind blocker snapshot within 12s");
    assert_eq!(reader_snapshot.configure_state, "queued");
    assert!(reader_snapshot
        .blockers
        .iter()
        .any(|blocker| blocker == "queued_behind_configure(2)"));
    assert!(reader_snapshot
        .blockers
        .iter()
        .any(|blocker| blocker == "waiting_on_readers"));

    let executor = test_executor(2, 1, 1, 2);
    let (_maintenance_dir, maintenance_root) = test_root("blocker-maintenance");
    let (_occupied_dir, occupied_root) = test_root("blocker-occupied");
    let (_target_dir, target_root) = test_root("blocker-target");
    for root in [&maintenance_root, &occupied_root, &target_root] {
        executor.register_actor(root.clone(), test_ctx());
    }
    let (maintenance_started_tx, maintenance_started_rx) = crossbeam_channel::bounded(1);
    let (release_maintenance_tx, release_maintenance_rx) = crossbeam_channel::bounded(1);
    let maintenance = executor.submit_maintenance_async(
        maintenance_root,
        Lane::Mutating,
        "subc-maintenance-drain-watcher".to_string(),
        Box::new(move |_| {
            maintenance_started_tx.send(()).expect("maintenance starts");
            release_maintenance_rx.recv().expect("release maintenance");
            ok("maintenance")
        }),
    );
    maintenance_started_rx
        .recv_timeout(Duration::from_secs(12))
        .expect("maintenance starts");
    let (occupied_started_tx, occupied_started_rx) = crossbeam_channel::bounded(1);
    let (release_occupied_tx, release_occupied_rx) = crossbeam_channel::bounded(1);
    let occupied = executor.submit(
        occupied_root,
        Lane::PureRead,
        "occupied-worker".to_string(),
        Box::new(move |_| {
            occupied_started_tx.send(()).expect("occupied read starts");
            release_occupied_rx.recv().expect("release occupied read");
            ok("occupied")
        }),
    );
    occupied_started_rx
        .recv_timeout(Duration::from_secs(12))
        .expect("second worker starts");
    // Hold both bind and metadata reservations on other roots as well, so the
    // target bind's diagnostic really observes every worker occupied.
    let (reserve_started_tx, reserve_started_rx) = crossbeam_channel::unbounded();
    let (release_reserve_tx, release_reserve_rx) = crossbeam_channel::unbounded::<()>();
    let mut reserve_jobs = Vec::new();
    let mut reserve_dirs = Vec::new();
    let reserved_workers = BIND_RESERVE_WORKERS + PROVIDER_RESERVE_WORKERS;
    for index in 0..reserved_workers {
        let (dir, root) = test_root(&format!("blocker-reserve-{index}"));
        executor.register_actor(root.clone(), test_ctx());
        reserve_dirs.push(dir);
        let started = reserve_started_tx.clone();
        let release = release_reserve_rx.clone();
        let (lane, request_id) = if index < BIND_RESERVE_WORKERS {
            (Lane::Mutating, format!("subc-bind-reserve-{index}"))
        } else {
            (Lane::ProviderMetadata, format!("metadata-reserve-{index}"))
        };
        reserve_jobs.push(executor.submit(
            root,
            lane,
            request_id,
            Box::new(move |_| {
                started.send(()).expect("reserve bind starts");
                release.recv().expect("release reserve bind");
                ok("reserve-bind")
            }),
        ));
    }
    for _ in 0..reserved_workers {
        reserve_started_rx
            .recv_timeout(Duration::from_secs(12))
            .expect("reserved bind worker starts");
    }
    let target_bind = executor.submit(
        target_root.clone(),
        Lane::Mutating,
        "subc-bind-target".to_string(),
        Box::new(|_| ok("target-bind")),
    );
    // Both workers stay behind explicit release barriers while this nonblocking API
    // waits for an observable scheduler-lock opening; elapsed execution speed cannot
    // erase the pressure state the snapshot is meant to classify.
    let deadline = Instant::now() + Duration::from_secs(12);
    let pressure_snapshot = loop {
        if let Some(snapshot) = executor.try_bind_blocker_snapshot(&target_root, "subc-bind-target")
        {
            break Some(snapshot);
        }
        if Instant::now() >= deadline {
            break None;
        }
        std::thread::sleep(Duration::from_millis(5));
    };

    release_maintenance_tx
        .send(())
        .expect("release maintenance");
    release_occupied_tx.send(()).expect("release occupied read");
    for _ in 0..reserved_workers {
        release_reserve_tx.send(()).expect("release reserve bind");
    }
    for reserve_bind in reserve_jobs {
        reserve_bind
            .recv_timeout(Duration::from_secs(12))
            .expect("reserve bind completion");
    }
    assert_eq!(reserve_dirs.len(), reserved_workers);
    assert!(recv_async(maintenance, "maintenance completion").success);
    occupied
        .recv_timeout(Duration::from_secs(12))
        .expect("occupied completion");
    target_bind
        .recv_timeout(Duration::from_secs(12))
        .expect("target bind completion");

    let pressure_snapshot = pressure_snapshot.expect("nonblocking pressure snapshot within 12s");
    assert_eq!(pressure_snapshot.configure_state, "queued");
    assert!(pressure_snapshot
        .blockers
        .iter()
        .any(|blocker| blocker.starts_with("queued_behind_maintenance(")));
    assert!(pressure_snapshot
        .blockers
        .iter()
        .any(|blocker| blocker.starts_with("idle_workers==0(")));
}

#[test]
fn bind_blocker_snapshot_names_a_stuck_reader_occupant() {
    let executor = test_executor(1, 1, 1, 1);
    let (_dir, root) = test_root("stuck-reader-blocker");
    executor.register_actor(root.clone(), test_ctx());
    let (reader_started_tx, reader_started_rx) = crossbeam_channel::bounded(1);
    let (release_reader_tx, release_reader_rx) = crossbeam_channel::bounded(1);
    let reader = executor.submit(
        root.clone(),
        Lane::SerialLspStatus,
        "abandoned-inspect".to_string(),
        Box::new(move |_| {
            reader_started_tx.send(()).expect("inspect starts");
            release_reader_rx.recv().expect("release inspect");
            ok("inspect")
        }),
    );
    reader_started_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("inspect enters the LSP lane");
    {
        let mut state = executor.inner.state.lock();
        let running = state
            .running_jobs
            .values_mut()
            .find(|job| job.root_id == root && job.request_id == "abandoned-inspect")
            .expect("running inspect census entry");
        running.started_at = Instant::now() - READER_STUCK_CENSUS_AGE - Duration::from_secs(1);
    }
    let bind = executor.submit(
        root.clone(),
        Lane::Mutating,
        "subc-bind-after-inspect".to_string(),
        Box::new(|_| ok("bind")),
    );

    let deadline = Instant::now() + Duration::from_secs(1);
    let snapshot = loop {
        if let Some(snapshot) = executor.try_bind_blocker_snapshot(&root, "subc-bind-after-inspect")
        {
            break snapshot;
        }
        assert!(Instant::now() < deadline, "blocker snapshot timed out");
        std::thread::sleep(Duration::from_millis(5));
    };
    let blocker = snapshot
        .blockers
        .iter()
        .find(|blocker| blocker.starts_with("waiting_on_readers_stuck("))
        .expect("stuck-reader blocker");
    assert!(blocker.contains("job=abandoned-inspect"));
    assert!(blocker.contains("lane=SerialLspStatus"));
    assert!(blocker.contains("age_ms="));
    assert_eq!(snapshot.in_flight_readers.len(), 1);
    assert_eq!(
        snapshot.in_flight_readers[0].request_id,
        "abandoned-inspect"
    );
    assert!(snapshot.in_flight_readers[0].started_age_ms >= 60_000);
    assert!(snapshot.in_flight_readers[0].execution_started);
    assert!(snapshot.in_flight_readers[0].started_before_oldest_writer);
    assert!(snapshot.oldest_queued_writer_age_ms.is_some());
    assert_eq!(
        snapshot.reader_admissions_while_promoted_writer_waited, 0,
        "writer promotion must stop new reader admissions"
    );

    release_reader_tx.send(()).expect("release inspect");
    reader
        .recv_timeout(Duration::from_secs(1))
        .expect("inspect completes");
    bind.recv_timeout(Duration::from_secs(1))
        .expect("bind proceeds after the reader releases");
}

#[test]
fn held_inspect_writer_lease_times_out_and_queued_bind_proceeds() {
    let executor = test_executor(2, 1, 1, 1);
    let (_dir, root) = test_root("held-inspect-writer-lease");
    executor.register_actor(root.clone(), test_ctx());
    let project_root = root.as_path().to_path_buf();
    let inspect_dir = project_root.join("inspect-cache");
    let project_key = crate::path_identity::project_scope_key(&project_root);
    crate::root_cache::configure_artifact_access(&project_root, "shared", false);
    let project_dir = inspect_dir.join(&project_key);
    std::fs::create_dir_all(&project_dir).expect("create project inspect directory");
    let held = crate::fs_lock::try_acquire(
        &crate::root_cache::writer_lease_path(&project_dir),
        Duration::ZERO,
    )
    .expect("hold inspect writer lease");

    let (inspect_started_tx, inspect_started_rx) = crossbeam_channel::bounded(1);
    let (inspect_release_tx, inspect_release_rx) = crossbeam_channel::bounded(1);
    let inspect = executor.submit(
        root.clone(),
        Lane::SerialLspStatus,
        "inspect-held-writer-lease".to_string(),
        Box::new(move |_| {
            inspect_started_tx.send(()).expect("inspect starts");
            inspect_release_rx
                .recv_timeout(Duration::from_secs(30))
                .expect("bind is queued before the lease attempt");
            match InspectCache::open(inspect_dir, project_root) {
                Err(InspectCacheError::WriterLeaseTimeout) => Response::error(
                    "inspect-held-writer-lease",
                    "writer_lease_timeout",
                    "inspect writer lease deadline elapsed",
                ),
                Err(error) => panic!("unexpected inspect cache error: {error}"),
                Ok(_) => panic!("contended inspect cache unexpectedly opened"),
            }
        }),
    );
    inspect_started_rx
        .recv_timeout(Duration::from_secs(30))
        .expect("inspect starts while lease is held");

    let (bind_started_tx, bind_started_rx) = crossbeam_channel::bounded(1);
    let bind = executor.submit(
        root,
        Lane::Mutating,
        "subc-bind-after-writer-timeout".to_string(),
        Box::new(move |_| {
            bind_started_tx.send(()).expect("bind starts");
            ok("bind")
        }),
    );
    assert!(
        matches!(
            bind_started_rx.try_recv(),
            Err(crossbeam_channel::TryRecvError::Empty)
        ),
        "bind must wait while inspect still occupies the LSP lane"
    );
    inspect_release_tx
        .send(())
        .expect("start the lease attempt");

    let inspect_response = inspect
        .recv_timeout(Duration::from_secs(30))
        .expect("inspect reaches its writer lease deadline");
    assert!(!inspect_response.success);
    assert_eq!(inspect_response.data["code"], "writer_lease_timeout");
    bind_started_rx
        .recv_timeout(Duration::from_secs(30))
        .expect("bind starts after inspect reports the timeout");
    bind.recv_timeout(Duration::from_secs(30))
        .expect("bind completes while competing lease remains held");
    drop(held);
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
fn worker_panic_completes_keeps_capacity_and_marks_mutating_actor_fatal() {
    let executor = test_executor(2, 1, 1, 2);
    let (_block_dir, block_root) = test_root("panic-blocker");
    let (_panic_dir, panic_root) = test_root("panic-mutating");
    let (_other_dir, other_root) = test_root("panic-other");
    executor.register_actor(block_root.clone(), test_ctx());
    executor.register_actor(panic_root.clone(), test_ctx());
    executor.register_actor(other_root.clone(), test_ctx());

    let (block_started_tx, block_started_rx) = crossbeam_channel::bounded(1);
    let (release_block_tx, release_block_rx) = crossbeam_channel::bounded(1);
    let block_handle = executor.submit(
        block_root,
        Lane::PureRead,
        "test-request-6".to_string(),
        Box::new(move |_| {
            block_started_tx.send(()).expect("signal blocker start");
            release_block_rx
                .recv_timeout(Duration::from_secs(5))
                .expect("release blocker");
            ok("blocker")
        }),
    );
    block_started_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("blocker starts");

    let panic_handle = executor.submit(
        panic_root.clone(),
        Lane::Mutating,
        "test-request-7".to_string(),
        Box::new(|_| panic!("mutating panic sentinel")),
    );
    let panic_response = panic_handle
        .recv_timeout(Duration::from_secs(1))
        .expect("panic completion response");
    assert!(!panic_response.success);
    assert_eq!(panic_response.id, "test-request-7");
    assert_eq!(
        panic_response
            .data
            .get("code")
            .and_then(|value| value.as_str()),
        Some("actor_fatal")
    );
    assert!(panic_response
        .data
        .get("message")
        .and_then(|value| value.as_str())
        .is_some_and(|message| message.contains("mutating panic sentinel")));

    let (other_done_tx, other_done_rx) = crossbeam_channel::bounded(1);
    let other_handle = executor.submit(
        other_root,
        Lane::PureRead,
        "test-request-8".to_string(),
        Box::new(move |_| {
            other_done_tx.send(()).expect("signal other done");
            ok("other")
        }),
    );
    other_done_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("another actor runs while blocker still occupies one worker");
    let other_response = other_handle
        .recv_timeout(Duration::from_secs(1))
        .expect("other completion response");
    assert!(other_response.success);

    let fatal_ran = Arc::new(AtomicUsize::new(0));
    let fatal_ran_job = Arc::clone(&fatal_ran);
    let fatal_handle = executor.submit(
        panic_root.clone(),
        Lane::PureRead,
        "test-request-9".to_string(),
        Box::new(move |_| {
            fatal_ran_job.store(1, Ordering::Release);
            ok("should-not-run")
        }),
    );
    let fatal_response = fatal_handle
        .recv_timeout(Duration::from_secs(1))
        .expect("fatal actor response");
    assert!(!fatal_response.success);
    assert_eq!(fatal_response.id, "test-request-9");
    assert_eq!(
        fatal_response
            .data
            .get("code")
            .and_then(|value| value.as_str()),
        Some("actor_fatal")
    );
    assert_eq!(fatal_ran.load(Ordering::Acquire), 0);
    assert!(executor.actor_is_fatal(&panic_root));

    release_block_tx.send(()).expect("release blocker");
    block_handle
        .recv_timeout(Duration::from_secs(1))
        .expect("blocker completion response");
}

#[test]
fn unregistered_actor_error_uses_submitted_request_id() {
    let executor = test_executor(2, 1, 1, 2);
    let (_dir, root) = test_root("unregistered");

    let response = executor
        .submit(
            root,
            Lane::PureRead,
            "missing-actor-request".to_string(),
            Box::new(|_| ok("should-not-run")),
        )
        .recv_timeout(Duration::from_secs(1))
        .expect("unregistered actor completion response");

    assert!(!response.success);
    assert_eq!(response.id, "missing-actor-request");
    assert_eq!(
        response.data.get("code").and_then(|value| value.as_str()),
        Some("actor_not_registered")
    );
}

#[test]
fn submit_async_resolves_response() {
    let executor = test_executor(2, 1, 1, 2);
    let (_dir, root) = test_root("async");
    executor.register_actor(root.clone(), test_ctx());

    let rx = executor.submit_async(
        root,
        Lane::PureRead,
        "async-request".to_string(),
        Box::new(|_| ok("async")),
    );
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("build current-thread runtime");
    let response =
        runtime.block_on(async { rx.await.expect("async completion sender stays alive") });

    assert!(response.success);
    assert_eq!(response.id, "async");
}

#[test]
fn mutator_drains_then_exclusive() {
    let executor = test_executor(4, 2, 3, 2);
    let (_dir, root) = test_root("mutator");
    executor.register_actor(root.clone(), test_ctx());

    let current_reads = Arc::new(AtomicUsize::new(0));
    let (read_started_tx, read_started_rx) = crossbeam_channel::bounded(2);
    let (release_reads_tx, release_reads_rx) = crossbeam_channel::bounded(2);
    let mut read_handles = Vec::new();

    for index in 0..2 {
        let current_reads = Arc::clone(&current_reads);
        let read_started_tx = read_started_tx.clone();
        let release_reads_rx = release_reads_rx.clone();
        read_handles.push(executor.submit(
            root.clone(),
            Lane::PureRead,
            "test-request-10".to_string(),
            Box::new(move |_| {
                current_reads.fetch_add(1, Ordering::AcqRel);
                read_started_tx.send(index).expect("signal read start");
                release_reads_rx
                    .recv_timeout(Duration::from_secs(2))
                    .expect("release read before mutator");
                current_reads.fetch_sub(1, Ordering::AcqRel);
                ok(format!("read-{index}"))
            }),
        ));
    }

    for _ in 0..2 {
        read_started_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("read starts before mutator");
    }

    let (mutator_started_tx, mutator_started_rx) = crossbeam_channel::bounded(1);
    let (release_mutator_tx, release_mutator_rx) = crossbeam_channel::bounded(1);
    let reads_at_mutator = Arc::clone(&current_reads);
    let mutator_handle = executor.submit(
        root.clone(),
        Lane::Mutating,
        "test-request-11".to_string(),
        Box::new(move |_| {
            mutator_started_tx
                .send(reads_at_mutator.load(Ordering::Acquire))
                .expect("signal mutator start");
            release_mutator_rx
                .recv_timeout(Duration::from_secs(2))
                .expect("release mutator");
            ok("mutator")
        }),
    );

    let (late_read_started_tx, late_read_started_rx) = crossbeam_channel::bounded(1);
    let late_read_handle = executor.submit(
        root,
        Lane::PureRead,
        "test-request-12".to_string(),
        Box::new(move |_| {
            late_read_started_tx
                .send(())
                .expect("signal late read start");
            ok("late-read")
        }),
    );

    // Both blocked for now: the mutator behind the in-flight readers' epoch,
    // the late read behind read_cap (2 readers already running).
    assert!(mutator_started_rx
        .recv_timeout(Duration::from_millis(75))
        .is_err());
    assert!(late_read_started_rx
        .recv_timeout(Duration::from_millis(75))
        .is_err());

    for _ in 0..2 {
        release_reads_tx.send(()).expect("release initial read");
    }
    for handle in read_handles {
        handle
            .recv_timeout(Duration::from_secs(1))
            .expect("initial read completion response");
    }

    // Reads-first interactive admission: once read_cap frees, the late read is
    // admitted ahead of the earlier-queued mutator (the writer's wait is
    // bounded by the promotion age, not by arrival order).
    late_read_started_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("late read starts ahead of the queued mutator");
    late_read_handle
        .recv_timeout(Duration::from_secs(1))
        .expect("late read completion response");

    let observed_reads = mutator_started_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("mutator starts after reads drain");
    assert_eq!(observed_reads, 0);

    release_mutator_tx.send(()).expect("release mutator");
    mutator_handle
        .recv_timeout(Duration::from_secs(1))
        .expect("mutator completion response");
}

#[test]
fn no_dispatch_of_nonrunnable() {
    let executor = test_executor(5, 2, 2, 2);
    let (_dir_a, root_a) = test_root("random-a");
    let (_dir_b, root_b) = test_root("random-b");
    executor.register_actor(root_a.clone(), test_ctx());
    executor.register_actor(root_b.clone(), test_ctx());

    let total_jobs = 96;
    let (done_tx, done_rx) = crossbeam_channel::bounded(total_jobs);
    let mut handles = Vec::new();
    let mut state = 0x5eed_u64;

    for index in 0..total_jobs {
        state = state.wrapping_mul(6364136223846793005).wrapping_add(1);
        let root = if state & 1 == 0 {
            root_a.clone()
        } else {
            root_b.clone()
        };
        let lane = match index % 4 {
            0 => Lane::PureRead,
            1 => Lane::SerialLspStatus,
            2 => Lane::HeavyInit,
            _ => Lane::Mutating,
        };
        let done_tx = done_tx.clone();
        let sleep_for = Duration::from_micros(200 + (state % 7) * 100);
        handles.push(executor.submit(
            root,
            lane,
            "test-request-13".to_string(),
            Box::new(move |_| {
                thread::sleep(sleep_for);
                done_tx.send(index).expect("signal randomized job done");
                ok(format!("random-{index}"))
            }),
        ));
    }

    let started_at = Instant::now();
    for completed in 0..total_jobs {
        assert!(
            started_at.elapsed() < Duration::from_secs(6),
            "randomized scheduler run exceeded wall-clock watchdog"
        );
        done_rx
            .recv_timeout(Duration::from_secs(2))
            .unwrap_or_else(|_| {
                panic!("no global executor progress after {completed} randomized completions")
            });
    }

    for handle in handles {
        handle
            .recv_timeout(Duration::from_secs(1))
            .expect("randomized completion response");
    }

    assert_eq!(executor.nonrunnable_dispatch_count(), 0);
}

#[test]
fn same_actor_maintenance_commit_preserves_small_pool_interactive_slot() {
    let executor = test_executor(2, 1, 1, 1);
    assert_eq!(executor.pool_size(), 2);
    assert_eq!(executor.actor_cap(), 1);
    assert_eq!(executor.interactive_reserve(), 1);
    assert_eq!(executor.maintenance_cap(), 1);
    let (_dir, root) = test_root("small-pool-same-actor-reserve");
    executor.register_actor(root.clone(), test_ctx());

    let (maintenance_started_tx, maintenance_started_rx) = crossbeam_channel::bounded(1);
    let (release_maintenance_tx, release_maintenance_rx) = crossbeam_channel::bounded(1);
    let maintenance = executor.submit_maintenance_async(
        root.clone(),
        Lane::MaintenanceCommit,
        "gated-maintenance".to_string(),
        Box::new(move |_| {
            maintenance_started_tx
                .send(())
                .expect("signal maintenance start");
            release_maintenance_rx
                .recv_timeout(Duration::from_secs(30))
                .expect("release maintenance");
            ok("gated-maintenance")
        }),
    );
    maintenance_started_rx
        .recv_timeout(Duration::from_secs(2))
        .expect("maintenance starts");

    let (interactive_started_tx, interactive_started_rx) = crossbeam_channel::bounded(1);
    let interactive = executor.submit(
        root.clone(),
        Lane::HeavyInit,
        "same-actor-interactive".to_string(),
        Box::new(move |_| {
            interactive_started_tx
                .send(())
                .expect("signal interactive start");
            ok("same-actor-interactive")
        }),
    );
    let (mutation_started_tx, mutation_started_rx) = crossbeam_channel::bounded(1);
    let mutation = executor.submit(
        root,
        Lane::Mutating,
        "same-actor-mutation".to_string(),
        Box::new(move |_| {
            mutation_started_tx.send(()).expect("signal mutation start");
            ok("same-actor-mutation")
        }),
    );

    let interactive_admitted = interactive_started_rx.recv_timeout(Duration::from_secs(10));
    let mutation_waited = mutation_started_rx.try_recv().is_err();
    release_maintenance_tx
        .send(())
        .expect("release maintenance");
    interactive_admitted.expect("interactive work admits before same-actor maintenance completes");
    assert!(
        mutation_waited,
        "mutating work must still wait for the maintenance epoch reader"
    );
    interactive
        .recv_timeout(Duration::from_secs(2))
        .expect("interactive completion");
    assert!(recv_async(maintenance, "maintenance completion").success);
    mutation
        .recv_timeout(Duration::from_secs(2))
        .expect("mutation completion");
}

#[test]
fn maintenance_cap_preserves_reserved_workers_for_interactive() {
    let executor = test_executor(4, 1, 1, 2);
    assert_eq!(executor.interactive_reserve(), 2);
    assert_eq!(executor.maintenance_cap(), 2);

    let mut dirs = Vec::new();
    let mut roots = Vec::new();
    for index in 0..3 {
        let (dir, root) = test_root(&format!("maintenance-reserve-{index}"));
        executor.register_actor(root.clone(), test_ctx());
        dirs.push(dir);
        roots.push(root);
    }

    let (maintenance_started_tx, maintenance_started_rx) = crossbeam_channel::bounded(2);
    let (release_maintenance_tx, release_maintenance_rx) = crossbeam_channel::bounded(2);
    let mut maintenance = Vec::new();
    for index in 0..executor.maintenance_cap() {
        let started_tx = maintenance_started_tx.clone();
        let release_rx = release_maintenance_rx.clone();
        maintenance.push(executor.submit_maintenance_async(
            roots[index].clone(),
            Lane::Mutating,
            format!("maintenance-{index}"),
            Box::new(move |_| {
                started_tx.send(index).expect("signal maintenance start");
                release_rx
                    .recv_timeout(Duration::from_secs(2))
                    .expect("release maintenance blocker");
                ok(format!("maintenance-{index}"))
            }),
        ));
    }
    for _ in 0..executor.maintenance_cap() {
        maintenance_started_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("maintenance fills cap");
    }

    let (interactive_started_tx, interactive_started_rx) = crossbeam_channel::bounded(1);
    let interactive = executor.submit(
        roots[2].clone(),
        Lane::PureRead,
        "interactive".to_string(),
        Box::new(move |_| {
            interactive_started_tx
                .send(())
                .expect("signal interactive start");
            ok("interactive")
        }),
    );

    interactive_started_rx
        .recv_timeout(Duration::from_millis(300))
        .expect("interactive starts while maintenance remains blocked");
    interactive
        .recv_timeout(Duration::from_secs(1))
        .expect("interactive completion response");

    for _ in 0..executor.maintenance_cap() {
        release_maintenance_tx
            .send(())
            .expect("release maintenance blocker");
    }
    for rx in maintenance {
        assert!(recv_async(rx, "queued maintenance completion").success);
    }
    assert_eq!(dirs.len(), 3);
}

#[test]
fn interactive_mutator_dispatches_while_maintenance_backlog_saturates_pool() {
    let executor = test_executor(4, 1, 1, 2);
    let pool_size = executor.pool_size();

    let mut dirs = Vec::new();
    let mut roots = Vec::new();
    for index in 0..=pool_size {
        let (dir, root) = test_root(&format!("maintenance-backlog-{index}"));
        executor.register_actor(root.clone(), test_ctx());
        dirs.push(dir);
        roots.push(root);
    }

    let (maintenance_started_tx, maintenance_started_rx) = crossbeam_channel::bounded(pool_size);
    let (release_maintenance_tx, release_maintenance_rx) = crossbeam_channel::bounded(pool_size);
    let mut maintenance = Vec::new();
    for index in 0..pool_size {
        let started_tx = maintenance_started_tx.clone();
        let release_rx = release_maintenance_rx.clone();
        maintenance.push(executor.submit_maintenance_async(
            roots[index].clone(),
            Lane::Mutating,
            format!("maintenance-backlog-{index}"),
            Box::new(move |_| {
                started_tx.send(index).expect("signal maintenance start");
                release_rx
                    .recv_timeout(Duration::from_secs(2))
                    .expect("release maintenance backlog");
                ok(format!("maintenance-backlog-{index}"))
            }),
        ));
    }

    for _ in 0..executor.maintenance_cap() {
        maintenance_started_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("maintenance fills its cap");
    }

    let (interactive_started_tx, interactive_started_rx) = crossbeam_channel::bounded(1);
    let interactive = executor.submit(
        roots[pool_size].clone(),
        Lane::Mutating,
        "interactive-route-bind".to_string(),
        Box::new(move |_| {
            interactive_started_tx
                .send(())
                .expect("signal interactive mutator start");
            ok("interactive-route-bind")
        }),
    );

    interactive_started_rx
        .recv_timeout(Duration::from_millis(300))
        .expect("interactive mutator starts despite a pool-sized maintenance backlog");
    interactive
        .recv_timeout(Duration::from_secs(1))
        .expect("interactive completion response");

    for _ in 0..pool_size {
        release_maintenance_tx
            .send(())
            .expect("release maintenance backlog");
    }
    for rx in maintenance {
        assert!(recv_async(rx, "queued maintenance completion").success);
    }
    assert_eq!(dirs.len(), pool_size + 1);
}

#[test]
fn startup_burst_maintenance_warmups_do_not_delay_interactive_binds() {
    let executor = test_executor(4, 1, 1, 2);
    let maintenance_roots = 12;
    let interactive_roots = 4;

    let mut dirs = Vec::new();
    let mut maintenance = Vec::new();
    let mut interactive = Vec::new();
    for index in 0..maintenance_roots {
        let (dir, root) = test_root(&format!("startup-warm-{index}"));
        executor.register_actor(root.clone(), test_ctx());
        dirs.push(dir);
        maintenance.push(root);
    }
    for index in 0..interactive_roots {
        let (dir, root) = test_root(&format!("startup-bind-{index}"));
        executor.register_actor(root.clone(), test_ctx());
        dirs.push(dir);
        interactive.push(root);
    }

    let (maintenance_started_tx, maintenance_started_rx) =
        crossbeam_channel::bounded(maintenance_roots);
    let (release_maintenance_tx, release_maintenance_rx) =
        crossbeam_channel::bounded(maintenance_roots);
    let mut maintenance_receivers = Vec::new();
    for (index, root) in maintenance.into_iter().enumerate() {
        let started_tx = maintenance_started_tx.clone();
        let release_rx = release_maintenance_rx.clone();
        maintenance_receivers.push(executor.submit_maintenance_async(
            root,
            Lane::Mutating,
            format!("startup-warm-{index}"),
            Box::new(move |_| {
                started_tx
                    .send(index)
                    .expect("signal startup maintenance start");
                release_rx
                    .recv_timeout(Duration::from_secs(2))
                    .expect("release startup maintenance");
                ok(format!("startup-warm-{index}"))
            }),
        ));
    }

    for _ in 0..executor.maintenance_cap() {
        maintenance_started_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("startup maintenance fills cap");
    }

    let (interactive_done_tx, interactive_done_rx) = crossbeam_channel::bounded(interactive_roots);
    let mut interactive_handles = Vec::new();
    for (index, root) in interactive.into_iter().enumerate() {
        let done_tx = interactive_done_tx.clone();
        interactive_handles.push(executor.submit(
            root,
            Lane::Mutating,
            format!("startup-bind-{index}"),
            Box::new(move |_| {
                done_tx
                    .send(index)
                    .expect("signal startup interactive bind");
                ok(format!("startup-bind-{index}"))
            }),
        ));
    }

    for completed in 0..interactive_roots {
        interactive_done_rx
            .recv_timeout(Duration::from_millis(500))
            .unwrap_or_else(|_| panic!("interactive bind {completed} waited for maintenance"));
    }
    for handle in interactive_handles {
        handle
            .recv_timeout(Duration::from_secs(1))
            .expect("startup interactive completion response");
    }

    for _ in 0..maintenance_roots {
        release_maintenance_tx
            .send(())
            .expect("release startup maintenance");
    }
    for rx in maintenance_receivers {
        assert!(recv_async(rx, "queued maintenance completion").success);
    }
    assert_eq!(dirs.len(), maintenance_roots + interactive_roots);
}

#[test]
fn newer_interactive_mutator_beats_older_same_actor_maintenance_mutator() {
    let executor = test_executor(2, 1, 1, 2);
    let (_block_dir, block_root) = test_root("same-root-priority-block");
    let (_dir, root) = test_root("same-root-priority");
    executor.register_actor(block_root.clone(), test_ctx());
    executor.register_actor(root.clone(), test_ctx());

    let (block_started_tx, block_started_rx) = crossbeam_channel::bounded(1);
    let (release_block_tx, release_block_rx) = crossbeam_channel::bounded(1);
    let blocker = executor.submit_maintenance_async(
        block_root,
        Lane::Mutating,
        "maintenance-blocker".to_string(),
        Box::new(move |_| {
            block_started_tx.send(()).expect("signal blocker start");
            release_block_rx
                .recv_timeout(Duration::from_secs(2))
                .expect("release blocker");
            ok("blocker")
        }),
    );
    block_started_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("maintenance blocker starts");

    let (maintenance_started_tx, maintenance_started_rx) = crossbeam_channel::bounded(1);
    let maintenance = executor.submit_maintenance_async(
        root.clone(),
        Lane::Mutating,
        "older-maintenance".to_string(),
        Box::new(move |_| {
            maintenance_started_tx
                .send(())
                .expect("signal older maintenance start");
            ok("older-maintenance")
        }),
    );
    assert!(maintenance_started_rx
        .recv_timeout(Duration::from_millis(75))
        .is_err());

    let (interactive_started_tx, interactive_started_rx) = crossbeam_channel::bounded(1);
    let interactive = executor.submit(
        root,
        Lane::Mutating,
        "newer-interactive".to_string(),
        Box::new(move |_| {
            interactive_started_tx
                .send(())
                .expect("signal newer interactive start");
            ok("newer-interactive")
        }),
    );
    interactive_started_rx
        .recv_timeout(Duration::from_millis(300))
        .expect("newer interactive mutator starts before older maintenance");
    assert!(maintenance_started_rx
        .recv_timeout(Duration::from_millis(75))
        .is_err());
    interactive
        .recv_timeout(Duration::from_secs(1))
        .expect("interactive completion response");

    release_block_tx.send(()).expect("release blocker");
    assert!(recv_async(blocker, "priority blocker completion").success);
    maintenance_started_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("maintenance starts after cap frees");
    assert!(recv_async(maintenance, "maintenance completion").success);
}

#[test]
fn mutating_job_admits_while_callgraph_refresh_worker_is_writing() {
    // This test shares the process-wide refresh worker with the
    // refresh_worker_tests group; without the lock, its flush can shut the
    // worker down mid-batch for a concurrently running peer test.
    let _refresh_guard = crate::callgraph_store::REFRESH_WORKER_TEST_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let _ = flush_callgraph_store_refreshes_with_budget(Duration::from_secs(30));
    let root_dir = tempfile::Builder::new()
        .prefix("aft-executor-callgraph-offload-")
        .tempdir()
        .expect("create callgraph worker root");
    let storage = root_dir.path().join("storage");
    let source = root_dir.path().join("main.rs");
    std::fs::write(&source, "fn entry() { leaf(); }\nfn leaf() {}\n")
        .expect("write callgraph fixture");
    let ctx = Arc::new(AppContext::new(
        Box::new(TreeSitterProvider::new()),
        Config {
            project_root: Some(root_dir.path().to_path_buf()),
            storage_dir: Some(storage),
            indexes: crate::config::IndexesConfig {
                trigram: false,
                semantic: false,
                callgraph: true,
            },
            ..Config::default()
        },
    ));
    ctx.set_canonical_cache_root(root_dir.path().to_path_buf());
    let project_key = crate::search_index::artifact_cache_key(root_dir.path());
    crate::root_cache::configure_artifact_access(root_dir.path(), &project_key, false);
    ctx.set_cache_role(false, None);
    let (store, _) = CallGraphStore::cold_build_with_lease(
        ctx.callgraph_store_dir(),
        root_dir.path().to_path_buf(),
        std::slice::from_ref(&source),
    )
    .expect("build callgraph fixture");
    drop(store);
    set_callgraph_refresh_worker_test_seam(
        root_dir.path().to_path_buf(),
        Duration::from_millis(350),
        false,
    );

    let executor = test_executor(2, 1, 2, 1);
    let root = ProjectRootId::from_path(root_dir.path()).expect("canonical actor root");
    assert!(executor.register_actor(root.clone(), Arc::clone(&ctx)));
    let ctx_for_drain = Arc::clone(&ctx);
    let source_for_drain = source.clone();
    let maintenance = executor.submit_maintenance_async(
        root.clone(),
        Lane::MaintenanceCommit,
        "watcher-drain".to_string(),
        Box::new(move |_| {
            ctx_for_drain.enqueue_callgraph_store_refresh([source_for_drain]);
            ok("watcher-drain")
        }),
    );
    assert!(recv_async(maintenance, "maintenance completion").success);

    let deadline = Instant::now() + Duration::from_secs(1);
    while callgraph_refresh_worker_test_counts(root_dir.path()).0 == 0 {
        assert!(Instant::now() < deadline, "refresh worker did not start");
        thread::sleep(Duration::from_millis(5));
    }
    let (mutating_started_tx, mutating_started_rx) = crossbeam_channel::bounded(1);
    let mutating = executor.submit_async(
        root,
        Lane::Mutating,
        "interactive-edit".to_string(),
        Box::new(move |_| {
            mutating_started_tx
                .send(())
                .expect("signal writer admission");
            ok("interactive-edit")
        }),
    );
    // Hang catch only: the store worker is held mid-write by the test seam
    // until the flush below, so admission here is an ordering proof - a tight
    // bound would just measure runner scheduling (census S-class shape; flaked
    // at 150ms on loaded Linux CI).
    mutating_started_rx
        .recv_timeout(Duration::from_secs(10))
        .expect("interactive writer should admit while store worker is mid-write");
    assert!(recv_async(mutating, "interactive edit completion").success);
    // The flush releases the held write and waits for the worker to finish it;
    // that is a liveness check, not a latency one. Every other flush site in
    // the refresh_worker tests uses 30 s, and a 1 s bound failed on a Windows
    // runner whose whole libtest pass took 575 s.
    assert!(flush_callgraph_store_refreshes_with_budget(
        Duration::from_secs(30)
    ));
    clear_callgraph_refresh_worker_test_seam(root_dir.path());
}

#[test]
fn mutating_jobs_are_not_dispatched_to_park_on_epoch_write() {
    let executor = test_executor(4, 1, 3, 2);
    let (_dir, root) = test_root("mutator-admission");
    executor.register_actor(root.clone(), test_ctx());

    let (started_tx, started_rx) = crossbeam_channel::bounded(3);
    let (release_tx, release_rx) = crossbeam_channel::bounded(3);
    let mut handles = Vec::new();
    for index in 0..3 {
        let started_tx = started_tx.clone();
        let release_rx = release_rx.clone();
        handles.push(executor.submit(
            root.clone(),
            Lane::Mutating,
            format!("mutator-{index}"),
            Box::new(move |_| {
                started_tx.send(index).expect("signal mutator start");
                release_rx
                    .recv_timeout(Duration::from_secs(2))
                    .expect("release mutator");
                ok(format!("mutator-{index}"))
            }),
        ));
    }

    assert_eq!(
        started_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("first mutator starts"),
        0
    );
    assert!(started_rx.recv_timeout(Duration::from_millis(75)).is_err());
    let snapshot = executor
        .try_dispatch_liveness_snapshot()
        .expect("scheduler liveness snapshot");
    assert_eq!(snapshot.running.interactive, 1);
    assert_eq!(snapshot.interactive.queued, 2);

    for expected in 1..3 {
        release_tx.send(()).expect("release current mutator");
        assert_eq!(
            started_rx
                .recv_timeout(Duration::from_secs(1))
                .expect("next mutator starts"),
            expected
        );
        assert!(started_rx.recv_timeout(Duration::from_millis(50)).is_err());
    }
    release_tx.send(()).expect("release final mutator");
    for handle in handles {
        handle
            .recv_timeout(Duration::from_secs(1))
            .expect("mutator completion response");
    }
    assert_eq!(executor.nonrunnable_dispatch_count(), 0);
}

#[test]
fn pure_reads_admit_ahead_of_queued_interactive_mutating() {
    // One actor, one worker: a long-running mutating job holds the actor while
    // a second mutating job and a pure read queue behind it. Pre-I-C, strict
    // arrival order admitted the queued mutator first; reads-first admission
    // must run the read as soon as the first mutator completes.
    let executor = test_executor(1, 2, 2, 1);
    let (_dir, root) = test_root("reads-first");
    assert!(executor.register_actor(root.clone(), test_ctx()));

    let (release_tx, release_rx) = crossbeam_channel::bounded::<()>(1);
    let first_mutator = executor.submit_async(
        root.clone(),
        Lane::Mutating,
        "mutator-1".to_string(),
        Box::new(move |_ctx| {
            release_rx
                .recv_timeout(Duration::from_secs(5))
                .expect("release first mutator");
            ok("mutator-1")
        }),
    );
    // Give the scheduler time to start the first mutator before queueing more.
    std::thread::sleep(Duration::from_millis(50));
    let (order_tx, order_rx) = crossbeam_channel::unbounded::<&'static str>();
    let mutator_order = order_tx.clone();
    let second_mutator = executor.submit_async(
        root.clone(),
        Lane::Mutating,
        "mutator-2".to_string(),
        Box::new(move |_ctx| {
            mutator_order.send("mutator-2").expect("record mutator");
            ok("mutator-2")
        }),
    );
    let read_order = order_tx;
    let read = executor.submit_async(
        root.clone(),
        Lane::PureRead,
        "read-1".to_string(),
        Box::new(move |_ctx| {
            read_order.send("read-1").expect("record read");
            ok("read-1")
        }),
    );

    release_tx.send(()).expect("release first mutator");
    let first_admitted = order_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("first queued job runs");
    assert_eq!(
        first_admitted, "read-1",
        "pure read must be admitted ahead of the queued mutating job"
    );
    assert_eq!(
        order_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("second queued job runs"),
        "mutator-2"
    );
    for (index, handle) in [first_mutator, second_mutator, read]
        .into_iter()
        .enumerate()
    {
        let awaited = format!("read-priority completion {index}");
        recv_async(handle, &awaited);
    }
}

#[test]
fn starved_bind_promotes_over_pure_reads() {
    // A queued configure (subc-bind-*) older than the promotion age must win
    // the interactive lane pick even when pure reads are queued, and new reads
    // must stop being admitted while it waits.
    let (_dir, root) = test_root("bind-promotion");
    let mut actor = ActorState::new(test_ctx());
    let now = Instant::now();

    let (tx, _rx) = crossbeam_channel::bounded::<Response>(1);
    let bind_job = QueuedJob {
        request_id: "subc-bind-42".to_string(),
        command: "executor::Interactive::Mutating".to_string(),
        job: Box::new(|_ctx| ok("subc-bind-42")),
        completion: CompletionSender::Sync(tx.clone()),
        queued_at: now - INTERACTIVE_WRITER_PROMOTION_AGE - Duration::from_secs(1),
        cancellation: None,
        maintenance_coalesce_key: None,
        rerun: None,
        force_exclusive: false,
        not_before: None,
        exclusive_attempts: 0,
        provider_generation: None,
    };
    let read_job = QueuedJob {
        request_id: "read-1".to_string(),
        command: "executor::Interactive::PureRead".to_string(),
        job: Box::new(|_ctx| ok("read-1")),
        completion: CompletionSender::Sync(tx),
        queued_at: now,
        cancellation: None,
        maintenance_coalesce_key: None,
        rerun: None,
        force_exclusive: false,
        not_before: None,
        exclusive_attempts: 0,
        provider_generation: None,
    };
    // Read arrived FIRST in arrival order; the starved bind must still win.
    actor.push_job(JobClass::Interactive, Lane::PureRead, read_job);
    actor.push_job(JobClass::Interactive, Lane::Mutating, bind_job);

    assert_eq!(
        actor
            .class_queues(JobClass::Interactive)
            .next_interactive_lane(Instant::now(), None),
        Some(Lane::Mutating),
        "starved bind must preempt queued pure reads"
    );
    let _ = root;
}

#[test]
fn fresh_bind_does_not_preempt_pure_reads() {
    // A configure queued just now stays behind pure reads (reads-first), only
    // age promotes it.
    let mut actor = ActorState::new(test_ctx());
    let now = Instant::now();
    let (tx, _rx) = crossbeam_channel::bounded::<Response>(1);
    actor.push_job(
        JobClass::Interactive,
        Lane::Mutating,
        QueuedJob {
            request_id: "subc-bind-7".to_string(),
            command: "executor::Interactive::Mutating".to_string(),
            job: Box::new(|_ctx| ok("subc-bind-7")),
            completion: CompletionSender::Sync(tx.clone()),
            queued_at: now,
            cancellation: None,
            maintenance_coalesce_key: None,
            rerun: None,
            force_exclusive: false,
            not_before: None,
            exclusive_attempts: 0,
            provider_generation: None,
        },
    );
    actor.push_job(
        JobClass::Interactive,
        Lane::PureRead,
        QueuedJob {
            request_id: "read-2".to_string(),
            command: "executor::Interactive::PureRead".to_string(),
            job: Box::new(|_ctx| ok("read-2")),
            completion: CompletionSender::Sync(tx),
            queued_at: now,
            cancellation: None,
            maintenance_coalesce_key: None,
            rerun: None,
            force_exclusive: false,
            not_before: None,
            exclusive_attempts: 0,
            provider_generation: None,
        },
    );

    assert_eq!(
        actor
            .class_queues(JobClass::Interactive)
            .next_interactive_lane(Instant::now(), None),
        Some(Lane::PureRead),
        "fresh binds queue behind pure reads until the promotion age"
    );
}

#[test]
fn maintenance_defers_to_queued_interactive_mutating_anywhere_in_queue() {
    // The maintenance barrier check must consider ANY queued interactive
    // mutating job, not only the arrival-order head (pre-I-C it checked
    // front_lane, which reads-first admission can reorder past).
    let mut actor = ActorState::new(test_ctx());
    let (tx, _rx) = crossbeam_channel::bounded::<Response>(1);
    actor.push_job(
        JobClass::Interactive,
        Lane::PureRead,
        QueuedJob {
            request_id: "read-3".to_string(),
            command: "executor::Interactive::PureRead".to_string(),
            job: Box::new(|_ctx| ok("read-3")),
            completion: CompletionSender::Sync(tx.clone()),
            queued_at: Instant::now(),
            cancellation: None,
            maintenance_coalesce_key: None,
            rerun: None,
            force_exclusive: false,
            not_before: None,
            exclusive_attempts: 0,
            provider_generation: None,
        },
    );
    actor.push_job(
        JobClass::Interactive,
        Lane::Mutating,
        QueuedJob {
            request_id: "edit-1".to_string(),
            command: "executor::Interactive::Mutating".to_string(),
            job: Box::new(|_ctx| ok("edit-1")),
            completion: CompletionSender::Sync(tx),
            queued_at: Instant::now(),
            cancellation: None,
            maintenance_coalesce_key: None,
            rerun: None,
            force_exclusive: false,
            not_before: None,
            exclusive_attempts: 0,
            provider_generation: None,
        },
    );

    assert!(
        actor.higher_priority_writer_barrier_blocks(JobClass::Maintenance),
        "maintenance must defer while interactive mutating work is queued, even behind reads"
    );
}

#[test]
fn maintenance_commit_overlaps_pure_reads_and_defers_to_writers() {
    // A long MaintenanceCommit job must not block PureReads (the I-A invariant:
    // background maintenance can never exclude interactive reads), and a
    // second MaintenanceCommit must serialize behind the first.
    let executor = test_executor(4, 2, 4, 1);
    let (_dir, root) = test_root("maintenance-commit-overlap");
    assert!(executor.register_actor(root.clone(), test_ctx()));

    let (mc_started_tx, mc_started_rx) = crossbeam_channel::bounded(1);
    let (release_mc_tx, release_mc_rx) = crossbeam_channel::bounded::<()>(1);
    let mc = executor.submit_maintenance_async(
        root.clone(),
        Lane::MaintenanceCommit,
        "mc-1".to_string(),
        Box::new(move |_ctx| {
            mc_started_tx.send(()).expect("signal mc start");
            release_mc_rx
                .recv_timeout(Duration::from_secs(5))
                .expect("release maintenance commit");
            ok("mc-1")
        }),
    );
    mc_started_rx
        .recv_timeout(Duration::from_secs(2))
        .expect("maintenance commit starts");

    // Read overlaps the running MaintenanceCommit.
    let read = executor.submit_async(
        root.clone(),
        Lane::PureRead,
        "read-overlap".to_string(),
        Box::new(|_ctx| ok("read-overlap")),
    );
    let read_response = recv_async(read, "overlapping pure-read completion");
    assert!(
        read_response.success,
        "read must complete while maintenance holds the read gate"
    );

    // Second MaintenanceCommit queues behind the first (serialized per actor).
    let (mc2_started_tx, mc2_started_rx) = crossbeam_channel::bounded(1);
    let _mc2 = executor.submit_maintenance_async(
        root,
        Lane::MaintenanceCommit,
        "mc-2".to_string(),
        Box::new(move |_ctx| {
            mc2_started_tx.send(()).expect("signal mc2 start");
            ok("mc-2")
        }),
    );
    assert!(
        mc2_started_rx
            .recv_timeout(Duration::from_millis(150))
            .is_err(),
        "second maintenance commit must wait for the first"
    );

    release_mc_tx.send(()).expect("release maintenance commit");
    recv_async(mc, "first maintenance-commit completion");
    mc2_started_rx
        .recv_timeout(Duration::from_secs(2))
        .expect("second maintenance commit runs after the first");
}

#[test]
fn cancel_cancellable_mutating_removes_queued_job_and_settles_receiver() {
    let executor = test_executor(2, 1, 2, 2);
    let (_dir, root) = test_root("cancel-queued");
    assert!(executor.register_actor(root.clone(), test_ctx()));

    // A channel gate keeps the first writer running while the second job is
    // queued. If the test panics, dropping the sender disconnects the gate so
    // executor teardown cannot wait forever for a test-owned blocker.
    let (first_started_tx, first_started_rx) = crossbeam_channel::bounded::<()>(1);
    let (release_first_tx, release_first_rx) = crossbeam_channel::bounded::<()>(1);
    let first_rx = executor.submit_async(
        root.clone(),
        Lane::Mutating,
        "first-blocking".to_string(),
        Box::new(move |_ctx| {
            first_started_tx.send(()).expect("signal first start");
            release_first_rx
                .recv_timeout(Duration::from_secs(60))
                .expect("release first mutating job");
            ok("first-blocking")
        }),
    );
    first_started_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("first mutating job starts");

    let executed = Arc::new(AtomicUsize::new(0));
    let executed_probe = Arc::clone(&executed);
    let (second_rx, second_token) = executor.submit_cancellable_async(
        root.clone(),
        Lane::Mutating,
        "second-queued".to_string(),
        Box::new(move |_ctx| {
            executed_probe.fetch_add(1, Ordering::SeqCst);
            ok("second-queued")
        }),
    );

    // The running writer prevents this job from being dequeued. The blocking
    // cancellation outcome is authoritative; a nonblocking state probe can
    // legitimately return None while the scheduler owns its state lock.
    let outcome = executor.cancel_job(&root, &second_token);
    assert_eq!(outcome, JobCancelOutcome::QueuedRemoved);
    let second_response = recv_async(second_rx, "cancelled queued job completion");
    assert!(!second_response.success);
    assert_eq!(
        second_response.data.get("code").and_then(|v| v.as_str()),
        Some("request_cancelled")
    );
    assert_eq!(
        executed.load(Ordering::SeqCst),
        0,
        "queued-cancelled job must never execute"
    );

    release_first_tx
        .send(())
        .expect("release first mutating job");
    let first_response = recv_async(first_rx, "first mutating blocker completion");
    assert!(first_response.success);
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
fn cancel_cancellable_mutating_signals_running_job_and_preserves_actor() {
    let executor = test_executor(2, 1, 2, 2);
    let (_dir, root) = test_root("cancel-running");
    assert!(executor.register_actor(root.clone(), test_ctx()));

    let (started_tx, started_rx) = crossbeam_channel::bounded::<()>(1);
    // Dropping this sender during a panic releases the worker before Executor's
    // destructor joins it. Without that escape hatch, a failed assertion before
    // cancellation can leave the worker polling forever during test teardown.
    let (_abort_tx, abort_rx) = crossbeam_channel::bounded::<()>(1);
    let (running_rx, running_token) = executor.submit_cancellable_async(
        root.clone(),
        Lane::Mutating,
        "running-configure".to_string(),
        Box::new(move |_ctx| {
            started_tx.send(()).expect("signal running start");
            let token = current_job_cancellation().expect("cancellable job sees its own token");
            while !token.cancel_requested_before_commit() {
                match abort_rx.recv_timeout(Duration::from_millis(2)) {
                    Err(crossbeam_channel::RecvTimeoutError::Timeout) => {}
                    Ok(()) | Err(crossbeam_channel::RecvTimeoutError::Disconnected) => {
                        return Response::error(
                            "running-configure",
                            "test_aborted",
                            "test released cancellation worker during teardown",
                        );
                    }
                }
            }
            Response::error(
                "running-configure",
                "request_cancelled",
                "cancelled at checkpoint",
            )
        }),
    );
    started_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("running job starts");

    // The label query deliberately uses try_lock, so scheduler contention can
    // transiently return None even though the start signal proves the job runs.
    let state_deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match executor.try_mutating_job_state_label(&root, "running-configure") {
            Some("running") => break,
            None if Instant::now() < state_deadline => {
                thread::sleep(Duration::from_millis(2));
            }
            state => panic!("running job state did not become observable: {state:?}"),
        }
    }

    let outcome = executor.cancel_job(&root, &running_token);
    assert_eq!(outcome, JobCancelOutcome::RunningSignalled);
    let response = recv_async(running_rx, "cancelled running job completion");
    assert_eq!(
        response.data.get("code").and_then(|v| v.as_str()),
        Some("request_cancelled")
    );
    // The completion event settles scheduler state asynchronously after the
    // receiver resolves, and the label probe is try-lock (None = contended):
    // poll until the settled state is observable.
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match executor.try_mutating_job_state_label(&root, "running-configure") {
            Some("not_found") => break,
            _ if Instant::now() >= deadline => {
                panic!("cancelled job must fully settle scheduler state within 5s")
            }
            _ => thread::sleep(Duration::from_millis(2)),
        }
    }

    // Actor must remain usable: a follow-up mutating job completes normally.
    let follow_up = executor.submit(
        root.clone(),
        Lane::Mutating,
        "follow-up".to_string(),
        Box::new(|_ctx| ok("follow-up")),
    );
    let response = follow_up
        .recv_timeout(Duration::from_secs(5))
        .expect("follow-up mutating job completes");
    assert!(
        response.success,
        "actor must stay usable after cancellation"
    );
}

#[test]
fn cancel_job_after_commit_seal_reports_committed_and_lets_job_finish() {
    let executor = test_executor(2, 1, 2, 2);
    let (_dir, root) = test_root("cancel-committed");
    assert!(executor.register_actor(root.clone(), test_ctx()));

    let (sealed_tx, sealed_rx) = crossbeam_channel::bounded::<()>(1);
    let (release_tx, release_rx) = crossbeam_channel::bounded::<()>(1);
    let (rx, token) = executor.submit_cancellable_async(
        root.clone(),
        Lane::Mutating,
        "committed-configure".to_string(),
        Box::new(move |_ctx| {
            let token = current_job_cancellation().expect("cancellable job sees its own token");
            assert!(token.try_seal_committed(), "uncancelled seal must win");
            sealed_tx.send(()).expect("signal seal");
            release_rx
                .recv_timeout(Duration::from_secs(5))
                .expect("release committed job");
            ok("committed-configure")
        }),
    );
    sealed_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("job seals its commit");

    let outcome = executor.cancel_job(&root, &token);
    assert_eq!(outcome, JobCancelOutcome::RunningCommitted);
    assert!(
        !token.cancel_requested_before_commit(),
        "a sealed job must not observe the late cancel"
    );

    release_tx.send(()).expect("release job");
    let response = recv_async(rx, "committed job completion");
    assert!(response.success, "committed job finishes normally");
}

#[test]
fn remove_cancellable_removes_matching_lane_order_occurrence_not_first() {
    // Queue M1, Heavy, M2 then cancel M2 (second Mutating). The order ladder
    // must keep [Mutating, HeavyInit] — removing the FIRST Mutating occurrence
    // would corrupt it to [HeavyInit, Mutating] and dispatch Heavy before M1.
    let mut actor = ActorState::new(test_ctx());
    let (tx, _rx) = crossbeam_channel::bounded::<Response>(3);
    let m2_token = JobCancellation::new();
    actor.push_job(
        JobClass::Interactive,
        Lane::Mutating,
        QueuedJob {
            request_id: "m1".to_string(),
            command: "executor::Interactive::Mutating".to_string(),
            job: Box::new(|_ctx| ok("m1")),
            completion: CompletionSender::Sync(tx.clone()),
            queued_at: Instant::now(),
            cancellation: None,
            maintenance_coalesce_key: None,
            rerun: None,
            force_exclusive: false,
            not_before: None,
            exclusive_attempts: 0,
            provider_generation: None,
        },
    );
    actor.push_job(
        JobClass::Interactive,
        Lane::HeavyInit,
        QueuedJob {
            request_id: "h1".to_string(),
            command: "executor::Interactive::HeavyInit".to_string(),
            job: Box::new(|_ctx| ok("h1")),
            completion: CompletionSender::Sync(tx.clone()),
            queued_at: Instant::now(),
            cancellation: None,
            maintenance_coalesce_key: None,
            rerun: None,
            force_exclusive: false,
            not_before: None,
            exclusive_attempts: 0,
            provider_generation: None,
        },
    );
    actor.push_job(
        JobClass::Interactive,
        Lane::Mutating,
        QueuedJob {
            request_id: "m2".to_string(),
            command: "executor::Interactive::Mutating".to_string(),
            job: Box::new(|_ctx| ok("m2")),
            completion: CompletionSender::Sync(tx),
            queued_at: Instant::now(),
            cancellation: Some(m2_token.clone()),
            maintenance_coalesce_key: None,
            rerun: None,
            force_exclusive: false,
            not_before: None,
            exclusive_attempts: 0,
            provider_generation: None,
        },
    );

    let removed = actor
        .remove_queued_cancellable(&m2_token)
        .expect("m2 removed");
    assert_eq!(removed.request_id, "m2");

    // Arrival-order head must still be M1's lane, and popping in order must
    // yield m1 then h1 with a consistent ladder.
    let queues = actor.class_queues_mut(JobClass::Interactive);
    assert_eq!(queues.front_lane(), Some(Lane::Mutating));
    let first = queues.pop_front_job(Lane::Mutating).expect("m1 pops");
    assert_eq!(first.request_id, "m1");
    assert_eq!(queues.front_lane(), Some(Lane::HeavyInit));
    let second = queues.pop_front_job(Lane::HeavyInit).expect("h1 pops");
    assert_eq!(second.request_id, "h1");
    assert!(!queues.has_queued_jobs(), "ladder fully consistent");
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

#[test]
fn heavy_init_completion_and_following_read_do_not_wait_on_maintenance_epoch() {
    let executor = test_executor(4, 2, 3, 2);
    let (_dir, root) = test_root("heavy-response-epoch");
    executor.register_actor(root.clone(), test_ctx());

    let (maintenance_started_tx, maintenance_started_rx) = crossbeam_channel::bounded(1);
    let (release_maintenance_tx, release_maintenance_rx) = crossbeam_channel::bounded(1);
    let maintenance = executor.submit_maintenance_async(
        root.clone(),
        Lane::MaintenanceCommit,
        "gated-configure-tail".to_string(),
        Box::new(move |_| {
            maintenance_started_tx
                .send(())
                .expect("signal maintenance start");
            release_maintenance_rx
                .recv_timeout(Duration::from_secs(5))
                .expect("release maintenance epoch");
            ok("gated-configure-tail")
        }),
    );
    maintenance_started_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("maintenance holds the epoch read gate");

    let (heavy_done_tx, heavy_done_rx) = crossbeam_channel::bounded(1);
    let heavy = executor.submit(
        root.clone(),
        Lane::HeavyInit,
        "finalized-search".to_string(),
        Box::new(move |_| {
            heavy_done_tx.send(()).expect("signal heavy closure done");
            ok("finalized-search")
        }),
    );
    heavy_done_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("heavy command closure finishes");

    let read = executor.submit(
        root.clone(),
        Lane::PureRead,
        "read-after-finalized-search".to_string(),
        Box::new(|_| ok("read-after-finalized-search")),
    );
    let bind = executor.submit(
        root.clone(),
        Lane::Mutating,
        "subc-bind-after-finalized-search".to_string(),
        Box::new(|_| ok("subc-bind-after-finalized-search")),
    );

    let heavy_result = heavy.recv_timeout(Duration::from_secs(1));
    let read_result = read.recv_timeout(Duration::from_secs(1));
    let snapshot_deadline = Instant::now() + Duration::from_secs(1);
    let snapshot = loop {
        if let Some(snapshot) = executor
            .try_bind_blocker_snapshot(&root, "subc-bind-after-finalized-search")
            .filter(|snapshot| {
                snapshot.configure_state == "queued" && snapshot.in_flight_readers.is_empty()
            })
        {
            break Some(snapshot);
        }
        if Instant::now() >= snapshot_deadline {
            break None;
        }
        std::thread::sleep(Duration::from_millis(2));
    };

    release_maintenance_tx
        .send(())
        .expect("release maintenance epoch");
    let snapshot = snapshot.expect("completed read remained in the bind census");
    assert!(
        heavy_result
            .expect("heavy completion must not reacquire the actor epoch")
            .success
    );
    assert!(
        read_result
            .expect("a finalized HeavyInit must not park a later reader")
            .success
    );
    assert!(snapshot.in_flight_readers.is_empty());
    assert!(
        snapshot
            .blockers
            .iter()
            .all(|blocker| !blocker.starts_with("waiting_on_readers")
                && !blocker.starts_with("zombie_reader")),
        "completed reads must not remain in the bind census: {:?}",
        snapshot.blockers
    );
    assert!(recv_async(maintenance, "maintenance completion").success);
    bind.recv_timeout(Duration::from_secs(1))
        .expect("bind runs after the real maintenance reader exits");
}

#[test]
fn bind_blocker_snapshot_labels_reader_parked_before_execution_as_zombie() {
    let executor = test_executor(2, 1, 2, 1);
    let (_dir, root) = test_root("zombie-reader-blocker");
    executor.register_actor(root.clone(), test_ctx());
    let epoch = {
        let state = executor.inner.state.lock();
        Arc::clone(&state.actors.get(&root).expect("actor").epoch)
    };
    let epoch_writer = epoch.write();

    let read = executor.submit(
        root.clone(),
        Lane::PureRead,
        "parked-before-execution".to_string(),
        Box::new(|_| ok("parked-before-execution")),
    );
    // Bounded: a reader that never dispatches must fail the test, not hang it.
    // The bound is generous because it measures CI scheduling, not our code.
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let admitted = executor
            .inner
            .state
            .lock()
            .running_jobs
            .values()
            .find(|job| job.root_id == root && job.request_id == "parked-before-execution")
            .is_some_and(|job| !job.execution_started.load(Ordering::Acquire));
        if admitted {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "reader was not dispatched within 30s"
        );
        std::thread::yield_now();
    }

    let bind = executor.submit(
        root.clone(),
        Lane::Mutating,
        "subc-bind-zombie-census".to_string(),
        Box::new(|_| ok("subc-bind-zombie-census")),
    );
    // The snapshot is a try-lock read; under contention it returns None until
    // the scheduler releases the state lock, so retry - but bounded.
    let snapshot = loop {
        if let Some(snapshot) = executor.try_bind_blocker_snapshot(&root, "subc-bind-zombie-census")
        {
            break snapshot;
        }
        assert!(
            Instant::now() < deadline,
            "zombie bind blocker snapshot not readable within 30s"
        );
        std::thread::yield_now();
    };
    assert!(snapshot
        .blockers
        .iter()
        .any(|blocker| blocker.starts_with("zombie_reader(")));
    assert!(snapshot
        .blockers
        .iter()
        .all(|blocker| !blocker.starts_with("waiting_on_readers_stuck(")));
    assert_eq!(snapshot.in_flight_readers.len(), 1);
    assert!(!snapshot.in_flight_readers[0].execution_started);

    drop(epoch_writer);
    read.recv_timeout(Duration::from_secs(1))
        .expect("parked reader executes after epoch release");
    bind.recv_timeout(Duration::from_secs(1))
        .expect("bind executes after parked reader");
}

#[test]
fn cancelling_dispatched_reader_releases_barrier_before_epoch_writer_exits() {
    let executor = test_executor(2, 1, 2, 1);
    let (_dir, root) = test_root("cancel-dispatched-reader");
    executor.register_actor(root.clone(), test_ctx());
    let epoch = {
        let state = executor.inner.state.lock();
        Arc::clone(&state.actors.get(&root).expect("actor").epoch)
    };
    let epoch_writer = epoch.write();

    let executed = Arc::new(AtomicUsize::new(0));
    let executed_probe = Arc::clone(&executed);
    let (read, cancellation) = executor.submit_cancellable_async(
        root.clone(),
        Lane::PureRead,
        "dead-route-reader".to_string(),
        Box::new(move |_| {
            executed_probe.fetch_add(1, Ordering::AcqRel);
            ok("dead-route-reader")
        }),
    );
    let admission_deadline = Instant::now() + Duration::from_secs(1);
    loop {
        let parked = executor
            .inner
            .state
            .lock()
            .running_jobs
            .values()
            .find(|job| job.root_id == root && job.request_id == "dead-route-reader")
            .is_some_and(|job| !job.execution_started.load(Ordering::Acquire));
        if parked {
            break;
        }
        assert!(
            Instant::now() < admission_deadline,
            "reader did not reach the pre-execution epoch wait"
        );
        std::thread::sleep(Duration::from_millis(2));
    }

    let bind = executor.submit(
        root.clone(),
        Lane::Mutating,
        "subc-bind-after-dead-route".to_string(),
        Box::new(|_| ok("subc-bind-after-dead-route")),
    );
    assert_eq!(
        executor.cancel_job_before_execution(&root, &cancellation),
        PreExecutionCancelOutcome::DispatchedCancelled
    );

    let release_deadline = Instant::now() + Duration::from_secs(1);
    loop {
        let state = executor.try_mutating_job_state_label(&root, "subc-bind-after-dead-route");
        if state == Some("running") {
            break;
        }
        assert!(
            Instant::now() < release_deadline,
            "cancelled reader kept the scheduler writer barrier: {state:?}"
        );
        std::thread::sleep(Duration::from_millis(2));
    }

    drop(epoch_writer);
    let read_response = read.blocking_recv().expect("cancelled read completion");
    assert!(!read_response.success);
    assert_eq!(read_response.data["code"], "request_cancelled");
    assert_eq!(executed.load(Ordering::Acquire), 0);
    bind.recv_timeout(Duration::from_secs(1))
        .expect("bind completes after the external epoch writer exits");
}

#[track_caller]
fn wait_for_maintenance_inflight(executor: &Executor, expected: usize, label: &str) {
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        let snapshot = executor
            .try_dispatch_liveness_snapshot()
            .expect("dispatch liveness snapshot");
        if snapshot.running.maintenance == expected {
            assert_eq!(
                snapshot.phantom_running_maintenance, 0,
                "{label} left a maintenance reservation without a completion owner"
            );
            return;
        }
        assert!(
            Instant::now() < deadline,
            "{label}: expected {expected} running maintenance job(s), observed {}",
            snapshot.running.maintenance
        );
        thread::sleep(Duration::from_millis(2));
    }
}

#[track_caller]
fn assert_follow_up_maintenance_admitted(
    executor: &Executor,
    root: ProjectRootId,
    request_id: &str,
) {
    wait_for_maintenance_inflight(executor, 0, request_id);
    let follow_up = executor.submit_maintenance_async(
        root,
        Lane::MaintenanceCommit,
        request_id.to_string(),
        Box::new(|_| ok("follow-up-maintenance")),
    );
    assert!(recv_async(follow_up, request_id).success);
    wait_for_maintenance_inflight(executor, 0, request_id);
}

#[test]
fn maintenance_panic_releases_reservation_and_admits_another_actor() {
    let executor = test_executor(2, 1, 1, 2);
    let (_panic_dir, panic_root) = test_root("maintenance-panic-owner");
    let (_other_dir, other_root) = test_root("maintenance-panic-follow-up");
    executor.register_actor(panic_root.clone(), test_ctx());
    executor.register_actor(other_root.clone(), test_ctx());

    let panicked = executor.submit_maintenance_async(
        panic_root,
        Lane::MaintenanceCommit,
        "maintenance-panic".to_string(),
        Box::new(|_| panic!("maintenance completion guard panic sentinel")),
    );
    let response = recv_async(panicked, "panicked maintenance job");
    assert!(!response.success);
    assert_follow_up_maintenance_admitted(&executor, other_root, "after-maintenance-panic");
}

#[test]
fn dropped_maintenance_receiver_releases_reservation_and_admits_another_actor() {
    let executor = test_executor(2, 1, 1, 2);
    let (_dropped_dir, dropped_root) = test_root("maintenance-dropped-receiver");
    let (_other_dir, other_root) = test_root("maintenance-dropped-follow-up");
    executor.register_actor(dropped_root.clone(), test_ctx());
    executor.register_actor(other_root.clone(), test_ctx());

    let (done_tx, done_rx) = crossbeam_channel::bounded(1);
    let dropped = executor.submit_maintenance_async(
        dropped_root,
        Lane::MaintenanceCommit,
        "dropped-maintenance-receiver".to_string(),
        Box::new(move |_| {
            done_tx.send(()).expect("signal dropped receiver job");
            ok("dropped-maintenance-receiver")
        }),
    );
    drop(dropped);
    done_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("maintenance job runs after its receiver is dropped");
    assert_follow_up_maintenance_admitted(&executor, other_root, "after-dropped-receiver");
}

#[test]
fn cancelled_maintenance_cold_build_wait_releases_reservation_and_admits_another_actor() {
    let executor = test_executor(2, 1, 1, 2);
    let (_cancel_dir, cancel_root) = test_root("maintenance-cold-build-cancel");
    let (_other_dir, other_root) = test_root("maintenance-cold-build-follow-up");
    executor.register_actor(cancel_root.clone(), test_ctx());
    executor.register_actor(other_root.clone(), test_ctx());

    let limiter = crate::cold_build_limiter::test_limiter(1);
    let held = crate::cold_build_limiter::try_acquire_classified_with_limiter(
        &limiter,
        &crate::cold_build_limiter::ColdBuildAdmissionRequest::new(
            "held-cold-build-slot",
            crate::cold_build_limiter::ColdBuildAdmissionClass::Maintenance,
        ),
    )
    .expect("hold isolated cold-build slot");
    let (started_tx, started_rx) = crossbeam_channel::bounded(1);
    let waiting_limiter = Arc::clone(&limiter);
    let (waiting, cancellation) = executor.submit_maintenance_cancellable_async(
        cancel_root.clone(),
        Lane::MaintenanceCommit,
        "cancelled-cold-build-wait".to_string(),
        Box::new(move |_| {
            started_tx.send(()).expect("signal cold-build wait");
            let permit = crate::cold_build_limiter::acquire_blocking_while_cancellable_with_limiter(
                &waiting_limiter,
                "executor completion guard cancellation test",
                crate::cold_build_limiter::ColdBuildAdmissionRequest::new(
                    "cancelled-cold-build-wait",
                    crate::cold_build_limiter::ColdBuildAdmissionClass::Maintenance,
                ),
                || true,
                current_job_cancelled,
            );
            assert!(permit.is_none(), "cancelled wait must not acquire a slot");
            Response::error(
                "cancelled-cold-build-wait",
                "request_cancelled",
                "cancelled while waiting for cold-build capacity",
            )
        }),
    );
    started_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("maintenance cold-build wait starts");
    assert_eq!(
        executor.cancel_job(&cancel_root, &cancellation),
        JobCancelOutcome::RunningSignalled
    );
    let response = recv_async(waiting, "cancelled maintenance cold-build wait");
    assert_eq!(response.data["code"], "request_cancelled");
    drop(held);
    assert_follow_up_maintenance_admitted(&executor, other_root, "after-cold-build-cancel");
}

#[test]
fn removed_actor_running_maintenance_releases_reservation_for_another_actor() {
    let executor = test_executor(2, 1, 1, 2);
    let (_removed_dir, removed_root) = test_root("maintenance-removed-actor");
    let (_other_dir, other_root) = test_root("maintenance-removed-follow-up");
    executor.register_actor(removed_root.clone(), test_ctx());
    executor.register_actor(other_root.clone(), test_ctx());

    let (started_tx, started_rx) = crossbeam_channel::bounded(1);
    let (release_tx, release_rx) = crossbeam_channel::bounded(1);
    let running = executor.submit_maintenance_async(
        removed_root.clone(),
        Lane::MaintenanceCommit,
        "removed-actor-maintenance".to_string(),
        Box::new(move |_| {
            started_tx.send(()).expect("signal removed actor job");
            release_rx
                .recv_timeout(Duration::from_secs(1))
                .expect("release removed actor job");
            ok("removed-actor-maintenance")
        }),
    );
    started_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("removed actor maintenance starts");
    executor.remove_actor(&removed_root);
    release_tx.send(()).expect("release removed actor job");
    assert!(recv_async(running, "removed actor maintenance completion").success);
    assert_follow_up_maintenance_admitted(&executor, other_root, "after-actor-removal");
}

#[test]
fn duplicate_maintenance_request_ids_keep_distinct_completion_owners() {
    let executor = test_executor(4, 2, 2, 2);
    let (_shared_dir, shared_root) = test_root("duplicate-maintenance-id");
    let (_other_dir, other_root) = test_root("duplicate-maintenance-follow-up");
    executor.register_actor(shared_root.clone(), test_ctx());
    executor.register_actor(other_root.clone(), test_ctx());

    let (started_tx, started_rx) = crossbeam_channel::bounded(2);
    let (release_tx, release_rx) = crossbeam_channel::bounded(2);
    let mut jobs = Vec::new();
    for index in 0..2 {
        let started_tx = started_tx.clone();
        let release_rx = release_rx.clone();
        jobs.push(executor.submit_maintenance_async(
            shared_root.clone(),
            Lane::PureRead,
            "duplicate-maintenance-id".to_string(),
            Box::new(move |_| {
                started_tx.send(index).expect("signal duplicate-id job");
                release_rx
                    .recv_timeout(Duration::from_secs(1))
                    .expect("release duplicate-id job");
                ok(format!("duplicate-maintenance-id-{index}"))
            }),
        ));
    }
    for _ in 0..2 {
        started_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("both duplicate-id maintenance jobs start");
    }
    wait_for_maintenance_inflight(&executor, 2, "duplicate maintenance request ids");
    let snapshot = executor.try_dispatch_liveness_snapshot().unwrap();
    assert_eq!(snapshot.running.completion_owned_maintenance, 2);

    release_tx.send(()).unwrap();
    release_tx.send(()).unwrap();
    for job in jobs {
        assert!(recv_async(job, "duplicate-id maintenance completion").success);
    }
    assert_follow_up_maintenance_admitted(&executor, other_root, "after-duplicate-request-ids");
}

/// Bound on how long a route bind may wait in the executor before its
/// configure starts. subc refuses a module's binds once relays exceed 12s; the
/// bind path has to stay far inside that no matter what else is running.
const BIND_STORM_ACK_BOUND: Duration = Duration::from_secs(3);

/// Forty sessions rebind forty roots at once (a daemon restart) while every
/// general worker is already held by other roots' maintenance tails and
/// long interactive reads, with more interactive work queued behind them.
/// Every bind must still start its configure within the bound: none may wait
/// for one of those long jobs to finish.
#[test]
fn bind_storm_reaches_ack_while_maintenance_and_interactive_saturate_the_pool() {
    const BUSY_ROOTS: usize = 8;
    const READS_PER_BUSY_ROOT: usize = 3;
    const BIND_ROOTS: usize = 40;

    let executor = test_executor(4, 2, 2, 2);
    let pool_size = executor.pool_size();
    let mut dirs = Vec::new();

    let (busy_started_tx, busy_started_rx) = crossbeam_channel::unbounded::<()>();
    let (release_tx, release_rx) = crossbeam_channel::unbounded::<()>();
    let mut busy_handles = Vec::new();
    for index in 0..BUSY_ROOTS {
        let (dir, root) = test_root(&format!("bind-storm-busy-{index}"));
        assert!(executor.register_actor(root.clone(), test_ctx()));
        dirs.push(dir);

        // A configure tail from an earlier bind of another root.
        let started = busy_started_tx.clone();
        let release = release_rx.clone();
        busy_handles.push(executor.submit_maintenance_async(
            root.clone(),
            Lane::MaintenanceCommit,
            format!("subc-maintenance-drain-configure-tail-busy-{index}"),
            Box::new(move |_| {
                let _ = started.send(());
                let _ = release.recv_timeout(Duration::from_secs(30));
                ok("tail")
            }),
        ));
        // Long interactive reads of other sessions (bash, grep).
        for read in 0..READS_PER_BUSY_ROOT {
            let started = busy_started_tx.clone();
            let release = release_rx.clone();
            busy_handles.push(executor.submit_async(
                root.clone(),
                Lane::PureRead,
                format!("busy-read-{index}-{read}"),
                Box::new(move |_| {
                    let _ = started.send(());
                    let _ = release.recv_timeout(Duration::from_secs(30));
                    ok("busy-read")
                }),
            ));
        }
    }
    for occupied in 0..pool_size {
        busy_started_rx
            .recv_timeout(Duration::from_secs(5))
            .unwrap_or_else(|_| panic!("busy work occupies worker {occupied}"));
    }

    let (bind_started_tx, bind_started_rx) = crossbeam_channel::unbounded::<(usize, Duration)>();
    let mut bind_handles = Vec::new();
    for index in 0..BIND_ROOTS {
        let (dir, root) = test_root(&format!("bind-storm-bind-{index}"));
        assert!(executor.register_actor(root.clone(), test_ctx()));
        dirs.push(dir);
        let started = bind_started_tx.clone();
        let submitted_at = Instant::now();
        bind_handles.push(executor.submit_async(
            root,
            Lane::Mutating,
            format!("subc-bind-{index}"),
            Box::new(move |_| {
                let _ = started.send((index, submitted_at.elapsed()));
                // A fast-path configure: resolve config, register the session.
                thread::sleep(Duration::from_millis(5));
                ok(format!("subc-bind-{index}"))
            }),
        ));
    }

    let deadline = Instant::now() + BIND_STORM_ACK_BOUND;
    let mut waits = Vec::new();
    while waits.len() < BIND_ROOTS {
        let remaining = deadline.saturating_duration_since(Instant::now());
        match bind_started_rx.recv_timeout(remaining) {
            Ok(started) => waits.push(started),
            Err(_) => break,
        }
    }
    let started_in_bound = waits.len();

    // Release the saturating work either way so the executor drains.
    for _ in 0..busy_handles.len() {
        let _ = release_tx.send(());
    }
    for handle in bind_handles {
        assert!(recv_async(handle, "bind-storm bind completion").success);
    }
    for handle in busy_handles {
        let _ = recv_async(handle, "bind-storm busy completion");
    }

    let worst = waits.iter().map(|(_, wait)| *wait).max();
    assert_eq!(
        started_in_bound, BIND_ROOTS,
        "only {started_in_bound} of {BIND_ROOTS} binds started within {BIND_STORM_ACK_BOUND:?} \
         while the pool was saturated (worst observed wait {worst:?})"
    );
    assert!(
        waits.iter().all(|(_, wait)| *wait < BIND_STORM_ACK_BOUND),
        "a bind waited past the bound: {waits:?}"
    );
    assert_eq!(dirs.len(), BUSY_ROOTS + BIND_ROOTS);
}

/// A same-root configure tail holds the actor's epoch read gate, so a bind for
/// that root cannot start until the tail lets go. A tail that polls
/// `current_actor_writer_waiting` between its steps must see the queued bind
/// and return, so the bind starts within one step rather than after the
/// whole tail.
#[test]
fn same_root_maintenance_tail_observes_a_queued_bind_and_lets_it_run() {
    let executor = test_executor(4, 2, 2, 2);
    let (_dir, root) = test_root("tail-steps-aside");
    assert!(executor.register_actor(root.clone(), test_ctx()));

    let (tail_started_tx, tail_started_rx) = crossbeam_channel::bounded::<()>(1);
    let tail = executor.submit_maintenance_async(
        root.clone(),
        Lane::MaintenanceCommit,
        "subc-maintenance-drain-configure-tail-same-root".to_string(),
        Box::new(move |_| {
            assert!(
                !current_actor_writer_waiting(),
                "no writer is queued when the tail starts"
            );
            tail_started_tx.send(()).expect("signal tail start");
            // Up to ten seconds of 20 ms steps, like a tail loading artifacts.
            for step in 0..500 {
                if current_actor_writer_waiting() {
                    return ok(format!("stepped-aside-after-{step}"));
                }
                thread::sleep(Duration::from_millis(20));
            }
            ok("ran-to-completion")
        }),
    );
    tail_started_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("tail starts");

    let submitted_at = Instant::now();
    let bind = executor.submit_async(
        root,
        Lane::Mutating,
        "subc-bind-same-root".to_string(),
        Box::new(move |_| ok(format!("{}", submitted_at.elapsed().as_millis()))),
    );
    let bind_response = recv_async(bind, "same-root bind");
    let waited = submitted_at.elapsed();
    let tail_response = recv_async(tail, "same-root tail");
    assert!(
        tail_response.id.starts_with("stepped-aside-after-"),
        "the tail must observe the queued bind: {}",
        tail_response.id
    );
    assert!(bind_response.success);
    assert!(
        waited < Duration::from_secs(1),
        "the bind waited {waited:?} behind a same-root tail"
    );
    assert!(
        !current_actor_writer_waiting(),
        "off a worker thread no actor is observed"
    );
}

/// Shared handles a test needs to act like a detached view publication on an
/// actor, or to read its writer demand.
fn actor_epoch_and_demand(
    executor: &Executor,
    root: &ProjectRootId,
) -> (Arc<RwLock<()>>, Arc<AtomicUsize>, Arc<AtomicUsize>) {
    let state = executor.inner.state.lock();
    let actor = state.actors.get(root).expect("actor");
    (
        Arc::clone(&actor.epoch),
        Arc::clone(&actor.waiting_writers),
        Arc::clone(&actor.detached_writers),
    )
}

/// Submit a repeatable route bind the way the module loop does.
fn submit_repeatable_bind(
    executor: &Executor,
    root: &ProjectRootId,
    request_id: &str,
    job: impl Fn(&AppContext) -> Response + Send + Sync + 'static,
) -> (tokio::sync::oneshot::Receiver<Response>, JobCancellation) {
    executor.submit_bind_cancellable_async(root.clone(), request_id.to_string(), Arc::new(job))
}

/// Wait until the bind's diagnostics satisfy `accept`, or fail after `within`.
fn wait_for_bind_snapshot(
    executor: &Executor,
    root: &ProjectRootId,
    request_id: &str,
    within: Duration,
    accept: impl Fn(&BindBlockerSnapshot) -> bool,
) -> Option<BindBlockerSnapshot> {
    let deadline = Instant::now() + within;
    loop {
        if let Some(snapshot) = executor
            .try_bind_blocker_snapshot(root, request_id)
            .filter(|snapshot| accept(snapshot))
        {
            return Some(snapshot);
        }
        if Instant::now() >= deadline {
            return None;
        }
        thread::sleep(Duration::from_millis(2));
    }
}

/// After a daemon restart every session of a root rebinds while that root's
/// configure tail runs one long unit that cannot step aside (artifact and view
/// loads on a large root). A rebind of an unchanged root only reads the
/// published configuration and records its session, so each of the herd must
/// be answered beside the running tail, not after it.
#[test]
fn bind_herd_is_answered_beside_a_same_root_tail_stuck_in_one_long_step() {
    const BINDS: usize = 16;
    let executor = test_executor(4, 2, 2, 2);
    let (_dir, root) = test_root("bind-herd-beside-tail");
    assert!(executor.register_actor(root.clone(), test_ctx()));

    let (tail_started_tx, tail_started_rx) = crossbeam_channel::bounded::<()>(1);
    let (release_tail_tx, release_tail_rx) = crossbeam_channel::bounded::<()>(1);
    let tail = executor.submit_maintenance_async(
        root.clone(),
        Lane::MaintenanceCommit,
        "subc-maintenance-drain-configure-tail-herd".to_string(),
        Box::new(move |_| {
            tail_started_tx.send(()).expect("signal tail start");
            // A tail stage that never polls for waiting writers, like a long
            // stage on a large root that cannot step aside.
            let _ = release_tail_rx.recv_timeout(Duration::from_secs(30));
            ok("tail")
        }),
    );
    tail_started_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("tail starts");

    let submitted_at = Instant::now();
    let (answered_tx, answered_rx) = crossbeam_channel::unbounded::<Duration>();
    let waiters = (0..BINDS)
        .map(|index| {
            let (bind, _token) = submit_repeatable_bind(
                &executor,
                &root,
                &format!("subc-bind-herd-{index}"),
                move |_| {
                    assert!(current_bind_gate_is_shared());
                    ok(format!("subc-bind-herd-{index}"))
                },
            );
            let answered = answered_tx.clone();
            thread::spawn(move || {
                if let Ok(response) = bind.blocking_recv() {
                    assert!(response.success);
                    let _ = answered.send(submitted_at.elapsed());
                }
            })
        })
        .collect::<Vec<_>>();
    let deadline = Instant::now() + BIND_STORM_ACK_BOUND;
    let mut latencies = Vec::new();
    while latencies.len() < BINDS {
        match answered_rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
            Ok(latency) => latencies.push(latency),
            Err(_) => break,
        }
    }
    let answered_beside_tail = latencies.len();

    let _ = release_tail_tx.send(());
    assert!(recv_async(tail, "herd tail").success);
    for waiter in waiters {
        waiter.join().expect("bind waiter");
    }
    assert_eq!(
        answered_beside_tail, BINDS,
        "only {answered_beside_tail} of {BINDS} same-root binds were answered within \
         {BIND_STORM_ACK_BOUND:?} while the tail held one long step: {latencies:?}"
    );
}

/// A bind admitted beside a tail that turns out to need exclusive use of the
/// actor hands its worker back and is queued again as a writer. The polling
/// tail sees that writer and steps aside, and the bind runs again, this time
/// exclusively, from the start.
///
/// The tail starts polling only after the bind's shared attempt has begun.
/// A freshly submitted bind already counts as a waiting writer while it sits
/// in the queue, so a tail that polls in the moment between the submit and
/// the scheduler admitting the bind steps aside for it; the bind then finds
/// the actor free and correctly runs exclusively on its first attempt, and
/// there is no shared attempt to observe. Waiting for the shared attempt
/// means the only writer the tail can see is the bind queued again.
#[test]
fn bind_needing_exclusive_use_is_rerun_after_a_polling_tail_steps_aside() {
    let executor = test_executor(4, 2, 2, 2);
    let (_dir, root) = test_root("bind-rerun-exclusive");
    assert!(executor.register_actor(root.clone(), test_ctx()));

    let (tail_started_tx, tail_started_rx) = crossbeam_channel::bounded::<()>(1);
    let (shared_attempt_tx, shared_attempt_rx) = crossbeam_channel::bounded::<()>(1);
    let tail = executor.submit_maintenance_async(
        root.clone(),
        Lane::MaintenanceCommit,
        "subc-maintenance-drain-configure-tail-rerun".to_string(),
        Box::new(move |_| {
            tail_started_tx.send(()).expect("signal tail start");
            // If the bind is never admitted beside the tail, give up waiting
            // and poll anyway: the bind then runs only exclusively and the
            // assertions below report it instead of the test hanging.
            let _ = shared_attempt_rx.recv_timeout(Duration::from_secs(5));
            for step in 0..500 {
                if current_actor_writer_waiting() {
                    return ok(format!("stepped-aside-after-{step}"));
                }
                thread::sleep(Duration::from_millis(20));
            }
            ok("ran-to-completion")
        }),
    );
    tail_started_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("tail starts");

    let runs = Arc::new(Mutex::new(Vec::<bool>::new()));
    let recorded = Arc::clone(&runs);
    let submitted_at = Instant::now();
    let (bind, _token) = submit_repeatable_bind(&executor, &root, "subc-bind-rerun", move |_| {
        let shared = current_bind_gate_is_shared();
        recorded.lock().push(shared);
        if shared {
            let _ = shared_attempt_tx.try_send(());
        }
        if request_exclusive_rerun() {
            return Response::error("subc-bind-rerun", "configure_needs_exclusive", "rerun");
        }
        ok("subc-bind-rerun")
    });
    let response = recv_async(bind, "rerun bind");
    let waited = submitted_at.elapsed();
    let tail_response = recv_async(tail, "rerun tail");

    assert!(response.success, "{:?}", response.data);
    assert_eq!(
        *runs.lock(),
        vec![true, false],
        "shared attempt, then exclusive"
    );
    assert!(
        tail_response.id.starts_with("stepped-aside-after-"),
        "the tail must observe the requeued bind: {}",
        tail_response.id
    );
    assert!(
        waited < Duration::from_secs(1),
        "the bind waited {waited:?}"
    );
}

/// A cancel after the token returns to pending but before the worker's full
/// probe must be settled by the worker without queuing exclusive writer demand.
#[test]
fn bind_cancelled_during_the_requeue_handoff_is_settled_without_queuing() {
    let executor = Arc::new(test_executor(4, 2, 2, 2));
    let (_dir, root) = test_root("bind-requeue-handoff-cancel");
    assert!(executor.register_actor(root.clone(), test_ctx()));
    let (_epoch, waiting_writers, _detached) = actor_epoch_and_demand(&executor, &root);

    let (tail_started_tx, tail_started_rx) = crossbeam_channel::bounded::<()>(1);
    let (release_tail_tx, release_tail_rx) = crossbeam_channel::bounded::<()>(1);
    let tail = executor.submit_maintenance_async(
        root.clone(),
        Lane::MaintenanceCommit,
        "subc-maintenance-drain-configure-tail-handoff".to_string(),
        Box::new(move |_| {
            tail_started_tx.send(()).expect("signal tail start");
            let _ = release_tail_rx.recv_timeout(Duration::from_secs(30));
            ok("tail")
        }),
    );
    tail_started_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("tail starts");

    let (token_tx, token_rx) = crossbeam_channel::bounded::<JobCancellation>(1);
    let (cancelled_tx, cancelled_rx) = crossbeam_channel::bounded::<JobCancelOutcome>(1);
    {
        let executor = Arc::clone(&executor);
        let root = root.clone();
        set_requeue_handoff_hook_for_test("subc-bind-handoff", move || {
            let token = token_rx
                .recv_timeout(Duration::from_secs(5))
                .expect("bind token");
            let _ = cancelled_tx.send(executor.cancel_job(&root, &token));
        });
    }
    let (mut bind, token) = submit_repeatable_bind(&executor, &root, "subc-bind-handoff", |_| {
        if request_exclusive_rerun() {
            return Response::error("subc-bind-handoff", "configure_needs_exclusive", "rerun");
        }
        ok("subc-bind-handoff")
    });
    token_tx.send(token).expect("hand the token to the hook");
    let cancel = cancelled_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("the hook ran in the handoff window");

    // The tail still holds the actor, so cancellation must settle before execution.
    let deadline = Instant::now() + Duration::from_secs(2);
    let response = loop {
        if let Ok(response) = bind.try_recv() {
            break Some(response);
        }
        if Instant::now() >= deadline {
            break None;
        }
        thread::sleep(Duration::from_millis(2));
    };
    let demand_while_tail_held = waiting_writers.load(Ordering::Acquire);
    let _ = release_tail_tx.send(());
    assert!(recv_async(tail, "handoff tail").success);
    let response = match response {
        Some(response) => response,
        None => {
            let late = recv_async(bind, "late handoff bind");
            panic!(
                "the cancelled bind was answered only after the tail let go \
                 (writer demand while held: {demand_while_tail_held}): {:?}",
                late.data
            );
        }
    };

    assert_eq!(cancel, JobCancelOutcome::RunningSignalled);
    assert_eq!(response.data["code"], "request_cancelled");
    assert_eq!(
        demand_while_tail_held, 0,
        "no cancelled writer demand queued"
    );
}

/// Cancellation after the worker's full probe must be settled by the scheduler,
/// not left as exclusive writer demand behind an existing reader.
#[test]
fn bind_cancelled_after_the_worker_probe_is_settled_without_queuing() {
    let executor = Arc::new(test_executor(4, 2, 2, 2));
    let (_dir, root) = test_root("bind-after-worker-probe-cancel");
    assert!(executor.register_actor(root.clone(), test_ctx()));
    let (_epoch, waiting_writers, _detached) = actor_epoch_and_demand(&executor, &root);
    let reader = StuckReader::start(&executor, &root, "after-probe-reader", "read");
    let (parked_tx, parked_rx) = crossbeam_channel::bounded(1);
    let (release_tx, release_rx) = crossbeam_channel::bounded(1);
    set_requeue_after_probe_hook_for_test("subc-bind-after-worker-probe", move || {
        parked_tx.send(()).expect("signal post-probe handoff");
        let _ = release_rx.recv_timeout(Duration::from_secs(10));
    });
    let (mut bind, token) =
        submit_repeatable_bind(&executor, &root, "subc-bind-after-worker-probe", |_| {
            assert!(
                request_exclusive_rerun(),
                "the held reader forces a shared attempt"
            );
            ok("subc-bind-after-worker-probe")
        });
    parked_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("the worker's cancellation probe passed");
    let cancel = executor.cancel_job(&root, &token);
    release_tx.send(()).expect("release post-probe handoff");

    // The reader prevents an incorrectly queued exclusive rerun from answering.
    let response = try_recv_within(&mut bind, Duration::from_secs(2));
    let demand_while_reader_held = waiting_writers.load(Ordering::Acquire);
    let queued_while_reader_held = {
        let state = executor.inner.state.lock();
        state.actors[&root].has_queued_mutating_job("subc-bind-after-worker-probe")
    };
    assert!(reader.finish().success);
    let response = response.unwrap_or_else(|| {
        let late = recv_async(bind, "late post-probe cancellation");
        panic!(
            "scheduler left a cancelled post-probe requeue behind the reader \
             (writer demand: {demand_while_reader_held}, queued: {queued_while_reader_held}): {:?}",
            late.data
        );
    });
    assert_eq!(cancel, JobCancelOutcome::RunningSignalled);
    assert!(!response.success);
    assert_eq!(response.data["code"], "request_cancelled");
    assert_eq!(
        demand_while_reader_held, 0,
        "no cancelled writer demand queued"
    );
    assert!(!queued_while_reader_held, "cancelled bind was queued");
}

/// A bind sent back for exclusive use waits in the actor's queue, not on a
/// worker: every worker but the tail's stays idle, it counts as a waiting
/// writer, its diagnostics say why it waits, and cancelling it removes it
/// and its writer demand at once.
#[test]
fn requeued_bind_waits_off_worker_names_its_blockers_and_cancels_cleanly() {
    let executor = test_executor(4, 2, 2, 2);
    let (_dir, root) = test_root("bind-requeued-cancel");
    let ctx = test_ctx();
    assert!(executor.register_actor(root.clone(), Arc::clone(&ctx)));
    let (_epoch, waiting_writers, _detached) = actor_epoch_and_demand(&executor, &root);

    let (tail_started_tx, tail_started_rx) = crossbeam_channel::bounded::<()>(1);
    let (release_tail_tx, release_tail_rx) = crossbeam_channel::bounded::<()>(1);
    let tail = executor.submit_maintenance_async(
        root.clone(),
        Lane::MaintenanceCommit,
        "subc-maintenance-drain-configure-tail-cancel".to_string(),
        Box::new(move |ctx| {
            ctx.note_configure_tail_stage(Some("view_load"));
            tail_started_tx.send(()).expect("signal tail start");
            let _ = release_tail_rx.recv_timeout(Duration::from_secs(30));
            ctx.note_configure_tail_stage(None);
            ok("tail")
        }),
    );
    tail_started_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("tail starts");

    let (bind, token) = submit_repeatable_bind(&executor, &root, "subc-bind-requeued", |_| {
        if request_exclusive_rerun() {
            return Response::error("subc-bind-requeued", "configure_needs_exclusive", "rerun");
        }
        ok("subc-bind-requeued")
    });
    let snapshot = wait_for_bind_snapshot(
        &executor,
        &root,
        "subc-bind-requeued",
        Duration::from_secs(5),
        |snapshot| {
            snapshot.configure_state == "queued"
                && snapshot
                    .blockers
                    .iter()
                    .any(|blocker| blocker == "rerun_as_exclusive_writer")
        },
    );
    let idle_workers = executor.inner.state.lock().idle_workers;
    let total_workers = executor.inner.state.lock().config.total_workers();
    let demand_while_queued = waiting_writers.load(Ordering::Acquire);
    let cancel = executor.cancel_job(&root, &token);
    let demand_after_cancel = waiting_writers.load(Ordering::Acquire);
    let _ = release_tail_tx.send(());
    let response = recv_async(bind, "cancelled bind");
    assert!(recv_async(tail, "cancel tail").success);

    let snapshot = snapshot.expect("the bind is queued again for exclusive use");
    assert!(
        snapshot
            .configure_phase_timings
            .as_deref()
            .is_some_and(|phases| phases.contains("requeued_exclusive=")),
        "{:?}",
        snapshot.configure_phase_timings
    );
    assert!(
        snapshot
            .blockers
            .iter()
            .any(|blocker| blocker.starts_with("configure_tail_stage(stage=view_load,")),
        "{:?}",
        snapshot.blockers
    );
    assert_eq!(
        idle_workers,
        total_workers - 1,
        "only the tail may hold a worker"
    );
    assert_eq!(demand_while_queued, 1, "the queued bind is writer demand");
    assert_eq!(cancel, JobCancelOutcome::QueuedRemoved);
    assert_eq!(demand_after_cancel, 0);
    assert!(!response.success);
    assert_eq!(response.data["code"], "request_cancelled");
}

/// A bind waiting for the epoch write gate (held here the way a detached
/// view publication holds it) watches its cancellation, so a cancelled bind
/// frees its worker without waiting for the gate.
#[test]
fn cancelled_bind_waiting_for_the_epoch_frees_its_worker() {
    let executor = test_executor(4, 2, 2, 2);
    let (_dir, root) = test_root("bind-cancel-epoch-wait");
    assert!(executor.register_actor(root.clone(), test_ctx()));
    let (epoch, _waiting, _detached) = actor_epoch_and_demand(&executor, &root);
    let writer = epoch.write();

    let (bind, token) = executor.submit_cancellable_async(
        root.clone(),
        Lane::Mutating,
        "subc-bind-epoch-wait".to_string(),
        Box::new(|_| ok("subc-bind-epoch-wait")),
    );
    let running = wait_for_bind_snapshot(
        &executor,
        &root,
        "subc-bind-epoch-wait",
        Duration::from_secs(5),
        |snapshot| snapshot.configure_state == "running",
    );
    let cancel = executor.cancel_job(&root, &token);
    let mut bind = bind;
    let deadline = Instant::now() + Duration::from_secs(2);
    let response = loop {
        if let Ok(response) = bind.try_recv() {
            break Some(response);
        }
        if Instant::now() >= deadline {
            break None;
        }
        thread::sleep(Duration::from_millis(5));
    };
    drop(writer);

    assert!(running.is_some(), "the bind was dispatched");
    assert_eq!(cancel, JobCancelOutcome::RunningSignalled);
    let response = response.expect("a cancelled bind must not wait for the epoch");
    assert_eq!(response.data["code"], "request_cancelled");
}

/// A detached view publication holds the epoch write gate for its install,
/// and while it holds it may also wait for the process-wide publication lock
/// behind other roots' installs, so the hold can outlast one install. An
/// exclusive route bind that finds the gate held therefore backs off in the
/// actor's queue, holding no worker, and runs once the gate is free.
#[test]
fn repeatable_bind_blocked_by_a_detached_writer_backs_off_without_a_worker() {
    let executor = test_executor(4, 2, 2, 2);
    let (_dir, root) = test_root("bind-detached-writer-backoff");
    assert!(executor.register_actor(root.clone(), test_ctx()));
    let (epoch, _waiting, _detached) = actor_epoch_and_demand(&executor, &root);
    let writer = epoch.write();

    let runs = Arc::new(AtomicUsize::new(0));
    let counted = Arc::clone(&runs);
    let (bind, _token) = submit_repeatable_bind(&executor, &root, "subc-bind-backoff", move |_| {
        counted.fetch_add(1, Ordering::AcqRel);
        ok("subc-bind-backoff")
    });
    let deadline = Instant::now() + Duration::from_secs(2);
    let queued = loop {
        // Observe queue ownership and worker accounting under the same lock.
        // Retry attempts can consume a worker briefly, so their frequency in a
        // wall-clock sample is not evidence that a worker waits for the gate.
        {
            let state = executor.inner.state.lock();
            let snapshot = state.bind_blocker_snapshot(&root, "subc-bind-backoff");
            if snapshot.configure_state == "queued"
                && snapshot
                    .blockers
                    .iter()
                    .any(|blocker| blocker == "rerun_as_exclusive_writer")
            {
                break Some((state.idle_workers, state.config.total_workers()));
            }
        }
        if Instant::now() >= deadline {
            break None;
        }
        thread::sleep(Duration::from_millis(2));
    };
    let ran_while_held = runs.load(Ordering::Acquire);
    drop(writer);
    let response = recv_async(bind, "backoff bind");

    let (idle_workers, total_workers) =
        queued.expect("the bind waits in the queue, not on a worker");
    assert_eq!(
        idle_workers, total_workers,
        "a queued retry must own no worker"
    );
    assert_eq!(ran_while_held, 0);
    assert!(response.success);
    assert_eq!(runs.load(Ordering::Acquire), 1);
}

/// The epoch lock is task-fair, so a reader waits behind a parked writer even
/// while only readers hold it. With a detached view publication parked on the
/// write gate behind a running tail, a shared bind must not block taking its
/// hold: it is queued again as an exclusive writer at once. The parked
/// publication also counts as writer demand, so the tail can step aside.
#[test]
fn shared_bind_does_not_block_behind_a_parked_publication_writer() {
    let executor = test_executor(4, 2, 2, 2);
    let (_dir, root) = test_root("bind-parked-publication");
    assert!(executor.register_actor(root.clone(), test_ctx()));
    let (epoch, _waiting, detached) = actor_epoch_and_demand(&executor, &root);

    let (tail_started_tx, tail_started_rx) = crossbeam_channel::bounded::<()>(1);
    let (release_tail_tx, release_tail_rx) = crossbeam_channel::bounded::<()>(1);
    let (saw_writer_tx, saw_writer_rx) = crossbeam_channel::bounded::<()>(1);
    let tail = executor.submit_maintenance_async(
        root.clone(),
        Lane::MaintenanceCommit,
        "subc-maintenance-drain-configure-tail-publication".to_string(),
        Box::new(move |_| {
            tail_started_tx.send(()).expect("signal tail start");
            let mut saw_writer = false;
            loop {
                if !saw_writer && current_actor_writer_waiting() {
                    saw_writer = true;
                    let _ = saw_writer_tx.send(());
                }
                if release_tail_rx
                    .recv_timeout(Duration::from_millis(1))
                    .is_ok()
                {
                    break;
                }
            }
            ok(format!("saw-writer-{saw_writer}"))
        }),
    );
    tail_started_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("tail starts");

    // A detached publication, counted as the view-publication thread counts
    // itself, parks on the write gate behind the tail's read hold.
    let publication = {
        let epoch = Arc::clone(&epoch);
        let detached = Arc::clone(&detached);
        thread::spawn(move || {
            detached.fetch_add(1, Ordering::AcqRel);
            let guard = epoch.write();
            detached.fetch_sub(1, Ordering::AcqRel);
            drop(guard);
        })
    };
    let parked_deadline = Instant::now() + Duration::from_secs(5);
    while !epoch.is_locked_exclusive() {
        assert!(Instant::now() < parked_deadline, "publication never parked");
        thread::sleep(Duration::from_millis(1));
    }

    let runs = Arc::new(Mutex::new(Vec::<bool>::new()));
    let recorded = Arc::clone(&runs);
    let (bind, _token) =
        submit_repeatable_bind(&executor, &root, "subc-bind-publication", move |_| {
            recorded.lock().push(current_bind_gate_is_shared());
            ok("subc-bind-publication")
        });
    let requeued = wait_for_bind_snapshot(
        &executor,
        &root,
        "subc-bind-publication",
        Duration::from_secs(2),
        |snapshot| {
            snapshot.configure_state == "queued"
                && snapshot
                    .blockers
                    .iter()
                    .any(|blocker| blocker == "rerun_as_exclusive_writer")
        },
    );
    let tail_saw_writer = saw_writer_rx.recv_timeout(Duration::from_secs(2)).is_ok();
    let _ = release_tail_tx.send(());
    let tail_response = recv_async(tail, "publication tail");
    publication.join().expect("publication thread");
    let response = recv_async(bind, "publication bind");

    let requeued = requeued.expect("the bind was queued again instead of blocking on the lock");
    assert!(
        requeued
            .blockers
            .iter()
            .any(|blocker| blocker == "detached_view_publication_writer_waiting"),
        "{:?}",
        requeued.blockers
    );
    assert!(tail_saw_writer, "the tail sees writer demand");
    assert_eq!(tail_response.id, "saw-writer-true");
    assert!(response.success);
    assert_eq!(*runs.lock(), vec![false], "the bind ran once, exclusively");
}

/// The reserved bind workers run route-bind configures only. An edit queued
/// on the same root ahead of a bind must not ride the bind pass onto a
/// reserved worker (a long edit would then hold it); it waits for the general
/// pool, and the bind is admitted ahead of it.
#[test]
fn reserved_bind_workers_run_only_binds_and_the_bind_jumps_a_queued_edit() {
    let executor = test_executor(4, 2, 2, 2);
    let pool_size = executor.pool_size();
    let mut dirs = Vec::new();

    let (busy_started_tx, busy_started_rx) = crossbeam_channel::unbounded::<()>();
    let (release_busy_tx, release_busy_rx) = crossbeam_channel::unbounded::<()>();
    let mut busy = Vec::new();
    for index in 0..pool_size {
        let (dir, root) = test_root(&format!("reserve-only-busy-{index}"));
        assert!(executor.register_actor(root.clone(), test_ctx()));
        dirs.push(dir);
        let started = busy_started_tx.clone();
        let release = release_busy_rx.clone();
        busy.push(executor.submit_async(
            root,
            Lane::PureRead,
            format!("busy-{index}"),
            Box::new(move |_| {
                let _ = started.send(());
                let _ = release.recv_timeout(Duration::from_secs(30));
                ok("busy")
            }),
        ));
    }
    for occupied in 0..pool_size {
        busy_started_rx
            .recv_timeout(Duration::from_secs(5))
            .unwrap_or_else(|_| panic!("general worker {occupied} occupied"));
    }

    let (_dir, root) = test_root("reserve-only-target");
    assert!(executor.register_actor(root.clone(), test_ctx()));
    let edit_started = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let edit_flag = Arc::clone(&edit_started);
    let (release_edit_tx, release_edit_rx) = crossbeam_channel::bounded::<()>(1);
    let edit = executor.submit_async(
        root.clone(),
        Lane::Mutating,
        "edit-before-bind".to_string(),
        Box::new(move |_| {
            edit_flag.store(true, Ordering::SeqCst);
            let _ = release_edit_rx.recv_timeout(Duration::from_secs(30));
            ok("edit")
        }),
    );
    let (bind_started_tx, bind_started_rx) = crossbeam_channel::bounded::<bool>(1);
    let edit_seen_by_bind = Arc::clone(&edit_started);
    let bind = executor.submit_async(
        root,
        Lane::Mutating,
        "subc-bind-after-edit".to_string(),
        Box::new(move |_| {
            let _ = bind_started_tx.send(edit_seen_by_bind.load(Ordering::SeqCst));
            ok("bind")
        }),
    );

    let bind_start = bind_started_rx.recv_timeout(Duration::from_secs(1));
    // Hold the general pool a moment longer: the edit must not have found a
    // worker while only the reserved ones were free.
    thread::sleep(Duration::from_millis(100));
    let edit_ran_on_reserve = edit_started.load(Ordering::SeqCst);
    for _ in 0..pool_size {
        let _ = release_busy_tx.send(());
    }
    let _ = release_edit_tx.send(());
    assert!(recv_async(bind, "bind behind edit").success);
    assert!(recv_async(edit, "edit ahead of bind").success);
    for handle in busy {
        let _ = recv_async(handle, "busy read");
    }

    assert_eq!(
        bind_start,
        Ok(false),
        "the bind must start within 1s on a reserved worker, before the queued edit"
    );
    assert!(
        !edit_ran_on_reserve,
        "an edit must never start on a reserved bind worker"
    );
    assert_eq!(dirs.len(), pool_size);
}

/// A same-root read held on its worker and reported to the scheduler as
/// having run for longer than `READER_STUCK_CENSUS_AGE`, as an indexed grep
/// stuck on a pathological file is. Released when dropped.
struct StuckReader {
    response: Option<tokio::sync::oneshot::Receiver<Response>>,
    finished: Option<Response>,
    release: crossbeam_channel::Sender<()>,
}

impl StuckReader {
    fn start(executor: &Executor, root: &ProjectRootId, request_id: &str, tool: &str) -> Self {
        let (started_tx, started_rx) = crossbeam_channel::bounded::<()>(1);
        let (release_tx, release_rx) = crossbeam_channel::bounded::<()>(1);
        let id = request_id.to_string();
        let (response, _token) = executor.submit_tool_call_cancellable_async(
            root.clone(),
            Lane::PureRead,
            request_id.to_string(),
            tool,
            Box::new(move |_| {
                started_tx.send(()).expect("signal reader start");
                let _ = release_rx.recv_timeout(Duration::from_secs(30));
                ok(id)
            }),
        );
        started_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("the reader starts");
        {
            let mut state = executor.inner.state.lock();
            let running = state
                .running_jobs
                .values_mut()
                .find(|job| job.root_id == *root && job.request_id == request_id)
                .expect("running reader census entry");
            running.started_at = Instant::now()
                .checked_sub(READER_STUCK_CENSUS_AGE + Duration::from_secs(1))
                .expect("monotonic clock is older than the stuck-reader age");
        }
        Self {
            response: Some(response),
            finished: None,
            release: release_tx,
        }
    }

    /// True while the reader's job has not answered.
    fn still_running(&mut self) -> bool {
        if self.finished.is_some() {
            return false;
        }
        match self.response.as_mut().map(|response| response.try_recv()) {
            Some(Ok(response)) => {
                self.finished = Some(response);
                false
            }
            _ => true,
        }
    }

    fn finish(mut self) -> Response {
        let _ = self.release.send(());
        if let Some(response) = self.finished.take() {
            return response;
        }
        recv_async(
            self.response.take().expect("reader response"),
            "stuck reader",
        )
    }
}

impl Drop for StuckReader {
    fn drop(&mut self) {
        // A failed assertion must not leave the reader parked on its worker.
        let _ = self.release.send(());
    }
}

/// The response if it arrives within `within`, without consuming the
/// receiver otherwise.
fn try_recv_within(
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

fn is_stuck_reader_blocker(blocker: &str) -> bool {
    blocker.starts_with("waiting_on_readers_stuck(")
}

/// A rebind of an unchanged root needs only a shared hold, so a read that has
/// run far longer than the route-bind deadline must not hold it up: the bind
/// is answered beside the reader, within the bound a bind storm is answered
/// in, while the reader is still running.
#[test]
fn shared_bind_is_answered_beside_a_reader_held_past_the_bind_deadline() {
    let executor = test_executor(6, 3, 4, 2);
    let (_dir, root) = test_root("shared-bind-beside-stuck-reader");
    assert!(executor.register_actor(root.clone(), test_ctx()));
    let mut reader = StuckReader::start(&executor, &root, "subc-1-789", "grep");

    let (mut bind, _token) =
        submit_repeatable_bind(&executor, &root, "subc-bind-beside-reader", |_| {
            Response::success(
                "subc-bind-beside-reader",
                serde_json::json!({ "shared": current_bind_gate_is_shared() }),
            )
        });
    let answered = try_recv_within(&mut bind, BIND_STORM_ACK_BOUND);
    let reader_held_at_answer = reader.still_running();
    let blockers = executor
        .try_bind_blocker_snapshot(&root, "subc-bind-beside-reader")
        .map(|snapshot| snapshot.blockers);
    let reader_response = reader.finish();
    if answered.is_none() {
        let _ = recv_async(bind, "bind after the reader released");
    }

    let answered = answered.unwrap_or_else(|| {
        panic!(
            "the bind was not answered within {BIND_STORM_ACK_BOUND:?} beside a stuck reader: \
             {blockers:?}"
        )
    });
    assert!(answered.success, "{:?}", answered.data);
    assert_eq!(
        answered.data["shared"], true,
        "the bind held the gate shared"
    );
    assert!(reader_held_at_answer, "the reader was still running");
    assert!(reader_response.success);
}

/// While a shared bind runs beside a stuck reader, the root keeps serving new
/// reads, including after the bind has waited past the age at which a queued
/// bind starts holding new reads back.
#[test]
fn new_reads_are_admitted_while_a_shared_bind_and_a_stuck_reader_overlap() {
    let executor = test_executor(6, 3, 4, 2);
    let (_dir, root) = test_root("reads-beside-shared-bind-and-stuck-reader");
    assert!(executor.register_actor(root.clone(), test_ctx()));
    let reader = StuckReader::start(&executor, &root, "subc-1-789", "grep");

    let (release_bind_tx, release_bind_rx) = crossbeam_channel::bounded::<()>(1);
    let (bind, _token) = submit_repeatable_bind(&executor, &root, "subc-bind-overlap", move |_| {
        let _ = release_bind_rx.recv_timeout(Duration::from_secs(30));
        ok("subc-bind-overlap")
    });
    thread::sleep(BIND_PROMOTION_AGE + Duration::from_millis(200));

    let (mut read, _read_token) = executor.submit_tool_call_cancellable_async(
        root.clone(),
        Lane::PureRead,
        "subc-1-790".to_string(),
        "read",
        Box::new(|_| ok("subc-1-790")),
    );
    let read_answered = try_recv_within(&mut read, Duration::from_secs(2));
    let blockers = executor
        .try_bind_blocker_snapshot(&root, "subc-bind-overlap")
        .map(|snapshot| snapshot.blockers);
    let _ = release_bind_tx.send(());
    let reader_response = reader.finish();
    let bind_response = recv_async(bind, "overlapping bind");

    let read_answered = read_answered
        .unwrap_or_else(|| panic!("a new read was held back on the root: {blockers:?}"));
    assert!(read_answered.success);
    assert!(bind_response.success, "{:?}", bind_response.data);
    assert!(reader_response.success);
}

/// A bind that must change the root still cannot run beside a reader. It
/// waits in the queue, off any worker, names the stuck reader (job and tool)
/// as its blocker, never runs exclusively while the reader holds the root,
/// and cancelling it removes it and its writer demand at once.
#[test]
fn exclusive_bind_beside_a_held_reader_waits_names_its_blocker_and_cancels_cleanly() {
    let executor = test_executor(6, 3, 4, 2);
    let (_dir, root) = test_root("exclusive-bind-beside-stuck-reader");
    assert!(executor.register_actor(root.clone(), test_ctx()));
    let (_epoch, waiting_writers, _detached) = actor_epoch_and_demand(&executor, &root);
    let reader = StuckReader::start(&executor, &root, "subc-1-791", "grep");

    let runs = Arc::new(Mutex::new(Vec::<bool>::new()));
    let recorded = Arc::clone(&runs);
    let (bind, token) =
        submit_repeatable_bind(&executor, &root, "subc-bind-needs-exclusive", move |_| {
            recorded.lock().push(current_bind_gate_is_shared());
            if request_exclusive_rerun() {
                return Response::error(
                    "subc-bind-needs-exclusive",
                    "configure_needs_exclusive",
                    "rerun",
                );
            }
            ok("subc-bind-needs-exclusive")
        });
    let snapshot = wait_for_bind_snapshot(
        &executor,
        &root,
        "subc-bind-needs-exclusive",
        Duration::from_secs(5),
        |snapshot| {
            snapshot.configure_state == "queued"
                && snapshot
                    .oldest_queued_writer_age_ms
                    .is_some_and(|age| age >= duration_millis_u64(BIND_PROMOTION_AGE) + 100)
                && snapshot
                    .blockers
                    .iter()
                    .any(|blocker| is_stuck_reader_blocker(blocker))
        },
    );
    let (idle_workers, total_workers) = {
        let state = executor.inner.state.lock();
        (state.idle_workers, state.config.total_workers())
    };
    let runs_while_reader_held = runs.lock().clone();
    let demand_while_queued = waiting_writers.load(Ordering::Acquire);
    let cancel = executor.cancel_job(&root, &token);
    let demand_after_cancel = waiting_writers.load(Ordering::Acquire);
    let response = recv_async(bind, "cancelled exclusive bind");
    let reader_response = reader.finish();

    let snapshot = snapshot.expect("the bind waits behind the stuck reader");
    let blocker = snapshot
        .blockers
        .iter()
        .find(|blocker| is_stuck_reader_blocker(blocker))
        .expect("stuck-reader blocker");
    assert!(blocker.contains("job=subc-1-791"), "{blocker}");
    assert!(blocker.contains("command=grep"), "{blocker}");
    assert!(
        runs_while_reader_held.iter().all(|shared| *shared),
        "the bind ran exclusively beside the reader: {runs_while_reader_held:?}"
    );
    assert_eq!(
        idle_workers,
        total_workers - 1,
        "only the reader may hold a worker"
    );
    assert_eq!(demand_while_queued, 1, "the queued bind is writer demand");
    assert_eq!(cancel, JobCancelOutcome::QueuedRemoved);
    assert_eq!(demand_after_cancel, 0);
    assert!(!response.success);
    assert_eq!(response.data["code"], "request_cancelled");
    assert!(reader_response.success);
}

/// A bind that needs exclusive use and has waited past its promotion age
/// would normally stop new reads so the readers ahead of it drain. When every
/// reader ahead of it is stuck, that only freezes the root: new reads must
/// still be admitted, and the blocker the bind reports (what the module names
/// in its refusal when the bind deadline passes) names the stuck job and its
/// tool.
#[test]
fn promoted_bind_does_not_freeze_reads_behind_a_stuck_reader() {
    let executor = test_executor(6, 3, 4, 2);
    let (_dir, root) = test_root("promoted-bind-stuck-reader");
    assert!(executor.register_actor(root.clone(), test_ctx()));
    let reader = StuckReader::start(&executor, &root, "subc-1-792", "grep");

    let (bind, token) = submit_repeatable_bind(&executor, &root, "subc-bind-promoted", |_| {
        if request_exclusive_rerun() {
            return Response::error("subc-bind-promoted", "configure_needs_exclusive", "rerun");
        }
        ok("subc-bind-promoted")
    });
    let snapshot = wait_for_bind_snapshot(
        &executor,
        &root,
        "subc-bind-promoted",
        Duration::from_secs(5),
        |snapshot| {
            snapshot.configure_state == "queued"
                && snapshot
                    .oldest_queued_writer_age_ms
                    .is_some_and(|age| age >= duration_millis_u64(BIND_PROMOTION_AGE) + 100)
        },
    );
    let (mut read, _read_token) = executor.submit_tool_call_cancellable_async(
        root.clone(),
        Lane::PureRead,
        "subc-1-793".to_string(),
        "read",
        Box::new(|_| ok("subc-1-793")),
    );
    let read_answered = try_recv_within(&mut read, Duration::from_secs(2));
    let cancel = executor.cancel_job(&root, &token);
    let bind_response = recv_async(bind, "cancelled promoted bind");
    let reader_response = reader.finish();
    if read_answered.is_none() {
        let _ = recv_async(read, "read after the bind was cancelled");
    }

    let snapshot = snapshot.expect("the bind waits past its promotion age");
    let read_answered = read_answered.unwrap_or_else(|| {
        panic!(
            "a waiting bind froze new reads behind a stuck reader: {:?}",
            snapshot.blockers
        )
    });
    assert!(read_answered.success);
    let stuck = snapshot
        .in_flight_readers
        .iter()
        .find(|reader| reader.request_id == "subc-1-792")
        .expect("the stuck reader is reported");
    assert_eq!(stuck.command, "grep");
    assert!(stuck.started_before_oldest_writer);
    assert!(stuck.started_age_ms >= duration_millis_u64(READER_STUCK_CENSUS_AGE));
    assert_eq!(cancel, JobCancelOutcome::QueuedRemoved);
    assert_eq!(bind_response.data["code"], "request_cancelled");
    assert!(reader_response.success);
}

#[test]
fn quiesced_root_past_grace_signals_running_maintenance_and_refuses_late_work() {
    let (_dir, root) = test_root("quiesce-running");
    let ctx = test_ctx();
    ctx.set_unbound_build_abandon_grace_for_test(Duration::ZERO);
    let executor = test_executor(2, 2, 2, 1);
    executor.register_actor(root.clone(), Arc::clone(&ctx));
    let (started_tx, started_rx) = crossbeam_channel::bounded(1);
    let (release_tx, release_rx) = crossbeam_channel::bounded(1);
    let running = executor.submit_maintenance_async(
        root.clone(),
        Lane::HeavyInit,
        "running".into(),
        Box::new(move |_| {
            started_tx.send(()).unwrap();
            release_rx.recv_timeout(Duration::from_secs(10)).unwrap();
            assert!(
                current_job_cancelled(),
                "running maintenance must observe quiesce"
            );
            ok("running")
        }),
    );
    started_rx.recv_timeout(Duration::from_secs(10)).unwrap();
    ctx.mark_subc_unbound();
    assert_eq!(executor.cancel_queued_root_maintenance(&root), 0);
    let late = executor.submit_maintenance_async(
        root.clone(),
        Lane::HeavyInit,
        "late".into(),
        Box::new(|_| panic!("unbound root admitted maintenance")),
    );
    assert!(!recv_async(late, "late unbound maintenance").success);
    release_tx.send(()).unwrap();
    assert!(recv_async(running, "cancelled running maintenance").success);
    ctx.mark_subc_bound();
    let rebound = executor.submit_maintenance_async(
        root,
        Lane::HeavyInit,
        "rebound".into(),
        Box::new(|_| {
            assert!(!current_job_cancelled());
            ok("rebound")
        }),
    );
    assert!(recv_async(rebound, "rebound maintenance").success);
}

#[test]
fn deleted_root_aborts_running_maintenance_without_a_reaper() {
    let (dir, root) = test_root("deleted-running");
    let executor = test_executor(2, 2, 2, 1);
    executor.register_actor(root.clone(), test_ctx());
    let (started_tx, started_rx) = crossbeam_channel::bounded(1);
    let (release_tx, release_rx) = crossbeam_channel::bounded(1);
    let running = executor.submit_maintenance_async(
        root.clone(),
        Lane::HeavyInit,
        "deleted".into(),
        Box::new(move |_| {
            started_tx.send(()).unwrap();
            release_rx.recv_timeout(Duration::from_secs(10)).unwrap();
            assert!(
                current_job_cancelled(),
                "deletion must abort at the next checkpoint"
            );
            ok("deleted")
        }),
    );
    started_rx.recv_timeout(Duration::from_secs(10)).unwrap();
    dir.close().unwrap();
    release_tx.send(()).unwrap();
    assert!(recv_async(running, "deleted root maintenance").success);
    let late = executor.submit_maintenance_async(
        root,
        Lane::HeavyInit,
        "late".into(),
        Box::new(|_| panic!("missing root admitted maintenance")),
    );
    assert!(!recv_async(late, "late deleted maintenance").success);
}

#[test]
fn quiesced_root_rebound_within_grace_preserves_running_maintenance() {
    let (_dir, root) = test_root("short-unbind");
    let ctx = test_ctx();
    ctx.set_unbound_build_abandon_grace_for_test(Duration::from_secs(600));
    ctx.mark_subc_bound();
    let executor = test_executor(2, 2, 2, 1);
    executor.register_actor(root.clone(), Arc::clone(&ctx));
    let (started_tx, started_rx) = crossbeam_channel::bounded(1);
    let (release_tx, release_rx) = crossbeam_channel::bounded(1);
    let running = executor.submit_maintenance_async(
        root.clone(),
        Lane::HeavyInit,
        "short-unbind".into(),
        Box::new(move |_| {
            started_tx.send(()).unwrap();
            release_rx.recv_timeout(Duration::from_secs(10)).unwrap();
            assert!(
                !current_job_cancelled(),
                "short rebind must preserve running maintenance"
            );
            ok("short-unbind")
        }),
    );
    started_rx.recv_timeout(Duration::from_secs(10)).unwrap();
    ctx.mark_subc_unbound();
    executor.cancel_queued_root_maintenance(&root);
    ctx.mark_subc_bound();
    release_tx.send(()).unwrap();
    assert!(recv_async(running, "short rebind maintenance").success);
}

#[test]
fn provider_metadata_reservation_bypasses_index_writers_and_serializes_each_root() {
    let executor = test_executor(2, 1, 1, 1);
    let roots: Vec<_> = (0..4)
        .map(|i| test_root(&format!("provider-reserve-{i}")))
        .collect();
    for (_, root) in &roots {
        executor.register_actor(root.clone(), test_ctx());
    }
    let (entered, running) = crossbeam_channel::unbounded();
    let mut releases = Vec::new();
    let mut held_jobs = Vec::new();
    for (index, lane, id) in [
        (0, Lane::Mutating, "held-writer"),
        (1, Lane::HeavyInit, "held-index"),
        (2, Lane::Mutating, "subc-bind-provider-a"),
        (3, Lane::Mutating, "subc-bind-provider-b"),
    ] {
        let (release, held) = crossbeam_channel::bounded::<()>(1);
        releases.push(release);
        let entered = entered.clone();
        held_jobs.push(executor.submit(
            roots[index].1.clone(),
            lane,
            id.into(),
            Box::new(move |_| {
                let _ = entered.send(id);
                let _ = held.recv();
                ok(id)
            }),
        ));
    }
    for _ in 0..4 {
        running
            .recv_timeout(Duration::from_secs(3))
            .expect("general and bind slots occupied");
    }
    let mut provider_releases = Vec::new();
    let mut provider_jobs = Vec::new();
    for index in 0..2 {
        let (release, held) = crossbeam_channel::bounded::<()>(1);
        provider_releases.push(release);
        let entered = entered.clone();
        provider_jobs.push(executor.submit(
            roots[index].1.clone(),
            Lane::ProviderMetadata,
            format!("provider-{index}"),
            Box::new(move |_| {
                let _ = entered.send("provider");
                let _ = held.recv();
                ok("provider")
            }),
        ));
    }
    for _ in 0..2 {
        running
            .recv_timeout(Duration::from_millis(500))
            .expect("metadata bypasses held index, writer, and bind slots");
    }
    assert_eq!(
        executor.inner.state.lock().provider_inflight,
        PROVIDER_RESERVE_WORKERS
    );
    let second_ran = Arc::new(AtomicBool::new(false));
    let probe = second_ran.clone();
    let second = executor.submit(
        roots[0].1.clone(),
        Lane::ProviderMetadata,
        "same-root-provider".into(),
        Box::new(move |_| {
            probe.store(true, Ordering::SeqCst);
            ok("same-root")
        }),
    );
    let (release_third, held_third) = crossbeam_channel::bounded::<()>(1);
    let entered_third = entered.clone();
    let third = executor.submit(
        roots[2].1.clone(),
        Lane::ProviderMetadata,
        "third-root-provider".into(),
        Box::new(move |_| {
            let _ = entered_third.send("third-root");
            let _ = held_third.recv();
            ok("third-root")
        }),
    );
    assert!(
        running.try_recv().is_err(),
        "metadata reservation must not exceed two jobs"
    );
    provider_releases[1].send(()).unwrap();
    assert_eq!(
        running.recv_timeout(Duration::from_millis(500)).unwrap(),
        "third-root"
    );
    assert!(
        !second_ran.load(Ordering::SeqCst),
        "same-root policy operations must not overlap"
    );
    provider_releases[0].send(()).unwrap();
    assert!(second.recv_timeout(Duration::from_secs(2)).unwrap().success);
    release_third.send(()).unwrap();
    for release in releases {
        release.send(()).unwrap();
    }
    for job in held_jobs.into_iter().chain(provider_jobs).chain([third]) {
        assert!(job.recv_timeout(Duration::from_secs(3)).unwrap().success);
    }
}

#[test]
fn provider_metadata_quiesce_cancels_queued_work_but_preserves_a_started_commit() {
    let executor = test_executor(2, 1, 1, 1);
    let (_dir, root) = test_root("provider-quiesce");
    let ctx = test_ctx();
    executor.register_actor(root.clone(), ctx.clone());
    let (entered, running) = crossbeam_channel::bounded(1);
    let (release, held) = crossbeam_channel::bounded::<()>(1);
    let started = executor.submit(
        root.clone(),
        Lane::ProviderMetadata,
        "started-provider".into(),
        Box::new(move |_| {
            let _ = entered.send(());
            let _ = held.recv();
            assert!(!current_job_cancelled());
            ok("started-provider")
        }),
    );
    running.recv_timeout(Duration::from_secs(2)).unwrap();
    assert!(!executor.actor_is_idle(&root));
    assert!(!executor.retire_idle_actor_in_background(&root));
    let ran = Arc::new(AtomicBool::new(false));
    let probe = ran.clone();
    let queued = executor.submit(
        root.clone(),
        Lane::ProviderMetadata,
        "queued-provider".into(),
        Box::new(move |_| {
            probe.store(true, Ordering::SeqCst);
            ok("queued")
        }),
    );
    assert_eq!(
        executor.cancel_queued_maintenance(&root),
        0,
        "standing-root maintenance cancellation must not cancel a bound session's metadata"
    );
    assert!(
        executor
            .submit(
                root.clone(),
                Lane::PureRead,
                "read-beside-metadata".into(),
                Box::new(|_| ok("read"))
            )
            .recv_timeout(Duration::from_secs(1))
            .unwrap()
            .success,
        "metadata must not spend the root's ordinary actor capacity"
    );
    ctx.mark_subc_unbound();
    assert_eq!(executor.cancel_queued_root_maintenance(&root), 1);
    assert_eq!(
        queued.recv_timeout(Duration::from_secs(1)).unwrap().data["code"],
        "request_cancelled"
    );
    let probe = ran.clone();
    let late = executor.submit(
        root.clone(),
        Lane::ProviderMetadata,
        "unbound-provider".into(),
        Box::new(move |_| {
            probe.store(true, Ordering::SeqCst);
            ok("late")
        }),
    );
    assert_eq!(
        late.recv_timeout(Duration::from_secs(1)).unwrap().data["code"],
        "request_cancelled"
    );
    ctx.mark_subc_bound();
    release.send(()).unwrap();
    assert!(
        started
            .recv_timeout(Duration::from_secs(2))
            .unwrap()
            .success
    );
    assert!(!ran.load(Ordering::SeqCst));
    assert!(
        executor
            .submit(
                root,
                Lane::ProviderMetadata,
                "rebound-provider".into(),
                Box::new(|_| ok("rebound"))
            )
            .recv_timeout(Duration::from_secs(2))
            .unwrap()
            .success
    );
}

#[test]
fn dispatched_provider_metadata_cannot_start_after_unbind_and_rebind() {
    let executor = test_executor(2, 1, 1, 1);
    let (_dir, root) = test_root("provider-dispatched-unbind");
    let ctx = test_ctx();
    executor.register_actor(root.clone(), ctx.clone());
    let token = JobCancellation::new();
    let (entered, running) = crossbeam_channel::bounded(1);
    let (release, held) = crossbeam_channel::bounded(1);
    *token.inner.root_probe_barrier.lock() = Some((entered, held));
    let ran = Arc::new(AtomicBool::new(false));
    let probe = ran.clone();
    let response = executor.submit_tool_call_with_cancellation_async(
        root.clone(),
        Lane::ProviderMetadata,
        "dispatched-provider".into(),
        "tool.catalog",
        Box::new(move |_| {
            probe.store(true, Ordering::SeqCst);
            ok("provider")
        }),
        token,
    );
    running.recv_timeout(Duration::from_secs(2)).unwrap();
    ctx.mark_subc_unbound();
    ctx.mark_subc_bound();
    release.send(()).unwrap();
    let response = recv_async(response, "provider fenced by unbind");
    assert_eq!(response.data["code"], "request_cancelled");
    assert!(!ran.load(Ordering::SeqCst));
    assert!(
        executor
            .submit(
                root,
                Lane::ProviderMetadata,
                "after-provider-unbind".into(),
                Box::new(|_| ok("after"))
            )
            .recv_timeout(Duration::from_secs(2))
            .unwrap()
            .success
    );
}
