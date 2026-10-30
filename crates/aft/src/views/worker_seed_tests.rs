use super::*;

fn git(root: &Path, args: &[&str]) {
    // Pin line endings: Windows runners default to core.autocrlf=true, which
    // would check the worktree out with CRLF while the main checkout's files
    // were written with LF, so every file would get a new content key.
    assert!(std::process::Command::new("git")
        .current_dir(root)
        .args(["-c", "core.autocrlf=false"])
        .args(args)
        .status()
        .unwrap()
        .success());
}

fn checkouts(files: usize) -> (Vec<tempfile::TempDir>, AssemblyRequest, AssemblyRequest) {
    let main = super::cold_fallback_tests::repository(files);
    fs::write(
        main.path().join("tsconfig.json"),
        r#"{"compilerOptions":{"baseUrl":".","paths":{"@target":["file_0"]}}}"#,
    )
    .unwrap();
    fs::write(
        main.path().join("alias.ts"),
        "import { target_0 } from '@target'; export function alias() { return target_0(); }",
    )
    .unwrap();
    fs::write(
        main.path().join("lib.rs"),
        "mod target; use target::Thing; pub fn caller() { let t = Thing; t.method(); }",
    )
    .unwrap();
    fs::write(
        main.path().join("target.rs"),
        "pub struct Thing; impl Thing { pub fn method(&self) {} }",
    )
    .unwrap();
    git(main.path(), &["add", "."]);
    git(
        main.path(),
        &[
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.com",
            "commit",
            "--quiet",
            "-m",
            "resolver fixtures",
        ],
    );
    let worker = tempfile::tempdir().unwrap();
    git(
        main.path(),
        &[
            "worktree",
            "add",
            "--quiet",
            "--detach",
            worker.path().to_str().unwrap(),
            "HEAD",
        ],
    );
    let storage = tempfile::tempdir().unwrap();
    let request = |root: &Path| AssemblyRequest {
        storage: storage.path().to_path_buf(),
        project_root: root.to_path_buf(),
        family: crate::search_index::artifact_cache_key(root),
        scope: crate::path_identity::project_scope_key(root),
        desired_head: head_tree_fingerprint(&head_tree_entries(root).unwrap()),
        changed_paths: Default::default(),
        semantic_keys: Default::default(),
        require_semantic: false,
        allow_blob_put: true,
        callgraph: true,
    };
    let main_request = request(main.path());
    let worker_request = request(worker.path());
    (vec![main, worker, storage], main_request, worker_request)
}

