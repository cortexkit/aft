use std::path::{Path, PathBuf};

use super::*;
use crate::semantic_index::{EmbedTextCaps, SemanticIndex, SemanticIndexFingerprint};

const POLICY: TrigramPolicy = TrigramPolicy {
    max_file_size: 1 << 20,
};

fn write_tree(root: &Path, files: &[(&str, &[u8])]) {
    for (path, bytes) in files {
        let path = root.join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, bytes).unwrap();
    }
}

fn fixture() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("checkout");
    fs::create_dir_all(&root).unwrap();
    write_tree(
        &root,
        &[
            (
                "src/alpha.rs",
                b"pub fn alpha_total(values: &[u32]) -> u32 {\n    values.iter().sum()\n}\n",
            ),
            (
                "src/beta.rs",
                b"pub struct BetaCache {\n    entries: Vec<String>,\n}\n",
            ),
            (
                "notes/readme.md",
                b"# Notes\n\nSome prose about the alpha total.\n",
            ),
            ("assets/blob.bin", &[0, 159, 146, 150, 0, 1, 2, 3, 0, 0]),
        ],
    );
    let root = fs::canonicalize(&root).unwrap();
    (dir, root)
}

fn legacy_cache(root: &Path, cache_dir: &Path, max_file_size: u64) -> Vec<u8> {
    let mut index = crate::search_index::SearchIndex::build_with_limit(root, max_file_size);
    assert!(index.write_to_disk(cache_dir, None), "legacy cache written");
    fs::read(cache_dir.join("cache.bin")).unwrap()
}

/// The canonical conversion of a legacy cache equals what a cold build of
/// the same files produces: every payload, and the segment byte for byte.
#[test]
fn trigram_conversion_equals_a_cold_segment() {
    let (dir, root) = fixture();
    let bytes = legacy_cache(&root, &dir.path().join("cache"), POLICY.max_file_size);
    let bundle = convert_trigram(&bytes, &POLICY).unwrap();
    let members = bundle
        .entries
        .iter()
        .map(|entry| String::from_utf8(entry.rel_path.as_bytes().to_vec()).unwrap())
        .collect::<Vec<_>>();
    assert!(members.contains(&"src/alpha.rs".to_owned()), "{members:?}");
    assert!(
        members.contains(&"notes/readme.md".to_owned()),
        "{members:?}"
    );
    for entry in &bundle.entries {
        let source =
            fs::read(root.join(segment_store::rel_path_to_os(&entry.rel_path).unwrap())).unwrap();
        assert_eq!(entry.content, ContentHash::of(&source));
        assert_eq!(
            entry.payload,
            TrigramPayload::extract(&source, &POLICY).encode(),
            "payload of {:?}",
            entry.rel_path
        );
    }
    let imported = trigram_segment(&bundle, &POLICY).unwrap();
    let rel_paths = bundle
        .entries
        .iter()
        .map(|entry| entry.rel_path.clone())
        .collect::<Vec<_>>();
    let cold = segment_store::build_from_files(&root, &rel_paths, &POLICY).unwrap();
    assert_eq!(imported.id, cold.id);
    assert_eq!(imported.bytes, cold.bytes);
}

/// Anything not provably equal to a cold build is rejected, never relabelled.
#[test]
fn incompatible_trigram_caches_are_rejected() {
    let (dir, root) = fixture();
    let bytes = legacy_cache(&root, &dir.path().join("cache"), POLICY.max_file_size);
    let other_limit = TrigramPolicy {
        max_file_size: 4096,
    };
    assert!(convert_trigram(&bytes, &other_limit)
        .unwrap_err()
        .contains("max_file_size"));
    let mut version = bytes.clone();
    version[4] ^= 0xff;
    assert!(convert_trigram(&version, &POLICY)
        .unwrap_err()
        .contains("version"));
    let mut flipped = bytes.clone();
    let middle = 40 + (bytes.len() - 40) / 3;
    flipped[middle] ^= 0x01;
    assert!(convert_trigram(&flipped, &POLICY)
        .unwrap_err()
        .contains("checksum"));
    assert!(convert_trigram(&bytes[..bytes.len() - 7], &POLICY).is_err());
}

