//! Work-counting measurements and lock-in tests for the call-graph store's
//! cold build and incremental refresh.
//!
//! The counts (files parsed, resolution-config files read, SQL statements
//! compiled) are deterministic, unlike wall time on a shared machine, so the
//! lock-in tests assert on them. The ignored real-repository measurement also
//! writes the graph rows and query outputs to a directory, so two builds of
//! the store can be compared for identical results.

use super::*;
use crate::inspect::CallgraphSnapshot;
use rusqlite::ffi;
use std::fs;
use std::os::raw::{c_char, c_int, c_void};
use std::sync::atomic::AtomicUsize;

extern "C" fn count_statement_compilations(
    counter: *mut c_void,
    action: c_int,
    _: *const c_char,
    _: *const c_char,
    _: *const c_char,
    _: *const c_char,
) -> c_int {
    // SQLite calls the authorizer while it compiles a statement, never while
    // it runs one, so a statement served from the prepared-statement cache is
    // not counted. Only the top-level DML actions are counted, which gives one
    // count per compiled statement (a sub-select adds one more).
    if matches!(
        action,
        ffi::SQLITE_SELECT | ffi::SQLITE_INSERT | ffi::SQLITE_UPDATE | ffi::SQLITE_DELETE
    ) {
        // SAFETY: the pointer is a leaked `AtomicUsize` that lives for the
        // rest of the process (see `install_compile_counter`).
        unsafe { &*(counter as *const AtomicUsize) }.fetch_add(1, AtomicOrdering::Relaxed);
    }
    ffi::SQLITE_OK
}

/// Count every SQL statement compiled on the store's connection from now on.
pub(super) fn install_compile_counter(store: &CallGraphStore) -> &'static AtomicUsize {
    let counter: &'static AtomicUsize = Box::leak(Box::new(AtomicUsize::new(0)));
    let conn = store.conn.lock().expect("callgraph store mutex poisoned");
    // SAFETY: the handle belongs to a live connection and the callback's data
    // pointer is never freed.
    let rc = unsafe {
        ffi::sqlite3_set_authorizer(
            conn.handle(),
            Some(count_statement_compilations),
            counter as *const AtomicUsize as *mut c_void,
        )
    };
    assert_eq!(rc, ffi::SQLITE_OK, "install SQLite authorizer");
    counter
}

#[derive(Debug, Default, Clone, Copy)]
pub(super) struct WorkCounts {
    pub parses: usize,
    pub max_parses_of_one_file: usize,
    pub package_json_reads: usize,
    pub tsconfig_reads: usize,
    pub cargo_toml_reads: usize,
    pub config_reads: usize,
    pub statements_compiled: usize,
    pub staging_reads: usize,
    pub wall: Duration,
}

pub(super) fn measure<T>(
    root: &Path,
    compiles: &AtomicUsize,
    run: impl FnOnce() -> T,
) -> (T, WorkCounts) {
    work_counts::reset_under(root);
    let compiled_before = compiles.load(AtomicOrdering::Relaxed);
    let started = Instant::now();
    let value = run();
    let wall = started.elapsed();
    let counts = WorkCounts {
        parses: work_counts::parses_under(root),
        max_parses_of_one_file: work_counts::max_parses_of_one_file_under(root),
        package_json_reads: work_counts::config_reads_under(root, Some("package.json")),
        tsconfig_reads: work_counts::config_reads_under(root, Some("tsconfig.json")),
        cargo_toml_reads: work_counts::config_reads_under(root, Some("Cargo.toml")),
        config_reads: work_counts::config_reads_under(root, None),
        statements_compiled: compiles.load(AtomicOrdering::Relaxed) - compiled_before,
        staging_reads: work_counts::staging_reads_under(root),
        wall,
    };
    (value, counts)
}

const GRAPH_TABLES: &[&str] = &[
    "files",
    "nodes",
    "refs",
    "edges",
    "file_dependencies",
    "dispatch_hints",
    "type_ref_names",
    "backend_file_state",
    "resolution_config_fields",
];

/// Every graph row, sorted, with the fixture root replaced so two copies of
/// the same tree compare equal.
fn dump_graph_rows(store: &CallGraphStore) -> String {
    let root = store.project_root().display().to_string();
    let conn = store.conn.lock().expect("callgraph store mutex poisoned");
    let mut out = String::new();
    for table in GRAPH_TABLES {
        // Filesystem/build timestamps vary between identical copies of a tree;
        // graph evidence and freshness content identities must not vary.
        let columns = match *table {
            "files" => "path, content_hash, size, lang, is_dead_code_root, is_public_api, surface_fingerprint",
            "backend_file_state" => "backend, workspace_root, file_path, content_hash, status",
            _ => "*",
        };
        let mut statement = conn
            .prepare(&format!("SELECT {columns} FROM {table}"))
            .expect("prepare dump");
        let columns = statement.column_count();
        let mut rows = statement
            .query_map([], |row| {
                let mut values = Vec::with_capacity(columns);
                for index in 0..columns {
                    values.push(match row.get_ref(index)? {
                        rusqlite::types::ValueRef::Null => "NULL".to_string(),
                        rusqlite::types::ValueRef::Integer(value) => value.to_string(),
                        rusqlite::types::ValueRef::Real(value) => value.to_string(),
                        rusqlite::types::ValueRef::Text(value) => {
                            String::from_utf8_lossy(value).into_owned()
                        }
                        rusqlite::types::ValueRef::Blob(value) => format!("{value:?}"),
                    });
                }
                Ok(values.join("\u{1f}"))
            })
            .expect("query dump")
            .collect::<rusqlite::Result<Vec<_>>>()
            .expect("read dump");
        rows.sort();
        for row in rows {
            out.push_str(table);
            out.push('\t');
            out.push_str(&row.replace(&root, "<root>"));
            out.push('\n');
        }
    }
    out
}

#[test]
fn stat_first_restart_does_not_read_unchanged_sources() {
    let dir = tempfile::tempdir().unwrap();
    let root = fixture_root(&dir, "project");
    let files = write_scaling_fixture(&root, 8);
    let store_dir = dir.path().join("store");
    let (store, _) =
        CallGraphStore::cold_build_with_lease_chunked(store_dir.clone(), root.clone(), &files, 100)
            .unwrap();
    drop(store);

    let reader = CallGraphStore::open_readonly(store_dir, root.clone())
        .unwrap()
        .unwrap();
    let source_reads = crate::project_content::Probe::new(&root);
    let reads = crate::cache_freshness::watch_verify_stats(
        &root,
        crate::cache_freshness::VerifyScheduleForTest::SingleWorkerDrain,
    );
    let report = reader.reconcile_with_disk(1_000).unwrap();
    assert_eq!(report.changed_count(), 0);
    assert_eq!(report.examined_files, files.len());
    let observed = reads.take_stats();
    assert!(
        observed.hash_reads.is_empty(),
        "an unchanged restart read {} source files",
        observed.hash_reads.len()
    );
    assert_eq!(report.hashed_files, 0);
    assert!(
        source_reads.paths().is_empty(),
        "unchanged comparison opened source contents"
    );
}

#[test]
fn stat_first_legacy_store_hashes_once_then_records_source_stats() {
    let dir = tempfile::tempdir().unwrap();
    let root = fixture_root(&dir, "project");
    let files = write_scaling_fixture(&root, 8);
    let store_dir = dir.path().join("store");
    let (store, _) =
        CallGraphStore::cold_build_with_lease_chunked(store_dir.clone(), root.clone(), &files, 100)
            .unwrap();
    let sqlite_path = store.sqlite_path().to_path_buf();
    let graph_before = dump_graph_rows(&store);
    let revision_before = store.projection_write_revision().unwrap();
    drop(store);
    let conn = Connection::open(&sqlite_path).unwrap();
    let has_identity = conn
        .prepare("PRAGMA table_info(files)")
        .unwrap()
        .query_map([], |row| row.get::<_, String>(1))
        .unwrap()
        .any(|column| column.unwrap() == "source_identity");
    if has_identity {
        conn.execute_batch("ALTER TABLE files DROP COLUMN source_identity")
            .unwrap();
    }
    drop(conn);

    let source_reads = crate::project_content::Probe::new(&root);
    let reads = crate::cache_freshness::watch_verify_stats(
        &root,
        crate::cache_freshness::VerifyScheduleForTest::SingleWorkerDrain,
    );
    let reader = CallGraphStore::open_readonly(store_dir.clone(), root.clone())
        .unwrap()
        .unwrap();
    let first = reader.reconcile_with_disk(1_000).unwrap();
    assert_eq!(first.changed_count(), 0);
    assert_eq!(first.hashed_files, files.len());
    assert_eq!(reads.take_stats().hash_reads.len(), files.len());
    assert_eq!(source_reads.paths().len(), files.len());
    drop(reader);

    let reader = CallGraphStore::open_readonly(store_dir.clone(), root.clone())
        .unwrap()
        .unwrap();
    let second = reader.reconcile_with_disk(1_000).unwrap();
    assert_eq!(second.changed_count(), 0);
    assert_eq!(
        second.hashed_files, 0,
        "the legacy fallback must persist its validated stat observations"
    );
    assert!(reads.take_stats().hash_reads.is_empty());
    assert_eq!(
        source_reads.paths().len(),
        files.len(),
        "second comparison opened source contents again"
    );
    drop(reader);
    let store = CallGraphStore::open(store_dir, root).unwrap();
    assert_eq!(store.projection_write_revision().unwrap(), revision_before);
    assert_eq!(
        dump_graph_rows(&store),
        graph_before,
        "stat backfill must not change graph rows"
    );
}

#[test]
#[cfg(any(unix, windows))]
fn stat_first_refresh_records_replacement_identity_without_graph_churn() {
    let dir = tempfile::tempdir().unwrap();
    let root = fixture_root(&dir, "project");
    let source = "export function before() {}\n";
    let path = write(&root, "source.ts", source);
    let store_dir = dir.path().join("store");
    let (store, _) = CallGraphStore::cold_build_with_lease_chunked(
        store_dir.clone(),
        root.clone(),
        std::slice::from_ref(&path),
        100,
    )
    .unwrap();
    let revision = store.projection_write_revision().unwrap();
    crate::file_identity::replace_keeping_times(&path, source.as_bytes());
    store.refresh_files(std::slice::from_ref(&path)).unwrap();
    assert_eq!(
        store.projection_write_revision().unwrap(),
        revision,
        "a new identity with equal contents is metadata-only"
    );
    drop(store);

    let reader = CallGraphStore::open_readonly(store_dir.clone(), root.clone())
        .unwrap()
        .unwrap();
    let source_reads = crate::project_content::Probe::new(&root);
    assert_eq!(reader.reconcile_with_disk(1_000).unwrap().hashed_files, 0);
    assert!(source_reads.paths().is_empty());
    drop(reader);
    drop(source_reads);

    let store = CallGraphStore::open(store_dir.clone(), root.clone()).unwrap();
    crate::file_identity::replace_keeping_times(
        &path,
        source.replace("before", "after_").as_bytes(),
    );
    store.refresh_files(std::slice::from_ref(&path)).unwrap();
    drop(store);
    let reader = CallGraphStore::open_readonly(store_dir, root.clone())
        .unwrap()
        .unwrap();
    let source_reads = crate::project_content::Probe::new(&root);
    let report = reader.reconcile_with_disk(1_000).unwrap();
    assert_eq!(report.changed_count(), 0);
    assert_eq!(report.hashed_files, 0);
    assert!(
        source_reads.paths().is_empty(),
        "refresh must record the stat of the newly extracted contents"
    );
}

