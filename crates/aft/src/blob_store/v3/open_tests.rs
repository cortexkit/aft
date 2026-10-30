//! Trace the actual opens, including source-reader connections created in v2.
use super::*;
use rusqlite::ffi;
use std::cell::RefCell;
use std::ffi::{c_char, c_int, c_uint, c_void, CStr};

#[derive(Default, Debug, PartialEq, Eq)]
struct Trace {
    path: String,
    statements: u64,
    schema: u64,
    payload_writes: u64,
}
thread_local! { static TRACE: RefCell<Trace> = RefCell::new(Trace::default()); }

unsafe extern "C" fn statement(
    _: c_uint,
    _: *mut c_void,
    stmt: *mut c_void,
    _: *mut c_void,
) -> c_int {
    let stmt = stmt as *mut ffi::sqlite3_stmt;
    let db = unsafe { ffi::sqlite3_db_handle(stmt) };
    let name = unsafe { ffi::sqlite3_db_filename(db, c"main".as_ptr()) };
    if name.is_null() {
        return 0;
    }
    let path = unsafe { CStr::from_ptr(name) }.to_string_lossy();
    // Compare canonical spellings even when SQLite preserves a directory alias.
    let Ok(path) = fs::canonicalize(path.as_ref()) else {
        return 0;
    };
    let path = path.to_string_lossy();
    TRACE.with_borrow_mut(|trace| {
        if trace.path.is_empty() || trace.path != path {
            return;
        }
        let sql = unsafe { CStr::from_ptr(ffi::sqlite3_sql(stmt)) }
            .to_string_lossy()
            .trim()
            .to_ascii_uppercase();
        trace.statements += 1;
        // Count schema-capable statements conservatively, including no-op DDL.
        // Persistent schema/page snapshots below also detect any actual change.
        if ["CREATE ", "ALTER ", "DROP ", "REINDEX ", "VACUUM"]
            .iter()
            .any(|prefix| sql.starts_with(prefix))
            || (sql.starts_with("PRAGMA SCHEMA_VERSION") && sql.contains('='))
        {
            trace.schema += 1;
        }
        if ["INSERT ", "UPDATE ", "DELETE ", "REPLACE "]
            .iter()
            .any(|prefix| sql.starts_with(prefix))
            && sql.contains("BLOB_PAYLOADS")
        {
            trace.payload_writes += 1;
        }
    });
    0
}
unsafe extern "C" fn on_open(
    db: *mut ffi::sqlite3,
    _: *mut *mut c_char,
    _: *const ffi::sqlite3_api_routines,
) -> c_int {
    // The extension is process-global, but only the test thread that selected
    // a fixture installs a tracer. Other tests keep their ordinary connections.
    if !TRACE
        .try_with(|trace| !trace.borrow().path.is_empty())
        .unwrap_or(false)
    {
        return ffi::SQLITE_OK;
    }
    unsafe {
        ffi::sqlite3_trace_v2(
            db,
            ffi::SQLITE_TRACE_STMT as u32,
            Some(statement),
            std::ptr::null_mut(),
        );
    }
    ffi::SQLITE_OK
}
fn start(path: &Path) {
    // SQLite resolves directory symlinks, including macOS /var -> /private/var.
    // The database may not exist yet, so canonicalize its existing parent.
    let path = fs::canonicalize(path.parent().unwrap())
        .unwrap()
        .join(path.file_name().unwrap());
    static REGISTER: std::sync::Once = std::sync::Once::new();
    REGISTER.call_once(|| unsafe {
        assert_eq!(ffi::sqlite3_auto_extension(Some(on_open)), ffi::SQLITE_OK);
    });
    TRACE.set(Trace {
        path: path.to_str().unwrap().to_owned(),
        ..Trace::default()
    });
}
fn finish() -> Trace {
    TRACE.replace(Trace::default())
}
fn snapshot(path: &Path) -> (u64, u64) {
    let c = IdentityConnection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_WRITE,
        "views-v3-open-test",
    )
    .unwrap();
    (
        c.pragma_query_value(None, "schema_version", |r| r.get(0))
            .unwrap(),
        c.pragma_query_value(None, "page_count", |r| r.get(0))
            .unwrap(),
    )
}

#[test]
fn statement_tracer_detects_schema_and_payload_writes() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("control.sqlite");
    start(&path);
    let c = IdentityConnection::open(&path, "views-v3-open-test").unwrap();
    c.execute_batch("CREATE TABLE blob_payloads(payload BLOB); INSERT INTO blob_payloads VALUES(x'00'); UPDATE blob_payloads SET payload=x'01';").unwrap();
    let trace = finish();
    assert_eq!(trace.schema, 1, "{trace:?}");
    assert_eq!(trace.payload_writes, 2, "{trace:?}");
}

