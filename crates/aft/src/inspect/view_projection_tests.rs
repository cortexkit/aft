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
    crate::views::assembly::publish_checkout(&request).unwrap();
    let path = request.project_root.join("target.ts");
    write_projection_cache_file(&path, "export function used() { return 2; }\n");
    let before = LEGACY_VIEW_REFRESHES.with(std::cell::Cell::get);
    assert!(
        build_tier2_callgraph_snapshot_with_refresh(&job, true, std::slice::from_ref(&path))
            .is_none()
    );
    request.changed_paths.insert(b"target.ts".to_vec());
    crate::views::assembly::publish_checkout(&request).unwrap();
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
