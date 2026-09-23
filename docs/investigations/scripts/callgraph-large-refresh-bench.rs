//! Offline measurement harness for large incremental callgraph refreshes,
//! compared against a cold build of the same revision and the tier-2 dead-code
//! snapshot that follows. It is not part of the build: copy it to
//! `crates/aft/tests/callgraph_large_refresh_bench.rs` and run it with
//! `--ignored`. See docs/investigations/callgraph-large-refresh-2026-09.md.
//!
//! One process measures one operation so process-lifetime peaks (max RSS,
//! lifetime-max phys_footprint) belong to that operation alone. Driven by env:
//!
//! - `AFT_LRB_MODE`: `cold`, `refresh`, or `snapshot`.
//! - `AFT_LRB_STORE`: callgraph store directory (the `callgraph_dir`).
//! - `AFT_LRB_ROOT`: project root (the checkout the store describes).
//! - `AFT_LRB_PATHS`: refresh only; newline-delimited project-relative paths,
//!   filtered by `parser::detect_language` exactly like the refresh worker.
//! - `AFT_LRB_REPEAT=1`: refresh only; run the same refresh a second time on the
//!   same store, as the tier-2 dead-code job does after the watcher refresh.
//! - `AFT_LRB_COUNT_ROWS=1`: refresh only; install per-table row-audit triggers
//!   (a separate work-count pass: its timings include trigger overhead).
//! - `AFT_LRB_WAL_BREAKDOWN=1`: refresh only; map WAL frames to tables/indexes.
//! - `AFT_LRB_OUT`: directory for `samples.csv` (200 ms memory time series).
//! - `AFT_LRB_PAUSE_FILE`: after the measured operation, write the pid to this
//!   file and wait until it is deleted, so `malloc_history -highWaterMark` can
//!   inspect the process (run it under `MallocStackLogging=1`).

use aft::callgraph_store::{project_dead_code_snapshot, CallGraphStore};
use rusqlite::{Connection, OpenFlags};
use std::alloc::{GlobalAlloc, Layout, System};
use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

// ---------------------------------------------------------------------------
// Counting allocator: live bytes, peak live bytes, cumulative bytes and calls,
// and a per-size-class call histogram. It wraps the system allocator, which is
// also what the daemon uses (macOS malloc zones).
// ---------------------------------------------------------------------------

struct CountingAllocator;

static LIVE: AtomicU64 = AtomicU64::new(0);
static PEAK: AtomicU64 = AtomicU64::new(0);
static TOTAL_BYTES: AtomicU64 = AtomicU64::new(0);
static TOTAL_CALLS: AtomicU64 = AtomicU64::new(0);
// Size classes: <=1 KiB (malloc tiny), <=128 KiB (small), <=1 MiB, >1 MiB.
static CLASS_CALLS: [AtomicU64; 4] = [
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
];
static CLASS_BYTES: [AtomicU64; 4] = [
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
];

fn size_class(size: usize) -> usize {
    match size {
        0..=1024 => 0,
        1025..=131_072 => 1,
        131_073..=1_048_576 => 2,
        _ => 3,
    }
}

fn note_alloc(size: usize) {
    let size64 = size as u64;
    let live = LIVE.fetch_add(size64, Ordering::Relaxed) + size64;
    PEAK.fetch_max(live, Ordering::Relaxed);
    TOTAL_BYTES.fetch_add(size64, Ordering::Relaxed);
    TOTAL_CALLS.fetch_add(1, Ordering::Relaxed);
    let class = size_class(size);
    CLASS_CALLS[class].fetch_add(1, Ordering::Relaxed);
    CLASS_BYTES[class].fetch_add(size64, Ordering::Relaxed);
}

unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let ptr = unsafe { System.alloc(layout) };
        if !ptr.is_null() {
            note_alloc(layout.size());
        }
        ptr
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        let ptr = unsafe { System.alloc_zeroed(layout) };
        if !ptr.is_null() {
            note_alloc(layout.size());
        }
        ptr
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) };
        LIVE.fetch_sub(layout.size() as u64, Ordering::Relaxed);
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let new_ptr = unsafe { System.realloc(ptr, layout, new_size) };
        if !new_ptr.is_null() {
            LIVE.fetch_sub(layout.size() as u64, Ordering::Relaxed);
            note_alloc(new_size);
        }
        new_ptr
    }
}

#[global_allocator]
static GLOBAL: CountingAllocator = CountingAllocator;

#[derive(Clone, Copy, Default, Debug)]
struct AllocCounters {
    live: u64,
    peak: u64,
    total_bytes: u64,
    total_calls: u64,
    class_calls: [u64; 4],
    class_bytes: [u64; 4],
}

