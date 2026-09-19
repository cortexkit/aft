//! Offline measurement harness for one-file incremental callgraph refreshes.
//!
//! Run against a copy of a production store:
//! `AFT_CALLGRAPH_REFRESH_STORE=/path/to/<root-key> AFT_CALLGRAPH_REFRESH_ROOT=/path/to/project cargo test -p agent-file-tools --test callgraph_refresh_bench -- --ignored --nocapture`
//! Set `AFT_CALLGRAPH_REFRESH_FILE` to choose a project-relative file, or
//! `AFT_CALLGRAPH_REFRESH_PATHS` to a newline-delimited list for a real transition.
//! In path-list mode the copied store must represent the base and the project
//! must contain the next revision: warmup and forced staleness are skipped so
//! they cannot consume the transition before measurement. Without store/root
//! variables the harness builds a synthetic store with a large fixture.
//! `AFT_CALLGRAPH_REFRESH_COUNT_ROWS=1` installs audit triggers in the private
//! copy for a separate work-count pass. Its timings and WAL include the audit
//! overhead and must not be used as the timing baseline.

use aft::callgraph_store::{project_dead_code_snapshot, CallGraphStore, RefreshFilesProfile};
use rusqlite::{backup::Backup, params, Connection, OpenFlags};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use tempfile::TempDir;

#[test]
#[ignore = "offline benchmark copies or builds a large callgraph store"]
fn bench_refresh_files_on_store_copy() {
    let temp = tempfile::tempdir().expect("benchmark temp dir");
    let path_list = std::env::var_os("AFT_CALLGRAPH_REFRESH_PATHS");
    let (store, changed_file) = match (
        std::env::var_os("AFT_CALLGRAPH_REFRESH_STORE"),
        std::env::var_os("AFT_CALLGRAPH_REFRESH_ROOT"),
    ) {
        (Some(source_store), Some(project_root)) => open_production_store_copy(
            &temp,
            Path::new(&source_store),
            PathBuf::from(project_root),
            std::env::var_os("AFT_CALLGRAPH_REFRESH_FILE").map(PathBuf::from),
            path_list.is_some(),
        ),
        (None, None) => build_synthetic_store(&temp),
        _ => panic!(
            "AFT_CALLGRAPH_REFRESH_STORE and AFT_CALLGRAPH_REFRESH_ROOT must be set together"
        ),
    };

    let changed_files = if let Some(path_list) = path_list {
        assert!(
            std::env::var_os("AFT_CALLGRAPH_REFRESH_STORE").is_some(),
            "path-list mode requires a base store"
        );
        let paths = read_transition_paths(Path::new(&path_list), store.project_root());
        eprintln!("transition_git_paths={}", paths.len());
        paths
            .into_iter()
            .filter(|path| aft::parser::detect_language(path).is_some())
            .collect()
    } else {
        let warmup = store
            .refresh_files(std::slice::from_ref(&changed_file))
            .expect("normalize copied rows with the measured binary");
        eprintln!("measurement_warmup stats={warmup:?}");
        force_stale(store.sqlite_path(), store.project_root(), &changed_file);
        report_refresh_row_counts(store.sqlite_path(), store.project_root(), &changed_file);
        vec![changed_file]
    };
    eprintln!("refresh_requested_paths={}", changed_files.len());
    report_query_plans(store.sqlite_path());
    let initial_checkpoint = wal_checkpoint(store.sqlite_path(), "TRUNCATE");
    assert_eq!(initial_checkpoint.busy, 0, "clear WAL before measurement");
    sync_sqlite_file_set(store.sqlite_path());
    let page_size = sqlite_page_size(store.sqlite_path());
    let wal_path = sqlite_sidecar(store.sqlite_path(), "-wal");
    let count_rows = std::env::var_os("AFT_CALLGRAPH_REFRESH_COUNT_ROWS").is_some();
    if count_rows {
        install_row_audit(store.sqlite_path());
    }
    let refresh_usage_before = process_write_usage();
    let refresh_cpu_before = process_cpu_us();
    let refresh_started = Instant::now();
    let (stats, profile) = store
        .refresh_files_profiled(&changed_files)
        .expect("profile incremental refresh");
    let refresh_elapsed = refresh_started.elapsed();
    let refresh_cpu_us = process_cpu_us().saturating_sub(refresh_cpu_before);
    let refresh_usage = process_write_usage().delta(refresh_usage_before);
    let wal_bytes = fs::metadata(&wal_path)
        .map(|metadata| metadata.len())
        .unwrap_or(0);
    let wal_frames = wal_frame_count(wal_bytes, page_size);
    let wal_pages_by_object = wal_page_breakdown(store.sqlite_path(), &wal_path, page_size);
    let object_kinds = sqlite_object_kinds(store.sqlite_path());
    let index_keys = sqlite_index_keys(store.sqlite_path());

    let checkpoint_usage_before = process_write_usage();
    let passive_checkpoint = wal_checkpoint(store.sqlite_path(), "PASSIVE");
    let checkpoint_usage = process_write_usage().delta(checkpoint_usage_before);
    let truncate_checkpoint = wal_checkpoint(store.sqlite_path(), "TRUNCATE");
    sync_sqlite_file_set(store.sqlite_path());

    eprintln!("refresh_cpu_us={refresh_cpu_us}");
    eprintln!("refresh_files stats: {stats:?}");
    eprintln!("refresh_files phases: {}", profile.report());
    eprintln!(
        "refresh_io wal_bytes={wal_bytes} wal_mb={:.3} wal_frames={wal_frames} elapsed_ms={} physical_bytes={} logical_bytes={}",
        mib(wal_bytes),
        refresh_elapsed.as_millis(),
        refresh_usage.physical_bytes,
        refresh_usage.logical_bytes,
    );
    eprintln!(
        "checkpoint passive={{busy:{},log_pages:{},checkpointed_pages:{}}} truncate={{busy:{},log_pages:{},checkpointed_pages:{}}} main_pages_written={} main_mb={:.3} physical_bytes={} logical_bytes={}",
        passive_checkpoint.busy,
        passive_checkpoint.log_pages,
        passive_checkpoint.checkpointed_pages,
        truncate_checkpoint.busy,
        truncate_checkpoint.log_pages,
        truncate_checkpoint.checkpointed_pages,
        passive_checkpoint.checkpointed_pages,
        mib(passive_checkpoint.checkpointed_pages.saturating_mul(page_size)),
        checkpoint_usage.physical_bytes,
        checkpoint_usage.logical_bytes,
    );
    for (object, pages) in wal_pages_by_object {
        let bytes = pages.saturating_mul(page_size);
        let kind = object_kinds
            .get(&object)
            .map(String::as_str)
            .unwrap_or("unmapped");
        eprintln!(
            "wal_pages kind={kind} object={object} pages={pages} bytes={bytes} mb={:.3}",
            mib(bytes)
        );
        if let Some((table, columns)) = index_keys.get(&object) {
            eprintln!("wal_index_key object={object} table={table} columns={columns:?}");
        }
    }
    if count_rows {
        report_row_audit(store.sqlite_path(), store.project_root(), &changed_files);
    }
    report_dominant_phase(&profile);
    measure_snapshot_read(store.sqlite_path());
}