fn deterministic(text: &str) -> Vec<f32> {
    blake3::hash(text.as_bytes()).as_bytes()[..16]
        .iter()
        .map(|byte| (f32::from(*byte) - 127.5) / 127.5)
        .collect()
}

fn fingerprint() -> SemanticIndexFingerprint {
    SemanticIndexFingerprint {
        backend: "test".to_owned(),
        model: "deterministic".to_owned(),
        base_url: "none".to_owned(),
        dimension: 16,
        chunking_version: 2,
        embed_text_caps: EmbedTextCaps::default(),
        ..SemanticIndexFingerprint::default()
    }
}

fn producer() -> SemanticProducer {
    let fingerprint = fingerprint();
    SemanticProducer::current(fingerprint.as_string(), fingerprint.embed_text_caps)
}

fn legacy_semantic(root: &Path) -> Vec<u8> {
    let files = ["src/alpha.rs", "src/beta.rs", "notes/readme.md"]
        .iter()
        .map(|path| root.join(path))
        .collect::<Vec<_>>();
    let mut index = SemanticIndex::build(
        root,
        &files,
        &mut |texts: Vec<String>| Ok(texts.iter().map(|text| deterministic(text)).collect()),
        64,
    )
    .unwrap();
    index.set_fingerprint(fingerprint());
    index.to_bytes()
}

/// An imported semantic payload is byte-equal to the payload a cold fill
/// stores for the same file and producer; a file edited since the snapshot
/// is left for the plane to embed.
#[test]
fn semantic_conversion_equals_cold_fill_payloads() {
    let (_dir, root) = fixture();
    let snapshot = legacy_semantic(&root);
    fs::write(root.join("src/beta.rs"), b"pub struct BetaCache;\n").unwrap();
    let producer = producer();
    let (bundle, skipped) = convert_semantic(&snapshot, &root, &producer).unwrap();
    let converted = bundle
        .entries
        .iter()
        .map(|entry| String::from_utf8(entry.rel_path.as_bytes().to_vec()).unwrap())
        .collect::<Vec<_>>();
    // Only files the semantic plane chunks are imported; the plane never
    // embeds the others either.
    let expected = ["notes/readme.md", "src/alpha.rs"]
        .into_iter()
        .filter(|path| {
            crate::views::semantic::applies_to(&RelPath::new(path.as_bytes().to_vec()).unwrap())
        })
        .collect::<Vec<_>>();
    assert!(expected.contains(&"src/alpha.rs"));
    assert_eq!(converted, expected);
    assert!(skipped >= 1, "the edited file is not imported");
    let payload_producer = crate::semantic_index::ViewPayloadProducer {
        chunker_version: &producer.chunker_version,
        template_version: &producer.template_version,
        model_fingerprint: &producer.model_fingerprint,
    };
    for entry in &bundle.entries {
        let relative = segment_store::rel_path_to_os(&entry.rel_path).unwrap();
        let source = fs::read(root.join(&relative)).unwrap();
        let chunks =
            crate::semantic_index::chunk_view_file(&root, &relative, &source, producer.caps)
                .unwrap()
                .unwrap();
        let mut embed =
            |texts: Vec<String>| Ok(texts.iter().map(|text| deterministic(text)).collect());
        let runs = crate::semantic_index::embed_view_files(vec![chunks], &mut embed, 64).unwrap();
        assert_eq!(
            entry.payload,
            runs[0].encode_view_payload(&payload_producer),
            "payload of {relative:?}"
        );
        assert_eq!(
            entry.key,
            *producer.key(&source, &entry.rel_path).as_bytes()
        );
    }
}

#[test]
fn a_snapshot_of_another_model_is_rejected() {
    let (_dir, root) = fixture();
    let snapshot = legacy_semantic(&root);
    let mut other = fingerprint();
    other.model = "another".to_owned();
    let other = SemanticProducer::current(other.as_string(), other.embed_text_caps);
    assert!(convert_semantic(&snapshot, &root, &other).is_err());
}

