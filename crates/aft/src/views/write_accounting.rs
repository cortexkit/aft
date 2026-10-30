//! Logical bytes accepted by each store's existing SQLite VFS, not physical disk I/O.
//! Observation never opens a database or sidecar and never changes SQLite policy.

use rusqlite::ffi;
use std::cell::RefCell;
use std::ffi::{c_char, c_int, c_void, CStr};
use std::path::Path;
use std::sync::{Arc, Mutex, OnceLock};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Writes {
    pub write_calls: u64,
    pub main_bytes: u64,
    pub wal_bytes: u64,
    pub journal_bytes: u64,
    pub checkpoints: u64,
    pub backfilled_pages: u64,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Snapshot(pub [Writes; 5]);

#[derive(Clone, Default)]
pub(crate) struct Publication(Arc<Mutex<Snapshot>>);

thread_local! {
    // Count only work executed by this publisher, even on shared blob connections.
    // Flush once per prepare/commit scope, not once per page under a global lock.
    static ACTIVE: RefCell<Option<Active>> = const { RefCell::new(None) };
}

struct Active {
    publication: Publication,
    writes: Snapshot,
}

pub(crate) struct Scope(Option<Active>);
impl Drop for Scope {
    fn drop(&mut self) {
        ACTIVE.with(|active| {
            let finished = active.replace(self.0.take()).unwrap();
            let mut total = finished
                .publication
                .0
                .lock()
                .unwrap_or_else(|p| p.into_inner());
            for (total, writes) in total.0.iter_mut().zip(finished.writes.0) {
                total.main_bytes += writes.main_bytes;
                total.wal_bytes += writes.wal_bytes;
                total.journal_bytes += writes.journal_bytes;
                total.write_calls += writes.write_calls;
                total.checkpoints += writes.checkpoints;
                total.backfilled_pages += writes.backfilled_pages;
            }
        });
    }
}
impl Publication {
    pub(crate) fn current_or_default() -> Self {
        ACTIVE.with(|active| {
            active
                .borrow()
                .as_ref()
                .map(|active| active.publication.clone())
                .unwrap_or_default()
        })
    }
    pub(crate) fn enter(&self) -> Scope {
        Scope(ACTIVE.with(|active| {
            active.replace(Some(Active {
                publication: self.clone(),
                writes: Snapshot::default(),
            }))
        }))
    }
    pub(crate) fn snapshot(&self) -> Snapshot {
        let mut snapshot = *self.0.lock().unwrap_or_else(|p| p.into_inner());
        ACTIVE.with(|active| {
            if let Some(active) = active
                .borrow()
                .as_ref()
                .filter(|active| Arc::ptr_eq(&active.publication.0, &self.0))
            {
                for (total, writes) in snapshot.0.iter_mut().zip(active.writes.0) {
                    total.main_bytes += writes.main_bytes;
                    total.wal_bytes += writes.wal_bytes;
                    total.journal_bytes += writes.journal_bytes;
                    total.write_calls += writes.write_calls;
                    total.checkpoints += writes.checkpoints;
                    total.backfilled_pages += writes.backfilled_pages;
                }
            }
        });
        snapshot
    }
    pub(crate) fn fields(&self) -> String {
        let mut fields = String::from("store_io_scope=publication_thread_logical store_io_source=sqlite_vfs_and_manifest_writer");
        for (name, writes) in ["blob", "derived", "alias", "manifest", "pointer"]
            .into_iter()
            .zip(self.snapshot().0)
        {
            use std::fmt::Write;
            let _ = write!(fields, " store_{name}_write_calls={} store_{name}_main_bytes={} store_{name}_wal_bytes={} store_{name}_journal_bytes={} store_{name}_checkpoints={} store_{name}_backfilled_pages={}", writes.write_calls, writes.main_bytes, writes.wal_bytes, writes.journal_bytes, writes.checkpoints, writes.backfilled_pages);
        }
        fields
    }
}

fn record(store: usize, update: impl FnOnce(&mut Writes)) {
    ACTIVE.with(|active| {
        if let Some(active) = active.borrow_mut().as_mut() {
            update(&mut active.writes.0[store]);
        }
    });
}

#[cfg(test)]
pub(crate) fn counter_overhead_us() -> f64 {
    let publication = Publication::default();
    let _scope = publication.enter();
    let started = std::time::Instant::now();
    for _ in 0..100_000 {
        record(std::hint::black_box(0), |writes| writes.wal_bytes += 4096);
    }
    started.elapsed().as_secs_f64() * 1e6 / 100_000.0
}

pub(crate) fn manifest_bytes(bytes: usize) {
    record(3, |writes| {
        writes.main_bytes += bytes as u64;
        writes.write_calls += 1;
    });
}

pub(crate) struct ManifestWriter<'a>(pub &'a mut std::fs::File);
impl std::io::Write for ManifestWriter<'_> {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        let written = std::io::Write::write(self.0, bytes)?;
        if written > 0 {
            manifest_bytes(written);
        }
        Ok(written)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        std::io::Write::flush(self.0)
    }
}

