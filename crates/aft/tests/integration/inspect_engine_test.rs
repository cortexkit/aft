use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, RwLock};
use std::thread;
use std::time::{Duration, Instant};

use aft::cache_freshness;
use aft::config::Config;
use aft::inspect::{
    contribution_is_fresh, inspect_pool_size_for_test, inspect_pool_thread_count_for_test,
    verify_contribution_file, ContributionFreshness, FileContribution, InspectCache,
    InspectCategory, InspectManager, InspectResult, InspectScanSuccess, InspectSnapshot,
    InspectWorker, JobKey, JobOutcome, JobScope,
};
use aft::parser::SymbolCache;
use serde_json::json;

use super::helpers::{user_config, AftProcess};

fn fixture_project() -> (tempfile::TempDir, PathBuf, PathBuf) {
    let temp_dir = tempfile::tempdir().expect("tempdir");
    let root = temp_dir.path().join("project");
    let src = root.join("src");
    fs::create_dir_all(&src).expect("create src");
    fs::write(
        root.join("Cargo.toml"),
        "[package]\nname = \"inspect-fixture\"\nversion = \"0.1.0\"\n",
    )
    .expect("write manifest");
    let file = src.join("lib.rs");
    fs::write(&file, "pub fn alive() {}\n").expect("write source");
    (temp_dir, root, file)
}

fn snapshot(project_root: &Path, inspect_dir: &Path) -> InspectSnapshot {
    let config = Config {
        project_root: Some(project_root.to_path_buf()),
        ..Config::default()
    };
    InspectSnapshot::new(
        project_root.to_path_buf(),
        inspect_dir.to_path_buf(),
        Arc::new(config),
        Arc::new(RwLock::new(SymbolCache::new())),
    )
}

fn test_worker(worker_count: Arc<AtomicUsize>, sleep_for: Duration, count: u64) -> InspectWorker {
    Arc::new(move |job| {
        let started = Instant::now();
        worker_count.fetch_add(1, Ordering::SeqCst);
        thread::sleep(sleep_for);
        let aggregate = json!({
            "count": count,
            "items": [{"file": "src/lib.rs", "line": 1}],
        });
        InspectResult::success(
            &job,
            InspectScanSuccess {
                scanned_files: job.scope_files.clone(),
                contributions: Vec::new(),
                aggregate,
            },
            started.elapsed(),
        )
    })
}

fn wait_for_worker_count(worker_count: &AtomicUsize, expected: usize, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    loop {
        if worker_count.load(Ordering::SeqCst) >= expected {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "worker_count did not reach {expected} before timeout; current={}",
            worker_count.load(Ordering::SeqCst)
        );
        thread::sleep(Duration::from_millis(5));
    }
}

fn wait_for_flag(flag: &AtomicBool, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while !flag.load(Ordering::SeqCst) && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(5));
    }
    flag.load(Ordering::SeqCst)
}

fn interleaving_worker(
    large_root: PathBuf,
    large_started: Arc<AtomicBool>,
    large_finished: Arc<AtomicBool>,
    small_finished: Arc<AtomicBool>,
    small_interleaved: Arc<AtomicBool>,
) -> InspectWorker {
    Arc::new(move |job| {
        let started = Instant::now();
        let is_large = job.project_root == large_root;
        if is_large {
            large_started.store(true, Ordering::SeqCst);
            let deadline = Instant::now() + Duration::from_secs(5);
            while !small_finished.load(Ordering::SeqCst) && Instant::now() < deadline {
                thread::sleep(Duration::from_millis(5));
            }
            large_finished.store(true, Ordering::SeqCst);
        } else {
            small_interleaved.store(!large_finished.load(Ordering::SeqCst), Ordering::SeqCst);
            thread::sleep(Duration::from_millis(25));
            small_finished.store(true, Ordering::SeqCst);
        }
        InspectResult::success(
            &job,
            InspectScanSuccess {
                scanned_files: job.scope_files.clone(),
                contributions: Vec::new(),
                aggregate: json!({"count": job.scope_files.len()}),
            },
            started.elapsed(),
        )
    })
}

#[test]
fn inspect_engine_active_categories_include_diagnostics() {
    assert!(InspectCategory::active().contains(&InspectCategory::Diagnostics));
    assert!(InspectCategory::Diagnostics.is_active());
}