#[test]
#[cfg(any(unix, windows))]
fn stat_first_reconcile_rerecords_changed_identity_with_equal_contents() {
    let dir = tempfile::tempdir().unwrap();
    let root = fixture_root(&dir, "project");
    let source = "export function source() {}\n";
    let path = write(&root, "source.ts", source);
    let store_dir = dir.path().join("store");
    let (store, _) = CallGraphStore::cold_build_with_lease_chunked(
        store_dir.clone(),
        root.clone(),
        std::slice::from_ref(&path),
        100,
    )
    .unwrap();
    let revision = store.projection_write_revision().unwrap();
    drop(store);
    crate::file_identity::replace_keeping_times(&path, source.as_bytes());
    let reader = CallGraphStore::open_readonly(store_dir.clone(), root.clone())
        .unwrap()
        .unwrap();
    let source_reads = crate::project_content::Probe::new(&root);
    let report = reader.reconcile_with_disk(1_000).unwrap();
    assert_eq!(report.changed_count(), 0);
    assert_eq!(report.hashed_files, 1);
    assert_eq!(source_reads.paths(), vec![path]);
    assert_eq!(reader.projection_write_revision().unwrap(), revision);
    drop(reader);

    let reader = CallGraphStore::open_readonly(store_dir, root)
        .unwrap()
        .unwrap();
    assert_eq!(reader.reconcile_with_disk(1_000).unwrap().hashed_files, 0);
    assert_eq!(
        source_reads.paths().len(),
        1,
        "the new machine-local identity must be persisted"
    );
}

#[test]
fn stat_first_backfill_defers_to_a_live_writer_without_rebuilding() {
    let dir = tempfile::tempdir().unwrap();
    let root = fixture_root(&dir, "project");
    let files = write_scaling_fixture(&root, 4);
    let store_dir = dir.path().join("store");
    let (store, _) =
        CallGraphStore::cold_build_with_lease_chunked(store_dir.clone(), root.clone(), &files, 100)
            .unwrap();
    let sqlite = store.sqlite_path().to_path_buf();
    drop(store);
    let conn = Connection::open(&sqlite).unwrap();
    conn.execute("UPDATE files SET source_identity = NULL", [])
        .unwrap();
    drop(conn);
    let held = crate::fs_lock::try_acquire(
        &crate::root_cache::writer_lease_path(&store_dir),
        Duration::ZERO,
    )
    .unwrap();
    let reader = CallGraphStore::open_readonly(store_dir.clone(), root.clone())
        .unwrap()
        .unwrap();
    let report = reader.reconcile_with_disk(1_000).unwrap();
    assert_eq!(report.changed_count(), 0);
    assert_eq!(report.hashed_files, files.len());
    assert_eq!(reader.sqlite_path(), sqlite);
    assert!(
        held.verify_writer_epoch().unwrap(),
        "a stat backfill must never steal a live lease"
    );
    let conn = Connection::open(&sqlite).unwrap();
    let missing: usize = conn
        .query_row(
            "SELECT COUNT(*) FROM files WHERE source_identity IS NULL",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(missing, files.len());
    drop(conn);
    drop(held);
    assert_eq!(
        reader.reconcile_with_disk(1_000).unwrap().hashed_files,
        files.len()
    );
    drop(reader);
    let reader = CallGraphStore::open_readonly(store_dir, root)
        .unwrap()
        .unwrap();
    assert_eq!(reader.reconcile_with_disk(1_000).unwrap().hashed_files, 0);
}

#[test]
#[cfg(any(unix, windows))]
fn stat_first_reconcile_detects_mtime_preserving_replacement_by_identity() {
    let dir = tempfile::tempdir().unwrap();
    let root = fixture_root(&dir, "project");
    let source = "export function caller() { before(); }\nfunction before() {}\n";
    let path = write(&root, "caller.ts", source);
    let store_dir = dir.path().join("store");
    let (store, _) = CallGraphStore::cold_build_with_lease_chunked(
        store_dir.clone(),
        root.clone(),
        std::slice::from_ref(&path),
        100,
    )
    .unwrap();
    drop(store);
    let (old_stat, old_identity) = crate::file_identity::metadata(&path).unwrap();
    crate::file_identity::replace_keeping_times(
        &path,
        source.replace("before", "after_").as_bytes(),
    );
    let (new_stat, new_identity) = crate::file_identity::metadata(&path).unwrap();
    assert_eq!(old_stat.len(), new_stat.len());
    assert_eq!(old_stat.modified().unwrap(), new_stat.modified().unwrap());
    assert_ne!(
        old_identity, new_identity,
        "the replacement changed file identity, not mtime or size"
    );

    let reader = CallGraphStore::open_readonly(store_dir, root.clone())
        .unwrap()
        .unwrap();
    let source_reads = crate::project_content::Probe::new(&root);
    let reads = crate::cache_freshness::watch_verify_stats(
        &root,
        crate::cache_freshness::VerifyScheduleForTest::SingleWorkerDrain,
    );
    let report = reader.reconcile_with_disk(1_000).unwrap();
    assert_eq!(report.modified, vec![path]);
    assert_eq!(report.hashed_files, 1);
    assert_eq!(reads.take_stats().hash_reads.len(), 1);
    assert_eq!(source_reads.paths().len(), 1);
}

/// Save a deterministic corpus before an I/O change, then compare the logical
/// rows and public graph walks against that saved baseline after the change.
#[test]
#[ignore = "measurement: AFT_CALLGRAPH_IO_PARITY names the baseline dump"]
fn callgraph_io_fixture_equivalence() {
    let dump = PathBuf::from(std::env::var("AFT_CALLGRAPH_IO_PARITY").expect("baseline dump"));
    fs::create_dir_all(dump.parent().unwrap()).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let root = fixture_root(&dir, "project");
    let files = write_scaling_fixture(&root, 512);
    let (store, _) =
        CallGraphStore::cold_build_with_lease_chunked(dir.path().join("store"), root, &files, 100)
            .unwrap();
    let rows = dump_graph_rows(&store);
    let queries = dump_query_outputs(&store, usize::MAX);
    for (name, value) in [("rows", rows), ("queries", queries)] {
        let path = dump.with_extension(name);
        if std::env::var_os("AFT_CALLGRAPH_IO_PARITY_RECORD").is_some() {
            fs::write(&path, &value).unwrap();
        } else {
            assert_eq!(fs::read_to_string(&path).unwrap(), value, "{name} changed");
        }
        eprintln!(
            "parity {name}: lines={} blake3={}",
            value.lines().count(),
            blake3::hash(value.as_bytes())
        );
    }
}

fn vm_steps(conn: &Connection, run: impl FnOnce()) -> usize {
    struct Reset<'a>(&'a Connection);
    impl Drop for Reset<'_> {
        fn drop(&mut self) {
            // Remove the callback before its stack-owned counter disappears,
            // including when an assertion in the measured closure panics.
            unsafe {
                ffi::sqlite3_progress_handler(self.0.handle(), 0, None, std::ptr::null_mut());
            }
        }
    }
    unsafe extern "C" fn count(context: *mut c_void) -> c_int {
        // SQLite invokes the callback synchronously while the counter is alive.
        unsafe { &*(context.cast::<AtomicUsize>()) }.fetch_add(1, AtomicOrdering::Relaxed);
        0
    }
    let steps = AtomicUsize::new(0);
    unsafe {
        ffi::sqlite3_progress_handler(
            conn.handle(),
            1,
            Some(count),
            (&steps as *const AtomicUsize).cast_mut().cast(),
        );
    }
    let reset = Reset(conn);
    run();
    drop(reset);
    steps.load(AtomicOrdering::Relaxed)
}