// The platform file occupies the original prefix, so all unmodified methods
// still receive exactly the pointer/layout their VFS allocated. Our metadata is
// in an aligned tail; xWrite, checkpoint notifications and xClose are intercepted.
#[repr(C)]
struct Tail {
    original: *const ffi::sqlite3_io_methods,
    patched: ffi::sqlite3_io_methods,
    store: Option<usize>,
    wal: bool,
    journal: bool,
    checkpoint: bool,
}

struct Vfs {
    base: usize,
    tail_offset: usize,
}
fn configuration() -> &'static Vfs {
    static VFS: OnceLock<Vfs> = OnceLock::new();
    VFS.get_or_init(|| unsafe {
        let base = ffi::sqlite3_vfs_find(std::ptr::null());
        assert!(!base.is_null());
        let align = std::mem::align_of::<Tail>();
        let tail_offset = ((*base).szOsFile as usize + align - 1) & !(align - 1);
        let mut wrapper = *base;
        wrapper.zName = c"aft-store-writes".as_ptr();
        wrapper.szOsFile = (tail_offset + std::mem::size_of::<Tail>()) as c_int;
        wrapper.xOpen = Some(open);
        let wrapper = Box::into_raw(Box::new(wrapper));
        assert_eq!(ffi::sqlite3_vfs_register(wrapper, 0), ffi::SQLITE_OK);
        Vfs {
            base: base as usize,
            tail_offset,
        }
    })
}

pub(crate) fn open_connection(
    path: &Path,
    flags: rusqlite::OpenFlags,
) -> rusqlite::Result<rusqlite::Connection> {
    if store(path).is_none() {
        return rusqlite::Connection::open_with_flags(path, flags);
    }
    configuration();
    rusqlite::Connection::open_with_flags_and_vfs(path, flags, "aft-store-writes")
}

#[cfg(test)]
pub(crate) fn vfs() -> *mut ffi::sqlite3_vfs {
    configuration();
    unsafe { ffi::sqlite3_vfs_find(c"aft-store-writes".as_ptr()) }
}

// These are the persistent view store filenames, including private generations.
fn store(path: &Path) -> Option<usize> {
    let name = path.file_name()?.to_str()?;
    let name = name
        .strip_suffix("-wal")
        .or_else(|| name.strip_suffix("-journal"))
        .unwrap_or(name);
    match name {
        "semantic.sqlite" | "callgraph.sqlite" => Some(0),
        "oid-alias.sqlite" => Some(2),
        "pointer.sqlite" => Some(4),
        _ if name.starts_with("derived-")
            && name != "derived-index.sqlite"
            && name.ends_with(".sqlite") =>
        {
            Some(1)
        }
        _ => None,
    }
}

unsafe fn tail(file: *mut ffi::sqlite3_file) -> *mut Tail {
    unsafe { file.cast::<u8>().add(configuration().tail_offset).cast() }
}

unsafe extern "C" fn open(
    _: *mut ffi::sqlite3_vfs,
    name: *const c_char,
    file: *mut ffi::sqlite3_file,
    flags: c_int,
    out: *mut c_int,
) -> c_int {
    let base = configuration().base as *mut ffi::sqlite3_vfs;
    let rc = unsafe { ((*base).xOpen.unwrap())(base, name, file, flags, out) };
    if rc != ffi::SQLITE_OK || unsafe { (*file).pMethods.is_null() } {
        return rc;
    }
    let original = unsafe { (*file).pMethods };
    let mut patched = unsafe { *original };
    patched.xWrite = Some(write);
    patched.xClose = Some(close);
    patched.xFileControl = Some(control);
    let store = if name.is_null() {
        None
    } else {
        store(Path::new(
            unsafe { CStr::from_ptr(name) }.to_string_lossy().as_ref(),
        ))
    };
    let metadata = unsafe { tail(file) };
    unsafe {
        metadata.write(Tail {
            original,
            patched,
            store,
            wal: flags & ffi::SQLITE_OPEN_WAL != 0,
            journal: flags & (ffi::SQLITE_OPEN_MAIN_JOURNAL | ffi::SQLITE_OPEN_SUBJOURNAL) != 0,
            checkpoint: false,
        });
        (*file).pMethods = &(*metadata).patched;
    }
    rc
}

