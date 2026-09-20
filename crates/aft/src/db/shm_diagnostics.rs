//! Opt-in Unix SQLite I/O tracing for WAL-index truncation crashes.
//!
//! xTruncate does not see WAL-index truncation: unixShmMap calls ftruncate
//! directly after taking the dead-man-switch lock. Intercept that syscall via
//! SQLite's supported xSetSystemCall seam as well as the public I/O methods.
//! Install before starting test workers; the registered VFS lives until exit.

use std::cell::Cell;
use std::ffi::{c_char, c_int, c_void, CStr};
use std::sync::atomic::{AtomicPtr, AtomicUsize, Ordering};
use std::sync::Once;

use rusqlite::ffi;

static PLATFORM: AtomicPtr<ffi::sqlite3_vfs> = AtomicPtr::new(std::ptr::null_mut());
static TAIL_OFFSET: AtomicUsize = AtomicUsize::new(0);
static FTRUNCATE: AtomicUsize = AtomicUsize::new(0);

#[repr(C)]
struct Tail {
    methods: ffi::sqlite3_io_methods,
    original: *const ffi::sqlite3_io_methods,
    path: [u8; 1024],
    path_len: usize,
    device: u64,
    inode: u64,
}

#[derive(Clone, Copy)]
struct Context {
    tail: *const Tail,
    site: &'static [u8],
    region: i64,
    size: i64,
}

thread_local! {
    static CONTEXT: Cell<Context> = const { Cell::new(Context {
        tail: std::ptr::null(), site: b"outside_wrapped_io", region: -1, size: 0,
    }) };
}

/// Arm the default VFS only when crash capture is explicitly enabled.
/// Call before any worker opens a database, not from a signal handler.
pub fn install() {
    if std::env::var_os("AFT_CAPTURE_CRASH_DIAGNOSTICS").as_deref()
        != Some(std::ffi::OsStr::new("1"))
    {
        return;
    }
    static INSTALL: Once = Once::new();
    INSTALL.call_once(|| unsafe {
        let platform = ffi::sqlite3_vfs_find(std::ptr::null());
        assert!(!platform.is_null());
        assert!(
            (*platform).iVersion >= 3,
            "Unix VFS syscall tracing requires version 3"
        );
        let get = (*platform).xGetSystemCall.expect("Unix VFS xGetSystemCall");
        let set = (*platform).xSetSystemCall.expect("Unix VFS xSetSystemCall");
        let original = get(platform, c"ftruncate".as_ptr()).expect("Unix VFS ftruncate");
        FTRUNCATE.store(original as usize, Ordering::Release);
        let hook = std::mem::transmute::<
            unsafe extern "C" fn(c_int, libc::off_t) -> c_int,
            unsafe extern "C" fn(),
        >(trace_ftruncate);
        assert_eq!(
            set(platform, c"ftruncate".as_ptr(), Some(hook)),
            ffi::SQLITE_OK
        );
        PLATFORM.store(platform, Ordering::Release);
        let alignment = std::mem::align_of::<Tail>();
        let offset = ((*platform).szOsFile as usize).div_ceil(alignment) * alignment;
        TAIL_OFFSET.store(offset, Ordering::Release);
        let mut shim = *platform;
        shim.zName = c"aft-shm-diagnostics".as_ptr();
        shim.pNext = std::ptr::null_mut();
        shim.szOsFile = (offset + std::mem::size_of::<Tail>()) as c_int;
        shim.xOpen = Some(trace_open);
        assert_eq!(
            ffi::sqlite3_vfs_register(Box::into_raw(Box::new(shim)), 1),
            ffi::SQLITE_OK
        );
    });
}

unsafe extern "C" fn trace_open(
    _vfs: *mut ffi::sqlite3_vfs,
    name: *const c_char,
    file: *mut ffi::sqlite3_file,
    flags: c_int,
    out_flags: *mut c_int,
) -> c_int {
    unsafe {
        let platform = PLATFORM.load(Ordering::Acquire);
        let rc = ((*platform).xOpen.unwrap())(platform, name, file, flags, out_flags);
        if rc != ffi::SQLITE_OK || (*file).pMethods.is_null() {
            return rc;
        }
        let original = (*file).pMethods;
        let mut tail = Tail {
            methods: *original,
            original,
            path: [0; 1024],
            path_len: 0,
            device: 0,
            inode: 0,
        };
        if !name.is_null() {
            let bytes = CStr::from_ptr(name).to_bytes();
            tail.path_len = bytes.len().min(tail.path.len() - 5);
            tail.path[..tail.path_len].copy_from_slice(&bytes[..tail.path_len]);
            let mut stat = std::mem::zeroed::<libc::stat>();
            if libc::stat(name, &mut stat) == 0 {
                tail.device = stat.st_dev as u64;
                tail.inode = stat.st_ino as u64;
            }
        }
        tail.methods.xClose = Some(trace_close);
        tail.methods.xTruncate = Some(trace_truncate);
        if tail.methods.iVersion >= 2 && tail.methods.xShmMap.is_some() {
            tail.methods.xShmMap = Some(trace_shm_map);
            tail.methods.xShmUnmap = Some(trace_shm_unmap);
        }
        // Keep the native file layout intact. Only pMethods points into the
        // appended storage, so unchanged Unix methods still get a unixFile.
        let ptr = file
            .cast::<u8>()
            .add(TAIL_OFFSET.load(Ordering::Acquire))
            .cast::<Tail>();
        ptr.write(tail);
        (*file).pMethods = std::ptr::addr_of!((*ptr).methods);
        rc
    }
}