#[test]
fn callgraph_io_status_scans_visit_only_nonfresh_rows() {
    let conn = Connection::open_in_memory().unwrap();
    initialize_schema(&conn).unwrap();
    conn.execute_batch("WITH RECURSIVE n(x) AS (VALUES(1) UNION ALL SELECT x+1 FROM n WHERE x<60000)
        INSERT INTO backend_file_state(backend,workspace_root,file_path,content_hash,status,updated_at)
        SELECT 'treesitter','/project',printf('file%d.ts',x),'hash','fresh',0 FROM n;").unwrap();
    conn.execute_batch(
        "INSERT INTO backend_file_state VALUES
        ('treesitter','/project','stale.ts','hash','stale',0),
        ('treesitter','/project','binary.ts','hash','undecodable',0)",
    )
    .unwrap();
    let root = Path::new("/project");
    let steps = vm_steps(&conn, || {
        assert_eq!(
            stale_backend_file_paths(&conn, root, true).unwrap(),
            vec!["stale.ts"]
        );
        assert_eq!(
            backend_file_paths_with_status(&conn, root, BACKEND_STATUS_UNDECODABLE).unwrap(),
            vec!["binary.ts"]
        );
    });
    assert!(
        steps < 200,
        "status scans executed {steps} VM steps for fresh rows"
    );
}

#[test]
fn callgraph_io_root_lookup_uses_meta() {
    let dir = tempfile::tempdir().unwrap();
    let root = fixture_root(&dir, "project");
    let files = write_scaling_fixture(&root, 8);
    let store = CallGraphStore::open(dir.path().join("store"), root.clone()).unwrap();
    store.cold_build(&files).unwrap();
    let conn = store.conn.lock().unwrap();
    let steps = vm_steps(&conn, || {
        assert_eq!(
            stored_workspace_roots(&conn).unwrap(),
            vec![root.display().to_string()]
        );
    });
    assert!(
        steps < 30,
        "root lookup executed {steps} VM steps instead of reading meta"
    );
}

#[test]
fn callgraph_io_legacy_root_scan_is_cached_once_and_repair_updates_meta() {
    let dir = tempfile::tempdir().unwrap();
    let root = fixture_root(&dir, "project");
    let old_root = dir.path().join("moved-away").display().to_string();
    let mut conn = Connection::open_in_memory().unwrap();
    initialize_schema(&conn).unwrap();
    conn.execute(
        "INSERT INTO backend_file_state VALUES('treesitter',?1,'file.ts','hash','fresh',0)",
        [&old_root],
    )
    .unwrap();
    assert_eq!(stored_workspace_roots(&conn).unwrap(), vec![old_root]);
    assert!(
        vm_steps(&conn, || {
            stored_workspace_roots(&conn).unwrap();
        }) < 30
    );
    assert!(matches!(
        reconcile_workspace_roots(&mut conn, &root, true).unwrap(),
        OpenRootRepair::ReRooted
    ));
    assert_eq!(
        stored_workspace_roots(&conn).unwrap(),
        vec![root.display().to_string()]
    );
}

#[test]
fn callgraph_io_extraction_commits_by_bytes_not_chunk_files() {
    let dir = tempfile::tempdir().unwrap();
    let root = fixture_root(&dir, "project");
    let files = write_scaling_fixture(&root, 300);
    let store = CallGraphStore::open(dir.path().join("store"), root).unwrap();
    let batches = Arc::new(std::sync::Mutex::new(Vec::new()));
    let observed = Arc::clone(&batches);
    set_cold_build_extract_observer(Some(Arc::new(move |paths| {
        observed.lock().unwrap().push(paths.len())
    })));
    let result = store.cold_build_chunked(&files, 100);
    set_cold_build_extract_observer(None);
    result.unwrap();
    assert_eq!(*batches.lock().unwrap(), vec![300]);
}

#[test]
fn callgraph_io_extraction_residency_is_bounded_inside_bulk_commits() {
    fn heap_in_use() -> usize {
        #[cfg(all(target_os = "linux", target_env = "gnu"))]
        {
            // mallinfo2 is thread-safe and takes no pointer arguments.
            unsafe { libc::mallinfo2().uordblks }
        }
        #[cfg(not(all(target_os = "linux", target_env = "gnu")))]
        {
            0
        }
    }

    let dir = tempfile::tempdir().unwrap();
    let root = fixture_root(&dir, "project");
    let source = format!(
        "function leaf() {{}}\nexport function run() {{\n{}}}\n",
        "  leaf();\n".repeat(128)
    );
    let files = (0..300)
        .map(|index| write(&root, &format!("f{index:03}.ts"), &source))
        .collect::<Vec<_>>();
    let store = CallGraphStore::open(dir.path().join("store"), root).unwrap();
    let commit_samples = Arc::new(std::sync::Mutex::new((None, None)));
    let observed_commits = Arc::clone(&commit_samples);
    let project_key = store.project_key().to_owned();
    set_cold_build_slice_observer(Some(Arc::new(move |stage, completed, total| {
        if stage != "extraction" {
            return;
        }
        let mut samples = observed_commits.lock().unwrap();
        if completed == 0 && samples.0.is_none() {
            samples.0 = Some(callgraph_write_metrics_for_project(&project_key).commits_60s);
        } else if completed == total && samples.1.is_none() {
            samples.1 = Some(callgraph_write_metrics_for_project(&project_key).commits_60s);
        }
    })));
    let batches = Arc::new(std::sync::Mutex::new(Vec::new()));
    let observed = Arc::clone(&batches);
    set_cold_build_extract_observer(Some(Arc::new(move |paths| {
        observed.lock().unwrap().push(paths.len());
    })));
    let residents = Arc::new(std::sync::Mutex::new(Vec::new()));
    let observed = Arc::clone(&residents);
    let heap_before = heap_in_use();
    set_cold_build_resident_extract_observer(Some(Arc::new(move |count| {
        observed.lock().unwrap().push((count, heap_in_use()));
    })));
    let result = store.cold_build_chunked(&files, 100);
    set_cold_build_extract_observer(None);
    set_cold_build_resident_extract_observer(None);
    set_cold_build_slice_observer(None);
    result.unwrap();

    let residents = residents.lock().unwrap();
    let peak_residents = residents.iter().map(|(count, _)| *count).max().unwrap();
    let peak_heap = residents.iter().map(|(_, bytes)| *bytes).max().unwrap();
    eprintln!(
        "extraction residency: peak_extracts={peak_residents} heap_before={heap_before} \
         peak_glibc_heap_in_use={peak_heap} heap_delta={}",
        peak_heap.saturating_sub(heap_before)
    );
    // One database commit can cover all 300 files without keeping their parsed
    // graphs in memory together.
    assert_eq!(*batches.lock().unwrap(), vec![300]);
    let (before, after) = *commit_samples.lock().unwrap();
    assert_eq!(after.unwrap().checked_sub(before.unwrap()), Some(1));
    assert!(
        peak_residents <= 16,
        "resident extracts {peak_residents} exceeded the 16-file cold-build residency bound"
    );
}

fn tiny_file_fixture(root: &Path) -> Vec<PathBuf> {
    (0..COLD_BUILD_EXTRACT_MAX_FILES + 17)
        .map(|index| {
            write(
                root,
                &format!("tiny{index:05}.ts"),
                if index % 2 == 0 { "" } else { "// tiny\n" },
            )
        })
        .collect()
}

#[test]
fn callgraph_io_tiny_file_batches_have_finite_file_cap() {
    let dir = tempfile::tempdir().unwrap();
    let root = fixture_root(&dir, "project");
    let files = tiny_file_fixture(&root);
    let source_bytes: u64 = files
        .iter()
        .map(|path| fs::metadata(path).unwrap().len())
        .sum();
    assert!(source_bytes < COLD_BUILD_EXTRACT_BATCH_BYTES);
    let store = CallGraphStore::open(dir.path().join("store"), root).unwrap();
    let batches = Arc::new(std::sync::Mutex::new(Vec::new()));
    let observed = Arc::clone(&batches);
    set_cold_build_extract_observer(Some(Arc::new(move |paths| {
        observed.lock().unwrap().push(paths.len());
    })));
    let result = store.cold_build_chunked(&files, 100);
    set_cold_build_extract_observer(None);
    result.unwrap();
    assert_eq!(
        *batches.lock().unwrap(),
        vec![COLD_BUILD_EXTRACT_MAX_FILES, 17]
    );
    assert_eq!(store.indexed_file_count().unwrap(), files.len());
}

#[test]
fn callgraph_io_tiny_file_cancellation_stops_after_one_capped_batch() {
    let dir = tempfile::tempdir().unwrap();
    let root = fixture_root(&dir, "project");
    let files = tiny_file_fixture(&root);
    let store = CallGraphStore::open(dir.path().join("store"), root).unwrap();
    let epoch = crate::root_cache::ArtifactPublishEpoch::default();
    let expected = epoch.current();
    let cancel_epoch = epoch.clone();
    let batches = Arc::new(std::sync::Mutex::new(Vec::new()));
    let observed = Arc::clone(&batches);
    set_cold_build_extract_observer(Some(Arc::new(move |paths| {
        observed.lock().unwrap().push(paths.len());
        // Supersede the build after batch admission. It may finish this batch,
        // but must stop before retaining extracts for the next one.
        cancel_epoch.next();
    })));
    let result = with_publish_epoch(epoch, expected, || store.cold_build_chunked(&files, 100));
    set_cold_build_extract_observer(None);
    assert!(matches!(result, Err(CallGraphStoreError::Superseded)));
    assert_eq!(*batches.lock().unwrap(), vec![COLD_BUILD_EXTRACT_MAX_FILES]);
    let conn = store.conn.lock().unwrap();
    assert_eq!(
        query_count(&conn, "SELECT COUNT(*) FROM files").unwrap(),
        COLD_BUILD_EXTRACT_MAX_FILES as u64
    );
    assert_eq!(
        staged_build_phase(&conn).unwrap().as_deref(),
        Some("extracting")
    );
}

#[test]
fn callgraph_io_build_cache_matches_bounded_reader_budget() {
    let conn = TrackedConnection::open_in_memory(SqliteStore::CallgraphColdGeneration).unwrap();
    configure_build_connection(&conn).unwrap();
    let cache: i64 = conn
        .pragma_query_value(None, "cache_size", |row| row.get(0))
        .unwrap();
    assert_eq!(cache, -8192);
    configure_connection(&conn).unwrap();
    let cache: i64 = conn
        .pragma_query_value(None, "cache_size", |row| row.get(0))
        .unwrap();
    assert_eq!(cache, -8192);
}

#[test]
fn callgraph_io_refresh_reuses_connection_and_releases_it_on_eviction() {
    let dir = tempfile::tempdir().unwrap();
    let root = fixture_root(&dir, "project");
    let files = write_scaling_fixture(&root, 3);
    let store_dir = dir.path().join("store");
    let (store, _) =
        CallGraphStore::cold_build_with_lease(store_dir.clone(), root.clone(), &files).unwrap();
    let path = store.sqlite_path().to_path_buf();
    drop(store);
    let worker = isolated_callgraph_refresh_worker_for_test(root.clone());
    let pending = PendingCallGraphStorePaths::default();
    let key = RefreshRoot {
        callgraph_dir: store_dir.clone(),
        project_root: root.clone(),
    };
    for attempt in 0..2 {
        fs::write(
            &files[0],
            format!("export function edited{attempt}() {{}}\n"),
        )
        .unwrap();
        assert!(enqueue_callgraph_store_refresh(
            store_dir.clone(),
            root.clone(),
            vec![files[0].clone()],
            pending.clone()
        ));
        assert!(worker
            .worker
            .wait_for_root_with_budget(&root, Duration::from_secs(15)));
        let stores = worker.worker.shared.stores.lock().unwrap();
        let store = &stores
            .get(&key)
            .expect("idle refresh connection retained")
            .store;
        assert!(
            store.writer_lease.is_none(),
            "idle connection must not retain the writer lease"
        );
        let conn = store.conn.lock().unwrap();
        if attempt == 0 {
            conn.execute_batch(
                "CREATE TEMP TABLE reuse_sentinel(value); INSERT INTO reuse_sentinel VALUES(746)",
            )
            .unwrap();
        } else {
            assert_eq!(
                conn.query_row("SELECT value FROM reuse_sentinel", [], |row| row
                    .get::<_, i64>(0))
                    .unwrap(),
                746
            );
        }
    }
    close_refresh_connections(&root);
    assert_eq!(
        crate::db::file_identity::open_connections(&path),
        0,
        "eviction must close the connection and WAL mapping"
    );
    assert!(enqueue_callgraph_store_refresh(
        store_dir.clone(),
        root.clone(),
        vec![files[0].clone()],
        pending
    ));
    assert!(worker
        .worker
        .wait_for_root_with_budget(&root, Duration::from_secs(15)));
    let (next, _) = CallGraphStore::force_cold_build_with_lease_chunked(
        store_dir.clone(),
        root.clone(),
        &files,
        100,
    )
    .unwrap();
    let next_path = next.sqlite_path().to_path_buf();
    assert_ne!(path, next_path);
    drop(next);
    assert!(enqueue_callgraph_store_refresh(
        store_dir,
        root.clone(),
        vec![files[0].clone()],
        PendingCallGraphStorePaths::default()
    ));
    assert!(worker
        .worker
        .wait_for_root_with_budget(&root, Duration::from_secs(15)));
    assert_eq!(
        crate::db::file_identity::open_connections(&path),
        0,
        "superseded generation must be closed before reusing a writer"
    );
    assert_eq!(
        worker.worker.shared.stores.lock().unwrap()[&key]
            .store
            .sqlite_path(),
        next_path
    );
    drop(worker);
    assert_eq!(
        crate::db::file_identity::open_connections(&next_path),
        0,
        "worker shutdown must close cached connections"
    );
}

#[test]
fn callgraph_io_rebind_retires_the_previous_roots_connection() {
    let dir = tempfile::tempdir().unwrap();
    let root = fixture_root(&dir, "previous-root");
    let files = write_scaling_fixture(&root, 3);
    let store_dir = dir.path().join("store");
    let (store, _) =
        CallGraphStore::cold_build_with_lease(store_dir.clone(), root.clone(), &files).unwrap();
    let path = store.sqlite_path().to_path_buf();
    drop(store);
    let worker = isolated_callgraph_refresh_worker_for_test(root.clone());
    let generation = Arc::new(std::sync::atomic::AtomicU64::new(1));
    let epoch = crate::root_cache::ArtifactPublishEpoch::default();
    for publication in [false, true] {
        let lifecycle = SubcLifecycleAdmission::default();
        let ticket = CallgraphRefreshTicket::new(
            lifecycle.clone(),
            Arc::clone(&generation),
            generation.load(AtomicOrdering::SeqCst),
            epoch.clone(),
            epoch.current(),
        );
        assert!(enqueue_callgraph_store_refresh_fenced(
            store_dir.clone(),
            root.clone(),
            vec![files[0].clone()],
            PendingCallGraphStorePaths::default(),
            ticket
        ));
        assert!(worker
            .worker
            .wait_for_root_with_budget(&root, Duration::from_secs(15)));
        assert_eq!(crate::db::file_identity::open_connections(&path), 1);
        if publication {
            epoch.next();
        } else {
            generation.fetch_add(1, AtomicOrdering::SeqCst);
        }
        // No path from the replacement configure is needed to retire the old
        // root's connection, even when configure switches canonical roots.
        retire_obsolete_callgraph_refresh_waiters();
        assert_eq!(crate::db::file_identity::open_connections(&path), 0);
    }
}

/// `callers`, `call_tree` and `trace_to` output for an evenly spaced sample
/// of the stored symbols.
fn dump_query_outputs(store: &CallGraphStore, samples: usize) -> String {
    let root = store.project_root().display().to_string();
    let symbols = {
        let conn = store.conn.lock().expect("callgraph store mutex poisoned");
        let mut statement = conn
            .prepare(
                "SELECT DISTINCT file_path, scoped_name FROM nodes ORDER BY file_path, scoped_name",
            )
            .expect("prepare symbols");
        statement
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })
            .expect("query symbols")
            .collect::<rusqlite::Result<Vec<_>>>()
            .expect("read symbols")
    };
    let step = (symbols.len() / samples.max(1)).max(1);
    let mut out = String::new();
    for (file, symbol) in symbols.iter().step_by(step) {
        let path = Path::new(file);
        out.push_str(&format!("== {file} {symbol}\n"));
        out.push_str(&format!(
            "callers: {:?}\n",
            store.callers_of(path, symbol, 2)
        ));
        out.push_str(&format!(
            "call_tree: {:?}\n",
            store.call_tree(path, symbol, 2)
        ));
        out.push_str(&format!(
            "trace_to: {:?}\n",
            store.trace_to(path, symbol, 3)
        ));
    }
    out.replace(&root, "<root>")
}

pub(super) fn copy_tracked_sources(source: &Path, destination: &Path) {
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(source)
        .args(["ls-files", "-z"])
        .output()
        .expect("git ls-files");
    assert!(output.status.success(), "git ls-files failed");
    for rel in output.stdout.split(|byte| *byte == 0) {
        if rel.is_empty() {
            continue;
        }
        let rel = Path::new(std::str::from_utf8(rel).expect("utf-8 path"));
        let wanted = crate::parser::detect_language(rel).is_some()
            || resolution_config::resolution_config_kind(rel).is_some()
            || rel
                .file_name()
                .is_some_and(|name| name == ".gitignore" || name == ".aftignore");
        let from = source.join(rel);
        if !wanted || !from.is_file() || from.is_symlink() {
            continue;
        }
        let to = destination.join(rel);
        fs::create_dir_all(to.parent().expect("parent")).expect("create parent");
        fs::copy(&from, &to).expect("copy source file");
    }
}

