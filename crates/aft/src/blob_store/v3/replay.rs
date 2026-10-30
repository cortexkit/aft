//! Test-only power-loss simulation through SQLite's own VFS. The wrapper
//! forwards every call to the platform VFS and records what a power cut would
//! lose: bytes written or truncated since the file's last sync, files created
//! since their directory was last synced, and deletions not yet made durable
//! by a directory sync. [`finish`] then puts each file back to its last
//! synced state, again through the platform VFS, so no database or sidecar
//! byte is read or written outside SQLite.
//!
//! A run may cut power before its N-th mutating call (write, truncate, sync,
//! create or delete; creating an empty WAL file is not counted, see
//! `open_file`). From then on every mutating call fails with an I/O
//! error and changes nothing, as if the device had gone away; reads still
//! see what reached the files before the cut.
//!
//! Directory syncs come from two places: SQLite's own sync of a journal or
//! WAL opened for creation (the unix VFS syncs its directory on that file's
//! first sync) and `durability::sync_dir`, read from its test event log. Files
//! that Rust code creates (the v3 opener pre-creates a new database file)
//! and directories are not modelled; replay fixtures create their stores
//! before the replay starts.
//!
//! The replay tests run on unix only, where directory sync is a real
//! operation; elsewhere only the open hook and the fault switch are used.
#![cfg_attr(not(unix), allow(dead_code))]

use rusqlite::{ffi, Connection, OpenFlags};
use std::collections::HashMap;
use std::ffi::{c_char, c_int, c_void, CStr, CString};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, MutexGuard, OnceLock};

const NAME: &CStr = c"aft-v3-replay";
static ENABLED: AtomicBool = AtomicBool::new(false);

thread_local! {
    static UNFLUSHED_CLOSURE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Test-only fault injection: a publication on this thread neither syncs its
/// plane commits nor runs the closure flush before `ClosureFlushed`.
pub(super) fn unflushed_closure() -> bool {
    UNFLUSHED_CLOSURE.with(std::cell::Cell::get)
}
pub(super) fn set_unflushed_closure(on: bool) {
    UNFLUSHED_CLOSURE.with(|flag| flag.set(on));
}

enum Undo {
    Write {
        offset: i64,
        old: Vec<u8>,
        prior_size: i64,
        end: i64,
    },
    Truncate {
        prior_size: i64,
        from: i64,
        tail: Vec<u8>,
    },
}

enum DirOp {
    Created {
        path: PathBuf,
        dir: PathBuf,
        durable: bool,
    },
    Deleted {
        path: PathBuf,
        aside: PathBuf,
        dir: PathBuf,
        durable: bool,
    },
}

struct OpenFile {
    methods: usize,
    path: PathBuf,
    syncs_dir_on_first_sync: bool,
}

#[derive(Default)]
struct State {
    crash_at: Option<u64>,
    ops: u64,
    crashed: bool,
    open: HashMap<usize, OpenFile>,
    undo: HashMap<PathBuf, Vec<Undo>>,
    dir_ops: Vec<DirOp>,
    dir_syncs: Vec<PathBuf>,
    asides: u64,
    trace: Vec<String>,
}

/// What one replayed run did before its power cut.
#[derive(Debug)]
pub(super) struct Report {
    /// Mutating calls that reached the files.
    pub ops: u64,
    pub crashed: bool,
    /// Directories synced before the cut, in order.
    pub dir_syncs: Vec<PathBuf>,
    /// Each counted call in order (`write members.sqlite-wal @0+32`, ...), so
    /// a test can name where two runs of the same operation diverged.
    pub trace: Vec<String>,
}

fn state() -> MutexGuard<'static, State> {
    static STATE: OnceLock<Mutex<State>> = OnceLock::new();
    STATE
        .get_or_init(|| Mutex::new(State::default()))
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}
fn tables() -> MutexGuard<'static, HashMap<usize, usize>> {
    static TABLES: OnceLock<Mutex<HashMap<usize, usize>>> = OnceLock::new();
    TABLES
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}
fn platform() -> *mut ffi::sqlite3_vfs {
    static PLATFORM: OnceLock<usize> = OnceLock::new();
    *PLATFORM.get_or_init(|| {
        // SAFETY: SQLite returns a process-lifetime VFS registration.
        let ptr = unsafe { ffi::sqlite3_vfs_find(std::ptr::null()) };
        assert!(!ptr.is_null());
        ptr as usize
    }) as *mut ffi::sqlite3_vfs
}

