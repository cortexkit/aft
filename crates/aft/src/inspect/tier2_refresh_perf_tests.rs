// Kept ignored because it builds a call graph for an entire real checkout.
#[cfg(unix)]
#[test]
#[ignore = "real-checkout CPU probe; set AFT_TIER2_BENCH_ROOT and AFT_TIER2_BENCH_OUTPUT"]
fn profile_tier2_unchanged_checkout() {
    fn cpu_ms() -> f64 {
        let mut usage: libc::rusage = unsafe { std::mem::zeroed() };
        assert_eq!(unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut usage) }, 0);
        (usage.ru_utime.tv_sec + usage.ru_stime.tv_sec) as f64 * 1000.0
            + (usage.ru_utime.tv_usec + usage.ru_stime.tv_usec) as f64 / 1000.0
    }
    let workspace = Path::new(crate::test_support::manifest_dir())
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .canonicalize()
        .unwrap();
    let root = PathBuf::from(std::env::var_os("AFT_TIER2_BENCH_ROOT").unwrap())
        .canonicalize()
        .unwrap();
    assert!(
        root.starts_with(workspace.join("target")),
        "use a disposable corpus"
    );
    let output = PathBuf::from(std::env::var_os("AFT_TIER2_BENCH_OUTPUT").unwrap());
    let storage = tempfile::tempdir_in(workspace.join("target")).unwrap();
    let inspect_dir = storage.path().join("inspect");
    let key = crate::search_index::artifact_cache_key(&root);
    crate::root_cache::configure_artifact_access(&root, &key, false);
    let files = crate::callgraph::walk_project_files(&root).collect::<Vec<_>>();
    let graph_dir = callgraph_store_dir_from_inspect_dir(&inspect_dir, &root).unwrap();
    eprintln!("tier2_probe building graph files={}", files.len());
    let (store, _) =
        CallGraphStore::cold_build_with_lease(graph_dir, root.clone(), &files).unwrap();
    drop(store);
    let manager = InspectManager::new();
    let mut job = snapshot_job(&root, &inspect_dir, true);
    job.scope_files = files;
    let cache = manager
        .cache_for_paths(inspect_dir.clone(), root.clone())
        .unwrap();
    let snapshot = InspectSnapshot::new(
        root.clone(),
        inspect_dir,
        Arc::clone(&job.config),
        Arc::clone(&job.symbol_cache),
    );
    let scope = JobScope::from_roots(root.clone(), vec![root.join("packages/aft-bridge/src")]);
    let cpu = cpu_ms();
    let started = Instant::now();
    let graph = manager
        .build_tier2_callgraph_snapshot_with_refresh(&job, false, false, &[])
        .unwrap();
    eprintln!(
        "tier2_probe projection files={} exports={} edges={} wall_ms={:.3} cpu_ms={:.3}",
        graph.files.len(),
        graph.exported_symbols.len(),
        graph.outbound_calls.len(),
        started.elapsed().as_secs_f64() * 1000.0,
        cpu_ms() - cpu
    );
    let mut aggregates = serde_json::Map::new();
    for category in [
        InspectCategory::DeadCode,
        InspectCategory::UnusedExports,
        InspectCategory::Cycles,
    ] {
        job.category = category;
        job.key = JobKey::for_project_category(category);
        let options = Tier2ReuseOptions {
            require_callgraph_snapshot: category == InspectCategory::DeadCode,
            ..Tier2ReuseOptions::default()
        };
        let mut previous = None;
        for run in 0..4 {
            crate::callgraph_store::take_projection_work();
            let cpu = cpu_ms();
            let started = Instant::now();
            let success = manager
                .tier2_run_with_reuse_job(&job, &cache, &options)
                .unwrap_or_else(|error| panic!("tier2 failed: {error:?}"));
            let elapsed = started.elapsed().as_secs_f64() * 1000.0;
            let cpu = cpu_ms() - cpu;
            let work = crate::callgraph_store::take_projection_work();
            if let Some(previous) = &previous {
                assert_eq!(previous, &success.aggregate, "unchanged {category}");
            }
            eprintln!("tier2_probe category={category} run={run} files={} wall_ms={elapsed:.3} cpu_ms={cpu:.3} full_projections={} outbound_rows={} scanned={}", job.scope_files.len(), work.0, work.1, success.scanned_files.len());
            previous = Some(success.aggregate);
        }
        let project_payload = previous.unwrap();
        let mut previous = None;
        for run in 0..4 {
            crate::callgraph_store::take_projection_work();
            let cpu = cpu_ms();
            let started = Instant::now();
            let payload = completed_scoped_waiter(
                &manager,
                &snapshot,
                &cache,
                category,
                &scope,
                project_payload.clone(),
            );
            let elapsed = started.elapsed().as_secs_f64() * 1000.0;
            let cpu = cpu_ms() - cpu;
            let work = crate::callgraph_store::take_projection_work();
            if let Some(previous) = &previous {
                assert_eq!(previous, &payload, "unchanged scoped {category}");
            }
            eprintln!("tier2_probe scoped category={category} run={run} wall_ms={elapsed:.3} cpu_ms={cpu:.3} full_projections={} outbound_rows={}", work.0, work.1);
            previous = Some(payload);
        }
        aggregates.insert(
            category.to_string(),
            serde_json::json!({"project": project_payload, "scoped": previous.unwrap()}),
        );
    }
    std::fs::write(output, serde_json::to_vec_pretty(&aggregates).unwrap()).unwrap();
}

