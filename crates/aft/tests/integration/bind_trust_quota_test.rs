use std::fs;
use std::path::{Path, PathBuf};

use aft::blob_store::{BlobPlane, BlobStore, SemanticKey};
use aft::subc::blob_store::{BlobQuota, BoundBlobStore, BoundPutOutcome};
use aft::subc::BindTrust;

fn key() -> aft::blob_store::FullKey {
    SemanticKey::for_current(b"source", b"src/lib.rs", "model-a").full_key()
}

fn bound(storage: &Path, family: &str, trust: BindTrust, view: &str) -> BoundBlobStore {
    BoundBlobStore::new(
        BlobStore::open(storage, family, BlobPlane::Semantic).expect("open blob store"),
        trust,
        view,
    )
}

fn assert_untrusted_bind_class(trust: BindTrust) {
    let storage = tempfile::tempdir().expect("tempdir");
    let full_key = key();
    let mut owner = bound(storage.path(), "family-a", BindTrust::FirstParty, "view-a");
    assert!(matches!(
        owner
            .put("family-a", b"src/lib.rs", &full_key, b"payload")
            .expect("owner put"),
        BoundPutOutcome::Stored(_)
    ));

    let mut untrusted = bound(storage.path(), "family-a", trust, "view-a");
    assert_eq!(
        untrusted
            .put("family-a", b"src/lib.rs", &full_key, b"payload")
            .expect("untrusted put"),
        BoundPutOutcome::Denied
    );
    assert_eq!(
        untrusted.get("family-a", &full_key).expect("own read"),
        Some(b"payload".to_vec())
    );
    assert_eq!(
        untrusted.get("family-b", &full_key).expect("other read"),
        None
    );
    assert!(untrusted.allow_manifest_write("family-a", "view-a"));
    assert!(!untrusted.allow_manifest_write("family-a", "view-b"));
}

#[test]
fn mcp_bind_trust_gates_put_read_and_manifest_write() {
    // `mcp:*` route binds resolve to the untrusted capability class.
    assert_untrusted_bind_class(BindTrust::Untrusted);
}

#[test]
fn unverified_bind_trust_gates_put_read_and_manifest_write() {
    assert_untrusted_bind_class(BindTrust::Untrusted);
}

#[test]
fn federation_bind_trust_gates_put_read_and_manifest_write() {
    // `fed:*` route binds resolve to the untrusted capability class.
    assert_untrusted_bind_class(BindTrust::Untrusted);
}

#[test]
fn bound_puts_cache_usage_below_the_quota_threshold() {
    let storage = tempfile::tempdir().expect("tempdir");
    let mut store = BoundBlobStore::with_quota(
        BlobStore::open(storage.path(), "family-a", BlobPlane::Semantic).expect("open blob store"),
        BindTrust::FirstParty,
        "view-a",
        BlobQuota {
            payload_bytes: 10_000,
            rows: 10_000,
        },
    );

    for number in 0..100 {
        let path = format!("src/{number}.rs");
        let full_key = SemanticKey::for_current(
            format!("source-{number}").as_bytes(),
            path.as_bytes(),
            "model-a",
        )
        .full_key();
        assert!(matches!(
            store
                .put("family-a", path.as_bytes(), &full_key, b"payload")
                .expect("put"),
            BoundPutOutcome::Stored(_)
        ));
    }
    assert_eq!(
        store.usage_read_count(),
        1,
        "100 puts use one cached usage read"
    );
}

#[test]
fn injected_value_coverage_refuses_byte_quota_and_requests_sweep() {
    let storage = tempfile::tempdir().expect("tempdir");
    let full_key = key();
    let mut store = BoundBlobStore::with_quota(
        BlobStore::open(storage.path(), "family-a", BlobPlane::Semantic).expect("open blob store"),
        BindTrust::FirstParty,
        "view-a",
        BlobQuota {
            payload_bytes: 7,
            rows: 10,
        },
    );

    assert!(matches!(
        store
            .put("family-a", b"src/lib.rs", &full_key, b"payload")
            .expect("put at exact byte limit"),
        BoundPutOutcome::Stored(_)
    ));
    let next_key = SemanticKey::for_current(b"next source", b"src/next.rs", "model-a").full_key();
    assert_eq!(
        store
            .put("family-a", b"src/next.rs", &next_key, b"payload")
            .expect("quota put"),
        BoundPutOutcome::QuotaExceeded
    );
    assert_eq!(store.failed_paths().collect::<Vec<_>>()[0].reason, "quota");
    assert_eq!(store.sweep_requests()[0].reason, "quota");
}

#[test]
fn injected_value_coverage_refuses_row_quota_and_requests_sweep() {
    let storage = tempfile::tempdir().expect("tempdir");
    let full_key = key();
    let mut store = BoundBlobStore::with_quota(
        BlobStore::open(storage.path(), "family-a", BlobPlane::Semantic).expect("open blob store"),
        BindTrust::FirstParty,
        "view-a",
        BlobQuota {
            payload_bytes: 100,
            rows: 0,
        },
    );

    assert_eq!(
        store
            .put("family-a", b"src/lib.rs", &full_key, b"payload")
            .expect("quota put"),
        BoundPutOutcome::QuotaExceeded
    );
    assert_eq!(store.failed_paths().collect::<Vec<_>>()[0].reason, "quota");
    assert_eq!(store.sweep_requests()[0].reason, "quota");
}

#[test]
fn only_the_bound_facade_opens_or_puts_blob_stores() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/subc");
    let mut sources = Vec::new();
    collect_rust_sources(&root, &mut sources);
    let facade = root.join("blob_store.rs");
    assert!(
        sources.contains(&facade),
        "the scan must include the facade"
    );

    for source in sources {
        let contents = fs::read_to_string(&source).expect("read subc source");
        if source == facade {
            assert!(
                contents.contains("self.store.put("),
                "facade must own blob puts"
            );
            continue;
        }
        assert!(
            !contents.contains("BlobStore::open") && !contents.contains("BlobStore::put"),
            "only subc/blob_store.rs may open or put a raw BlobStore: {}",
            source.display()
        );
    }
}

fn collect_rust_sources(directory: &Path, out: &mut Vec<PathBuf>) {
    for entry in fs::read_dir(directory).expect("read subc directory") {
        let path = entry.expect("read subc entry").path();
        if path.is_dir() {
            collect_rust_sources(&path, out);
        } else if path.extension().and_then(|extension| extension.to_str()) == Some("rs") {
            out.push(path);
        }
    }
}
