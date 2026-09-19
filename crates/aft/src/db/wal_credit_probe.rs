//! Offline reconciliation of the write ledger against the bytes SQLite pushes
//! into a callgraph store, measured on a copy of a real store.
//!
//! `fs_usage` attributes kernel disk I/O per file but needs root, so it cannot
//! run inside a test. This module stands in for it with a SQLite VFS that wraps
//! the platform VFS and counts `xWrite` bytes per file class, which is the same
//! quantity split the same way: main database file, WAL, rollback journal.
//!
//! The probe is `#[ignore]`d and reads its store from an environment variable.
//! It needs a multi-hundred-megabyte real store to say anything about a real
//! store, and it copies whatever it is given before touching it.
//!
//! ```text
//! AFT_WAL_CREDIT_PROBE_STORE=/tmp/aft-cg-probe/prefrontal.sqlite \
//!   cargo test --lib db::wal_credit_probe -- --ignored --nocapture
//! ```

use std::collections::BTreeMap;
use std::ffi::CStr;
use std::os::raw::{c_char, c_int, c_void};
use std::path::Path;
use std::sync::{Mutex, OnceLock};

use rusqlite::ffi;
use rusqlite::{Connection, OpenFlags};

use super::lifecycle::{SqliteStore, TrackedConnection};
use crate::write_ledger::Domain as WriteDomain;

/// The file SQLite was writing to, taken from the open flags rather than the
/// name so a rollback journal cannot hide behind an unexpected suffix.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum FileClass {
    MainDb,
    Wal,
    RollbackJournal,
    Temporary,
    Other,
}

impl FileClass {
    fn of(flags: c_int) -> Self {
        const TEMPORARY: c_int = ffi::SQLITE_OPEN_TEMP_DB
            | ffi::SQLITE_OPEN_TEMP_JOURNAL
            | ffi::SQLITE_OPEN_TRANSIENT_DB
            | ffi::SQLITE_OPEN_SUBJOURNAL;
        if flags & ffi::SQLITE_OPEN_MAIN_DB != 0 {
            Self::MainDb
        } else if flags & ffi::SQLITE_OPEN_WAL != 0 {
            Self::Wal
        } else if flags & (ffi::SQLITE_OPEN_MAIN_JOURNAL | ffi::SQLITE_OPEN_SUPER_JOURNAL) != 0 {
            Self::RollbackJournal
        } else if flags & TEMPORARY != 0 {
            Self::Temporary
        } else {
            Self::Other
        }
    }
}

const PROBE_VFS_NAME: &CStr = c"aft-write-count";

fn written_bytes() -> &'static Mutex<BTreeMap<FileClass, u64>> {
    static BYTES: OnceLock<Mutex<BTreeMap<FileClass, u64>>> = OnceLock::new();
    BYTES.get_or_init(|| Mutex::new(BTreeMap::new()))
}

/// Which file class each open `sqlite3_file` belongs to. A `sqlite3_file` is
/// only ever handed out by `xOpen`, so re-registering on every open keeps this
/// correct even when SQLite reuses an address for a later file.
fn open_files() -> &'static Mutex<BTreeMap<usize, FileClass>> {
    static FILES: OnceLock<Mutex<BTreeMap<usize, FileClass>>> = OnceLock::new();
    FILES.get_or_init(|| Mutex::new(BTreeMap::new()))
}

/// Maps each patched I/O-method table back to the platform table it was copied
/// from, so the counting `xWrite` can call the real one.
fn patched_methods() -> &'static Mutex<BTreeMap<usize, usize>> {
    static METHODS: OnceLock<Mutex<BTreeMap<usize, usize>>> = OnceLock::new();
    METHODS.get_or_init(|| Mutex::new(BTreeMap::new()))
}

fn platform_vfs() -> *mut ffi::sqlite3_vfs {
    static VFS: OnceLock<usize> = OnceLock::new();
    *VFS.get_or_init(|| {
        let vfs = unsafe { ffi::sqlite3_vfs_find(std::ptr::null()) };
        assert!(!vfs.is_null(), "no default SQLite VFS to wrap");
        vfs as usize
    }) as *mut ffi::sqlite3_vfs
}

