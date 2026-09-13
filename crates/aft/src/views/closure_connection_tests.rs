use super::*;
use crate::db::lifecycle::{thread_counts, SqliteStore};
use crate::views::probe_publication_closure;

fn fixture() -> (tempfile::TempDir, SqliteClosure, Manifest) {
    let dir = tempfile::tempdir().unwrap();
    let closure = SqliteClosure {
        semantic: dir.path().join("semantic.sqlite"),
        callgraph: dir.path().join("callgraph.sqlite"),
        trigram: dir.path().join("trigram"),
        connections: Default::default(),
    };
    fs::write(&closure.trigram, []).unwrap();
    for path in [&closure.semantic, &closure.callgraph] {
        let conn = Connection::open(path).unwrap();
        conn.execute_batch("CREATE TABLE blob_payloads(full_key BLOB PRIMARY KEY, payload BLOB)")
            .unwrap();
        for key in 0..600u16 {
            conn.execute(
                "INSERT INTO blob_payloads VALUES(?1, X'')",
                [decode_hex(&format!("{key:064x}")).unwrap()],
            )
            .unwrap();
        }
    }
    let manifest = Manifest::new((0..600u16).map(|key| {
        (
            RelPath::new(format!("file{key}.ts").into_bytes()).unwrap(),
            ManifestEntry::Regular {
                mode: 0o100644,
                planes: RegularPlanes {
                    semantic: Some(format!("{key:064x}")),
                    callgraph: Some(format!("{key:064x}")),
                },
                resolution_input: false,
            },
        )
    }))
    .unwrap();
    (dir, closure, manifest)
}

#[test]
fn closure_probe_opens_at_most_one_connection_per_plane() {
    let (_dir, closure, manifest) = fixture();
    let opens = thread_counts::total_opens_on_this_thread(SqliteStore::BlobStore);
    let live = thread_counts::open_on_this_thread(SqliteStore::BlobStore);
    probe_publication_closure(&manifest, &ClosureRequirements::default(), &closure).unwrap();
    assert_eq!(
        thread_counts::total_opens_on_this_thread(SqliteStore::BlobStore) - opens,
        2
    );
    assert_eq!(
        thread_counts::open_on_this_thread(SqliteStore::BlobStore) - live,
        2
    );
    drop(closure);
    assert_eq!(
        thread_counts::open_on_this_thread(SqliteStore::BlobStore),
        live
    );
}

#[test]
fn closure_probe_rejects_absent_key_after_successful_probes() {
    let (_dir, closure, manifest) = fixture();
    probe_publication_closure(&manifest, &ClosureRequirements::default(), &closure).unwrap();
    let key = "ff".repeat(32);
    let missing = Manifest::new([(
        RelPath::new(b"missing.ts".to_vec()).unwrap(),
        ManifestEntry::Regular {
            mode: 0o100644,
            planes: RegularPlanes {
                semantic: Some(key.clone()),
                callgraph: None,
            },
            resolution_input: false,
        },
    )])
    .unwrap();
    assert!(
        matches!(probe_publication_closure(&missing, &ClosureRequirements::default(), &closure),
        Err(ViewError::MissingBlob { plane: ArtifactPlane::Semantic, key: actual }) if actual == key)
    );
    assert!(!closure
        .contains_blob(ArtifactPlane::Callgraph, &key)
        .unwrap());
}

#[test]
fn malformed_key_does_not_open_a_plane() {
    let (_dir, closure, _) = fixture();
    let opens = thread_counts::total_opens_on_this_thread(SqliteStore::BlobStore);
    assert!(!closure
        .contains_blob(ArtifactPlane::Semantic, "not-a-key")
        .unwrap());
    assert_eq!(
        thread_counts::total_opens_on_this_thread(SqliteStore::BlobStore),
        opens
    );
}

#[test]
#[ignore = "requires copied closure artifacts via AFT_CLOSURE_* environment variables"]
fn bench_closure_probe_strategies() {
    use std::time::Instant;
    let path = |name| PathBuf::from(std::env::var(name).expect(name));
    let manifest: Manifest =
        serde_json::from_slice(&fs::read(path("AFT_CLOSURE_MANIFEST")).unwrap()).unwrap();
    let closure = SqliteClosure {
        semantic: path("AFT_CLOSURE_SEMANTIC"),
        callgraph: path("AFT_CLOSURE_CALLGRAPH"),
        trigram: path("AFT_CLOSURE_TRIGRAM"),
        connections: Default::default(),
    };
    let keys = manifest.plane_keys().collect::<Vec<_>>();
    let started = Instant::now();
    for (plane, key) in &keys {
        let path = if *plane == ArtifactPlane::Semantic {
            &closure.semantic
        } else {
            &closure.callgraph
        };
        assert!(Connection::open(path)
            .unwrap()
            .query_row(
                "SELECT 1 FROM blob_payloads WHERE full_key=?1",
                [decode_hex(key).unwrap()],
                |_| Ok(())
            )
            .optional()
            .unwrap()
            .is_some());
    }
    let per_key_ms = started.elapsed().as_millis();
    let started = Instant::now();
    for &(plane, key) in &keys {
        assert!(closure.contains_blob(plane, key).unwrap());
    }
    let retained_ms = started.elapsed().as_millis();
    drop(closure.connections.replace(Default::default()));
    let started = Instant::now();
    probe_publication_closure(&manifest, &ClosureRequirements::default(), &closure).unwrap();
    eprintln!(
        "closure_strategy keys={} per_key_ms={per_key_ms} retained_ms={retained_ms} batch_ms={}",
        keys.len(),
        started.elapsed().as_millis()
    );
}
