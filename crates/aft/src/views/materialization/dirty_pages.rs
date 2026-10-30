//! Offline, single-worker page accounting through SQLite's own writer and VFS.
//! The VFS observes buffers SQLite is already writing; it never opens a sidecar.

use super::*;
use rusqlite::ffi;
use std::cell::RefCell;
use std::ffi::{c_char, c_int, c_void, CStr};
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

#[path = "dirty_publication.rs"]
pub(crate) mod publication;

#[derive(Default)]
struct Capture {
    target: PathBuf,
    pages: BTreeMap<i64, u32>,
    frame_writes: Vec<u32>,
    hook_frames: Vec<i32>,
    wal_bytes: u64,
    main_bytes: u64,
}

thread_local! {
    static CAPTURE: RefCell<Option<Capture>> = const { RefCell::new(None) };
}

fn absolute_sqlite_path(path: &Path) -> PathBuf {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir().unwrap().join(path)
    };
    crate::inspect::job::normalize_path(&absolute)
}

fn platform() -> *mut ffi::sqlite3_vfs {
    static PLATFORM: OnceLock<usize> = OnceLock::new();
    *PLATFORM.get_or_init(|| crate::views::write_accounting::vfs() as usize)
        as *mut ffi::sqlite3_vfs
}

const VFS_NAME: &str = "aft-derived-dirty-pages";

pub(crate) fn open_probe_connection(
    path: &Path,
    flags: rusqlite::OpenFlags,
) -> rusqlite::Result<Connection> {
    let measured = CAPTURE.with(|capture| {
        capture
            .borrow()
            .as_ref()
            .is_some_and(|capture| capture.target == absolute_sqlite_path(path))
    }) || publication::captures(path);
    if measured {
        Connection::open_with_flags_and_vfs(path, flags, VFS_NAME)
    } else {
        crate::views::write_accounting::open_connection(path, flags)
    }
}

fn ffi_result(body: impl FnOnce() -> c_int) -> c_int {
    // A test assertion or poisoned observer must never unwind through SQLite.
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(body)).unwrap_or(ffi::SQLITE_IOERR)
}

fn methods() -> &'static Mutex<BTreeMap<usize, usize>> {
    static METHODS: OnceLock<Mutex<BTreeMap<usize, usize>>> = OnceLock::new();
    METHODS.get_or_init(Default::default)
}

fn files() -> &'static Mutex<BTreeMap<usize, (PathBuf, bool)>> {
    static FILES: OnceLock<Mutex<BTreeMap<usize, (PathBuf, bool)>>> = OnceLock::new();
    FILES.get_or_init(Default::default)
}

unsafe extern "C" fn open(
    _: *mut ffi::sqlite3_vfs,
    name: *const c_char,
    file: *mut ffi::sqlite3_file,
    flags: c_int,
    out: *mut c_int,
) -> c_int {
    ffi_result(|| {
        let base = platform();
        let Some(open) = (unsafe { base.as_ref() }).and_then(|base| base.xOpen) else {
            return ffi::SQLITE_CANTOPEN;
        };
        let rc = unsafe { open(base, name, file, flags, out) };
        if rc != ffi::SQLITE_OK || name.is_null() {
            return rc;
        }
        let path = absolute_sqlite_path(Path::new(
            unsafe { CStr::from_ptr(name) }.to_string_lossy().as_ref(),
        ));
        let wal = flags & ffi::SQLITE_OPEN_WAL != 0;
        files().lock().unwrap().insert(file as usize, (path, wal));
        let original = unsafe { (*file).pMethods };
        if !original.is_null() {
            let mut tables = methods().lock().unwrap();
            let patched = tables
                .iter()
                .find(|(_, value)| **value == original as usize)
                .map(|(key, _)| *key)
                .unwrap_or_else(|| {
                    let mut copy = unsafe { *original };
                    copy.xWrite = Some(write);
                    let key = Box::into_raw(Box::new(copy)) as usize;
                    tables.insert(key, original as usize);
                    key
                });
            unsafe { (*file).pMethods = patched as *const ffi::sqlite3_io_methods };
        }
        rc
    })
}