fn assert_cold_parity(request: &AssemblyRequest, report: &AssemblyReport) {
    let view = ViewStore::open(&request.storage, &request.scope).unwrap();
    let derived = view
        .derived_path(report.generation.as_deref().unwrap())
        .unwrap();
    // Request readers deliberately fail fast during checkpoint contention.
    // This fixture compares settled graph content, not that transient state.
    crate::views::generation::wait_for_derived_checkpoint_for_test(view.view_dir());
    assert_eq!(
        crate::db::file_identity::open_connections(&derived),
        0,
        "the parity fixture must settle its deferred checkpoint before opening a reader"
    );
    let blobs = BlobStore::open(&request.storage, &request.family, BlobPlane::Callgraph).unwrap();
    let cold_dir = tempfile::tempdir().unwrap();
    let cold = cold_dir.path().join("derived-cold.sqlite");
    crate::views::materialization::materialize_manifest_view_database(
        &cold,
        blobs.path(),
        report.manifest.as_ref().unwrap(),
    )
    .unwrap();
    crate::views::materialization::parity::assert_logical_parity(&derived, &cold);
    let _pin =
        crate::pins::QueryPin::acquire(view.view_dir(), report.generation.as_deref().unwrap())
            .unwrap();
    let open = |dir: &Path, generation: &str| {
        crate::callgraph_store::ReadonlyCallGraphStore::open_pinned_derived(
            request.project_root.clone(),
            request.family.clone(),
            dir.to_path_buf(),
            generation,
        )
        .unwrap()
    };
    let seeded = open(view.view_dir(), report.generation.as_deref().unwrap());
    let fresh = open(cold_dir.path(), "cold");
    let primary = if fs::read_to_string(request.project_root.join("file_0.ts"))
        .unwrap()
        .contains("function target_0(")
    {
        "target_0"
    } else {
        "replaced_0"
    };
    for (file, symbol) in [("file_0.ts", primary), ("alias.ts", "alias")] {
        let path = Path::new(file);
        assert_eq!(
            format!("{:?}", seeded.callers_of(path, symbol, 5).unwrap()).as_bytes(),
            format!("{:?}", fresh.callers_of(path, symbol, 5).unwrap()).as_bytes(),
            "callers bytes for {symbol}"
        );
        assert_eq!(
            format!("{:?}", seeded.impact_of(path, symbol, 5).unwrap()).as_bytes(),
            format!("{:?}", fresh.impact_of(path, symbol, 5).unwrap()).as_bytes(),
            "impact bytes for {symbol}"
        );
        assert_eq!(
            serde_json::to_vec(&seeded.trace_to(path, symbol, 5).unwrap()).unwrap(),
            serde_json::to_vec(&fresh.trace_to(path, symbol, 5).unwrap()).unwrap(),
            "trace bytes for {symbol}"
        );
    }
    let dead_code = |reader: &crate::callgraph_store::ReadonlyCallGraphStore| {
        use crate::inspect::{InspectCategory, InspectJob, JobKey};
        let (_, mut snapshot, _, _) =
            crate::callgraph_store::project_dead_code_snapshot_from_view(reader).unwrap();
        // Projection timestamps describe the query invocation, not graph content.
        snapshot.generated_at = None;
        let job = InspectJob {
            job_id: 1,
            key: JobKey::for_project_category(InspectCategory::DeadCode),
            category: InspectCategory::DeadCode,
            scope_files: snapshot.files.clone(),
            project_root: request.project_root.clone(),
            inspect_dir: cold_dir.path().join("inspect"),
            config: std::sync::Arc::new(crate::config::Config {
                project_root: Some(request.project_root.clone()),
                ..Default::default()
            }),
            symbol_cache: std::sync::Arc::new(std::sync::RwLock::new(
                crate::parser::SymbolCache::new(),
            )),
            inspect_writer: true,
            callgraph_writer: true,
            callgraph_snapshot: Some(std::sync::Arc::new(snapshot)),
        };
        serde_json::to_vec(
            &crate::inspect::scanners::dead_code::run_dead_code_scan(&job)
                .outcome
                .unwrap()
                .aggregate,
        )
        .unwrap()
    };
    assert_eq!(
        dead_code(&seeded),
        dead_code(&fresh),
        "dead-code query bytes"
    );
}

#[test]
fn worker_first_publication_clones_main_with_cold_query_parity() {
    let (_dirs, main, mut worker) = checkouts(24);
    let main_report = publish_checkout(&main).unwrap();
    let mut republish = main.clone();
    republish.desired_head = "same-graph-new-generation".into();
    let reused = publish_checkout(&republish).unwrap();
    let main_view = ViewStore::open(&main.storage, &main.scope).unwrap();
    let main_path = main_view
        .derived_path(main_report.generation.as_deref().unwrap())
        .unwrap();
    assert_eq!(
        main_view
            .derived_path(reused.generation.as_deref().unwrap())
            .unwrap(),
        main_path,
        "the donor pointer may name a reused artifact owned by an older manifest"
    );
    let before = crate::views::materialization::parity::logical_snapshot(&main_path);
    fs::write(
        worker.project_root.join("file_0.ts"),
        "export function target_0() { return 999; }",
    )
    .unwrap();
    fs::write(
        worker.project_root.join("target.rs"),
        "pub struct Thing; impl Thing { pub fn method(&self) { helper(); } } fn helper() {} ",
    )
    .unwrap();
    worker
        .changed_paths
        .extend([b"file_0.ts".to_vec(), b"target.rs".to_vec()]);
    fs::remove_file(worker.project_root.join("file_1.ts")).unwrap();
    fs::rename(
        worker.project_root.join("file_2.ts"),
        worker.project_root.join("renamed.ts"),
    )
    .unwrap();
    fs::write(
        worker.project_root.join("added.ts"),
        "import { target_2 } from './renamed'; export function added() { return target_2(); }",
    )
    .unwrap();
    git(&worker.project_root, &["add", "-A"]);
    git(
        &worker.project_root,
        &[
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.com",
            "commit",
            "--quiet",
            "-m",
            "worker membership changes",
        ],
    );
    worker.desired_head = head_tree_fingerprint(&head_tree_entries(&worker.project_root).unwrap());
    worker.changed_paths.extend([
        b"file_1.ts".to_vec(),
        b"file_2.ts".to_vec(),
        b"renamed.ts".to_vec(),
        b"added.ts".to_vec(),
    ]);
    let mut prepared = prepare_checkout(&worker, &mut |_| Ok(())).unwrap();
    assert!(
        prepared.profile.derived_cloned,
        "a new linked checkout must clone its main checkout's compatible derived database"
    );
    assert_eq!(prepared.profile.seed_source, "sibling");
    // A settled published donor has an empty WAL, so macOS copies it
    // copy-on-write instead of rewriting every page.
    #[cfg(target_os = "macos")]
    assert_eq!(prepared.profile.clone_mechanism, "clonefile");
    #[cfg(not(target_os = "macos"))]
    assert_eq!(prepared.profile.clone_mechanism, "sqlite_backup");
    let report = prepared.commit().unwrap();
    assert_eq!(report.blob_puts, 3);
    assert_cold_parity(&worker, &report);
    assert_eq!(
        before,
        crate::views::materialization::parity::logical_snapshot(&main_path),
        "cloning must not mutate the donor"
    );
}