fn alloc_counters() -> AllocCounters {
    let mut counters = AllocCounters {
        live: LIVE.load(Ordering::Relaxed),
        peak: PEAK.load(Ordering::Relaxed),
        total_bytes: TOTAL_BYTES.load(Ordering::Relaxed),
        total_calls: TOTAL_CALLS.load(Ordering::Relaxed),
        ..Default::default()
    };
    for class in 0..4 {
        counters.class_calls[class] = CLASS_CALLS[class].load(Ordering::Relaxed);
        counters.class_bytes[class] = CLASS_BYTES[class].load(Ordering::Relaxed);
    }
    counters
}

/// Restart the peak at the current live size so the next operation's peak is
/// its own.
fn reset_peak() {
    PEAK.store(LIVE.load(Ordering::Relaxed), Ordering::Relaxed);
}

// ---------------------------------------------------------------------------
// Darwin process accounting (rusage_info_v4 field offsets: 16-byte uuid, then
// u64 fields; 6 resident, 7 phys_footprint, 17 diskio_byteswritten,
// 27 logical_writes, 28 lifetime_max_phys_footprint).
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Default, Debug)]
struct ProcSample {
    resident: u64,
    footprint: u64,
    physical_written: u64,
    logical_written: u64,
    lifetime_max_footprint: u64,
}

#[cfg(target_os = "macos")]
fn proc_sample() -> ProcSample {
    const RUSAGE_INFO_V4: libc::c_int = 4;
    unsafe extern "C" {
        fn proc_pid_rusage(
            pid: libc::c_int,
            flavor: libc::c_int,
            buffer: *mut libc::c_void,
        ) -> libc::c_int;
    }
    let mut buffer = [0_u8; 512];
    let result = unsafe {
        proc_pid_rusage(
            std::process::id() as libc::c_int,
            RUSAGE_INFO_V4,
            buffer.as_mut_ptr().cast(),
        )
    };
    if result != 0 {
        return ProcSample::default();
    }
    let field = |index: usize| {
        let offset = 16 + index * 8;
        u64::from_ne_bytes(buffer[offset..offset + 8].try_into().unwrap())
    };
    ProcSample {
        resident: field(6),
        footprint: field(7),
        physical_written: field(17),
        logical_written: field(27),
        lifetime_max_footprint: field(28),
    }
}

#[cfg(not(target_os = "macos"))]
fn proc_sample() -> ProcSample {
    ProcSample::default()
}

fn max_rss_bytes() -> u64 {
    let mut usage = std::mem::MaybeUninit::<libc::rusage>::uninit();
    if unsafe { libc::getrusage(libc::RUSAGE_SELF, usage.as_mut_ptr()) } != 0 {
        return 0;
    }
    let usage = unsafe { usage.assume_init() };
    // macOS reports ru_maxrss in bytes; Linux in KiB.
    if cfg!(target_os = "macos") {
        usage.ru_maxrss as u64
    } else {
        usage.ru_maxrss as u64 * 1024
    }
}

fn cpu_ms() -> u64 {
    let mut usage = std::mem::MaybeUninit::<libc::rusage>::uninit();
    if unsafe { libc::getrusage(libc::RUSAGE_SELF, usage.as_mut_ptr()) } != 0 {
        return 0;
    }
    let usage = unsafe { usage.assume_init() };
    ((usage.ru_utime.tv_sec + usage.ru_stime.tv_sec) as u64) * 1000
        + ((usage.ru_utime.tv_usec + usage.ru_stime.tv_usec) as u64) / 1000
}

/// Samples live heap, footprint and RSS every 200 ms; tracks the maxima so a
/// transient peak between two process-level reads is still reported.
struct Sampler {
    stop: Arc<AtomicBool>,
    max_footprint: Arc<AtomicU64>,
    max_resident: Arc<AtomicU64>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl Sampler {
    fn start(label: &str) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let max_footprint = Arc::new(AtomicU64::new(0));
        let max_resident = Arc::new(AtomicU64::new(0));
        let mut out = std::env::var_os("AFT_LRB_OUT").map(|dir| {
            let dir = PathBuf::from(dir);
            fs::create_dir_all(&dir).unwrap();
            let mut file = fs::File::create(dir.join(format!("samples-{label}.csv"))).unwrap();
            writeln!(file, "ms,live_heap,phys_footprint,resident").unwrap();
            file
        });
        let (stop2, fp2, rss2) = (stop.clone(), max_footprint.clone(), max_resident.clone());
        let started = Instant::now();
        let handle = std::thread::spawn(move || loop {
            let sample = proc_sample();
            fp2.fetch_max(sample.footprint, Ordering::Relaxed);
            rss2.fetch_max(sample.resident, Ordering::Relaxed);
            if let Some(file) = out.as_mut() {
                let _ = writeln!(
                    file,
                    "{},{},{},{}",
                    started.elapsed().as_millis(),
                    LIVE.load(Ordering::Relaxed),
                    sample.footprint,
                    sample.resident
                );
            }
            if stop2.load(Ordering::Relaxed) {
                break;
            }
            std::thread::sleep(Duration::from_millis(200));
        });
        Self {
            stop,
            max_footprint,
            max_resident,
            handle: Some(handle),
        }
    }