#[test]
fn inspect_engine_cache_persists_tier2_contributions_and_aggregate() {
    let (_temp_dir, root, file) = fixture_project();
    let inspect_dir = root.join(".aft-cache").join("inspect");
    let cache = InspectCache::open(inspect_dir.clone(), root.clone()).expect("open cache");
    let freshness = cache_freshness::collect(&file).expect("collect freshness");
    let key = JobKey::for_project_category(InspectCategory::DeadCode);
    let contribution = FileContribution::new(
        InspectCategory::DeadCode,
        file.clone(),
        freshness,
        json!({"file": "src/lib.rs", "exported_symbols": [], "outbound_calls": []}),
    );

    cache
        .store_tier2_result(
            key.clone(),
            std::slice::from_ref(&file),
            &[contribution],
            json!({"count": 1, "items": [{"file": "src/lib.rs", "symbol": "alive"}]}),
        )
        .expect("store result");

    assert!(cache.sqlite_path().starts_with(&inspect_dir));
    assert!(
        cache
            .contribution_set_hash(InspectCategory::DeadCode)
            .unwrap()
            .len()
            >= 32
    );

    let reopened = InspectCache::open(inspect_dir, root).expect("reopen cache");
    let aggregate = reopened
        .get_aggregated(&key)
        .expect("read aggregate")
        .expect("aggregate present");
    assert_eq!(aggregate["count"], 1);

    let contributions = reopened
        .load_tier2_contributions(InspectCategory::DeadCode)
        .expect("load contributions");
    assert_eq!(contributions.len(), 1);
    assert_eq!(contributions[0].file_path, PathBuf::from("src/lib.rs"));
    assert_eq!(contributions[0].contribution["file"], "src/lib.rs");
}

#[test]
fn inspect_engine_freshness_treats_hot_and_content_fresh_as_fresh() {
    let temp_dir = tempfile::tempdir().expect("tempdir");
    let file = temp_dir.path().join("a.rs");
    fs::write(&file, "alpha").expect("write file");
    let freshness = cache_freshness::collect(&file).expect("collect freshness");

    assert!(contribution_is_fresh(&file, &freshness));

    filetime::set_file_mtime(&file, filetime::FileTime::from_unix_time(1, 0)).expect("touch mtime");
    match verify_contribution_file(&file, &freshness) {
        ContributionFreshness::Fresh {
            metadata_changed, ..
        } => assert!(metadata_changed),
        other => panic!("expected content-fresh contribution, got {other:?}"),
    }

    // Same-size content change. The non-strict fast path returns HotFresh
    // WITHOUT hashing when (mtime, size) both match the cached snapshot — so to
    // exercise the content-hash path that detects this change we must ensure the
    // mtime differs from the cached snapshot. Set it explicitly to a fixed value
    // distinct from the original collect time; otherwise on coarse-granularity
    // filesystems (e.g. Docker overlayfs, 1s mtime resolution) the write can land
    // in the same mtime bucket as the original collect and the fast path would
    // report HotFresh, masking the content change. A fixed mtime makes the
    // content-hash comparison deterministic on every filesystem.
    fs::write(&file, "bravo").expect("write changed same-size file");
    filetime::set_file_mtime(&file, filetime::FileTime::from_unix_time(2, 0))
        .expect("set distinct mtime after same-size edit");
    assert_eq!(
        verify_contribution_file(&file, &freshness),
        ContributionFreshness::Stale
    );

    fs::remove_file(&file).expect("delete file");
    assert_eq!(
        verify_contribution_file(&file, &freshness),
        ContributionFreshness::Deleted
    );
}

#[test]
fn inspect_engine_deduplicates_in_flight_waiters() {
    let (_temp_dir, root, _file) = fixture_project();
    let inspect_dir = root.join(".aft-cache").join("inspect");
    let worker_count = Arc::new(AtomicUsize::new(0));
    let manager = Arc::new(InspectManager::with_worker(
        test_worker(Arc::clone(&worker_count), Duration::from_millis(150), 7),
        Duration::from_secs(2),
    ));
    let snapshot = snapshot(&root, &inspect_dir);
    let scope = JobScope::for_project(root.clone());

    let first_manager = Arc::clone(&manager);
    let first_snapshot = snapshot.clone();
    let first_scope = scope.clone();
    let first = thread::spawn(move || {
        first_manager.submit_category(first_snapshot, InspectCategory::DeadCode, first_scope)
    });

    wait_for_worker_count(worker_count.as_ref(), 1, Duration::from_secs(2));

    let second_manager = Arc::clone(&manager);
    let second = thread::spawn(move || {
        second_manager.submit_category(snapshot, InspectCategory::DeadCode, scope)
    });

    let first = first.join().expect("first waiter");
    let second = second.join().expect("second waiter");

    assert_eq!(
        worker_count.load(Ordering::SeqCst),
        1,
        "one worker job should serve both waiters"
    );
    assert!(matches!(first, JobOutcome::Fresh { .. }));
    assert!(matches!(second, JobOutcome::Fresh { .. }));
    assert_eq!(first.payload().unwrap()["count"], 7);
    assert_eq!(second.payload().unwrap()["count"], 7);
}