fn open_production_store_copy(
    temp: &TempDir,
    source_store: &Path,
    project_root: PathBuf,
    requested_file: Option<PathBuf>,
    transition: bool,
) -> (CallGraphStore, PathBuf) {
    let pointer = fs::read_dir(source_store)
        .expect("read source store directory")
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .find(|path| path.extension().and_then(|ext| ext.to_str()) == Some("current"))
        .expect("source store has a current pointer");
    let source_db = source_store.join(
        fs::read_to_string(pointer)
            .expect("read current pointer")
            .trim(),
    );
    let copied_store = temp.path().join("store-copy");
    fs::create_dir_all(&copied_store).expect("create copied store directory");
    let project_key = aft::search_index::artifact_cache_key(&project_root);
    let copied_db = copied_store.join(format!("{project_key}.sqlite"));
    sqlite_backup(&source_db, &copied_db);

    if transition {
        // The archive deliberately has next-revision bytes. Re-root only the
        // copied backend identity before open so root repair cannot cold-build
        // those bytes and silently consume the transition before timing it.
        Connection::open(&copied_db)
            .unwrap()
            .execute(
                "UPDATE backend_file_state SET workspace_root = ?1",
                [project_root.to_string_lossy().as_ref()],
            )
            .unwrap();
        let store =
            CallGraphStore::open(copied_store, project_root).expect("open copied base store");
        assert_eq!(
            store.sqlite_path(),
            copied_db,
            "opening the base must not publish a cold replacement"
        );
        return (store, PathBuf::new());
    }
    let rel_path = requested_file.unwrap_or_else(|| select_fixture_file(&copied_db));
    let changed_file = if rel_path.is_absolute() {
        rel_path
    } else {
        project_root.join(rel_path)
    };
    assert!(
        changed_file.is_file(),
        "benchmark file must exist: {}",
        changed_file.display()
    );
    force_stale(&copied_db, &project_root, &changed_file);

    let store = CallGraphStore::open(copied_store, project_root).expect("open copied store");
    (store, changed_file)
}