pub(super) fn enabled() -> bool {
    ENABLED.load(Ordering::Relaxed)
}

/// Starts recording. Every v3 database opened from now on goes through the
/// wrapper; `crash_at` cuts power before that mutating call (1-based).
pub(super) fn begin(crash_at: Option<u64>) {
    let mut state = state();
    assert!(state.open.is_empty(), "a replayed file is still open");
    *state = State {
        crash_at,
        ..State::default()
    };
    // Directory syncs before the run belong to its fixture.
    let _ = crate::durability::take();
    ENABLED.store(true, Ordering::Relaxed);
}

/// Cuts power (if the run did not already) and restores every recorded file
/// to its last durable state. Every replayed connection must be closed.
pub(super) fn finish() -> Report {
    ENABLED.store(false, Ordering::Relaxed);
    let mut state = state();
    assert!(
        state.open.is_empty(),
        "replayed files are still open at the power cut"
    );
    drain_dir_syncs(&mut state);
    let cut = state.crashed;
    state.crashed = true;
    let ops = std::mem::take(&mut state.dir_ops);
    for op in ops.into_iter().rev() {
        match op {
            DirOp::Created {
                path,
                durable: false,
                ..
            } => {
                let _ = fs::remove_file(&path);
                state.undo.remove(&path);
            }
            DirOp::Deleted {
                path,
                aside,
                durable: false,
                ..
            } => {
                fs::rename(&aside, &path).expect("restore a deletion lost at the power cut");
                if let Some(log) = state.undo.remove(&aside) {
                    state.undo.insert(path, log);
                }
            }
            DirOp::Deleted {
                aside,
                durable: true,
                ..
            } => {
                let _ = fs::remove_file(&aside);
                state.undo.remove(&aside);
            }
            DirOp::Created { durable: true, .. } => {}
        }
    }
    for (path, log) in std::mem::take(&mut state.undo) {
        if log.is_empty() || !path.exists() {
            continue;
        }
        // SAFETY: the platform VFS owns the file object for this block.
        unsafe { roll_back(&path, &log) };
    }
    Report {
        ops: state.ops,
        crashed: cut,
        dir_syncs: std::mem::take(&mut state.dir_syncs),
        trace: std::mem::take(&mut state.trace),
    }
}

pub(super) fn open(path: &Path, flags: OpenFlags) -> rusqlite::Result<Connection> {
    static REGISTERED: OnceLock<()> = OnceLock::new();
    REGISTERED.get_or_init(|| {
        // SAFETY: the copied VFS has process lifetime. Unchanged callbacks use
        // the platform implementation; xOpen and xDelete forward explicitly.
        unsafe {
            let mut vfs = *platform();
            vfs.zName = NAME.as_ptr();
            vfs.pNext = std::ptr::null_mut();
            vfs.xOpen = Some(open_file);
            vfs.xDelete = Some(delete_file);
            assert_eq!(
                ffi::sqlite3_vfs_register(Box::into_raw(Box::new(vfs)), 0),
                ffi::SQLITE_OK
            );
        }
    });
    Connection::open_with_flags_and_vfs(path, flags, NAME.to_str().unwrap())
}

fn canonical_dir(path: &Path) -> PathBuf {
    let dir = path.parent().unwrap_or(Path::new("."));
    fs::canonicalize(dir).unwrap_or_else(|_| dir.to_path_buf())
}

fn mark_dir_durable(state: &mut State, dir: &Path) {
    let dir = fs::canonicalize(dir).unwrap_or_else(|_| dir.to_path_buf());
    for op in &mut state.dir_ops {
        match op {
            DirOp::Created {
                dir: parent,
                durable,
                ..
            }
            | DirOp::Deleted {
                dir: parent,
                durable,
                ..
            } if *parent == dir => *durable = true,
            _ => {}
        }
    }
    state.dir_syncs.push(dir);
}