/// Wrap the platform VFS by copying it and replacing only `xOpen`. Every other
/// VFS method keeps pointing at the platform implementation, and because the
/// shim hands SQLite the platform's own `sqlite3_file` -- it does not wrap the
/// file object -- the file methods it does not replace need no forwarding
/// either.
fn register_probe_vfs() {
    static REGISTERED: OnceLock<()> = OnceLock::new();
    REGISTERED.get_or_init(|| {
        let platform = platform_vfs();
        let mut shim = unsafe { *platform };
        shim.zName = PROBE_VFS_NAME.as_ptr();
        shim.pNext = std::ptr::null_mut();
        shim.xOpen = Some(probe_open);
        let shim = Box::leak(Box::new(shim));
        let rc = unsafe { ffi::sqlite3_vfs_register(shim, 0) };
        assert_eq!(rc, ffi::SQLITE_OK, "could not register the probe VFS");
    });
}

unsafe extern "C" fn probe_open(
    _vfs: *mut ffi::sqlite3_vfs,
    name: *const c_char,
    file: *mut ffi::sqlite3_file,
    flags: c_int,
    out_flags: *mut c_int,
) -> c_int {
    let platform = platform_vfs();
    let open = unsafe { (*platform).xOpen }.expect("the platform VFS implements xOpen");
    let rc = unsafe { open(platform, name, file, flags, out_flags) };
    if rc != ffi::SQLITE_OK {
        return rc;
    }
    open_files()
        .lock()
        .expect("probe file map poisoned")
        .insert(file as usize, FileClass::of(flags));

    let platform_methods = unsafe { (*file).pMethods };
    if !platform_methods.is_null() {
        let mut methods = patched_methods().lock().expect("probe method map poisoned");
        let patched = methods
            .iter()
            .find(|(_, platform)| **platform == platform_methods as usize)
            .map(|(patched, _)| *patched)
            .unwrap_or_else(|| {
                let mut copy = unsafe { *platform_methods };
                copy.xWrite = Some(probe_write);
                let copy: &'static ffi::sqlite3_io_methods = Box::leak(Box::new(copy));
                let patched = std::ptr::from_ref(copy) as usize;
                methods.insert(patched, platform_methods as usize);
                patched
            });
        unsafe {
            (*file).pMethods = patched as *const ffi::sqlite3_io_methods;
        }
    }
    rc
}

unsafe extern "C" fn probe_write(
    file: *mut ffi::sqlite3_file,
    buffer: *const c_void,
    amount: c_int,
    offset: i64,
) -> c_int {
    let patched = unsafe { (*file).pMethods };
    let platform = patched_methods()
        .lock()
        .expect("probe method map poisoned")
        .get(&(patched as usize))
        .copied()
        .expect("every patched I/O table was copied from a platform one")
        as *const ffi::sqlite3_io_methods;

    if amount > 0 {
        let class = open_files()
            .lock()
            .expect("probe file map poisoned")
            .get(&(file as usize))
            .copied()
            .unwrap_or(FileClass::Other);
        *written_bytes()
            .lock()
            .expect("probe byte map poisoned")
            .entry(class)
            .or_default() += u64::try_from(amount).unwrap_or(0);
    }

    let write = unsafe { (*platform).xWrite }.expect("the platform VFS implements xWrite");
    unsafe { write(file, buffer, amount, offset) }
}

fn take_written_bytes() -> BTreeMap<FileClass, u64> {
    std::mem::take(&mut *written_bytes().lock().expect("probe byte map poisoned"))
}

fn open_counted(path: &Path, flags: OpenFlags) -> Connection {
    register_probe_vfs();
    Connection::open_with_flags_and_vfs(
        path,
        flags,
        PROBE_VFS_NAME
            .to_str()
            .expect("the probe VFS name is valid UTF-8"),
    )
    .expect("open the store copy through the counting VFS")
}

fn mib(bytes: u64) -> f64 {
    bytes as f64 / (1024.0 * 1024.0)
}

/// The two files with the most references in the store. Rewriting their rows
/// approximates a delta refresh of the busiest files, which is the write shape
/// this module compares SQLite's writes against the ledger's credits for.
fn busiest_files(conn: &Connection) -> Vec<String> {
    let mut statement = conn
        .prepare("SELECT caller_file FROM refs GROUP BY caller_file ORDER BY COUNT(*) DESC LIMIT 2")
        .expect("rank files by reference count");
    let files = statement
        .query_map([], |row| row.get::<_, String>(0))
        .expect("read the ranked files")
        .collect::<rusqlite::Result<Vec<_>>>()
        .expect("read the ranked files");
    assert_eq!(files.len(), 2, "store has fewer than two referencing files");
    files
}

