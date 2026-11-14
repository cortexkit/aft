//! Filesystem identity of a directory, independent of its path spelling.

use std::path::Path;

/// A directory's device and inode (on Windows, volume serial number and
/// file index), which change when it is replaced.
pub type DirIdentity = (u64, u64);

#[cfg(unix)]
pub fn dir_identity(path: &Path) -> Option<DirIdentity> {
    use std::os::unix::fs::MetadataExt;
    std::fs::metadata(path)
        .ok()
        .map(|metadata| (metadata.dev(), metadata.ino()))
}

/// On Windows, the volume serial number and the 64-bit file index, which is
/// what an inode is on Unix. Creation time would not do: NTFS file-name
/// tunneling gives a name reused within about 15 s the creation time of the
/// entry that last had it, so a directory renamed aside and recreated looks
/// unchanged by that measure.
#[cfg(windows)]
pub(crate) fn dir_identity(path: &Path) -> Option<DirIdentity> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION,
        FILE_FLAG_BACKUP_SEMANTICS, FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE, FILE_SHARE_READ,
        FILE_SHARE_WRITE, OPEN_EXISTING,
    };

    // A deep storage or cache path is past the plain-path limit of CreateFileW.
    let path = crate::windows_path::win32_call_path(path);
    let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    // SAFETY: `wide` is a NUL-terminated UTF-16 path that outlives the call;
    // the remaining arguments are plain flags and null pointers, and the
    // returned handle is checked before use and closed below.
    let handle = unsafe {
        CreateFileW(
            wide.as_ptr(),
            FILE_READ_ATTRIBUTES,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            std::ptr::null(),
            OPEN_EXISTING,
            // Required to open a directory handle.
            FILE_FLAG_BACKUP_SEMANTICS,
            std::ptr::null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        return None;
    }
    // SAFETY: an all-zero BY_HANDLE_FILE_INFORMATION is a valid value (plain
    // integers and FILETIME structs); the call only writes into it.
    let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
    // SAFETY: `handle` is a valid open handle and `info` a valid out pointer.
    let ok = unsafe { GetFileInformationByHandle(handle, &mut info) } != 0;
    // SAFETY: `handle` came from CreateFileW above and is closed exactly once.
    unsafe {
        CloseHandle(handle);
    }
    ok.then(|| {
        (
            u64::from(info.dwVolumeSerialNumber),
            (u64::from(info.nFileIndexHigh) << 32) | u64::from(info.nFileIndexLow),
        )
    })
}

#[cfg(not(any(unix, windows)))]
pub(crate) fn dir_identity(_path: &Path) -> Option<DirIdentity> {
    None
}

#[cfg(all(test, any(unix, windows)))]
mod tests {
    use super::*;

    #[test]
    fn rename_preserves_identity_but_replacement_changes_it() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("original");
        let renamed = dir.path().join("renamed");
        std::fs::create_dir(&path).unwrap();
        let original = dir_identity(&path).unwrap();
        std::fs::rename(&path, &renamed).unwrap();
        assert_eq!(dir_identity(&renamed), Some(original));
        assert_eq!(dir_identity(&path), None);
        std::fs::create_dir(&path).unwrap();
        assert_ne!(dir_identity(&path), Some(original));
    }
}
