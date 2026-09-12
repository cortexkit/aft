//! Offline measurement harness for one-file incremental callgraph refreshes.
//!
//! Run against a copy of a production store:
//! `AFT_CALLGRAPH_REFRESH_STORE=/path/to/<root-key> AFT_CALLGRAPH_REFRESH_ROOT=/path/to/project cargo test -p agent-file-tools --test callgraph_refresh_bench -- --ignored --nocapture`
//! Set `AFT_CALLGRAPH_REFRESH_FILE` to choose a project-relative file. Without
//! these variables the harness builds a synthetic store with a large fixture.

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
    let (store, changed_file) = match (
        std::env::var_os("AFT_CALLGRAPH_REFRESH_STORE"),
        std::env::var_os("AFT_CALLGRAPH_REFRESH_ROOT"),
    ) {
        (Some(source_store), Some(project_root)) => open_production_store_copy(
            &temp,
            Path::new(&source_store),
            PathBuf::from(project_root),
            std::env::var_os("AFT_CALLGRAPH_REFRESH_FILE").map(PathBuf::from),
        ),
        (None, None) => build_synthetic_store(&temp),
        _ => panic!(
            "AFT_CALLGRAPH_REFRESH_STORE and AFT_CALLGRAPH_REFRESH_ROOT must be set together"
        ),
    };

    let warmup = store
        .refresh_files(std::slice::from_ref(&changed_file))
        .expect("normalize copied rows with the measured binary");
    eprintln!("measurement_warmup stats={warmup:?}");
    force_stale(store.sqlite_path(), store.project_root(), &changed_file);

    report_query_plans(store.sqlite_path());
    report_refresh_row_counts(store.sqlite_path(), store.project_root(), &changed_file);
    let initial_checkpoint = wal_checkpoint(store.sqlite_path(), "TRUNCATE");
    assert_eq!(initial_checkpoint.busy, 0, "clear WAL before measurement");
    sync_sqlite_file_set(store.sqlite_path());
    let page_size = sqlite_page_size(store.sqlite_path());
    let wal_path = sqlite_sidecar(store.sqlite_path(), "-wal");
    let refresh_usage_before = process_write_usage();
    let refresh_cpu_before = process_cpu_us();
    let refresh_started = Instant::now();
    let (stats, profile) = store
        .refresh_files_profiled(std::slice::from_ref(&changed_file))
        .expect("profile one-file refresh");
    let refresh_elapsed = refresh_started.elapsed();
    let refresh_cpu_us = process_cpu_us().saturating_sub(refresh_cpu_before);
    let refresh_usage = process_write_usage().delta(refresh_usage_before);
    let wal_bytes = fs::metadata(&wal_path)
        .map(|metadata| metadata.len())
        .unwrap_or(0);
    let wal_frames = wal_frame_count(wal_bytes, page_size);
    let wal_pages_by_object = wal_page_breakdown(store.sqlite_path(), &wal_path, page_size);

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
        eprintln!(
            "wal_pages object={object} pages={pages} mb={:.3}",
            mib(pages.saturating_mul(page_size))
        );
    }
    report_dominant_phase(&profile);
    measure_snapshot_read(store.sqlite_path());
}

fn open_production_store_copy(
    temp: &TempDir,
    source_store: &Path,
    project_root: PathBuf,
    requested_file: Option<PathBuf>,
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

/// Compare the shipped configuration with the legacy full-scan configuration,
/// which scans 1,000 pages and skips row-difference checks. Keep this test ignored
/// because it performs 50 edits for an offline write-amplification measurement.
#[test]
#[ignore = "offline write-amplification measurement"]
fn measure_write_amplification_ab() {
    let baseline = run_write_amplification_sequence(true);
    let optimized = run_write_amplification_sequence(false);
    let byte_ratio = if baseline.directory_delta_bytes == 0 {
        None
    } else {
        Some(optimized.directory_delta_bytes as f64 / baseline.directory_delta_bytes as f64)
    };
    let block_ratio = if baseline.output_blocks == 0 {
        None
    } else {
        Some(optimized.output_blocks as f64 / baseline.output_blocks as f64)
    };
    eprintln!(
        "write_amplification_ab baseline={{directory_delta_bytes:{}, output_blocks:{}}} optimized={{directory_delta_bytes:{}, output_blocks:{}}} ratios={{bytes:{byte_ratio:?}, blocks:{block_ratio:?}}}",
        baseline.directory_delta_bytes,
        baseline.output_blocks,
        optimized.directory_delta_bytes,
        optimized.output_blocks,
    );
}

fn run_write_amplification_sequence(baseline: bool) -> WriteAmplificationMeasurement {
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

    let previous = std::env::var_os("AFT_CALLGRAPH_WRITE_AMP_BASELINE");
    if baseline {
        std::env::set_var("AFT_CALLGRAPH_WRITE_AMP_BASELINE", "1");
    } else {
        std::env::remove_var("AFT_CALLGRAPH_WRITE_AMP_BASELINE");
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
    match previous {
        Some(value) => std::env::set_var("AFT_CALLGRAPH_WRITE_AMP_BASELINE", value),
        None => std::env::remove_var("AFT_CALLGRAPH_WRITE_AMP_BASELINE"),
    }
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
