fn view_projection_fixture() -> (
    tempfile::TempDir,
    tempfile::TempDir,
    InspectJob,
    crate::views::assembly::AssemblyRequest,
) {
    let _ = env_logger::builder().is_test(true).try_init();
    let project = tempfile::tempdir().unwrap();
    let storage = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(project.path()).unwrap();
    write_projection_cache_file(
        &root.join("main.ts"),
        "import { used } from './target';\nexport function main() { used(); }\nmain();\n",
    );
    write_projection_cache_file(
        &root.join("target.ts"),
        "export function used() {}\nexport function dead() {}\n",
    );
    for args in [
        vec!["init", "--quiet"],
        vec!["add", "."],
        vec![
            "-c",
            "user.name=AFT",
            "-c",
            "user.email=aft@example.com",
            "commit",
            "--quiet",
            "-m",
            "fixture",
        ],
    ] {
        let mut command = std::process::Command::new("git");
        crate::test_env::apply_hermetic_git_env(command.current_dir(&root));
        assert!(command.args(args).status().unwrap().success());
    }
    let family = crate::search_index::artifact_cache_key(&root);
    crate::root_cache::configure_artifact_access(&root, &family, false);
    let inspect_dir = storage.path().join("inspect");
    let callgraph_dir = callgraph_store_dir_from_inspect_dir(&inspect_dir, &root).unwrap();
    let files = crate::callgraph::walk_project_files(&root).collect::<Vec<_>>();
    drop(CallGraphStore::cold_build_with_lease(callgraph_dir, root.clone(), &files).unwrap());
    let mut job = snapshot_job(&root, &inspect_dir, true);
    job.scope_files = files;
    let config = Arc::make_mut(&mut job.config);
    config.storage_dir = Some(storage.path().to_path_buf());
    config.views.enabled = true;
    job.callgraph_writer = true;
    let request = crate::views::assembly::AssemblyRequest {
        storage: storage.path().to_path_buf(),
        project_root: root.clone(),
        family,
        scope: crate::path_identity::project_scope_key(&root),
        desired_head: crate::views::assembly::head_tree_fingerprint(
            &crate::alias::head_tree_entries(&root).unwrap(),
        ),
        changed_paths: Default::default(),
        semantic_keys: Default::default(),
        require_semantic: false,
        allow_blob_put: true,
        callgraph: true,
    };
    (project, storage, job, request)
}

#[test]
fn views_tier2_published_plane_never_refreshes_legacy() {
    let (_project, _storage, job, request) = view_projection_fixture();
    crate::views::assembly::publish_checkout(&request).unwrap();
    let before = LEGACY_VIEW_REFRESHES.with(std::cell::Cell::get);
    let snapshot = build_tier2_callgraph_snapshot_with_refresh(&job, true, &job.scope_files);
    assert!(snapshot.is_some(), "published view must project");
    assert_eq!(
        LEGACY_VIEW_REFRESHES.with(std::cell::Cell::get),
        before,
        "views-on Tier-2 refreshed the legacy store"
    );
}

#[test]
fn inspect_checkout_view_warm_verification_does_not_rehash_corpus() {
    let (_project, _storage, mut job, mut request) = view_projection_fixture();
    for i in 0..3000 {
        write_projection_cache_file(&job.project_root.join(format!("source_{i}.ts")), &format!("export function source_{i}() {{ return {i}; }}\n"));
    }
    for args in [vec!["add", "."], vec!["-c", "user.name=AFT", "-c", "user.email=aft@example.com", "commit", "--quiet", "-m", "large fixture"]] {
        let mut git = std::process::Command::new("git");
        crate::test_env::apply_hermetic_git_env(git.current_dir(&job.project_root));
        assert!(git.args(args).status().unwrap().success());
    }
    request.desired_head = crate::views::assembly::head_tree_fingerprint(&crate::alias::head_tree_entries(&job.project_root).unwrap());
    crate::views::assembly::publish_checkout(&request).unwrap();
    job.scope_files = crate::callgraph::walk_project_files(&job.project_root).collect();
    let snapshot = InspectSnapshot::new_with_capabilities(job.project_root.clone(), job.inspect_dir.clone(), job.config.clone(), job.symbol_cache.clone(), false, false);
    let manager = InspectManager::new();
    let stats = job.scope_files.iter().map(|path| {
        let metadata = std::fs::metadata(path).unwrap();
        (path.clone(), metadata.len(), metadata.modified().unwrap())
    }).collect::<Vec<_>>();
    crate::views::read::take_verification_io();
    assert!(manager.current_checkout_view(&snapshot, Some(&stats)).is_some());
    let cold = crate::views::read::take_verification_io();
    assert!(manager.current_checkout_view(&snapshot, Some(&stats)).is_some());
    let warm = crate::views::read::take_verification_io();
    eprintln!("inspect_view_verification fixture_files={} cold={cold:?} warm={warm:?}", job.scope_files.len());
    assert_eq!(warm.files_read, 0, "unchanged view verification reread source files: {warm:?}");
    assert_eq!(warm.bytes_hashed, 0, "unchanged view verification rehashed source bytes: {warm:?}");
    assert_eq!(warm.files_statd, 0, "blocking inspection must reuse the root stats it already collected");
    assert_eq!(cold.files_read, 3002, "first verification must actually read the entire source set");
    assert!(manager.current_checkout_view(&snapshot, None).is_some());
    let standalone = crate::views::read::take_verification_io();
    eprintln!("inspect_view_verification standalone={standalone:?}");
    assert_eq!(standalone.files_statd, 3002);
    assert_eq!(standalone.files_read, 0);
    // A watcher event must defeat the memo even if an external writer preserved
    // both size and mtime. The known root stats deliberately remain unchanged.
    let edited = job.project_root.join("source_0.ts");
    let modified = std::fs::metadata(&edited).unwrap().modified().unwrap();
    std::fs::write(&edited, "export function source_0() { return 9; }\n").unwrap();
    std::fs::File::open(&edited).unwrap().set_modified(modified).unwrap();
    crate::cache_freshness::invalidate_verify_memo(&job.project_root);
    assert!(manager.current_checkout_view(&snapshot, Some(&stats)).is_none());
    assert!(crate::views::read::take_verification_io().files_read > 0);
}