#[test]
fn worker_seed_uses_freshest_ready_sibling_and_repairs_unusable_own_base() {
    let (_dirs, main, mut worker) = checkouts(24);
    let original = publish_checkout(&main).unwrap();
    fs::write(
        worker.project_root.join("file_0.ts"),
        "export function target_0() { return 42; }",
    )
    .unwrap();
    worker.changed_paths.insert(b"file_0.ts".to_vec());
    let newer = publish_checkout(&worker).unwrap();
    let worker_view = ViewStore::open(&worker.storage, &worker.scope).unwrap();
    let newer_path = worker_view
        .derived_path(newer.generation.as_deref().unwrap())
        .unwrap();
    let third = tempfile::tempdir().unwrap();
    git(
        &main.project_root,
        &[
            "worktree",
            "add",
            "--quiet",
            "--detach",
            third.path().to_str().unwrap(),
            "HEAD",
        ],
    );
    let mut third_request = main.clone();
    third_request.project_root = third.path().to_path_buf();
    third_request.scope = crate::path_identity::project_scope_key(third.path());
    let freshest = worker_seed::sibling_checkout_seed(
        &third_request,
        Instant::now() + crate::db::STEADY_BUSY_TIMEOUT,
    )
    .unwrap()
    .unwrap();
    assert_eq!(
        freshest.path, newer_path,
        "a newer ready worker must outrank the main checkout"
    );
    drop(freshest);
    let main_view = ViewStore::open(&main.storage, &main.scope).unwrap();
    let old_path = main_view
        .derived_path(original.generation.as_deref().unwrap())
        .unwrap();
    let deadline = Instant::now() + std::time::Duration::from_secs(10);
    while crate::db::file_identity::open_connections(&old_path) != 0 {
        assert!(Instant::now() < deadline);
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    Connection::open(&old_path)
        .unwrap()
        .execute("UPDATE meta SET v='0' WHERE k='ready'", [])
        .unwrap();
    let seed =
        worker_seed::sibling_checkout_seed(&main, Instant::now() + crate::db::STEADY_BUSY_TIMEOUT)
            .unwrap()
            .unwrap();
    assert_eq!(seed.path, newer_path);
    drop(seed);
    let mut repaired = prepare_checkout(&main, &mut |_| Ok(())).unwrap();
    assert_eq!(repaired.profile.seed_source, "sibling");
    assert_eq!(repaired.profile.cold_build_reason, "none");
    let report = repaired.commit().unwrap();
    assert_cold_parity(&main, &report);
}

/// A separate clone of `main`'s repository: the same family, but not one of
/// its Git worktrees, so the worktree census never offers `main` as a donor.
fn separate_clone(main: &AssemblyRequest) -> (tempfile::TempDir, AssemblyRequest) {
    let clone = tempfile::tempdir().unwrap();
    assert!(std::process::Command::new("git")
        .args(["-c", "core.autocrlf=false", "clone", "--quiet"])
        .arg(&main.project_root)
        .arg(clone.path())
        .status()
        .unwrap()
        .success());
    let mut request = main.clone();
    request.project_root = clone.path().to_path_buf();
    request.scope = crate::path_identity::project_scope_key(clone.path());
    request.family = crate::search_index::artifact_cache_key(clone.path());
    assert_eq!(request.family, main.family, "a clone stays in its family");
    request.desired_head = head_tree_fingerprint(&head_tree_entries(clone.path()).unwrap());
    (clone, request)
}

fn index_of(request: &AssemblyRequest) -> PathBuf {
    let blobs = BlobStore::open(&request.storage, &request.family, BlobPlane::Callgraph).unwrap();
    derived_index::index_path(blobs.path())
}

#[test]
fn identical_scope_outside_the_worktree_census_reuses_the_indexed_graph() {
    let (_dirs, main, _worker) = checkouts(24);
    let published = publish_checkout(&main).unwrap();
    assert_eq!(
        derived_index::recorded_identity(&index_of(&main), &main.scope),
        Some(derived_index::identity(
            &main.family,
            published.manifest.as_ref().unwrap()
        )),
        "a published ready derived database must be indexed"
    );
    let (_clone, clone) = separate_clone(&main);

    let mut prepared = prepare_checkout(&clone, &mut |_| Ok(())).unwrap();

    assert_eq!(prepared.profile.seed_source, "sibling");
    assert_eq!(prepared.profile.seed_lookup, "index_exact");
    assert_eq!(prepared.profile.cold_build_reason, "none");
    assert!(prepared.profile.derived_cloned);
    let report = prepared.commit().unwrap();
    assert_eq!(report.blob_puts, 0);
    assert_cold_parity(&clone, &report);
}

#[test]
fn seeding_prefers_an_identical_donor_over_a_newer_divergent_one() {
    let (_dirs, main, mut worker) = checkouts(128);
    publish_checkout(&main).unwrap();
    // The newest publication in the family diverges past the incremental
    // cutoff, which the worktree census would offer first.
    for i in 0..128 {
        fs::write(
            worker.project_root.join(format!("file_{i}.ts")),
            format!("export function replaced_{i}() {{}} "),
        )
        .unwrap();
        worker
            .changed_paths
            .insert(format!("file_{i}.ts").into_bytes());
    }
    publish_checkout(&worker).unwrap();
    let third = tempfile::tempdir().unwrap();
    git(
        &main.project_root,
        &[
            "worktree",
            "add",
            "--quiet",
            "--detach",
            third.path().to_str().unwrap(),
            "HEAD",
        ],
    );
    let mut third_request = main.clone();
    third_request.project_root = third.path().to_path_buf();
    third_request.scope = crate::path_identity::project_scope_key(third.path());

    let mut prepared = prepare_checkout(&third_request, &mut |_| Ok(())).unwrap();

    assert_eq!(prepared.profile.seed_source, "sibling");
    assert_eq!(prepared.profile.seed_lookup, "index_exact");
    assert_eq!(prepared.profile.cold_build_reason, "none");
    let report = prepared.commit().unwrap();
    assert_cold_parity(&third_request, &report);
}

#[test]
fn near_identical_scope_patches_the_nearest_indexed_donor() {
    let (_dirs, main, _worker) = checkouts(24);
    publish_checkout(&main).unwrap();
    let (_clone, mut clone) = separate_clone(&main);
    fs::write(
        clone.project_root.join("file_0.ts"),
        "export function target_0() { return 5; }",
    )
    .unwrap();
    clone.changed_paths.insert(b"file_0.ts".to_vec());

    let mut prepared = prepare_checkout(&clone, &mut |_| Ok(())).unwrap();

    assert_eq!(prepared.profile.seed_source, "sibling");
    assert_eq!(prepared.profile.seed_lookup, "index_nearest");
    assert!(prepared.profile.derived_cloned);
    let report = prepared.commit().unwrap();
    assert_eq!(report.blob_puts, 1);
    assert_cold_parity(&clone, &report);
}

#[test]
fn indexed_donor_from_an_older_build_output_is_forgotten_not_reused() {
    let (_dirs, main, _worker) = checkouts(24);
    let published = publish_checkout(&main).unwrap();
    let main_view = ViewStore::open(&main.storage, &main.scope).unwrap();
    crate::views::generation::wait_for_derived_checkpoint_for_test(main_view.view_dir());
    let derived = main_view
        .derived_path(published.generation.as_deref().unwrap())
        .unwrap();
    let deadline = Instant::now() + std::time::Duration::from_secs(10);
    while crate::db::file_identity::open_connections(&derived) != 0 {
        assert!(Instant::now() < deadline);
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    // What a donor published by an older binary looks like to this one: the
    // same manifest, but another build-output fingerprint.
    Connection::open(&derived)
        .unwrap()
        .execute(
            "UPDATE meta SET v = 'older-build' WHERE k = 'fingerprint'",
            [],
        )
        .unwrap();
    let (_clone, clone) = separate_clone(&main);

    let mut prepared = prepare_checkout(&clone, &mut |_| Ok(())).unwrap();

    assert_eq!(prepared.profile.seed_source, "cold");
    assert_eq!(prepared.profile.seed_lookup, "none");
    assert_eq!(
        derived_index::recorded_identity(&index_of(&main), &main.scope),
        None,
        "a donor that fails readiness must leave the index"
    );
    let report = prepared.commit().unwrap();
    assert_cold_parity(&clone, &report);
}

#[test]
fn deferred_derived_index_write_is_counted_in_the_publication_io() {
    let (_dirs, main, _worker) = checkouts(24);
    let view_dir = ViewStore::open(&main.storage, &main.scope)
        .unwrap()
        .view_dir()
        .to_owned();
    let root_id = main.project_root.display().to_string();
    let mut prepared = prepare_checkout(&main, &mut |_| Ok(())).unwrap();
    prepared.commit().unwrap();
    let field = |fields: &str, name: &str| -> Option<u64> {
        fields
            .split(' ')
            .find_map(|field| field.strip_prefix(name)?.strip_prefix('='))?
            .parse()
            .ok()
    };
    let before = prepared.profile.io.fields();
    let ledger_before =
        crate::write_ledger::pending_for_test(crate::write_ledger::Domain::ViewsDerived, &root_id);

    // The row is written here, after `commit` closed the publication's phases.
    prepared.settle_status();

    assert_eq!(index_writes_for_test(&view_dir), vec![IndexWrite::Recorded]);
    let after = prepared.profile.io.fields();
    assert!(
        publication_profile_line(&prepared.profile).contains("derived_index_ms="),
        "the publication line must report the deferred write's time"
    );
    if cfg!(any(target_os = "macos", target_os = "linux")) {
        let written = field(&after, "derived_index_logical_bytes_written").unwrap();
        assert_eq!(
            field(&before, "derived_index_logical_bytes_written"),
            Some(0)
        );
        assert!(
            written > 0,
            "the deferred SQLite write must reach the publication's I/O: {after}"
        );
        for total in [
            "derived_logical_bytes_written",
            "total_logical_bytes_written",
        ] {
            assert_eq!(
                field(&after, total).unwrap() - field(&before, total).unwrap(),
                written,
                "{total} must include the deferred write"
            );
        }
        let ledger_after = crate::write_ledger::pending_for_test(
            crate::write_ledger::Domain::ViewsDerived,
            &root_id,
        );
        assert_eq!(
            ledger_after.0 - ledger_before.0,
            written,
            "the views_derived ledger must be credited with the deferred write"
        );
    }
}

#[test]
fn publication_writes_a_reference_sidecar_matching_its_manifest() {
    let (_dirs, main, _worker) = checkouts(4);
    publish_checkout(&main).unwrap();
    let view = ViewStore::open(&main.storage, &main.scope).unwrap();
    let generation = view.current_generation_read_only().unwrap().unwrap();
    let json = fs::read(view.manifest_path(&generation).unwrap()).unwrap();
    let expected: Vec<[u8; 32]> =
        crate::views::generation::decode_reference_keys_for_test(&generation, &json)
            .unwrap()
            .into_iter()
            .collect();
    assert!(!expected.is_empty());
    // A policy with a long settle window still trusts the publisher's sidecar.
    let sidecars = crate::views::reference_sidecar::ReferenceSidecars::new(
        std::time::Duration::from_secs(3600),
    );
    let mut seen = Vec::new();
    // Room is not under test here.
    let mut unlimited = usize::MAX;
    let source = sidecars
        .visit(
            view.view_dir(),
            &generation,
            |_| panic!("the publisher's sidecar should make decoding unnecessary"),
            &mut |keys| {
                seen = vec![[0_u8; 32]; keys.len()];
                keys.read(0, &mut seen)
            },
            &mut unlimited,
        )
        .unwrap();
    assert_eq!(source, crate::views::reference_sidecar::KeySource::Sidecar);
    assert_eq!(seen, expected);
}

#[test]
fn derived_index_row_of_a_superseded_publication_is_never_written() {
    let (_dirs, mut main, _worker) = checkouts(24);
    publish_checkout(&main).unwrap();
    let view_dir = ViewStore::open(&main.storage, &main.scope)
        .unwrap()
        .view_dir()
        .to_owned();
    let edit = |request: &mut AssemblyRequest, body: &str| {
        fs::write(
            request.project_root.join("file_0.ts"),
            format!("export function target_0() {{ return {body}; }}"),
        )
        .unwrap();
        request.changed_paths.insert(b"file_0.ts".to_vec());
    };
    edit(&mut main, "1");
    let mut older = prepare_checkout(&main, &mut |_| Ok(())).unwrap();
    let older_generation = older.commit().unwrap().generation.unwrap();
    // A newer publication is committed and settled before the older one's
    // deferred index write runs.
    edit(&mut main, "2");
    let newer = publish_checkout(&main).unwrap();
    older.settle_status();

    assert_eq!(
        index_writes_for_test(&view_dir),
        vec![
            IndexWrite::Recorded,
            IndexWrite::Recorded,
            IndexWrite::Superseded
        ]
    );
    let rows = derived_index::candidates(&index_of(&main), &[0; 32], "some-other-scope").unwrap();
    let row = rows.iter().find(|row| row.scope == main.scope).unwrap();
    assert_eq!(Some(&row.generation), newer.generation.as_ref());
    assert_ne!(row.generation, older_generation);
}

#[test]
fn worker_seed_copy_that_fails_readiness_is_rebuilt_cold_not_served() {
    // A copy that is well-formed SQLite but whose build never finished, and a
    // torn copy cut off partway through. Neither may be patched into the
    // worker's generation.
    type Damage = fn(&Path);
    let unfinished: Damage = |derived| {
        Connection::open(derived)
            .unwrap()
            .execute("UPDATE meta SET v = '0' WHERE k = 'ready'", [])
            .unwrap();
    };
    let torn: Damage = |derived| {
        let file = fs::OpenOptions::new().write(true).open(derived).unwrap();
        let len = file.metadata().unwrap().len();
        file.set_len(len / 2).unwrap();
    };
    for (label, damage, expected) in [
        ("unfinished", unfinished, &["base_not_ready"][..]),
        ("torn", torn, &["base_not_ready", "base_unreadable"][..]),
    ] {
        let (_dirs, main, mut worker) = checkouts(24);
        publish_checkout(&main).unwrap();
        crate::views::generation::wait_for_derived_checkpoint_for_test(
            ViewStore::open(&main.storage, &main.scope)
                .unwrap()
                .view_dir(),
        );
        fs::write(
            worker.project_root.join("file_0.ts"),
            "export function target_0() { return 7; }",
        )
        .unwrap();
        worker.changed_paths.insert(b"file_0.ts".to_vec());
        super::after_derived_clone_for_test(damage);
        let mut prepared = prepare_checkout(&worker, &mut |_| Ok(())).unwrap();
        assert_eq!(prepared.profile.seed_source, "cold", "{label}");
        assert!(
            expected.contains(&prepared.profile.cold_build_reason),
            "{label}: {}",
            prepared.profile.cold_build_reason
        );
        let report = prepared.commit().unwrap();
        assert_cold_parity(&worker, &report);
    }
}

#[test]
fn worker_seed_rejects_foreign_family_and_oversized_diff() {
    let (_dirs, main, mut worker) = checkouts(128);
    publish_checkout(&main).unwrap();
    worker.family = "unrelated-family".into();
    assert!(worker_seed::sibling_checkout_seed(
        &worker,
        Instant::now() + crate::db::STEADY_BUSY_TIMEOUT
    )
    .unwrap()
    .is_none());
    worker.family = main.family;
    for i in 0..128 {
        fs::write(
            worker.project_root.join(format!("file_{i}.ts")),
            format!("export function replaced_{i}() {{}} "),
        )
        .unwrap();
        worker
            .changed_paths
            .insert(format!("file_{i}.ts").into_bytes());
    }
    let mut prepared = prepare_checkout(&worker, &mut |_| Ok(())).unwrap();
    assert!(
        !prepared.profile.derived_cloned,
        "the diff cutoff must apply before copying a sibling database"
    );
    assert_eq!(prepared.profile.seed_source, "cold");
    assert_eq!(prepared.profile.cold_build_reason, "large_diff");
    let report = prepared.commit().unwrap();
    assert_cold_parity(&worker, &report);
}

#[test]
#[ignore = "offline publication measurement with 4,891 tracked files and a linked checkout"]
fn bench_4891_file_worker_publication() {
    let (_dirs, main, worker) = checkouts(4887);
    let mut initial = prepare_checkout(&main, &mut |_| Ok(())).unwrap();
    println!(
        "main files=4891 materialize_ms={}",
        initial.profile.materialization_call_ms
    );
    initial.commit().unwrap();
    let started = Instant::now();
    let mut prepared = prepare_checkout(&worker, &mut |_| Ok(())).unwrap();
    println!(
        "worker files=4891 publication_ms={} materialize_ms={} clone_ms={} cloned={}",
        started.elapsed().as_millis(),
        prepared.profile.materialization_call_ms,
        prepared.profile.derived_clone_ms,
        prepared.profile.derived_cloned
    );
    let report = prepared.commit().unwrap();
    assert_eq!(report.manifest.as_ref().unwrap().entries().count(), 4891);
    assert_eq!(report.blob_puts, 0);
    assert_cold_parity(&worker, &report);
}

#[test]
#[ignore = "offline cold materialization of this repository's committed source corpus"]
fn bench_repository_cold_materialization() {
    use std::io::Write;
    use std::process::{Command, Stdio};
    let project = tempfile::tempdir().unwrap();
    let storage = tempfile::tempdir().unwrap();
    let archive = Command::new("git")
        .args(["archive", "HEAD"])
        .output()
        .unwrap();
    assert!(archive.status.success());
    let mut unpack = Command::new("tar")
        .arg("-x")
        .current_dir(project.path())
        .stdin(Stdio::piped())
        .spawn()
        .unwrap();
    unpack
        .stdin
        .take()
        .unwrap()
        .write_all(&archive.stdout)
        .unwrap();
    assert!(unpack.wait().unwrap().success());
    git(project.path(), &["init", "--quiet"]);
    git(project.path(), &["add", "."]);
    git(
        project.path(),
        &[
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.com",
            "commit",
            "--quiet",
            "-m",
            "repository corpus",
        ],
    );
    let request = AssemblyRequest {
        storage: storage.path().to_path_buf(),
        project_root: project.path().to_path_buf(),
        family: crate::search_index::artifact_cache_key(project.path()),
        scope: crate::path_identity::project_scope_key(project.path()),
        desired_head: head_tree_fingerprint(&head_tree_entries(project.path()).unwrap()),
        changed_paths: Default::default(),
        semantic_keys: Default::default(),
        require_semantic: false,
        allow_blob_put: true,
        callgraph: true,
    };
    crate::blob_store::PAYLOAD_READ_COUNTS.with(|counts| counts.set(Default::default()));
    let prepared = prepare_checkout(&request, &mut |_| Ok(())).unwrap();
    let reads = crate::blob_store::PAYLOAD_READ_COUNTS.with(|counts| counts.get());
    let (path, connection) = prepared.derived_checkpoint.as_ref().unwrap();
    let unknown: usize = connection
        .query_row("SELECT COUNT(*) FROM view_unknown_live", [], |row| {
            row.get(0)
        })
        .unwrap();
    println!("repository files={} materialize_ms={} derived_bytes={} unknown_live_rows={} payload_reads={} payload_bytes={}", prepared.report.manifest.as_ref().unwrap().entries().count(), prepared.profile.materialization_call_ms, fs::metadata(path).unwrap().len(), unknown, reads.blake3_calls, reads.bytes);
    for name in [
        "refs",
        "edges",
        "view_bindings",
        "view_unknown_live",
        "sqlite_autoindex_view_unknown_live_1",
    ] {
        let bytes: i64 = connection
            .query_row(
                "SELECT COALESCE(SUM(pgsize),0) FROM dbstat WHERE name=?1",
                [name],
                |row| row.get(0),
            )
            .unwrap();
        println!("repository table={name} allocated_bytes={bytes}");
    }
}

/// Bytes written by a sibling-seeded worker publication, with the donor copied
/// by SQLite backup (the previous behavior) and copy-on-write (macOS).
///
/// Set `AFT_IO_PROBE_DIR` to an empty directory and `AFT_IO_PROBE_ROOT` to a
/// Git checkout. The driver clones that checkout into the probe directory,
/// adds two linked worktrees with the same one-file edit, publishes the clone
/// cold, then publishes each worktree seeded from it. It prints `IO_PROBE`
/// JSON lines like the other view I/O drivers.
#[cfg(target_os = "macos")]
#[test]
#[ignore = "builds a real checkout's views twice; run single-threaded with isolated HOME/XDG"]
fn io_amplification_sibling_seed_probe() {
    use crate::process_io::Bytes;
    use crate::write_ledger::{pending_for_test, Domain};
    use std::sync::atomic::Ordering;

    let base =
        PathBuf::from(std::env::var_os("AFT_IO_PROBE_DIR").expect("set isolated probe directory"));
    let source = PathBuf::from(std::env::var_os("AFT_IO_PROBE_ROOT").expect("set checkout root"));
    fs::create_dir_all(&base).unwrap();
    let base = base.canonicalize().unwrap();
    let main_root = base.join("main");
    assert!(!main_root.exists(), "use an empty directory for a cold run");
    assert!(std::process::Command::new("git")
        .args(["clone", "--quiet", "--local"])
        .arg(&source)
        .arg(&main_root)
        .status()
        .unwrap()
        .success());
    let workers = ["worker-backup", "worker-clonefile"].map(|name| base.join(name));
    for worker in &workers {
        git(
            &main_root,
            &[
                "worktree",
                "add",
                "--quiet",
                "--detach",
                worker.to_str().unwrap(),
                "HEAD",
            ],
        );
    }
    let storage = base.join("store");
    let request = |root: &Path| AssemblyRequest {
        storage: storage.clone(),
        project_root: root.to_path_buf(),
        family: crate::search_index::artifact_cache_key(root),
        scope: crate::path_identity::project_scope_key(root),
        desired_head: head_tree_fingerprint(&head_tree_entries(root).unwrap()),
        changed_paths: Default::default(),
        semantic_keys: Default::default(),
        require_semantic: false,
        allow_blob_put: true,
        callgraph: true,
    };
    let measure = |label: &str, request: &AssemblyRequest| {
        let root_id = request.project_root.display().to_string();
        let domains = [
            Domain::ViewsBlob,
            Domain::ViewsDerived,
            Domain::ViewsClosure,
        ];
        let before_domains = domains.map(|domain| pending_for_test(domain, &root_id));
        let before = Bytes::capture().unwrap();
        let started = Instant::now();
        let mut prepared = prepare_checkout(request, &mut |_| Ok(())).unwrap();
        let seed_source = prepared.profile.seed_source;
        let cold_build_reason = prepared.profile.cold_build_reason;
        let clone_mechanism = prepared.profile.clone_mechanism;
        let clone_ms = prepared.profile.derived_clone_ms;
        let report = prepared.commit().unwrap();
        let io = Bytes::capture().unwrap().delta(before).unwrap();
        let elapsed_ms = started.elapsed().as_millis();
        for (domain, baseline) in domains.into_iter().zip(before_domains) {
            let after = pending_for_test(domain, &root_id);
            eprintln!(
                "IO_PROBE {}",
                serde_json::json!({
                    "kind": "domain", "label": label, "domain": domain.as_str(),
                    "logical_bytes": after.0 - baseline.0,
                    "physical_bytes": after.1 - baseline.1,
                })
            );
        }
        eprintln!(
            "IO_PROBE {}",
            serde_json::json!({
                "kind": "publication", "label": label, "io": io,
                "elapsed_ms": elapsed_ms, "clone_ms": clone_ms,
                "seed_source": seed_source, "cold_build_reason": cold_build_reason,
                "clone_mechanism": clone_mechanism, "blob_puts": report.blob_puts,
            })
        );
        let view = ViewStore::open(&request.storage, &request.scope).unwrap();
        crate::views::generation::wait_for_derived_checkpoint_for_test(view.view_dir());
        (seed_source, clone_mechanism)
    };
    measure("main_cold", &request(&main_root));
    // The same small edit in both worktrees, so both patches do the same work.
    let edited = "crates/aft/src/views/profile.rs";
    for (worker, force_backup) in workers.iter().zip([true, false]) {
        let path = worker.join(edited);
        let mut text = fs::read_to_string(&path).unwrap();
        text.push_str("\n// sibling seed probe edit\n");
        fs::write(&path, text).unwrap();
        let mut worker_request = request(worker);
        worker_request
            .changed_paths
            .insert(edited.as_bytes().to_vec());
        crate::views::generation::FORCE_BACKUP_CLONE_FOR_TEST
            .store(force_backup, Ordering::Relaxed);
        let label = if force_backup {
            "worker_sqlite_backup"
        } else {
            "worker_clonefile"
        };
        let (seed_source, mechanism) = measure(label, &worker_request);
        assert_eq!(seed_source, "sibling", "{label}");
        assert_eq!(
            mechanism,
            if force_backup {
                "sqlite_backup"
            } else {
                "clonefile"
            },
            "{label}"
        );
    }
    crate::views::generation::FORCE_BACKUP_CLONE_FOR_TEST.store(false, Ordering::Relaxed);
}