#[test]
fn summary_budget_marker_is_the_only_ignored_fingerprint_field() {
    let configured = fingerprint().as_string();
    let mut stored: serde_json::Value = serde_json::from_str(&configured).unwrap();
    stored["file_summary_budget_limited"] = serde_json::json!(true);
    assert!(crate::migration::fingerprint_is_compatible(
        &stored.to_string(),
        &configured
    ));
    for (field, value) in [
        ("model", serde_json::json!("another-model")),
        ("dimension", serde_json::json!(999)),
        ("other_summary_rule", serde_json::json!(true)),
    ] {
        let mut other = stored.clone();
        other[field] = value;
        assert!(
            !crate::migration::fingerprint_is_compatible(&other.to_string(), &configured),
            "ignored {field}"
        );
    }
    stored["file_summary_budget_limited"] = serde_json::json!("not-a-boolean");
    assert!(!crate::migration::fingerprint_is_compatible(
        &stored.to_string(),
        &configured
    ));
}

#[test]
fn small_cap_legacy_import_preserves_rows_for_summary_only_repair() {
    for segmented in [false, true] {
        let (_project, storage, root, identity) =
            crate::semantic_index::saved_pre_budget_summary_fixture(true);
        let path = storage.path().join("semantic/summary-budget/semantic.bin");
        let producer = SemanticProducer::current(identity.as_string(), identity.embed_text_caps);
        let mut old = fs::read(&path).unwrap();
        if segmented {
            let index =
                SemanticIndex::read_from_disk(storage.path(), "summary-budget", &root, false, None)
                    .unwrap();
            index.extend_dirty_paths_for_test([root.join("summary.rs")]);
            let (frame, _) = index.segment_frames_for_test(&index, 1).unwrap();
            old.extend(frame);
            fs::write(&path, &old).unwrap();
        }
        let (bundle, skipped) = convert_semantic(&old, &root, &producer).unwrap();
        assert_eq!(
            bundle.entries.len(),
            1,
            "a changed summary must not discard reusable symbols"
        );
        assert_eq!(skipped, 0);
        let payload_producer = crate::semantic_index::ViewPayloadProducer {
            chunker_version: &producer.chunker_version,
            template_version: &producer.template_version,
            model_fingerprint: &producer.model_fingerprint,
        };
        let run = crate::semantic_index::SemanticVectors::decode_view_payload_for_summary_repair(
            &bundle.entries[0].payload,
            Path::new("summary.rs"),
            &payload_producer,
        )
        .unwrap();
        assert_eq!(run.chunk_count(), 2);
        let original = SemanticIndex::from_bytes(&old, &root)
            .unwrap()
            .entry_vectors_for_test()
            .into_iter()
            .find(|(_, name, _)| name == "exported")
            .unwrap()
            .2;
        let imported = run
            .embedded_texts_and_vectors()
            .find(|(text, _)| !text.contains("kind:file-summary"))
            .unwrap()
            .1;
        assert_eq!(imported.as_ref(), original.as_slice());
        let source = fs::read(root.join("summary.rs")).unwrap();
        let relative = RelPath::new(b"summary.rs".to_vec()).unwrap();
        let legacy = crate::blob_store::SemanticKey::from_bytes(
            &source,
            relative.as_bytes(),
            &producer.chunker_version,
            &producer.template_version,
            &producer.model_fingerprint,
        )
        .full_key();
        assert_eq!(bundle.entries[0].key, *legacy.as_bytes());
        assert_ne!(
            bundle.entries[0].key,
            *producer.key(&source, &relative).as_bytes()
        );

        let (fixed, changed) = SemanticIndex::read_from_disk_with_summary_repair(
            storage.path(),
            "summary-budget",
            &root,
            false,
            Some(&identity.as_string()),
            &mut |texts: Vec<String>| Ok(texts.iter().map(|_| vec![1.0; 4]).collect()),
            16,
        )
        .unwrap()
        .unwrap();
        assert!(changed);
        assert!(fixed.write_to_disk(storage.path(), "summary-budget"));
        let (fixed, skipped) =
            convert_semantic(&fs::read(path).unwrap(), &root, &producer).unwrap();
        assert_eq!(skipped, 0);
        assert_eq!(fixed.entries.len(), 1);
        assert_eq!(
            fixed.entries[0].key,
            *producer.key(&source, &relative).as_bytes()
        );
        let run = crate::semantic_index::SemanticVectors::decode_view_payload_for_summary_repair(
            &fixed.entries[0].payload,
            Path::new("summary.rs"),
            &payload_producer,
        )
        .unwrap();
        assert!(run
            .embedded_texts_and_vectors()
            .all(|(text, _)| text.chars().count() <= 70));
    }
}