#[test]
fn views_tier2_pending_plane_never_falls_back_to_legacy() {
    let (_project, _storage, job, _request) = view_projection_fixture();
    let before = LEGACY_VIEW_REFRESHES.with(std::cell::Cell::get);
    assert!(
        build_tier2_callgraph_snapshot_with_refresh(&job, true, &job.scope_files).is_none(),
        "an unpublished view is pending even when legacy is ready"
    );
    assert_eq!(LEGACY_VIEW_REFRESHES.with(std::cell::Cell::get), before);
}

#[test]
fn views_tier2_and_legacy_dead_code_verdicts_match() {
    let (_project, _storage, mut job, request) = view_projection_fixture();
    for source in [
        "export function used() {}\nexport function dead() {}\n",
        "export function used() { dead(); }\nexport function dead() {}\n",
        "export function used() {}\nexport function dead() {}\n",
        "export function used() { alternate(); }\nexport function alternate() {}\n",
        "export function used() {}\nexport function dead() {}\n",
    ] {
        write_projection_cache_file(&job.project_root.join("target.ts"), source);
        crate::views::assembly::publish_checkout(&request).unwrap();
        Arc::make_mut(&mut job.config).views.enabled = true;
        let view =
            build_tier2_callgraph_snapshot_with_refresh(&job, true, &job.scope_files).unwrap();
        Arc::make_mut(&mut job.config).views.enabled = false;
        let legacy =
            build_tier2_callgraph_snapshot_with_refresh(&job, true, &job.scope_files).unwrap();
        job.callgraph_snapshot = Some(legacy);
        let contributions = crate::inspect::scanners::dead_code::run_dead_code_scan(&job)
            .outcome
            .unwrap()
            .contributions;
        assert!(!contributions.is_empty());
        let legacy_verdict = roll_up_dead_code_contributions(&job, &contributions, None);
        job.callgraph_snapshot = Some(view);
        let view_verdict = roll_up_dead_code_contributions(&job, &contributions, None);
        assert_eq!(
            view_verdict, legacy_verdict,
            "same contribution set after source transition: {source}"
        );
    }
}

#[test]
fn views_navigation_shared_reader_preserves_pinned_generation() {
    let (_project, _storage, _job, mut request) = view_projection_fixture();
    let first = crate::views::assembly::publish_checkout(&request)
        .unwrap()
        .generation
        .unwrap();
    let view = crate::views::ViewStore::open(&request.storage, &request.scope).unwrap();
    let pin = Some(Arc::new(
        crate::pins::QueryPin::acquire(view.view_dir(), &first).unwrap(),
    ));
    write_projection_cache_file(
        &request.project_root.join("target.ts"),
        "export function used() { return 2; }\n",
    );
    request.changed_paths.insert(b"target.ts".to_vec());
    let second = crate::views::assembly::publish_checkout(&request)
        .unwrap()
        .generation
        .unwrap();
    assert_ne!(first, second);
    let before = ReadonlyCallGraphStore::open_manifest_view(
        request.project_root.clone(),
        request.family.clone(),
        view.view_dir().to_path_buf(),
        &first,
        pin.clone(),
    )
    .unwrap();
    let after = crate::views::read::open_published_callgraph(
        request.project_root,
        request.family,
        view.view_dir().to_path_buf(),
        &first,
        pin,
    )
    .unwrap();
    assert_eq!(before.sqlite_path(), after.sqlite_path());
    assert_eq!(after.sqlite_path(), view.derived_path(&first).unwrap());
}