    fn finish(mut self) -> (u64, u64) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(handle) = self.handle.take() {
            handle.join().unwrap();
        }
        (
            self.max_footprint.load(Ordering::Relaxed),
            self.max_resident.load(Ordering::Relaxed),
        )
    }
}

fn mib(bytes: u64) -> f64 {
    bytes as f64 / (1024.0 * 1024.0)
}

/// Run one measured operation and print a single `lrb_measure` line.
fn measure<T>(label: &str, operation: impl FnOnce() -> T) -> T {
    reset_peak();
    let before_alloc = alloc_counters();
    let before_proc = proc_sample();
    let before_cpu = cpu_ms();
    let sampler = Sampler::start(label);
    let started = Instant::now();
    let result = operation();
    let elapsed = started.elapsed();
    let (sampled_max_footprint, sampled_max_resident) = sampler.finish();
    let after_proc = proc_sample();
    let after_alloc = alloc_counters();
    let class = |index: usize| {
        format!(
            "{}calls/{:.0}MiB",
            after_alloc.class_calls[index] - before_alloc.class_calls[index],
            mib(after_alloc.class_bytes[index] - before_alloc.class_bytes[index])
        )
    };
    eprintln!(
        "lrb_measure op={label} elapsed_ms={} cpu_ms={} physical_written_mib={:.1} logical_written_mib={:.1} \
         heap_live_before_mib={:.1} heap_peak_mib={:.1} heap_live_after_mib={:.1} \
         alloc_total_mib={:.1} alloc_calls={} class_le1k={} class_le128k={} class_le1m={} class_gt1m={} \
         sampled_max_footprint_mib={:.1} sampled_max_resident_mib={:.1} lifetime_max_footprint_mib={:.1} max_rss_mib={:.1} \
         footprint_after_mib={:.1}",
        elapsed.as_millis(),
        cpu_ms() - before_cpu,
        mib(after_proc.physical_written - before_proc.physical_written),
        mib(after_proc.logical_written - before_proc.logical_written),
        mib(before_alloc.live),
        mib(after_alloc.peak),
        mib(after_alloc.live),
        mib(after_alloc.total_bytes - before_alloc.total_bytes),
        after_alloc.total_calls - before_alloc.total_calls,
        class(0),
        class(1),
        class(2),
        class(3),
        mib(sampled_max_footprint),
        mib(sampled_max_resident),
        mib(after_proc.lifetime_max_footprint),
        mib(max_rss_bytes()),
        mib(after_proc.footprint),
    );
    result
}

// ---------------------------------------------------------------------------
// SQLite helpers.
// ---------------------------------------------------------------------------

fn current_db(store_dir: &Path) -> PathBuf {
    let pointer = fs::read_dir(store_dir)
        .expect("read store directory")
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .find(|path| path.extension().and_then(|ext| ext.to_str()) == Some("current"))
        .expect("store has a current pointer");
    store_dir.join(fs::read_to_string(pointer).unwrap().trim())
}

fn sidecar(db: &Path, suffix: &str) -> PathBuf {
    let mut name = db.as_os_str().to_os_string();
    name.push(suffix);
    PathBuf::from(name)
}

fn file_len(path: &Path) -> u64 {
    fs::metadata(path).map(|m| m.len()).unwrap_or(0)
}