#[test]
fn small_cap_legacy_import_rejects_other_summary_text_changes() {
    let (_project, storage, root, identity) =
        crate::semantic_index::saved_pre_budget_summary_fixture(true);
    let mut bytes = fs::read(storage.path().join("semantic/summary-budget/semantic.bin")).unwrap();
    let tag = b"kind:file-summary";
    let offset = bytes
        .windows(tag.len())
        .position(|window| window == tag)
        .unwrap();
    bytes[offset..offset + tag.len()].copy_from_slice(b"kind:fake-summary");
    let producer = SemanticProducer::current(identity.as_string(), identity.embed_text_caps);
    let (bundle, skipped) = convert_semantic(&bytes, &root, &producer).unwrap();
    assert!(bundle.entries.is_empty());
    assert_eq!(skipped, 1);
}

#[test]
fn semantic_import_keeps_the_first_vector_payload_and_imports_other_keys() {
    let (dir, root) = fixture();
    let storage = dir.path().join("storage");
    let producer = producer();
    let snapshot = legacy_semantic(&root);
    let (bundle, _) = convert_semantic(&snapshot, &root, &producer).unwrap();
    assert!(bundle.entries.len() >= 2);
    let registry = FamilyRegistry::open(&storage, "family").unwrap();
    let registration = registry
        .register_view_with_storage(registry.storage(), "scope", &root)
        .unwrap();
    let store = registration.open_store(FamilyPlane::Semantic).unwrap();
    let first = &bundle.entries[0];
    let relative = segment_store::rel_path_to_os(&first.rel_path).unwrap();
    let source = fs::read(root.join(&relative)).unwrap();
    let chunks = crate::semantic_index::chunk_view_file(&root, &relative, &source, producer.caps)
        .unwrap()
        .unwrap();
    // The same input and producer can yield different float bytes on another
    // backend invocation. It is still the same input-addressed family key.
    let runs = crate::semantic_index::embed_view_files(
        vec![chunks],
        &mut |texts: Vec<String>| {
            Ok(texts
                .iter()
                .map(|text| deterministic(text).into_iter().map(|v| v * 0.99).collect())
                .collect())
        },
        64,
    )
    .unwrap();
    let payload = runs[0].encode_view_payload(&crate::semantic_index::ViewPayloadProducer {
        chunker_version: &producer.chunker_version,
        template_version: &producer.template_version,
        model_fingerprint: &producer.model_fingerprint,
    });
    assert_ne!(payload, first.payload);
    let key = FamilyKey::new(FamilyPlane::Semantic, first.key);
    store.put_or_touch(&key, &payload).unwrap();
    let legacy = legacy_semantic_path(&storage, "legacy");
    fs::create_dir_all(legacy.parent().unwrap()).unwrap();
    fs::write(&legacy, &snapshot).unwrap();
    let report = run_import(
        &ImportRequest {
            durable_storage: storage.clone(),
            storage: storage.clone(),
            family: "family".into(),
            legacy_key: "legacy".into(),
            scope: "scope".into(),
            root: root.clone(),
            producers: Producers {
                trigram: "unregistered".into(),
                semantic: Some(producer.id()),
                callgraph: "unregistered".into(),
            },
            trigram_policy: None,
            semantic: Some(producer),
        },
        None,
    )
    .expect("a single vector conflict must not abort the import");
    assert_eq!(report.outcome, ImportOutcome::Completed);
    assert_eq!(report.semantic_files, bundle.entries.len());
    assert_eq!(
        store.get(&key).unwrap().unwrap(),
        payload,
        "the first payload is immutable"
    );
    for entry in &bundle.entries[1..] {
        assert_eq!(
            store
                .get(&FamilyKey::new(FamilyPlane::Semantic, entry.key))
                .unwrap()
                .unwrap(),
            entry.payload
        );
    }
    assert!(
        ImportLedger::open(&storage, "legacy")
            .unwrap()
            .status()
            .unwrap()
            .complete
    );
    assert_eq!(
        fs::read(legacy).unwrap(),
        snapshot,
        "the legacy cache stays intact"
    );
}

