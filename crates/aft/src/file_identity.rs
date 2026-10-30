//! A file's identity on its volume, for in-memory change stamps.
//!
//! A stamp built only from a file's size and times misses a replacement that
//! keeps the size and lands within one tick of the filesystem clock. Windows
//! stamps file times from a clock that ticks about every 15.6 ms, and Linux
//! filesystems take theirs from the kernel's coarse clock, which ticks every
//! few milliseconds. AFT and Git replace files atomically (write a temporary
//! file, then rename it over the old name), and the renamed file is a
//! different file, so its identity differs even when every time and the size
//! match.
//!
//! Unix reports the identity in every `stat`: the device and inode numbers.
//! Rust's stable Windows API exposes no file index, so on Windows it is read
//! with `GetFileInformationByHandle` from a handle that requests no data
//! access, the same kind of handle `std::fs::metadata` opens internally.
//!
//! An identity is only meaningful on the machine that read it. Do not use it
//! as a portable file identifier. A per-machine callgraph cache may persist it
//! as a stat hint alongside size and mtime; a missing or unavailable identity
//! must still fall back to checking the contents.

use std::fs::{File, Metadata};
use std::io;
use std::path::Path;

/// Which volume a file lives on and which file it is on that volume.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum FileIdentity {
    /// Unix: device and inode numbers. Windows: volume serial number and
    /// file index.
    Known { volume: u64, index: u64 },
    /// The platform or filesystem did not report an identity. Two unavailable
    /// identities compare equal, so a stamp holding one detects changes only
    /// through its other fields, exactly as it did before it held an identity.
    Unavailable,
}

impl FileIdentity {
    /// The identity of an open file.
    pub(crate) fn of_file(file: &File) -> Self {
        #[cfg(unix)]
        {
            file.metadata().map_or(Self::Unavailable, |metadata| {
                Self::of_unix_metadata(&metadata)
            })
        }
        #[cfg(windows)]
        {
            windows_identity(file).0
        }
        #[cfg(not(any(unix, windows)))]
        {
            let _ = file;
            Self::Unavailable
        }
    }

    /// Unix `stat` results carry the identity, so no extra system call is
    /// needed. Windows metadata does not, which is why this exists only on
    /// Unix: Windows callers use [`metadata`], [`symlink_metadata`] or
    /// [`FileIdentity::of_file`].
    #[cfg(unix)]
    pub(crate) fn of_unix_metadata(metadata: &Metadata) -> Self {
        use std::os::unix::fs::MetadataExt;
        Self::Known {
            volume: metadata.dev(),
            index: metadata.ino(),
        }
    }
}

/// Like `std::fs::metadata` (follows symlinks), plus the identity of the same
/// file.
pub(crate) fn metadata(path: &Path) -> io::Result<(Metadata, FileIdentity)> {
    stat(path, true).map(|(metadata, identity, _)| (metadata, identity))
}

/// Like `std::fs::symlink_metadata` (describes a symlink itself rather than
/// its target), plus the identity of the same file.
pub(crate) fn symlink_metadata(path: &Path) -> io::Result<(Metadata, FileIdentity)> {
    stat(path, false).map(|(metadata, identity, _)| (metadata, identity))
}

/// Read identity and link count in the same metadata query. SQLite alias
/// admission uses the count to avoid refreshing sidecars for ordinary files.
pub(crate) fn metadata_with_link_count(path: &Path) -> io::Result<(Metadata, FileIdentity, u64)> {
    stat(path, true)
}

#[cfg(unix)]
fn stat(path: &Path, follow_symlinks: bool) -> io::Result<(Metadata, FileIdentity, u64)> {
    use std::os::unix::fs::MetadataExt;
    let metadata = if follow_symlinks {
        std::fs::metadata(path)?
    } else {
        std::fs::symlink_metadata(path)?
    };
    let identity = FileIdentity::of_unix_metadata(&metadata);
    let links = metadata.nlink();
    Ok((metadata, identity, links))
}

/// One handle supplies both the metadata and the identity, so they always
/// describe the same file. Like `std::fs::metadata`, the handle requests no
/// data access, shares read, write and delete, and can open a directory, so
/// it neither blocks a concurrent writer's rename over the file nor costs
/// more than the metadata read it replaces.
#[cfg(windows)]
fn stat(path: &Path, follow_symlinks: bool) -> io::Result<(Metadata, FileIdentity, u64)> {
    use std::os::windows::fs::OpenOptionsExt;
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
    };
    let flags = FILE_FLAG_BACKUP_SEMANTICS
        | if follow_symlinks {
            0
        } else {
            FILE_FLAG_OPEN_REPARSE_POINT
        };
    match std::fs::OpenOptions::new()
        .access_mode(0)
        .custom_flags(flags)
        .open(path)
    {
        Ok(file) => {
            let metadata = file.metadata()?;
            let (identity, links) = windows_identity(&file);
            Ok((metadata, identity, links))
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => Err(error),
        // `std::fs::metadata` falls back to reading the directory entry when
        // the file cannot be opened (for example while it is being deleted).
        // Keep that behavior; a directory entry carries no file index.
        Err(_) => {
            let metadata = if follow_symlinks {
                std::fs::metadata(path)?
            } else {
                std::fs::symlink_metadata(path)?
            };
            Ok((metadata, FileIdentity::Unavailable, 1))
        }
    }
}

