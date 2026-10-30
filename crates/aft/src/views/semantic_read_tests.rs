#[test]
fn resident_chunk_shortcut_matches_store_and_full_rebuild() {
    let source = large_source();
    let edited = source.replace("input + 149\n", "input * 149\n");
    let mut payloads = Vec::new();
    for resident_shortcut in [true, false] {
        let storage = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        write_tree(root.path(), &[("src/large.rs", &source)]);
        let plane = new_plane(storage.path(), "model-a");
        let checkout = ready_checkout(storage.path(), root.path(), &plane);
        let arena = plane.arena("family");
        arena.set_chunk_shortcut(resident_shortcut);
        let snapshot = edit_snapshot(&checkout, &edited);
        let store = checkout.owner.open_store(FamilyPlane::Semantic).unwrap();
        let probe = crate::blob_store::v2::read_tests::ReadProbe::start(&store);
        let before = crate::views::io::work_snapshot();
        let report = checkout.fill(&snapshot, &Model::default(), "model-a");
        let after = crate::views::io::work_snapshot();
        let reads = probe.finish();
        let gets: usize = reads
            .statements
            .iter()
            .filter(|(sql, _)| sql.starts_with("SELECT payload, payload_digest, payload_schema"))
            .map(|(_, (calls, _))| calls)
            .sum();
        eprintln!("LARGE_EDIT shortcut={resident_shortcut} chunks={} resident_hits={} gets={gets} payload_bytes={} page_misses={}", report.chunks, report.chunk_resident_hits, after["semantic_store_payload_bytes"].as_u64().unwrap() - before["semantic_store_payload_bytes"].as_u64().unwrap(), reads.misses);
        assert_canonical_blob(&checkout, &edited);
        // Admission preflight and the file/lease rechecks are intentional reads:
        // file claims cannot span cold readmission, and chunk ownership must
        // recheck persistence after excluding another process's dispatch.
        assert_eq!(gets, if resident_shortcut { 8 } else { 519 });
        assert_eq!(report.source_reads, 1);
        assert_eq!(report.embedded_texts, 1);
        assert_eq!(
            report.chunk_resident_hits > 0,
            resident_shortcut,
            "{report:?}"
        );
        arena.set_chunk_shortcut(true);
        let key = plane.producer.key(edited.as_bytes(), &large_rel_path());
        payloads.push(
            checkout
                .owner
                .open_store(FamilyPlane::Semantic)
                .unwrap()
                .get(&key)
                .unwrap()
                .unwrap(),
        );
        let run = arena.get(&key).unwrap();
        // Chunk text includes the platform-native relative path, so use the same
        // path representation as a normal index build.
        let chunk = crate::semantic_index::chunk_view_file(
            root.path(),
            &Path::new("src").join("large.rs"),
            edited.as_bytes(),
            plane.producer.caps,
        )
        .unwrap()
        .unwrap()
        .remove(0);
        let chunk_key = plane.producer.chunk_key(&chunk.embed_text);
        assert!(arena.chunk_vector(&chunk_key).is_some());
        plane.unbind(&checkout.access);
        drop(run);
        arena.trim();
        assert!(
            arena.chunk_vector(&chunk_key).is_some(),
            "unbind evicted a run while the installed bundle still holds its reader"
        );
        drop(checkout);
        arena.trim();
        assert!(
            arena.chunk_vector(&chunk_key).is_none(),
            "the chunk index retained an evicted run"
        );
    }
    assert_eq!(
        payloads[0], payloads[1],
        "resident and store shortcuts differ"
    );
}

#[test]
fn legacy_file_vectors_never_seed_chunk_shortcuts() {
    let storage = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    let source = large_source();
    write_tree(root.path(), &[("src/large.rs", &source)]);
    let plane = new_plane(storage.path(), "model-a");
    let checkout = Checkout::open(storage.path(), "scope-a", root.path(), &plane);
    let store = checkout.owner.open_store(FamilyPlane::Semantic).unwrap();
    // Seed the same native-path chunk identities that a full build would persist.
    let chunks = crate::semantic_index::chunk_view_file(
        root.path(),
        &Path::new("src").join("large.rs"),
        source.as_bytes(),
        plane.producer.caps,
    )
    .unwrap()
    .unwrap();
    for chunk in &chunks {
        let payload = encode_chunk_vector(
            &plane.producer.chunk_identity(),
            &chunk.embed_text,
            &vector("model-a", &chunk.embed_text),
        )
        .unwrap();
        store
            .put_or_touch(&plane.producer.chunk_key(&chunk.embed_text), &payload)
            .unwrap();
    }
    let mut legacy = crate::semantic_index::embed_view_files(
        vec![chunks],
        &mut |texts| Model::default().embed("legacy-floats", texts),
        64,
    )
    .unwrap();
    let key = plane.producer.key(source.as_bytes(), &large_rel_path());
    store
        .put_or_touch(
            &key,
            &legacy
                .remove(0)
                .encode_view_payload(&plane.producer.payload()),
        )
        .unwrap();
    checkout.load();
    let edited = source.replace("input + 149\n", "input * 149\n");
    let snapshot = edit_snapshot(&checkout, &edited);
    let report = checkout.fill(&snapshot, &Model::default(), "model-a");
    assert_eq!(report.embedded_texts, 1);
    assert_canonical_blob(&checkout, &edited);
    let shifted = format!("\n{edited}");
    let snapshot = edit_snapshot(&checkout, &shifted);
    let report = checkout.fill(&snapshot, &Model::default(), "model-a");
    assert_eq!(report.embedded_texts, 0);
    assert!(
        report.chunk_resident_hits >= 300,
        "one-time validation did not warm canonical chunks: {report:?}"
    );
    assert_canonical_blob(&checkout, &shifted);
}