#[test]
fn semantic_import_rejects_only_the_unusable_key() {
    for quarantined in [false, true] {
        let (dir, root) = fixture();
        let storage = dir.path().join("storage");
        let producer = producer();
        let snapshot = legacy_semantic(&root);
        let (bundle, _) = convert_semantic(&snapshot, &root, &producer).unwrap();
        assert!(bundle.entries.len() >= 2);
        let registry = FamilyRegistry::open(&storage, "family").unwrap();
        let registration = registry
            .register_view_with_storage(registry.storage(), "scope", &root)
            .unwrap();
        let store = registration.open_store(FamilyPlane::Semantic).unwrap();
        let first = &bundle.entries[0];
        let key = FamilyKey::new(FamilyPlane::Semantic, first.key);
        if quarantined {
            store.quarantine(&key).unwrap();
        } else {
            store
                .put_or_touch(&key, b"invalid semantic payload")
                .unwrap();
        }
        let legacy = legacy_semantic_path(&storage, "legacy");
        fs::create_dir_all(legacy.parent().unwrap()).unwrap();
        fs::write(&legacy, snapshot).unwrap();
        let report = run_import(
            &ImportRequest {
                durable_storage: storage.clone(),
                storage: storage.clone(),
                family: "family".into(),
                legacy_key: "legacy".into(),
                scope: "scope".into(),
                root: root.clone(),
                producers: Producers {
                    trigram: "unregistered".into(),
                    semantic: Some(producer.id()),
                    callgraph: "unregistered".into(),
                },
                trigram_policy: None,
                semantic: Some(producer),
            },
            None,
        )
        .expect("one unusable semantic key must not abort the import");
        assert_eq!(report.semantic_files, bundle.entries.len() - 1);
        for entry in &bundle.entries[1..] {
            assert_eq!(
                store
                    .get(&FamilyKey::new(FamilyPlane::Semantic, entry.key))
                    .unwrap()
                    .unwrap(),
                entry.payload
            );
        }
        let view = registration.view_store().unwrap();
        let generation = view.current_generation().unwrap().unwrap();
        let manifest = view.load_manifest_v2(&generation).unwrap();
        let entry = manifest.get(&first.rel_path).unwrap();
        assert!(matches!(
            entry.plane_state(FamilyPlane::Semantic),
            Some(PlaneState::Pending { .. })
        ));
        assert!(
            ImportLedger::open(&storage, "legacy")
                .unwrap()
                .status()
                .unwrap()
                .complete
        );
    }
}

fn owner_of(pid: u32, start_time: u64, token: &str) -> ClaimOwner {
    ClaimOwner {
        pid,
        start_time,
        token: token.to_owned(),
    }
}

/// A live owner holds the row; an abandoned claim of this very process (its
/// import returned) is taken over with the next attempt.
#[test]
fn a_live_claim_is_single_flight_and_an_abandoned_one_is_taken_over() {
    let storage = tempfile::tempdir().unwrap();
    let ledger = ImportLedger::open(storage.path(), "key").unwrap();
    let first = ClaimOwner::new();
    let active = ActiveClaim::register(&first);
    assert!(matches!(
        ledger.claim(Artifact::Trigram, &first).unwrap(),
        Claim::Owned(ImportRow { attempt: 1, .. })
    ));
    let second = ClaimOwner::new();
    let _second_active = ActiveClaim::register(&second);
    assert!(matches!(
        ledger.claim(Artifact::Trigram, &second).unwrap(),
        Claim::Busy(holder) if holder == first
    ));
    drop(active);
    match ledger.claim(Artifact::Trigram, &second).unwrap() {
        Claim::Owned(row) => {
            assert_eq!(row.attempt, 2);
            assert_eq!(row.owner.as_ref(), Some(&second));
        }
        _ => panic!("an abandoned claim must be taken over"),
    }
    // The superseded owner can no longer move the row.
    assert!(matches!(
        ledger.advance(
            Artifact::Trigram,
            &first,
            ImportState::Claimed,
            ImportState::Staged,
            Advance::default()
        ),
        Err(ImportError::LostClaim(Artifact::Trigram))
    ));
}