unsafe fn tail(file: *mut ffi::sqlite3_file) -> *const Tail {
    unsafe { (*file).pMethods.cast::<Tail>() }
}

unsafe fn enter(
    file: *mut ffi::sqlite3_file,
    site: &'static [u8],
    region: i64,
    size: i64,
) -> Context {
    CONTEXT.with(|slot| {
        slot.replace(Context {
            tail: unsafe { tail(file) },
            site,
            region,
            size,
        })
    })
}

unsafe extern "C" fn trace_close(file: *mut ffi::sqlite3_file) -> c_int {
    unsafe {
        let original = (*tail(file)).original;
        let previous = enter(file, b"xClose", -1, 0);
        let rc = ((*original).xClose.unwrap())(file);
        CONTEXT.with(|slot| slot.set(previous));
        rc
    }
}

unsafe extern "C" fn trace_shm_map(
    file: *mut ffi::sqlite3_file,
    region: c_int,
    size: c_int,
    extend: c_int,
    mapping: *mut *mut c_void,
) -> c_int {
    unsafe {
        let original = (*tail(file)).original;
        let previous = enter(file, b"xShmMap/unixShmMap", region as i64, size as i64);
        emit(b"enter", -1, extend as i64, 0, 0, 0, None);
        let rc = ((*original).xShmMap.unwrap())(file, region, size, extend, mapping);
        let returned_mapping = if rc == ffi::SQLITE_OK && !mapping.is_null() {
            *mapping as usize
        } else {
            0
        };
        emit(
            b"leave",
            -1,
            rc as i64,
            0,
            0,
            0,
            Some(returned_mapping),
        );
        CONTEXT.with(|slot| slot.set(previous));
        rc
    }
}

unsafe extern "C" fn trace_shm_unmap(file: *mut ffi::sqlite3_file, delete: c_int) -> c_int {
    unsafe {
        let original = (*tail(file)).original;
        let previous = enter(file, b"xShmUnmap/unixShmUnmap", -1, 0);
        emit(b"enter", -1, delete as i64, 0, 0, 0, None);
        let rc = ((*original).xShmUnmap.unwrap())(file, delete);
        emit(b"leave", -1, rc as i64, 0, 0, 0, None);
        CONTEXT.with(|slot| slot.set(previous));
        rc
    }
}

unsafe extern "C" fn trace_truncate(file: *mut ffi::sqlite3_file, size: i64) -> c_int {
    unsafe {
        let original = (*tail(file)).original;
        let previous = enter(file, b"xTruncate/unixTruncate", -1, size);
        emit(b"enter", -1, size, 0, 0, 0, None);
        let rc = ((*original).xTruncate.unwrap())(file, size);
        emit(b"leave", -1, rc as i64, 0, 0, 0, None);
        CONTEXT.with(|slot| slot.set(previous));
        rc
    }
}

unsafe extern "C" fn trace_ftruncate(fd: c_int, length: libc::off_t) -> c_int {
    unsafe {
        let original: unsafe extern "C" fn(c_int, libc::off_t) -> c_int =
            std::mem::transmute(FTRUNCATE.load(Ordering::Acquire));
        let errno = *errno_ptr();
        let mut stat = std::mem::zeroed::<libc::stat>();
        let known = libc::fstat(fd, &mut stat) == 0;
        emit(
            b"ftruncate_before",
            fd,
            length,
            stat.st_dev as u64,
            stat.st_ino as u64,
            if known { stat.st_size } else { -1 },
            None,
        );
        *errno_ptr() = errno;
        let rc = original(fd, length);
        let errno = *errno_ptr();
        let known = libc::fstat(fd, &mut stat) == 0;
        emit(
            b"ftruncate_after",
            fd,
            rc as i64,
            stat.st_dev as u64,
            stat.st_ino as u64,
            if known { stat.st_size } else { -1 },
            None,
        );
        *errno_ptr() = errno;
        rc
    }
}

#[cfg(target_os = "macos")]
unsafe fn errno_ptr() -> *mut c_int {
    unsafe { libc::__error() }
}
#[cfg(not(target_os = "macos"))]
unsafe fn errno_ptr() -> *mut c_int {
    unsafe { libc::__errno_location() }
}