#[test]
fn views_tier2_head_mismatch_is_pending() {
    let (_project, _storage, job, request) = view_projection_fixture();
    crate::views::assembly::publish_checkout(&request).unwrap();
    write_projection_cache_file(
        &request.project_root.join("target.ts"),
        "export function used() { return 2; }\n",
    );
    for args in [
        vec!["add", "."],
        vec![
            "-c",
            "user.name=AFT",
            "-c",
            "user.email=aft@example.com",
            "commit",
            "--quiet",
            "-m",
            "changed",
        ],
    ] {
        let mut command = std::process::Command::new("git");
        crate::test_env::apply_hermetic_git_env(command.current_dir(&request.project_root));
        assert!(command.args(args).status().unwrap().success());
    }
    let before = LEGACY_VIEW_REFRESHES.with(std::cell::Cell::get);
    assert!(build_tier2_callgraph_snapshot_with_refresh(&job, true, &job.scope_files).is_none());
    assert_eq!(LEGACY_VIEW_REFRESHES.with(std::cell::Cell::get), before);
}

#[test]
fn views_tier2_watcher_edit_is_pending_until_publication() {
    let (_project, _storage, job, mut request) = view_projection_fixture();
    let view = crate::views::ViewStore::open(&request.storage, &request.scope).unwrap();
    crate::views::assembly::publish_checkout(&request).unwrap();
    crate::views::wait_for_derived_checkpoint_for_test(view.view_dir());
    let path = request.project_root.join("target.ts");
    write_projection_cache_file(&path, "export function used() { return 2; }\n");
    let before = LEGACY_VIEW_REFRESHES.with(std::cell::Cell::get);
    assert!(
        build_tier2_callgraph_snapshot_with_refresh(&job, true, std::slice::from_ref(&path))
            .is_none()
    );
    request.changed_paths.insert(b"target.ts".to_vec());
    crate::views::assembly::publish_checkout(&request).unwrap();
    // Publication schedules a detached checkpoint whose keeper close can briefly
    // contend with a zero-wait reader. Test content freshness after it settles.
    crate::views::wait_for_derived_checkpoint_for_test(view.view_dir());
    assert!(build_tier2_callgraph_snapshot_with_refresh(&job, true, &[path]).is_some());
    assert_eq!(LEGACY_VIEW_REFRESHES.with(std::cell::Cell::get), before);
}

#[test]
fn views_projection_adapter_refuses_legacy_reader() {
    let (_project, _storage, job, _request) = view_projection_fixture();
    let dir = callgraph_store_dir_from_inspect_dir(&job.inspect_dir, &job.project_root).unwrap();
    let legacy = CallGraphStore::open_readonly(dir, job.project_root.clone())
        .unwrap()
        .unwrap();
    let error = crate::callgraph_store::project_dead_code_snapshot_from_view(&legacy).unwrap_err();
    assert!(error.to_string().contains("requires a view reader"));
}

#[test]
#[ignore = "manual read latency benchmark requires AFT_HUNT_ROOT and AFT_HUNT_STORAGE for completed drill artifacts"]
fn views_profile_navigation_reads_on_drill_artifacts() {
    let root = PathBuf::from(std::env::var_os("AFT_HUNT_ROOT").expect("AFT_HUNT_ROOT"));
    let storage = PathBuf::from(std::env::var_os("AFT_HUNT_STORAGE").expect("AFT_HUNT_STORAGE"));
    let family = crate::search_index::artifact_cache_key(&root);
    let view =
        crate::views::ViewStore::open(&storage, &crate::path_identity::project_scope_key(&root))
            .unwrap();
    let generation = view.current_generation().unwrap().unwrap();
    let pin = Some(Arc::new(
        crate::pins::QueryPin::acquire(view.view_dir(), &generation).unwrap(),
    ));
    let legacy =
        CallGraphStore::open_readonly(storage.join("callgraph").join(&family), root.clone())
            .unwrap()
            .unwrap();
    let path = Path::new("packages/app/src/settings/timeline-detail.tsx");
    let symbol = "TimelineDetailControl";
    let mut rows = Vec::new();
    for use_view in [false, true] {
        let mut opens = Duration::ZERO;
        let mut callers = Duration::ZERO;
        let mut impact = Duration::ZERO;
        let mut caller_count = 0;
        let mut impact_count = 0;
        for _ in 0..10 {
            let start = Instant::now();
            let selected = use_view.then(|| {
                crate::views::read::open_published_callgraph(
                    root.clone(),
                    family.clone(),
                    view.view_dir().to_path_buf(),
                    &generation,
                    pin.clone(),
                )
                .unwrap()
            });
            opens += start.elapsed();
            let reader = selected.as_ref().unwrap_or(&legacy);
            let start = Instant::now();
            caller_count = reader.callers_of(path, symbol, 3).unwrap().callers.len();
            callers += start.elapsed();
            let start = Instant::now();
            impact_count = reader.impact_of(path, symbol, 3).unwrap().callers.len();
            impact += start.elapsed();
        }
        assert!(caller_count > 0 && impact_count > 0);
        eprintln!("navigation_read source={} queries=10 open_us={} callers_us={} impact_us={} callers={} impacted={}", if use_view { "view_reopened" } else { "legacy_retained" }, opens.as_micros()/10, callers.as_micros()/10, impact.as_micros()/10, caller_count, impact_count);
        rows.push((caller_count, impact_count));
    }
    assert_eq!(rows[0], rows[1]);
}