#[test]
fn inspect_engine_blocking_deadline_outlives_soft_deadline_for_cold_scan() {
    let (_temp_dir, root, _file) = fixture_project();
    let inspect_dir = root.join(".aft-cache").join("inspect");
    let worker_count = Arc::new(AtomicUsize::new(0));
    let manager = InspectManager::with_worker(
        test_worker(Arc::clone(&worker_count), Duration::from_millis(100), 11),
        Duration::from_millis(1),
    );
    let snapshot = snapshot(&root, &inspect_dir);
    let scope = JobScope::for_project(root.clone());

    let soft_outcome =
        manager.submit_category(snapshot.clone(), InspectCategory::Metrics, scope.clone());
    assert!(matches!(
        soft_outcome,
        JobOutcome::Pending { in_flight: true }
    ));

    let blocking_outcome = manager.submit_category_until(
        snapshot,
        InspectCategory::Metrics,
        scope,
        Instant::now() + Duration::from_secs(10),
    );

    assert!(matches!(blocking_outcome, JobOutcome::Fresh { .. }));
    assert_eq!(blocking_outcome.payload().unwrap()["count"], 11);
    assert_eq!(
        worker_count.load(Ordering::SeqCst),
        1,
        "the blocking waiter must attach to the cold scan that exceeded the soft deadline"
    );
}

#[test]
fn inspect_engine_roots_share_bounded_named_thread_pool() {
    let (_first_temp, first_root, _first_file) = fixture_project();
    let (_second_temp, second_root, _second_file) = fixture_project();
    let worker_count = Arc::new(AtomicUsize::new(0));
    let first_manager = Arc::new(InspectManager::with_worker(
        test_worker(Arc::clone(&worker_count), Duration::from_millis(150), 1),
        Duration::from_secs(2),
    ));
    let second_manager = Arc::new(InspectManager::with_worker(
        test_worker(Arc::clone(&worker_count), Duration::from_millis(150), 2),
        Duration::from_secs(2),
    ));

    let first_pool = first_manager.inspect_pool_for_test();
    let second_pool = second_manager.inspect_pool_for_test();
    assert!(
        Arc::ptr_eq(&first_pool, &second_pool),
        "inspect roots must submit to one process-wide rayon pool"
    );
    let pool_size = inspect_pool_size_for_test();

    let first_snapshot = snapshot(&first_root, &first_root.join(".aft-cache/inspect"));
    let second_snapshot = snapshot(&second_root, &second_root.join(".aft-cache/inspect"));
    let first_scope = JobScope::for_project(first_root.clone());
    let second_scope = JobScope::for_project(second_root.clone());
    let first_manager_for_thread = Arc::clone(&first_manager);
    let first = thread::spawn(move || {
        first_manager_for_thread.submit_category(
            first_snapshot,
            InspectCategory::DeadCode,
            first_scope,
        )
    });
    let second_manager_for_thread = Arc::clone(&second_manager);
    let second = thread::spawn(move || {
        second_manager_for_thread.submit_category(
            second_snapshot,
            InspectCategory::DeadCode,
            second_scope,
        )
    });

    wait_for_worker_count(worker_count.as_ref(), 2, Duration::from_secs(2));
    let named_threads = inspect_pool_thread_count_for_test();
    assert!(
        named_threads > 0,
        "running scans must start named inspect workers"
    );
    assert!(
        named_threads <= pool_size,
        "named inspect workers ({named_threads}) must stay within the shared pool size ({pool_size})"
    );
    assert!(matches!(
        first.join().expect("first scan"),
        JobOutcome::Fresh { .. }
    ));
    assert!(matches!(
        second.join().expect("second scan"),
        JobOutcome::Fresh { .. }
    ));
}