fn sqlite_backup(source_db: &Path, copied_db: &Path) {
    let source = Connection::open_with_flags(
        source_db,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI,
    )
    .expect("open source store read-only");
    let mut destination = Connection::open(copied_db).expect("create copied database");
    let backup = Backup::new(&source, &mut destination).expect("start SQLite online backup");
    backup
        .run_to_completion(256, Duration::from_millis(5), None)
        .expect("copy SQLite store consistently");
}

fn select_fixture_file(db: &Path) -> PathBuf {
    let conn = Connection::open(db).expect("open copied store for fixture selection");
    conn.query_row(
        "SELECT path FROM files WHERE path LIKE '%.rs' ORDER BY size DESC LIMIT 1",
        [],
        |row| row.get::<_, String>(0),
    )
    .map(PathBuf::from)
    .expect("copied store has a TypeScript file")
}

fn force_stale(db: &Path, project_root: &Path, changed_file: &Path) {
    let rel_path = changed_file
        .strip_prefix(project_root)
        .expect("benchmark file belongs to project root")
        .to_string_lossy()
        .replace('\\', "/");
    let conn = Connection::open(db).expect("open copied store for stale marker");
    let workspace_root = project_root.display().to_string();
    conn.execute(
        "UPDATE backend_file_state SET workspace_root = ?1 WHERE workspace_root <> ?1",
        params![workspace_root],
    )
    .expect("re-root copied backend state without touching the source store");
    let changed = conn
        .execute(
            "UPDATE files SET content_hash = ?1, mtime_ns = 0 WHERE path = ?2",
            params!["benchmark-forced-stale", rel_path],
        )
        .expect("force copied row stale");
    assert_eq!(changed, 1, "benchmark file must already be indexed");
}

fn build_synthetic_store(temp: &TempDir) -> (CallGraphStore, PathBuf) {
    let project_root = temp.path().join("synthetic-project");
    let src = project_root.join("src");
    fs::create_dir_all(&src).expect("create synthetic fixture");

    let large_file = src.join("large.ts");
    let mut large_source = String::from("export function symbol0() {\n  let total = 0;\n");
    for index in 0..596 {
        large_source.push_str(&format!("  total += {index};\n"));
    }
    large_source.push_str("  return total;\n}\n");
    fs::write(&large_file, large_source).expect("write large synthetic file");

    let mut files = vec![large_file.clone()];
    for index in 0..1_000 {
        let path = src.join(format!("consumer{index}.ts"));
        fs::write(
            &path,
            format!(
                "import {{ symbol0 }} from './large';\nexport function consumer{index}() {{ return symbol0(); }}\n"
            ),
        )
        .expect("write synthetic consumer");
        files.push(path);
    }

    let store = CallGraphStore::open(temp.path().join("synthetic-store"), project_root)
        .expect("open synthetic store");
    store.cold_build(&files).expect("build synthetic store");
    let changed = fs::read_to_string(&large_file)
        .expect("read synthetic changed file")
        .replace("total += 595", "total += 596");
    fs::write(&large_file, changed).expect("make synthetic file stale");
    (store, large_file)
}

fn report_query_plans(db: &Path) {
    let conn = Connection::open(db).expect("open copied store for query plans");
    for (name, sql) in [
        (
            "dependent_refs",
            "EXPLAIN QUERY PLAN SELECT DISTINCT r.ref_id FROM refs r WHERE r.caller_file IN (SELECT file_path FROM file_dependencies WHERE dep_file = 'src/large.ts') OR r.target_file = 'src/large.ts'",
        ),
        (
            "delete_edges_by_ref",
            "EXPLAIN QUERY PLAN DELETE FROM edges WHERE ref_id = 'benchmark-ref'",
        ),
        (
            "method_refs_by_caller",
            "EXPLAIN QUERY PLAN SELECT r.ref_id FROM refs r JOIN files f ON f.path = r.caller_file JOIN nodes n ON n.id = r.caller_node WHERE r.kind = 'call' AND r.status = 'unresolved' AND r.caller_file = 'src/large.ts'",
        ),
        (
            "outbound_projection",
            "EXPLAIN QUERY PLAN SELECT r.caller_file, r.caller_node, n.name, r.short_name, r.full_ref, r.status, COALESCE(r.target_file, e.target_file), COALESCE(tn.name, r.target_symbol, e.target_symbol), r.line, COALESCE(e.provenance, r.provenance), r.byte_start, r.byte_end, r.ref_id FROM refs r LEFT JOIN nodes n ON n.id = r.caller_node LEFT JOIN edges e ON e.ref_id = r.ref_id AND e.kind = r.kind LEFT JOIN nodes tn ON tn.id = e.target_node WHERE r.kind IN ('call', 'value_ref')",
        ),
        (
            "all_index_nodes",
            "EXPLAIN QUERY PLAN SELECT file_path, id, name, scoped_name, exported, is_default_export FROM nodes",
        ),
    ] {
        let mut stmt = conn.prepare(sql).expect("prepare query plan");
        let plan = stmt
            .query_map([], |row| row.get::<_, String>(3))
            .expect("run query plan")
            .collect::<Result<Vec<_>, _>>()
            .expect("collect query plan")
            .join(" | ");
        eprintln!("query_plan[{name}]: {plan}");
    }
}