unsafe extern "C" fn write(
    file: *mut ffi::sqlite3_file,
    buffer: *const c_void,
    amount: c_int,
    offset: i64,
) -> c_int {
    ffi_result(|| {
        let Some(file_ref) = (unsafe { file.as_ref() }) else {
            return ffi::SQLITE_IOERR_WRITE;
        };
        let Ok(tables) = methods().lock() else {
            return ffi::SQLITE_IOERR_WRITE;
        };
        let Some(original) = tables.get(&(file_ref.pMethods as usize)).copied() else {
            return ffi::SQLITE_IOERR_WRITE;
        };
        drop(tables);
        let original = original as *const ffi::sqlite3_io_methods;
        let Some(write) = (unsafe { original.as_ref() }).and_then(|methods| methods.xWrite) else {
            return ffi::SQLITE_IOERR_WRITE;
        };
        let rc = unsafe { write(file, buffer, amount, offset) };
        if rc != ffi::SQLITE_OK || amount <= 0 {
            return rc;
        }
        if let Some((path, wal)) = files().lock().unwrap().get(&(file as usize)) {
            publication::written(path, *wal, buffer, amount, offset);
        }
        CAPTURE.with(|capture| {
            let mut capture = capture.borrow_mut();
            let Some(capture) = capture.as_mut() else {
                return;
            };
            let opened = files().lock().unwrap();
            let Some((path, wal)) = opened.get(&(file as usize)) else {
                return;
            };
            if !*wal && path == &capture.target {
                capture.main_bytes += amount as u64;
            } else if *wal && path == &PathBuf::from(format!("{}-wal", capture.target.display())) {
                capture.wal_bytes += amount as u64;
                // Bundled SQLite writes each frame header at a frame boundary.
                // A missing/split header fails the frame-count reconciliation below.
                if offset >= 32 && (offset - 32) % 4120 == 0 && amount >= 24 {
                    let bytes = unsafe { std::slice::from_raw_parts(buffer.cast::<u8>(), 4) };
                    let page = u32::from_be_bytes(bytes.try_into().unwrap());
                    capture.pages.insert((offset - 32) / 4120, page);
                    capture.frame_writes.push(page);
                }
            }
        });
        rc
    })
}

struct Session;

impl Session {
    fn install() -> Self {
        static SHIM: OnceLock<usize> = OnceLock::new();
        let shim = *SHIM.get_or_init(|| {
            let mut shim = unsafe { *platform() };
            shim.zName = c"aft-derived-dirty-pages".as_ptr();
            shim.pNext = std::ptr::null_mut();
            shim.xOpen = Some(open);
            Box::into_raw(Box::new(shim)) as usize
        });
        assert_eq!(
            // Other probes cache SQLite's default VFS. Never let them capture
            // this wrapper and replace its per-file method table a second time.
            unsafe { ffi::sqlite3_vfs_register(shim as *mut _, 0) },
            ffi::SQLITE_OK
        );
        Self
    }

    fn start(target: &Path) -> Self {
        let session = Self::install();
        let target = absolute_sqlite_path(target);
        CAPTURE.with(|capture| {
            assert!(capture.borrow().is_none());
            *capture.borrow_mut() = Some(Capture {
                target,
                ..Default::default()
            });
        });
        session
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        CAPTURE.with(|capture| *capture.borrow_mut() = None);
    }
}

#[test]
fn missing_write_method_returns_sqlite_error() {
    let mut file = ffi::sqlite3_file {
        pMethods: std::ptr::null(),
    };
    assert_eq!(
        unsafe { write(&mut file, std::ptr::null(), 0, 0) },
        ffi::SQLITE_IOERR_WRITE
    );
}

