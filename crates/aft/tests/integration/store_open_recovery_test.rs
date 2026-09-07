use std::fs;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Barrier};
use std::thread;

use aft::blob_store::{BlobPlane, BlobStore, BlobStoreBreaker, SemanticKey, BUSY_TIMEOUT_MS};

#[derive(Default)]
struct CountingBreaker {
    deaths: AtomicUsize,
}

impl BlobStoreBreaker for CountingBreaker {
    fn record_corruption_death(&self, artifact_key: &str, plane: BlobPlane) {
        assert_eq!(artifact_key, "family-a");
        assert_eq!(plane, BlobPlane::Semantic);
        self.deaths.fetch_add(1, Ordering::SeqCst);
    }
}

#[test]
fn eight_concurrent_first_opens_create_one_schema_without_errors() {
    let storage = tempfile::tempdir().expect("create temporary storage");
    let barrier = Arc::new(Barrier::new(8));
    let mut handles = Vec::new();

    for _ in 0..8 {
        let path = storage.path().to_path_buf();
        let barrier = Arc::clone(&barrier);
        handles.push(thread::spawn(move || {
            barrier.wait();
            BlobStore::open(&path, "family-a", BlobPlane::Semantic)
                .map(|store| store.pragmas().clone())
        }));
    }

    for handle in handles {
        let pragmas = handle
            .join()
            .expect("first-open thread did not panic")
            .expect("first-open thread did not fail");
        assert_eq!(pragmas.busy_timeout_ms, BUSY_TIMEOUT_MS as i64);
    }

    let database = storage.path().join("blobs/family-a/semantic.sqlite");
    let connection = rusqlite::Connection::open(database).expect("open created database");
    let schemas: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'blob_payloads'",
            [],
            |row| row.get(0),
        )
        .expect("count payload schemas");
    assert_eq!(schemas, 1);
}

#[test]
fn garbled_header_is_preserved_and_records_one_breaker_death() {
    let storage = tempfile::tempdir().expect("create temporary storage");
    let database_dir = storage.path().join("blobs/family-a");
    fs::create_dir_all(&database_dir).expect("create family directory");
    let database = database_dir.join("semantic.sqlite");
    fs::write(&database, b"not a sqlite database").expect("garble sqlite header");
    let breaker = CountingBreaker::default();

    let store =
        BlobStore::open_with_breaker(storage.path(), "family-a", BlobPlane::Semantic, &breaker)
            .expect("recover garbled database");

    assert_eq!(breaker.deaths.load(Ordering::SeqCst), 1);
    assert!(store.path().is_file());
    let entries = fs::read_dir(&database_dir)
        .expect("list family directory")
        .map(|entry| {
            entry
                .expect("read directory entry")
                .file_name()
                .into_string()
                .unwrap()
        })
        .collect::<Vec<_>>();
    assert!(entries
        .iter()
        .any(|name| name.starts_with("semantic.sqlite.corrupt-")));

    let key = SemanticKey::for_current(b"source", b"src/lib.rs", "model-a").full_key();
    assert_eq!(store.get(&key).expect("read clean replacement"), None);
}