fn report_refresh_row_counts(db: &Path, project_root: &Path, changed_file: &Path) {
    let rel_path = changed_file
        .strip_prefix(project_root)
        .expect("measured file belongs to project root")
        .to_string_lossy()
        .replace('\\', "/");
    let conn = Connection::open(db).expect("open store for row counts");
    for (object, sql) in [
        ("files", "SELECT count(*) FROM files WHERE path = ?1"),
        ("nodes", "SELECT count(*) FROM nodes WHERE file_path = ?1"),
        ("refs", "SELECT count(*) FROM refs WHERE caller_file = ?1"),
        (
            "edges",
            "SELECT count(*) FROM edges WHERE ref_id IN (SELECT ref_id FROM refs WHERE caller_file = ?1)",
        ),
        (
            "file_dependencies",
            "SELECT count(*) FROM file_dependencies WHERE file_path = ?1",
        ),
        (
            "dispatch_hints",
            "SELECT count(*) FROM dispatch_hints WHERE file = ?1",
        ),
        (
            "dependent_refs",
            "SELECT count(DISTINCT ref_id) FROM refs WHERE caller_file IN (SELECT file_path FROM file_dependencies WHERE dep_file = ?1) OR target_file = ?1",
        ),
    ] {
        let rows: u64 = conn
            .query_row(sql, params![rel_path], |row| row.get(0))
            .unwrap_or_else(|error| panic!("count {object} rows: {error}"));
        eprintln!("refresh_rows object={object} rows={rows}");
    }
}

#[derive(Clone, Copy, Debug)]
struct WalCheckpoint {
    busy: u64,
    log_pages: u64,
    checkpointed_pages: u64,
}

fn wal_checkpoint(db: &Path, mode: &str) -> WalCheckpoint {
    assert!(matches!(mode, "PASSIVE" | "TRUNCATE"));
    let conn = Connection::open(db).expect("open store for WAL checkpoint");
    conn.query_row(&format!("PRAGMA wal_checkpoint({mode})"), [], |row| {
        Ok(WalCheckpoint {
            busy: row.get(0)?,
            log_pages: row.get(1)?,
            checkpointed_pages: row.get(2)?,
        })
    })
    .expect("checkpoint copied store")
}

fn sqlite_page_size(db: &Path) -> u64 {
    Connection::open(db)
        .expect("open store for page size")
        .pragma_query_value(None, "page_size", |row| row.get(0))
        .expect("read SQLite page size")
}

fn sqlite_sidecar(db: &Path, suffix: &str) -> PathBuf {
    let mut name = db.as_os_str().to_os_string();
    name.push(suffix);
    PathBuf::from(name)
}

fn sync_sqlite_file_set(db: &Path) {
    for path in [
        db.to_path_buf(),
        sqlite_sidecar(db, "-wal"),
        sqlite_sidecar(db, "-shm"),
    ] {
        if let Ok(file) = fs::File::open(path) {
            file.sync_all().expect("sync copied SQLite file");
        }
    }
}

fn wal_frame_count(wal_bytes: u64, page_size: u64) -> u64 {
    wal_bytes
        .saturating_sub(32)
        .checked_div(page_size + 24)
        .unwrap_or(0)
}

fn sqlite_object_kinds(db: &Path) -> BTreeMap<String, String> {
    let conn = Connection::open(db).expect("open store for SQLite object types");
    let mut kinds = conn
        .prepare("SELECT name, type FROM sqlite_master WHERE type IN ('table', 'index')")
        .and_then(|mut statement| {
            statement
                .query_map([], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                })?
                .collect::<rusqlite::Result<BTreeMap<_, _>>>()
        })
        .unwrap_or_default();
    kinds.insert("sqlite_schema".to_string(), "table".to_string());
    kinds
}