/// Applies directory syncs that `durability::sync_dir` recorded since the
/// last call. Syncs after the power cut never happened.
fn drain_dir_syncs(state: &mut State) {
    let events = crate::durability::take();
    if state.crashed {
        return;
    }
    for (kind, path) in events {
        if kind == crate::durability::EventKind::DirectorySync {
            mark_dir_durable(state, &path);
        }
    }
}

/// Counts one mutating call, recording `label` in the run's trace. `false`
/// means power is already off.
fn step(state: &mut State, label: impl FnOnce() -> String) -> bool {
    drain_dir_syncs(state);
    if state.crashed {
        return false;
    }
    state.trace.push(label());
    state.ops += 1;
    if state.crash_at == Some(state.ops) {
        state.crashed = true;
        state.ops -= 1;
        return false;
    }
    true
}

fn file_name(path: &Path) -> String {
    path.file_name().map_or_else(
        || path.display().to_string(),
        |name| name.to_string_lossy().into_owned(),
    )
}

fn c_path(name: *const c_char) -> PathBuf {
    // SAFETY: SQLite passes a NUL-terminated file name.
    PathBuf::from(
        unsafe { CStr::from_ptr(name) }
            .to_string_lossy()
            .into_owned(),
    )
}

unsafe extern "C" fn open_file(
    _vfs: *mut ffi::sqlite3_vfs,
    name: *const c_char,
    file: *mut ffi::sqlite3_file,
    flags: c_int,
    out: *mut c_int,
) -> c_int {
    let durable_kinds =
        ffi::SQLITE_OPEN_MAIN_DB | ffi::SQLITE_OPEN_WAL | ffi::SQLITE_OPEN_MAIN_JOURNAL;
    let path = (!name.is_null() && flags & durable_kinds != 0).then(|| c_path(name));
    let mut creating = false;
    if let Some(path) = &path {
        if flags & ffi::SQLITE_OPEN_CREATE != 0 && !path.exists() {
            if flags & ffi::SQLITE_OPEN_WAL != 0 {
                // SQLite creates a WAL file empty, even for a read-only
                // connection, and a read-only close cannot remove it. Whether
                // one is already there depends on what earlier connections to
                // the same database did, and on a loaded CI runner it has
                // differed between runs of the same test. Counting the
                // creation made the number of cut points depend on that
                // leftover file.
                // An empty file holds no durable state, so no cut point is
                // lost: every write to it is still counted, and the power cut
                // still removes the file unless its directory was synced.
                let mut state = state();
                drain_dir_syncs(&mut state);
                if state.crashed {
                    return ffi::SQLITE_CANTOPEN;
                }
            } else if !step(&mut state(), || format!("create {}", file_name(path))) {
                return ffi::SQLITE_CANTOPEN;
            }
            creating = true;
        }
    }
    // SAFETY: forward SQLite's name, file, flags and out unchanged, using the
    // platform VFS pointer in place of the wrapper's pointer.
    let rc = unsafe { ((*platform()).xOpen.unwrap())(platform(), name, file, flags, out) };
    let Some(path) = path else {
        return rc;
    };
    // SAFETY: a successful xOpen initializes the file's method table.
    let original = unsafe { (*file).pMethods };
    if rc != ffi::SQLITE_OK || original.is_null() {
        return rc;
    }
    let mut state = state();
    if creating {
        state.dir_ops.push(DirOp::Created {
            dir: canonical_dir(&path),
            path: path.clone(),
            durable: false,
        });
    }
    state.open.insert(
        file as usize,
        OpenFile {
            methods: original as usize,
            path,
            syncs_dir_on_first_sync: flags & ffi::SQLITE_OPEN_CREATE != 0
                && flags & (ffi::SQLITE_OPEN_WAL | ffi::SQLITE_OPEN_MAIN_JOURNAL) != 0,
        },
    );
    let patched = *tables().entry(original as usize).or_insert_with(|| {
        // SAFETY: copy the platform table, keeping every callback not wrapped here.
        let mut copy = unsafe { *original };
        copy.xWrite = Some(write);
        copy.xTruncate = Some(truncate);
        copy.xSync = Some(sync);
        copy.xClose = Some(close);
        Box::into_raw(Box::new(copy)) as usize
    });
    // SAFETY: leaked method tables outlive every replayed file.
    unsafe { (*file).pMethods = patched as *const ffi::sqlite3_io_methods };
    rc
}