/// Re-write every `refs` row for one file, the way a delta refresh does: drop
/// the file's rows and insert them again. The values are read back first so the
/// store still describes the same code afterwards and the probe can be re-run.
fn refresh_one_file(conn: &Connection, file: &str) {
    let columns = "ref_id, caller_node, caller_file, kind, short_name, full_ref, module_path, \
                   import_kind, local_name, requested_name, namespace_alias, wildcard, line, \
                   byte_start, byte_end, status, target_node, target_file, target_symbol, \
                   provenance";
    let transaction = conn
        .unchecked_transaction()
        .expect("begin the refresh transaction");
    transaction
        .execute_batch(&format!(
            "CREATE TEMP TABLE IF NOT EXISTS probe_rows AS SELECT {columns} FROM refs WHERE 0;
             DELETE FROM probe_rows;"
        ))
        .expect("stage the file's rows");
    transaction
        .execute(
            &format!("INSERT INTO probe_rows SELECT {columns} FROM refs WHERE caller_file = ?1"),
            [file],
        )
        .expect("copy the file's rows aside");
    transaction
        .execute("DELETE FROM refs WHERE caller_file = ?1", [file])
        .expect("delete the file's rows");
    transaction
        .execute(
            &format!("INSERT INTO refs ({columns}) SELECT {columns} FROM probe_rows"),
            [],
        )
        .expect("reinsert the file's rows");
    transaction.commit().expect("commit the refresh");
}

/// One measured refresh-and-close cycle on a copy of a real store.
#[derive(Debug)]
struct Cycle {
    page_size: u64,
    main_at_open: u64,
    main_bytes: u64,
    wal_bytes: u64,
    journal_bytes: u64,
    temporary_bytes: u64,
    refresh_credit: u64,
    checkpoint_credit: u64,
    backfill_frames: u64,
    checkpoints: u64,
    close_frames: u64,
}

impl Cycle {
    /// How far the ledger's main-file credit is from the bytes SQLite wrote
    /// there, as a fraction of those bytes.
    fn credit_error(&self) -> f64 {
        (self.checkpoint_credit as f64 - self.main_bytes as f64).abs() / self.main_bytes as f64
    }

    fn report(&self, label: &str) {
        println!(
            "\n=== {label} ===\n\
             page size {} B, store {:.2} MiB at open\n\
             \n\
             bytes SQLite wrote (counting VFS, the fs_usage stand-in)\n\
               main database     {:9.2} MiB\n\
               WAL               {:9.2} MiB\n\
               rollback journal  {:9.2} MiB\n\
               temporary         {:9.2} MiB\n\
             \n\
             WAL-index backfill counter\n\
               moved during run  {:9.2} MiB over {} checkpoints\n\
               left to the close {:9.2} MiB ({} frames)\n\
             \n\
             ledger credit\n\
               callgraph_refresh    {:9.2} MiB  vs {:9.2} MiB written to the WAL\n\
               callgraph_checkpoint {:9.2} MiB  vs {:9.2} MiB written to the main file  ({:+.1} %)",
            self.page_size,
            mib(self.main_at_open),
            mib(self.main_bytes),
            mib(self.wal_bytes),
            mib(self.journal_bytes),
            mib(self.temporary_bytes),
            mib(self.backfill_frames * self.page_size),
            self.checkpoints,
            mib(self.close_frames * self.page_size),
            self.close_frames,
            mib(self.refresh_credit),
            mib(self.wal_bytes),
            mib(self.checkpoint_credit),
            mib(self.main_bytes),
            100.0 * (self.checkpoint_credit as f64 - self.main_bytes as f64)
                / self.main_bytes as f64,
        );
    }
}