#[test]
fn inspect_engine_small_root_interleaves_with_large_scan() {
    let (_large_temp, large_root, _large_file) = fixture_project();
    let (_small_temp, small_root, _small_file) = fixture_project();
    let pool_size = inspect_pool_size_for_test();
    let large_started = Arc::new(AtomicBool::new(false));
    let large_finished = Arc::new(AtomicBool::new(false));
    let small_finished = Arc::new(AtomicBool::new(false));
    let small_interleaved = Arc::new(AtomicBool::new(false));
    let worker = interleaving_worker(
        large_root.clone(),
        Arc::clone(&large_started),
        Arc::clone(&large_finished),
        Arc::clone(&small_finished),
        Arc::clone(&small_interleaved),
    );
    let large_manager = Arc::new(InspectManager::with_worker(
        Arc::clone(&worker),
        Duration::from_secs(10),
    ));
    let small_manager = Arc::new(InspectManager::with_worker(worker, Duration::from_secs(10)));

    let large_snapshot = snapshot(&large_root, &large_root.join(".aft-cache/inspect"));
    let large_scope = JobScope::for_project(large_root.clone());
    let large_manager_for_thread = Arc::clone(&large_manager);
    let large = thread::spawn(move || {
        large_manager_for_thread.submit_category(
            large_snapshot,
            InspectCategory::Todos,
            large_scope,
        )
    });
    assert!(
        wait_for_flag(large_started.as_ref(), Duration::from_secs(2)),
        "large scan did not start"
    );

    let small_snapshot = snapshot(&small_root, &small_root.join(".aft-cache/inspect"));
    let small_scope = JobScope::for_project(small_root.clone());
    // Submit the small scan from this test thread. Spawning another OS thread
    // here lets a heavily loaded runner delay submission until after the fixed
    // large-scan sleep, producing a false serialization failure.
    let small_result =
        small_manager.submit_category(small_snapshot, InspectCategory::Todos, small_scope);
    let small_completed_before_timeout =
        wait_for_flag(small_finished.as_ref(), Duration::from_secs(5));
    let large_result = large.join().expect("large scan");

    assert!(
        small_completed_before_timeout,
        "small root did not complete within the interleaving budget"
    );
    if pool_size > 1 {
        assert!(
            small_interleaved.load(Ordering::SeqCst),
            "small root did not start while the large scan was running (pool_size={pool_size})"
        );
    }
    assert!(matches!(large_result, JobOutcome::Fresh { .. }));
    assert!(matches!(small_result, JobOutcome::Fresh { .. }));
}

#[test]
fn inspect_engine_drain_routes_idle_scan_to_cache() {
    let (_temp_dir, root, _file) = fixture_project();
    let inspect_dir = root.join(".aft-cache").join("inspect");
    let worker_count = Arc::new(AtomicUsize::new(0));
    let manager = InspectManager::with_worker(
        test_worker(Arc::clone(&worker_count), Duration::from_millis(25), 3),
        Duration::from_secs(1),
    );
    let snapshot = snapshot(&root, &inspect_dir);
    let scope = JobScope::for_project(root.clone());
    let key = manager
        .submit_background(snapshot.clone(), InspectCategory::Duplicates, scope)
        .expect("queue background scan");

    let mut drained = 0usize;
    for _ in 0..20 {
        drained += manager.drain_completions();
        if drained > 0 {
            break;
        }
        thread::sleep(Duration::from_millis(25));
    }

    assert_eq!(worker_count.load(Ordering::SeqCst), 1);
    assert_eq!(
        drained, 1,
        "background completion should drain exactly once"
    );
    let cache = manager.cache_for_snapshot(&snapshot).expect("cache");
    let aggregate = cache
        .get_aggregated(&key)
        .expect("aggregate read")
        .expect("aggregate present");
    assert_eq!(aggregate["count"], 3);
}

#[test]
fn inspect_engine_command_returns_lane_a_shape() {
    let (_temp_dir, root, _file) = fixture_project();
    let mut aft = AftProcess::spawn();
    let configure = aft.send(
        &json!({
            "id": "cfg",
            "command": "configure",
            "harness": "opencode",
            "project_root": root,
            "config": user_config(json!({
                "lsp": { "disabled": ["rust"] }
            })),
        })
        .to_string(),
    );
    assert_eq!(
        configure["success"], true,
        "configure should succeed: {configure:?}"
    );

    let response = aft.send(
        &json!({
            "id": "inspect-engine",
            "command": "inspect",
            "sections": "all",
            "topK": 5,
        })
        .to_string(),
    );

    assert_eq!(
        response["success"], true,
        "inspect should succeed: {response:?}"
    );
    assert_eq!(
        response["inspect_terminal"], "fresh",
        "successful lane-A inspect must reach the fresh terminal: {response:?}"
    );
    let diagnostics = response["summary"]["diagnostics"]
        .as_object()
        .expect("diagnostics summary");
    assert_eq!(
        diagnostics.get("errors").and_then(|value| value.as_u64()),
        Some(0),
        "disabled diagnostics producers should yield verified zero errors: {response:?}"
    );
    assert!(response["details"]["diagnostics"].is_array());
    assert!(response["summary"]["metrics"].is_object());
    assert!(response["summary"]["todos"].is_object());
    assert!(response["details"]["dead_code"].is_array());
    assert!(response["scanner_state"]["disabled_categories"]
        .as_array()
        .expect("disabled categories")
        .iter()
        .any(|category| category == "vulnerabilities"));

    assert!(aft.shutdown().success());
}
