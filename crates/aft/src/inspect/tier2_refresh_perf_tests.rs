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
    let (store, _) =
        CallGraphStore::cold_build_with_lease(graph_dir, root.clone(), &files).unwrap();
    drop(store);
    let manager = InspectManager::new();
    let mut job = snapshot_job(&root, &inspect_dir, true);
    job.scope_files = files;
    let cache = manager.cache_for_paths(inspect_dir, root.clone()).unwrap();
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
        aggregates.insert(category.to_string(), previous.unwrap());
    }
    std::fs::write(output, serde_json::to_vec_pretty(&aggregates).unwrap()).unwrap();
}
