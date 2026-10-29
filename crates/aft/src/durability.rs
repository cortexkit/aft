//! Durable file commits use the platform's full sync, including F_FULLFSYNC on
//! Apple targets. Filesystems that reject full sync fall back to fsync with a warning.
//! Rebuildable and process-lifetime stores deliberately do not call these
//! sync helpers. Atomic writes select their durability explicitly. The test
//! ledger observes the real calls, never substitutes for them.

use std::fs::{self, File};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

#[cfg(test)]
mod source_guard;

// Some binaries do not use every atomic-write durability option. Callers choose
// whether private state needs no sync, file sync, or file plus directory sync.
#[derive(Clone, Copy)]
pub(crate) enum AtomicWriteDurability {
    None,
    // Callers publishing several files sync the directory once after the batch.
    File,
    FileAndDirectory,
}

/// Publish private state without leaving a partial destination or a failed temp.
/// Authorization and directory creation remain the caller's responsibility.
pub(crate) fn atomic_write(
    path: &Path,
    bytes: &[u8],
    durability: AtomicWriteDurability,
) -> io::Result<()> {
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let mut random = [0_u8; 16];
    getrandom::fill(&mut random).map_err(io::Error::other)?;
    let temporary = parent.join(format!(".aft-{:032x}.tmp", u128::from_ne_bytes(random)));
    #[cfg(test)]
    before_temp_create(&temporary);
    let file = crate::private_storage::options()
        .write(true)
        .create_new(true)
        .open(&temporary)?;
    let mut cleanup = TempCleanup(Some(temporary.clone()));
    // Close before replacing so Windows does not retain an open temp handle.
    (move || -> io::Result<()> {
        let mut file = file;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if file.metadata()?.permissions().mode() & 0o777 != crate::private_storage::FILE_MODE {
                file.set_permissions(fs::Permissions::from_mode(
                    crate::private_storage::FILE_MODE,
                ))?;
            }
        }
        file.write_all(bytes)?;
        if !matches!(durability, AtomicWriteDurability::None) {
            sync_file(&file, path)?;
        }
        Ok(())
    })()?;
    crate::fs_lock::rename_over(&temporary, path)?;
    cleanup.0 = None;
    record(EventKind::AtomicReplace, path);
    if matches!(durability, AtomicWriteDurability::FileAndDirectory) {
        sync_dir(parent)?;
    }
    Ok(())
}

struct TempCleanup(Option<PathBuf>);

impl Drop for TempCleanup {
    fn drop(&mut self) {
        if let Some(path) = &self.0 {
            let _ = fs::remove_file(path);
        }
    }
}