fn report(label: &str, counts: &WorkCounts) {
    eprintln!(
        "{label}: parses={} max_parses_of_one_file={} package_json_reads={} tsconfig_reads={} cargo_toml_reads={} config_reads={} statements_compiled={} staging_reads={} wall_ms={}",
        counts.parses,
        counts.max_parses_of_one_file,
        counts.package_json_reads,
        counts.tsconfig_reads,
        counts.cargo_toml_reads,
        counts.config_reads,
        counts.statements_compiled,
        counts.staging_reads,
        counts.wall.as_millis()
    );
}

/// Frozen tracked inputs and sorted logical rows follow the extraction-work
/// census, but include both storage paths and the dead-code consumer this time.
#[test]
#[ignore = "measurement: needs AFT_CALLGRAPH_MEASURE_ROOT and AFT_CALLGRAPH_MEASURE_DUMP"]
fn perf_audit2_real_repo_output_equivalence() {
    use crate::views::{Manifest, ManifestEntry, RegularPlanes, RelPath};
    let source = PathBuf::from(std::env::var("AFT_CALLGRAPH_MEASURE_ROOT").expect("root"));
    let dump = PathBuf::from(std::env::var("AFT_CALLGRAPH_MEASURE_DUMP").expect("dump"));
    let dir = tempfile::tempdir().unwrap();
    let root = fixture_root(&dir, "project");
    copy_tracked_sources(&source, &root);
    let files = callgraph::walk_project_files(&root).collect::<Vec<_>>();
    fs::create_dir_all(&dump).unwrap();
    let (store, counts) = cold_build_counts(&root, &dir.path().join("store"), &files);
    report("real_repo_cold", &counts);
    fs::write(dump.join("legacy_rows.txt"), dump_graph_rows(&store)).unwrap();
    let snapshot = project_dead_code_snapshot(store.sqlite_path()).unwrap();
    fs::write(
        dump.join("legacy_dead_code.txt"),
        dump_dead_code(&root, &files, snapshot),
    )
    .unwrap();

    let blobs_path = dir.path().join("blobs.sqlite");
    let blobs = Connection::open(&blobs_path).unwrap();
    blobs
        .execute_batch(
            "CREATE TABLE blob_payloads(full_key BLOB PRIMARY KEY, payload BLOB NOT NULL)",
        )
        .unwrap();
    let mut entries = Vec::new();
    let inputs = std::process::Command::new("git")
        .arg("-C")
        .arg(&source)
        .args(["ls-files", "-z"])
        .output()
        .unwrap();
    assert!(inputs.status.success());
    for rel in inputs
        .stdout
        .split(|byte| *byte == 0)
        .filter(|rel| !rel.is_empty())
    {
        let path = root.join(std::str::from_utf8(rel).unwrap());
        if !path.is_file() {
            continue;
        }
        let bytes = fs::read(&path).unwrap();
        let config = resolution_config::resolution_config_kind(&path).is_some()
            || path
                .file_name()
                .is_some_and(|name| name == ".gitignore" || name == ".aftignore");
        let blob = if config {
            join::CallgraphBlob::config(bytes, "ruled-callgraph-v2")
        } else if let Some(lang) = crate::parser::detect_language(&path) {
            join::CallgraphBlob::extract(
                std::str::from_utf8(&bytes).unwrap(),
                lang_label(lang),
                "ruled-callgraph-v2",
            )
            .unwrap()
        } else {
            continue;
        };
        let payload = blob.to_bytes().unwrap();
        let key = blake3::hash(&payload);
        blobs
            .execute(
                "INSERT OR IGNORE INTO blob_payloads VALUES (?1, ?2)",
                params![key.as_bytes().as_slice(), payload],
            )
            .unwrap();
        entries.push((
            RelPath::new(rel).unwrap(),
            ManifestEntry::Regular {
                mode: 0o100644,
                planes: RegularPlanes {
                    semantic: None,
                    callgraph: Some(key.to_hex().to_string()),
                },
                resolution_input: config,
            },
        ));
    }
    let manifest = Manifest::new(entries).unwrap();
    let views = dir.path().join("views");
    fs::create_dir_all(&views).unwrap();
    let generation = "audit2";
    let database = manifest_view_database_path(&views, generation).unwrap();
    crate::views::materialization::materialize_manifest_view_database(
        &database,
        &blobs_path,
        &manifest,
    )
    .unwrap();
    let mut rows = crate::views::materialization::parity::logical_snapshot(&database);
    // Operational metadata contains build timestamps, not graph evidence.
    rows.remove("meta");
    fs::write(
        dump.join("view_rows.json"),
        serde_json::to_vec(&rows).unwrap(),
    )
    .unwrap();
    let view = ReadonlyCallGraphStore::open_manifest_view(
        root.clone(),
        "audit2".into(),
        views,
        generation,
        None,
    )
    .unwrap();
    let (_, snapshot, _, _) = project_dead_code_snapshot_from_view(&view).unwrap();
    fs::write(
        dump.join("view_dead_code.txt"),
        dump_dead_code(&root, &files, snapshot),
    )
    .unwrap();
    eprintln!(
        "equivalence corpus: files={} legacy_edges={} view_edges={} dispatch_sites={} artifacts={}",
        files.len(),
        store.edge_snapshot().unwrap().len(),
        rows["edges"].len(),
        rows["view_dispatch_sites"].len(),
        dump.display()
    );
}

fn dump_dead_code(root: &Path, files: &[PathBuf], mut snapshot: CallgraphSnapshot) -> String {
    use crate::inspect::{InspectCategory, InspectJob, JobKey};
    snapshot.generated_at = None;
    let projection = format!("{snapshot:?}");
    let job = InspectJob {
        job_id: 1,
        key: JobKey::for_project_category(InspectCategory::DeadCode),
        category: InspectCategory::DeadCode,
        scope_files: files.to_vec(),
        project_root: root.to_path_buf(),
        inspect_dir: root.join(".inspect"),
        config: Arc::new(crate::config::Config {
            project_root: Some(root.to_path_buf()),
            ..Default::default()
        }),
        symbol_cache: Arc::new(std::sync::RwLock::new(crate::parser::SymbolCache::new())),
        inspect_writer: true,
        callgraph_writer: true,
        callgraph_snapshot: Some(Arc::new(snapshot)),
    };
    let result = crate::inspect::scanners::dead_code::run_dead_code_scan(&job)
        .outcome
        .unwrap();
    let mut contributions = result
        .contributions
        .iter()
        .map(|contribution| {
            format!(
                "{} {:?}",
                contribution.file_path.display(),
                contribution.contribution
            )
        })
        .collect::<Vec<_>>();
    contributions.sort();
    format!(
        "{projection}\n{}\n{}",
        result.aggregate,
        contributions.join("\n")
    )
    .replace(&root.display().to_string(), "<root>")
}

/// Cold build and one incremental refresh of a copy of a real repository.
///
/// `AFT_CALLGRAPH_MEASURE_ROOT` names the repository, `AFT_CALLGRAPH_MEASURE_EDIT`
/// a repository-relative file that gets one exported function appended before
/// the refresh, and the optional `AFT_CALLGRAPH_MEASURE_DUMP` a directory for
/// the graph rows and query outputs after each step.
#[test]
#[ignore = "measurement: needs AFT_CALLGRAPH_MEASURE_ROOT and AFT_CALLGRAPH_MEASURE_EDIT"]
fn measure_cold_build_and_refresh_on_real_repo() {
    let source = PathBuf::from(std::env::var("AFT_CALLGRAPH_MEASURE_ROOT").expect("root"));
    let edit = PathBuf::from(std::env::var("AFT_CALLGRAPH_MEASURE_EDIT").expect("edit"));
    let dump = std::env::var_os("AFT_CALLGRAPH_MEASURE_DUMP").map(PathBuf::from);
    let samples = std::env::var("AFT_CALLGRAPH_MEASURE_SAMPLES")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(200usize);

    let dir = tempfile::tempdir().expect("temp dir");
    let root = dir.path().join("project");
    fs::create_dir_all(&root).expect("create root");
    let root = fs::canonicalize(root).expect("canonical root");
    copy_tracked_sources(&source, &root);
    let files: Vec<PathBuf> = callgraph::walk_project_files(&root).collect();
    eprintln!("files={}", files.len());

    let store = CallGraphStore::open(dir.path().join("store"), root.clone()).expect("open store");
    let compiles = install_compile_counter(&store);
    callgraph::clear_workspace_package_cache();
    let (stats, cold) = measure(&root, compiles, || store.cold_build(&files));
    let stats = stats.expect("cold build");
    report("cold_build", &cold);
    eprintln!("cold_build stats: {stats:?}");
    if let Some(dump) = &dump {
        fs::create_dir_all(dump).expect("create dump dir");
        fs::write(dump.join("cold_rows.txt"), dump_graph_rows(&store)).expect("write rows");
        fs::write(
            dump.join("cold_queries.txt"),
            dump_query_outputs(&store, samples),
        )
        .expect("write queries");
    }

    let edit_path = root.join(&edit);
    assert!(store.checkpoint_wal_truncate());
    let addition = match edit_path.extension().and_then(|ext| ext.to_str()) {
        Some("rs") => "\npub fn aft_measure_added_function() {}\n",
        Some("py") => "\n\ndef aft_measure_added_function():\n    pass\n",
        _ => "\nexport function aftMeasureAddedFunction() {}\n",
    };
    let mut text = fs::read_to_string(&edit_path).expect("read edit file");
    text.push_str(addition);
    fs::write(&edit_path, text).expect("write edit file");

    let (refresh_stats, refresh) = measure(&root, compiles, || {
        store.refresh_files(std::slice::from_ref(&edit_path))
    });
    let refresh_stats = refresh_stats.expect("refresh");
    report("refresh", &refresh);
    let conn = store.conn.lock().unwrap();
    let page_size: u64 = conn
        .pragma_query_value(None, "page_size", |row| row.get(0))
        .unwrap();
    let wal_bytes = fs::metadata(sqlite_file_set_path(store.sqlite_path(), "-wal"))
        .unwrap()
        .len();
    let checkpoint: (u64, u64, u64) = conn
        .query_row("PRAGMA wal_checkpoint(PASSIVE)", [], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?))
        })
        .unwrap();
    eprintln!("refresh bytes: database_bytes={} wal_bytes={wal_bytes} dirty_frames={} checkpoint_bytes={}", fs::metadata(store.sqlite_path()).unwrap().len(), checkpoint.1, checkpoint.2 * page_size);
    drop(conn);
    eprintln!(
        "refresh stats: dependency_selected_refs={} refreshed_own_files={} surface_changed={:?}",
        refresh_stats.dependency_selected_refs,
        refresh_stats.refreshed_own_files,
        refresh_stats.surface_changed
    );
    if let Some(dump) = &dump {
        fs::write(dump.join("refresh_rows.txt"), dump_graph_rows(&store)).expect("write rows");
        fs::write(
            dump.join("refresh_queries.txt"),
            dump_query_outputs(&store, samples),
        )
        .expect("write queries");
    }
}

fn fixture_root(dir: &tempfile::TempDir, name: &str) -> PathBuf {
    let root = dir.path().join(name);
    fs::create_dir_all(&root).expect("create fixture root");
    fs::canonicalize(root).expect("canonical fixture root")
}

fn write(root: &Path, rel: &str, text: &str) -> PathBuf {
    let path = root.join(rel);
    fs::create_dir_all(path.parent().expect("parent")).expect("create parent");
    fs::write(&path, text).expect("write fixture file");
    path
}

