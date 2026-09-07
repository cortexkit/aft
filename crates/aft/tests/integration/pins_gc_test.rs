use std::collections::{BTreeMap, BTreeSet};
use std::fs;

use aft::blob_store::{BlobPlane, BlobStore, SemanticKey};
use aft::gc::{sweep, SweepReferences, SweepRequest, BLOB_AGE_FLOOR_MS};
use aft::pins::{AssemblyPin, QueryPin, PIN_TTL_MS};

fn semantic_key(name: &[u8]) -> aft::blob_store::FullKey {
    SemanticKey::for_current(name, name, "test-model").full_key()
}

fn age_payloads(store: &BlobStore) {
    let connection = rusqlite::Connection::open(store.path()).expect("open fixture database");
    connection
        .execute("UPDATE blob_payloads SET created_at_ms = 0", [])
        .expect("age fixture payloads");
}

fn sweep_request<'a>(
    storage: &'a tempfile::TempDir,
    view: &'a tempfile::TempDir,
    references: SweepReferences,
) -> SweepRequest<'a> {
    SweepRequest {
        storage: storage.path(),
        family: "family-a",
        view_dir: view.path(),
        byte_budget: 0,
        now_ms: BLOB_AGE_FLOOR_MS + PIN_TTL_MS + 1,
        references,
    }
}

#[test]
fn assembly_pin_writes_sorted_keys_before_protecting_slow_puts() {
    let storage = tempfile::tempdir().expect("create storage");
    let view = tempfile::tempdir().expect("create view");
    let first = semantic_key(b"first");
    let second = semantic_key(b"second");
    let mut store =
        BlobStore::open(storage.path(), "family-a", BlobPlane::Semantic).expect("open blob store");
    store.put(&first, b"first").expect("put first");
    store.put(&second, b"second").expect("put second");
    age_payloads(&store);

    let pin = AssemblyPin::create(
        view.path(),
        "family-a",
        "view-a",
        "generation-1",
        &[second.clone(), first.clone()],
    )
    .expect("create assembly pin before puts");
    let keys = fs::read_to_string(pin.keys_path()).expect("read durable keys");
    assert_eq!(
        keys.lines().collect::<Vec<_>>(),
        vec![first.to_hex(), second.to_hex()]
    );
    let metadata: serde_json::Value = serde_json::from_slice(
        &fs::read(view.path().join("pins/generation-1.json")).expect("read pin metadata"),
    )
    .expect("decode pin metadata");
    assert_eq!(metadata["family"], "family-a");
    assert_eq!(metadata["view"], "view-a");
    assert!(metadata["owner"]["pid"].is_u64());
    assert!(metadata["renewed_at"].is_u64());

    let report = sweep(sweep_request(&storage, &view, SweepReferences::default())).expect("sweep");
    assert_eq!(report.deleted_blobs, 0);
    assert_eq!(
        store.get(&first).expect("read first"),
        Some(b"first".to_vec())
    );
    assert_eq!(
        store.get(&second).expect("read second"),
        Some(b"second".to_vec())
    );
}

#[test]
fn gc_never_evicts_retained_or_query_pinned_generation_blobs() {
    let storage = tempfile::tempdir().expect("create storage");
    let view = tempfile::tempdir().expect("create view");
    let retained = semantic_key(b"retained");
    let query = semantic_key(b"query");
    let disposable = semantic_key(b"disposable");
    let mut store =
        BlobStore::open(storage.path(), "family-a", BlobPlane::Semantic).expect("open blob store");
    for key in [&retained, &query, &disposable] {
        store.put(key, b"payload").expect("put fixture blob");
    }
    age_payloads(&store);

    let _query_pin = QueryPin::acquire(view.path(), "generation-7").expect("pin query generation");
    let mut references = SweepReferences::default();
    references.retained_keys.insert(*retained.as_bytes());
    references.generation_keys.insert(
        "generation-7".to_owned(),
        BTreeSet::from([*query.as_bytes()]),
    );

    let report = sweep(sweep_request(&storage, &view, references)).expect("sweep");
    assert_eq!(report.deleted_blobs, 1);
    assert_eq!(
        store.get(&retained).expect("read retained"),
        Some(b"payload".to_vec())
    );
    assert_eq!(
        store.get(&query).expect("read query"),
        Some(b"payload".to_vec())
    );
    assert_eq!(store.get(&disposable).expect("read disposable"), None);
}

#[test]
fn expired_live_looking_assembly_pin_is_reclaimed_by_ttl() {
    let storage = tempfile::tempdir().expect("create storage");
    let view = tempfile::tempdir().expect("create view");
    let key = semantic_key(b"expired");
    let mut store =
        BlobStore::open(storage.path(), "family-a", BlobPlane::Semantic).expect("open blob store");
    store.put(&key, b"payload").expect("put fixture blob");
    age_payloads(&store);
    let pin = AssemblyPin::create(
        view.path(),
        "family-a",
        "view-a",
        "generation-2",
        &[key.clone()],
    )
    .expect("create assembly pin");
    let metadata_path = view.path().join("pins/generation-2.json");
    let mut metadata: serde_json::Value =
        serde_json::from_slice(&fs::read(&metadata_path).expect("read metadata"))
            .expect("decode metadata");
    metadata["renewed_at"] = serde_json::json!(0_u64);
    fs::write(
        &metadata_path,
        serde_json::to_vec(&metadata).expect("encode metadata"),
    )
    .expect("expire pin metadata");

    let report = sweep(sweep_request(&storage, &view, SweepReferences::default())).expect("sweep");
    assert_eq!(report.reclaimed_pins, 1);
    assert!(!metadata_path.exists());
    assert_eq!(store.get(&key).expect("read expired blob"), None);
    drop(pin);
}

#[test]
fn retained_reference_check_is_the_gc_safety_boundary() {
    let storage = tempfile::tempdir().expect("create storage");
    let view = tempfile::tempdir().expect("create view");
    let key = semantic_key(b"manifest-reference");
    let mut store =
        BlobStore::open(storage.path(), "family-a", BlobPlane::Semantic).expect("open blob store");
    store.put(&key, b"payload").expect("put fixture blob");
    age_payloads(&store);

    let references = SweepReferences {
        retained_keys: BTreeSet::from([*key.as_bytes()]),
        generation_keys: BTreeMap::new(),
    };
    let report = sweep(sweep_request(&storage, &view, references)).expect("sweep");
    assert_eq!(report.deleted_blobs, 0);
    assert_eq!(
        store.get(&key).expect("read retained blob"),
        Some(b"payload".to_vec())
    );
}
