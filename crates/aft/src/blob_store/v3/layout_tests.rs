use super::*;
use crate::blob_store::{BlobPlane, BlobStore, FullKey};

const FAMILY: &str = "0123456789abcdef";

#[test]
fn new_plane_uses_rowid_layout_tag() {
    let _guard = tests::serial();
    let root = tempfile::tempdir().unwrap();
    publish(root.path(), FAMILY, 1024, None, |p| {
        p.put(
            &FamilyKey::new(FamilyPlane::Semantic, [7; 32]),
            b"payload",
            1,
        )
    })
    .unwrap();
    let reader = PlaneReader::open_existing(root.path(), FAMILY, FamilyPlane::Semantic).unwrap();
    let c = reader.database.connection().unwrap();
    assert_eq!(
        c.query_row("SELECT layout_tag FROM format_meta", [], |r| r
            .get::<_, u32>(0))
            .unwrap(),
        2
    );
}

fn touch_wal_peak(storage: &Path) -> u64 {
    let bytes = [9; 32];
    let key = FamilyKey::new(FamilyPlane::Semantic, bytes);
    let payload = vec![6; 8 * 1024 * 1024];
    let mut source = BlobStore::open(storage, FAMILY, BlobPlane::Semantic).unwrap();
    source
        .put(
            &FullKey {
                bytes,
                plane: BlobPlane::Semantic,
            },
            &payload,
        )
        .unwrap();
    fs::create_dir_all(storage.join("views/holder/pins")).unwrap();
    let copy = CopySource::V1 {
        artifact_key: FAMILY,
        plane: BlobPlane::Semantic,
        key: bytes,
    };
    copy_on_first_use(storage, copy, "holder", None).unwrap();
    let reader = PlaneReader::open_existing(storage, FAMILY, FamilyPlane::Semantic).unwrap();
    let _writer = connections::shared(
        &plane_path(storage, FAMILY, FamilyPlane::Semantic).unwrap(),
        Kind::Plane(FamilyPlane::Semantic),
        false,
        false,
    )
    .unwrap();
    let peak = std::sync::atomic::AtomicU64::new(0);
    unsafe extern "C" fn wal(
        ctx: *mut std::ffi::c_void,
        _: *mut rusqlite::ffi::sqlite3,
        _: *const std::ffi::c_char,
        pages: std::ffi::c_int,
    ) -> std::ffi::c_int {
        let peak = unsafe { &*(ctx as *const std::sync::atomic::AtomicU64) };
        peak.fetch_max(pages as u64, std::sync::atomic::Ordering::SeqCst);
        0
    }
    let page_size: u64;
    {
        let c = reader.database.connection().unwrap();
        c.execute("UPDATE store_meta SET gc_epoch=17", []).unwrap();
        checkpoint(&c).unwrap();
        page_size = c
            .pragma_query_value(None, "page_size", |r| r.get(0))
            .unwrap();
        // SQLite reports committed WAL frames on the owning connection, including
        // the peak before publication checkpoints the file back to zero.
        unsafe {
            rusqlite::ffi::sqlite3_wal_hook(c.handle(), Some(wal), &peak as *const _ as *mut _);
        }
    }
    assert_eq!(
        copy_on_first_use(storage, copy, "holder", None).unwrap(),
        CopyOutcome::Reused
    );
    {
        let c = reader.database.connection().unwrap();
        unsafe {
            rusqlite::ffi::sqlite3_wal_hook(c.handle(), None, std::ptr::null_mut());
        }
    }
    assert_eq!(reader.get(&key).unwrap(), Some(payload));
    let bytes = peak.load(std::sync::atomic::Ordering::SeqCst) * (page_size + 24) + 32;
    eprintln!("reuse touch peak WAL bytes: {bytes}");
    assert!(bytes > 32, "the WAL hook must observe the touch commit");
    bytes
}

#[test]
fn reuse_touch_of_multi_mib_payload_has_small_wal_peak() {
    let _guard = tests::serial();
    let root = tempfile::tempdir().unwrap();
    let peak = touch_wal_peak(root.path());
    assert!(
        peak < 256 * 1024,
        "touch rewrote payload pages: {peak} WAL bytes"
    );
}