/// A workspace whose app files import two workspace packages by name, so
/// each import walks `package.json` files to find the package. Returns the
/// source files and the number of `package.json` files.
fn ts_workspace_fixture(root: &Path, app_files: usize) -> (Vec<PathBuf>, usize) {
    write(
        root,
        "package.json",
        r#"{"name":"fx-root","private":true,"workspaces":["packages/*"]}"#,
    );
    write(
        root,
        "tsconfig.json",
        r#"{"compilerOptions":{"baseUrl":".","paths":{}}}"#,
    );
    let mut files = Vec::new();
    for lib in ["alpha", "beta"] {
        write(
            root,
            &format!("packages/{lib}/package.json"),
            &format!(r#"{{"name":"@fx/{lib}","exports":{{".":{{"source":"./src/index.ts"}}}}}}"#),
        );
        files.push(write(
            root,
            &format!("packages/{lib}/src/index.ts"),
            &format!("export function {lib}(value: number) {{ return value + 1; }}\n"),
        ));
    }
    write(root, "packages/app/package.json", r#"{"name":"@fx/app"}"#);
    for index in 0..app_files {
        files.push(write(
            root,
            &format!("packages/app/src/deep/nested/caller_{index:03}.ts"),
            &format!(
                "import {{ alpha }} from \"@fx/alpha\";\nimport {{ beta }} from \"@fx/beta\";\n\
                 export function caller{index}() {{ return alpha({index}) + beta({index}); }}\n"
            ),
        ));
    }
    (files, 4)
}

/// A Rust crate whose `caller.rs` makes `calls` path-qualified calls into a
/// sibling module, plus a call into an inline module.
fn rust_qualified_call_fixture(root: &Path, calls: usize) -> Vec<PathBuf> {
    write(
        root,
        "Cargo.toml",
        "[package]\nname = \"fx\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    );
    let mut caller = String::new();
    for index in 0..calls {
        caller.push_str(&format!(
            "pub fn caller_{index}() {{ crate::util::helper(); crate::util::inner::deep(); }}\n"
        ));
    }
    vec![
        write(root, "src/lib.rs", "pub mod caller;\npub mod util;\n"),
        write(
            root,
            "src/util.rs",
            "pub fn helper() {}\npub mod inner {\n    pub fn deep() {}\n}\n",
        ),
        write(root, "src/caller.rs", &caller),
    ]
}

fn cold_build_counts(
    root: &Path,
    store_dir: &Path,
    files: &[PathBuf],
) -> (CallGraphStore, WorkCounts) {
    let store =
        CallGraphStore::open(store_dir.to_path_buf(), root.to_path_buf()).expect("open store");
    let compiles = install_compile_counter(&store);
    callgraph::clear_workspace_package_cache();
    let (stats, counts) = measure(root, compiles, || store.cold_build(files));
    stats.expect("cold build");
    (store, counts)
}

fn empty_go_caller_data() -> FileCallData {
    FileCallData {
        calls_by_symbol: HashMap::new(),
        value_refs_by_symbol: HashMap::new(),
        macro_body_calls_by_macro: HashMap::new(),
        macro_mentions_by_symbol: HashMap::new(),
        exported_symbols: Vec::new(),
        symbol_metadata: HashMap::new(),
        default_export_symbol: None,
        import_block: crate::imports::ImportBlock::empty(),
        lang: LangId::Go,
    }
}

fn go_package_directory_work(adapter: &str) {
    let dir = tempfile::tempdir().unwrap();
    let root = fixture_root(&dir, "project");
    let files = vec![
        write(&root, "pkg/a.go", "package pkg\nfunc Target() {}\n"),
        write(
            &root,
            "pkg/b.go",
            "package pkg\nfunc Caller() { Target() }\n",
        ),
    ];
    let (store, _) = cold_build_counts(&root, &dir.path().join("store"), &files);
    let conn = store.conn.lock().unwrap();
    let memos = DiskIndexMemos::default();
    let module_memo = callgraph::ModuleResolutionMemo::default();
    let data = empty_go_caller_data();
    let disk = DiskProjectIndex {
        project_root: &root,
        conn: &conn,
        caller_file: "pkg/b.go",
        caller_data: &data,
        workspace_crate_prefixes: WorkspaceCratePrefixCache::default(),
        module_resolution_memo: &module_memo,
        memos: &memos,
        memoize_resolver_indexes: true,
    };
    let extracts = HashMap::new();
    let memory = if adapter == "lazy" {
        ProjectIndex::from_db_and_callers(
            &conn,
            &root,
            &extracts,
            WorkspaceCratePrefixCache::default(),
        )
    } else {
        ProjectIndex::from_parts(
            &root,
            ["pkg/a.go", "pkg/b.go"]
                .into_iter()
                .map(|file| (file.to_string(), (*disk.file_index(file).unwrap()).clone()))
                .collect(),
            HashMap::new(),
            WorkspaceCratePrefixCache::default(),
            Rc::new(DiskFacts::new(&root)),
        )
    };
    work_counts::reset_under(&root);
    for _ in 0..32 {
        let target = if adapter == "disk" {
            resolve_go_target(&disk, "pkg/b.go", "Target", "Target", &data)
        } else {
            resolve_go_target(&memory, "pkg/b.go", "Target", "Target", &data)
        };
        assert_eq!(
            target,
            Some(("resolved".into(), "pkg/a.go".into(), "Target".into()))
        );
    }
    memos.ensure_lookups_succeeded().unwrap();
    assert!(memory.take_lazy_load_error().is_none());
    assert_eq!(
        work_counts::directory_reads_under(&root.join("pkg")),
        1,
        "32 references into one package must list its directory only once ({adapter})"
    );
}

#[test]
fn go_package_directory_is_listed_once_disk() {
    go_package_directory_work("disk");
}

#[test]
fn go_package_directory_is_listed_once_memory() {
    go_package_directory_work("memory");
}

#[test]
fn go_package_directory_is_listed_once_lazy() {
    go_package_directory_work("lazy");
}

#[test]
fn go_package_symbol_index_matches_scan() {
    let dir = tempfile::tempdir().unwrap();
    let root = fixture_root(&dir, "project");
    write(&root, "go.mod", "module example\n");
    let files = vec![
        write(
            &root,
            "pkg/a.go",
            "package pkg\nfunc Public() {}\nfunc private() {}\nfunc duplicate() {}\n",
        ),
        write(&root, "pkg/b.go", "package pkg\nfunc duplicate() {}\n"),
        write(
            &root,
            "pkg/a_test.go",
            "package pkg\nfunc Public() {}\nfunc TestOnly() {}\n",
        ),
    ];
    write(&root, "pkg/unindexed.go", "package pkg\nfunc Public() {}\n");
    let (store, _) = cold_build_counts(&root, &dir.path().join("store"), &files);
    let conn = store.conn.lock().unwrap();
    let memos = DiskIndexMemos::default();
    let module_memo = callgraph::ModuleResolutionMemo::default();
    let data = empty_go_caller_data();
    let disk = DiskProjectIndex {
        project_root: &root,
        conn: &conn,
        caller_file: "pkg/b.go",
        caller_data: &data,
        workspace_crate_prefixes: WorkspaceCratePrefixCache::default(),
        module_resolution_memo: &module_memo,
        memos: &memos,
        memoize_resolver_indexes: true,
    };
    let memory = ProjectIndex::from_parts(
        &root,
        ["pkg/a.go", "pkg/b.go", "pkg/a_test.go"]
            .into_iter()
            .map(|file| (file.into(), (*disk.file_index(file).unwrap()).clone()))
            .collect(),
        HashMap::new(),
        WorkspaceCratePrefixCache::default(),
        Rc::new(DiskFacts::new(&root)),
    );
    let extracts = HashMap::new();
    let lazy = ProjectIndex::from_db_and_callers(
        &conn,
        &root,
        &extracts,
        WorkspaceCratePrefixCache::default(),
    );
    let scan_memos = DiskIndexMemos::default();
    let scan = DiskProjectIndex {
        memos: &scan_memos,
        workspace_crate_prefixes: WorkspaceCratePrefixCache::default(),
        memoize_resolver_indexes: false,
        ..disk
    };
    fn check(index: &impl ResolverIndex, scan: &impl ResolverIndex) {
        for caller in ["pkg/b.go", "pkg/a_test.go"] {
            for module in [None, Some("example/pkg"), Some("external/pkg")] {
                assert_eq!(
                    index.go_package_files(caller, module),
                    scan.go_package_files(caller, module),
                    "cached membership must match independent enumeration"
                );
                for symbol in ["Public", "private", "duplicate", "TestOnly", "missing"] {
                    let expected = match (caller.ends_with("_test.go"), module, symbol) {
                        (_, Some("external/pkg"), _) => None,
                        (false, _, "Public") | (_, None, "private") => Some("pkg/a.go".into()),
                        (true, _, "TestOnly") => Some("pkg/a_test.go".into()),
                        _ => None,
                    };
                    assert_eq!(
                        index.go_symbol_target(caller, module, symbol),
                        expected,
                        "caller={caller} module={module:?} symbol={symbol}"
                    );
                    assert_eq!(
                        index.go_symbol_target(caller, module, symbol),
                        go_symbol_target_by_scan(scan, caller, module, symbol)
                    );
                }
            }
        }
    }
    check(&disk, &scan);
    check(&memory, &scan);
    check(&lazy, &scan);
    memos.ensure_lookups_succeeded().unwrap();
    scan_memos.ensure_lookups_succeeded().unwrap();
    assert!(lazy.take_lazy_load_error().is_none());

    let mut oversized = ProjectIndex::from_parts(
        &root,
        memory.files.clone(),
        HashMap::new(),
        WorkspaceCratePrefixCache::default(),
        Rc::new(DiskFacts::new(&root)),
    );
    let long_symbol = "x".repeat(GO_PACKAGE_MEMO_BYTES);
    oversized
        .files
        .get_mut("pkg/a.go")
        .unwrap()
        .node_by_bare
        .insert(long_symbol.clone(), "oversized-node".into());
    assert_eq!(
        oversized.go_symbol_target("pkg/b.go", None, &long_symbol),
        Some("pkg/a.go".into()),
        "an oversized package must still resolve through the scan"
    );
    assert!(oversized.go_packages.borrow().packages.is_empty());
}

#[test]
fn go_package_oversized_build_is_attempted_once_and_scans_list_once() {
    let dir = tempfile::tempdir().unwrap();
    let root = fixture_root(&dir, "project");
    write(&root, "pkg/a.go", "package pkg\n");
    write(&root, "pkg/b.go", "package pkg\n");
    let long_symbol = "x".repeat(GO_PACKAGE_MEMO_BYTES);
    let mut file = DbFileIndex::empty(Some(LangId::Go));
    file.node_by_bare
        .insert(long_symbol.clone(), "oversized-node".into());
    let index = ProjectIndex::from_parts(
        &root,
        HashMap::from([
            ("pkg/a.go".into(), file),
            ("pkg/b.go".into(), DbFileIndex::empty(Some(LangId::Go))),
        ]),
        HashMap::new(),
        WorkspaceCratePrefixCache::default(),
        Rc::new(DiskFacts::new(&root)),
    );
    work_counts::reset_under(&root);
    const REFERENCES: usize = 32;
    for _ in 0..REFERENCES {
        assert_eq!(
            go_symbol_target_by_scan(&index, "pkg/b.go", None, &long_symbol),
            Some("pkg/a.go".into())
        );
    }
    assert_eq!(work_counts::go_package_builds_under(&root), 0);
    assert_eq!(work_counts::directory_reads_under(&root), REFERENCES);

    work_counts::reset_under(&root);
    for _ in 0..REFERENCES {
        assert_eq!(
            index.go_symbol_target("pkg/b.go", None, &long_symbol),
            Some("pkg/a.go".into())
        );
    }
    assert_eq!(
        work_counts::go_package_builds_under(&root),
        1,
        "an oversized package must be attempted only once per pass"
    );
    assert_eq!(
        work_counts::directory_reads_under(&root),
        REFERENCES + 1,
        "one construction listing plus one uncached directory listing per scan"
    );
    assert!(index.go_packages.borrow().packages.is_empty());
}

#[test]
fn go_package_memo_evicts_whole_packages_within_budget() {
    let mut memo = GoPackageMemo::default();
    let package = || {
        Rc::new(GoPackage {
            bytes: GO_PACKAGE_MEMO_BYTES / 2 + 1,
            ..Default::default()
        })
    };
    memo.insert("first".into(), package());
    memo.insert("second".into(), package());
    assert!(!memo.packages.contains_key(Path::new("first")));
    assert!(memo.packages.contains_key(Path::new("second")));
    assert_eq!(memo.order.len(), 1);
    assert!(memo.bytes <= GO_PACKAGE_MEMO_BYTES);
}

#[test]
fn go_package_oversized_markers_are_bounded_without_retrying_builds() {
    let dir = tempfile::tempdir().unwrap();
    let root = fixture_root(&dir, "project");
    write(&root, "pkg/a.go", "package pkg\n");
    let mut memo = GoPackageMemo::default();
    const DIRECTORIES: usize = 1024;
    for number in 0..DIRECTORIES {
        memo.mark_oversized(PathBuf::from(format!("{}-{number}", "x".repeat(256))));
    }
    assert!(memo.oversized_dirs.len() < DIRECTORIES);
    assert!(memo.oversized_bytes <= GO_PACKAGE_OVERSIZED_MEMO_BYTES);
    let retained = memo.oversized_dirs.len();
    memo.mark_oversized("another".into());
    assert_eq!(memo.oversized_dirs.len(), retained);

    let cached_dir = root.join("cached");
    memo.insert(cached_dir, Rc::new(GoPackage::default()));
    let memo = RefCell::new(memo);
    let facts = DiskFacts::new(&root);
    let paths = FactPaths {
        root: &root,
        facts: &facts,
    };
    let modules = WorkspaceCratePrefixCache::default();
    work_counts::reset_under(&root);
    for caller in ["pkg/a.go", "unseen/a.go"] {
        assert!(cached_go_package(
            &paths,
            &modules,
            &memo,
            caller,
            None,
            |_| true,
            |_| None::<Rc<DbFileIndex>>,
        )
        .is_none());
    }
    assert!(cached_go_package(
        &paths,
        &modules,
        &memo,
        "cached/a.go",
        None,
        |_| true,
        |_| None::<Rc<DbFileIndex>>,
    )
    .is_some());
    assert_eq!(work_counts::go_package_builds_under(&root), 0);
    assert_eq!(work_counts::directory_reads_under(&root), 0);
}

#[test]
fn cold_build_parses_each_file_once_and_matches_a_reparsing_build() {
    let dir = tempfile::tempdir().expect("temp dir");
    let root = fixture_root(&dir, "project");
    let (mut files, _) = ts_workspace_fixture(&root, 12);
    let rust_root = root.join("crates/fx");
    files.extend(rust_qualified_call_fixture(&rust_root, 6));

    let (retaining, retained) = cold_build_counts(&root, &dir.path().join("store-a"), &files);
    assert_eq!(
        retained.parses,
        files.len(),
        "every source file is parsed by extraction and by nothing else"
    );
    assert_eq!(retained.max_parses_of_one_file, 1);

    // With no retention budget the resolution pass parses every caller again,
    // which is the reference the retained resolver inputs must match.
    set_retained_caller_budget_for_test(Some(0));
    let (reparsing, reparsed) = cold_build_counts(&root, &dir.path().join("store-b"), &files);
    set_retained_caller_budget_for_test(None);
    eprintln!(
        "parses for {} files: retained={} reparsing={}",
        files.len(),
        retained.parses,
        reparsed.parses
    );
    assert!(
        reparsed.parses > files.len(),
        "control: without retention callers are parsed twice ({} parses for {} files)",
        reparsed.parses,
        files.len()
    );
    assert_eq!(
        dump_graph_rows(&retaining),
        dump_graph_rows(&reparsing),
        "retained resolver inputs must resolve exactly like a fresh parse"
    );
}

#[test]
fn perf_audit2_fresh_staging_does_not_read_sources_before_extraction() {
    let dir = tempfile::tempdir().expect("temp dir");
    let root = fixture_root(&dir, "project");
    let (files, _) = ts_workspace_fixture(&root, 96);
    let (store, counts) = cold_build_counts(&root, &dir.path().join("store"), &files);
    assert_eq!(counts.parses, files.len());
    assert!(store.edge_snapshot().unwrap().len() >= 192);
    eprintln!(
        "fresh staging: {} files, {} source re-reads",
        files.len(),
        counts.staging_reads
    );
    assert_eq!(
        counts.staging_reads, 0,
        "an absent staged row cannot match content"
    );
}

#[test]
fn perf_audit2_one_function_refresh_writes_a_delta_not_the_store() {
    let dir = tempfile::tempdir().unwrap();
    let mut volumes = Vec::new();
    for file_count in [32, 256] {
        let root = fixture_root(&dir, &format!("project-{file_count}"));
        let mut files = Vec::new();
        for file in 0..file_count {
            let mut source = String::new();
            for function in 0..64 {
                source.push_str(&format!(
                    "export function f{function}() {{ console.log('value'); return f{}(); }}\n",
                    (function + 1) % 64
                ));
            }
            files.push(write(&root, &format!("src/f{file:03}.ts"), &source));
        }
        let (store, _) = cold_build_counts(
            &root,
            &dir.path().join(format!("store-{file_count}")),
            &files,
        );
        assert!(store.checkpoint_wal_truncate());
        let database_bytes = fs::metadata(store.sqlite_path()).unwrap().len();
        let path = &files[0];
        let source = fs::read_to_string(path).unwrap();
        // One function body changes at the end, leaving every other symbol and
        // reference location unchanged. A call edge is added, not just a comment.
        let position = source.rfind("return f0();").unwrap();
        let edited = format!("{}f1(); {}", &source[..position], &source[position..]);
        fs::write(path, edited).unwrap();
        let before = store.conn.lock().unwrap().total_changes();
        let stats = store.refresh_files(std::slice::from_ref(path)).unwrap();
        assert_eq!(stats.refreshed_own_files, 1);
        let conn = store.conn.lock().unwrap();
        let changed_rows = conn.total_changes() - before;
        let page_size: u64 = conn
            .pragma_query_value(None, "page_size", |row| row.get(0))
            .unwrap();
        let wal_bytes = fs::metadata(sqlite_file_set_path(store.sqlite_path(), "-wal"))
            .unwrap()
            .len();
        let (busy, frames, checkpointed): (u64, u64, u64) = conn
            .query_row("PRAGMA wal_checkpoint(PASSIVE)", [], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?))
            })
            .unwrap();
        assert_eq!(busy, 0);
        assert_eq!(frames, checkpointed);
        let checkpoint_bytes = checkpointed * page_size;
        drop(conn);
        let rebuilt = CallGraphStore::open(
            dir.path().join(format!("rebuilt-{file_count}")),
            root.clone(),
        )
        .unwrap();
        rebuilt.cold_build(&files).unwrap();
        assert_eq!(
            dump_graph_rows(&store),
            dump_graph_rows(&rebuilt),
            "one-function refresh must match a cold build"
        );
        eprintln!("one-function refresh: files={file_count} database_bytes={database_bytes} changed_rows={changed_rows} wal_bytes={wal_bytes} dirty_frames={frames} checkpoint_bytes={checkpoint_bytes}");
        volumes.push((frames, changed_rows));
    }
    assert!(
        volumes
            .iter()
            .all(|(frames, rows)| *frames <= 128 && *rows <= 32),
        "one-function edit must not rewrite unchanged rows or full tables: {volumes:?}"
    );
    assert!(
        volumes[1].0 <= volumes[0].0 + 32,
        "8x membership must not cause store-sized WAL/checkpoints: {volumes:?}"
    );
}