#[test]
fn a_dead_process_claim_is_taken_over() {
    let storage = tempfile::tempdir().unwrap();
    let ledger = ImportLedger::open(storage.path(), "key").unwrap();
    // A process that has exited: liveness is checked through the OS for
    // this pid, as for a crashed importer. An impossible pid would not do:
    // some platforms cannot look it up and conservatively call it live.
    let mut exited = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "no-such-test", "--quiet"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let pid = exited.id();
    exited.wait().unwrap();
    // Release the handle, so nothing keeps the exited process's record open.
    drop(exited);
    let dead = owner_of(pid, 1, "dead");
    assert!(matches!(
        ledger.claim(Artifact::Semantic, &dead).unwrap(),
        Claim::Owned(_)
    ));
    let me = ClaimOwner::new();
    let _active = ActiveClaim::register(&me);
    assert!(matches!(
        ledger.claim(Artifact::Semantic, &me).unwrap(),
        Claim::Owned(ImportRow { attempt: 2, .. })
    ));
}

#[test]
fn a_ledger_from_a_newer_build_is_refused_by_name() {
    let storage = tempfile::tempdir().unwrap();
    let dir = ledger_dir(storage.path(), "newer");
    fs::create_dir_all(&dir).unwrap();
    let connection = rusqlite::Connection::open(dir.join(LEDGER_FILE)).unwrap();
    connection.execute_batch(SCHEMA).unwrap();
    connection
        .execute(
            "INSERT INTO ledger_meta (singleton, format_version) VALUES (1, ?1)",
            params![LEDGER_FORMAT_VERSION + 1],
        )
        .unwrap();
    drop(connection);
    assert!(matches!(
        ImportLedger::open(storage.path(), "newer"),
        Err(ImportError::NewerLedger(version)) if version == LEDGER_FORMAT_VERSION + 1
    ));
}

#[test]
fn a_staged_bundle_roundtrips_and_detects_corruption() {
    let bundle = Bundle {
        entries: vec![BundleEntry {
            rel_path: RelPath::new(b"a.rs".to_vec()).unwrap(),
            content: ContentHash::of(b"a"),
            size: 1,
            key: [7; 32],
            payload: vec![1, 2, 3],
        }],
    };
    let bytes = bundle.encode(Artifact::Trigram);
    assert_eq!(Bundle::decode(&bytes, Artifact::Trigram).unwrap(), bundle);
    assert!(Bundle::decode(&bytes, Artifact::Semantic).is_err());
    let mut corrupt = bytes.clone();
    corrupt[12] ^= 1;
    assert!(Bundle::decode(&corrupt, Artifact::Trigram).is_err());
}

fn snapshot_tree(root: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    let mut files = BTreeMap::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in fs::read_dir(&dir).unwrap() {
            let entry = entry.unwrap();
            let path = entry.path();
            if entry.file_type().unwrap().is_dir() {
                stack.push(path);
            } else {
                files.insert(
                    path.strip_prefix(root).unwrap().to_path_buf(),
                    fs::read(&path).unwrap(),
                );
            }
        }
    }
    files
}