fn completed_scoped_waiter(
    manager: &InspectManager,
    snapshot: &InspectSnapshot,
    cache: &InspectCache,
    category: InspectCategory,
    scope: &JobScope,
    payload: Value,
) -> Value {
    // Model completion fanout after a shared project scan has finished. Keeping
    // the channel ready makes the work assertion independent of thread timing.
    let (tx, rx) = crossbeam_channel::bounded(1);
    tx.send(JobOutcome::Fresh { payload }).unwrap();
    match manager.wait_for_tier2_reuse(
        &JobKey::for_project_category(category),
        scope,
        cache,
        rx,
        snapshot,
    ) {
        JobOutcome::Fresh { payload } => payload,
        other => panic!("expected fresh scoped completion, got {other:?}"),
    }
}

thread_local! {
    static CALLGRAPH_READ_RETRY_OBSERVER: std::cell::RefCell<Option<Box<dyn FnOnce()>>> =
        std::cell::RefCell::new(None);
    static CALLGRAPH_READ_RETRY_CLOCK: std::cell::Cell<Option<Instant>> = const {
        std::cell::Cell::new(None)
    };
}

pub(super) fn callgraph_read_retry_now_for_test() -> Option<Instant> {
    CALLGRAPH_READ_RETRY_CLOCK.with(|clock| {
        let now = clock.get()?;
        // Advance on every deadline check so an unreleased lock still exhausts
        // the retry budget, without charging filesystem or scheduling delays.
        clock.set(Some(now + Duration::from_millis(5)));
        Some(now)
    })
}

pub(super) fn notify_callgraph_read_retry_for_test() {
    let observer = CALLGRAPH_READ_RETRY_OBSERVER.with(|slot| slot.borrow_mut().take());
    if let Some(observer) = observer {
        observer();
    }
}

pub(super) fn observe_callgraph_read_error_for_test(
    error: &CallGraphStoreError,
    attempt_elapsed: Duration,
    retry_remaining: Duration,
) {
    CALLGRAPH_READ_RETRY_OBSERVER.with(|slot| {
        if slot.borrow().is_some() {
            eprintln!(
                "locked callgraph read: attempt={attempt_elapsed:?}, wall_retry_remaining={retry_remaining:?}, error={error:?}"
            );
        }
    });
}

struct CallgraphReadRetryTestGuard;

impl CallgraphReadRetryTestGuard {
    fn install() -> Self {
        CALLGRAPH_READ_RETRY_CLOCK.with(|clock| clock.set(Some(Instant::now())));
        Self
    }
}

impl Drop for CallgraphReadRetryTestGuard {
    fn drop(&mut self) {
        CALLGRAPH_READ_RETRY_OBSERVER.with(|slot| slot.borrow_mut().take());
        CALLGRAPH_READ_RETRY_CLOCK.with(|clock| clock.set(None));
    }
}

#[test]
fn dead_code_store_read_does_not_retry_an_expired_lock() {
    let _retry = CallgraphReadRetryTestGuard::install();
    let start = CALLGRAPH_READ_RETRY_CLOCK.with(|clock| clock.get().unwrap());
    let mut attempts = 0;
    let result = retry_dead_code_store_read(|| {
        attempts += 1;
        // A lock that consumes the whole budget stays pending, even with the
        // controlled clock used by the transient-lock fixtures.
        CALLGRAPH_READ_RETRY_CLOCK
            .with(|clock| clock.set(Some(start + Duration::from_millis(100))));
        Err::<(), _>(CallGraphStoreError::Sqlite(rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_BUSY),
            None,
        )))
    });
    assert!(result.unwrap_err().is_transient_lock_contention());
    assert_eq!(
        attempts, 1,
        "an expired read must not start another attempt"
    );
}

#[test]
fn dead_code_store_read_exhausts_its_retry_budget() {
    let _retry = CallgraphReadRetryTestGuard::install();
    let mut attempts = 0;
    let result = retry_dead_code_store_read(|| {
        attempts += 1;
        Err::<(), _>(CallGraphStoreError::Sqlite(rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_LOCKED),
            None,
        )))
    });
    assert!(result.unwrap_err().is_transient_lock_contention());
    assert_eq!(attempts, 20, "a persistent lock must stop at the deadline");
}