fn table_counts(db: &Path) -> String {
    let conn = Connection::open_with_flags(db, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
    let mut parts = Vec::new();
    for table in [
        "files",
        "nodes",
        "refs",
        "edges",
        "file_dependencies",
        "dispatch_hints",
    ] {
        let count: i64 = conn
            .query_row(&format!("SELECT count(*) FROM {table}"), [], |row| row.get(0))
            .unwrap_or(-1);
        parts.push(format!("{table}={count}"));
    }
    let page_count: i64 = conn
        .pragma_query_value(None, "page_count", |row| row.get(0))
        .unwrap_or(0);
    let freelist: i64 = conn
        .pragma_query_value(None, "freelist_count", |row| row.get(0))
        .unwrap_or(0);
    parts.push(format!("page_count={page_count} freelist={freelist}"));
    parts.join(" ")
}

fn wal_checkpoint(db: &Path, mode: &str) -> (i64, i64, i64) {
    let conn = Connection::open(db).unwrap();
    conn.query_row(&format!("PRAGMA wal_checkpoint({mode})"), [], |row| {
        Ok((row.get(0)?, row.get(1)?, row.get(2)?))
    })
    .unwrap()
}

fn wal_page_breakdown(db: &Path, wal: &Path) -> BTreeMap<String, u64> {
    let Ok(bytes) = fs::read(wal) else {
        return BTreeMap::new();
    };
    let conn = Connection::open(db).unwrap();
    let page_size: u64 = conn
        .pragma_query_value(None, "page_size", |row| row.get(0))
        .unwrap();
    let page_objects: BTreeMap<u32, String> = conn
        .prepare("SELECT pageno, name FROM dbstat")
        .and_then(|mut statement| {
            statement
                .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
                .collect()
        })
        .unwrap_or_default();
    let mut pages = BTreeMap::new();
    let frame_size = (page_size + 24) as usize;
    if let Some(frames) = bytes.get(32..) {
        for frame in frames.chunks_exact(frame_size) {
            let page = u32::from_be_bytes(frame[0..4].try_into().unwrap());
            let object = page_objects
                .get(&page)
                .cloned()
                .unwrap_or_else(|| "<freelist-or-new>".to_string());
            *pages.entry(object).or_default() += 1;
        }
    }
    pages
}

fn install_row_audit(db: &Path) {
    let conn = Connection::open(db).unwrap();
    let tables: Vec<String> = conn
        .prepare("SELECT name FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%'")
        .unwrap()
        .query_map([], |row| row.get(0))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    conn.execute_batch(
        "CREATE TABLE lrb_row_audit (object TEXT, operation TEXT, rows INTEGER, PRIMARY KEY(object, operation))",
    )
    .unwrap();
    for (index, table) in tables.iter().enumerate() {
        for operation in ["INSERT", "UPDATE", "DELETE"] {
            conn.execute_batch(&format!(
                "CREATE TRIGGER lrb_audit_{index}_{operation} AFTER {operation} ON \"{table}\" BEGIN \
                 INSERT INTO lrb_row_audit VALUES ('{table}', '{operation}', 1) \
                 ON CONFLICT(object, operation) DO UPDATE SET rows=rows+1; END;"
            ))
            .unwrap();
        }
    }
}

fn report_row_audit(db: &Path) {
    let conn = Connection::open(db).unwrap();
    let mut statement = conn
        .prepare("SELECT object, operation, rows FROM lrb_row_audit ORDER BY object, operation")
        .unwrap();
    let rows = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, i64>(2)?,
            ))
        })
        .unwrap();
    for row in rows {
        let (object, operation, rows) = row.unwrap();
        eprintln!("lrb_audit object={object} operation={operation} rows={rows}");
    }
}

fn read_paths(list: &Path, root: &Path) -> Vec<PathBuf> {
    let text = fs::read_to_string(list).expect("read path list");
    let mut paths: Vec<PathBuf> = text
        .lines()
        .filter(|line| !line.is_empty())
        .map(|line| root.join(line))
        .filter(|path| aft::parser::detect_language(path).is_some())
        .collect();
    paths.sort();
    paths.dedup();
    paths
}

fn pause_for_inspection() {
    let Some(pause) = std::env::var_os("AFT_LRB_PAUSE_FILE") else {
        return;
    };
    let pause = PathBuf::from(pause);
    fs::write(&pause, std::process::id().to_string()).unwrap();
    eprintln!("lrb_pause pid={} file={}", std::process::id(), pause.display());
    while pause.exists() {
        std::thread::sleep(Duration::from_millis(500));
    }
}

// ---------------------------------------------------------------------------
// Modes.
// ---------------------------------------------------------------------------

#[test]
#[ignore = "offline benchmark over a large external checkout"]
fn large_refresh_bench() {
    let mode = std::env::var("AFT_LRB_MODE").expect("AFT_LRB_MODE");
    let store_dir = PathBuf::from(std::env::var_os("AFT_LRB_STORE").expect("AFT_LRB_STORE"));
    let root = PathBuf::from(std::env::var_os("AFT_LRB_ROOT").expect("AFT_LRB_ROOT"));
    match mode.as_str() {
        "cold" => run_cold(&store_dir, &root),
        "refresh" => run_refresh(&store_dir, &root),
        "snapshot" => run_snapshot(&store_dir),
        other => panic!("unknown AFT_LRB_MODE {other}"),
    }
    pause_for_inspection();
}