// Autoindexes have no CREATE INDEX SQL, so query their keys through the pragma
// as well. Column names describe the SQL key, not inputs to hashed row IDs.
fn sqlite_index_keys(db: &Path) -> BTreeMap<String, (String, Vec<String>)> {
    let conn = Connection::open(db).expect("open store for index keys");
    let mut statement = conn
        .prepare("SELECT name, tbl_name FROM sqlite_master WHERE type = 'index' ORDER BY name")
        .expect("prepare index owners");
    let indexes = statement
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })
        .expect("query index owners")
        .collect::<rusqlite::Result<Vec<_>>>()
        .expect("read index owners");
    indexes
        .into_iter()
        .map(|(index, table)| {
            let columns = conn
                .prepare("SELECT name FROM pragma_index_info(?1) ORDER BY seqno")
                .expect("prepare index key columns")
                .query_map([&index], |row| row.get::<_, Option<String>>(0))
                .expect("query index key columns")
                .map(|column| {
                    column
                        .expect("read index key column")
                        .unwrap_or_else(|| "<expression>".to_string())
                })
                .collect();
            (index, (table, columns))
        })
        .collect()
}

#[test]
fn index_keys_include_autoindexes_composites_and_expressions() {
    let temp = tempfile::tempdir().unwrap();
    let db = temp.path().join("keys.sqlite");
    Connection::open(&db)
        .unwrap()
        .execute_batch(
            "CREATE TABLE refs (ref_id TEXT PRIMARY KEY, line INTEGER, byte_start INTEGER);
         CREATE INDEX positions ON refs(line, byte_start);
         CREATE INDEX computed ON refs(lower(ref_id));",
        )
        .unwrap();
    assert_eq!(
        sqlite_index_keys(&db),
        BTreeMap::from([
            (
                "sqlite_autoindex_refs_1".into(),
                ("refs".into(), vec!["ref_id".into()])
            ),
            (
                "positions".into(),
                ("refs".into(), vec!["line".into(), "byte_start".into()])
            ),
            (
                "computed".into(),
                ("refs".into(), vec!["<expression>".into()])
            ),
        ])
    );
}

fn wal_page_breakdown(db: &Path, wal: &Path, page_size: u64) -> BTreeMap<String, u64> {
    let Ok(bytes) = fs::read(wal) else {
        return BTreeMap::new();
    };
    let conn = Connection::open(db).expect("open store for WAL page mapping");
    let page_objects = conn
        .prepare("SELECT pageno, name FROM dbstat")
        .and_then(|mut statement| {
            statement
                .query_map([], |row| {
                    Ok((row.get::<_, u32>(0)?, row.get::<_, String>(1)?))
                })?
                .collect::<rusqlite::Result<BTreeMap<_, _>>>()
        })
        .unwrap_or_default();
    let mut pages = BTreeMap::new();
    let frame_size = (page_size + 24) as usize;
    let Some(frames) = bytes.get(32..) else {
        return pages;
    };
    for frame in frames.chunks_exact(frame_size) {
        let page_number = u32::from_be_bytes(frame[0..4].try_into().unwrap());
        let object = page_objects
            .get(&page_number)
            .cloned()
            .unwrap_or_else(|| "<freelist-or-unmapped>".to_string());
        *pages.entry(object).or_default() += 1;
    }
    pages
}

fn measure_snapshot_read(db: &Path) {
    let wal = sqlite_sidecar(db, "-wal");
    let wal_before = fs::metadata(&wal)
        .map(|metadata| metadata.len())
        .unwrap_or(0);
    let usage_before = process_write_usage();
    let started = Instant::now();
    let cpu_before = process_cpu_us();
    let snapshot = project_dead_code_snapshot(db).expect("project dead-code snapshot");
    let cpu_us = process_cpu_us().saturating_sub(cpu_before);
    let elapsed = started.elapsed();
    let usage = process_write_usage().delta(usage_before);
    eprintln!("snapshot_cpu_us={cpu_us}");
    let wal_after = fs::metadata(&wal)
        .map(|metadata| metadata.len())
        .unwrap_or(0);
    eprintln!(
        "snapshot_read files={} exports={} edges={} elapsed_ms={} wal_delta_bytes={} physical_bytes={} logical_bytes={}",
        snapshot.files.len(),
        snapshot.exported_symbols.len(),
        snapshot.outbound_calls.len(),
        elapsed.as_millis(),
        wal_after.saturating_sub(wal_before),
        usage.physical_bytes,
        usage.logical_bytes,
    );
}

fn process_cpu_us() -> u64 {
    #[cfg(unix)]
    {
        let mut usage = std::mem::MaybeUninit::<libc::rusage>::uninit();
        // getrusage initializes the output on success; no pointer escapes.
        if unsafe { libc::getrusage(libc::RUSAGE_SELF, usage.as_mut_ptr()) } == 0 {
            let usage = unsafe { usage.assume_init() };
            return (usage.ru_utime.tv_sec + usage.ru_stime.tv_sec) as u64 * 1_000_000
                + (usage.ru_utime.tv_usec + usage.ru_stime.tv_usec) as u64;
        }
    }
    0
}

