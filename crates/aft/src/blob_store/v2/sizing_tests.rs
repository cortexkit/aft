//! Verify sizing against physical overflow-page reads, not just SQL results.

use std::cell::RefCell;
use std::collections::BTreeSet;
use std::os::raw::{c_int, c_void};

use rusqlite::ffi;

use super::*;

struct ReadCounts {
    original: unsafe extern "C" fn(*mut ffi::sqlite3_file, *mut c_void, c_int, i64) -> c_int,
    overflow_pages: BTreeSet<i64>,
    page_size: i64,
    reads: usize,
    overflow_reads: usize,
}

thread_local! {
    // SQLite performs synchronous VFS reads on the thread executing the query.
    static COUNTS: RefCell<Option<ReadCounts>> = const { RefCell::new(None) };
}

unsafe extern "C" fn counted_read(
    file: *mut ffi::sqlite3_file,
    buffer: *mut c_void,
    amount: c_int,
    offset: i64,
) -> c_int {
    let original = COUNTS.with_borrow_mut(|counts| {
        let counts = counts.as_mut().expect("read probe installed");
        counts.reads += 1;
        if amount > 0 {
            let first = offset / counts.page_size + 1;
            let last = (offset + i64::from(amount) - 1) / counts.page_size + 1;
            counts.overflow_reads += counts.overflow_pages.range(first..=last).count();
        }
        counts.original
    });
    unsafe { original(file, buffer, amount, offset) }
}

/// Replace only this connection's main-file xRead; other tests keep their VFS.
/// The connection must outlive the probe and stay on the installing thread.
struct ReadProbe {
    file: *mut ffi::sqlite3_file,
    original: *const ffi::sqlite3_io_methods,
    _methods: Box<ffi::sqlite3_io_methods>,
}

impl ReadProbe {
    fn install(connection: &rusqlite::Connection) -> Self {
        connection
            .execute_batch("PRAGMA mmap_size=0; PRAGMA shrink_memory;")
            .unwrap();
        let page_size = connection
            .pragma_query_value(None, "page_size", |row| row.get(0))
            .unwrap();
        let overflow_pages = connection
            .prepare("SELECT pageno FROM dbstat WHERE name='blob_payloads' AND pagetype='overflow'")
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .collect::<rusqlite::Result<BTreeSet<i64>>>()
            .unwrap();
        assert!(
            !overflow_pages.is_empty(),
            "fixture must have overflow pages"
        );
        connection.execute_batch("PRAGMA shrink_memory;").unwrap();
        let mut file: *mut ffi::sqlite3_file = std::ptr::null_mut();
        let rc = unsafe {
            ffi::sqlite3_file_control(
                connection.handle(),
                c"main".as_ptr(),
                ffi::SQLITE_FCNTL_FILE_POINTER,
                std::ptr::from_mut(&mut file).cast(),
            )
        };
        assert_eq!(rc, ffi::SQLITE_OK);
        assert!(!file.is_null());
        let original = unsafe { (*file).pMethods };
        let mut methods = Box::new(unsafe { *original });
        COUNTS.with_borrow_mut(|counts| {
            assert!(counts.is_none());
            *counts = Some(ReadCounts {
                original: methods.xRead.unwrap(),
                overflow_pages,
                page_size,
                reads: 0,
                overflow_reads: 0,
            });
        });
        methods.xRead = Some(counted_read);
        unsafe {
            (*file).pMethods = std::ptr::from_ref(methods.as_ref());
        }
        Self {
            file,
            original,
            _methods: methods,
        }
    }

    fn counts(&self) -> (usize, usize) {
        COUNTS.with_borrow(|counts| {
            let counts = counts.as_ref().unwrap();
            (counts.reads, counts.overflow_reads)
        })
    }
}

impl Drop for ReadProbe {
    fn drop(&mut self) {
        unsafe {
            (*self.file).pMethods = self.original;
        }
        COUNTS.with_borrow_mut(|counts| *counts = None);
    }
}