fn run_cold(store_dir: &Path, root: &Path) {
    // Mirror the daemon: an empty file list lets the builder walk the root
    // into its staging table; chunk size is the config default (100).
    let (store, stats) = measure("cold_build", || {
        CallGraphStore::cold_build_with_lease_chunked(
            store_dir.to_path_buf(),
            root.to_path_buf(),
            &[],
            100,
        )
        .expect("cold build")
    });
    eprintln!("lrb_cold_stats {stats:?}");
    let db = store.sqlite_path().to_path_buf();
    drop(store);
    eprintln!(
        "lrb_store db_bytes={} wal_bytes={} {}",
        file_len(&db),
        file_len(&sidecar(&db, "-wal")),
        table_counts(&db)
    );
}

fn run_refresh(store_dir: &Path, root: &Path) {
    let list = PathBuf::from(std::env::var_os("AFT_LRB_PATHS").expect("AFT_LRB_PATHS"));
    let paths = read_paths(&list, root);
    let db = current_db(store_dir);
    // Start from an empty WAL so the WAL size after refresh is this refresh's.
    eprintln!("lrb_initial_checkpoint {:?}", wal_checkpoint(&db, "TRUNCATE"));
    eprintln!(
        "lrb_store_before db_bytes={} {}",
        file_len(&db),
        table_counts(&db)
    );
    let count_rows = std::env::var_os("AFT_LRB_COUNT_ROWS").is_some();
    if count_rows {
        install_row_audit(&db);
    }
    let store = CallGraphStore::open_ready(store_dir.to_path_buf(), root.to_path_buf())
        .expect("open ready store")
        .expect("store is ready and writer lease is free");
    assert_eq!(store.sqlite_path(), db, "open must not publish a new generation");
    eprintln!("lrb_refresh_paths={}", paths.len());
    let passes = if std::env::var_os("AFT_LRB_REPEAT").is_some() {
        2
    } else {
        1
    };
    for pass in 1..=passes {
        let label = format!("refresh_pass{pass}");
        let (stats, profile) = measure(&label, || {
            store.refresh_files_profiled(&paths).expect("refresh")
        });
        eprintln!(
            "lrb_refresh_stats pass={pass} changed={} surface_changed={} deleted={} dependency_selected_refs={} refreshed_own_files={} unchanged_extract_files={} skipped_out_of_root={}",
            stats.changed_files.len(),
            stats.surface_changed.len(),
            stats.deleted_files.len(),
            stats.dependency_selected_refs,
            stats.refreshed_own_files,
            stats.unchanged_extract_files,
            stats.skipped_out_of_root.len(),
        );
        eprintln!("lrb_refresh_profile pass={pass} {}", profile.report());
        let wal = sidecar(&db, "-wal");
        eprintln!(
            "lrb_refresh_wal pass={pass} wal_bytes={} wal_mib={:.1}",
            file_len(&wal),
            mib(file_len(&wal))
        );
        if std::env::var_os("AFT_LRB_WAL_BREAKDOWN").is_some() {
            let mut rows: Vec<_> = wal_page_breakdown(&db, &wal).into_iter().collect();
            rows.sort_by_key(|(_, pages)| std::cmp::Reverse(*pages));
            for (object, pages) in rows {
                eprintln!(
                    "lrb_wal_pages pass={pass} object={object} pages={pages} mib={:.1}",
                    mib(pages * 4096)
                );
            }
        }
        let before = proc_sample();
        let checkpoint = wal_checkpoint(&db, "TRUNCATE");
        let after = proc_sample();
        eprintln!(
            "lrb_checkpoint pass={pass} result={checkpoint:?} physical_written_mib={:.1}",
            mib(after.physical_written - before.physical_written)
        );
    }
    if count_rows {
        report_row_audit(&db);
    }
    drop(store);
    eprintln!(
        "lrb_store_after db_bytes={} {}",
        file_len(&db),
        table_counts(&db)
    );
}

fn run_snapshot(store_dir: &Path) {
    let db = current_db(store_dir);
    let snapshot = measure("dead_code_snapshot", || {
        project_dead_code_snapshot(&db).expect("dead-code snapshot")
    });
    eprintln!(
        "lrb_snapshot files={} exports={} edges={} entry_points={}",
        snapshot.files.len(),
        snapshot.exported_symbols.len(),
        snapshot.outbound_calls.len(),
        snapshot.entry_points.len(),
    );
}