#[derive(Clone, Copy, Debug, Default)]
struct ProcessWriteUsage {
    physical_bytes: u64,
    logical_bytes: u64,
}

impl ProcessWriteUsage {
    fn delta(self, before: Self) -> Self {
        Self {
            physical_bytes: self.physical_bytes.saturating_sub(before.physical_bytes),
            logical_bytes: self.logical_bytes.saturating_sub(before.logical_bytes),
        }
    }
}

#[cfg(target_os = "macos")]
fn process_write_usage() -> ProcessWriteUsage {
    const RUSAGE_INFO_V4: libc::c_int = 4;
    const BUFFER_BYTES: usize = 512;
    const DISK_WRITE_OFFSET: usize = 16 + 17 * std::mem::size_of::<u64>();
    const LOGICAL_WRITE_OFFSET: usize = 16 + 27 * std::mem::size_of::<u64>();

    unsafe extern "C" {
        fn proc_pid_rusage(
            pid: libc::c_int,
            flavor: libc::c_int,
            buffer: *mut libc::c_void,
        ) -> libc::c_int;
    }

    let mut buffer = [0_u8; BUFFER_BYTES];
    let result = unsafe {
        proc_pid_rusage(
            std::process::id() as libc::c_int,
            RUSAGE_INFO_V4,
            buffer.as_mut_ptr().cast(),
        )
    };
    if result != 0 {
        return ProcessWriteUsage::default();
    }
    let read_u64 = |offset: usize| {
        u64::from_ne_bytes(
            buffer[offset..offset + std::mem::size_of::<u64>()]
                .try_into()
                .unwrap(),
        )
    };
    ProcessWriteUsage {
        physical_bytes: read_u64(DISK_WRITE_OFFSET),
        logical_bytes: read_u64(LOGICAL_WRITE_OFFSET),
    }
}

#[cfg(all(unix, not(target_os = "macos")))]
fn process_write_usage() -> ProcessWriteUsage {
    unsafe {
        let mut usage = std::mem::zeroed();
        if libc::getrusage(libc::RUSAGE_SELF, &mut usage) == 0 {
            ProcessWriteUsage {
                physical_bytes: (usage.ru_oublock as u64).saturating_mul(512),
                logical_bytes: 0,
            }
        } else {
            ProcessWriteUsage::default()
        }
    }
}

// Windows has no rusage; the harness still runs there but reports zero I/O
// deltas, so the WAL-frame measurements remain the comparable numbers.
#[cfg(not(unix))]
fn process_write_usage() -> ProcessWriteUsage {
    ProcessWriteUsage::default()
}

fn mib(bytes: u64) -> f64 {
    bytes as f64 / (1024.0 * 1024.0)
}

fn report_dominant_phase(profile: &RefreshFilesProfile) {
    let phases = [
        ("parse", profile.parse),
        ("dependency_selection", profile.dependency_selection),
        ("row_deletes", profile.row_deletes),
        ("row_inserts", profile.row_inserts),
        ("dependent_parse", profile.dependent_parse),
        ("index_load", profile.index_load),
        ("ref_resolution", profile.ref_resolution),
        ("method_dispatch", profile.method_dispatch),
        ("commit", profile.commit),
    ];
    let (name, elapsed) = phases
        .into_iter()
        .max_by_key(|(_, elapsed)| *elapsed)
        .expect("profile has phases");
    eprintln!("refresh_files hot_loop: {name} ({}ms)", elapsed.as_millis());
}

#[derive(Debug)]
struct WriteAmplificationMeasurement {
    directory_delta_bytes: u64,
    output_blocks: u64,
}

/// Measure the shipped refresh configuration over 50 graph-neutral edits. The
/// legacy baseline (unconditional row replacement, `synchronous=FULL`, the
/// 1,000-page autocheckpoint) is no longer a switch in the product; to compare
/// against it, apply that behaviour as a scratch edit in `callgraph_store/mod.rs`
/// (see docs/investigations/callgraph-refresh-write-amplification-2026-09.md),
/// run this measurement, and revert. Ignored because it performs 50 edits.
#[test]
#[ignore = "offline write-amplification measurement"]
fn measure_write_amplification() {
    let measurement = run_write_amplification_sequence();
    eprintln!(
        "write_amplification directory_delta_bytes={} output_blocks={}",
        measurement.directory_delta_bytes, measurement.output_blocks,
    );
}

