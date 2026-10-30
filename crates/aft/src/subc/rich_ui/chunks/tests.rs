use std::sync::{Arc, Mutex};

use rusqlite::Connection;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use super::*;

const GOLDEN_MANIFEST: &[u8] =
    include_bytes!("../../../../tests/fixtures/rich_ui/manifest.jcs.json");

fn sha256(bytes: &[u8]) -> String {
    format!("sha256:{:x}", Sha256::digest(bytes))
}

fn migrated() -> Mutex<Box<Connection>> {
    let mut conn = Connection::open_in_memory().unwrap();
    crate::db::run_migrations(&mut conn).unwrap();
    Mutex::new(Box::new(conn))
}

fn store_template(db: &Mutex<Box<Connection>>, bytes: &[u8]) -> String {
    let digest = sha256(bytes);
    db.lock()
        .unwrap()
        .execute(
            "INSERT INTO rich_ui_templates (digest, bytes) VALUES (?1, ?2)",
            rusqlite::params![digest, bytes],
        )
        .unwrap();
    digest
}

fn code(result: Result<Value, ErrorBody>) -> String {
    let error = result.expect_err("expected a refusal");
    assert_eq!(
        error.message,
        operations::fixed_message(&error.code),
        "every chunk refusal carries its code's fixed message"
    );
    error.code
}

/// A 300,000-byte asset whose first chunk boundary (byte 131,072) falls
/// between the two bytes of a two-byte UTF-8 character.
pub(crate) fn split_utf8_asset() -> Vec<u8> {
    let mut asset = b"a".to_vec();
    while asset.len() + 2 <= 300_000 {
        asset.extend_from_slice("é".as_bytes());
    }
    asset.resize(300_000, b'a');
    assert_eq!(asset.len(), 300_000);
    assert_eq!(asset[CHUNK_BYTES - 1], 0xC3, "a chunk ends on a lead byte");
    assert_eq!(
        asset[CHUNK_BYTES], 0xA9,
        "the next chunk starts mid-character"
    );
    asset
}

/// Fetch every chunk of `digest` through `fetch`, checking each reply's
/// shape, and return the reassembled bytes.
pub(crate) fn reassemble(digest: &str, mut fetch: impl FnMut(u64) -> Value) -> Vec<u8> {
    let first = fetch(0);
    let chunks = first["chunks"].as_u64().unwrap();
    let total = first["bytes"].as_u64().unwrap();
    let mut joined = Vec::new();
    for index in 0..chunks {
        let reply = if index == 0 {
            first.clone()
        } else {
            fetch(index)
        };
        let mut fields: Vec<&str> = reply
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        fields.sort_unstable();
        assert_eq!(fields, ["bytes", "chunks", "data", "digest", "index"]);
        assert_eq!(reply["digest"], digest);
        assert_eq!(reply["index"], index);
        assert_eq!(reply["chunks"], chunks);
        assert_eq!(reply["bytes"], total);
        let data = base64::engine::general_purpose::STANDARD
            .decode(reply["data"].as_str().unwrap())
            .unwrap();
        assert!(data.len() <= CHUNK_BYTES);
        joined.extend(data);
    }
    assert_eq!(joined.len() as u64, total);
    joined
}

#[test]
fn chunk_queries_admit_only_the_occipital_principal() {
    assert!(admit_principal(Some("reserved:occipital")).is_ok());
    for principal in [
        None,
        Some("reserved:prefrontal-core"),
        Some("reserved:basal"),
        Some("direct"),
        Some("unverified"),
        Some("reserved:occipital-shadow"),
    ] {
        assert_eq!(
            code(admit_principal(principal).map(|()| Value::Null)),
            "principal_not_allowed",
            "{principal:?}"
        );
    }
}

#[test]
fn chunk_operation_names_are_exact() {
    assert!(is_chunk_operation("rich_ui.manifest_chunk"));
    assert!(is_chunk_operation("rich_ui.template_chunk"));
    assert!(!is_chunk_operation("rich_ui.chunk"));
    assert!(!is_chunk_operation("tool.catalog"));
}

