use std::path::Path;

use aft::blob_store::{
    BlobPlane, BlobStore, BlobStoreError, CallgraphKey, PutOutcome, SemanticKey,
    DEFAULT_WAL_AUTOCHECKPOINT_PAGES,
};
use rusqlite::params;

fn semantic_store(storage: &Path) -> BlobStore {
    BlobStore::open(storage, "family-a", BlobPlane::Semantic).expect("open semantic blob store")
}

#[test]
fn plane_store_uses_the_family_layout_and_required_pragmas() {
    let storage = tempfile::tempdir().expect("create temporary storage");
    let semantic = semantic_store(storage.path());
    let callgraph = BlobStore::open(storage.path(), "family-a", BlobPlane::Callgraph)
        .expect("open callgraph blob store");

    assert_eq!(
        semantic.path(),
        storage.path().join("blobs/family-a/semantic.sqlite")
    );
    assert_eq!(
        callgraph.path(),
        storage.path().join("blobs/family-a/callgraph.sqlite")
    );
    for store in [&semantic, &callgraph] {
        assert_eq!(store.pragmas().journal_mode, "wal");
        assert_eq!(store.pragmas().synchronous, 1);
        assert_eq!(store.pragmas().busy_timeout_ms, 5_000);
        assert_eq!(store.pragmas().foreign_keys, 0);
        assert_eq!(
            store.pragmas().wal_autocheckpoint_pages,
            DEFAULT_WAL_AUTOCHECKPOINT_PAGES
        );
    }
}

#[test]
fn immutable_puts_are_idempotent_and_payloads_round_trip() {
    let storage = tempfile::tempdir().expect("create temporary storage");
    let mut store = semantic_store(storage.path());
    let key = SemanticKey::for_current(b"source", b"src/lib.rs", "model-a").full_key();

    let inserted = store.put(&key, b"first payload").expect("insert payload");
    let reused = store
        .put(&key, b"replacement payload")
        .expect("repeat payload put");

    assert_eq!(inserted.outcome, PutOutcome::Inserted);
    assert!(inserted.durable);
    assert_eq!(reused.outcome, PutOutcome::Reused);
    assert!(reused.durable);
    assert_eq!(
        store.get(&key).expect("read payload"),
        Some(b"first payload".to_vec())
    );

    store.quarantine(&key).expect("quarantine key");
    let quarantined = store
        .put(&key, b"ignored payload")
        .expect("put quarantined key");
    assert_eq!(quarantined.outcome, PutOutcome::Quarantined);
    assert!(!quarantined.durable);
}

#[test]
fn payload_integrity_mismatches_are_cache_misses() {
    let storage = tempfile::tempdir().expect("create temporary storage");
    let mut store = semantic_store(storage.path());
    let key = SemanticKey::for_current(b"source", b"src/lib.rs", "model-a").full_key();
    store.put(&key, b"payload").expect("insert payload");
    let path = store.path().to_path_buf();
    drop(store);

    let connection = rusqlite::Connection::open(&path).expect("open fixture database");
    connection
        .execute(
            "UPDATE blob_payloads SET payload = ?1 WHERE full_key = ?2",
            params![b"corrupted payload", key.as_bytes().as_slice()],
        )
        .expect("corrupt fixture payload");
    drop(connection);

    let store = semantic_store(storage.path());
    assert_eq!(store.get(&key).expect("read corrupt payload"), None);
}

#[test]
fn semantic_keys_include_paths_but_callgraph_keys_reuse_equal_content() {
    let bytes = b"same bytes";
    let semantic_a = SemanticKey::for_current(bytes, b"src/a.rs", "model-a").full_key();
    let semantic_b = SemanticKey::for_current(bytes, b"src/b.rs", "model-a").full_key();
    let callgraph_a = CallgraphKey::for_current(bytes, "rust").full_key();
    let callgraph_b = CallgraphKey::for_current(bytes, "rust").full_key();
    let config = CallgraphKey::for_current(b"[package]", "config").full_key();

    assert_ne!(semantic_a, semantic_b);
    assert_eq!(callgraph_a, callgraph_b);
    assert_ne!(callgraph_a, config);

    let storage = tempfile::tempdir().expect("create temporary storage");
    let mut semantic_store = semantic_store(storage.path());
    assert!(matches!(
        semantic_store.put(&callgraph_a, b"wrong plane"),
        Err(BlobStoreError::PlaneKeyMismatch {
            store_plane: BlobPlane::Semantic,
            key_plane: BlobPlane::Callgraph,
        })
    ));
}
