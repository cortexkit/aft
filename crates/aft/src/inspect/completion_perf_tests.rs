fn duplicate_perf_job(root: &Path) -> InspectJob {
    let mut job = snapshot_job(root, &root.join(".aft-cache/inspect"), true);
    job.category = InspectCategory::Duplicates;
    job.key = JobKey::for_project_category(InspectCategory::Duplicates);
    job.scope_files = crate::callgraph::walk_project_files(root).collect();
    job
}

#[test]
fn duplicates_completion_does_not_clone_contributions() {
    let dir = tempfile::tempdir().unwrap();
    let job = duplicate_perf_job(dir.path());
    let success = InspectScanSuccess {
        scanned_files: Vec::new(),
        contributions: vec![FileContribution::new(
            InspectCategory::Duplicates,
            dir.path().join("example.rs"),
            crate::cache_freshness::FileFreshness {
                mtime: std::time::UNIX_EPOCH,
                size: 0,
                content_hash: blake3::hash(b""),
            },
            json!({"fragments": (0..10_000).map(|i| format!("fragment-{i}")).collect::<Vec<_>>() }),
        )],
        aggregate: json!({"count": 0}),
    };
    let result = InspectResult::success(&job, success, Duration::ZERO);
    let manager = InspectManager::new();
    let (_, allocations) =
        crate::test_allocations::count(|| manager.route_tier2_reuse_completion(result));
    println!("reuse completion allocations={allocations} for 10000 fragments");
    assert!(
        allocations < 100,
        "unused facts were cloned: {allocations} allocations"
    );
}

#[test]
fn tier2_regular_completion_does_not_clone_unused_facts() {
    let dir = tempfile::tempdir().unwrap();
    let job = duplicate_perf_job(dir.path());
    let success = InspectScanSuccess {
        scanned_files: vec![dir.path().join("example.rs")],
        contributions: vec![FileContribution::new(
            job.category,
            dir.path().join("example.rs"),
            crate::cache_freshness::FileFreshness {
                mtime: std::time::UNIX_EPOCH,
                size: 0,
                content_hash: blake3::hash(b""),
            },
            json!({"fragments": (0..10_000).map(|i| format!("fragment-{i}")).collect::<Vec<_>>() }),
        )],
        aggregate: json!({"count": 0}),
    };
    let direct = InspectResult::success(&job, success.clone(), Duration::ZERO);
    let routed = InspectResult::success(&job, success, Duration::ZERO);
    let manager = InspectManager::new();
    manager
        .cache_for_paths(job.inspect_dir.clone(), job.project_root.clone())
        .unwrap();
    let (_, direct_allocations) =
        crate::test_allocations::count(|| manager.completion_outcome(direct));
    let (_, routed_allocations) =
        crate::test_allocations::count(|| manager.route_completion(routed));
    println!(
        "regular completion allocations: direct={direct_allocations}, routed={routed_allocations}"
    );
    assert!(
        routed_allocations <= direct_allocations + 100,
        "routing copied unused contributions"
    );
}

#[test]
fn tier2_changed_rollup_loads_contributions_once() {
    let dir = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(dir.path()).unwrap();
    let source = format!(
        "fn example() {{\n{}\n}}\n",
        (0..20)
            .map(|i| format!("println!(\"{i}\");"))
            .collect::<Vec<_>>()
            .join("\n")
    );
    write_projection_cache_file(&root.join("src/in/a.rs"), &source);
    write_projection_cache_file(&root.join("src/out/b.rs"), &source);
    let job = duplicate_perf_job(&root);
    let cache = InspectCache::open(job.inspect_dir.clone(), root.clone()).unwrap();
    let initial = super::super::scanners::duplicates::run_duplicates_scan(&job)
        .outcome
        .unwrap();
    cache
        .store_tier2_result_for_config(
            job.key.clone(),
            &initial.scanned_files,
            &initial.contributions,
            initial.aggregate,
            &job.config,
        )
        .unwrap();
    write_projection_cache_file(
        &root.join("src/in/a.rs"),
        &format!("{source}\nfn extra() {{}}\n"),
    );
    super::super::cache::CONTRIBUTION_LOAD_WORK.with(|work| work.set((0, 0, 0)));
    let result = InspectManager::new()
        .tier2_run_with_reuse_job(&job, &cache, &Tier2ReuseOptions::default())
        .unwrap();
    let work = super::super::cache::CONTRIBUTION_LOAD_WORK.with(std::cell::Cell::get);
    println!(
        "changed duplicates SELECTs={} BLOB rows={} decoded bytes={}",
        work.0, work.1, work.2
    );
    assert_eq!(work.0, 1);
    assert_eq!(work.1, 2);
    assert!(work.2 > 0);
    assert!(
        result.contributions.is_empty(),
        "stored rollup facts must not be returned to completion"
    );
    let cold = super::super::scanners::duplicates::run_duplicates_scan(&job)
        .outcome
        .unwrap();
    assert!(cold.aggregate["total_groups"].as_u64().unwrap() > 0);
    assert_eq!(
        serde_json::to_vec(&result.aggregate).unwrap(),
        serde_json::to_vec(&cold.aggregate).unwrap()
    );
    let scope = JobScope::from_roots(&root, vec![root.join("src/in")]);
    assert_eq!(
        filter_payload_for_scope(result.aggregate, &scope),
        filter_payload_for_scope(cold.aggregate, &scope)
    );

    super::super::cache::CONTRIBUTION_LOAD_WORK.with(|work| work.set((0, 0, 0)));
    let manager = InspectManager::new();
    assert!(manager
        .tier2_quick_reuse_success(&job, &cache, &Tier2ReuseOptions::default())
        .unwrap()
        .is_some());
    assert_eq!(
        super::super::cache::CONTRIBUTION_LOAD_WORK.with(std::cell::Cell::get),
        (0, 0, 0)
    );
}