/// Refresh two files `rounds` times on a fresh copy of the store, then close.
///
/// With `contended`, a read-only connection holds a read mark across every
/// round and is dropped just before the writer closes. That is what stops a
/// PASSIVE autocheckpoint from draining the WAL, so the log accumulates several
/// versions of the same pages and the frames left behind fall to the close.
fn run_cycle(source: &Path, rounds: usize, contended: bool) -> Cycle {
    let dir = tempfile::tempdir().expect("probe scratch directory");
    let path = dir.path().join("store.sqlite");
    std::fs::copy(source, &path).expect("copy the store before touching it");

    // A ledger entry is keyed by domain and root, and this process may run the
    // probe more than once, so each cycle gets a root of its own rather than
    // reading another cycle's credit back.
    static CYCLE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let root = format!(
        "/wal-credit-probe/{}/{}",
        std::process::id(),
        CYCLE.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    );
    let refresh_before =
        crate::write_ledger::pending_for_test(WriteDomain::CallgraphRefresh, &root).1;
    let checkpoint_before =
        crate::write_ledger::pending_for_test(WriteDomain::CallgraphCheckpoint, &root).1;
    let main_at_open = std::fs::metadata(&path).expect("stat the copy").len();
    take_written_bytes();

    let (page_size, backfill) = {
        let writer = TrackedConnection::from_connection_attributed(
            open_counted(&path, OpenFlags::SQLITE_OPEN_READ_WRITE),
            SqliteStore::CallgraphGeneration,
            root.clone(),
        )
        .expect("track the writer");
        // The pragmas a callgraph refresh writer runs, so the autocheckpoint
        // threshold and journal mode under measurement are the production ones.
        writer
            .busy_timeout(std::time::Duration::from_secs(5))
            .unwrap();
        writer.pragma_update(None, "journal_mode", "WAL").unwrap();
        writer.pragma_update(None, "synchronous", "NORMAL").unwrap();
        writer.set_wal_autocheckpoint(4_000).unwrap();
        writer
            .pragma_update(None, "cache_size", -8 * 1024_i64)
            .unwrap();
        let page_size: u64 = writer
            .pragma_query_value(None, "page_size", |row| row.get(0))
            .unwrap();

        let files = busiest_files(&writer);
        // A read-only connection the way the dead-code projection opens one.
        // BEGIN then a read: the statement on its own would end its implicit
        // transaction and give the read mark straight back.
        let projection = contended.then(|| {
            let projection = open_counted(&path, OpenFlags::SQLITE_OPEN_READ_ONLY);
            projection.execute_batch("BEGIN").expect("begin the read");
            projection
                .query_row("SELECT COUNT(*) FROM refs", [], |row| row.get::<_, i64>(0))
                .expect("take a read mark");
            projection
        });

        let mut backfill = BackfillWatch::default();
        for _ in 0..rounds {
            for file in &files {
                refresh_one_file(&writer, file);
                backfill.sample(&path);
            }
        }
        drop(projection);
        writer.sample_write_pages();
        backfill.sample(&path);
        (page_size, backfill)
    };

    let written = take_written_bytes();
    let class = |class: FileClass| written.get(&class).copied().unwrap_or(0);
    Cycle {
        page_size,
        main_at_open,
        main_bytes: class(FileClass::MainDb),
        wal_bytes: class(FileClass::Wal),
        journal_bytes: class(FileClass::RollbackJournal),
        temporary_bytes: class(FileClass::Temporary),
        refresh_credit: crate::write_ledger::pending_for_test(WriteDomain::CallgraphRefresh, &root)
            .1
            .saturating_sub(refresh_before),
        checkpoint_credit: crate::write_ledger::pending_for_test(
            WriteDomain::CallgraphCheckpoint,
            &root,
        )
        .1
        .saturating_sub(checkpoint_before),
        backfill_frames: backfill.frames,
        checkpoints: backfill.checkpoints,
        close_frames: backfill.outstanding(),
    }
}

#[test]
#[ignore = "needs AFT_WAL_CREDIT_PROBE_STORE pointing at a copy of a real callgraph store"]
fn ledger_credit_reconciles_with_the_bytes_written_to_a_real_store() {
    let Ok(source) = std::env::var("AFT_WAL_CREDIT_PROBE_STORE") else {
        panic!("set AFT_WAL_CREDIT_PROBE_STORE to a copy of a real callgraph store");
    };
    let source = Path::new(&source);

    // One pass over two changed files with nothing else reading the store: the
    // shape a callgraph refresh has when it runs alone. Each commit's
    // autocheckpoint drains the WAL, so SQLite restarts the log for the next
    // transaction and every frame holds a distinct page.
    let quiet = run_cycle(source, 1, false);
    quiet.report("one 2-file refresh + close, no concurrent reader");

    // The same work with the dead-code projection's read mark held across it.
    // The WAL cannot drain, so it accumulates several versions of the same
    // pages and the frames the blocked checkpoints left behind fall to the
    // close.
    let contended = run_cycle(source, 6, true);
    contended.report("six 2-file refreshes + close, projection reader held");

    // A connection writing the main database outside WAL mode would land here,
    // and would mean the missing bytes are not a checkpoint-credit problem at
    // all but a writer the ledger has never seen.
    assert_eq!(
        (quiet.journal_bytes, contended.journal_bytes),
        (0, 0),
        "a rollback-journal writer touched the store"
    );

    // The frames the close-time checkpoint had to copy: ones that no
    // autocheckpoint and no explicit checkpoint had already credited. Without a
    // close-time credit these are exactly the bytes that reach the main
    // database under no domain at all.
    assert!(
        contended.close_frames > 0,
        "the close had nothing to checkpoint, so this run says nothing about crediting it"
    );

    assert!(
        quiet.main_bytes > 0 && quiet.wal_bytes > 0,
        "the workload wrote nothing; the probe measured nothing"
    );
    assert!(
        quiet.credit_error() <= 0.05,
        "main-file credit {:.2} MiB is {:.1} % off the {:.2} MiB SQLite wrote",
        mib(quiet.checkpoint_credit),
        100.0 * quiet.credit_error(),
        mib(quiet.main_bytes)
    );
}

