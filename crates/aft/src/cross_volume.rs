//! Moves between storage folders that may sit on different volumes.
//!
//! The rebuildable cache folders under the storage root (views, blobs,
//! callgraph, semantic, index, symbols, inspect) can be placed on a separate
//! volume, as a mount point or through a symlink. A `rename(2)` from one
//! volume to another fails with `EXDEV` (`ERROR_NOT_SAME_DEVICE` on Windows),
//! so a move whose source and destination folders can differ in volume goes
//! through [`move_path`], which falls back to a copy that is published by a
//! same-folder, no-replace rename in the destination and only then removes the
//! source. Filesystems without no-replace support prefer an atomic hard link
//! for regular files. Directories, and files where hard links are unsupported,
//! use a non-atomic existence check before renaming.
//!
//! [`rename`] also refuses to replace an existing destination. Tests make it
//! report the cross-device error for folders registered with
//! `crate::walk_boundary::simulated_volumes`, because CI has no second volume.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

/// True when `error` is the operating system's "cannot move across volumes"
/// error.
pub(crate) fn is_cross_device(error: &io::Error) -> bool {
    #[cfg(unix)]
    if error.raw_os_error() == Some(libc::EXDEV) {
        return true;
    }
    // ERROR_NOT_SAME_DEVICE
    #[cfg(windows)]
    if error.raw_os_error() == Some(17) {
        return true;
    }
    error.kind() == io::ErrorKind::CrossesDevices
}

#[cfg(test)]
fn cross_device_error() -> io::Error {
    #[cfg(unix)]
    return io::Error::from_raw_os_error(libc::EXDEV);
    #[cfg(windows)]
    return io::Error::from_raw_os_error(17);
    #[cfg(not(any(unix, windows)))]
    return io::Error::from(io::ErrorKind::CrossesDevices);
}

/// Rename without replacing a destination, including an empty directory.
/// When the filesystem lacks no-replace support, directories and files without
/// hard-link support use a non-atomic existence check and rename. Each fallback
/// reason produces one process-wide warning.
// Cache retirement now uses directory-handle-relative renames. Keep this
// pathname helper for the cross-volume fallback fixtures and other move users.
#[allow(dead_code)]
pub(crate) fn rename(from: &Path, to: &Path) -> io::Result<()> {
    if let Some(error) = publish_no_replace(from, to)? {
        Moved::PublishedWithWarning(error).warn_if_incomplete(from, to);
    }
    Ok(())
}