#[test]
fn both_layouts_allow_liveness_touch_and_delete() {
    let _guard = tests::serial();
    for old in [true, false] {
        CREATE_3A_LAYOUT.set(old);
        let root = tempfile::tempdir().unwrap();
        let key = FamilyKey::new(FamilyPlane::Semantic, [7; 32]);
        publish(root.path(), FAMILY, 1024, None, |p| {
            p.put(&key, b"payload", 3)
        })
        .unwrap();
        CREATE_3A_LAYOUT.set(false);
        let db = connections::shared(
            &plane_path(root.path(), FAMILY, FamilyPlane::Semantic).unwrap(),
            Kind::Plane(FamilyPlane::Semantic),
            false,
            false,
        )
        .unwrap();
        let c = db.connection().unwrap();
        c.execute("UPDATE store_meta SET gc_epoch=23", []).unwrap();
        drop(c);
        publish(root.path(), FAMILY, 64 * 1024, None, |p| {
            assert!(p.touch_row(&key, blake3::hash(b"payload").as_bytes())?);
            Ok(())
        })
        .unwrap();
        let c = db.connection().unwrap();
        let sql = if old {
            "SELECT ref_epoch FROM blob_payloads"
        } else {
            "SELECT ref_epoch FROM blob_liveness"
        };
        assert_eq!(c.query_row(sql, [], |r| r.get::<_, i64>(0)).unwrap(), 23);
        c.execute("DELETE FROM blob_payloads", []).unwrap();
        if !old {
            assert_eq!(
                c.query_row("SELECT count(*) FROM blob_liveness", [], |r| r
                    .get::<_, i64>(0))
                    .unwrap(),
                0
            );
        }
    }
}

#[test]
fn touch_batches_one_transaction_per_present_plane_in_both_layouts() {
    let _guard = tests::serial();
    for old in [true, false] {
        CREATE_3A_LAYOUT.set(old);
        let root = tempfile::tempdir().unwrap();
        let present: Vec<_> = (0..16)
            .map(|i| FamilyKey::new(FamilyPlane::Semantic, [i; 32]))
            .collect();
        publish(root.path(), FAMILY, 1 << 20, None, |p| {
            for key in &present {
                p.put(key, b"payload", 3)?;
            }
            Ok(())
        })
        .unwrap();
        CREATE_3A_LAYOUT.set(false);
        let db = connections::shared(
            &plane_path(root.path(), FAMILY, FamilyPlane::Semantic).unwrap(),
            Kind::Plane(FamilyPlane::Semantic),
            false,
            false,
        )
        .unwrap();
        db.connection()
            .unwrap()
            .execute("UPDATE store_meta SET gc_epoch=23", [])
            .unwrap();
        let absent = FamilyKey::new(FamilyPlane::Trigram, [99; 32]);
        let missing = FamilyKey::new(FamilyPlane::Semantic, [99; 32]);
        let mut keys = vec![absent, missing];
        keys.extend(&present);
        keys.push(absent);
        work_profile::take();
        let untouched = publish(root.path(), FAMILY, 1 << 20, None, |p| p.touch(&keys)).unwrap();
        assert_eq!(untouched, vec![absent, missing, absent]);
        assert_eq!(work_profile::count("touch_transaction"), 1);
        assert!(!plane_path(root.path(), FAMILY, FamilyPlane::Trigram)
            .unwrap()
            .exists());
        let sql = if old {
            "SELECT COUNT(*) FROM blob_payloads WHERE ref_epoch=23"
        } else {
            "SELECT COUNT(*) FROM blob_liveness WHERE ref_epoch=23"
        };
        assert_eq!(
            db.connection()
                .unwrap()
                .query_row(sql, [], |r| r.get::<_, usize>(0))
                .unwrap(),
            present.len()
        );
    }
}

#[test]
fn per_file_format_report_reads_both_layouts_and_registry_without_writes() {
    let _guard = tests::serial();
    for old in [true, false] {
        CREATE_3A_LAYOUT.set(old);
        let root = tempfile::tempdir().unwrap();
        publish(root.path(), FAMILY, 1024, None, |p| {
            p.put(
                &FamilyKey::new(FamilyPlane::Semantic, [7; 32]),
                b"payload",
                1,
            )
        })
        .unwrap();
        CREATE_3A_LAYOUT.set(false);
        for (path, name, tag) in [
            (
                plane_path(root.path(), FAMILY, FamilyPlane::Semantic).unwrap(),
                "views_v3_store",
                if old { 1 } else { 2 },
            ),
            (
                family_dir(root.path(), FAMILY)
                    .unwrap()
                    .join("members.sqlite"),
                "views_v3_registry",
                0,
            ),
        ] {
            let kind = if tag == 0 {
                Kind::Registry
            } else {
                Kind::Plane(FamilyPlane::Semantic)
            };
            let db = connections::shared(&path, kind, false, false).unwrap();
            let state = || {
                let c = db.connection().unwrap();
                (
                    c.pragma_query_value(None, "schema_version", |r| r.get::<_, i64>(0))
                        .unwrap(),
                    c.pragma_query_value(None, "page_count", |r| r.get::<_, i64>(0))
                        .unwrap(),
                    c.total_changes(),
                )
            };
            let before = state();
            let report = crate::blob_store::v3_format_report(&path).unwrap();
            assert_eq!(
                report,
                V3FormatReport {
                    store_name: name,
                    format_version: 1,
                    schema_version: 1,
                    layout_tag: tag
                }
            );
            assert_eq!(before, state());
        }
        assert!(!root.path().join("blobs/v2").exists());
        assert!(!root.path().join("blobs").join(FAMILY).exists());
    }
}

