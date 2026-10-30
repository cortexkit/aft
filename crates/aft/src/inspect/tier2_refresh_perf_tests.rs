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
    let workspace = Path::new(env!("CARGO_MANIFEST_DIR"))
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

#[test]
fn completed_scoped_dead_code_waiters_share_projection_and_rollup() {
    let (_dir, root, inspect_dir, mut job) = published_projection_fixture();
    job.scope_files = crate::callgraph::walk_project_files(&root).collect();
    let manager = InspectManager::new();
    let cache = manager
        .cache_for_paths(inspect_dir.clone(), root.clone())
        .unwrap();
    let project = manager
        .tier2_run_with_reuse_job(&job, &cache, &Tier2ReuseOptions::default())
        .unwrap();
    let snapshot = InspectSnapshot::new(
        root.clone(),
        inspect_dir,
        Arc::clone(&job.config),
        Arc::clone(&job.symbol_cache),
    );
    let scope = JobScope::from_roots(root.clone(), vec![root.join("src/target.ts")]);
    let expected = scoped_tier2_payload_from_contributions(
        None,
        &snapshot,
        job.category,
        cache.as_ref(),
        project.aggregate.clone(),
        &scope,
    )
    .unwrap();
    assert_eq!(expected["callgraph_available"], true);
    let rollup = manager
        .previous_dead_code_rollup_state(&root)
        .expect("shared scan cached its rollup");
    std::thread::scope(|threads| {
        let waiters = (0..2)
            .map(|_| {
                threads.spawn(|| {
                    crate::callgraph_store::take_projection_work();
                    let payload = completed_scoped_waiter(
                        &manager,
                        &snapshot,
                        &cache,
                        job.category,
                        &scope,
                        project.aggregate.clone(),
                    );
                    (payload, crate::callgraph_store::take_projection_work())
                })
            })
            .collect::<Vec<_>>();
        for waiter in waiters {
            let (payload, work) = waiter.join().unwrap();
            assert_eq!(
                payload, expected,
                "reuse must preserve the uncached scoped answer"
            );
            assert_eq!(
                work,
                (0, 0),
                "completed scoped waiters must not re-project the shared graph"
            );
        }
    });
    crate::callgraph_store::take_projection_work();
    let direct = manager.tier2_run_with_reuse(snapshot.clone(), job.category, scope.clone(), None);
    assert_eq!(fresh_payload(direct), expected);
    assert_eq!(
        crate::callgraph_store::take_projection_work(),
        (0, 0),
        "synchronous scoped completions must reuse the same projection"
    );
    assert!(
        Arc::ptr_eq(
            &rollup,
            &manager.previous_dead_code_rollup_state(&root).unwrap()
        ),
        "unchanged contributions must reuse the shared rollup"
    );

    // A graph-only change must invalidate reuse even when the contribution hash
    // is unchanged: scoped liveness includes callers outside the requested file.
    write_projection_cache_file(&root.join("src/main.ts"), "export function main() {}\n");
    let graph_dir = callgraph_store_dir_from_inspect_dir(&job.inspect_dir, &root).unwrap();
    let store = CallGraphStore::open_ready_no_rebuild(graph_dir, root.clone())
        .unwrap()
        .unwrap();
    store.refresh_files(&[root.join("src/main.ts")]).unwrap();
    drop(store);
    let changed = completed_scoped_waiter(
        &manager,
        &snapshot,
        &cache,
        job.category,
        &scope,
        project.aggregate.clone(),
    );
    let expected_changed = scoped_tier2_payload_from_contributions(
        None,
        &snapshot,
        job.category,
        cache.as_ref(),
        project.aggregate,
        &scope,
    )
    .unwrap();
    assert_eq!(changed, expected_changed);
    // Cached src/main.ts facts still import firstTarget, so findings can stay equal.
    // The graph revision must nevertheless replace the retained rollup state.
    assert!(!Arc::ptr_eq(
        &rollup,
        &manager.previous_dead_code_rollup_state(&root).unwrap()
    ));
}