unsafe extern "C" fn delete_file(
    _vfs: *mut ffi::sqlite3_vfs,
    name: *const c_char,
    sync_dir: c_int,
) -> c_int {
    let path = c_path(name);
    let mut state = state();
    if !step(&mut state, || format!("delete {}", file_name(&path))) {
        return ffi::SQLITE_IOERR_DELETE;
    }
    if !path.exists() {
        return ffi::SQLITE_IOERR_DELETE_NOENT;
    }
    // Keep the bytes aside so a deletion the power cut loses can come back.
    state.asides += 1;
    let mut aside = path.clone().into_os_string();
    aside.push(format!(".replay-deleted-{}", state.asides));
    let aside = PathBuf::from(aside);
    if fs::rename(&path, &aside).is_err() {
        return ffi::SQLITE_IOERR_DELETE;
    }
    if let Some(log) = state.undo.remove(&path) {
        state.undo.insert(aside.clone(), log);
    }
    let dir = canonical_dir(&path);
    state.dir_ops.push(DirOp::Deleted {
        path,
        aside,
        dir: dir.clone(),
        durable: false,
    });
    if sync_dir != 0 {
        mark_dir_durable(&mut state, &dir);
    }
    ffi::SQLITE_OK
}

fn methods_of(
    state: &State,
    file: *mut ffi::sqlite3_file,
) -> (*const ffi::sqlite3_io_methods, PathBuf) {
    let open = &state.open[&(file as usize)];
    (
        open.methods as *const ffi::sqlite3_io_methods,
        open.path.clone(),
    )
}

/// Reads `[offset, offset + len)` through the file's original methods.
unsafe fn read_range(
    file: *mut ffi::sqlite3_file,
    methods: *const ffi::sqlite3_io_methods,
    offset: i64,
    len: i64,
) -> Vec<u8> {
    let mut bytes = vec![0u8; len.max(0) as usize];
    if !bytes.is_empty() {
        // SAFETY: the buffer holds `len` bytes; a short read zero-fills it.
        let rc = unsafe {
            ((*methods).xRead.unwrap())(
                file,
                bytes.as_mut_ptr().cast::<c_void>(),
                len as c_int,
                offset,
            )
        };
        assert!(
            rc == ffi::SQLITE_OK || rc == ffi::SQLITE_IOERR_SHORT_READ,
            "replay could not read the bytes a write replaces: {rc}"
        );
    }
    bytes
}

unsafe fn size_of(file: *mut ffi::sqlite3_file, methods: *const ffi::sqlite3_io_methods) -> i64 {
    let mut size = 0i64;
    // SAFETY: the original method reports the open file's size.
    let rc = unsafe { ((*methods).xFileSize.unwrap())(file, &mut size) };
    assert_eq!(rc, ffi::SQLITE_OK, "replay could not size a file");
    size
}

unsafe extern "C" fn write(
    file: *mut ffi::sqlite3_file,
    buffer: *const c_void,
    amount: c_int,
    offset: i64,
) -> c_int {
    let mut state = state();
    let (methods, path) = methods_of(&state, file);
    if !step(&mut state, || {
        format!("write {} @{offset}+{amount}", file_name(&path))
    }) {
        return ffi::SQLITE_IOERR_WRITE;
    }
    // SAFETY: the file is open and its original methods are valid.
    let prior_size = unsafe { size_of(file, methods) };
    let end = offset + i64::from(amount);
    // SAFETY: as above.
    let old = unsafe { read_range(file, methods, offset, end.min(prior_size) - offset) };
    state.undo.entry(path).or_default().push(Undo::Write {
        offset,
        old,
        prior_size,
        end,
    });
    // SAFETY: forward SQLite's buffer and offsets to the original callback.
    unsafe { ((*methods).xWrite.unwrap())(file, buffer, amount, offset) }
}