#[test]
fn incremental_reuse_requires_matching_path_and_content() {
    let storage = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    write_tree(root.path(), FILES);
    let plane = new_plane(storage.path(), "model-a");
    let checkout = ready_checkout(storage.path(), root.path(), &plane);
    let old = checkout.load();
    let resident = plane
        .resident(&checkout.access, old.generation().name())
        .unwrap();
    let state = ViewState::default();
    let unchanged = RelPath::new(b"src/beta.rs".to_vec()).unwrap();
    let DiskState::Present { content, .. } = old.disk_state(&unchanged) else {
        panic!("missing member")
    };
    assert!(matches!(
        SemanticPlane::vector_source(&state, &resident, &old, &unchanged, content, true),
        VectorSource::Base
    ));
    write_tree(
        root.path(),
        &[("src/alpha.rs", "pub fn alpha_total() -> u32 { 999 }\n")],
    );
    checkout.driver.register_write_intent();
    let changed = checkout.load();
    let path = RelPath::new(b"src/alpha.rs".to_vec()).unwrap();
    let DiskState::Present { content, .. } = changed.disk_state(&path) else {
        panic!("missing changed member")
    };
    assert!(
        matches!(
            SemanticPlane::vector_source(&state, &resident, &changed, &path, content, true),
            VectorSource::Pending
        ),
        "changed content reused an old generation's vector"
    );
    let report = checkout.fill(&changed, &Model::default(), "model-a");
    assert_eq!(
        report.source_reads, 1,
        "unchanged paths were filled again: {report:?}"
    );
    let next = checkout.load();
    let report = checkout.fill(&next, &Model::default(), "model-a");
    assert_eq!(
        (report.queued, report.source_reads, report.chunks),
        (0, 0, 0)
    );
}

