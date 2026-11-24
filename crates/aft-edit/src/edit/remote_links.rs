//! Hard-link checks at the remote mutation boundary, not for local edits.

use std::fs::{File, OpenOptions};
use std::io;
use std::path::Path;

#[doc(hidden)]
pub fn refused(path: &Path) -> io::Error {
    io::Error::new(
        io::ErrorKind::PermissionDenied,
        format!(
            "path '{}' is denied by the access policy floor",
            path.display()
        ),
    )
}

#[doc(hidden)]
pub fn check_open(file: &File, path: &Path) -> io::Result<()> {
    if aft_db::read_fence::remote_mutation() {
        check_count(crate::file_identity::link_count(file)?, path)?;
    }
    Ok(())
}

pub(crate) fn check_count(links: u64, path: &Path) -> io::Result<()> {
    if links != 1 {
        return Err(refused(path));
    }
    Ok(())
}

#[doc(hidden)]
pub fn check_path(path: &Path) -> io::Result<()> {
    if !aft_db::read_fence::remote_mutation() {
        return Ok(());
    }
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    if metadata.is_dir() {
        return Ok(());
    }
    #[cfg(unix)]
    if !metadata.is_file() {
        use std::os::unix::fs::MetadataExt;
        return check_count(metadata.nlink(), path);
    }
    let mut options = OpenOptions::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        #[cfg(target_os = "linux")]
        const FLAGS: libc::c_int = libc::O_PATH | libc::O_NOFOLLOW;
        #[cfg(not(target_os = "linux"))]
        const FLAGS: libc::c_int = libc::O_NOFOLLOW | libc::O_NONBLOCK;
        options.read(true).custom_flags(FLAGS);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        use windows_sys::Win32::Storage::FileSystem::{
            FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
        };
        options
            .access_mode(0)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT);
    }
    check_open(&options.open(path)?, path)
}

/// For a name-only operation, read the current entry relative to the same
/// directory handle used by the syscall. POSIX has no unlink-by-inode API.
#[cfg(unix)]
#[doc(hidden)]
pub fn check_at(dir: std::os::fd::RawFd, name: &std::ffi::CStr, path: &Path) -> io::Result<()> {
    if !aft_db::read_fence::remote_mutation() {
        return Ok(());
    }
    let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
    // SAFETY: the caller holds the parent open and provides one name.
    let status = unsafe {
        libc::fstatat(
            dir,
            name.as_ptr(),
            stat.as_mut_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    };
    if status != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: a successful fstatat initialized the structure.
    check_count(unsafe { stat.assume_init() }.st_nlink as u64, path)
}

#[doc(hidden)]
pub fn unlink(path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    if aft_db::read_fence::remote_mutation() {
        let (dir, name) = pin_parent(path)?;
        check_at(dir.as_raw_fd(), &name, path)?;
        // SAFETY: the pinned directory and single-component C string are live.
        if unsafe { libc::unlinkat(dir.as_raw_fd(), name.as_ptr(), 0) } != 0 {
            return Err(io::Error::last_os_error());
        }
        return Ok(());
    }
    #[cfg(windows)]
    if aft_db::read_fence::remote_mutation() {
        use super::delete_tree_win::{dispose, location, open_entry};
        let file = open_entry(path)?;
        if location(&file)? != crate::policy_path::policy_path(path) {
            return Err(refused(path));
        }
        check_open(&file, path)?;
        return dispose(&file);
    }
    std::fs::remove_file(path)
}

#[cfg(unix)]
#[doc(hidden)]
pub fn pin_parent(path: &Path) -> io::Result<(crate::pinned_dir::PinnedDir, std::ffi::CString)> {
    let (Some(parent), Some(name)) = (path.parent(), path.file_name()) else {
        return Err(refused(path));
    };
    let pinned = crate::pinned_dir::PinnedDir::open(parent)?;
    if !pinned.same_identity(&crate::pinned_dir::PinnedDir::open(parent)?)? {
        return Err(refused(path));
    }
    Ok((pinned, crate::pinned_dir::os_cstring(name)?))
}

/// Rollback is also a source write. Keep local rollback's old behavior, but
/// never truncate an unchecked descriptor when restoring a remote edit.
#[doc(hidden)]
pub fn restore(path: &Path, bytes: &[u8]) -> io::Result<()> {
    if aft_db::read_fence::remote_mutation() {
        return super::atomic_write_source(path, bytes).map(|_| ());
    }
    std::fs::write(path, bytes)
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn remote_unlink_refuses_a_multiply_linked_entry() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("target");
        let alias = dir.path().join("alias");
        fs::write(&target, b"original").unwrap();
        fs::hard_link(&target, &alias).unwrap();
        let result = aft_db::read_fence::with_remote_mutation(true, || unlink(&alias));
        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::PermissionDenied);
        assert_eq!(fs::read(&target).unwrap(), b"original");
        assert_eq!(fs::read(&alias).unwrap(), b"original");
    }

    #[test]
    fn remote_edit_checks_the_in_place_writer_and_local_edit_keeps_links() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("target");
        let alias = dir.path().join("alias");
        fs::write(&target, b"original").unwrap();
        fs::hard_link(&target, &alias).unwrap();
        let error = aft_db::read_fence::with_remote_mutation(true, || {
            super::super::write_source_in_place(
                &target,
                &fs::metadata(&target).unwrap(),
                b"blocked",
                &mut |_, _| panic!("multi-link descriptor reached the source writer"),
                Vec::new(),
            )
        })
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        assert_eq!(error.to_string(), refused(&target).to_string());
        assert_eq!(fs::read(&target).unwrap(), b"original");
        assert_eq!(fs::read(&alias).unwrap(), b"original");

        let opened = File::open(&target).unwrap();
        fs::remove_file(&target).unwrap();
        fs::hard_link(&alias, dir.path().join("second-alias")).unwrap();
        fs::write(&target, b"new name").unwrap();
        // The displayed path is now single-link, but the opened inode is not.
        assert!(
            aft_db::read_fence::with_remote_mutation(true, || check_open(&opened, &target))
                .is_err()
        );
        assert_eq!(
            super::super::atomic_write_source(&alias, b"local contents")
                .unwrap()
                .mode,
            super::super::SourceWriteMode::InPlace
        );
        assert_eq!(
            fs::read(dir.path().join("second-alias")).unwrap(),
            b"local contents"
        );
        assert_eq!(fs::read(target).unwrap(), b"new name");
    }
}