#[test]
fn cold_build_reads_each_package_json_a_bounded_number_of_times() {
    // Compare two fixture sizes rather than one fixed bound: how many readers
    // race to the first read of a file depends on the runner's core count, but
    // with one memo per build the number of reads must not grow with the
    // number of importers.
    let reads_for = |app_files: usize| {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = fixture_root(&dir, "project");
        let (files, package_jsons) = ts_workspace_fixture(&root, app_files);
        let (_store, counts) = cold_build_counts(&root, &dir.path().join("store"), &files);
        (counts.package_json_reads, package_jsons)
    };
    let (small, package_jsons) = reads_for(24);
    let (large, _) = reads_for(96);
    // Each package.json can be read once more per parse thread that loses the
    // race to fill the memo, so a size difference within that slack is noise.
    let race_slack = package_jsons * build_pool_size();
    eprintln!("package.json reads: 24 importers={small} 96 importers={large} (slack {race_slack})");
    assert!(
        large < 96,
        "package.json reads must stay far below the 96 importers: {large}"
    );
    assert!(
        large <= small + race_slack,
        "package.json reads must not scale with importers: {small} for 24, {large} for 96, slack {race_slack}"
    );
}

/// Executions of the whole-table scans a Rust qualified call used to repeat:
/// the Rust file list and the module-reference list. Only the store in
/// `rust_qualified_call_resolution_scans_tables_independent_of_call_count`
/// installs the tracer that counts them.
static RUST_LOOKUP_SCANS: AtomicUsize = AtomicUsize::new(0);

fn count_rust_lookup_scans(sql: &str) {
    if sql.contains("FROM files WHERE lang = 'rust'")
        || sql.contains("WHERE kind = 'module' AND module_path IS NOT NULL")
    {
        RUST_LOOKUP_SCANS.fetch_add(1, AtomicOrdering::Relaxed);
    }
}

#[test]
fn rust_qualified_call_resolution_scans_tables_independent_of_call_count() {
    let dir = tempfile::tempdir().expect("temp dir");
    let mut compiled = Vec::new();
    let mut scans = Vec::new();
    for calls in [20usize, 80] {
        let root = fixture_root(&dir, &format!("project-{calls}"));
        let files = rust_qualified_call_fixture(&root, calls);
        let store = CallGraphStore::open(dir.path().join(format!("store-{calls}")), root.clone())
            .expect("open store");
        let compiles = install_compile_counter(&store);
        store
            .conn
            .lock()
            .expect("callgraph store mutex poisoned")
            .trace(Some(count_rust_lookup_scans));
        let scans_before = RUST_LOOKUP_SCANS.load(AtomicOrdering::Relaxed);
        let (stats, counts) = measure(&root, compiles, || store.cold_build(&files));
        stats.expect("cold build");
        scans.push(RUST_LOOKUP_SCANS.load(AtomicOrdering::Relaxed) - scans_before);
        let edges = dump_graph_rows(&store)
            .lines()
            .filter(|line| line.starts_with("edges\t") && line.contains("src/util.rs"))
            .count();
        assert_eq!(edges, calls * 2, "every qualified call must resolve");
        compiled.push(counts.statements_compiled);
    }
    eprintln!(
        "rust lookup scans for 20 and 80 calls: {scans:?}; statements compiled: {compiled:?}"
    );
    assert_eq!(
        scans[0], scans[1],
        "60 more qualified calls must not rescan the files or module tables: {scans:?}"
    );
    assert!(
        compiled[1].saturating_sub(compiled[0]) < 20,
        "60 more qualified calls must not compile statements per call: {compiled:?}"
    );
}

/// Refresh `edit` in a store built from `files`, check the graph equals a
/// cold build of the edited tree, and return the refresh's work counts.
fn refresh_matches_cold_build(files: &[(&str, &str)], edit: (&str, &str)) -> WorkCounts {
    let dir = tempfile::tempdir().expect("temp dir");
    let root = fixture_root(&dir, "project");
    let paths = files
        .iter()
        .map(|(rel, text)| write(&root, rel, text))
        .collect::<Vec<_>>();
    let (store, _) = cold_build_counts(&root, &dir.path().join("store"), &paths);
    let compiles = install_compile_counter(&store);
    let edited = write(&root, edit.0, edit.1);
    let (stats, counts) = measure(&root, compiles, || {
        store.refresh_files(std::slice::from_ref(&edited))
    });
    stats.expect("refresh");

    let (rebuilt, _) = cold_build_counts(&root, &dir.path().join("store-rebuilt"), &paths);
    assert_eq!(
        dump_graph_rows(&store),
        dump_graph_rows(&rebuilt),
        "refreshing {} must give the rows of a cold build",
        edit.0
    );
    counts
}

