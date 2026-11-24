//! Handle-based removal shared by remote edits and tree deletion.

use std::fs::File;
use std::io;
use std::path::{Path, PathBuf};

#[doc(hidden)]
pub const DELETE: u32 = 0x0001_0000;
#[doc(hidden)]
pub const FILE_READ_ATTRIBUTES: u32 = 0x0000_0080;
#[doc(hidden)]
pub const FILE_SHARE_ALL: u32 = 0x0000_0007;
const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
const FILE_DISPOSITION_INFO: i32 = 4;
const FILE_DISPOSITION_INFO_EX: i32 = 21;
const FILE_DISPOSITION_FLAG_DELETE: u32 = 0x0000_0001;
const FILE_DISPOSITION_FLAG_POSIX_SEMANTICS: u32 = 0x0000_0002;

#[link(name = "kernel32")]
extern "system" {
    fn SetFileInformationByHandle(
        file: std::os::windows::io::RawHandle,
        class: i32,
        information: *mut std::ffi::c_void,
        size: u32,
    ) -> i32;
}

/// Open the entry at `path` itself, never a link's target, with the
/// access a delete needs. Directories open too (`BACKUP_SEMANTICS`).
#[doc(hidden)]
pub fn open_entry(path: &Path) -> io::Result<File> {
    use std::os::windows::fs::OpenOptionsExt;
    std::fs::OpenOptions::new()
        .access_mode(DELETE | FILE_READ_ATTRIBUTES)
        .share_mode(FILE_SHARE_ALL)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_BACKUP_SEMANTICS)
        .open(path)
}

/// Where the open entry is now, in the form the access policy uses for
/// its identities (no `\\?\` prefix). With the link itself opened, this
/// names the link entry, not what it points at.
#[doc(hidden)]
pub fn location(entry: &File) -> io::Result<PathBuf> {
    crate::file_read_exact::opened_location(entry)
        .map(|path| crate::path_identity::normalize_path(&path))
}

/// Delete the open entry through its handle: POSIX semantics where the
/// volume supports them (the name goes away now), otherwise
/// delete-on-close (the name goes away when the handle is dropped).
///
/// The disposition applies to the object the handle holds, wherever it is
/// now. That object was the one authorized; if it was renamed since the
/// last check, deleting it is still within the delete contract (objects
/// are authorized, not pathnames at the instant of unlink).
#[doc(hidden)]
pub fn dispose(entry: &File) -> io::Result<()> {
    use std::os::windows::io::AsRawHandle;
    let handle = entry.as_raw_handle();
    let mut flags = FILE_DISPOSITION_FLAG_DELETE | FILE_DISPOSITION_FLAG_POSIX_SEMANTICS;
    // SAFETY: `entry` is open for the call and the buffer matches the
    // size given for FILE_DISPOSITION_INFO_EX.
    let posix = unsafe {
        SetFileInformationByHandle(
            handle,
            FILE_DISPOSITION_INFO_EX,
            (&mut flags as *mut u32).cast(),
            std::mem::size_of_val(&flags) as u32,
        )
    };
    if posix != 0 {
        return Ok(());
    }
    let error = io::Error::last_os_error();
    // ERROR_NOT_SUPPORTED or ERROR_INVALID_PARAMETER: no POSIX delete on
    // this volume. Anything else (access, sharing, not empty) is real.
    if !matches!(error.raw_os_error(), Some(50 | 87)) {
        return Err(error);
    }
    let mut delete_file = 1_u8;
    // SAFETY: as above; FILE_DISPOSITION_INFO is a single BOOLEAN.
    let legacy = unsafe {
        SetFileInformationByHandle(
            handle,
            FILE_DISPOSITION_INFO,
            (&mut delete_file as *mut u8).cast(),
            std::mem::size_of_val(&delete_file) as u32,
        )
    };
    if legacy == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}
