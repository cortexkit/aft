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
        ..ExecutorConfig::default()
    })
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
                release_rx
                    .recv_timeout(Duration::from_secs(2))
                    .expect("release read job");
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
    assert!(started_rx.recv_timeout(Duration::from_millis(75)).is_err());

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
                release_rx
                    .recv_timeout(Duration::from_secs(2))
                    .expect("release heavy job");
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
    assert!(started_rx.recv_timeout(Duration::from_millis(75)).is_err());

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
                release_rx
                    .recv_timeout(Duration::from_secs(2))
                    .expect("release injected heavy delay");
                ok(format!("heavy-storm-{index}"))
            }),
        ));
    }
    heavy_started_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("one HeavyInit job starts");
    assert!(
        heavy_started_rx
            .recv_timeout(Duration::from_millis(75))
            .is_err(),
        "the HeavyInit cap must leave one worker idle"
    );

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
            .get_mut(&(root.clone(), "abandoned-inspect".to_string()))
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
    let inspect = executor.submit(
        root.clone(),
        Lane::SerialLspStatus,
        "inspect-held-writer-lease".to_string(),
        Box::new(move |_| {
            inspect_started_tx.send(()).expect("inspect starts");
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
        .recv_timeout(Duration::from_secs(1))
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
        bind_started_rx
            .recv_timeout(Duration::from_millis(50))
            .is_err(),
        "bind must wait while inspect still occupies the LSP lane"
    );

    let inspect_response = inspect
        .recv_timeout(Duration::from_secs(3))
        .expect("inspect reaches its writer lease deadline");
    assert!(!inspect_response.success);
    assert_eq!(inspect_response.data["code"], "writer_lease_timeout");
    bind_started_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("bind starts after inspect reports the timeout");
    bind.recv_timeout(Duration::from_secs(1))
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
    let _ = flush_callgraph_store_refreshes_with_budget(Duration::from_secs(1));
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
            callgraph_store: true,
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
    assert!(flush_callgraph_store_refreshes_with_budget(
        Duration::from_secs(1)
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
        deadline: None,
        maintenance_coalesce_key: None,
    };
    let read_job = QueuedJob {
        request_id: "read-1".to_string(),
        command: "executor::Interactive::PureRead".to_string(),
        job: Box::new(|_ctx| ok("read-1")),
        completion: CompletionSender::Sync(tx),
        queued_at: now,
        cancellation: None,
        deadline: None,
        maintenance_coalesce_key: None,
    };
    // Read arrived FIRST in arrival order; the starved bind must still win.
    actor.push_job(JobClass::Interactive, Lane::PureRead, read_job);
    actor.push_job(JobClass::Interactive, Lane::Mutating, bind_job);

    assert_eq!(
        actor
            .class_queues(JobClass::Interactive)
            .next_interactive_lane(Instant::now()),
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
            deadline: None,
            maintenance_coalesce_key: None,
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
            deadline: None,
            maintenance_coalesce_key: None,
        },
    );

    assert_eq!(
        actor
            .class_queues(JobClass::Interactive)
            .next_interactive_lane(Instant::now()),
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
            deadline: None,
            maintenance_coalesce_key: None,
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
            deadline: None,
            maintenance_coalesce_key: None,
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
            deadline: None,
            maintenance_coalesce_key: None,
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
            deadline: None,
            maintenance_coalesce_key: None,
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
            deadline: None,
            maintenance_coalesce_key: None,
        },
    );

    let (removed_class, removed) = actor
        .remove_queued_cancellable(&m2_token)
        .expect("m2 removed");
    assert_eq!(removed_class, JobClass::Interactive);
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
fn queued_deadline_job_is_pruned_and_counted_at_next_turn() {
    // A queued job whose deadline elapses while blocked must be settled by the
    // scheduler (not executed) and counted as a deadline expiry.
    let executor = test_executor(2, 1, 1, 1);
    let (_dir, root) = test_root("prune-queued");
    executor.register_actor(root.clone(), test_ctx());

    let (started_tx, started_rx) = crossbeam_channel::bounded(1);
    let (release_tx, release_rx) = crossbeam_channel::bounded(1);
    let blocker = executor.submit(
        root.clone(),
        Lane::Mutating,
        "prune-blocker".to_string(),
        Box::new(move |_| {
            started_tx.send(()).expect("signal start");
            release_rx
                .recv_timeout(Duration::from_secs(5))
                .expect("release");
            ok("prune-blocker")
        }),
    );
    started_rx
        .recv_timeout(Duration::from_secs(2))
        .expect("prune blocker starts");

    // A queued reader with an already-tight deadline; the writer blocker holds
    // the actor so this job stays queued until the next scheduler turn prunes it.
    let executed = Arc::new(AtomicUsize::new(0));
    let executed_probe = Arc::clone(&executed);
    let (rx, _token) = executor.submit_cancellable_async_with_deadline(
        root.clone(),
        Lane::PureRead,
        "prune-victim".to_string(),
        Box::new(move |_| {
            executed_probe.fetch_add(1, Ordering::AcqRel);
            ok("prune-victim")
        }),
        Some(Instant::now() + Duration::from_millis(50)),
    );
    // Let the victim's deadline elapse while the blocker still holds the actor.
    // Releasing the blocker then gives the scheduler a completion event; the
    // next turn prunes the elapsed victim and settles it with
    // request_deadline_exceeded instead of executing it.
    thread::sleep(Duration::from_millis(120));

    // Releasing the blocker completes the writer; the completion event wakes
    // the scheduler and the next turn prunes the elapsed victim, settling it
    // with request_deadline_exceeded instead of executing it.
    release_tx.send(()).expect("release prune blocker");
    assert!(
        blocker
            .recv_timeout(Duration::from_secs(5))
            .expect("prune blocker completes")
            .success
    );

    let response = recv_async(rx, "pruned job completion");
    assert!(!response.success);
    assert_eq!(response.data["code"], "request_deadline_exceeded");
    assert_eq!(executed.load(Ordering::Acquire), 0);
}

#[test]
fn interactive_queue_cap_returns_typed_backpressure_per_actor_and_global() {
    // pool 2 / actor_cap 1: one running blocker per actor, then the per-actor
    // interactive cap admits 2 more; the next is rejected with the actor scope.
    // A second actor's global budget is sized so its first overflow reports the
    // global scope.
    let executor = test_executor(2, 1, 1, 1);
    let (_dir_a, root_a) = test_root("interactive-cap-a");
    executor.register_actor(root_a.clone(), test_ctx());

    let (blocker_started_tx, blocker_started_rx) = crossbeam_channel::bounded(1);
    let (release_blocker_tx, release_blocker_rx) = crossbeam_channel::bounded(1);
    let blocker = executor.submit(
        root_a.clone(),
        Lane::Mutating,
        "interactive-cap-blocker".to_string(),
        Box::new(move |_| {
            blocker_started_tx.send(()).expect("signal blocker start");
            release_blocker_rx
                .recv_timeout(Duration::from_secs(5))
                .expect("release interactive blocker");
            ok("interactive-cap-blocker")
        }),
    );
    blocker_started_rx
        .recv_timeout(Duration::from_secs(2))
        .expect("interactive blocker starts");

    let executed = Arc::new(AtomicUsize::new(0));
    let mut admitted = Vec::new();
    for _ in 0..executor.interactive_actor_queue_cap() {
        let executed_probe = Arc::clone(&executed);
        admitted.push(executor.submit_async(
            root_a.clone(),
            Lane::PureRead,
            "interactive-cap-admitted".to_string(),
            Box::new(move |_| {
                executed_probe.fetch_add(1, Ordering::AcqRel);
                ok("interactive-cap-admitted")
            }),
        ));
    }
    let overflow = executor.submit_async(
        root_a,
        Lane::PureRead,
        "interactive-cap-overflow".to_string(),
        Box::new(|_| ok("interactive-cap-overflow")),
    );

    let overflow_response = recv_async(overflow, "interactive backpressure completion");
    assert!(!overflow_response.success);
    assert_eq!(overflow_response.data["code"], "executor_backpressure");
    assert_eq!(overflow_response.data["retryable"], serde_json::json!(true));
    assert_eq!(
        overflow_response.data["queue_class"],
        serde_json::json!("interactive")
    );
    assert_eq!(
        overflow_response.data["queue_scope"],
        serde_json::json!("actor")
    );
    assert_eq!(executed.load(Ordering::Acquire), 0);

    release_blocker_tx
        .send(())
        .expect("release interactive blocker");
    assert!(
        blocker
            .recv_timeout(Duration::from_secs(5))
            .expect("blocker completes")
            .success
    );
    for receiver in admitted {
        assert!(
            recv_async(receiver, "admitted interactive completion").success,
            "admitted interactive job must execute"
        );
    }
    assert_eq!(
        executed.load(Ordering::Acquire),
        executor.interactive_actor_queue_cap(),
        "every admitted interactive job must execute exactly once"
    );
}

#[test]
fn coalesced_maintenance_skips_capacity_and_dedupe_releases_capacity() {
    // A coalesced duplicate does not consume new capacity and is answered with
    // the ordinary maintenance_cancelled coalesce response. When the per-actor
    // cap is full, a duplicate removal frees exactly one slot for the next job.
    let executor = test_executor(2, 1, 1, 1);
    let (_dir, root) = test_root("coalesce-capacity");
    executor.register_actor(root.clone(), test_ctx());

    let (blocker_started_tx, blocker_started_rx) = crossbeam_channel::bounded(1);
    let (release_blocker_tx, release_blocker_rx) = crossbeam_channel::bounded(1);
    let blocker = executor.submit_maintenance_async(
        root.clone(),
        Lane::MaintenanceCommit,
        "coalesce-cap-blocker".to_string(),
        Box::new(move |_| {
            blocker_started_tx.send(()).expect("signal blocker start");
            release_blocker_rx
                .recv_timeout(Duration::from_secs(5))
                .expect("release blocker");
            ok("coalesce-cap-blocker")
        }),
    );
    blocker_started_rx
        .recv_timeout(Duration::from_secs(2))
        .expect("blocker starts");

    // Queue the first drain, then submit a duplicate. The duplicate coalesces
    // behind the identical queued drain and settles immediately with the
    // ordinary coalesce response without consuming new capacity.
    let first = executor.submit_coalescable_maintenance_async(
        root.clone(),
        Lane::MaintenanceCommit,
        "watcher-drain".to_string(),
        MaintenanceCoalesceKey::WatcherDrain,
        Box::new(|_| ok("watcher-drain")),
    );
    let coalesced_second = executor.submit_coalescable_maintenance_async(
        root.clone(),
        Lane::MaintenanceCommit,
        "watcher-drain".to_string(),
        MaintenanceCoalesceKey::WatcherDrain,
        Box::new(|_| ok("watcher-drain")),
    );
    let coalesced_response = recv_async(coalesced_second, "coalesced duplicate completion");
    assert!(!coalesced_response.success);
    assert_eq!(coalesced_response.data["code"], "maintenance_cancelled");

    release_blocker_tx
        .send(())
        .expect("release coalesce blocker");
    assert!(recv_async(blocker, "coalesce blocker completion").success);
    assert!(
        recv_async(first, "first coalescable drain").success,
        "the first coalescable drain executes after the blocker drains"
    );
}

#[test]
fn queue_accounting_tracks_dispatch_cancellation_and_actor_retirement() {
    // Depths must return to zero after dispatch, queued cancellation, and
    // actor removal; liveness mirrors the same numbers without contention.
    let executor = test_executor(1, 1, 1, 1);
    let (_dir, root) = test_root("accounting");
    executor.register_actor(root.clone(), test_ctx());

    let (started_tx, started_rx) = crossbeam_channel::bounded(1);
    let (release_tx, release_rx) = crossbeam_channel::bounded(1);
    let blocker = executor.submit_async(
        root.clone(),
        Lane::Mutating,
        "accounting-blocker".to_string(),
        Box::new(move |_| {
            started_tx.send(()).expect("signal start");
            release_rx
                .recv_timeout(Duration::from_secs(5))
                .expect("release");
            ok("accounting-blocker")
        }),
    );
    started_rx
        .recv_timeout(Duration::from_secs(2))
        .expect("blocker starts");

    let (queued_rx, queued_token) = executor.submit_cancellable_async(
        root.clone(),
        Lane::PureRead,
        "accounting-queued".to_string(),
        Box::new(|_| ok("accounting-queued")),
    );
    let snapshot = executor
        .try_dispatch_liveness_snapshot()
        .expect("liveness snapshot");
    assert_eq!(snapshot.interactive.queued, 1);

    assert_eq!(
        executor.cancel_job(&root, &queued_token),
        JobCancelOutcome::QueuedRemoved
    );
    assert!(!recv_async(queued_rx, "queued cancel completion").success);
    let snapshot = executor
        .try_dispatch_liveness_snapshot()
        .expect("liveness snapshot after cancel");
    assert_eq!(snapshot.interactive.queued, 0);

    release_tx.send(()).expect("release blocker");
    assert!(recv_async(blocker, "blocker completion").success);

    executor.remove_actor(&root);
    let snapshot = executor
        .try_dispatch_liveness_snapshot()
        .expect("liveness after removal");
    assert_eq!(snapshot.interactive.queued, 0);
    assert_eq!(snapshot.maintenance.queued, 0);
}

#[test]
fn already_expired_deadline_rejects_admission_with_request_deadline_exceeded() {
    let executor = test_executor(2, 1, 1, 1);
    let (_dir, root) = test_root("expired-admission");
    executor.register_actor(root.clone(), test_ctx());

    let executed = Arc::new(AtomicUsize::new(0));
    let executed_probe = Arc::clone(&executed);
    let (rx, _token) = executor.submit_cancellable_async_with_deadline(
        root,
        Lane::PureRead,
        "expired-admission-job".to_string(),
        Box::new(move |_| {
            executed_probe.fetch_add(1, Ordering::AcqRel);
            ok("expired-admission-job")
        }),
        Some(Instant::now() - Duration::from_secs(1)),
    );
    let response = recv_async(rx, "expired admission completion");
    assert!(!response.success);
    assert_eq!(response.data["code"], "request_deadline_exceeded");
    assert_eq!(response.data["retryable"], serde_json::json!(false));
    assert_eq!(response.data["phase"], serde_json::json!("queue"));
    assert_eq!(executed.load(Ordering::Acquire), 0);
}

#[test]
fn dispatched_job_is_not_auto_cancelled_after_deadline_passes() {
    // Once popped, a job runs to completion even if its deadline elapses
    // mid-execution; the queue-scoped rule keeps dispatched work authoritative.
    let executor = test_executor(2, 1, 1, 1);
    let (_dir, root) = test_root("dispatched-not-cancelled");
    executor.register_actor(root.clone(), test_ctx());

    let (rx, _token) = executor.submit_cancellable_async_with_deadline(
        root,
        Lane::PureRead,
        "late-runner".to_string(),
        Box::new(|_| {
            thread::sleep(Duration::from_millis(150));
            ok("late-runner")
        }),
        Some(Instant::now() + Duration::from_millis(20)),
    );
    let response = recv_async(rx, "late runner completion");
    assert!(
        response.success,
        "a dispatched job must complete despite an elapsed deadline"
    );
}

#[test]
fn deadline_aware_writer_urgency_matches_budget_boundaries() {
    struct Case {
        label: &'static str,
        deadline: Option<Instant>,
        now_offset_ms: u64,
        expect_urgent: bool,
    }
    let now = Instant::now();
    let cases = [
        // Budget <= 6s: urgent immediately (remaining <= promotion-age floor).
        Case {
            label: "small budget immediate urgency",
            deadline: Some(now + Duration::from_secs(6)),
            now_offset_ms: 0,
            expect_urgent: true,
        },
        // 12s RouteBind budget, queued for ~0ms: urgency at 6s age. Not urgent yet.
        Case {
            label: "halfway not reached",
            deadline: Some(now + Duration::from_secs(12)),
            now_offset_ms: 0,
            expect_urgent: false,
        },
        // 12s budget queued at 6s: halfway point reached.
        Case {
            label: "halfway urgency",
            deadline: Some(now + Duration::from_secs(6)),
            now_offset_ms: 6_000,
            expect_urgent: true,
        },
        // Deadline-less writers fall back to the promotion age.
        Case {
            label: "deadline-less below promotion age",
            deadline: None,
            now_offset_ms: 5_999,
            expect_urgent: false,
        },
        Case {
            label: "deadline-less at promotion age",
            deadline: None,
            now_offset_ms: 6_000,
            expect_urgent: true,
        },
    ];
    for case in cases {
        let mut actor = ActorState::new(test_ctx());
        let (tx, _rx) = crossbeam_channel::bounded::<Response>(1);
        actor.push_job(
            JobClass::Interactive,
            Lane::Mutating,
            QueuedJob {
                request_id: "bind".to_string(),
                command: "executor::Interactive::Mutating".to_string(),
                job: Box::new(|_ctx| ok("bind")),
                completion: CompletionSender::Sync(tx),
                queued_at: now,
                deadline: case.deadline,
                cancellation: None,
                maintenance_coalesce_key: None,
            },
        );
        let probe_now = now + Duration::from_millis(case.now_offset_ms);
        assert_eq!(
            actor
                .class_queues(JobClass::Interactive)
                .has_urgent_writer(probe_now),
            case.expect_urgent,
            "urgency boundary failed: {}",
            case.label
        );
    }
}