#[test]
fn session_keeps_default_and_wal_probe_isolated() {
    const CHILD: &str = "AFT_DIRTY_VFS_ISOLATION_CHILD";
    if std::env::var_os(CHILD).is_none() {
        // The WAL probe must first cache its base while the session is active.
        // A fresh process makes that ordering independent of other lib tests.
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "views::materialization::tests::dirty_pages::session_keeps_default_and_wal_probe_isolated", "--nocapture"])
            .env(CHILD, "1")
            .output().unwrap();
        assert!(
            output.status.success(),
            "VFS child {:?}\n{}\n{}",
            output.status.code(),
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(String::from_utf8_lossy(&output.stdout)
            .contains("isolated-vfs measured=64 default=64 wal-credit=64"));
        return;
    }
    fn counted(connection: &Connection) -> bool {
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
        methods()
            .lock()
            .unwrap()
            .contains_key(&(unsafe { (*file).pMethods } as usize))
    }
    fn write_row(connection: &Connection) -> rusqlite::Result<()> {
        connection.execute_batch("PRAGMA journal_mode=WAL; CREATE TABLE IF NOT EXISTS probe(value INTEGER); INSERT INTO probe VALUES(1); PRAGMA wal_checkpoint(TRUNCATE)")
    }
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    let measured = root.join("measured.sqlite");
    let default_before = unsafe { ffi::sqlite3_vfs_find(std::ptr::null()) } as usize;
    let session = Session::start(&measured);
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
    let other_barrier = barrier.clone();
    let other_root = root.clone();
    let other = std::thread::spawn(move || {
        let mut errors = Vec::new();
        let mut default_writes = 0;
        let mut credit_writes = 0;
        other_barrier.wait();
        for _ in 0..64 {
            let connection = Connection::open(other_root.join("default.sqlite")).unwrap();
            if counted(&connection) {
                errors.push("default connection was counted".to_string());
            }
            match write_row(&connection) {
                Ok(()) => default_writes += 1,
                Err(error) => errors.push(format!("default write: {error}")),
            }
            let credit = crate::db::wal_credit_probe::open_counted(
                &other_root.join("credit.sqlite"),
                rusqlite::OpenFlags::default(),
            );
            match write_row(&credit) {
                Ok(()) => credit_writes += 1,
                Err(error) => errors.push(format!("WAL credit write: {error}")),
            }
            if crate::db::wal_credit_probe::main_io_counts(&credit).0 == 0 {
                errors.push("WAL credit probe missed its main-file writes".to_string());
            }
        }
        (errors, default_writes, credit_writes)
    });
    let mut errors = Vec::new();
    let mut measured_writes = 0;
    barrier.wait();
    for iteration in 0..64 {
        let perform = |connection: &Connection| (counted(connection), write_row(connection));
        let (is_counted, result) = match iteration % 5 {
            0 => perform(
                &crate::db::file_identity::IdentityConnection::open_with_flags(
                    &measured,
                    rusqlite::OpenFlags::default(),
                    "views::materialization::materialize",
                )
                .unwrap(),
            ),
            1 => perform(
                &crate::db::file_identity::IdentityConnection::open(
                    &measured,
                    "views::materialization::materialize",
                )
                .unwrap(),
            ),
            2 => perform(
                &crate::db::lifecycle::TrackedConnection::open(
                    &measured,
                    crate::db::lifecycle::SqliteStore::BlobStore,
                )
                .unwrap(),
            ),
            3 => perform(
                &crate::db::lifecycle::TrackedConnection::open_with_flags(
                    measured.to_str().unwrap(),
                    rusqlite::OpenFlags::default(),
                    crate::db::lifecycle::SqliteStore::BlobStore,
                )
                .unwrap(),
            ),
            _ => perform(
                &crate::db::lifecycle::TrackedConnection::open_path_with_flags(
                    &measured,
                    rusqlite::OpenFlags::default(),
                    crate::db::lifecycle::SqliteStore::BlobStore,
                )
                .unwrap(),
            ),
        };
        if !is_counted {
            errors.push("measured connection bypassed counting VFS".to_string());
        }
        match result {
            Ok(()) => measured_writes += 1,
            Err(error) => errors.push(format!("measured write: {error}")),
        }
    }
    let (other_errors, default_writes, credit_writes) = other.join().unwrap();
    errors.extend(other_errors);
    let default_during = unsafe { ffi::sqlite3_vfs_find(std::ptr::null()) } as usize;
    let wal_bytes = CAPTURE.with(|capture| capture.borrow().as_ref().unwrap().wal_bytes);
    drop(session);
    assert!(errors.is_empty(), "VFS write failures: {errors:?}");
    assert_eq!(
        default_during, default_before,
        "session changed the process default VFS"
    );
    assert_eq!(
        unsafe { ffi::sqlite3_vfs_find(std::ptr::null()) } as usize,
        default_before
    );
    assert!(wal_bytes > 0, "measured WAL writes must be observed");
    assert_eq!(
        (measured_writes, default_writes, credit_writes),
        (64, 64, 64)
    );
    for name in ["measured.sqlite", "default.sqlite", "credit.sqlite"] {
        let rows: i64 = Connection::open(root.join(name))
            .unwrap()
            .query_row("SELECT COUNT(*) FROM probe", [], |row| row.get(0))
            .unwrap();
        assert_eq!(rows, 64, "writes must survive closing {name}");
    }
    println!("isolated-vfs measured=64 default=64 wal-credit=64");
}