fn ts_importer_files(plain: usize) -> Vec<(String, String)> {
    let mut files = vec![(
        "src/lib.ts".to_string(),
        "export function alpha() { return 1; }\nexport function beta() { return 2; }\n\
         export default function main() { return alpha(); }\n"
            .to_string(),
    )];
    for index in 0..plain {
        files.push((
            format!("src/plain_{index:02}.ts"),
            format!(
                "import {{ alpha, beta }} from \"./lib\";\n\
                 export function plain{index}() {{ return alpha() + beta(); }}\n"
            ),
        ));
    }
    files.push((
        "src/named.ts".to_string(),
        "import { zed } from \"./lib\";\nexport function named() { return zed(); }\n".to_string(),
    ));
    files.push((
        "src/aliased.ts".to_string(),
        "import { zed as z } from \"./lib\";\nexport function aliased() { return z(); }\n"
            .to_string(),
    ));
    files.push((
        "src/through_default.ts".to_string(),
        "import main from \"./lib\";\nexport function viaDefault() { return main(); }\n"
            .to_string(),
    ));
    files
}

fn as_refs(files: &[(String, String)]) -> Vec<(&str, &str)> {
    files
        .iter()
        .map(|(rel, text)| (rel.as_str(), text.as_str()))
        .collect()
}

#[test]
fn refresh_parses_only_importers_that_name_a_changed_symbol() {
    let files = ts_importer_files(16);
    let added = "export function alpha() { return 1; }\nexport function beta() { return 2; }\n\
                 export default function main() { return alpha(); }\nexport function zed() { return 3; }\n";
    let counts = refresh_matches_cold_build(&as_refs(&files), ("src/lib.ts", added));
    eprintln!("added export: refresh parses={}", counts.parses);
    // The changed file, the importer calling `zed` and the one calling it as
    // `z`; the 16 importers of other names and the default importer are
    // left alone.
    assert_eq!(counts.parses, 3, "refresh parses: {counts:?}");
}

#[test]
fn refresh_reresolves_every_caller_bound_to_a_removed_or_moved_symbol() {
    let files = ts_importer_files(4);
    // `beta` disappears and `alpha` moves (its node id changes).
    let edited = "\nexport function alpha() { return 1; }\n\
                  export default function main() { return alpha(); }\n";
    refresh_matches_cold_build(&as_refs(&files), ("src/lib.ts", edited));
}

#[test]
fn refresh_of_a_changed_default_export_falls_back_to_every_importer() {
    let files = ts_importer_files(4);
    let edited = "export function alpha() { return 1; }\nexport function beta() { return 2; }\n\
                  export default function other() { return beta(); }\n";
    let counts = refresh_matches_cold_build(&as_refs(&files), ("src/lib.ts", edited));
    assert_eq!(
        counts.parses,
        files.len(),
        "a default-export change reaches importers by their local name, so all are re-resolved"
    );
}

#[test]
fn rust_refresh_parses_only_callers_naming_a_changed_function() {
    let mut files = vec![
        (
            "Cargo.toml".to_string(),
            "[package]\nname = \"fx\"\nversion = \"0.1.0\"\nedition = \"2021\"\n".to_string(),
        ),
        (
            "src/lib.rs".to_string(),
            "pub mod util;\npub mod late;\n".to_string()
                + &(0..8)
                    .map(|index| format!("pub mod caller_{index};\n"))
                    .collect::<String>(),
        ),
        (
            "src/util.rs".to_string(),
            "pub fn helper() {}\n".to_string(),
        ),
        (
            "src/late.rs".to_string(),
            "pub fn late() { crate::util::added(); }\n".to_string(),
        ),
    ];
    for index in 0..8 {
        files.push((
            format!("src/caller_{index}.rs"),
            format!("use crate::util::helper;\npub fn run_{index}() {{ helper(); crate::util::helper(); }}\n"),
        ));
    }
    let counts = refresh_matches_cold_build(
        &as_refs(&files),
        ("src/util.rs", "pub fn helper() {}\npub fn added() {}\n"),
    );
    eprintln!("rust added fn: refresh parses={}", counts.parses);
    assert_eq!(counts.parses, 2, "refresh parses: {counts:?}");
}

#[test]
fn refresh_releases_the_store_connection_while_it_parses() {
    let dir = tempfile::tempdir().expect("temp dir");
    let root = fixture_root(&dir, "project");
    let (files, _) = ts_workspace_fixture(&root, 4);
    let (store, _) = cold_build_counts(&root, &dir.path().join("store"), &files);
    let edited = write(
        &root,
        "packages/alpha/src/index.ts",
        "export function alpha(value: number) { return value + 2; }\n",
    );

    let (started_tx, started_rx) = std::sync::mpsc::channel::<()>();
    let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
    let release_rx = Mutex::new(release_rx);
    let started_tx = Mutex::new(started_tx);
    set_refresh_parse_hook_for_test(
        &root,
        Some(Arc::new(move || {
            let _ = started_tx.lock().expect("hook").send(());
            // The parse waits here until the reader below has finished.
            let _ = release_rx
                .lock()
                .expect("hook")
                .recv_timeout(Duration::from_secs(30));
        })),
    );
    std::thread::scope(|scope| {
        let refresh = scope.spawn(|| store.refresh_files(std::slice::from_ref(&edited)));
        started_rx
            .recv_timeout(Duration::from_secs(30))
            .expect("refresh reached its parse");
        let (read_tx, read_rx) = std::sync::mpsc::channel();
        let store = &store;
        scope.spawn(move || {
            let _ = read_tx.send(store.indexed_file_count());
        });
        let read = read_rx.recv_timeout(Duration::from_secs(10));
        release_tx.send(()).expect("release parse");
        let read = read.expect("a reader must get the connection while the refresh parses");
        assert_eq!(read.expect("indexed file count"), files.len());
        refresh.join().expect("refresh thread").expect("refresh");
    });
    set_refresh_parse_hook_for_test(&root, None);
}

/// Statements compiled by a cold build of `files` TS modules and by a
/// refresh that rewrites every one of them.
fn build_and_refresh_compiles(dir: &tempfile::TempDir, files: usize) -> (usize, usize) {
    let root = fixture_root(dir, &format!("project-{files}"));
    let module = |index: usize, extra: &str| {
        format!(
            "import {{ helper }} from \"./lib\";\n\
             export class Worker{index} {{ run() {{ return helper(); }} }}\n\
             export function use{index}(worker: any) {{ return worker.run() + helper(); }}\n{extra}"
        )
    };
    let mut paths = vec![write(
        &root,
        "src/lib.ts",
        "export function helper() { return 1; }\n",
    )];
    for index in 0..files {
        paths.push(write(
            &root,
            &format!("src/m{index:03}.ts"),
            &module(index, ""),
        ));
    }
    let (store, cold) =
        cold_build_counts(&root, &dir.path().join(format!("store-{files}")), &paths);
    let compiles = install_compile_counter(&store);
    let mut changed = Vec::new();
    for index in 0..files {
        changed.push(write(
            &root,
            &format!("src/m{index:03}.ts"),
            &module(
                index,
                &format!("export function added{index}() {{ return 2; }}\n"),
            ),
        ));
    }
    let (stats, refresh) = measure(&root, compiles, || store.refresh_files(&changed));
    stats.expect("refresh");
    (cold.statements_compiled, refresh.statements_compiled)
}

#[test]
fn cold_build_and_refresh_compile_statements_independent_of_file_count() {
    let dir = tempfile::tempdir().expect("temp dir");
    let (small_cold, small_refresh) = build_and_refresh_compiles(&dir, 10);
    let (large_cold, large_refresh) = build_and_refresh_compiles(&dir, 40);
    eprintln!(
        "statements compiled: cold 10 files={small_cold} 40 files={large_cold}; \
         refresh 10 files={small_refresh} 40 files={large_refresh}"
    );
    assert!(
        large_cold.saturating_sub(small_cold) < 30,
        "30 more files must not compile statements per file or row in a cold build: \
         {small_cold} vs {large_cold}"
    );
    assert!(
        large_refresh.saturating_sub(small_refresh) < 30,
        "30 more refreshed files must not compile statements per file or row: \
         {small_refresh} vs {large_refresh}"
    );
}

/// realpath calls made by `run` on this thread.
fn canonicalize_calls<T>(run: impl FnOnce() -> T) -> (T, usize) {
    let before = CANONICALIZE_CALLS.with(std::cell::Cell::get);
    let value = run();
    (
        value,
        CANONICALIZE_CALLS.with(std::cell::Cell::get) - before,
    )
}

#[test]
fn graph_walks_do_not_resolve_stored_paths_against_the_filesystem_per_node() {
    use crate::commands::callgraph_store_adapter::{call_tree_result, trace_to_result};
    let dir = tempfile::tempdir().expect("temp dir");
    let root = fixture_root(&dir, "project");
    let length = 24;
    let mut paths = Vec::new();
    for index in 0..length {
        let source = if index + 1 < length {
            format!(
                "import {{ f{next} }} from \"./f{next:02}\";\nexport function f{index}() {{ return f{next}(); }}\n",
                next = index + 1
            )
        } else {
            format!("export function f{index}() {{ return 1; }}\n")
        };
        paths.push(write(&root, &format!("src/f{index:02}.ts"), &source));
    }
    let (store, _) = cold_build_counts(&root, &dir.path().join("store"), &paths);

    let (tree, tree_realpaths) = canonicalize_calls(|| {
        call_tree_result(&store, Path::new("src/f00.ts"), "f0", length + 2, true)
    });
    let tree = tree.expect("call tree");
    let mut depth = 0;
    let mut node = &tree;
    while let Some(child) = node.children.first() {
        depth += 1;
        node = child;
    }
    assert_eq!(depth, length - 1, "the whole chain must be walked");

    let last = format!("src/f{:02}.ts", length - 1);
    let (trace, trace_realpaths) = canonicalize_calls(|| {
        trace_to_result(
            &store,
            Path::new(&last),
            &format!("f{}", length - 1),
            40,
            true,
        )
    });
    let trace = trace.expect("trace");
    assert!(trace.truncated_paths > 0 || trace.total_paths > 0);
    eprintln!("realpath calls: call_tree={tree_realpaths} trace_to={trace_realpaths}");
    assert!(
        tree_realpaths <= 2 && trace_realpaths <= 2,
        "a {length}-node walk resolved paths per node: call_tree={tree_realpaths} trace_to={trace_realpaths}"
    );
}

/// Linux `/proc/self/io` counters. `rchar` counts every byte SQLite asked the
/// OS for through `pread`, whether or not the page cache served it, so it
/// shows the read volume a store with a cold OS cache turns into disk reads
/// (`read_bytes`). The engine's own `legacy_callgraph_refresh` event reports
/// `read_bytes`, `write_bytes` and `wchar` from the same file.
#[derive(Clone, Copy, Debug, Default)]
struct ProcIo {
    rchar: u64,
    wchar: u64,
    read_bytes: u64,
    write_bytes: u64,
}

impl ProcIo {
    fn sample() -> Self {
        let text = fs::read_to_string("/proc/self/io").unwrap_or_default();
        let mut io = Self::default();
        for line in text.lines() {
            let Some((key, value)) = line.split_once(':') else {
                continue;
            };
            let value = value.trim().parse().unwrap_or(0);
            match key.trim() {
                "rchar" => io.rchar = value,
                "wchar" => io.wchar = value,
                "read_bytes" => io.read_bytes = value,
                "write_bytes" => io.write_bytes = value,
                _ => {}
            }
        }
        io
    }

    fn since(self, before: Self) -> Self {
        Self {
            rchar: self.rchar.saturating_sub(before.rchar),
            wchar: self.wchar.saturating_sub(before.wchar),
            read_bytes: self.read_bytes.saturating_sub(before.read_bytes),
            write_bytes: self.write_bytes.saturating_sub(before.write_bytes),
        }
    }
}

impl std::fmt::Display for ProcIo {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "rchar={} wchar={} read_bytes={} write_bytes={}",
            self.rchar, self.wchar, self.read_bytes, self.write_bytes
        )
    }
}

/// Per-statement totals collected by [`record_statement_profile`].
#[derive(Clone, Copy, Debug, Default)]
struct StatementProfile {
    runs: u64,
    fullscan_steps: u64,
    vm_steps: u64,
    nanos: u64,
}