#[cfg(test)]
type BeforeTempCreate = Box<dyn FnOnce(&Path)>;
#[cfg(test)]
thread_local! {
    pub(crate) static BEFORE_TEMP_CREATE: std::cell::RefCell<Option<BeforeTempCreate>> = const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
pub(crate) fn before_temp_create(path: &Path) {
    let hook = BEFORE_TEMP_CREATE.with(|slot| slot.borrow_mut().take());
    if let Some(hook) = hook {
        hook(path);
    }
}

pub(crate) fn sync_file(file: &File, path: &Path) -> io::Result<()> {
    sync_handle(file, path, false)?;
    record(EventKind::FileSync, path);
    Ok(())
}

/// Keep data-only sync semantics on platforms where they differ from full sync.
pub(crate) fn sync_data(file: &File, path: &Path) -> io::Result<()> {
    sync_handle(file, path, true)?;
    record(EventKind::FileSync, path);
    Ok(())
}

/// Sync the already-pinned directory, without reopening a replaceable path.
pub(crate) fn sync_dir_handle(file: &File, path: &Path) -> io::Result<()> {
    sync_handle(file, path, false)?;
    record(EventKind::DirectorySync, path);
    Ok(())
}

pub(crate) fn sync_dir(path: &Path) -> io::Result<()> {
    #[cfg(test)]
    if FAIL_DIRECTORY_SYNC.with(|slot| {
        let mut slot = slot.borrow_mut();
        if slot.as_deref() == Some(path) {
            slot.take();
            true
        } else {
            false
        }
    }) {
        return Err(FAIL_DIRECTORY_SYNC_ERRNO.with(|errno| {
            errno.take().map_or_else(
                || io::Error::other("injected directory sync failure"),
                io::Error::from_raw_os_error,
            )
        }));
    }
    #[cfg(unix)]
    {
        sync_dir_handle(&File::open(path)?, path)?;
    }
    // Windows cannot open a directory as a regular File. Its rename uses
    // MoveFileExW write-through semantics; data files are synced before rename.
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

fn sync_handle(file: &File, path: &Path, data_only: bool) -> io::Result<()> {
    #[cfg(any(target_vendor = "apple", all(test, unix)))]
    {
        full_sync::sync(file, path, data_only)
    }
    #[cfg(not(any(target_vendor = "apple", all(test, unix))))]
    {
        let _ = path;
        if data_only {
            file.sync_data()
        } else {
            file.sync_all()
        }
    }
}

// Unix tests exercise the same fallback through atomic_write, even off Apple.
#[cfg(any(target_vendor = "apple", all(test, unix)))]
mod full_sync {
    use super::*;
    use std::os::fd::AsRawFd;

    pub(super) fn sync(file: &File, path: &Path, data_only: bool) -> io::Result<()> {
        match full_sync(file, data_only) {
            Err(error)
                if matches!(
                    error.raw_os_error(),
                    Some(libc::ENOTTY | libc::ENOTSUP | libc::EINVAL)
                ) =>
            {
                #[cfg(target_vendor = "apple")]
                warn_once(file, path, &error);
                #[cfg(not(target_vendor = "apple"))]
                let _ = path;
                #[cfg(test)]
                FALLBACK_CALLS.with(|calls| calls.set(calls.get() + 1));
                // The descriptor is borrowed from a live File for this call.
                result(unsafe { libc::fsync(file.as_raw_fd()) })
            }
            result => result,
        }
    }

    fn full_sync(file: &File, data_only: bool) -> io::Result<()> {
        #[cfg(test)]
        if let Some(errno) = FORCED_ERROR.with(|error| error.get()) {
            return Err(io::Error::from_raw_os_error(errno));
        }
        #[cfg(target_vendor = "apple")]
        {
            // Apple's data-only and full sync both request F_FULLFSYNC.
            let _ = data_only;
            // F_FULLFSYNC also flushes device caches where the filesystem supports it.
            // The descriptor is borrowed from a live File; this command takes no argument.
            result(unsafe { libc::fcntl(file.as_raw_fd(), libc::F_FULLFSYNC) })
        }
        #[cfg(not(target_vendor = "apple"))]
        if data_only {
            file.sync_data()
        } else {
            file.sync_all()
        }
    }

    fn result(code: libc::c_int) -> io::Result<()> {
        if code == -1 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    }

    #[cfg(target_vendor = "apple")]
    fn warn_once(file: &File, path: &Path, error: &io::Error) {
        use std::sync::atomic::{AtomicBool, Ordering};
        static WARNED: AtomicBool = AtomicBool::new(false);
        if !WARNED.swap(true, Ordering::Relaxed) {
            let mut stat = std::mem::MaybeUninit::<libc::statfs>::uninit();
            // fstatfs initializes stat on success; the open descriptor identifies the
            // actual filesystem even when path names a not-yet-published destination.
            let filesystem = if unsafe { libc::fstatfs(file.as_raw_fd(), stat.as_mut_ptr()) } == 0 {
                let stat = unsafe { stat.assume_init() };
                // Darwin's f_fstypename is a NUL-terminated filesystem type name.
                unsafe { std::ffi::CStr::from_ptr(stat.f_fstypename.as_ptr()) }
                    .to_string_lossy()
                    .into_owned()
            } else {
                format!("unknown ({})", io::Error::last_os_error())
            };
            // Syncs also run in the panic hook and terminal logger. Bypass
            // env_logger and do not hold a once-lock that a panic could reenter.
            crate::logging::write_stderr_diagnostic(&format!(
                "[aft] WARN F_FULLFSYNC unsupported on filesystem {filesystem} for {} ({error}); falling back to fsync (device-cache flush unavailable)\n",
                path.display()
            ));
        }
    }

    #[cfg(test)]
    thread_local! {
        pub(super) static FORCED_ERROR: std::cell::Cell<Option<i32>> = const { std::cell::Cell::new(None) };
        pub(super) static FALLBACK_CALLS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum EventKind {
    FileSync,
    #[cfg_attr(not(unix), allow(dead_code))]
    DirectorySync,
    DirectoryCreated,
    AtomicReplace,
    CorruptCheckpointSkipped,
}

#[cfg(test)]
thread_local! {
    pub(crate) static FAIL_DIRECTORY_SYNC: std::cell::RefCell<Option<PathBuf>> = const {
        std::cell::RefCell::new(None)
    };
    // Optional errno for FAIL_DIRECTORY_SYNC; leaving it unset injects the
    // existing generic I/O error, so path-only callers need no change.
    pub(crate) static FAIL_DIRECTORY_SYNC_ERRNO: std::cell::Cell<Option<i32>> = const {
        std::cell::Cell::new(None)
    };
    static EVENTS: std::cell::RefCell<Vec<(EventKind, std::path::PathBuf)>> = const {
        std::cell::RefCell::new(Vec::new())
    };
}

#[inline]
pub(crate) fn record(kind: EventKind, path: &Path) {
    #[cfg(test)]
    EVENTS.with(|events| events.borrow_mut().push((kind, path.to_path_buf())));
    #[cfg(not(test))]
    let _ = (kind, path);
}

#[cfg(test)]
pub(crate) fn take() -> Vec<(EventKind, std::path::PathBuf)> {
    EVENTS.with(|events| std::mem::take(&mut *events.borrow_mut()))
}

#[cfg(test)]
pub(crate) fn sync_count(events: &[(EventKind, std::path::PathBuf)]) -> usize {
    events
        .iter()
        .filter(|(kind, _)| matches!(kind, EventKind::FileSync | EventKind::DirectorySync))
        .count()
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    struct FullSyncError;

    impl FullSyncError {
        fn inject(errno: i32) -> Self {
            full_sync::FORCED_ERROR.with(|error| error.set(Some(errno)));
            full_sync::FALLBACK_CALLS.with(|calls| calls.set(0));
            take();
            Self
        }
    }

    impl Drop for FullSyncError {
        fn drop(&mut self) {
            full_sync::FORCED_ERROR.with(|error| error.set(None));
        }
    }

    #[test]
    fn atomic_write_falls_back_when_full_sync_is_unsupported() {
        for errno in [libc::ENOTTY, libc::ENOTSUP, libc::EINVAL] {
            let directory = tempfile::tempdir().unwrap();
            let path = directory.path().join("state");
            fs::write(&path, b"old").unwrap();
            let _injection = FullSyncError::inject(errno);

            atomic_write(&path, b"committed", AtomicWriteDurability::FileAndDirectory).unwrap();

            assert_eq!(fs::read(&path).unwrap(), b"committed");
            // Both the temporary file and its parent must really reach fsync.
            assert_eq!(full_sync::FALLBACK_CALLS.with(|calls| calls.get()), 2);
            assert_eq!(
                take(),
                vec![
                    (EventKind::FileSync, path.clone()),
                    (EventKind::AtomicReplace, path),
                    (EventKind::DirectorySync, directory.path().to_path_buf()),
                ]
            );
            assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 1);
        }
    }

    #[test]
    fn atomic_write_does_not_fall_back_on_full_sync_io_error() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("state");
        fs::write(&path, b"old").unwrap();
        let _injection = FullSyncError::inject(libc::EIO);

        let error =
            atomic_write(&path, b"new", AtomicWriteDurability::FileAndDirectory).unwrap_err();

        assert_eq!(error.raw_os_error(), Some(libc::EIO));
        assert_eq!(full_sync::FALLBACK_CALLS.with(|calls| calls.get()), 0);
        assert_eq!(fs::read(&path).unwrap(), b"old");
        assert!(take().is_empty());
        assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 1);
    }

    #[test]
    fn directory_sync_does_not_fall_back_on_full_sync_io_error() {
        let directory = tempfile::tempdir().unwrap();
        let _injection = FullSyncError::inject(libc::EIO);

        let error = sync_dir(directory.path()).unwrap_err();

        assert_eq!(error.raw_os_error(), Some(libc::EIO));
        assert_eq!(full_sync::FALLBACK_CALLS.with(|calls| calls.get()), 0);
        assert!(take().is_empty());
    }

    #[test]
    fn data_sync_falls_back_when_full_sync_is_unsupported() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("state");
        let mut file = File::create(&path).unwrap();
        file.write_all(b"committed").unwrap();
        let _injection = FullSyncError::inject(libc::ENOTTY);

        sync_data(&file, &path).unwrap();

        assert_eq!(full_sync::FALLBACK_CALLS.with(|calls| calls.get()), 1);
        assert_eq!(fs::read(&path).unwrap(), b"committed");
        assert_eq!(take(), vec![(EventKind::FileSync, path)]);
    }

    #[test]
    fn data_sync_does_not_fall_back_on_full_sync_io_error() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("state");
        let file = File::create(&path).unwrap();
        let _injection = FullSyncError::inject(libc::EIO);

        assert_eq!(
            sync_data(&file, &path).unwrap_err().raw_os_error(),
            Some(libc::EIO)
        );
        assert_eq!(full_sync::FALLBACK_CALLS.with(|calls| calls.get()), 0);
        assert!(take().is_empty());
    }

    #[test]
    fn directory_handle_sync_does_not_reopen_the_path() {
        let parent = tempfile::tempdir().unwrap();
        let path = parent.path().join("original");
        fs::create_dir(&path).unwrap();
        let directory = File::open(&path).unwrap();
        fs::rename(&path, parent.path().join("renamed")).unwrap();
        let _injection = FullSyncError::inject(libc::ENOTTY);

        sync_dir_handle(&directory, &path).unwrap();

        assert!(!path.exists());
        assert_eq!(full_sync::FALLBACK_CALLS.with(|calls| calls.get()), 1);
        assert_eq!(take(), vec![(EventKind::DirectorySync, path)]);
    }
}