// Kept out of the normal suite because this probe embeds every Rust source in
// the checkout with a deterministic 768-dimensional model, without a service.
#[test]
#[ignore = "repository-sized SQLite read probe; run explicitly with --ignored --nocapture"]
fn repository_semantic_fill_read_probe() {
    use crate::blob_store::v2::read_tests::{describe, ReadProbe};

    let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let revision = std::env::var("AFT_SEMANTIC_READ_PROBE_REV").unwrap_or_else(|_| "HEAD".into());
    let listing = std::process::Command::new("git")
        .current_dir(&source)
        .args([
            "ls-tree",
            "-r",
            "--name-only",
            "-z",
            &revision,
            "--",
            "crates/aft/src",
        ])
        .output()
        .unwrap();
    assert!(
        listing.status.success(),
        "{}",
        String::from_utf8_lossy(&listing.stderr)
    );
    let root = tempfile::tempdir().unwrap();
    let storage = tempfile::tempdir().unwrap();
    let mut files = 0;
    let mut edit_paths = Vec::new();
    for path in listing
        .stdout
        .split(|byte| *byte == 0)
        .filter(|path| !path.is_empty())
    {
        let path = std::str::from_utf8(path).unwrap();
        if !path.ends_with(".rs") {
            continue;
        }
        let destination = root.path().join(path);
        std::fs::create_dir_all(destination.parent().unwrap()).unwrap();
        let contents = std::process::Command::new("git")
            .current_dir(&source)
            .args(["show", &format!("{revision}:{path}")])
            .output()
            .unwrap();
        assert!(contents.status.success());
        if contents.stdout.len() > 4_000 && contents.stdout.len() < 20_000 {
            edit_paths.push(path.to_owned());
        }
        std::fs::write(destination, contents.stdout).unwrap();
        files += 1;
    }
    assert!(files > 500, "not a repository-sized input: {files}");
    let git = |args: &[&str]| {
        assert!(std::process::Command::new("git")
            .current_dir(root.path())
            .args(args)
            .status()
            .unwrap()
            .success());
    };
    git(&["init", "--quiet"]);
    git(&["add", "."]);
    git(&[
        "-c",
        "user.name=Probe",
        "-c",
        "user.email=probe@example.test",
        "commit",
        "--quiet",
        "-m",
        "probe",
    ]);
    let sibling = tempfile::tempdir().unwrap();
    git(&[
        "worktree",
        "add",
        "--quiet",
        "--detach",
        sibling.path().to_str().unwrap(),
    ]);
    let plane = new_plane(storage.path(), "read-probe-768");
    let first = Checkout::open(storage.path(), "first", root.path(), &plane);
    let snapshot = first.load();
    let store = first.owner.open_store(FamilyPlane::Semantic).unwrap();
    let fill = |checkout: &Checkout, snapshot: &Snapshot| {
        checkout
            .plane
            .fill(
                &checkout.owner,
                snapshot,
                FillBudget {
                    max_files: usize::MAX,
                    ..FillBudget::default()
                },
                &mut |texts| {
                    Ok(texts
                        .iter()
                        .map(|text| {
                            vector("read-probe-768", text)
                                .into_iter()
                                .cycle()
                                .take(768)
                                .collect()
                        })
                        .collect())
                },
                &|| snapshot.clone(),
            )
            .unwrap()
    };
    let probe = ReadProbe::start(&store);
    let first_report = fill(&first, &snapshot);
    eprintln!(
        "INPUT files={files}; FIRST {first_report:?}\nSQL {:?}",
        probe.finish()
    );
    assert!(first_report.errors.is_empty() && first_report.store_errors.is_empty());
    assert_eq!(first_report.installed, first_report.queued);
    first.load();
    if std::env::var_os("AFT_SEMANTIC_READ_PROBE_DISABLE_SHORTCUT").is_some() {
        plane.arena("family").set_chunk_shortcut(false);
    }
    describe(&store);
    let sample_path = Path::new(&edit_paths[0]);
    let sample_bytes = std::fs::read(root.path().join(sample_path)).unwrap();
    let sample_chunks = crate::semantic_index::chunk_view_file(
        root.path(),
        sample_path,
        &sample_bytes,
        plane.producer.caps,
    )
    .unwrap()
    .unwrap();
    for (index, chunk) in sample_chunks.iter().take(10).enumerate() {
        let key = plane.producer.chunk_key(&chunk.embed_text);
        let probe = ReadProbe::start(&store);
        let payload = store.get(&key).unwrap().unwrap();
        let counts = probe.finish();
        eprintln!(
            "COLD_GET sample={index} payload_bytes={} page_cache_misses={}",
            payload.len(),
            counts.misses
        );
    }
    let second = Checkout::open(storage.path(), "second", sibling.path(), &plane);
    let before_sibling = crate::views::io::work_snapshot();
    let probe = ReadProbe::start(&store);
    let second_snapshot = second.load();
    let second_report = fill(&second, &second_snapshot);
    eprintln!("SIBLING {second_report:?}\nSQL {:?}", probe.finish());
    let after_sibling = crate::views::io::work_snapshot();
    eprintln!(
        "SIBLING_COUNTERS gets={} payload_bytes={} fills={}",
        after_sibling["semantic_store_gets"].as_u64().unwrap()
            - before_sibling["semantic_store_gets"].as_u64().unwrap(),
        after_sibling["semantic_store_payload_bytes"]
            .as_u64()
            .unwrap()
            - before_sibling["semantic_store_payload_bytes"]
                .as_u64()
                .unwrap(),
        after_sibling["semantic_fills"].as_u64().unwrap()
            - before_sibling["semantic_fills"].as_u64().unwrap()
    );
    assert_eq!(second_report.embedded_texts, 0);
    assert_eq!(second_report.stored_hits, 0);
    assert_eq!(second_report.queued, 0);
    eprintln!(
        "SIBLING arena decodes={} reads={}",
        plane.arena("family").decode_count(),
        plane.arena("family").store_reads()
    );
    for (label, edits) in [("one-file", 1), ("ten-file", 10), ("no-change", 0)] {
        for index in 0..edits {
            let path = sibling.path().join(&edit_paths[index]);
            let mut text = std::fs::read_to_string(&path).unwrap();
            text.push_str(&format!(
                "\npub fn probe_{index}() -> u32 {{ {} }}\n",
                edits + index
            ));
            std::fs::write(path, text).unwrap();
        }
        let before = crate::views::io::work_snapshot();
        second.driver.register_write_intent();
        let snapshot = second.load();
        let report = fill(&second, &snapshot);
        let after = crate::views::io::work_snapshot();
        let delta = [
            "semantic_store_gets",
            "semantic_store_payload_bytes",
            "semantic_touch_rows_written",
            "semantic_touch_transactions",
            "semantic_files_filled",
        ]
        .map(|key| {
            (
                key,
                after[key].as_u64().unwrap() - before[key].as_u64().unwrap(),
            )
        });
        eprintln!(
            "REPUBLISH {label} queued={} filled={} chunks={} delta={delta:?}",
            report.queued, report.source_reads, report.chunks
        );
        assert!(report.errors.is_empty() && report.store_errors.is_empty());
        assert_eq!(report.source_reads, edits);
        if edits == 0 {
            assert_eq!(delta.map(|(_, value)| value), [0; 5]);
        }
    }
    plane.unbind(&first.access);
    plane.unbind(&second.access);
    drop(first);
    drop(second);
    drop(plane);
    let plane = new_plane(storage.path(), "read-probe-768");
    let reopened = Checkout::open(storage.path(), "reopened", sibling.path(), &plane);
    let probe = ReadProbe::start(&store);
    let snapshot = reopened.load();
    let report = fill(&reopened, &snapshot);
    eprintln!("REOPENED {report:?}\nSQL {:?}", probe.finish());
    assert_eq!(report.embedded_texts, 0);
    assert_eq!(report.queued, 0);
    assert_eq!(
        plane.arena("family").decode_count(),
        first_report.installed as u64
    );
}

/// Ends the session that owns `checkout` and opens the same checkout over the
/// same storage in a new plane, as a daemon restart does: every arena, run,
/// chunk-index entry and fill map is gone; only storage survives.
fn restart(checkout: Checkout, storage: &Path) -> Checkout {
    let root = checkout.root.clone();
    let arena = Arc::downgrade(&checkout.plane.arena("family"));
    checkout.plane.unbind(&checkout.access);
    drop(checkout);
    assert!(
        arena.upgrade().is_none(),
        "the previous session's arena outlived the restart"
    );
    let plane = new_plane(storage, "model-a");
    Checkout::open(storage, "scope-a", &root, &plane)
}