fn populate(storage: &Path, rows: usize, payload_size: usize) -> FamilyStore {
    let access = StoreWriteAccess::for_registered_view(storage, "sizing");
    let store = FamilyStore::open(&access, FamilyPlane::Callgraph).unwrap();
    let mut payload = vec![0x9d; payload_size];
    for index in 0..rows {
        // Repeated epochs and reverse keys exercise both ordering tie-breaks.
        store.raise_epoch((index / 3) as u64).unwrap();
        payload.truncate(payload_size - index % 19);
        let mut key = [0; 32];
        key[..8].copy_from_slice(&((rows - index) as u64).to_be_bytes());
        store
            .put_or_touch(&FamilyKey::new(FamilyPlane::Callgraph, key), &payload)
            .unwrap();
        payload.resize(payload_size, 0x9d);
    }
    lock(&store.inner.connection)
        .execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")
        .unwrap();
    store
}

#[test]
fn sizing_large_blobs_reads_no_overflow_pages() {
    let dir = tempfile::tempdir().unwrap();
    let store = populate(dir.path(), 96, 64 * 1024);
    let connection = lock(&store.inner.connection);
    // Materializing the payload is deliberately the independent, expensive oracle.
    let expected_total: u64 = connection
        .prepare("SELECT payload FROM blob_payloads")
        .unwrap()
        .query_map([], |row| Ok(row.get::<_, Vec<u8>>(0)?.len() as u64))
        .unwrap()
        .map(Result::unwrap)
        .sum();
    let expected_rows = connection.prepare(
        "SELECT full_key, length(payload), ref_epoch FROM blob_payloads ORDER BY ref_epoch, full_key",
    ).unwrap().query_map([], |row| Ok(StoredRow {
        key: FamilyKey::new(FamilyPlane::Callgraph, row.get::<_, Vec<u8>>(0)?.try_into().unwrap()),
        payload_bytes: row.get(1)?, ref_epoch: row.get(2)?,
    })).unwrap().collect::<rusqlite::Result<Vec<_>>>().unwrap();
    let probe = ReadProbe::install(&connection);
    drop(connection);
    let usage = store.usage().unwrap();
    assert_eq!((usage.rows, usage.payload_bytes), (96, expected_total));
    assert_eq!(probe.counts().1, 0, "aggregate sizing read overflow pages");
    assert_eq!(store.rows_oldest(0).unwrap(), []);
    assert_eq!(store.rows_oldest(17).unwrap(), expected_rows[..17]);
    assert_eq!(store.rows().unwrap(), expected_rows);
    let (reads, overflow_reads) = probe.counts();
    assert!(reads > 0, "cold sizing must reach xRead");
    assert_eq!(overflow_reads, 0, "row sizing read overflow pages");
    // A payload read must register, proving the probe is not bypassed by mmap/cache.
    store.get(&expected_rows[0].key).unwrap().unwrap();
    assert!(
        probe.counts().1 > 0,
        "payload positive control missed overflow reads"
    );
}

/// An offline, under-budget family pass with 512 MiB of synthetic callgraph blobs.
#[test]
#[ignore = "creates a 512 MiB store for before/after disk-limit profiling"]
fn disk_limit_pass_large_store_probe() {
    use crate::views::eviction::{enforce, EnforceOptions};
    use crate::views::registry::FamilyRegistry;

    let dir = tempfile::tempdir().unwrap();
    let registry = FamilyRegistry::open(dir.path(), "sizing").unwrap();
    let store = populate(dir.path(), 16_384, 32 * 1024);
    for round in 0..3 {
        let probe = ReadProbe::install(&lock(&store.inner.connection));
        let start = std::time::Instant::now();
        let report = enforce(&registry, EnforceOptions::default()).unwrap();
        let elapsed = start.elapsed();
        let (reads, overflow_reads) = probe.counts();
        assert_eq!(
            report.before.family_store.payload_bytes,
            report.after.family_store.payload_bytes
        );
        assert_eq!(report.sweep.as_ref().unwrap().deleted_blobs, 0);
        assert!(report.sweep_error.is_none());
        assert!(!report.stopped_early);
        eprintln!("disk-limit probe round={round} payload_bytes={} file_bytes={} elapsed_ms={:.3} main_reads={reads} overflow_reads={overflow_reads}",
            report.before.family_store.payload_bytes, report.before.family_store.file_bytes,
            elapsed.as_secs_f64() * 1000.0);
        assert_eq!(overflow_reads, 0, "under-budget pass read overflow pages");
    }
}