/// The sweeps an older binary runs (the orphan index and artifact-owner
/// sweeps, and the views-on generation sweep of the old layout) never reach
/// the v2 stores or the import ledger. The import itself leaves the legacy
/// set byte-identical, so an offline rollback finds it.
#[test]
fn old_sweeps_do_not_reach_v2_and_the_import_keeps_the_legacy_set() {
    let (dir, root) = fixture();
    let storage = dir.path().join("storage");
    let key = "legacykey";
    legacy_cache(
        &root,
        &storage.join("index").join(key),
        POLICY.max_file_size,
    );
    let semantic_dir = storage.join("semantic").join(key);
    fs::create_dir_all(&semantic_dir).unwrap();
    fs::write(semantic_dir.join("semantic.bin"), legacy_semantic(&root)).unwrap();
    let legacy_before = [
        snapshot_tree(&storage.join("index")),
        snapshot_tree(&storage.join("semantic")),
    ];
    let producer = producer();
    let request = ImportRequest {
        durable_storage: storage.clone(),
        storage: storage.clone(),
        family: key.to_owned(),
        legacy_key: key.to_owned(),
        scope: "scope".to_owned(),
        root: root.clone(),
        producers: Producers {
            trigram: POLICY.fingerprint_hex(),
            semantic: Some(producer.id()),
            callgraph: "callgraph-test".to_owned(),
        },
        trigram_policy: Some(POLICY),
        semantic: Some(producer),
    };
    let report = run_import(&request, None).unwrap();
    assert_eq!(report.outcome, ImportOutcome::Completed);
    assert!(report.published.is_some());
    assert_eq!(
        [
            snapshot_tree(&storage.join("index")),
            snapshot_tree(&storage.join("semantic")),
        ],
        legacy_before,
        "the import never writes the legacy set"
    );

    // Complete the new V2 publication pass before auditing only the legacy
    // sweeps below; those old entry points must still never reach V2 storage.
    crate::views::wait_for_generation_maintenance_for_test(&storage.join("views/v2/scope"));
    let v2_before = [
        snapshot_tree(&storage.join("blobs").join("v2")),
        snapshot_tree(&storage.join("views").join("v2")),
        snapshot_tree(&storage.join("migration")),
    ];
    crate::search_index::sweep_orphaned_index_dirs(&crate::root_cache::StoragePaths::same(
        &storage,
    ));
    let _ = crate::artifact_owner::sweep_orphaned_owner_manifests(&storage);
    let old_view = crate::views::ViewStore::open(&storage, "scope").unwrap();
    old_view.sweep_generations().unwrap();
    assert_eq!(
        [
            snapshot_tree(&storage.join("blobs").join("v2")),
            snapshot_tree(&storage.join("views").join("v2")),
            snapshot_tree(&storage.join("migration")),
        ],
        v2_before
    );
}

fn write_legacy_header(storage: &Path, key: &str, fingerprint: &SemanticIndexFingerprint) {
    let mut index = SemanticIndex::build(
        storage,
        &[],
        &mut |texts: Vec<String>| Ok(texts.iter().map(|text| deterministic(text)).collect()),
        64,
    )
    .unwrap();
    index.set_fingerprint(fingerprint.clone());
    let mut bytes = index.to_bytes();
    // An empty build has no vectors to give it a dimension; stamp the one
    // the fingerprint names, as a real build of that model would.
    bytes[1..5].copy_from_slice(&(fingerprint.dimension as u32).to_le_bytes());
    let dir = storage.join("semantic").join(key);
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("semantic.bin"), bytes).unwrap();
}

/// The lane's producer is the loaded model's fingerprint. For every backend
/// except Synapse that is the configuration at the model's dimension, which
/// the snapshot's dimension reproduces; for the default local model too.
/// A Synapse snapshot's served-space identity comes from its own stored
/// fingerprint, so it matches the lane while that identity is unchanged.
#[test]
fn the_legacy_producer_matches_the_lane_for_local_and_synapse_backends() {
    let storage = tempfile::tempdir().unwrap();
    let config = crate::config::SemanticBackendConfig::default();
    let local = SemanticIndexFingerprint::for_config_dimension(&config, 384);
    write_legacy_header(storage.path(), "local", &local);
    let producer = legacy_semantic_producer(storage.path(), "local", &config).unwrap();
    assert_eq!(producer.model_fingerprint, local.as_string());
    assert_eq!(
        producer.id(),
        SemanticProducer::current(local.as_string(), local.embed_text_caps).id()
    );

    let mut synapse = local.clone();
    synapse.synapse_fingerprint = Some("served-space".to_owned());
    synapse.synapse_table_epoch = Some(7);
    write_legacy_header(storage.path(), "synapse", &synapse);
    let producer = legacy_semantic_producer(storage.path(), "synapse", &config).unwrap();
    assert_eq!(producer.model_fingerprint, synapse.as_string());

    let mut other_model = local.clone();
    other_model.model = "another-model".to_owned();
    write_legacy_header(storage.path(), "other", &other_model);
    let producer = legacy_semantic_producer(storage.path(), "other", &config).unwrap();
    assert_eq!(
        producer.model_fingerprint,
        local.as_string(),
        "a snapshot of another model yields the configured producer, which the import rejects"
    );
}