/// Full-record payload lookups (`FamilyStore::get`) a probe saw.
fn payload_gets(reads: &crate::blob_store::v2::read_tests::ReadCounts) -> usize {
    reads
        .statements
        .iter()
        .filter(|(sql, _)| sql.starts_with("SELECT payload, payload_digest, payload_schema"))
        .map(|(_, (calls, _))| calls)
        .sum()
}

fn large_payload_and_marker(checkout: &Checkout, source: &str) -> (Vec<u8>, FamilyKey) {
    use crate::views::semantic_arena::ChunkNaming;
    let producer = checkout.plane.semantic_producer();
    let store = checkout.owner.open_store(FamilyPlane::Semantic).unwrap();
    let payload = store
        .get(&producer.key(source.as_bytes(), &large_rel_path()))
        .unwrap()
        .unwrap();
    let epoch = store.deletion_epoch().unwrap().unwrap();
    let marker = producer.verified_run_marker(&blake3::hash(&payload), &epoch);
    (payload, marker)
}

fn chunk_test_key(index: usize) -> FamilyKey {
    FamilyKey::new(
        FamilyPlane::Semantic,
        *blake3::hash(&(index as u64).to_le_bytes()).as_bytes(),
    )
}

#[test]
fn restart_requeue_of_unchanged_chunks_reads_no_chunk_rows() {
    let storage = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    let source = large_source();
    write_tree(root.path(), &[("src/large.rs", &source)]);
    let plane = new_plane(storage.path(), "model-a");
    let checkout = ready_checkout(storage.path(), root.path(), &plane);
    drop(plane);
    let checkout = restart(checkout, storage.path());
    // A leading blank line changes the file's content key but none of its
    // chunk texts, so the file is filled again from unchanged chunks only.
    let shifted = format!("\n{source}");
    let snapshot = edit_snapshot(&checkout, &shifted);
    let arena = checkout.plane.arena("family");
    assert_eq!(arena.seeded_chunks(), LARGE_DEFINITIONS as u64);
    let store = checkout.owner.open_store(FamilyPlane::Semantic).unwrap();
    let probe = crate::blob_store::v2::read_tests::ReadProbe::start(&store);
    let report = checkout.fill(&snapshot, &Model::default(), "model-a");
    let gets = payload_gets(&probe.finish());
    assert_eq!(
        arena.chunk_row_reads(),
        (0, 0),
        "unchanged chunks were read from the store after a restart: {report:?}"
    );
    assert_eq!(report.embedded_texts, 0, "{report:?}");
    assert_eq!(report.chunk_reused, LARGE_DEFINITIONS, "{report:?}");
    // Only the new file key: missed once, then rechecked under its claim.
    assert_eq!(gets, 2, "{report:?}");
    assert_canonical_blob(&checkout, &shifted);
}

#[test]
fn restart_edit_still_looks_up_and_embeds_the_changed_chunk() {
    let storage = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    let source = large_source();
    write_tree(root.path(), &[("src/large.rs", &source)]);
    let plane = new_plane(storage.path(), "model-a");
    let checkout = ready_checkout(storage.path(), root.path(), &plane);
    drop(plane);
    let checkout = restart(checkout, storage.path());
    let edited = source.replace("input + 149\n", "input * 149\n");
    let snapshot = edit_snapshot(&checkout, &edited);
    let report = checkout.fill(&snapshot, &Model::default(), "model-a");
    // The changed chunk misses the chunk index and the store, so it is the
    // one text the model embeds; the rest are reused.
    assert_eq!(report.embedded_texts, 1, "{report:?}");
    assert_eq!(report.chunk_reused, LARGE_DEFINITIONS - 1, "{report:?}");
    assert_canonical_blob(&checkout, &edited);
}

#[test]
fn a_marker_vouches_only_for_the_payload_it_names() {
    let storage = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    let source = large_source();
    write_tree(root.path(), &[("src/large.rs", &source), FILES[0]]);
    let plane = new_plane(storage.path(), "model-a");
    let checkout = Checkout::open(storage.path(), "scope-a", root.path(), &plane);
    let store = checkout.owner.open_store(FamilyPlane::Semantic).unwrap();
    // Canonical chunk rows, and a legacy file run for the same file whose
    // floats differ from them, as an import from an older index leaves.
    let chunks = crate::semantic_index::chunk_view_file(
        root.path(),
        &Path::new("src").join("large.rs"),
        source.as_bytes(),
        plane.producer.caps,
    )
    .unwrap()
    .unwrap();
    for chunk in &chunks {
        let payload = encode_chunk_vector(
            &plane.producer.chunk_identity(),
            &chunk.embed_text,
            &vector("model-a", &chunk.embed_text),
        )
        .unwrap();
        store
            .put_or_touch(&plane.producer.chunk_key(&chunk.embed_text), &payload)
            .unwrap();
    }
    let mut legacy = crate::semantic_index::embed_view_files(
        vec![chunks],
        &mut |texts| Model::default().embed("legacy-floats", texts),
        64,
    )
    .unwrap();
    store
        .put_or_touch(
            &plane.producer.key(source.as_bytes(), &large_rel_path()),
            &legacy
                .remove(0)
                .encode_view_payload(&plane.producer.payload()),
        )
        .unwrap();
    // A fill of another file stores a marker for that file's run only.
    let report = checkout.fill(&checkout.load(), &Model::default(), "model-a");
    assert_eq!(report.embedded_keys, 1, "{report:?}");
    checkout.load();
    drop(store);
    drop(plane);
    let checkout = restart(checkout, storage.path());
    let shifted = format!("\n{source}");
    let snapshot = edit_snapshot(&checkout, &shifted);
    let report = checkout.fill(&snapshot, &Model::default(), "model-a");
    assert_eq!(report.embedded_texts, 0, "{report:?}");
    assert_canonical_blob(&checkout, &shifted);
}