/// A warning means publication succeeded, but the hard-link fallback could
/// not unlink its source. Never turn that into a pre-publication failure.
fn publish_no_replace(from: &Path, to: &Path) -> io::Result<Option<io::Error>> {
    #[cfg(test)]
    {
        use crate::walk_boundary::simulated_volumes::device_of;
        let source = device_of(from);
        let destination = to.parent().and_then(device_of);
        if (source.is_some() || destination.is_some()) && source != destination {
            return Err(cross_device_error());
        }
    }
    native_publish_no_replace(from, to)
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn native_publish_no_replace(from: &Path, to: &Path) -> io::Result<Option<io::Error>> {
    let result = flagged_rename(from, to);
    rename_or_fallback(from, to, result)
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn flagged_rename(from: &Path, to: &Path) -> io::Result<()> {
    #[cfg(test)]
    if let Some(errno) = UNSUPPORTED_RENAME.with(|slot| slot.take()) {
        return Err(io::Error::from_raw_os_error(errno));
    }
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    let source = CString::new(from.as_os_str().as_bytes())?;
    let destination = CString::new(to.as_os_str().as_bytes())?;
    // SAFETY: both paths are NUL-terminated and live through the syscall.
    #[cfg(target_os = "macos")]
    let result =
        unsafe { libc::renamex_np(source.as_ptr(), destination.as_ptr(), libc::RENAME_EXCL) };
    #[cfg(target_os = "linux")]
    let result = unsafe {
        libc::syscall(
            libc::SYS_renameat2,
            libc::AT_FDCWD,
            source.as_ptr(),
            libc::AT_FDCWD,
            destination.as_ptr(),
            libc::RENAME_NOREPLACE,
        )
    };
    if result == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn rename_or_fallback(
    from: &Path,
    to: &Path,
    result: io::Result<()>,
) -> io::Result<Option<io::Error>> {
    match result {
        Ok(()) => Ok(None),
        Err(error) if no_replace_unsupported(&error) => {
            let metadata = fs::symlink_metadata(from)?;
            if metadata.is_dir() {
                return checked_rename(from, to, CheckedRenameReason::DirectoryWithoutNoReplace);
            }
            // Hard links still publish regular files atomically when native
            // no-replace flags are unavailable. Other entry types are refused.
            if !metadata.is_file() {
                return Err(error);
            }
            match hard_link_for_publication(from, to) {
                Ok(()) => {}
                Err(error) if hard_link_unsupported(&error) => {
                    return checked_rename(from, to, CheckedRenameReason::FileWithoutHardLinks);
                }
                Err(error) => return Err(error),
            }
            Ok(fs::remove_file(from).err().map(|error| {
                io::Error::new(
                    error.kind(),
                    format!(
                        "could not unlink published source {}: {error}",
                        from.display()
                    ),
                )
            }))
        }
        Err(error) => Err(error),
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
enum CheckedRenameReason {
    DirectoryWithoutNoReplace,
    FileWithoutHardLinks,
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn checked_rename(
    from: &Path,
    to: &Path,
    reason: CheckedRenameReason,
) -> io::Result<Option<io::Error>> {
    // Some external/network filesystems support neither native no-replace nor
    // hard links. Preserve moves there with the old rename behavior, but check
    // first. Another process can still create a destination between this check
    // and rename, and that destination can be replaced. The race is confined
    // to filesystems where atomic publication is unavailable for this entry.
    static DIRECTORY_WARN: std::sync::Once = std::sync::Once::new();
    static FILE_WARN: std::sync::Once = std::sync::Once::new();
    let (warning, reason) = match reason {
        CheckedRenameReason::DirectoryWithoutNoReplace => (
            &DIRECTORY_WARN,
            "atomic no-replace rename is unavailable for directories",
        ),
        CheckedRenameReason::FileWithoutHardLinks => (
            &FILE_WARN,
            "atomic no-replace rename and hard links are unavailable for files",
        ),
    };
    warning.call_once(|| {
        crate::slog_warn!(
            "{reason} at {}; using a non-atomic existence check before rename, which can race with destination creation",
            to.display()
        );
    });
    match fs::symlink_metadata(to) {
        Ok(_) => return Err(io::Error::from(io::ErrorKind::AlreadyExists)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    fs::rename(from, to).map(|()| None)
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn hard_link_unsupported(error: &io::Error) -> bool {
    // macOS msdos/exFAT reports EPERM for unsupported hard links. A link-count
    // limit (EMLINK) or another permission/I/O failure must remain a refusal.
    #[cfg(target_os = "macos")]
    if error.raw_os_error() == Some(libc::EPERM) {
        return true;
    }
    error
        .raw_os_error()
        .is_some_and(|errno| errno == libc::ENOTSUP || errno == libc::EOPNOTSUPP)
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn hard_link_for_publication(from: &Path, to: &Path) -> io::Result<()> {
    #[cfg(test)]
    if let Some(errno) = UNSUPPORTED_LINK.with(|slot| slot.take()) {
        return Err(io::Error::from_raw_os_error(errno));
    }
    fs::hard_link(from, to)
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn no_replace_unsupported(error: &io::Error) -> bool {
    #[cfg(target_os = "macos")]
    return error.raw_os_error() == Some(libc::ENOTSUP);
    #[cfg(target_os = "linux")]
    matches!(
        error.raw_os_error(),
        Some(libc::EINVAL | libc::ENOSYS | libc::ENOTSUP)
    )
}

#[cfg(windows)]
fn native_publish_no_replace(from: &Path, to: &Path) -> io::Result<Option<io::Error>> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::MoveFileExW;
    fn wide(path: &Path) -> io::Result<Vec<u16>> {
        let mut wide: Vec<u16> = path.as_os_str().encode_wide().collect();
        if wide.contains(&0) {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "NUL in path"));
        }
        wide.push(0);
        Ok(wide)
    }
    let source = wide(from)?;
    let destination = wide(to)?;
    // SAFETY: valid, NUL-terminated paths. No REPLACE_EXISTING or COPY_ALLOWED:
    // collisions must be refused and cross-volume moves must use our staging.
    if unsafe { MoveFileExW(source.as_ptr(), destination.as_ptr(), 0) } != 0 {
        Ok(None)
    } else {
        let error = io::Error::last_os_error();
        // Windows may report ACCESS_DENIED for an existing directory. Check
        // only after the atomic refusal; this check never authorizes replacement.
        if error.kind() == io::ErrorKind::PermissionDenied && fs::symlink_metadata(to).is_ok() {
            Err(io::Error::new(io::ErrorKind::AlreadyExists, error))
        } else {
            Err(error)
        }
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
fn native_publish_no_replace(_from: &Path, _to: &Path) -> io::Result<Option<io::Error>> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "atomic no-replace publication is unavailable on this platform",
    ))
}

/// True when `path` is on a different volume from its parent folder, which is
/// what a mount point looks like. False when either device cannot be read
/// (Windows standard metadata has no volume identifier).
pub(crate) fn is_mount_point(path: &Path) -> io::Result<bool> {
    let Some(parent) = path.parent() else {
        return Ok(false);
    };
    let own = crate::walk_boundary::filesystem_device_id(path)?;
    let above = crate::walk_boundary::filesystem_device_id(parent)?;
    Ok(matches!((own, above), (Some(own), Some(above)) if own != above))
}

/// A successful result always makes the destination authoritative, even when
/// finalization fails and a stale source copy remains.
#[derive(Debug)]
#[must_use = "report any move finalization warning before using the destination"]
pub(crate) enum Moved {
    /// One rename: source and destination are on the same volume.
    Renamed,
    /// The volumes differ: the source was copied, published under the
    /// destination name, and then removed.
    Copied,
    /// The destination was published, but directory sync or source cleanup
    /// failed. Callers must use the destination, not a leftover source copy.
    PublishedWithWarning(io::Error),
}

impl Moved {
    pub(crate) fn warn_if_incomplete(self, from: &Path, to: &Path) {
        if let Self::PublishedWithWarning(error) = self {
            crate::slog_warn!(
                "moved {} to {}, but finalization failed; using the destination: {error}",
                from.display(),
                to.display()
            );
        }
    }
}

/// A collision leaves the source untouched and belongs to the caller's policy:
/// state repair adopts the winner, while data migrations retain their source.
#[derive(Debug)]
pub(crate) enum MoveError {
    AlreadyPublished { cleanup_warning: Option<io::Error> },
    Failed(io::Error),
}

impl MoveError {
    #[cfg(test)]
    pub(crate) fn kind(&self) -> io::ErrorKind {
        match self {
            Self::AlreadyPublished { .. } => io::ErrorKind::AlreadyExists,
            Self::Failed(error) => error.kind(),
        }
    }

    pub(crate) fn warn_if_incomplete(&self, to: &Path) {
        if let Self::AlreadyPublished {
            cleanup_warning: Some(error),
        } = self
        {
            crate::slog_warn!("using already-published {}: {error}", to.display());
        }
    }
}

impl std::fmt::Display for MoveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::AlreadyPublished { cleanup_warning } => {
                write!(f, "destination already published; source left in place")?;
                if let Some(warning) = cleanup_warning {
                    write!(f, "; {warning}")?;
                }
                Ok(())
            }
            Self::Failed(error) => error.fmt(f),
        }
    }
}

impl std::error::Error for MoveError {}

impl From<io::Error> for MoveError {
    fn from(error: io::Error) -> Self {
        Self::Failed(error)
    }
}

/// Move the file or directory `from` to `to`, which must not exist.
///
/// A same-volume move is one rename. A cross-volume move copies `from` into a
/// staging name inside `to`'s own folder, syncs every copied file and folder,
/// publishes the staging copy without replacement, and removes `from` last. If
/// the copy fails, staging cleanup is attempted and `from` is left untouched.
/// The source is authoritative unless [`MoveError::AlreadyPublished`]
/// identifies a destination won by another mover. That outcome never removes
/// the source and carries any staging-cleanup warning for the caller to report.
/// Directories and linkless files on filesystems without native no-replace
/// support use the checked, non-atomic fallback described by [`rename`].
/// Once published, directory sync or source removal failures return
/// [`Moved::PublishedWithWarning`]: callers must report the warning and use `to`.
pub(crate) fn move_path(from: &Path, to: &Path) -> Result<Moved, MoveError> {
    let parent = to
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let error = match publish_no_replace(from, to) {
        Ok(warning) => {
            return Ok(finish_publication(warning, parent, Moved::Renamed));
        }
        Err(error) => error,
    };
    if error.kind() == io::ErrorKind::AlreadyExists {
        return Err(MoveError::AlreadyPublished {
            cleanup_warning: None,
        });
    }
    if !is_cross_device(&error) {
        return Err(error.into());
    }
    if fs::symlink_metadata(to).is_ok() {
        return Err(MoveError::AlreadyPublished {
            cleanup_warning: None,
        });
    }
    let staging = staging_path(parent, to)?;
    let mut publishing = false;
    let publication = copy_tree(from, &staging).and_then(|()| {
        #[cfg(test)]
        before_publish()?;
        publishing = true;
        publish_no_replace(&staging, to)
    });
    let warning = match publication {
        Ok(warning) => warning,
        Err(error) => {
            let cleanup_warning = cleanup_staging(&staging).err();
            if publishing && error.kind() == io::ErrorKind::AlreadyExists {
                return Err(MoveError::AlreadyPublished { cleanup_warning });
            }
            let cleanup = cleanup_warning
                .map(|warning| format!("; {warning}"))
                .unwrap_or_default();
            let message = format!(
                    "cannot move {} across volumes to {}: {error}; the source was left in place{cleanup}",
                    from.display(), to.display(),
                );
            return Err(io::Error::new(error.kind(), message).into());
        }
    };
    // Publication has committed the destination's authority. Keep the source
    // until that directory entry is synced, but never select it for new writes.
    if let Moved::PublishedWithWarning(error) = finish_publication(warning, parent, Moved::Copied) {
        return Ok(Moved::PublishedWithWarning(error));
    }
    if let Err(error) = remove_path(from) {
        return Ok(Moved::PublishedWithWarning(io::Error::new(
            error.kind(),
            format!("could not remove the source: {error}"),
        )));
    }
    Ok(Moved::Copied)
}

fn finish_publication(warning: Option<io::Error>, parent: &Path, success: Moved) -> Moved {
    let sync_error = crate::durability::sync_dir(parent).err();
    match (warning, sync_error) {
        (None, None) => success,
        (Some(error), None) | (None, Some(error)) => Moved::PublishedWithWarning(error),
        (Some(error), Some(sync)) => Moved::PublishedWithWarning(io::Error::new(
            error.kind(),
            format!("{error}; directory sync failed: {sync}"),
        )),
    }
}

fn cleanup_staging(path: &Path) -> io::Result<()> {
    let cleanup = || {
        #[cfg(windows)]
        clear_staging_readonly(path)?;
        #[cfg(test)]
        before_staging_removal(path);
        remove_path(path)
    };
    cleanup().map_err(|error| {
        io::Error::new(
            error.kind(),
            format!("could not remove staging copy {}: {error}", path.display()),
        )
    })
}

#[cfg(windows)]
fn clear_staging_readonly(path: &Path) -> io::Result<()> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    // Only unpublished copies are made writable. Never follow a link into
    // source data, or change permissions on a published destination.
    if metadata.is_symlink() {
        return Ok(());
    }
    if metadata.permissions().readonly() {
        let mut permissions = metadata.permissions();
        permissions.set_readonly(false);
        fs::set_permissions(path, permissions)?;
    }
    if metadata.is_dir() {
        for entry in fs::read_dir(path)? {
            clear_staging_readonly(&entry?.path())?;
        }
    }
    Ok(())
}

fn staging_path(parent: &Path, to: &Path) -> io::Result<PathBuf> {
    let mut nonce = [0_u8; 8];
    getrandom::fill(&mut nonce).map_err(io::Error::other)?;
    Ok(parent.join(format!(
        ".{}.aft-move-{}-{:016x}",
        to.file_name().unwrap_or_default().to_string_lossy(),
        std::process::id(),
        u64::from_le_bytes(nonce)
    )))
}

/// Copy `from` to the new path `to`, keeping permissions, and sync it.
/// Symlinks are recreated as links (never followed); any other special file
/// is refused rather than silently skipped.
fn copy_tree(from: &Path, to: &Path) -> io::Result<()> {
    let metadata = fs::symlink_metadata(from)?;
    let kind = metadata.file_type();
    if kind.is_symlink() {
        #[cfg(unix)]
        return std::os::unix::fs::symlink(fs::read_link(from)?, to);
        #[cfg(not(unix))]
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            format!("cannot copy symlink {} across volumes", from.display()),
        ));
    }
    if kind.is_file() {
        fs::copy(from, to)?;
        #[cfg(not(windows))]
        fs::File::open(to)?.sync_all()?;
        #[cfg(windows)]
        {
            // FlushFileBuffers requires a writable handle. fs::copy preserves
            // the readonly attribute, so clear it on the unpublished copy only
            // and restore it even if opening or syncing the file fails.
            let permissions = fs::metadata(to)?.permissions();
            if permissions.readonly() {
                let mut writable = permissions.clone();
                writable.set_readonly(false);
                fs::set_permissions(to, writable)?;
            }
            let synced = fs::OpenOptions::new()
                .write(true)
                .open(to)
                .and_then(|file| file.sync_all());
            if permissions.readonly() {
                fs::set_permissions(to, permissions)?;
            }
            synced?;
        }
        return Ok(());
    }
    if !kind.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            format!("cannot copy special file {} across volumes", from.display()),
        ));
    }
    fs::create_dir(to)?;
    for entry in fs::read_dir(from)? {
        let entry = entry?;
        copy_tree(&entry.path(), &to.join(entry.file_name()))?;
    }
    fs::set_permissions(to, metadata.permissions())?;
    crate::durability::sync_dir(to)
}

