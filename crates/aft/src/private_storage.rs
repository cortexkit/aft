//! Private store creation, independent of the process umask. These helpers are
//! for AFT-owned state, never for edits or restorations in the user's project.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::Path;

#[cfg(unix)]
pub(crate) const DIR_MODE: u32 = 0o700;
#[cfg(unix)]
pub(crate) const FILE_MODE: u32 = 0o600;

pub fn create_dir_all(path: impl AsRef<Path>) -> io::Result<()> {
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
    #[allow(unused_mut)]
    let mut builder = fs::DirBuilder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(DIR_MODE);
    }
    builder.create(path)
}

pub(crate) fn options() -> OpenOptions {
    #[allow(unused_mut)]
    let mut options = OpenOptions::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(FILE_MODE);
    }
    options
}

pub(crate) fn executable_options() -> OpenOptions {
    #[allow(unused_mut)]
    let mut options = options();
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(DIR_MODE);
    }
    options
}

pub(crate) fn write_executable(path: &Path, contents: &[u8]) -> io::Result<()> {
    executable_options()
        .write(true)
        .create(true)
        .truncate(true)
        .open(path)?
        .write_all(contents)
}

pub fn create(path: impl AsRef<Path>) -> io::Result<File> {
    options().write(true).create(true).truncate(true).open(path)
}

pub fn write(path: impl AsRef<Path>, contents: impl AsRef<[u8]>) -> io::Result<()> {
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

pub(crate) fn tighten_keyed_dir(path: &Path, domain: &str) {
    let root = path
        .parent()
        .filter(|parent| parent.file_name().is_some_and(|name| name == domain))
        .and_then(Path::parent)
        .unwrap_or(path);
    tighten_open_dir(root, path);
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
        if error.kind() == io::ErrorKind::NotFound {
            return;
        }
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
    if !flags.contains(rusqlite::OpenFlags::SQLITE_OPEN_MEMORY)
        && !path.as_os_str().is_empty()
        && path != Path::new(":memory:")
        && !path.to_string_lossy().starts_with("file:")
    {
        if flags.contains(rusqlite::OpenFlags::SQLITE_OPEN_CREATE) {
            match options().write(true).create_new(true).open(path) {
                Ok(_) => {}
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                    tighten_sqlite_file(path)
                }
                Err(error) => return Err(rusqlite::Error::ToSqlConversionFailure(Box::new(error))),
            }
        } else {
            // Even a read-only schema peek can create the WAL index when its
            // parent is writable, so repair the selected database before it.
            tighten_sqlite_file(path);
        }
    }
    #[cfg(not(unix))]
    let _ = (path, flags);
    Ok(())
}

/// Repair the one database being opened so newly created sidecars cannot
/// inherit an old public mode. No directory or sibling file is changed here.
#[cfg(unix)]
fn tighten_sqlite_file(path: &Path) {
    use std::os::fd::{AsRawFd, FromRawFd};
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    // Already-private databases are the hot path. No chmod is needed, so one
    // lstat avoids reopening every ancestor on each read connection.
    if fs::symlink_metadata(path)
        .is_ok_and(|metadata| !metadata.is_file() || metadata.permissions().mode() & 0o077 == 0)
    {
        return;
    }
    let result = (|| -> io::Result<()> {
        let absolute = if path.is_absolute() {
            path.to_path_buf()
        } else {
            std::env::current_dir()?.join(path)
        };
        let mut handle = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
            .open("/")?;
        let components: Vec<_> = absolute.components().collect();
        for (index, component) in components.iter().enumerate().skip(1) {
            let std::path::Component::Normal(name) = component else {
                return Ok(());
            };
            let name = std::ffi::CString::new(name.as_bytes()).map_err(io::Error::other)?;
            let flags = libc::O_RDONLY
                | libc::O_NOFOLLOW
                | libc::O_CLOEXEC
                | if index + 1 == components.len() {
                    0
                } else {
                    libc::O_DIRECTORY
                };
            let fd = unsafe { libc::openat(handle.as_raw_fd(), name.as_ptr(), flags) };
            if fd < 0 {
                return Err(io::Error::last_os_error());
            }
            handle = unsafe { File::from_raw_fd(fd) };
        }
        let metadata = handle.metadata()?;
        if metadata.is_file() && metadata.permissions().mode() & 0o077 != 0 {
            handle.set_permissions(fs::Permissions::from_mode(FILE_MODE))?;
        }
        Ok(())
    })();
    if let Err(error) = result {
        if !matches!(
            error.raw_os_error(),
            Some(libc::ELOOP | libc::ENOTDIR | libc::ENOENT)
        ) {
            warn_once(path, &error);
        }
    }
}

/// Extract a newly downloaded debug archive without trusting its stored modes.
/// Streaming each member into our own file handle avoids unzip/ditto restoring
/// public archive permissions, and treats archived symlinks as inert data.
#[cfg(unix)]
pub fn extract_zip(archive: &Path, destination: &Path) -> io::Result<()> {
    use std::process::{Command, Stdio};
    let listing = Command::new("unzip").arg("-Z1").arg(archive).output()?;
    if !listing.status.success() {
        return Err(io::Error::other("could not list debug archive"));
    }
    let members = std::str::from_utf8(&listing.stdout).map_err(io::Error::other)?;
    for member in members.lines() {
        let relative = Path::new(member);
        if relative.is_absolute()
            || relative
                .components()
                .any(|component| !matches!(component, std::path::Component::Normal(_)))
        {
            return Err(io::Error::other("debug archive member escapes destination"));
        }
        let path = destination.join(relative);
        if member.ends_with('/') {
            create_dir_all(&path)?;
        } else {
            if let Some(parent) = path.parent() {
                create_dir_all(parent)?;
            }
            let file = options().write(true).create_new(true).open(&path)?;
            let status = Command::new("unzip")
                .arg("-p")
                .arg(archive)
                .arg(member)
                .stdin(Stdio::null())
                .stdout(Stdio::from(file))
                .stderr(Stdio::null())
                .status()?;
            if !status.success() {
                return Err(io::Error::other("could not extract debug archive member"));
            }
        }
    }
    Ok(())
}