#[test]
fn restart_without_a_marker_falls_back_to_reading_chunk_rows() {
    let storage = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    let source = large_source();
    write_tree(root.path(), &[("src/large.rs", &source)]);
    let plane = new_plane(storage.path(), "model-a");
    let checkout = ready_checkout(storage.path(), root.path(), &plane);
    drop(plane);
    let (_, marker) = large_payload_and_marker(&checkout, &source);
    let store = checkout.owner.open_store(FamilyPlane::Semantic).unwrap();
    assert!(store.contains(&marker).unwrap(), "the fill vouched for nothing");
    // Markers are disposable: GC or a disk limit may delete one at any time.
    assert_eq!(
        rusqlite::Connection::open(store.path())
            .unwrap()
            .execute(
                "DELETE FROM blob_payloads WHERE full_key = ?1",
                [marker.as_bytes().as_slice()],
            )
            .unwrap(),
        1
    );
    let checkout = restart(checkout, storage.path());
    let shifted = format!("\n{source}");
    let snapshot = edit_snapshot(&checkout, &shifted);
    let arena = checkout.plane.arena("family");
    assert_eq!(arena.seeded_chunks(), 0);
    let report = checkout.fill(&snapshot, &Model::default(), "model-a");
    assert_eq!(arena.chunk_row_reads().0, LARGE_DEFINITIONS as u64);
    assert_eq!(report.embedded_texts, 0, "{report:?}");
    assert!(report.errors.is_empty() && report.store_errors.is_empty());
    assert_canonical_blob(&checkout, &shifted);
}

#[test]
fn runs_holding_vectors_without_chunk_rows_get_no_marker() {
    let storage = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    let source = large_source();
    write_tree(root.path(), &[("src/large.rs", &source)]);
    let plane = new_plane(storage.path(), "model-a");
    let checkout = Checkout::open(storage.path(), "scope-a", root.path(), &plane);
    let store = checkout.owner.open_store(FamilyPlane::Semantic).unwrap();
    let chunks = crate::semantic_index::chunk_view_file(
        root.path(),
        &Path::new("src").join("large.rs"),
        source.as_bytes(),
        plane.producer.caps,
    )
    .unwrap()
    .unwrap();
    // The store refuses one chunk row, so that chunk's vector exists only in
    // the file run and nothing checked it against a row.
    store
        .quarantine(&plane.producer.chunk_key(&chunks[7].embed_text))
        .unwrap();
    let report = checkout.fill(&checkout.load(), &Model::default(), "model-a");
    assert_eq!(report.embedded_keys, 1, "{report:?}");
    let (_, marker) = large_payload_and_marker(&checkout, &source);
    assert!(
        !store.contains(&marker).unwrap(),
        "a run with an unchecked vector was vouched for"
    );
}

#[test]
fn restart_seeding_stops_at_the_chunk_key_cap_without_evicting() {
    use crate::views::semantic_arena::MAX_CHUNK_KEYS;
    let storage = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    let source = large_source();
    write_tree(root.path(), &[("src/large.rs", &source)]);
    let plane = new_plane(storage.path(), "model-a");
    let checkout = ready_checkout(storage.path(), root.path(), &plane);
    drop(plane);
    let checkout = restart(checkout, storage.path());
    let arena = checkout.plane.arena("family");
    let free = 10;
    let held = (0..MAX_CHUNK_KEYS - free)
        .map(|index| arena.remember_chunk(chunk_test_key(index), vec![index as f32], None))
        .collect::<Vec<_>>();
    checkout.load();
    assert_eq!(arena.seeded_chunks(), free as u64);
    assert_eq!(arena.chunk_index_len(), MAX_CHUNK_KEYS);
    for (index, vector) in held.iter().enumerate() {
        assert!(
            arena
                .chunk_vector(&chunk_test_key(index))
                .is_some_and(|resident| Arc::ptr_eq(&resident, vector)),
            "seeding evicted or replaced chunk-index entry {index}"
        );
    }
    let shifted = format!("\n{source}");
    let snapshot = edit_snapshot(&checkout, &shifted);
    let report = checkout.fill(&snapshot, &Model::default(), "model-a");
    assert_eq!(report.embedded_texts, 0, "{report:?}");
    assert!(arena.chunk_index_len() <= MAX_CHUNK_KEYS);
    assert!(arena.chunk_row_reads().0 >= (LARGE_DEFINITIONS - free) as u64);
    assert_canonical_blob(&checkout, &shifted);
    drop(held);
}