#[cfg(test)]
thread_local! {
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    static UNSUPPORTED_RENAME: std::cell::Cell<Option<i32>> = const { std::cell::Cell::new(None) };
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    static UNSUPPORTED_LINK: std::cell::Cell<Option<i32>> = const { std::cell::Cell::new(None) };
    pub(crate) static BEFORE_PUBLISH: std::cell::RefCell<Option<Box<dyn FnOnce() -> io::Result<()>>>> = const {
        std::cell::RefCell::new(None)
    };
    static BEFORE_STAGING_REMOVAL: std::cell::RefCell<Option<Box<dyn FnOnce(&Path)>>> = const {
        std::cell::RefCell::new(None)
    };
    static FAIL_SOURCE_REMOVAL: std::cell::RefCell<Option<PathBuf>> = const {
        std::cell::RefCell::new(None)
    };
}

#[cfg(test)]
fn before_publish() -> io::Result<()> {
    let hook = BEFORE_PUBLISH.with(|slot| slot.borrow_mut().take());
    hook.map_or(Ok(()), |hook| hook())
}

#[cfg(test)]
fn before_staging_removal(path: &Path) {
    let hook = BEFORE_STAGING_REMOVAL.with(|slot| slot.borrow_mut().take());
    if let Some(hook) = hook {
        hook(path);
    }
}