#[cfg(unix)]
#[test]
fn statement_tracer_normalizes_symlinked_parents_before_file_creation() {
    let root = tempfile::tempdir().unwrap();
    let store = root.path().join("store");
    fs::create_dir(&store).unwrap();
    let store = fs::canonicalize(store).unwrap();
    let alias = root.path().join("alias");
    std::os::unix::fs::symlink(&store, &alias).unwrap();
    let path = alias.join("control.sqlite");
    assert!(!path.exists());
    start(&path);
    let other =
        IdentityConnection::open(root.path().join("other.sqlite"), "views-v3-open-test").unwrap();
    other
        .execute_batch(
            "CREATE TABLE blob_payloads(payload BLOB); INSERT INTO blob_payloads VALUES(x'00');",
        )
        .unwrap();
    let c = IdentityConnection::open(store.join("control.sqlite"), "views-v3-open-test").unwrap();
    c.execute_batch("CREATE TABLE blob_payloads(payload BLOB); INSERT INTO blob_payloads VALUES(x'00'); UPDATE blob_payloads SET payload=x'01';").unwrap();
    let trace = finish();
    assert_eq!(trace.schema, 1, "{trace:?}");
    assert_eq!(trace.payload_writes, 2, "{trace:?}");
    assert_eq!(
        trace.statements, 3,
        "unrelated database must stay excluded: {trace:?}"
    );
}

#[test]
#[ignore = "1 GiB checkpointed fixture; run by the rowid-layout harness"]
fn old_store_dispatch_and_copy_readers_never_migrate_on_open() {
    let _guard = tests::serial();
    let root = tempfile::tempdir().unwrap();
    let family = "0123456789abcdef";
    let registry = crate::views::registry::FamilyRegistry::open(root.path(), family).unwrap();
    let registration = registry
        .register_view_with_storage(root.path(), "holder", root.path())
        .unwrap();
    drop(registration.open_store(FamilyPlane::Semantic).unwrap());
    let v2 = super::super::v2::plane_path(root.path(), family, FamilyPlane::Semantic).unwrap();
    {
        let mut c = IdentityConnection::open(&v2, "views-v3-open-test").unwrap();
        let digest = blake3::hash(&vec![0; 64 * 1024 * 1024]);
        let tx = c.transaction().unwrap();
        // SQLite writes real pages, not a sparse file padded to look large.
        for i in 0..17u8 {
            tx.execute(
                "INSERT INTO blob_payloads VALUES(?1,zeroblob(67108864),?2,1,0,0)",
                params![vec![i; 32], digest.as_bytes().as_slice()],
            )
            .unwrap();
        }
        tx.commit().unwrap();
        checkpoint(&c).unwrap();
    }
    assert!(fs::metadata(&v2).unwrap().len() >= 1 << 30);
    assert!(!v2.with_extension("sqlite-wal").exists());
    drop(
        crate::blob_store::BlobStore::open(
            root.path(),
            family,
            crate::blob_store::BlobPlane::Semantic,
        )
        .unwrap(),
    );
    let v1 = root
        .path()
        .join("blobs")
        .join(family)
        .join("semantic.sqlite");
    {
        let c = IdentityConnection::open(&v1, "views-v3-open-test").unwrap();
        assert!(c.prepare("SELECT created_at_ms FROM blob_payloads").is_ok());
        c.execute(
            "INSERT INTO blob_payloads VALUES(zeroblob(32),x'01',?1,1,0)",
            [blake3::hash(&[1]).as_bytes().as_slice()],
        )
        .unwrap();
        checkpoint(&c).unwrap();
    }
    for (path, is_v2) in [(&v1, false), (&v2, true)] {
        let before = snapshot(path);
        start(path);
        // The dispatch must refuse an old file before any bootstrap, DDL or WAL switch.
        assert!(open_db(path, Kind::Plane(FamilyPlane::Semantic), true, false).is_err());
        assert!(open_db(path, Kind::Plane(FamilyPlane::Semantic), false, true).is_err());
        // Close a writable observer to remove SQLite's empty read-only sidecars
        // before exercising the copy-source open on the checkpointed fixture.
        snapshot(path);
        assert!(!path.with_extension("sqlite-wal").exists());
        if is_v2 {
            let reader = copy::v2_reader(
                root.path(),
                family,
                &FamilyKey::new(FamilyPlane::Semantic, [0; 32]),
            )
            .unwrap()
            .unwrap();
            assert!(reader
                .copy_source_identity(&FamilyKey::new(FamilyPlane::Semantic, [0; 32]))
                .unwrap()
                .is_some());
        } else {
            let reader = copy::open_v1(path).unwrap().unwrap();
            assert_eq!(
                reader
                    .query_row("SELECT length(payload) FROM blob_payloads", [], |r| r
                        .get::<_, u64>(0))
                    .unwrap(),
                1
            );
        }
        let trace = finish();
        eprintln!("{path:?}: {trace:?}");
        assert!(
            trace.statements > 0,
            "the tracer must observe the actual open"
        );
        assert_eq!(trace.schema, 0, "{trace:?}");
        assert_eq!(trace.payload_writes, 0, "{trace:?}");
        let after = snapshot(path);
        eprintln!(
            "schema_version/page_count before={before:?} after={after:?}, bytes={}",
            fs::metadata(path).unwrap().len()
        );
        assert_eq!(after, before);
        // A read-only WAL reader may leave an empty sidecar; there must be no frames.
        assert!(fs::metadata(path.with_extension("sqlite-wal")).map_or(true, |m| m.len() == 0));
    }
}