fn owners(connection: &Connection) -> BTreeMap<u32, String> {
    connection
        .prepare("SELECT pageno, name FROM dbstat")
        .unwrap()
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap()
}

unsafe extern "C" fn wal_hook(
    _: *mut c_void,
    _: *mut ffi::sqlite3,
    _: *const c_char,
    frames: c_int,
) -> c_int {
    ffi_result(|| {
        CAPTURE.with(|capture| {
            if let Some(capture) = capture.borrow_mut().as_mut() {
                capture.hook_frames.push(frames);
            }
        });
        ffi::SQLITE_OK
    })
}

pub(in crate::views::materialization) fn observe(
    connection: &Connection,
    path: &Path,
) -> Option<BTreeMap<u32, String>> {
    publication::observe(connection, path);
    if CAPTURE.with(|capture| capture.borrow().is_none()) {
        return None;
    }
    let matching_path = absolute_sqlite_path(path);
    if !CAPTURE.with(|capture| {
        let mut capture = capture.borrow_mut();
        let capture = capture.as_mut().unwrap();
        if capture.target != matching_path {
            return false;
        }
        // Use SQLite's own filename after opening; resolving a live database
        // through the filesystem could introduce another OS handle on Windows.
        capture.target = absolute_sqlite_path(Path::new(connection.path().unwrap()));
        true
    }) {
        return None;
    }
    let size: i64 = connection
        .query_row("PRAGMA page_size", [], |row| row.get(0))
        .unwrap();
    assert_eq!(size, 4096);
    unsafe {
        ffi::sqlite3_wal_hook(connection.handle(), Some(wal_hook), std::ptr::null_mut());
    }
    Some(owners(connection))
}

pub(in crate::views::materialization) fn finish(
    before: Option<BTreeMap<u32, String>>,
    connection: &Connection,
) {
    publication::finish(connection);
    let Some(before) = before else { return };
    let after = owners(connection);
    let checkpoint: (i32, i32, i32) = connection
        .query_row("PRAGMA wal_checkpoint(PASSIVE)", [], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?))
        })
        .unwrap();
    assert_eq!(checkpoint.0, 0);
    assert_eq!(checkpoint.1, checkpoint.2);
    CAPTURE.with(|capture| {
        let capture = capture.borrow();
        let capture = capture.as_ref().unwrap();
        assert_eq!(
            capture.pages.len(),
            checkpoint.1 as usize,
            "VFS headers must account for every committed WAL frame"
        );
        if let Some(frames) = capture.hook_frames.last() {
            assert_eq!(*frames, checkpoint.1);
        } else {
            assert_eq!(checkpoint.1, 0);
        }
        let mut by_owner = BTreeMap::<String, usize>::new();
        for page in capture.pages.values() {
            let name = after
                .get(page)
                .or_else(|| before.get(page))
                .cloned()
                .unwrap_or_else(|| "<freelist>".into());
            *by_owner.entry(name).or_default() += 1;
        }
        eprintln!(
            "DIRTY_PAGES {}",
            serde_json::json!({
                "hook_frames": capture.hook_frames, "checkpoint": checkpoint,
            "wal_bytes": capture.wal_bytes, "backfill_bytes": capture.main_bytes,
            "frame_writes": capture.frame_writes.len(),
                "frames_by_owner": by_owner,
            })
        );
    });
    unsafe {
        ffi::sqlite3_wal_hook(connection.handle(), None, std::ptr::null_mut());
    }
}