/// A store holding markers must stay usable by v0.61.0, which predates them.
/// The statements are copied verbatim from v0.61.0's `blob_store/v2.rs`.
#[test]
fn verified_run_markers_work_with_v0_61_store_sql() {
    let storage = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    let source = large_source();
    write_tree(root.path(), &[("src/large.rs", &source)]);
    let plane = new_plane(storage.path(), "model-a");
    let checkout = ready_checkout(storage.path(), root.path(), &plane);
    let (run_payload, marker) = large_payload_and_marker(&checkout, &source);
    let store = checkout.owner.open_store(FamilyPlane::Semantic).unwrap();
    let connection = rusqlite::Connection::open(store.path()).unwrap();
    let key = marker.as_bytes().as_slice();
    let schema = |key: &[u8]| -> i64 {
        connection
            .query_row(
                "SELECT payload_schema FROM blob_payloads WHERE full_key = ?1",
                [key],
                |row| row.get(0),
            )
            .unwrap()
    };
    let run_key = plane.producer.key(source.as_bytes(), &large_rel_path());

    // get: the row passes v0.61.0's digest and schema checks, so an old
    // binary reading it would not report corruption...
    let (payload, digest, payload_schema): (Vec<u8>, Vec<u8>, i64) = connection
        .query_row(
            "SELECT payload, payload_digest, payload_schema FROM blob_payloads WHERE full_key = ?1",
            [key],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();
    assert_eq!(digest.as_slice(), blake3::hash(&payload).as_bytes());
    assert_eq!(payload_schema, schema(run_key.as_bytes().as_slice()));
    assert_eq!(
        payload,
        verified_run_marker_payload(
            &blake3::hash(&run_payload),
            &store.deletion_epoch().unwrap().unwrap()
        )
    );
    // ...and no decoder accepts it as vectors.
    assert!(crate::semantic_index::SemanticVectors::decode_view_payload(
        &payload,
        Path::new("src/large.rs"),
        &plane.producer.payload(),
    )
    .is_err());
    assert!(decode_chunk_vector(&payload, &plane.producer.chunk_identity(), "").is_none());

    // contains, put_or_touch and touch.
    let probes = [
        "SELECT 1 FROM blob_payloads INDEXED BY blob_membership WHERE full_key = ?1",
        "SELECT payload_digest, ref_epoch FROM blob_payloads WHERE full_key = ?1",
        "SELECT ref_epoch FROM blob_payloads INDEXED BY blob_membership WHERE full_key = ?1",
    ];
    for sql in probes {
        let mut statement = connection.prepare(sql).unwrap();
        assert!(statement.exists([key]).unwrap(), "{sql}");
    }
    let mut quarantined = connection
        .prepare("SELECT 1 FROM blob_quarantine WHERE full_key = ?1")
        .unwrap();
    assert!(!quarantined.exists([key]).unwrap());
    assert_eq!(
        connection
            .execute(
                "UPDATE blob_payloads SET ref_epoch = MAX(ref_epoch, ?2) WHERE full_key = ?1",
                rusqlite::params![key, 7i64],
            )
            .unwrap(),
        1
    );
    let inserted = FamilyKey::new(FamilyPlane::Semantic, [9; 32]);
    connection
        .execute(
            "INSERT INTO blob_payloads
                     (full_key, payload, payload_digest, payload_schema, ref_epoch, created_at_ms)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            rusqlite::params![
                inserted.as_bytes().as_slice(),
                b"other".as_slice(),
                blake3::hash(b"other").as_bytes().as_slice(),
                payload_schema,
                0i64,
                0i64,
            ],
        )
        .unwrap();

    // The disk-limit listing and usage totals count the marker as a row.
    let listed = connection
        .prepare(
            "SELECT full_key, length(payload), ref_epoch FROM blob_payloads
             ORDER BY ref_epoch ASC, full_key ASC LIMIT ?1",
        )
        .unwrap()
        .query_map([i64::MAX], |row| {
            Ok((row.get::<_, Vec<u8>>(0)?, row.get::<_, u64>(1)?))
        })
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert!(listed.contains(&(key.to_vec(), payload.len() as u64)));
    let (rows, _bytes): (u64, u64) = connection
        .query_row(
            "SELECT COUNT(*), COALESCE(SUM(length(payload)), 0) FROM blob_payloads",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(rows as usize, listed.len());

    // GC's conditional delete removes it like any other row, and the
    // deletion trigger counts that v0.61.0 delete.
    let before = store.deletion_epoch().unwrap().unwrap();
    assert_eq!(
        connection
            .execute(
                "DELETE FROM blob_payloads WHERE full_key = ?1 AND ref_epoch < ?2",
                rusqlite::params![key, 8i64],
            )
            .unwrap(),
        1
    );
    let after = store.deletion_epoch().unwrap().unwrap();
    assert_ne!(before, after, "a v0.61.0 delete did not move the deletion epoch");
    assert_eq!(after.to_bytes()[..16], before.to_bytes()[..16]);
    assert_eq!(
        u64::from_le_bytes(after.to_bytes()[16..].try_into().unwrap()),
        u64::from_le_bytes(before.to_bytes()[16..].try_into().unwrap()) + 1
    );
    let integrity: String = connection
        .query_row("PRAGMA integrity_check", [], |row| row.get(0))
        .unwrap();
    assert_eq!(integrity, "ok");
    assert!(!store.contains(&marker).unwrap());
    assert!(store.get(&run_key).unwrap().is_some());
}

/// Replaces the chunk row of `text` with different floats the way a GC
/// delete followed by another process's put would.
fn replace_chunk_row(store: &FamilyStore, producer: &SemanticProducer, text: &str) -> Vec<f32> {
    let key = producer.chunk_key(text);
    let epoch = store.raise_epoch(store.gc_epoch().unwrap() + 1).unwrap();
    assert!(store.delete_if_unreferenced_since(&key, epoch + 1).unwrap());
    let floats = vector("another-process", text);
    store
        .put_or_touch(
            &key,
            &encode_chunk_vector(&producer.chunk_identity(), text, &floats).unwrap(),
        )
        .unwrap();
    floats
}

#[test]
fn restart_after_a_chunk_row_is_replaced_seeds_nothing_and_reads_the_new_row() {
    let storage = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    let source = large_source();
    write_tree(root.path(), &[("src/large.rs", &source)]);
    let plane = new_plane(storage.path(), "model-a");
    let checkout = ready_checkout(storage.path(), root.path(), &plane);
    let (_, marker) = large_payload_and_marker(&checkout, &source);
    let store = checkout.owner.open_store(FamilyPlane::Semantic).unwrap();
    assert!(store.contains(&marker).unwrap(), "the fill vouched for nothing");
    let chunks = crate::semantic_index::chunk_view_file(
        root.path(),
        &Path::new("src").join("large.rs"),
        source.as_bytes(),
        plane.producer.caps,
    )
    .unwrap()
    .unwrap();
    let replaced = &chunks[7].embed_text;
    let floats = replace_chunk_row(&store, &plane.producer, replaced);
    // The old marker row survives; only the epoch says it no longer holds.
    assert!(store.contains(&marker).unwrap());
    drop(store);
    drop(plane);
    let checkout = restart(checkout, storage.path());
    let shifted = format!("\n{source}");
    let snapshot = edit_snapshot(&checkout, &shifted);
    let arena = checkout.plane.arena("family");
    assert_eq!(arena.seeded_chunks(), 0, "a marker from before the delete seeded");
    let report = checkout.fill(&snapshot, &Model::default(), "model-a");
    assert_eq!(report.embedded_texts, 0, "{report:?}");
    assert_eq!(arena.chunk_row_reads().0, LARGE_DEFINITIONS as u64);
    // The new file run carries the replacement row's floats, as every other
    // reader of that chunk row gets them.
    let producer = checkout.plane.semantic_producer();
    let stored = checkout
        .owner
        .open_store(FamilyPlane::Semantic)
        .unwrap()
        .get(&producer.key(shifted.as_bytes(), &large_rel_path()))
        .unwrap()
        .unwrap();
    let (run, digests) = crate::semantic_index::SemanticVectors::decode_view_payload_with_text_digests(
        &stored,
        &Path::new("src").join("large.rs"),
        &producer.payload(),
    )
    .unwrap();
    let position = digests
        .iter()
        .position(|digest| *digest == blake3::hash(replaced.as_bytes()))
        .unwrap();
    let vector = run.vectors().nth(position).unwrap();
    assert_eq!(
        vector.iter().map(|value| value.to_bits()).collect::<Vec<_>>(),
        floats.iter().map(|value| value.to_bits()).collect::<Vec<_>>(),
        "the fill reused floats from before the chunk row was replaced"
    );
}

#[test]
fn a_fill_never_vouches_with_chunk_checks_from_before_a_delete() {
    use crate::views::semantic_arena::ChunkNaming;
    let storage = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    let source = large_source();
    write_tree(root.path(), &[("src/large.rs", &source)]);
    let plane = new_plane(storage.path(), "model-a");
    let checkout = ready_checkout(storage.path(), root.path(), &plane);
    let store = checkout.owner.open_store(FamilyPlane::Semantic).unwrap();
    let chunks = crate::semantic_index::chunk_view_file(
        root.path(),
        &Path::new("src").join("large.rs"),
        source.as_bytes(),
        plane.producer.caps,
    )
    .unwrap()
    .unwrap();
    // This process still indexes the old floats of the replaced row, checked
    // before the delete; an edit in the same process reuses them.
    replace_chunk_row(&store, &plane.producer, &chunks[7].embed_text);
    let edited = source.replace("input + 149\n", "input * 149\n");
    let snapshot = edit_snapshot(&checkout, &edited);
    let report = checkout.fill(&snapshot, &Model::default(), "model-a");
    assert_eq!(report.embedded_keys, 1, "{report:?}");
    let payload = store
        .get(&plane.producer.key(edited.as_bytes(), &large_rel_path()))
        .unwrap()
        .unwrap();
    let marker = plane.producer.verified_run_marker(
        &blake3::hash(&payload),
        &store.deletion_epoch().unwrap().unwrap(),
    );
    assert!(
        !store.contains(&marker).unwrap(),
        "a run checked against chunk rows before a delete was vouched for"
    );
}

#[test]
fn restart_with_a_damaged_deletion_epoch_seeds_nothing_and_still_fills() {
    let storage = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    let source = large_source();
    write_tree(root.path(), &[("src/large.rs", &source)]);
    let plane = new_plane(storage.path(), "model-a");
    let checkout = ready_checkout(storage.path(), root.path(), &plane);
    let store = checkout.owner.open_store(FamilyPlane::Semantic).unwrap();
    rusqlite::Connection::open(store.path())
        .unwrap()
        .execute_batch("DROP TRIGGER payload_deletions_count")
        .unwrap();
    assert_eq!(store.deletion_epoch().unwrap(), None);
    drop(plane);
    // Keeping the store open across the restart keeps this process from
    // setting the counter up again, so the restarted fill sees the damage.
    let checkout = restart(checkout, storage.path());
    let shifted = format!("\n{source}");
    let snapshot = edit_snapshot(&checkout, &shifted);
    let arena = checkout.plane.arena("family");
    assert_eq!(arena.seeded_chunks(), 0);
    let report = checkout.fill(&snapshot, &Model::default(), "model-a");
    assert_eq!(report.embedded_texts, 0, "{report:?}");
    assert!(report.errors.is_empty() && report.store_errors.is_empty());
    assert_canonical_blob(&checkout, &shifted);
    // No marker can be written without an epoch either.
    assert_eq!(
        rusqlite::Connection::open(store.path())
            .unwrap()
            .query_row(
                "SELECT COUNT(*) FROM blob_payloads WHERE substr(payload, 1, 29) = ?1",
                [VERIFIED_RUN_DOMAIN],
                |row| row.get::<_, i64>(0),
            )
            .unwrap(),
        1,
        "only the marker from before the damage may exist"
    );
}

// Kept out of the normal suite: it fills a few thousand files. Run with
// `--ignored --nocapture --test-threads=1` so the process-wide work counters
// see only this test.
#[test]
#[ignore = "restart read probe over a few thousand files; run explicitly"]
fn restart_edit_read_probe() {
    const FILES: usize = 3_000;
    for seeding in [false, true] {
        let storage = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let source = large_source();
        let mut files = vec![("src/large.rs".to_owned(), source.clone())];
        for index in 0..FILES {
            files.push((
                format!("src/module_{index}.rs"),
                (0..4)
                    .map(|item| {
                        format!(
                            "pub fn module_{index}_item_{item}(input: u64) -> u64 {{\n    input * {item} + {index}\n}}\n\n"
                        )
                    })
                    .collect(),
            ));
        }
        let borrowed = files
            .iter()
            .map(|(path, text)| (path.as_str(), text.as_str()))
            .collect::<Vec<_>>();
        write_tree(root.path(), &borrowed);
        let plane = new_plane(storage.path(), "model-a");
        let checkout = Checkout::open(storage.path(), "scope-a", root.path(), &plane);
        let snapshot = checkout.load();
        let report = plane
            .fill(
                &checkout.owner,
                &snapshot,
                FillBudget {
                    max_files: usize::MAX,
                    ..FillBudget::default()
                },
                &mut |texts| Model::default().embed("model-a", texts),
                &|| snapshot.clone(),
            )
            .unwrap();
        assert_eq!(report.installed, FILES + 1, "{report:?}");
        checkout.load();
        drop(plane);
        let checkout = restart(checkout, storage.path());
        let arena = checkout.plane.arena("family");
        arena.set_run_seeding(seeding);
        let store = checkout.owner.open_store(FamilyPlane::Semantic).unwrap();
        let counters = |snapshot: &serde_json::Value| {
            [
                "semantic_store_gets",
                "semantic_store_payload_bytes",
            ]
            .map(|key| snapshot[key].as_u64().unwrap())
        };
        let before = counters(&crate::views::io::work_snapshot());
        let probe = crate::blob_store::v2::read_tests::ReadProbe::start(&store);
        checkout.load();
        let admission = probe.finish();
        let admitted = counters(&crate::views::io::work_snapshot());
        let edited = source.replace("input + 149\n", "input * 149\n");
        let snapshot = edit_snapshot(&checkout, &edited);
        let edit_start = counters(&crate::views::io::work_snapshot());
        let probe = crate::blob_store::v2::read_tests::ReadProbe::start(&store);
        let report = checkout.fill(&snapshot, &Model::default(), "model-a");
        let edit = probe.finish();
        let after = counters(&crate::views::io::work_snapshot());
        let marker_probes = admission
            .statements
            .iter()
            .filter(|(sql, _)| sql.starts_with("SELECT 1 FROM blob_payloads INDEXED BY"))
            .map(|(_, (calls, _))| calls)
            .sum::<usize>();
        eprintln!(
            "RESTART_PROBE seeding={seeding} files={} admission_gets={} admission_payload_bytes={} admission_index_probes={marker_probes} admission_page_misses={} seeded_chunks={} edit_gets={} edit_payload_bytes={} edit_page_misses={} chunk_row_reads={:?} embedded_texts={} chunk_reused={}",
            FILES + 1,
            admitted[0] - before[0],
            admitted[1] - before[1],
            admission.misses,
            arena.seeded_chunks(),
            after[0] - edit_start[0],
            after[1] - edit_start[1],
            edit.misses,
            arena.chunk_row_reads(),
            report.embedded_texts,
            report.chunk_reused,
        );
        assert_eq!(report.embedded_texts, 1);
        assert_eq!(payload_gets(&edit), if seeding { 4 } else { 4 + LARGE_DEFINITIONS - 1 });
        assert_canonical_blob(&checkout, &edited);
    }
}