#[test]
fn shared_layout_control_exposes_large_touch_wal_peak() {
    let _guard = tests::serial();
    CREATE_3A_LAYOUT.set(true);
    let root = tempfile::tempdir().unwrap();
    let peak = touch_wal_peak(root.path());
    CREATE_3A_LAYOUT.set(false);
    assert!(
        peak > 8 * 1024 * 1024,
        "old layout control did not rewrite payload: {peak}"
    );
}

fn immutable_column_refused(sql: &str) {
    let _guard = tests::serial();
    for old in [false, true] {
        CREATE_3A_LAYOUT.set(old);
        let root = tempfile::tempdir().unwrap();
        publish(root.path(), FAMILY, 1024, None, |p| {
            p.put(
                &FamilyKey::new(FamilyPlane::Semantic, [7; 32]),
                b"payload",
                1,
            )
        })
        .unwrap();
        CREATE_3A_LAYOUT.set(false);
        let db = connections::shared(
            &plane_path(root.path(), FAMILY, FamilyPlane::Semantic).unwrap(),
            Kind::Plane(FamilyPlane::Semantic),
            false,
            false,
        )
        .unwrap();
        let c = db.connection().unwrap();
        let error = c
            .execute(sql, [])
            .expect_err("in-place payload/digest rewrite must be refused");
        assert!(error.to_string().contains("immutable_payload"), "{error}");
    }
}
#[test]
fn payload_only_update_is_refused() {
    immutable_column_refused("UPDATE blob_payloads SET payload=x'00'");
}
#[test]
fn digest_only_update_is_refused() {
    immutable_column_refused("UPDATE blob_payloads SET payload_digest=zeroblob(32)");
}

#[test]
fn unknown_plane_layout_tag_is_refused_by_name() {
    let _guard = tests::serial();
    let root = tempfile::tempdir().unwrap();
    publish(root.path(), FAMILY, 1024, None, |p| {
        p.put(
            &FamilyKey::new(FamilyPlane::Semantic, [7; 32]),
            b"payload",
            1,
        )
    })
    .unwrap();
    let path = plane_path(root.path(), FAMILY, FamilyPlane::Semantic).unwrap();
    {
        let db =
            connections::shared(&path, Kind::Plane(FamilyPlane::Semantic), false, false).unwrap();
        db.connection()
            .unwrap()
            .execute("UPDATE format_meta SET layout_tag=99", [])
            .unwrap();
    }
    let error = crate::blob_store::v3_format_report(&path).unwrap_err();
    assert!(error.to_string().ends_with("layout_tag:99"), "{error}");
}

#[test]
fn both_layouts_get_each_distinct_payload_byte_identical() {
    let _guard = tests::serial();
    let expected: Vec<_> = (0..32u8)
        .map(|i| {
            let mut bytes = [7; 32];
            bytes[31] = i;
            (
                FamilyKey::new(FamilyPlane::Semantic, bytes),
                vec![i; 4096 + i as usize * 17],
            )
        })
        .collect();
    for old in [true, false] {
        CREATE_3A_LAYOUT.set(old);
        let root = tempfile::tempdir().unwrap();
        publish(root.path(), FAMILY, 1024 * 1024, None, |publication| {
            for (key, payload) in &expected {
                publication.put(key, payload, 7)?;
            }
            Ok(())
        })
        .unwrap();
        CREATE_3A_LAYOUT.set(false);
        let reader =
            PlaneReader::open_existing(root.path(), FAMILY, FamilyPlane::Semantic).unwrap();
        for (key, payload) in &expected {
            assert_eq!(reader.get(key).unwrap().as_ref(), Some(payload));
        }
    }
}