#[test]
fn duplicate_cache_upgrade_rebuilds_legacy_fragment_rows() {
    for legacy_version in [34, 35, 36, 37, 38, 39, 40, 41] {
        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        write_projection_cache_file(
            &root.join("src/a.rs"),
            "fn example() { let number = 1; println!(\"{number}\"); }\n",
        );
        let job = duplicate_perf_job(&root);
        let cache = InspectCache::open(job.inspect_dir.clone(), root).unwrap();
        let mut legacy = super::super::scanners::duplicates::run_duplicates_scan(&job)
            .outcome
            .unwrap();
        let expected = legacy.aggregate.clone();
        for contribution in &mut legacy.contributions {
            let fragments = contribution.contribution["fragments"].as_array().unwrap().iter().map(|fragment| {
            json!({"hash":fragment[0],"start_line":fragment[1],"end_line":fragment[2],"cost":fragment[3]})
        }).collect();
            contribution.contribution["fragments"] = Value::Array(fragments);
            contribution
                .contribution
                .as_object_mut()
                .unwrap()
                .remove("line_count");
        }
        cache
            .store_tier2_result_for_config(
                job.key.clone(),
                &legacy.scanned_files,
                &legacy.contributions,
                legacy.aggregate,
                &job.config,
            )
            .unwrap();
        let legacy_hash = cache
            .contribution_set_hash_for_version_for_test(job.category, &job.config, legacy_version)
            .unwrap();
        cache
            .store_tier2_aggregate(job.key.clone(), &legacy_hash, expected.clone())
            .unwrap();
        assert!(
            cache
                .get_aggregated_for_config(&job.key, &job.config)
                .unwrap()
                .is_none(),
            "version {legacy_version} aggregate must be rejected before rebuilding rows"
        );
        assert!(cache.contributions_need_fact_refresh(job.category).unwrap());
        let result = InspectManager::new()
            .tier2_run_with_reuse_job(&job, &cache, &Tier2ReuseOptions::default())
            .unwrap();
        assert_eq!(result.scanned_files, job.scope_files);
        assert_eq!(result.aggregate, expected);
        assert!(!cache.contributions_need_fact_refresh(job.category).unwrap());
        assert!(
            cache.load_tier2_contributions(job.category).unwrap()[0].contribution["fragments"]
                .as_array()
                .unwrap()
                .iter()
                .all(Value::is_array)
        );
        println!("legacy version {legacy_version}: aggregate rejected, source row rebuilt, exact payload preserved");
    }
}

#[test]
fn tier2_direct_result_returns_facts_without_reloading_rollup() {
    let dir = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(dir.path()).unwrap();
    let path = root.join("src/example.rs");
    write_projection_cache_file(
        &path,
        "fn example() { let number = 1; println!(\"{number}\"); }\n",
    );
    let manager = InspectManager::new();
    let snapshot = tier1_snapshot(&root);
    let run = || {
        manager
            .tier2_run_with_reuse_result(snapshot.clone(), InspectCategory::Duplicates, None)
            .outcome
            .unwrap()
    };
    let work = || super::super::cache::CONTRIBUTION_LOAD_WORK.with(std::cell::Cell::get);
    let reset = || super::super::cache::CONTRIBUTION_LOAD_WORK.with(|work| work.set((0, 0, 0)));
    reset();
    let cold = run();
    assert_eq!(cold.contributions.len(), 1);
    assert_eq!(work().0, 1);
    assert_eq!(work().1, 1);
    assert!(work().2 > 0);
    reset();
    let warm = run();
    assert!(warm.contributions.is_empty());
    assert_eq!(work(), (0, 0, 0));
    write_projection_cache_file(
        &path,
        "fn example() { let number = 333; println!(\"{number}\"); }\n",
    );
    reset();
    let changed = run();
    assert_eq!(changed.contributions.len(), 1);
    assert_eq!(work().0, 1);
    assert_eq!(work().1, 1);
    assert!(work().2 > 0);
    println!(
        "direct result facts: cold={} warm={} changed={}; changed load work={:?}",
        cold.contributions.len(),
        warm.contributions.len(),
        changed.contributions.len(),
        work()
    );
}