#[test]
fn manifest_chunk_before_publication_refuses_every_digest() {
    let _guard = set_test_manifest_for_test(None);
    for digest in [
        json!(sha256(GOLDEN_MANIFEST)),
        json!(sha256(b"")),
        json!("not-a-digest"),
    ] {
        assert_eq!(
            code(manifest_chunk(
                &json!({"manifestDigest": digest, "index": 0})
            )),
            "digest_unknown"
        );
    }
}

#[test]
fn manifest_chunk_serves_the_running_manifest_and_checks_the_index() {
    let _guard = set_test_manifest_for_test(Some(GOLDEN_MANIFEST.to_vec()));
    let digest = sha256(GOLDEN_MANIFEST);
    assert_eq!(
        digest,
        include_str!("../../../../tests/fixtures/rich_ui/manifest.sha256").trim()
    );
    let joined = reassemble(&digest, |index| {
        manifest_chunk(&json!({"manifestDigest": digest, "index": index})).unwrap()
    });
    assert_eq!(joined, GOLDEN_MANIFEST);
    assert_eq!(sha256(&joined), digest);
    let chunks = GOLDEN_MANIFEST.len().div_ceil(CHUNK_BYTES) as u64;
    for index in [
        json!(chunks),
        json!(-1),
        json!(1.5),
        json!("0"),
        Value::Null,
    ] {
        assert_eq!(
            code(manifest_chunk(
                &json!({"manifestDigest": digest, "index": index})
            )),
            "chunk_index_out_of_range",
            "{index}"
        );
    }
    assert_eq!(
        code(manifest_chunk(
            &json!({"manifestDigest": sha256(b"other"), "index": 0})
        )),
        "digest_unknown"
    );
    assert_eq!(code(manifest_chunk(&json!({"index": 0}))), "digest_unknown");
}

#[test]
fn template_chunk_splits_a_300000_byte_asset_into_three_chunks() {
    let db = migrated();
    let asset = split_utf8_asset();
    let digest = store_template(&db, &asset);
    let mut sizes = Vec::new();
    let joined = reassemble(&digest, |index| {
        let reply = template_chunk(
            &json!({"templateDigest": digest, "index": index}),
            Some(&db),
        )
        .unwrap();
        sizes.push(
            base64::engine::general_purpose::STANDARD
                .decode(reply["data"].as_str().unwrap())
                .unwrap()
                .len(),
        );
        reply
    });
    assert_eq!(sizes, [131_072, 131_072, 37_856]);
    assert_eq!(joined, asset);
    assert_eq!(sha256(&joined), digest);
    assert_eq!(
        code(template_chunk(
            &json!({"templateDigest": digest, "index": 3}),
            Some(&db)
        )),
        "chunk_index_out_of_range"
    );
}

#[test]
fn template_chunk_reports_database_state_rather_than_a_miss() {
    let request = json!({"templateDigest": sha256(b"never stored"), "index": 0});

    // No database handle at all.
    assert_eq!(
        code(template_chunk::<Box<Connection>>(&request, None)),
        "database_unavailable"
    );

    // The handle's lock is held: a busy database, never a miss.
    let db = migrated();
    {
        let _held = db.lock().unwrap();
        assert_eq!(code(template_chunk(&request, Some(&db))), "database_busy");
    }

    // A readable database that lacks the digest is the only real miss.
    assert_eq!(code(template_chunk(&request, Some(&db))), "digest_unknown");

    // A poisoned mutex cannot be trusted to hold a consistent connection.
    let poisoned = Arc::new(migrated());
    let thread_db = poisoned.clone();
    let _ = std::thread::spawn(move || {
        let _guard = thread_db.lock().unwrap();
        panic!("poison the database mutex");
    })
    .join();
    assert!(poisoned.is_poisoned());
    assert_eq!(
        code(template_chunk(&request, Some(&*poisoned))),
        "database_unavailable"
    );

    // A database whose query fails (here: no template table) is unreadable.
    let unmigrated = Mutex::new(Box::new(Connection::open_in_memory().unwrap()));
    assert_eq!(
        code(template_chunk(&request, Some(&unmigrated))),
        "database_unavailable"
    );
}