unsafe extern "C" fn write(
    file: *mut ffi::sqlite3_file,
    buffer: *const c_void,
    amount: c_int,
    offset: i64,
) -> c_int {
    let original = unsafe { (*tail(file)).original };
    let rc = unsafe { ((*original).xWrite.unwrap())(file, buffer, amount, offset) };
    let metadata = unsafe { &*tail(file) };
    if rc == ffi::SQLITE_OK && amount > 0 {
        if let Some(store) = metadata.store {
            record(store, |writes| {
                writes.write_calls += 1;
                if metadata.wal {
                    writes.wal_bytes += amount as u64;
                } else if metadata.journal {
                    writes.journal_bytes += amount as u64;
                } else {
                    writes.main_bytes += amount as u64;
                    if metadata.checkpoint {
                        // SQLite checkpoint writes one database page per xWrite.
                        writes.backfilled_pages += 1;
                    }
                }
            });
        }
    }
    rc
}

unsafe extern "C" fn control(
    file: *mut ffi::sqlite3_file,
    op: c_int,
    argument: *mut c_void,
) -> c_int {
    let metadata = unsafe { &mut *tail(file) };
    // SQLite sends these notifications for explicit, auto and close checkpoints.
    // Count the actual backfill writes, not the PRAGMA's cumulative WAL frames.
    if op == ffi::SQLITE_FCNTL_CKPT_START {
        metadata.checkpoint = true;
        if let Some(store) = metadata.store {
            record(store, |writes| writes.checkpoints += 1);
        }
    } else if op == ffi::SQLITE_FCNTL_CKPT_DONE {
        metadata.checkpoint = false;
    }
    let original = unsafe { (*metadata.original).xFileControl };
    match original {
        Some(control) => unsafe { control(file, op, argument) },
        None => ffi::SQLITE_NOTFOUND,
    }
}

unsafe extern "C" fn close(file: *mut ffi::sqlite3_file) -> c_int {
    let original = unsafe { (*tail(file)).original };
    unsafe {
        (*file).pMethods = original;
        ((*original).xClose.unwrap())(file)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn assembly_inherits_semantic_handoff_accounting() {
        let publication = Publication::default();
        let scope = publication.enter();
        manifest_bytes(12);
        let inherited = Publication::current_or_default();
        {
            let _assembly = inherited.enter();
            manifest_bytes(3);
        }
        assert_eq!(inherited.snapshot().0[3].main_bytes, 15);
        drop(scope);
        assert_eq!(publication.snapshot().0[3].main_bytes, 15);
        assert_eq!(
            Publication::current_or_default().snapshot(),
            Snapshot::default()
        );
    }

    #[test]
    fn checkpoint_pages_and_publication_scopes_are_local() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("derived-fixture.sqlite");
        let connection = open_connection(&path, rusqlite::OpenFlags::default()).unwrap();
        connection.execute_batch("PRAGMA journal_mode=WAL; CREATE TABLE payload(value); PRAGMA wal_checkpoint(TRUNCATE);").unwrap();
        let first = Publication::default();
        let second = Publication::default();
        let scope = first.enter();
        connection
            .execute("INSERT INTO payload VALUES (zeroblob(12000))", [])
            .unwrap();
        {
            let _second = second.enter();
            // An empty nested publication must not inherit the outer writes.
        }
        let checkpoint: (i64, i64, i64) = connection
            .query_row("PRAGMA wal_checkpoint(PASSIVE)", [], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?))
            })
            .unwrap();
        drop(scope);
        let writes = first.snapshot().0[1];
        let page_size: u64 = connection
            .pragma_query_value(None, "page_size", |row| row.get(0))
            .unwrap();
        assert!(writes.wal_bytes > 0);
        assert_eq!(writes.checkpoints, 1);
        assert!(writes.backfilled_pages > 0);
        assert_eq!(writes.main_bytes, writes.backfilled_pages * page_size);
        assert_eq!(checkpoint.0, 0);
        assert_eq!(checkpoint.1, checkpoint.2);
        assert_eq!(writes.backfilled_pages, checkpoint.2 as u64);
        assert_eq!(second.snapshot(), Snapshot::default());
        // Unrelated work on the same already-open connection has no active owner.
        connection
            .execute("INSERT INTO payload VALUES (2)", [])
            .unwrap();
        assert_eq!(first.snapshot().0[1], writes);
        assert_eq!(first.snapshot().0[0], Writes::default());
    }
}