// Diagnostics use stack storage and write(2), never the logger, allocator or
// unwinder. The fstat identity is the actual truncated descriptor, not a path
// probe that could already name a replacement. VFS enter/leave rows distinguish
// map attempts from completed maps even when recovery faults before returning.
unsafe fn emit(
    event: &[u8],
    fd: c_int,
    argument: i64,
    device: u64,
    inode: u64,
    length: i64,
    mapping: Option<usize>,
) {
    unsafe {
        let saved_errno = *errno_ptr();
        CONTEXT.with(|slot| {
            let context = slot.get();
            let mut line = Line {
                bytes: [0; 2048],
                len: 0,
            };
            line.text(b"\nAFT sqlite shm: event=");
            line.text(event);
            line.text(b" site=");
            line.text(context.site);
            line.field(b" thread=", libc::pthread_self() as u64);
            if !context.tail.is_null() {
                let t = &*context.tail;
                line.text(b" path=");
                line.text(&t.path[..t.path_len]);
                line.field(b" db_dev=", t.device);
                line.field(b" db_ino=", t.inode);
            }
            line.signed(b" region=", context.region);
            line.signed(b" map_size=", context.size);
            line.signed(b" fd=", fd as i64);
            line.signed(b" arg=", argument);
            line.field(b" fd_dev=", device);
            line.field(b" fd_ino=", inode);
            line.signed(b" file_size=", length);
            if let Some(mapping) = mapping {
                line.hex(b" map=", mapping);
            }
            line.text(b"\n");
            let mut sent = 0;
            while sent < line.len {
                let n = libc::write(
                    2,
                    line.bytes[sent..line.len].as_ptr().cast(),
                    line.len - sent,
                );
                if n <= 0 {
                    break;
                }
                sent += n as usize;
            }
        });
        *errno_ptr() = saved_errno;
    }
}

struct Line {
    bytes: [u8; 2048],
    len: usize,
}
impl Line {
    fn text(&mut self, text: &[u8]) {
        let n = text.len().min(self.bytes.len() - self.len);
        self.bytes[self.len..self.len + n].copy_from_slice(&text[..n]);
        self.len += n;
    }
    fn field(&mut self, name: &[u8], mut value: u64) {
        self.text(name);
        let mut digits = [0u8; 20];
        let mut start = digits.len();
        loop {
            start -= 1;
            digits[start] = b'0' + (value % 10) as u8;
            value /= 10;
            if value == 0 {
                break;
            }
        }
        self.text(&digits[start..]);
    }
    fn signed(&mut self, name: &[u8], value: i64) {
        self.text(name);
        if value < 0 {
            self.text(b"-");
        }
        self.field(b"", value.unsigned_abs());
    }
    fn hex(&mut self, name: &[u8], mut value: usize) {
        const DIGITS: &[u8; 16] = b"0123456789abcdef";
        self.text(name);
        self.text(b"0x");
        let mut bytes = [0_u8; usize::BITS as usize / 4];
        let mut start = bytes.len();
        loop {
            start -= 1;
            bytes[start] = DIGITS[value & 0xf];
            value >>= 4;
            if value == 0 {
                break;
            }
        }
        self.text(&bytes[start..]);
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn syscall_trace_observes_sqlite_shm_reset() {
        const CHILD: &str = "AFT_SHM_TRACE_CHILD_PATH";
        if let Some(path) = std::env::var_os(CHILD) {
            super::install();
            let connection = crate::db::TrackedConnection::open(
                std::path::Path::new(&path), crate::db::SqliteStore::CallgraphGeneration,
            ).unwrap();
            connection.execute_batch("PRAGMA journal_mode=WAL; CREATE TABLE trace_probe(value); INSERT INTO trace_probe VALUES(42);").unwrap();
            return;
        }
        // VFS installation is process-global. A child keeps this probe isolated
        // from other unit tests and lets the parent inspect real fd-2 output.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("trace.sqlite");
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "db::shm_diagnostics::tests::syscall_trace_observes_sqlite_shm_reset", "--nocapture"])
            .env("AFT_CAPTURE_CRASH_DIAGNOSTICS", "1")
            .env(CHILD, &path)
            .output().unwrap();
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(output.status.success(), "child failed: {stderr}");
        use std::os::unix::fs::MetadataExt;
        let shm = std::fs::metadata(format!("{}-shm", path.display())).unwrap();
        let reset_size = if cfg!(target_os = "macos") { 3 } else { 0 };
        let observed = stderr.lines().any(|line| {
            line.contains("AFT sqlite shm: event=ftruncate_before site=xShmMap")
                && line.contains("trace.sqlite")
                && line.contains(&format!(" fd_ino={} ", shm.ino()))
                && line.contains(&format!(" arg={reset_size} "))
        });
        assert!(observed, "SQLite's real WAL-index reset was not traced: {stderr}");
    }
}