#[test]
#[ignore = "offline size sweep; run single-threaded with isolated HOME/XDG"]
fn derived_dirty_page_probe() {
    for size in [1000, 5000, 20000] {
        let f = fixture();
        let blobs = Connection::open(&f.blobs).unwrap();
        let mut files = (0..size).map(|i| (
            format!("file_{i:05}.ts"),
            format!("export function value_{i}() {{ return 1; }} export function caller_{i}() {{ return value_{i}(); }}"),
        )).collect::<Vec<_>>();
        if std::env::var_os("AFT_DIRTY_DENSE").is_some() {
            for i in 0..512 {
                files[0].1.push_str(&format!(
                    "\nexport function extra_{i}() {{ return value_0(); }}"
                ));
            }
        }
        let make = |files: &[(String, String)]| {
            manifest(
                &blobs,
                &files
                    .iter()
                    .map(|(path, source)| (path.as_str(), source.as_str()))
                    .collect::<Vec<_>>(),
            )
        };
        let base = make(&files);
        let original = f.dir.path().join("base.sqlite");
        materialize_manifest_view_database(&original, &f.blobs, &base).unwrap();
        for case in ["function", "file", "noop", "300-path"] {
            let mut next_files = files.clone();
            match case {
                "function" => {
                    next_files[0].1 =
                        next_files[0]
                            .1
                            .replacen("return value_0()", "return missing_0()", 1)
                }
                "file" => next_files[0].1 = format!("\n{}", next_files[0].1),
                "300-path" => {
                    for (_, source) in &mut next_files[..300] {
                        *source = format!("\n{source}");
                    }
                }
                _ => {}
            }
            let next = make(&next_files);
            let db = f.dir.path().join(format!("{case}.sqlite"));
            crate::views::generation::clone_derived_for_test(&original, &db).unwrap();
            eprintln!("DIRTY_CASE size={size} case={case}");
            let session = Session::start(&db);
            let stats = apply_manifest_diff(&db, &base, &next, &f.blobs).unwrap();
            drop(session);
            eprintln!("DIRTY_STATS {stats:?}");
            let cold = f.dir.path().join(format!("cold-{case}.sqlite"));
            materialize_manifest_view_database(&cold, &f.blobs, &next).unwrap();
            assert_eq!(snapshot(&db), snapshot(&cold), "{size} {case}");
        }
    }
}

