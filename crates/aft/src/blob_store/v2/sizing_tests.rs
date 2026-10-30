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
    read_bytes: u64,
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
        counts.read_bytes += amount.max(0) as u64;
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
                read_bytes: 0,
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

    fn read_bytes(&self) -> u64 {
        COUNTS.with_borrow(|counts| counts.as_ref().unwrap().read_bytes)
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
            report.before.family_store.file_bytes,
            report.after.family_store.file_bytes
        );
        assert_eq!(report.sweep.as_ref().unwrap().deleted_blobs, 0);
        assert!(report.sweep_error.is_none());
        assert!(!report.stopped_early);
        eprintln!("disk-limit probe round={round} file_bytes={} elapsed_ms={:.3} main_reads={reads} overflow_reads={overflow_reads}",
            report.before.family_store.file_bytes,
            elapsed.as_secs_f64() * 1000.0);
        assert_eq!(overflow_reads, 0, "under-budget pass read overflow pages");
    }
}

/// A disk-limit family sweep that deletes 28,000 rows from a 2.7 GB store,
/// once with the deletion counter's trigger removed (as before the counter
/// existed) and once with it. Write-ahead-log frames are counted with
/// automatic checkpoints off, so they are every page the pass wrote.
#[test]
#[ignore = "creates two 2.7 GB stores, one after the other, for deletion-trigger profiling"]
fn disk_limit_pass_deleting_28000_rows_probe() {
    use crate::gc::family::{sweep_family_remeasured, FamilySweepPolicy, SweepBounds};
    use crate::views::registry::FamilyRegistry;

    const ROWS: usize = 84_000;
    const PAYLOAD: usize = 32 * 1024;
    const DELETED: usize = 28_000;
    for counted in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let registry = FamilyRegistry::open(dir.path(), "sizing").unwrap();
        let store = populate(dir.path(), ROWS, PAYLOAD);
        // The family sweep deletes only rows last touched before the epoch it
        // raises the store to; reset every row's epoch so any row qualifies.
        lock(&store.inner.connection)
            .execute_batch(
                "UPDATE blob_payloads SET ref_epoch = 0; UPDATE store_meta SET gc_epoch = 0;",
            )
            .unwrap();
        if !counted {
            lock(&store.inner.connection)
                .execute_batch("DROP TRIGGER payload_deletions_count")
                .unwrap();
        }
        let total = store.usage().unwrap().payload_bytes;
        let oldest = store.rows_oldest(DELETED).unwrap();
        let budget = total - oldest.iter().map(|row| row.payload_bytes).sum::<u64>();
        let before = store.deletion_epoch().unwrap();
        let wal_path = {
            let mut name = store.path().as_os_str().to_owned();
            name.push("-wal");
            PathBuf::from(name)
        };
        {
            let connection = lock(&store.inner.connection);
            connection
                .execute_batch("PRAGMA wal_autocheckpoint = 0; PRAGMA wal_checkpoint(TRUNCATE);")
                .unwrap();
        }
        let page_size: u64 = lock(&store.inner.connection)
            .pragma_query_value(None, "page_size", |row| row.get(0))
            .unwrap();
        let wal_before = std::fs::metadata(&wal_path).map_or(0, |meta| meta.len());
        let probe = ReadProbe::install(&lock(&store.inner.connection));
        let started = std::time::Instant::now();
        let report = sweep_family_remeasured(
            &registry,
            None,
            FamilySweepPolicy {
                byte_budget: budget,
            },
            SweepBounds::default(),
            None,
        )
        .unwrap();
        let elapsed = started.elapsed();
        let (reads, overflow_reads) = probe.counts();
        drop(probe);
        let wal_after = std::fs::metadata(&wal_path).map_or(0, |meta| meta.len());
        let after = store.deletion_epoch().unwrap();
        let file_bytes = std::fs::metadata(store.path()).unwrap().len();
        eprintln!(
            "DELETION_TRIGGER_PROBE counted={counted} store_file_bytes={file_bytes} payload_bytes={total} deleted_blobs={} deleted_bytes={} elapsed_ms={:.1} main_reads={reads} overflow_reads={overflow_reads} wal_bytes_written={} wal_frames={} epoch_before={before:?} epoch_after={after:?}",
            report.deleted_blobs,
            report.deleted_bytes,
            elapsed.as_secs_f64() * 1000.0,
            wal_after.saturating_sub(wal_before),
            wal_after.saturating_sub(wal_before.max(32)) / (page_size + 24),
        );
        assert_eq!(report.deleted_blobs, DELETED);
        if counted {
            let count = |epoch: DeletionEpoch| {
                u64::from_le_bytes(epoch.to_bytes()[16..].try_into().unwrap())
            };
            assert_eq!(
                count(after.unwrap()) - count(before.unwrap()),
                DELETED as u64
            );
        }
    }
}