#[test]
fn views_tier2_keyless_generation_reports_callgraph_disabled_not_empty() {
    let (_project, _storage, job, mut request) = view_projection_fixture();
    // Publish the current generation without call graph data, as a session
    // with the call graph off does.
    request.callgraph = false;
    crate::views::assembly::publish_checkout(&request).unwrap();
    // Both with and without paths to verify against the generation: the
    // path check alone already rejects a keyless entry, so the projection
    // with no refresh paths is the one the generation-wide guard protects.
    for refresh_paths in [&job.scope_files[..], &[]] {
        assert!(
            build_tier2_callgraph_snapshot_with_refresh(&job, true, refresh_paths).is_none(),
            "a generation without call graph data must not project an empty graph"
        );
    }
    let aggregate = crate::inspect::scanners::dead_code::callgraph_unavailable_aggregate_for_job(&job);
    assert_eq!(aggregate["callgraph_available"], false);
    assert_eq!(
        aggregate["callgraph_unavailable_reason"],
        crate::views::read::CALLGRAPH_DISABLED
    );
    assert_eq!(aggregate["notes"][1], "callgraph_disabled");
    // With the index itself off, the reason names that configuration setting.
    let mut off = job.clone();
    Arc::make_mut(&mut off.config).indexes.callgraph = false;
    let aggregate = crate::inspect::scanners::dead_code::callgraph_unavailable_aggregate_for_job(&off);
    assert_eq!(
        aggregate["callgraph_unavailable_reason"],
        "call graph is disabled (indexes.callgraph=false)"
    );
}

#[test]
fn views_keyless_callgraph_generation_preserves_unused_exports_but_names_dead_code_gap() {
    let (_project, _storage, mut job, mut request) = view_projection_fixture();
    request.callgraph = false;
    crate::views::assembly::publish_checkout(&request).unwrap();
    let view = crate::views::ViewStore::open(&request.storage, &request.scope).unwrap();
    crate::views::wait_for_derived_checkpoint_for_test(view.view_dir());

    // Unused exports is an independent import/export analysis, not a call-graph
    // projection. A generation without graph keys must preserve its real findings
    // while dead-code reachability remains unknown, never a zero-findings result.
    job.category = InspectCategory::UnusedExports;
    let scan = crate::inspect::scanners::unused_exports::run_unused_exports_scan(&job)
        .outcome
        .unwrap();
    let unused = roll_up_unused_exports_contributions(&job, &scan.contributions, None);
    assert_eq!(unused["count"], 2, "{unused:#}");
    // Item order is not part of this contract (it differs across platforms);
    // what matters is that both real findings survive.
    let mut symbols = unused["items"]
        .as_array()
        .expect("unused export items")
        .iter()
        .map(|item| item["symbol"].as_str().unwrap_or_default().to_string())
        .collect::<Vec<_>>();
    symbols.sort();
    assert_eq!(symbols, ["dead", "main"], "{unused:#}");

    job.category = InspectCategory::DeadCode;
    // No refresh-path check: this specifically exercises the generation guard,
    // rather than an unrelated mismatch between source bytes and missing keys.
    job.callgraph_snapshot = build_tier2_callgraph_snapshot_with_refresh(&job, true, &[]);
    let dead = crate::inspect::scanners::dead_code::run_dead_code_scan(&job)
        .outcome
        .unwrap()
        .aggregate;
    assert_eq!(dead["callgraph_available"], false, "{dead:#}");
    assert_eq!(
        dead["callgraph_unavailable_reason"],
        crate::views::read::CALLGRAPH_DISABLED,
        "{dead:#}"
    );
    assert!(
        dead.get("count").is_none(),
        "unknown dead code is not zero findings: {dead:#}"
    );
    assert_eq!(
        roll_up_dead_code_contributions(&job, &scan.contributions, None),
        dead
    );
}