#[test]
fn dense_one_function_edit_has_bounded_dirty_pages() {
    for size in [1000, 5000] {
        let f = fixture();
        let blobs = Connection::open(&f.blobs).unwrap();
        let mut files = (0..size).map(|i| (
            format!("file_{i:05}.ts"),
            format!("export function value_{i}() {{ return 1; }} export function caller_{i}() {{ return value_{i}(); }}"),
        )).collect::<Vec<_>>();
        for i in 0..512 {
            files[0].1.push_str(&format!(
                "\nexport function extra_{i}() {{ return value_0(); }}"
            ));
        }
        let make = |files: &[(String, String)]| {
            manifest(
                &blobs,
                &files
                    .iter()
                    .map(|(path, source)| (path.as_str(), source.as_str()))
                    .collect::<Vec<_>>(),
            )
        };
        let base = make(&files);
        files[0].1 = files[0]
            .1
            .replacen("return value_0()", "return missing_0()", 1);
        let next = make(&files);
        let db = f.dir.path().join("bounded.sqlite");
        materialize_manifest_view_database(&db, &f.blobs, &base).unwrap();
        let rowids = |db: &Path| {
            let connection = Connection::open(db).unwrap();
            let result = connection
                .prepare("SELECT id, rowid FROM nodes WHERE file_path='file_00000.ts'")
                .unwrap()
                .query_map([], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
                })
                .unwrap()
                .collect::<rusqlite::Result<BTreeMap<_, _>>>()
                .unwrap();
            result
        };
        let before = rowids(&db);
        let session = Session::start(&db);
        apply_manifest_diff(&db, &base, &next, &f.blobs).unwrap();
        let frames = CAPTURE.with(|capture| {
            let capture = capture.borrow();
            let capture = capture.as_ref().unwrap();
            assert!(
                !capture.hook_frames.is_empty(),
                "the edited writer must be observed"
            );
            capture.pages.len()
        });
        drop(session);
        assert!(frames <= 128, "one-function edit at {size} files dirtied {frames} pages; unchanged graph rows must not churn checkout-wide indexes");
        assert_eq!(
            before,
            rowids(&db),
            "retained nodes should keep their rowids"
        );
        let cold = f.dir.path().join("bounded-cold.sqlite");
        materialize_manifest_view_database(&cold, &f.blobs, &next).unwrap();
        assert_eq!(snapshot(&db), snapshot(&cold));
    }
}

#[test]
fn occupied_node_position_cycle_preserves_rows_and_rolls_back_late_failure() {
    let f = fixture();
    let blobs = Connection::open(&f.blobs).unwrap();
    let first = "export function aaa() { return 1; }";
    let second = "export function bbb() { return 2; }";
    let base = manifest(&blobs, &[("cycle.ts", &format!("{first}\n{second}"))]);
    let next = manifest(&blobs, &[("cycle.ts", &format!("{second}\n{first}"))]);
    let db = f.dir.path().join("cycle.sqlite");
    materialize_manifest_view_database(&db, &f.blobs, &base).unwrap();
    let positions = || {
        let connection = Connection::open(&db).unwrap();
        let result = connection.prepare("SELECT name, rowid, start_line, start_col, end_line, end_col, range_ordinal FROM nodes ORDER BY name").unwrap()
            .query_map([], |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?, (row.get::<_, i64>(2)?, row.get::<_, i64>(3)?, row.get::<_, i64>(4)?, row.get::<_, i64>(5)?, row.get::<_, i64>(6)?)))).unwrap()
            .collect::<rusqlite::Result<Vec<_>>>().unwrap();
        result
    };
    let before = positions();
    apply_manifest_diff(&db, &base, &next, &f.blobs).unwrap();
    let after = positions();
    assert_eq!(before.len(), 2);
    assert_eq!(
        before[0].2, after[1].2,
        "full unique source tuples must exchange places"
    );
    assert_eq!(before[1].2, after[0].2);
    for (old, new) in before.iter().zip(&after) {
        assert_eq!((&old.0, old.1), (&new.0, new.1));
        assert!(new.2 .4 >= 0);
    }
    let cold = f.dir.path().join("cycle-cold.sqlite");
    materialize_manifest_view_database(&cold, &f.blobs, &next).unwrap();
    assert_eq!(snapshot(&db), snapshot(&cold));
    let settled = snapshot(&db);
    Connection::open(&db)
        .unwrap()
        .execute_batch(
            "CREATE TRIGGER reject_position_restore BEFORE UPDATE OF range_ordinal ON nodes
         WHEN OLD.range_ordinal < 0 AND NEW.range_ordinal >= 0
         BEGIN SELECT RAISE(ABORT, 'injected position restore failure'); END;",
        )
        .unwrap();
    let error = apply_manifest_diff(&db, &next, &base, &f.blobs).unwrap_err();
    assert!(error
        .to_string()
        .contains("injected position restore failure"));
    Connection::open(&db)
        .unwrap()
        .execute_batch("DROP TRIGGER reject_position_restore")
        .unwrap();
    assert_eq!(
        snapshot(&db),
        settled,
        "late failure must roll back rows and manifest metadata"
    );
    assert_eq!(positions(), after);
}