fn run_write_amplification_sequence() -> WriteAmplificationMeasurement {
    let temp = tempfile::tempdir().expect("measurement temp dir");
    let project_root = temp.path().join("project");
    let store_dir = project_root.join(".store-write-amp");
    fs::create_dir_all(&project_root).expect("create measurement project");
    let mut files = Vec::new();
    for index in 0..30 {
        let path = project_root.join(format!("file{index}.ts"));
        let next = (index + 1) % 30;
        fs::write(
            &path,
            format!(
                "import {{ fn{next} }} from './file{next}';\nexport function fn{index}() {{ return fn{next}(); }}\n"
            ),
        )
        .expect("write measurement source");
        files.push(path);
    }

    let (store, _) =
        CallGraphStore::cold_build_with_lease(store_dir.clone(), project_root.clone(), &files)
            .expect("cold-build measurement store");
    let mut previous_bytes = directory_bytes(&store_dir);
    let before_blocks = output_blocks();
    let mut directory_delta_bytes = 0;
    for edit in 0..50 {
        let path = &files[edit % files.len()];
        let mut source = fs::read_to_string(path).expect("read measurement source");
        source.push_str(&format!("// whitespace-preserving edit {edit}\n"));
        fs::write(path, source).expect("write measurement edit");
        store
            .refresh_files(std::slice::from_ref(path))
            .expect("refresh measurement edit");
        let current_bytes = directory_bytes(&store_dir);
        directory_delta_bytes += current_bytes.abs_diff(previous_bytes);
        previous_bytes = current_bytes;
    }
    drop(store);
    let measurement = WriteAmplificationMeasurement {
        directory_delta_bytes,
        output_blocks: output_blocks().saturating_sub(before_blocks),
    };
    measurement
}

fn directory_bytes(path: &Path) -> u64 {
    let mut total = 0;
    for entry in fs::read_dir(path).expect("read measurement store") {
        let entry = entry.expect("read measurement entry");
        let metadata = entry.metadata().expect("stat measurement entry");
        total += if metadata.is_dir() {
            directory_bytes(&entry.path())
        } else {
            metadata.len()
        };
    }
    total
}

#[cfg(unix)]
fn output_blocks() -> u64 {
    unsafe {
        let mut usage = std::mem::zeroed();
        if libc::getrusage(libc::RUSAGE_SELF, &mut usage) == 0 {
            usage.ru_oublock as u64
        } else {
            0
        }
    }
}

#[cfg(not(unix))]
fn output_blocks() -> u64 {
    0
}

fn read_transition_paths(list: &Path, root: &Path) -> Vec<PathBuf> {
    let text = fs::read_to_string(list).expect("read transition path list");
    let paths: std::collections::BTreeSet<_> = text
        .lines()
        .map(|line| {
            let path = Path::new(line);
            assert!(
                !line.is_empty()
                    && path
                        .components()
                        .all(|part| matches!(part, std::path::Component::Normal(_))),
                "transition paths must be non-empty project-relative paths"
            );
            root.join(path)
        })
        .collect();
    assert!(!paths.is_empty(), "transition path list must not be empty");
    paths.into_iter().collect()
}

#[test]
fn transition_paths_include_removed_files_and_deduplicate() {
    let temp = tempfile::tempdir().unwrap();
    let list = temp.path().join("paths.txt");
    fs::write(&list, "src/removed.ts\nsrc/added.ts\nsrc/removed.ts\n").unwrap();
    assert_eq!(
        read_transition_paths(&list, temp.path()),
        vec![
            temp.path().join("src/added.ts"),
            temp.path().join("src/removed.ts")
        ]
    );
}

#[test]
#[should_panic(expected = "project-relative paths")]
fn transition_paths_reject_parent_escape() {
    let temp = tempfile::tempdir().unwrap();
    let list = temp.path().join("paths.txt");
    fs::write(&list, "../outside.ts\n").unwrap();
    read_transition_paths(&list, temp.path());
}

fn install_row_audit(db: &Path) {
    let conn = Connection::open(db).expect("open private store for row audit");
    let tables = conn
        .prepare("SELECT name FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%'")
        .unwrap()
        .query_map([], |row| row.get::<_, String>(0))
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap();
    conn.execute_batch("CREATE TABLE bench_row_audit (object TEXT, operation TEXT, caller TEXT, rows INTEGER, PRIMARY KEY(object, operation, caller))").unwrap();
    for (index, table) in tables.iter().enumerate() {
        let identifier = table.replace('"', "\"\"");
        let literal = table.replace('\'', "''");
        for operation in ["INSERT", "UPDATE", "DELETE"] {
            let caller = if table == "refs" {
                if operation == "DELETE" {
                    "OLD.caller_file"
                } else {
                    "NEW.caller_file"
                }
            } else {
                "''"
            };
            conn.execute_batch(&format!("CREATE TRIGGER bench_audit_{index}_{operation} AFTER {operation} ON \"{identifier}\" BEGIN INSERT INTO bench_row_audit VALUES ('{literal}', '{operation}', {caller}, 1) ON CONFLICT(object, operation, caller) DO UPDATE SET rows=rows+1; END;")).unwrap();
        }
    }
}

