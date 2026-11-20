//! Open checked file paths without following replaced components.

use std::fs::{File, OpenOptions};
use std::io;
use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpenError {
    AccessDenied,
    NotFound,
    IoError,
}

/// A file opened for an exact read. Closing any descriptor to a live SQLite
/// file releases SQLite's process-wide POSIX locks and can crash this process,
/// so on drop the handle is closed only when the live SQLite registry does not
/// identify it by its device and inode; otherwise (or when the handle cannot
/// even be identified) it is deliberately leaked. Every return after the open,
/// refusal or success, goes through this drop.
pub struct OpenedFile(std::mem::ManuallyDrop<File>);

/// Open below a checked root without following a swapped path component. The
/// caller must verify the handle location and policy before reading any bytes.
pub fn open_no_follow(root: &Path, target: &Path) -> Result<OpenedFile, OpenError> {
    open_contents(root, target).map(OpenedFile::new)
}

impl OpenedFile {
    pub fn new(file: File) -> Self {
        Self(std::mem::ManuallyDrop::new(file))
    }
}

impl std::ops::Deref for OpenedFile {
    type Target = File;
    fn deref(&self) -> &File {
        &self.0
    }
}

impl std::ops::DerefMut for OpenedFile {
    fn deref_mut(&mut self) -> &mut File {
        &mut self.0
    }
}

impl Drop for OpenedFile {
    fn drop(&mut self) {
        let closable = self
            .0
            .metadata()
            .is_ok_and(|metadata| !crate::live_db_files::is_live_sqlite_handle(&metadata));
        if closable {
            // SAFETY: the file is dropped exactly once, here, and never used after.
            unsafe { std::mem::ManuallyDrop::drop(&mut self.0) };
        }
    }
}

fn open_error(error: io::Error) -> OpenError {
    if error.kind() == io::ErrorKind::NotFound {
        OpenError::NotFound
    } else {
        OpenError::IoError
    }
}

/// Opens `target` by walking down from an opened `root` directory, one
/// component at a time, without following a symlink at any step.
///
/// `target` is the canonical path the checks approved, so it has no symlink
/// components; one found during the walk was swapped in after the checks and
/// is refused. O_NOFOLLOW on a plain path open would protect only the final
/// component and still follow a swapped parent directory out of the checkout.
#[cfg(unix)]
pub fn open_contents(root: &Path, target: &Path) -> Result<File, OpenError> {
    use std::os::fd::{AsRawFd, FromRawFd};
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::OpenOptionsExt;
    let relative = target
        .strip_prefix(root)
        .map_err(|_| OpenError::AccessDenied)?;
    let names = relative
        .components()
        .map(|component| match component {
            std::path::Component::Normal(name) => {
                std::ffi::CString::new(name.as_bytes()).map_err(|_| OpenError::AccessDenied)
            }
            _ => Err(OpenError::AccessDenied),
        })
        .collect::<Result<Vec<_>, _>>()?;
    // The root and every parent are opened only to look names up in them, so
    // they get search-only handles: a plain open of the file needs search
    // (execute) permission on its parents, not read (list) permission, and
    // the walk must not refuse a file under a parent like a 0711 directory
    // owned by someone else. Nonblocking opens of the final component keep a
    // FIFO replacement from stalling before fstat.
    let mut handle = OpenOptions::new()
        .read(true)
        .custom_flags(SEARCH_DIRECTORY | libc::O_NOFOLLOW)
        .open(root)
        .map_err(open_error)?;
    for (index, name) in names.iter().enumerate() {
        let last = index + 1 == names.len();
        let flags = libc::O_NOFOLLOW
            | libc::O_CLOEXEC
            | if last {
                libc::O_RDONLY | libc::O_NONBLOCK
            } else {
                SEARCH_DIRECTORY
            };
        // SAFETY: `handle` is an open directory and `name` is NUL-terminated.
        let fd = unsafe { libc::openat(handle.as_raw_fd(), name.as_ptr(), flags) };
        if fd < 0 {
            let error = io::Error::last_os_error();
            // A parent that is no longer a real directory (a symlink gives
            // ELOOP or ENOTDIR) was swapped in after the checks.
            if !last && matches!(error.raw_os_error(), Some(libc::ELOOP | libc::ENOTDIR)) {
                return Err(OpenError::AccessDenied);
            }
            return Err(open_error(error));
        }
        // SAFETY: `fd` was just returned by openat and nothing else owns it.
        handle = unsafe { File::from_raw_fd(fd) };
    }
    Ok(handle)
}

/// Open flags for a directory handle used only as an `openat` base. Linux's
/// O_PATH and Apple's O_SEARCH need search permission on the directory but
/// not read permission. The fstat, identity and real-location checks run on
/// the final file's handle only, which is still a plain O_RDONLY open.
#[cfg(any(target_os = "linux", target_os = "android"))]
const SEARCH_DIRECTORY: libc::c_int = libc::O_PATH | libc::O_DIRECTORY;
#[cfg(target_vendor = "apple")]
const SEARCH_DIRECTORY: libc::c_int = libc::O_SEARCH;
/// Other Unix systems get a read handle, so there a file whose parent grants
/// search but not read permission (such as a 0711 directory owned by another
/// user) is refused with an I/O error even though a plain open could read it.
#[cfg(all(
    unix,
    not(any(target_os = "linux", target_os = "android", target_vendor = "apple"))
))]
const SEARCH_DIRECTORY: libc::c_int = libc::O_RDONLY | libc::O_DIRECTORY;

/// Windows has no component-relative open here, so the parent directories
/// are resolved by path; `opened_location` then judges where the handle
/// really points before any byte is read.
#[cfg(windows)]
pub fn open_contents(_root: &Path, target: &Path) -> Result<File, OpenError> {
    use std::os::windows::fs::OpenOptionsExt;
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
    };
    OpenOptions::new()
        .read(true)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_BACKUP_SEMANTICS)
        .open(target)
        .map_err(open_error)
}