fn probe_wal(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push("-wal");
    name.into()
}

fn probe_checkpoint(store: &FamilyStore) {
    lock(&store.inner.connection)
        .execute_batch("PRAGMA wal_autocheckpoint=0; PRAGMA wal_checkpoint(TRUNCATE);")
        .unwrap();
}

/// Counts SQLite main-file reads and WAL appends separately from kernel I/O.
/// A cold SQLite cache is not a cold OS cache; physical reads may still be zero.
fn probe_touch(store: &FamilyStore, keys: &[FamilyKey], label: &str, split: bool) -> u64 {
    use crate::process_io::Bytes;

    probe_checkpoint(store);
    let probe = ReadProbe::install(&lock(&store.inner.connection));
    let before = Bytes::capture().unwrap();
    if split {
        let mut connection = lock(&store.inner.connection);
        let tx = connection.transaction().unwrap();
        for key in keys {
            tx.execute(
                "UPDATE probe_refs SET ref_epoch=ref_epoch+1 WHERE full_key=?1",
                params![key.as_bytes().as_slice()],
            )
            .unwrap();
        }
        tx.commit().unwrap();
    } else {
        assert_eq!(store.touch(keys).unwrap().touched, keys.len());
    }
    let reads = probe.read_bytes();
    assert!(reads > 0, "cold SQLite cache must reach the byte counter");
    let overflow_reads = probe.counts().1;
    drop(probe);
    let wal_bytes = std::fs::metadata(probe_wal(store.path())).map_or(0, |m| m.len());
    let operation_io = Bytes::capture().unwrap().delta(before).unwrap();
    let checkpoint: (u64, u64, u64) = lock(&store.inner.connection)
        .query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?))
        })
        .unwrap();
    assert_eq!(checkpoint.0, 0);
    let total_io = Bytes::capture().unwrap().delta(before).unwrap();
    eprintln!(
        "IO_PROBE {}",
        serde_json::json!({
            "kind": "touch", "label": label, "rows": keys.len(),
            "sqlite_main_read_bytes": reads, "overflow_reads": overflow_reads,
            "wal_append_bytes": wal_bytes, "operation_io": operation_io,
            "including_checkpoint_io": total_io,
        })
    );
    wal_bytes
}

#[test]
#[ignore = "offline SQLite I/O characterization; run single-threaded with isolated HOME/XDG"]
fn io_amplification_v2_touch_probe() {
    let base =
        PathBuf::from(std::env::var_os("AFT_IO_PROBE_DIR").expect("set isolated probe directory"));
    std::fs::create_dir_all(&base).unwrap();
    for payload_size in [4096, 16 * 1024, 64 * 1024, 256 * 1024, 1024 * 1024] {
        let dir = tempfile::tempdir_in(&base).unwrap();
        let store = populate(dir.path(), 32, payload_size);
        let keys: Vec<_> = store.rows().unwrap().into_iter().map(|r| r.key).collect();
        // Normalize the populate helper's varying epochs before measuring a no-op.
        store.touch(&keys).unwrap();
        let label = format!("payload_{payload_size}");
        let unchanged = probe_touch(&store, &keys, &format!("{label}_same_epoch"), false);
        assert_eq!(unchanged, 0, "current epochs must not open a writer");
        let epoch = store.gc_epoch().unwrap();
        store.raise_epoch(epoch + 1).unwrap();
        let updated = probe_touch(&store, &keys, &format!("{label}_advanced_epoch"), false);
        assert!(updated > 0, "epoch change must produce WAL frames");
        assert_eq!(
            probe_touch(&store, &keys, &format!("{label}_repeat"), false),
            0
        );
        // This side table is an experimental control, never a production migration.
        lock(&store.inner.connection).execute_batch(
            "CREATE TABLE probe_refs(full_key BLOB PRIMARY KEY, ref_epoch INTEGER NOT NULL) WITHOUT ROWID;
             INSERT INTO probe_refs SELECT full_key, ref_epoch FROM blob_payloads;",
        ).unwrap();
        let split = probe_touch(&store, &keys, &format!("{label}_split_control"), true);
        assert!(split > 0, "split control must also perform a real update");
    }
}

fn probe_inventory(path: &Path) {
    if path.is_dir() {
        let mut entries = std::fs::read_dir(path)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect::<Vec<_>>();
        entries.sort();
        for entry in entries {
            probe_inventory(&entry);
        }
    } else if path.extension().is_some_and(|ext| ext == "sqlite") {
        let connection =
            rusqlite::Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
                .unwrap();
        let objects: Vec<(String, u64)> = connection
            .prepare("SELECT name, SUM(pgsize) FROM dbstat GROUP BY name ORDER BY SUM(pgsize) DESC")
            .unwrap()
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        eprintln!(
            "IO_PROBE {}",
            serde_json::json!({
                "kind": "database", "path": path,
                "file_bytes": std::fs::metadata(path).unwrap().len(), "objects": objects,
            })
        );
    }
}