/// A checkpoint writes only the newest version of each page it finds in the
/// WAL, but `nBackfill` counts frame positions. The two agree only while the
/// WAL holds one version of each page, which is why the reconciliation above is
/// asserted on the uncontended cycle: once a reader stops the WAL from
/// draining, crediting frames over-states the bytes that reach the main file.
/// This records that gap on the real store rather than leaving it implied.
#[test]
#[ignore = "needs AFT_WAL_CREDIT_PROBE_STORE pointing at a copy of a real callgraph store"]
fn frames_overstate_main_file_bytes_once_the_wal_stops_draining() {
    let Ok(source) = std::env::var("AFT_WAL_CREDIT_PROBE_STORE") else {
        panic!("set AFT_WAL_CREDIT_PROBE_STORE to a copy of a real callgraph store");
    };
    let contended = run_cycle(Path::new(&source), 6, true);
    contended.report("duplicate-page over-credit");
    let credited_frames = contended.backfill_frames + contended.close_frames;
    let pages_written = contended.main_bytes / contended.page_size;
    println!(
        "  frames backfilled {credited_frames}, pages written {pages_written}, \
         ratio {:.2}x\n",
        credited_frames as f64 / pages_written as f64
    );
    assert!(
        credited_frames > pages_written,
        "expected the WAL to hold repeated page versions under a held read mark"
    );
}

/// The WAL generation salt, `mxFrame`, and `nBackfill`, read straight out of
/// the WAL-index. Asking `PRAGMA wal_checkpoint` instead would run a checkpoint
/// of its own and advance `nBackfill` before it could be read.
fn wal_index_state(path: &Path) -> (u64, u32, u32) {
    let mut shm = path.as_os_str().to_owned();
    shm.push("-shm");
    let bytes = std::fs::read(std::path::PathBuf::from(shm)).expect("read the WAL-index");
    let salt = u64::from_ne_bytes(bytes[32..40].try_into().unwrap());
    let mx_frame = u32::from_ne_bytes(bytes[16..20].try_into().unwrap());
    let backfilled = u32::from_ne_bytes(bytes[96..100].try_into().unwrap());
    (salt, mx_frame, backfilled)
}

/// Accumulates how far the WAL-index backfill counter moved, which is the
/// number of frames checkpoints copied into the main database file.
///
/// The salt is what says one WAL generation ended and another began. Frame
/// numbers cannot: SQLite restarts the log from frame 1 once a checkpoint has
/// copied all of it, and a restarted log that grows past the previous log's
/// high-water mark is indistinguishable from the previous log still growing.
#[derive(Debug, Default)]
struct BackfillWatch {
    generation: u64,
    last_mx_frame: u32,
    last_backfilled: u32,
    frames: u64,
    checkpoints: u64,
}

impl BackfillWatch {
    fn sample(&mut self, path: &Path) {
        let (salt, mx_frame, backfilled) = wal_index_state(path);
        if salt != self.generation {
            self.generation = salt;
            self.last_backfilled = 0;
        }
        if backfilled > self.last_backfilled {
            self.frames += u64::from(backfilled - self.last_backfilled);
            self.checkpoints += 1;
        }
        self.last_mx_frame = mx_frame;
        self.last_backfilled = backfilled;
    }

    fn outstanding(&self) -> u64 {
        u64::from(self.last_mx_frame.saturating_sub(self.last_backfilled))
    }
}