#[cfg(not(any(unix, windows)))]
fn stat(path: &Path, follow_symlinks: bool) -> io::Result<(Metadata, FileIdentity, u64)> {
    let metadata = if follow_symlinks {
        std::fs::metadata(path)?
    } else {
        std::fs::symlink_metadata(path)?
    };
    Ok((metadata, FileIdentity::Unavailable, 1))
}

#[cfg(windows)]
fn windows_identity(file: &File) -> (FileIdentity, u64) {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{
        GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION,
    };
    let mut info = std::mem::MaybeUninit::<BY_HANDLE_FILE_INFORMATION>::uninit();
    // SAFETY: the open file owns a valid handle and `info` is writable. A
    // successful call initializes the structure before it is read below.
    let info = unsafe {
        if GetFileInformationByHandle(file.as_raw_handle(), info.as_mut_ptr()) == 0 {
            return (FileIdentity::Unavailable, 1);
        }
        info.assume_init()
    };
    (
        FileIdentity::Known {
            volume: u64::from(info.dwVolumeSerialNumber),
            index: (u64::from(info.nFileIndexHigh) << 32) | u64::from(info.nFileIndexLow),
        },
        u64::from(info.nNumberOfLinks),
    )
}

/// Atomically replaces the file at `path` with `contents` of the same length,
/// giving the replacement the original's access, modification and (on
/// Windows) creation times. Size and times then all match, as they do when a
/// replacement lands within one clock tick, so only the file identity can
/// tell the two files apart.
#[cfg(test)]
pub(crate) fn replace_keeping_times(path: &Path, contents: &[u8]) {
    let metadata = std::fs::metadata(path).unwrap();
    assert_eq!(
        metadata.len(),
        contents.len() as u64,
        "the replacement must keep the original's length"
    );
    let mut name = path.file_name().unwrap().to_os_string();
    name.push(".replacement");
    let replacement = path.with_file_name(name);
    std::fs::write(&replacement, contents).unwrap();
    let times = std::fs::FileTimes::new()
        .set_accessed(metadata.accessed().unwrap())
        .set_modified(metadata.modified().unwrap());
    #[cfg(windows)]
    let times = {
        use std::os::windows::fs::FileTimesExt;
        times.set_created(metadata.created().unwrap())
    };
    std::fs::OpenOptions::new()
        .write(true)
        .open(&replacement)
        .unwrap()
        .set_times(times)
        .unwrap();
    std::fs::rename(&replacement, path).unwrap();
    let after = std::fs::metadata(path).unwrap();
    assert_eq!(after.len(), metadata.len());
    assert_eq!(after.modified().unwrap(), metadata.modified().unwrap());
    #[cfg(windows)]
    assert_eq!(after.created().unwrap(), metadata.created().unwrap());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replacement_with_identical_size_and_times_has_a_new_identity() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("record");
        std::fs::write(&path, b"first").unwrap();
        let (_, before) = metadata(&path).unwrap();
        assert!(
            cfg!(not(any(unix, windows))) || matches!(before, FileIdentity::Known { .. }),
            "a local temporary file must report its identity: {before:?}"
        );
        assert_eq!(metadata(&path).unwrap().1, before, "an unchanged file");
        assert_eq!(
            FileIdentity::of_file(&File::open(&path).unwrap()),
            before,
            "a path and an open handle name the same file"
        );
        replace_keeping_times(&path, b"other");
        if cfg!(any(unix, windows)) {
            assert_ne!(metadata(&path).unwrap().1, before);
        }
    }

    #[test]
    fn directories_and_missing_paths() {
        let directory = tempfile::tempdir().unwrap();
        let (metadata, identity) = symlink_metadata(directory.path()).unwrap();
        assert!(metadata.is_dir());
        assert!(cfg!(not(any(unix, windows))) || matches!(identity, FileIdentity::Known { .. }));
        let missing = directory.path().join("missing");
        assert_eq!(
            super::metadata(&missing).unwrap_err().kind(),
            io::ErrorKind::NotFound
        );
        assert_eq!(
            symlink_metadata(&missing).unwrap_err().kind(),
            io::ErrorKind::NotFound
        );
    }

    #[test]
    fn symlink_metadata_describes_the_link_and_metadata_its_target() {
        let directory = tempfile::tempdir().unwrap();
        let target = directory.path().join("target");
        std::fs::write(&target, b"target").unwrap();
        let link = directory.path().join("link");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&target, &link).unwrap();
        #[cfg(windows)]
        if std::os::windows::fs::symlink_file(&target, &link).is_err() {
            // Creating a symlink needs Developer Mode or an elevated account.
            return;
        }
        #[cfg(not(any(unix, windows)))]
        return;
        let (link_metadata, link_identity) = symlink_metadata(&link).unwrap();
        assert!(link_metadata.file_type().is_symlink());
        let (target_metadata, target_identity) = metadata(&link).unwrap();
        assert!(target_metadata.is_file());
        assert_eq!(target_identity, metadata(&target).unwrap().1);
        assert_ne!(link_identity, target_identity);
    }
}