unsafe extern "C" fn truncate(file: *mut ffi::sqlite3_file, size: i64) -> c_int {
    let mut state = state();
    let (methods, path) = methods_of(&state, file);
    if !step(&mut state, || {
        format!("truncate {} {size}", file_name(&path))
    }) {
        return ffi::SQLITE_IOERR_TRUNCATE;
    }
    // SAFETY: the file is open and its original methods are valid.
    let prior_size = unsafe { size_of(file, methods) };
    // SAFETY: as above.
    let tail = unsafe { read_range(file, methods, size, prior_size - size) };
    state.undo.entry(path).or_default().push(Undo::Truncate {
        prior_size,
        from: size,
        tail,
    });
    // SAFETY: forward the original SQLite-owned file and requested length.
    unsafe { ((*methods).xTruncate.unwrap())(file, size) }
}

unsafe extern "C" fn sync(file: *mut ffi::sqlite3_file, flags: c_int) -> c_int {
    let mut state = state();
    let (methods, path) = methods_of(&state, file);
    if !step(&mut state, || format!("sync {}", file_name(&path))) {
        return ffi::SQLITE_IOERR_FSYNC;
    }
    // SAFETY: forward the original SQLite-owned file and synchronization flags.
    let rc = unsafe { ((*methods).xSync.unwrap())(file, flags) };
    if rc == ffi::SQLITE_OK {
        state.undo.remove(&path);
        let open = state.open.get_mut(&(file as usize)).expect("open file");
        if std::mem::take(&mut open.syncs_dir_on_first_sync) {
            let dir = path.parent().unwrap_or(Path::new(".")).to_path_buf();
            mark_dir_durable(&mut state, &dir);
        }
    }
    rc
}

unsafe extern "C" fn close(file: *mut ffi::sqlite3_file) -> c_int {
    let open = state()
        .open
        .remove(&(file as usize))
        .expect("replayed file is open");
    let methods = open.methods as *const ffi::sqlite3_io_methods;
    // SAFETY: restore the original table before the platform closes its file.
    unsafe {
        (*file).pMethods = methods;
        ((*methods).xClose.unwrap())(file)
    }
}

/// Opens `path` through the platform VFS and undoes `log`, newest first.
unsafe fn roll_back(path: &Path, log: &[Undo]) {
    let vfs = platform();
    // SAFETY: the platform VFS reports the size of its file object.
    let size = unsafe { (*vfs).szOsFile } as usize;
    let mut object = vec![0u64; size.div_ceil(8).max(1)];
    let file = object.as_mut_ptr().cast::<ffi::sqlite3_file>();
    let name = CString::new(path.to_string_lossy().into_owned()).expect("path without NUL");
    let kind = if path.to_string_lossy().ends_with("-wal") {
        ffi::SQLITE_OPEN_WAL
    } else if path.to_string_lossy().ends_with("-journal") {
        ffi::SQLITE_OPEN_MAIN_JOURNAL
    } else {
        ffi::SQLITE_OPEN_MAIN_DB
    };
    let mut out = 0;
    // SAFETY: `name` and `object` outlive the open file, which closes below.
    let rc = unsafe {
        ((*vfs).xOpen.unwrap())(
            vfs,
            name.as_ptr(),
            file,
            ffi::SQLITE_OPEN_READWRITE | kind,
            &mut out,
        )
    };
    assert_eq!(
        rc,
        ffi::SQLITE_OK,
        "replay could not reopen {}",
        path.display()
    );
    // SAFETY: a successful xOpen initialized the method table.
    let methods = unsafe { (*file).pMethods };
    for undo in log.iter().rev() {
        let (offset, bytes, prior_size, end) = match undo {
            Undo::Write {
                offset,
                old,
                prior_size,
                end,
            } => (*offset, old, *prior_size, *end),
            Undo::Truncate {
                prior_size,
                from,
                tail,
            } => (*from, tail, *prior_size, *from),
        };
        if !bytes.is_empty() {
            // SAFETY: write the saved bytes back through the platform file.
            let rc = unsafe {
                ((*methods).xWrite.unwrap())(
                    file,
                    bytes.as_ptr().cast::<c_void>(),
                    bytes.len() as c_int,
                    offset,
                )
            };
            assert_eq!(rc, ffi::SQLITE_OK);
        }
        if end > prior_size {
            // SAFETY: shrink a file the lost call had extended.
            let rc = unsafe { ((*methods).xTruncate.unwrap())(file, prior_size) };
            assert_eq!(rc, ffi::SQLITE_OK);
        }
    }
    // SAFETY: close the file object opened above.
    unsafe { ((*methods).xClose.unwrap())(file) };
}
