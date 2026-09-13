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
        for key in 0..32u8 {
            conn.execute("INSERT INTO blob_payloads VALUES(?1, X'')", [vec![key; 32]])
                .unwrap();
        }
    }
    let manifest = Manifest::new((0..32u8).map(|key| {
        (
            RelPath::new(format!("file{key}.ts").into_bytes()).unwrap(),
            ManifestEntry::Regular {
                mode: 0o100644,
                planes: RegularPlanes {
                    semantic: Some(format!("{key:02x}").repeat(32)),
                    callgraph: Some(format!("{key:02x}").repeat(32)),
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