fn report_row_audit(db: &Path, root: &Path, changed: &[PathBuf]) {
    let conn = Connection::open(db).unwrap();
    let mut statement = conn.prepare("SELECT object, operation, sum(rows) FROM bench_row_audit GROUP BY object, operation ORDER BY object, operation").unwrap();
    let rows = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, u64>(2)?,
            ))
        })
        .unwrap();
    for row in rows {
        let (object, operation, rows) = row.unwrap();
        eprintln!("audit_rows object={object} operation={operation} rows={rows}");
    }
    let callers = conn.prepare("SELECT DISTINCT caller FROM bench_row_audit WHERE object='refs' AND operation='INSERT'").unwrap().query_map([], |row| row.get::<_, String>(0)).unwrap().collect::<rusqlite::Result<Vec<_>>>().unwrap();
    let importers = callers
        .iter()
        .filter(|caller| !changed.contains(&root.join(caller)))
        .count();
    eprintln!(
        "audit_ref_callers={} audit_unchanged_importers={importers} timing_includes_audit=true",
        callers.len()
    );
}

#[test]
fn row_audit_counts_insert_update_delete_and_reference_callers() {
    let temp = tempfile::tempdir().unwrap();
    let db = temp.path().join("audit.sqlite");
    let conn = Connection::open(&db).unwrap();
    conn.execute_batch("CREATE TABLE refs (ref_id TEXT PRIMARY KEY, caller_file TEXT); CREATE TABLE files (path TEXT PRIMARY KEY);").unwrap();
    install_row_audit(&db);
    conn.execute_batch("INSERT INTO refs VALUES ('a', 'caller.ts'), ('b', 'caller.ts'); UPDATE refs SET caller_file='other.ts' WHERE ref_id='b'; DELETE FROM refs WHERE ref_id='a'; INSERT INTO files VALUES ('caller.ts');").unwrap();
    let rows: Vec<(String, String, String, u64)> = conn.prepare("SELECT object, operation, caller, rows FROM bench_row_audit ORDER BY object, operation, caller").unwrap().query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))).unwrap().collect::<rusqlite::Result<_>>().unwrap();
    assert_eq!(
        rows,
        vec![
            ("files".into(), "INSERT".into(), "".into(), 1),
            ("refs".into(), "DELETE".into(), "caller.ts".into(), 1),
            ("refs".into(), "INSERT".into(), "caller.ts".into(), 2),
            ("refs".into(), "UPDATE".into(), "other.ts".into(), 1)
        ]
    );
}

#[test]
fn transition_copy_does_not_refresh_next_revision_during_open() {
    let source = tempfile::tempdir().unwrap();
    let root = source.path().join("project");
    fs::create_dir(&root).unwrap();
    let file = root.join("caller.ts");
    fs::write(&file, "export function before() {}\n").unwrap();
    let store_dir = source.path().join("store");
    let store = CallGraphStore::open(store_dir.clone(), root).unwrap();
    store.cold_build(&[file]).unwrap();
    let file_hash = |db: &Path| {
        Connection::open(db)
            .unwrap()
            .query_row(
                "SELECT content_hash FROM files WHERE path='caller.ts'",
                [],
                |row| row.get::<_, String>(0),
            )
            .unwrap()
    };
    let before_hash = file_hash(store.sqlite_path());
    fs::write(
        store_dir.join("bench.current"),
        store.sqlite_path().file_name().unwrap().to_str().unwrap(),
    )
    .unwrap();
    let next = source.path().join("next");
    fs::create_dir(&next).unwrap();
    let file = next.join("caller.ts");
    fs::write(&file, "export function after() {}\n").unwrap();
    let temp = tempfile::tempdir().unwrap();
    let (copied, _) = open_production_store_copy(&temp, &store_dir, next, None, true);
    assert_eq!(file_hash(copied.sqlite_path()), before_hash);
    let stats = copied.refresh_files(&[file]).unwrap();
    assert_eq!(stats.refreshed_own_files, 1);
    assert_ne!(file_hash(copied.sqlite_path()), before_hash);
}
