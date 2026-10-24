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

fn platform() -> *mut ffi::sqlite3_vfs {
    static PLATFORM: OnceLock<usize> = OnceLock::new();
    *PLATFORM.get_or_init(|| unsafe { ffi::sqlite3_vfs_find(std::ptr::null()) as usize })
        as *mut ffi::sqlite3_vfs
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
    let base = platform();
    let rc = unsafe { ((*base).xOpen.unwrap())(base, name, file, flags, out) };
    if rc != ffi::SQLITE_OK || name.is_null() {
        return rc;
    }
    let path = PathBuf::from(unsafe { CStr::from_ptr(name) }.to_string_lossy().as_ref());
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
}

unsafe extern "C" fn write(
    file: *mut ffi::sqlite3_file,
    buffer: *const c_void,
    amount: c_int,
    offset: i64,
) -> c_int {
    let original = methods().lock().unwrap()[&(unsafe { (*file).pMethods } as usize)]
        as *const ffi::sqlite3_io_methods;
    let rc = unsafe { ((*original).xWrite.unwrap())(file, buffer, amount, offset) };
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
            unsafe { ffi::sqlite3_vfs_register(shim as *mut _, 1) },
            ffi::SQLITE_OK
        );
        Self
    }

    fn start(target: &Path) -> Self {
        let session = Self::install();
        CAPTURE.with(|capture| {
            assert!(capture.borrow().is_none());
            *capture.borrow_mut() = Some(Capture {
                target: target.canonicalize().unwrap(),
                ..Default::default()
            });
        });
        session
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        CAPTURE.with(|capture| *capture.borrow_mut() = None);
        assert_eq!(
            unsafe { ffi::sqlite3_vfs_register(platform(), 1) },
            ffi::SQLITE_OK
        );
    }
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
    CAPTURE.with(|capture| {
        capture
            .borrow_mut()
            .as_mut()
            .unwrap()
            .hook_frames
            .push(frames)
    });
    ffi::SQLITE_OK
}

pub(in crate::views::materialization) fn observe(
    connection: &Connection,
    path: &Path,
) -> Option<BTreeMap<u32, String>> {
    publication::observe(connection, path);
    if !CAPTURE.with(|capture| {
        capture
            .borrow()
            .as_ref()
            .is_some_and(|capture| capture.target == path.canonicalize().unwrap())
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
        let frames = CAPTURE.with(|capture| capture.borrow().as_ref().unwrap().pages.len());
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
