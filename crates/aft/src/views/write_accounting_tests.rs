use super::*;

fn git(root: &Path, arguments: &[&str]) {
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(root)
        .args(arguments)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn publication_store_writes_are_local_and_noop_is_zero() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("checkout");
    std::fs::create_dir(&root).unwrap();
    for index in 0..320 {
        std::fs::write(
            root.join(format!("file_{index:03}.ts")),
            format!("export function value_{index}() {{ return 1; }}\n"),
        )
        .unwrap();
    }
    git(&root, &["init", "--quiet"]);
    git(&root, &["add", "."]);
    git(
        &root,
        &[
            "-c",
            "user.name=Fixture",
            "-c",
            "user.email=fixture@example.invalid",
            "commit",
            "--quiet",
            "-m",
            "fixture",
        ],
    );
    let mut request = AssemblyRequest {
        storage: directory.path().join("store"),
        project_root: root.clone(),
        family: crate::search_index::artifact_cache_key(&root),
        scope: crate::path_identity::project_scope_key(&root),
        desired_head: head_tree_fingerprint(&crate::alias::head_tree_entries(&root).unwrap()),
        changed_paths: Default::default(),
        semantic_keys: Default::default(),
        require_semantic: false,
        allow_blob_put: true,
        callgraph: true,
    };
    std::fs::create_dir(&request.storage).unwrap();
    for count in [0, 1, 300, 0] {
        request.changed_paths.clear();
        for index in 0..count {
            let path = format!("file_{index:03}.ts");
            std::fs::write(
                root.join(&path),
                format!(
                    "export function value_{index}() {{ return {}; }}\n",
                    count + 1
                ),
            )
            .unwrap();
            request.changed_paths.insert(path.into_bytes());
        }
        let _reason = super::super::publication_reason::Reason {
            events: super::super::publication_reason::Reason::CONTENT,
            query: count == 1,
            periodic: count == 0,
            ..Default::default()
        }
        .enter();
        let mut prepared = prepare_checkout(&request, &mut |_| Ok(())).unwrap();
        assert!(prepared
            .profile
            .scheduling
            .contains(&format!("schedule_changed_paths={count} ")));
        assert!(prepared
            .profile
            .scheduling
            .contains("schedule_event_kinds=content_only_paths "));
        assert!(prepared.profile.scheduling.contains(if count == 1 {
            "schedule_query_freshness=true"
        } else {
            "schedule_query_freshness=false"
        }));
        let report = prepared.commit().unwrap();
        let snapshot = prepared.profile.store_writes.snapshot();
        eprintln!(
            "STORE_FIXTURE changed_paths={count} published={} {}",
            report.published,
            prepared.profile.store_writes.fields()
        );
        if report.published {
            assert!(
                snapshot.0[0].wal_bytes > 0,
                "blob WAL must belong to blob store: {snapshot:?}"
            );
            assert!(
                snapshot.0[0].main_bytes > 0,
                "blob checkpoint must be observed: {snapshot:?}"
            );
            assert!(
                snapshot.0[1].main_bytes > 0,
                "derived main must belong to derived store: {snapshot:?}"
            );
            assert!(snapshot.0[0].checkpoints > 0);
            assert!(snapshot.0[0].backfilled_pages > 0);
            if count == 0 {
                assert!(
                    snapshot.0[2].main_bytes + snapshot.0[2].wal_bytes > 0,
                    "initial alias writes: {snapshot:?}"
                );
            }
            let view = ViewStore::open(&request.storage, &request.scope).unwrap();
            let manifest = view.view_dir().join(format!(
                "manifest-{}.json",
                report.generation.as_deref().unwrap()
            ));
            assert_eq!(
                snapshot.0[3].main_bytes,
                std::fs::metadata(manifest).unwrap().len()
            );
            assert_eq!(snapshot.0[3].wal_bytes, 0);
        } else {
            assert_eq!(count, 0);
            assert_eq!(
                snapshot,
                super::super::write_accounting::Snapshot::default()
            );
        }
        drop(prepared);
        let view = ViewStore::open(&request.storage, &request.scope).unwrap();
        crate::views::wait_for_generation_maintenance_for_test(view.view_dir());
    }
}

#[test]
fn publication_counter_read_overhead() {
    let publication = super::super::write_accounting::Publication::default();
    let started = Instant::now();
    for _ in 0..100_000 {
        std::hint::black_box(publication.snapshot());
    }
    eprintln!(
        "STORE_OVERHEAD snapshot_us={:.3} samples=100000",
        started.elapsed().as_secs_f64() * 1e6 / 100_000.0
    );
    let started = Instant::now();
    for _ in 0..10_000 {
        std::hint::black_box(publication.fields());
    }
    eprintln!(
        "STORE_OVERHEAD fields_us={:.3} samples=10000 record_us={:.3} record_samples=100000",
        started.elapsed().as_secs_f64() * 1e6 / 10_000.0,
        super::super::write_accounting::counter_overhead_us()
    );
    let paths = (0..300)
        .map(|index| format!("src/file_{index:03}.ts").into_bytes())
        .collect();
    let reason = super::super::publication_reason::Reason::default();
    let started = Instant::now();
    for _ in 0..10_000 {
        std::hint::black_box(reason.fields(&paths));
    }
    eprintln!(
        "STORE_OVERHEAD reason_us={:.3} samples=10000",
        started.elapsed().as_secs_f64() * 1e6 / 10_000.0
    );
    let started = Instant::now();
    for _ in 0..100_000 {
        drop(publication.enter());
    }
    eprintln!(
        "STORE_OVERHEAD scope_us={:.3} samples=100000",
        started.elapsed().as_secs_f64() * 1e6 / 100_000.0
    );
}