#[test]
#[ignore = "builds a real checkout's views; run single-threaded with isolated HOME/XDG"]
fn io_amplification_checkout_probe() {
    use crate::process_io::Bytes;
    use crate::views::assembly::{head_tree_fingerprint, publish_checkout, AssemblyRequest};
    use crate::write_ledger::{pending_for_test, Domain};
    use std::collections::{BTreeMap, BTreeSet};

    let base =
        PathBuf::from(std::env::var_os("AFT_IO_PROBE_DIR").expect("set isolated probe directory"));
    std::fs::create_dir_all(&base).unwrap();
    let root = PathBuf::from(std::env::var_os("AFT_IO_PROBE_ROOT").expect("set checkout root"))
        .canonicalize()
        .unwrap();
    let storage = base.join("checkout-store");
    assert!(!storage.exists(), "use an empty directory for a cold run");
    let mut request = AssemblyRequest {
        storage: storage.clone(),
        project_root: root.clone(),
        family: "io-probe".into(),
        scope: "probe-a".into(),
        desired_head: head_tree_fingerprint(&crate::alias::head_tree_entries(&root).unwrap()),
        changed_paths: BTreeSet::new(),
        semantic_keys: BTreeMap::new(),
        require_semantic: false,
        allow_blob_put: true,
        callgraph: true,
    };
    let root_id = root.display().to_string();
    for label in ["cold", "unchanged", "second_checkout"] {
        if label == "second_checkout" {
            request.scope = "probe-b".into();
        }
        let domains = [
            Domain::ViewsBlob,
            Domain::ViewsDerived,
            Domain::ViewsClosure,
        ];
        let before_domains = domains.map(|domain| pending_for_test(domain, &root_id));
        let before = Bytes::capture().unwrap();
        let started = std::time::Instant::now();
        let report = publish_checkout(&request).unwrap();
        let io = Bytes::capture().unwrap().delta(before).unwrap();
        assert!(report.pending_paths.is_empty());
        assert_eq!(report.published, label != "unchanged");
        for (domain, baseline) in domains.into_iter().zip(before_domains) {
            let after = pending_for_test(domain, &root_id);
            let delta = (after.0 - baseline.0, after.1 - baseline.1);
            eprintln!(
                "IO_PROBE {}",
                serde_json::json!({
                    "kind": "domain", "label": label, "domain": domain.as_str(),
                    "logical_bytes": delta.0, "physical_bytes": delta.1,
                })
            );
            if label == "cold" && domain == Domain::ViewsDerived {
                assert!(
                    delta.0 > 0,
                    "cold build must reach derived write accounting"
                );
            }
        }
        eprintln!(
            "IO_PROBE {}",
            serde_json::json!({
                "kind": "publication", "label": label, "io": io,
                "elapsed_ms": started.elapsed().as_millis(),
                "blob_puts": report.blob_puts, "generation": report.generation,
            })
        );
    }
    probe_inventory(&storage);
}