static STATEMENT_PROFILES: std::sync::LazyLock<
    std::sync::Mutex<std::collections::BTreeMap<String, StatementProfile>>,
> = std::sync::LazyLock::new(Default::default);

/// SQLite calls this when a statement finishes. The full-scan step counter
/// counts forward steps a statement took through a full table scan rather
/// than an index lookup, so a large value marks the "whole-table work" the
/// measurement looks for.
unsafe extern "C" fn record_statement_profile(
    mask: std::os::raw::c_uint,
    _context: *mut c_void,
    statement: *mut c_void,
    elapsed: *mut c_void,
) -> c_int {
    if mask != ffi::SQLITE_TRACE_PROFILE as std::os::raw::c_uint {
        return 0;
    }
    let statement = statement.cast::<ffi::sqlite3_stmt>();
    // SAFETY: for SQLITE_TRACE_PROFILE SQLite passes the finished statement
    // and a pointer to its elapsed nanoseconds; both are valid for the call.
    let (fullscan_steps, vm_steps, nanos, sql) = unsafe {
        let fullscan = ffi::sqlite3_stmt_status(statement, ffi::SQLITE_STMTSTATUS_FULLSCAN_STEP, 1);
        let vm = ffi::sqlite3_stmt_status(statement, ffi::SQLITE_STMTSTATUS_VM_STEP, 1);
        let nanos = *(elapsed as *const i64);
        let text = ffi::sqlite3_sql(statement);
        let sql = if text.is_null() {
            String::new()
        } else {
            std::ffi::CStr::from_ptr(text)
                .to_string_lossy()
                .into_owned()
        };
        (fullscan, vm, nanos, sql)
    };
    let sql = sql.split_whitespace().collect::<Vec<_>>().join(" ");
    let sql = sql.chars().take(220).collect::<String>();
    let mut profiles = STATEMENT_PROFILES.lock().unwrap();
    let entry = profiles.entry(sql).or_default();
    entry.runs += 1;
    entry.fullscan_steps += fullscan_steps.max(0) as u64;
    entry.vm_steps += vm_steps.max(0) as u64;
    entry.nanos += nanos.max(0) as u64;
    0
}

fn trace_statements(conn: &TrackedConnection) {
    STATEMENT_PROFILES.lock().unwrap().clear();
    // SAFETY: the handle belongs to a live connection; the callback keeps no
    // pointer past its own call.
    let rc = unsafe {
        ffi::sqlite3_trace_v2(
            conn.handle(),
            ffi::SQLITE_TRACE_PROFILE as std::os::raw::c_uint,
            Some(record_statement_profile),
            std::ptr::null_mut(),
        )
    };
    assert_eq!(rc, ffi::SQLITE_OK, "install SQLite profile trace");
}

/// Print profile totals and up to 12 statements, ordered by full-scan steps
/// and then by VM steps.
fn report_statement_profiles(label: &str) {
    let profiles = std::mem::take(&mut *STATEMENT_PROFILES.lock().unwrap());
    let mut rows = profiles.into_iter().collect::<Vec<_>>();
    rows.sort_by(|left, right| {
        (right.1.fullscan_steps, right.1.vm_steps).cmp(&(left.1.fullscan_steps, left.1.vm_steps))
    });
    let total_fullscan: u64 = rows.iter().map(|(_, profile)| profile.fullscan_steps).sum();
    let total_vm: u64 = rows.iter().map(|(_, profile)| profile.vm_steps).sum();
    eprintln!(
        "{label}: statements={} fullscan_steps={total_fullscan} vm_steps={total_vm}",
        rows.len()
    );
    for (sql, profile) in rows.iter().take(12) {
        eprintln!(
            "{label}:   fullscan_steps={} vm_steps={} runs={} ms={} sql={sql}",
            profile.fullscan_steps,
            profile.vm_steps,
            profile.runs,
            profile.nanos / 1_000_000
        );
    }
}

/// Root-relative path of module `index` in [`write_scaling_fixture`].
fn scaling_fixture_path(index: usize) -> String {
    format!("src/m{:04}/f{:03}.ts", index / 100, index % 100)
}

/// `files` TypeScript modules, 100 per directory. Each imports two functions
/// and a class from other modules, declares a class with two methods, and
/// makes direct, imported and method calls, so every graph table (nodes,
/// refs, edges, dependencies, dispatch hints) grows with the file count.
pub(crate) fn write_scaling_fixture(root: &Path, files: usize) -> Vec<PathBuf> {
    (0..files)
        .map(|index| {
            let other = (index * 7 + 3) % files;
            let next = (index + 1) % files;
            // Method names repeat every 997 modules, so a method call has a
            // few same-named candidates (as in real code) rather than one per
            // module, which would make dispatch quadratic in the file count.
            let method = index % 997;
            let next_method = next % 997;
            let source = format!(
                "import {{ f{other}a, f{other}b }} from '../m{:04}/f{:03}';\n\
                 import {{ Widget{next} }} from '../m{:04}/f{:03}';\n\
                 export class Widget{index} {{\n\
                 \x20 render{method}(): number {{ return this.paint{method}() + helper{index}(); }}\n\
                 \x20 paint{method}(): number {{ return {index}; }}\n\
                 }}\n\
                 export function f{index}a(x: number): number {{ return f{other}a(x) + f{index}b(x); }}\n\
                 export function f{index}b(x: number): number {{ const w = new Widget{next}(); return w.render{next_method}() + x; }}\n\
                 function helper{index}(): number {{ return f{other}b({index}); }}\n",
                other / 100,
                other % 100,
                next / 100,
                next % 100,
            );
            write(root, &scaling_fixture_path(index), &source)
        })
        .collect()
}

fn sqlite_file_bytes(path: &Path) -> (u64, u64) {
    let database = fs::metadata(path).map(|meta| meta.len()).unwrap_or(0);
    let wal = fs::metadata(sqlite_file_set_path(path, "-wal"))
        .map(|meta| meta.len())
        .unwrap_or(0);
    (database, wal)
}

/// Cold build and one-file refreshes of generated stores of growing size,
/// through the same entry points the engine uses: a leased, staged cold
/// build that publishes a generation, then three one-file edits sharing a
/// refresh connection, as the refresh worker does between lifecycle changes.
///
/// Prints `/proc/self/io` deltas for each step and, for the refresh body,
/// statement profile totals and the 12 statements with the most full-scan
/// steps, then VM steps. Linux only: elsewhere the counters
/// read as zero. Sizes come from `AFT_CALLGRAPH_SCALING_FILES`
/// (comma-separated, default `5000,20000,60000`). Run it alone
/// (`--ignored --test-threads=1`): the counters are process-wide.
#[test]
#[ignore = "measurement: prints cold-build and refresh I/O by store size"]
fn measure_cold_build_and_one_file_refresh_io_by_store_size() {
    let sizes = std::env::var("AFT_CALLGRAPH_SCALING_FILES")
        .unwrap_or_else(|_| "5000,20000,60000".to_string())
        .split(',')
        .map(|size| size.trim().parse::<usize>().expect("file count"))
        .collect::<Vec<_>>();
    for files_count in sizes {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = fixture_root(&dir, "project");
        let files = write_scaling_fixture(&root, files_count);
        let source_bytes: u64 = files
            .iter()
            .map(|path| fs::metadata(path).unwrap().len())
            .sum();
        let artifact_key = crate::search_index::artifact_cache_key(&root);
        crate::root_cache::configure_artifact_access(&root, &artifact_key, false);
        let callgraph_dir = dir.path().join("callgraph").join(artifact_key);

        callgraph::clear_workspace_package_cache();
        // Sample the counters as each named build phase starts, so the cold
        // build's I/O can be split by phase. The "publication" span also
        // covers the WAL checkpoint, journal switch and rename that follow it.
        let marks = Arc::new(std::sync::Mutex::new(Vec::<(&'static str, ProcIo)>::new()));
        let observer_marks = Arc::clone(&marks);
        set_cold_build_phase_observer(Some(Arc::new(move |phase: &'static str| {
            if matches!(
                phase,
                "extraction" | "symbol_export_index" | "resolution" | "publication"
            ) {
                observer_marks
                    .lock()
                    .unwrap()
                    .push((phase, ProcIo::sample()));
            }
        })));
        let before = ProcIo::sample();
        let started = Instant::now();
        let (store, stats) = CallGraphStore::cold_build_with_lease_chunked(
            callgraph_dir.clone(),
            root.clone(),
            &files,
            100,
        )
        .expect("cold build");
        let after = ProcIo::sample();
        set_cold_build_phase_observer(None);
        let cold = after.since(before);
        let marks = std::mem::take(&mut *marks.lock().unwrap());
        let mut previous = ("staging setup and inventory", before);
        for (phase, sample) in marks.into_iter().chain([("end", after)]) {
            eprintln!(
                "scaling files={files_count} cold_build phase={} {}",
                previous.0,
                sample.since(previous.1)
            );
            previous = (phase, sample);
        }
        let cold_ms = started.elapsed().as_millis();
        let (database_bytes, wal_bytes) = sqlite_file_bytes(store.sqlite_path());
        let rows = {
            let conn = store.conn.lock().unwrap();
            [
                "files",
                "nodes",
                "refs",
                "edges",
                "dispatch_hints",
                "backend_file_state",
            ]
            .iter()
            .map(|table| {
                let count: i64 = conn
                    .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
                        row.get(0)
                    })
                    .unwrap();
                format!("{table}={count}")
            })
            .collect::<Vec<_>>()
            .join(" ")
        };
        eprintln!(
            "scaling files={files_count} source_bytes={source_bytes} cold_ms={cold_ms} \
             edges={} database_bytes={database_bytes} wal_bytes={wal_bytes} {rows}",
            stats.edges
        );
        eprintln!(
            "scaling files={files_count} cold_build {cold} wchar_per_file={} \
             wchar_per_database_byte={:.1}",
            cold.wchar / files_count as u64,
            cold.wchar as f64 / database_bytes.max(1) as f64
        );
        drop(store);

        let mut cached_refresh = None;
        for (attempt, index) in [0usize, files_count / 2, files_count - 1]
            .into_iter()
            .enumerate()
        {
            let path = root.join(scaling_fixture_path(index));
            let mut text = fs::read_to_string(&path).unwrap();
            text.push_str(&format!(
                "export function added{attempt}(x: number): number {{ return f{index}a(x); }}\n"
            ));
            fs::write(&path, text).unwrap();

            let before_open = ProcIo::sample();
            let store = cached_refresh.take().unwrap_or_else(|| {
                CallGraphStore::open_ready_for_refresh(callgraph_dir.clone(), root.clone())
                    .expect("open for refresh")
                    .expect("ready store")
            });
            let open = ProcIo::sample().since(before_open);
            trace_statements(&store.conn.lock().unwrap());
            let before_refresh = ProcIo::sample();
            let started = Instant::now();
            let (refresh_stats, profile) = store
                .refresh_files_profiled(std::slice::from_ref(&path))
                .expect("refresh");
            let refresh = ProcIo::sample().since(before_refresh);
            let refresh_ms = started.elapsed().as_millis();
            let (database_bytes, wal_bytes) = sqlite_file_bytes(store.sqlite_path());
            eprintln!(
                "scaling files={files_count} refresh#{attempt} open {open}; refresh {refresh} \
                 ms={refresh_ms} refreshed_own_files={} database_bytes={database_bytes} \
                 wal_bytes={wal_bytes}",
                refresh_stats.refreshed_own_files
            );
            eprintln!(
                "scaling files={files_count} refresh#{attempt} phases: {}",
                profile.report()
            );
            report_statement_profiles(&format!("scaling files={files_count} refresh#{attempt}"));
            // The open-time root check runs before the trace can be
            // installed, so run that one statement again under the trace.
            {
                let conn = store.conn.lock().unwrap();
                trace_statements(&conn);
                let before_roots = ProcIo::sample();
                let roots = stored_workspace_roots(&conn).unwrap();
                eprintln!(
                    "scaling files={files_count} refresh#{attempt} open-time root check \
                     roots={} {}",
                    roots.len(),
                    ProcIo::sample().since(before_roots)
                );
            }
            report_statement_profiles(&format!(
                "scaling files={files_count} refresh#{attempt} open-time"
            ));
            cached_refresh = Some(store);
        }
    }
}
