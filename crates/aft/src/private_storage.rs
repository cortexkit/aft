//! Private store creation, independent of the process umask. These helpers are
//! for AFT-owned state, never for edits or restorations in the user's project.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::Path;

pub(crate) const DIR_MODE: u32 = 0o700;
pub(crate) const FILE_MODE: u32 = 0o600;

pub(crate) fn create_dir_all(path: impl AsRef<Path>) -> io::Result<()> {
    let mut builder = fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(DIR_MODE);
    }
    builder.create(path)
}

pub(crate) fn create_dir(path: &Path) -> io::Result<()> {
    let mut builder = fs::DirBuilder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(DIR_MODE);
    }
    builder.create(path)
}

pub(crate) fn options() -> OpenOptions {
    let mut options = OpenOptions::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(FILE_MODE);
    }
    options
}

pub(crate) fn create(path: impl AsRef<Path>) -> io::Result<File> {
    options().write(true).create(true).truncate(true).open(path)
}

pub(crate) fn write(path: impl AsRef<Path>, contents: impl AsRef<[u8]>) -> io::Result<()> {
    create(path)?.write_all(contents.as_ref())
}

/// std::fs::copy copies source permissions too, which would expose snapshots of
/// public files. Stream into an owner-only destination instead on Unix.
pub(crate) fn copy(from: impl AsRef<Path>, to: impl AsRef<Path>) -> io::Result<u64> {
    #[cfg(unix)]
    {
        io::copy(&mut File::open(from)?, &mut create(to)?)
    }
    #[cfg(not(unix))]
    {
        fs::copy(from, to)
    }
}

/// Open a storage root and protect its immediate domains, without traversing
/// histories or indexes. Existing files need no walk behind a private directory.
pub(crate) fn open_root(root: &Path) -> io::Result<()> {
    create_dir_all(root)?;
    tighten_root(root);
    Ok(())
}

pub(crate) fn tighten_root(root: &Path) {
    tighten_within(root, root);
    #[cfg(unix)]
    if let Ok(entries) = fs::read_dir(root) {
        for entry in entries.flatten() {
            if entry.file_type().is_ok_and(|kind| kind.is_dir()) {
                tighten_within(root, &entry.path());
            }
        }
    }
}

/// Protect only the ancestors of the domain/per-root directory being opened.
/// No chmod is attempted above `root`, or through a symlink inside the store.
pub(crate) fn open_dir(root: &Path, path: &Path) -> io::Result<()> {
    create_dir_all(path)?;
    tighten_open_dir(root, path);
    Ok(())
}

pub(crate) fn tighten_open_dir(root: &Path, path: &Path) {
    tighten_within(root, root);
    if let Ok(relative) = path.strip_prefix(root) {
        let mut current = root.to_path_buf();
        for component in relative.components() {
            if !matches!(component, std::path::Component::Normal(_)) {
                break;
            }
            current.push(component);
            tighten_within(root, &current);
        }
    }
}

/// Keyed caches normally have `<root>/<domain>/<key>` layout. Explicit cache
/// paths used by embedding callers are their own boundary, not a reason to
/// chmod two arbitrary parents outside AFT's storage.
pub(crate) fn open_keyed_dir(path: &Path, domain: &str) -> io::Result<()> {
    let root = path
        .parent()
        .filter(|parent| parent.file_name().is_some_and(|name| name == domain))
        .and_then(Path::parent)
        .unwrap_or(path);
    open_dir(root, path)
}

#[cfg(unix)]
fn tighten_within(root: &Path, path: &Path) {
    use std::os::fd::{AsRawFd, FromRawFd};
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    let result = (|| -> io::Result<()> {
        let relative = path.strip_prefix(root).map_err(io::Error::other)?;
        // Open each component relative to its pinned parent. O_NOFOLLOW on just
        // the final path would still chmod a target through a symlinked ancestor.
        let mut directory = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(root)?;
        for component in relative.components() {
            let std::path::Component::Normal(name) = component else {
                return Ok(());
            };
            let name = std::ffi::CString::new(name.as_bytes()).map_err(io::Error::other)?;
            let fd = unsafe {
                libc::openat(
                    directory.as_raw_fd(),
                    name.as_ptr(),
                    libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                )
            };
            if fd < 0 {
                return Err(io::Error::last_os_error());
            }
            directory = unsafe { File::from_raw_fd(fd) };
        }
        if directory.metadata()?.permissions().mode() & 0o077 != 0 {
            directory.set_permissions(fs::Permissions::from_mode(DIR_MODE))?;
        }
        Ok(())
    })();
    if let Err(error) = result {
        // A symlink is intentionally left alone, not a failed permission repair.
        if matches!(error.raw_os_error(), Some(libc::ELOOP | libc::ENOTDIR)) {
            return;
        }
        warn_once(path, &error);
    }
}

#[cfg(not(unix))]
fn tighten_within(_root: &Path, _path: &Path) {}

#[cfg(unix)]
fn warn_once(path: &Path, error: &io::Error) {
    use std::collections::HashSet;
    use std::sync::{Mutex, OnceLock};
    static WARNED: OnceLock<Mutex<HashSet<std::path::PathBuf>>> = OnceLock::new();
    if WARNED
        .get_or_init(Mutex::default)
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(path.to_path_buf())
    {
        log::warn!(
            "could not tighten private storage {}: {error}; continuing",
            path.display()
        );
    }
}

/// SQLite derives WAL/SHM modes from the database. Reserve the database before
/// SQLite's first writable open, but never create a file for a read-only open.
pub(crate) fn prepare_sqlite(path: &Path, flags: rusqlite::OpenFlags) -> rusqlite::Result<()> {
    #[cfg(unix)]
    if flags.contains(rusqlite::OpenFlags::SQLITE_OPEN_CREATE)
        && !flags.contains(rusqlite::OpenFlags::SQLITE_OPEN_MEMORY)
        && !path.as_os_str().is_empty()
        && path != Path::new(":memory:")
    {
        match options().write(true).create_new(true).open(path) {
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(rusqlite::Error::ToSqlConversionFailure(Box::new(error))),
        }
    }
    #[cfg(not(unix))]
    let _ = (path, flags);
    Ok(())
}