#[test]
#[ignore = "copies extracted callgraph payloads into an isolated v2 store for touch profiling"]
fn io_amplification_real_payload_touch_probe() {
    let base =
        PathBuf::from(std::env::var_os("AFT_IO_PROBE_DIR").expect("set isolated probe directory"));
    std::fs::create_dir_all(&base).unwrap();
    let input = PathBuf::from(
        std::env::var_os("AFT_IO_PROBE_BLOBS").expect("set extracted callgraph SQLite input"),
    );
    let source =
        rusqlite::Connection::open_with_flags(input, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .unwrap();
    let dir = tempfile::tempdir_in(&base).unwrap();
    let access = StoreWriteAccess::for_registered_view(dir.path(), "io-probe");
    let store = FamilyStore::open(&access, FamilyPlane::Callgraph).unwrap();
    let mut statement = source
        .prepare("SELECT full_key, payload FROM blob_payloads")
        .unwrap();
    let mut source_rows = statement.query([]).unwrap();
    while let Some(row) = source_rows.next().unwrap() {
        let key: Vec<u8> = row.get(0).unwrap();
        let payload: Vec<u8> = row.get(1).unwrap();
        store
            .put_or_touch(
                &FamilyKey::new(FamilyPlane::Callgraph, key.try_into().unwrap()),
                &payload,
            )
            .unwrap();
    }
    let rows = store.rows().unwrap();
    assert!(
        !rows.is_empty(),
        "input must contain real serialized callgraph payloads"
    );
    let mut sizes = rows.iter().map(|r| r.payload_bytes).collect::<Vec<_>>();
    sizes.sort_unstable();
    eprintln!(
        "IO_PROBE {}",
        serde_json::json!({
            "kind": "payload_distribution", "rows": sizes.len(),
            "total_bytes": sizes.iter().sum::<u64>(), "p50": sizes[sizes.len()/2],
            "p95": sizes[sizes.len()*95/100], "max": sizes.last(),
            "gc_epoch_before_touch": store.gc_epoch().unwrap(),
        })
    );
    let keys = rows.into_iter().map(|r| r.key).collect::<Vec<_>>();
    probe_touch(&store, &keys, "real_callgraph_same_epoch", false);
    store.raise_epoch(store.gc_epoch().unwrap() + 1).unwrap();
    probe_touch(&store, &keys, "real_callgraph_advanced_epoch", false);
    let registry = crate::views::registry::FamilyRegistry::open(dir.path(), "io-probe").unwrap();
    let epoch = store.gc_epoch().unwrap();
    let sweep = crate::gc::family::sweep_family_remeasured(
        &registry,
        None,
        crate::gc::family::FamilySweepPolicy {
            byte_budget: u64::MAX,
        },
        crate::gc::family::SweepBounds::default(),
        None,
    )
    .unwrap();
    assert_eq!(sweep.skipped, Some("nothing to collect"));
    assert_eq!(store.gc_epoch().unwrap(), epoch);
    probe_touch(
        &store,
        &keys,
        "real_callgraph_under_budget_after_preflight",
        false,
    );
    lock(&store.inner.connection)
        .execute_batch(
            "CREATE TABLE probe_refs(full_key BLOB PRIMARY KEY, ref_epoch INTEGER NOT NULL) WITHOUT ROWID;
             INSERT INTO probe_refs SELECT full_key, ref_epoch FROM blob_payloads;",
        )
        .unwrap();
    probe_touch(&store, &keys, "real_callgraph_split_control", true);

    // A zero-budget all-live pass must inspect references to establish that
    // collection is unnecessary; measure its manifest and SQLite read demand.
    use crate::views::manifest_v2::{
        EntryPlanes, EntryV2, GenerationName, ManifestHeader, ManifestReadProbe, ManifestV2,
        Producers, PublishV2,
    };
    use crate::views::readiness::PlaneState;
    use crate::views::RelPath;
    let checkout = dir.path().join("checkout");
    std::fs::create_dir_all(&checkout).unwrap();
    let view = registry
        .register_view_with_storage(dir.path(), "probe-live", &checkout)
        .unwrap();
    crate::views::eviction::wait_for_background_enforcement(&registry);
    let mut manifest = ManifestV2::new(ManifestHeader {
        producers: Producers {
            trigram: "trigram-v1".into(),
            semantic: None,
            callgraph: "callgraph-v1".into(),
        },
        head_tree: None,
        ignore_fingerprint: None,
        segment: None,
    });
    for (index, key) in keys.iter().enumerate() {
        manifest
            .insert(
                RelPath::new(format!("src/file-{index}.rs").into_bytes()).unwrap(),
                EntryV2::regular(
                    ContentHash::of(key.as_bytes()),
                    0,
                    EntryPlanes {
                        trigram: None,
                        semantic: None,
                        callgraph: Some(PlaneState::ready(key)),
                    },
                ),
            )
            .unwrap();
    }
    let view_store = view.view_store().unwrap();
    let name = GenerationName::for_manifest(&manifest).unwrap();
    let prepared = view_store.prepare_v2(&name, None, &manifest, None).unwrap();
    assert_eq!(
        view_store.commit_v2(prepared, None).unwrap(),
        PublishV2::Published
    );
    let manifests = ManifestReadProbe::start();
    let sqlite = ReadProbe::install(&lock(&store.inner.connection));
    let report = crate::gc::family::sweep_family_remeasured(
        &registry,
        None,
        crate::gc::family::FamilySweepPolicy { byte_budget: 0 },
        crate::gc::family::SweepBounds::default(),
        None,
    )
    .unwrap();
    let manifest_reads = manifests.finish();
    let loads = manifest_reads.values().map(|(loads, _)| loads).sum::<u64>();
    let bytes = manifest_reads.values().map(|(_, bytes)| bytes).sum::<u64>();
    eprintln!(
        "IO_PROBE {}",
        serde_json::json!({
            "kind": "all_live_sweep", "rows": keys.len(), "manifest_loads": loads,
            "manifest_read_bytes": bytes, "sqlite_main_read_bytes": sqlite.read_bytes(),
            "overflow_reads": sqlite.counts().1,
        })
    );
    assert_eq!(report.skipped, Some("nothing to collect"));
    assert_eq!(loads, 1, "skip cleanup must not reload the live manifest");
}