fn remove_path(path: &Path) -> io::Result<()> {
    #[cfg(test)]
    if FAIL_SOURCE_REMOVAL.with(|slot| {
        let mut slot = slot.borrow_mut();
        if slot.as_deref() == Some(path) {
            slot.take();
            true
        } else {
            false
        }
    }) {
        return Err(io::Error::other("injected source removal failure"));
    }
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_dir() => fs::remove_dir_all(path),
        Ok(_) => fs::remove_file(path),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::walk_boundary::simulated_volumes::simulate;

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn unsupported_flag_file_fallback_moves_without_replacing_an_existing_destination() {
        unsupported_flag_fallback(false, None);
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn unsupported_flag_directory_fallback_moves_without_replacing_an_existing_destination() {
        unsupported_flag_fallback(true, None);
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn linkless_file_fallback_moves_without_replacing_an_existing_destination() {
        #[cfg(target_os = "linux")]
        let errors = [libc::ENOTSUP, libc::EOPNOTSUPP];
        #[cfg(target_os = "macos")]
        let errors = [libc::ENOTSUP, libc::EOPNOTSUPP, libc::EPERM];
        for errno in errors {
            unsupported_flag_fallback(false, Some(errno));
        }
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn link_limit_and_permission_errors_do_not_use_checked_rename() {
        #[cfg(target_os = "linux")]
        let errors = [libc::EMLINK, libc::EACCES, libc::EPERM];
        #[cfg(target_os = "macos")]
        let errors = [libc::EMLINK, libc::EACCES];
        for errno in errors {
            let temp = tempfile::tempdir().unwrap();
            let source = temp.path().join("source");
            let destination = temp.path().join("destination");
            fs::write(&source, b"old").unwrap();
            UNSUPPORTED_RENAME.with(|slot| slot.set(Some(libc::ENOTSUP)));
            UNSUPPORTED_LINK.with(|slot| slot.set(Some(errno)));
            let result = move_path(&source, &destination);
            assert!(UNSUPPORTED_RENAME.with(|slot| slot.take()).is_none());
            assert!(UNSUPPORTED_LINK.with(|slot| slot.take()).is_none());
            assert!(
                matches!(result, Err(MoveError::Failed(ref error)) if error.raw_os_error() == Some(errno)),
                "{result:?}"
            );
            assert_eq!(fs::read(&source).unwrap(), b"old");
            assert!(!destination.exists());
        }
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    fn unsupported_flag_fallback(directory: bool, link_error: Option<i32>) {
        #[cfg(target_os = "linux")]
        let errors = [libc::EINVAL, libc::ENOSYS, libc::ENOTSUP];
        #[cfg(target_os = "macos")]
        let errors = [libc::ENOTSUP];
        for errno in errors {
            for cross_volume in [false, true] {
                let temp = tempfile::tempdir().unwrap();
                let source = temp.path().join("source");
                let other = temp.path().join("destination-volume");
                fs::create_dir(&other).unwrap();
                let _volume = cross_volume.then(|| simulate(&other, u64::MAX - 14));
                let destination = other.join("destination");
                let create_source = || {
                    if directory {
                        fs::create_dir(&source).unwrap();
                        fs::write(source.join("state.json"), b"old").unwrap();
                    } else {
                        fs::write(&source, b"old").unwrap();
                    }
                };
                create_source();
                UNSUPPORTED_RENAME.with(|slot| slot.set(Some(errno)));
                UNSUPPORTED_LINK.with(|slot| slot.set(link_error));
                let result = move_path(&source, &destination);
                assert!(
                    UNSUPPORTED_RENAME.with(|slot| slot.take()).is_none(),
                    "the unsupported syscall seam must execute"
                );
                assert!(
                    UNSUPPORTED_LINK.with(|slot| slot.take()).is_none(),
                    "the injected link error must execute"
                );
                assert!(result.is_ok(), "fallback must move the source: {result:?}");
                assert!(!source.exists());
                let saved = if directory {
                    destination.join("state.json")
                } else {
                    destination.clone()
                };
                assert_eq!(fs::read(&saved).unwrap(), b"old");

                if cross_volume {
                    continue;
                }
                create_source();
                fs::write(&saved, b"new").unwrap();
                if directory {
                    // Empty destinations are replaceable by POSIX rename, so
                    // the fallback's existence check must protect them too.
                    fs::remove_file(&saved).unwrap();
                }
                UNSUPPORTED_RENAME.with(|slot| slot.set(Some(errno)));
                UNSUPPORTED_LINK.with(|slot| slot.set(link_error));
                let result = move_path(&source, &destination);
                assert!(
                    UNSUPPORTED_RENAME.with(|slot| slot.take()).is_none(),
                    "the collision must reach the unsupported syscall seam"
                );
                assert!(
                    UNSUPPORTED_LINK.with(|slot| slot.take()).is_none(),
                    "the collision must reach the injected link error"
                );
                assert!(
                    matches!(
                        result,
                        Err(MoveError::AlreadyPublished {
                            cleanup_warning: None
                        })
                    ),
                    "{result:?}"
                );
                assert!(source.exists(), "collision must keep the source");
                if directory {
                    assert_eq!(fs::read_dir(&destination).unwrap().count(), 0);
                    assert_eq!(fs::read(source.join("state.json")).unwrap(), b"old");
                } else {
                    assert_eq!(fs::read(&saved).unwrap(), b"new");
                    assert_eq!(fs::read(&source).unwrap(), b"old");
                }
            }
        }
    }

    #[test]
    fn cross_volume_racing_file_publish_keeps_the_winners_new_value() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("state.json");
        fs::write(&source, b"old").unwrap();
        let other = temp.path().join("other-volume");
        fs::create_dir(&other).unwrap();
        let _volume = simulate(&other, u64::MAX - 10);
        let destination = other.join("state.json");
        let source_clone = source.clone();
        let destination_clone = destination.clone();
        BEFORE_PUBLISH.with(|slot| {
            *slot.borrow_mut() = Some(Box::new(move || {
                // The outer copier has staged the old bytes. A second process
                // stages the same bytes, publishes first, and then saves state.
                assert!(matches!(
                    move_path(&source_clone, &destination_clone).unwrap(),
                    Moved::Copied
                ));
                fs::write(destination_clone, b"new")
            }));
        });

        let result = move_path(&source, &destination);
        assert_eq!(fs::read(&destination).unwrap(), b"new");
        assert!(matches!(
            result,
            Err(MoveError::AlreadyPublished {
                cleanup_warning: None
            })
        ));
        assert_eq!(fs::read_dir(other).unwrap().count(), 1);
    }

    #[test]
    fn cross_volume_racing_directory_publish_preserves_the_winner() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source");
        fs::create_dir(&source).unwrap();
        let other = temp.path().join("other-volume");
        fs::create_dir(&other).unwrap();
        let _volume = simulate(&other, u64::MAX - 11);
        let destination = other.join("directory");
        let source_clone = source.clone();
        let destination_clone = destination.clone();
        BEFORE_PUBLISH.with(|slot| {
            *slot.borrow_mut() = Some(Box::new(move || {
                assert!(matches!(
                    move_path(&source_clone, &destination_clone).unwrap(),
                    Moved::Copied
                ));
                // Leave the winner empty: POSIX rename can replace an empty
                // directory too, even though it refuses a nonempty directory.
                Ok(())
            }));
        });

        let result = move_path(&source, &destination);
        assert!(
            matches!(
                result,
                Err(MoveError::AlreadyPublished {
                    cleanup_warning: None
                })
            ),
            "the losing directory publisher must refuse: {result:?}"
        );
        assert!(destination.is_dir());
        assert_eq!(fs::read_dir(other).unwrap().count(), 1);
    }

    #[test]
    fn same_volume_move_never_replaces_a_file_or_empty_directory() {
        for directory in [false, true] {
            let temp = tempfile::tempdir().unwrap();
            let source = temp.path().join("source");
            let destination = temp.path().join("destination");
            if directory {
                fs::create_dir(&source).unwrap();
                fs::create_dir(&destination).unwrap();
            } else {
                fs::write(&source, b"old").unwrap();
                fs::write(&destination, b"new").unwrap();
            }
            assert!(matches!(
                move_path(&source, &destination),
                Err(MoveError::AlreadyPublished {
                    cleanup_warning: None
                })
            ));
            assert!(source.exists());
            if !directory {
                assert_eq!(fs::read(&destination).unwrap(), b"new");
                assert_eq!(fs::read(&source).unwrap(), b"old");
            }
        }
    }

    #[test]
    fn failed_move_carries_staging_cleanup_failure_with_its_path() {
        for collision in [false, true] {
            let temp = tempfile::tempdir().unwrap();
            let source = temp.path().join("source");
            fs::write(&source, b"old").unwrap();
            let other = temp.path().join("other-volume");
            fs::create_dir(&other).unwrap();
            let _volume = simulate(&other, u64::MAX - 13);
            let destination = other.join("destination");
            let destination_clone = destination.clone();
            BEFORE_PUBLISH.with(|slot| {
                *slot.borrow_mut() = Some(Box::new(move || {
                    if collision {
                        fs::write(destination_clone, b"new")
                    } else {
                        Err(io::Error::other("injected publication failure"))
                    }
                }));
            });
            BEFORE_STAGING_REMOVAL.with(|slot| {
                *slot.borrow_mut() = Some(Box::new(|staging| {
                    FAIL_SOURCE_REMOVAL
                        .with(|slot| *slot.borrow_mut() = Some(staging.to_path_buf()));
                }));
            });
            let error = move_path(&source, &destination).unwrap_err();
            let message = error.to_string();
            assert!(
                message.contains("could not remove staging copy"),
                "{message}"
            );
            assert!(message.contains(".destination.aft-move-"), "{message}");
            assert!(
                message.contains("injected source removal failure"),
                "{message}"
            );
            assert_eq!(
                matches!(
                    error,
                    MoveError::AlreadyPublished {
                        cleanup_warning: Some(_)
                    }
                ),
                collision
            );
            assert_eq!(fs::read(&source).unwrap(), b"old");
            if collision {
                assert_eq!(fs::read(&destination).unwrap(), b"new");
            }
        }
    }

    #[cfg(windows)]
    #[test]
    fn cross_volume_failed_publish_cleans_readonly_staging() {
        for directory in [false, true] {
            let temp = tempfile::tempdir().unwrap();
            let source = temp.path().join("source");
            let file = if directory {
                fs::create_dir_all(source.join("nested")).unwrap();
                source.join("nested/state.json")
            } else {
                source.clone()
            };
            fs::write(&file, b"old").unwrap();
            let mut permissions = fs::metadata(&file).unwrap().permissions();
            permissions.set_readonly(true);
            fs::set_permissions(&file, permissions).unwrap();
            let other = temp.path().join("other-volume");
            fs::create_dir(&other).unwrap();
            let _volume = simulate(&other, u64::MAX - 12);
            BEFORE_PUBLISH.with(|slot| {
                *slot.borrow_mut() = Some(Box::new(|| {
                    Err(io::Error::other("injected publication failure"))
                }));
            });
            BEFORE_STAGING_REMOVAL.with(|slot| {
                *slot.borrow_mut() = Some(Box::new(|staging| {
                    fn assert_writable(path: &Path) {
                        let metadata = fs::symlink_metadata(path).unwrap();
                        assert!(
                            !metadata.permissions().readonly(),
                            "staging must be writable before unlink: {}",
                            path.display()
                        );
                        if metadata.is_dir() {
                            for entry in fs::read_dir(path).unwrap() {
                                assert_writable(&entry.unwrap().path());
                            }
                        }
                    }
                    // Newer Windows versions can ignore readonly during unlink.
                    // Check the actual attributes too, so older hosts stay safe.
                    assert_writable(staging);
                }));
            });

            let error = move_path(&source, &other.join("destination")).unwrap_err();
            let leftovers = fs::read_dir(&other).unwrap().count();
            let source_readonly = fs::metadata(&file).unwrap().permissions().readonly();
            let mut permissions = fs::metadata(&file).unwrap().permissions();
            permissions.set_readonly(false);
            fs::set_permissions(&file, permissions).unwrap();
            assert!(error.to_string().contains("injected publication failure"));
            assert_eq!(leftovers, 0, "readonly staging must be removed");
            assert!(source_readonly, "cleanup must not change the source");
        }
    }

    #[test]
    fn cross_volume_move_copies_publishes_and_removes_the_source() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source");
        fs::create_dir_all(source.join("nested")).unwrap();
        fs::write(source.join("a.json"), b"first").unwrap();
        fs::write(source.join("nested/b.bin"), b"second").unwrap();
        let other = temp.path().join("other-volume");
        fs::create_dir(&other).unwrap();
        let _volume = simulate(&other, u64::MAX - 1);
        let destination = other.join("moved");

        assert!(
            is_cross_device(&rename(&source, &destination).unwrap_err()),
            "the seam must reproduce the kernel's cross-volume refusal"
        );
        assert!(matches!(
            move_path(&source, &destination).unwrap(),
            Moved::Copied
        ));

        assert!(!source.exists(), "the source is removed after the copy");
        assert_eq!(fs::read(destination.join("a.json")).unwrap(), b"first");
        assert_eq!(
            fs::read(destination.join("nested/b.bin")).unwrap(),
            b"second"
        );
        let leftovers = fs::read_dir(&other)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect::<Vec<_>>();
        assert_eq!(leftovers, vec![std::ffi::OsString::from("moved")]);
    }

    #[test]
    fn same_volume_move_is_one_rename() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("file");
        fs::write(&source, b"x").unwrap();
        let destination = temp.path().join("renamed");
        assert!(matches!(
            move_path(&source, &destination).unwrap(),
            Moved::Renamed
        ));
        assert_eq!(fs::read(destination).unwrap(), b"x");
    }

    #[test]
    fn copied_readonly_file_is_synced_and_keeps_its_permissions() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("readonly.json");
        fs::write(&source, b"keep").unwrap();
        let original_permissions = fs::metadata(&source).unwrap().permissions();
        let mut readonly = original_permissions.clone();
        readonly.set_readonly(true);
        fs::set_permissions(&source, readonly).unwrap();
        let destination = temp.path().join("staging.json");

        let copied = copy_tree(&source, &destination);
        let source_readonly = fs::metadata(&source).unwrap().permissions().readonly();
        let destination_readonly = fs::metadata(&destination).unwrap().permissions().readonly();
        // Restore writable permissions before asserting so Windows can clean
        // up the fixture even when the copy or permission assertion fails.
        fs::set_permissions(&source, original_permissions.clone()).unwrap();
        fs::set_permissions(&destination, original_permissions).unwrap();

        copied.unwrap();
        assert!(source_readonly, "the source permissions must not change");
        assert!(destination_readonly, "the copy must keep its permissions");
        assert_eq!(fs::read(&destination).unwrap(), b"keep");
    }

    #[test]
    fn cross_volume_move_refuses_an_existing_destination_and_keeps_the_source() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("state.json");
        fs::write(&source, b"keep").unwrap();
        let other = temp.path().join("other-volume");
        fs::create_dir(&other).unwrap();
        let _volume = simulate(&other, u64::MAX - 2);
        let destination = other.join("state.json");
        fs::write(&destination, b"existing").unwrap();

        let error = move_path(&source, &destination).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(fs::read(&source).unwrap(), b"keep");
        assert_eq!(fs::read(&destination).unwrap(), b"existing");
    }

    #[test]
    fn cross_volume_published_copy_reports_directory_sync_failure() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("state.json");
        fs::write(&source, b"old").unwrap();
        let other = temp.path().join("other-volume");
        fs::create_dir(&other).unwrap();
        let _volume = simulate(&other, u64::MAX - 5);
        let destination = other.join("state.json");
        crate::durability::FAIL_DIRECTORY_SYNC.with(|slot| {
            *slot.borrow_mut() = Some(other.clone());
        });

        let Moved::PublishedWithWarning(error) = move_path(&source, &destination).unwrap() else {
            panic!("the destination is published but its directory sync failed");
        };
        assert_eq!(error.to_string(), "injected directory sync failure");
        assert_eq!(fs::read(&destination).unwrap(), b"old");
        assert_eq!(fs::read(&source).unwrap(), b"old");
    }

    #[test]
    fn cross_volume_published_copy_reports_source_removal_failure() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("state.json");
        fs::write(&source, b"old").unwrap();
        let other = temp.path().join("other-volume");
        fs::create_dir(&other).unwrap();
        let _volume = simulate(&other, u64::MAX - 6);
        let destination = other.join("state.json");
        FAIL_SOURCE_REMOVAL.with(|slot| {
            *slot.borrow_mut() = Some(source.clone());
        });

        let Moved::PublishedWithWarning(error) = move_path(&source, &destination).unwrap() else {
            panic!("the destination is published but source cleanup failed");
        };
        assert_eq!(
            error.to_string(),
            "could not remove the source: injected source removal failure"
        );
        assert_eq!(fs::read(&destination).unwrap(), b"old");
        assert_eq!(fs::read(&source).unwrap(), b"old");
    }

    #[test]
    fn same_volume_published_rename_reports_directory_sync_failure() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("state.json");
        fs::write(&source, b"old").unwrap();
        let destination = temp.path().join("renamed.json");
        crate::durability::FAIL_DIRECTORY_SYNC.with(|slot| {
            *slot.borrow_mut() = Some(temp.path().to_path_buf());
        });

        let Moved::PublishedWithWarning(error) = move_path(&source, &destination).unwrap() else {
            panic!("the destination is published but its directory sync failed");
        };
        assert_eq!(error.to_string(), "injected directory sync failure");
        assert_eq!(fs::read(&destination).unwrap(), b"old");
        assert!(!source.exists());
    }

    #[test]
    fn a_folder_on_another_device_than_its_parent_is_a_mount_point() {
        let temp = tempfile::tempdir().unwrap();
        let mounted = temp.path().join("views");
        fs::create_dir(&mounted).unwrap();
        let plain = temp.path().join("plain");
        fs::create_dir(&plain).unwrap();
        // Register the parent too so the comparison also works where the
        // platform reports no real device number.
        let _parent = simulate(temp.path(), u64::MAX - 3);
        let _volume = simulate(&mounted, u64::MAX - 4);
        assert!(is_mount_point(&mounted).unwrap());
        assert!(!is_mount_point(&plain).unwrap());
    }
}
