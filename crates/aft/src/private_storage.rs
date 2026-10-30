//! Private store creation, independent of the process umask. These helpers are
//! for AFT-owned state, never for edits or restorations in the user's project.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

#[cfg(unix)]
pub(crate) const DIR_MODE: u32 = 0o700;
#[cfg(unix)]
pub(crate) const FILE_MODE: u32 = 0o600;

pub fn create_dir_all(path: impl AsRef<Path>) -> io::Result<()> {
    let path = path.as_ref();
    if let Some(access) = crate::root_cache::pin_cache_path(path)? {
        access.directory(true)?;
        return Ok(());
    }
    crate::root_cache::refuse_new_durable_cache_entry(path)?;
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
    if let Some(access) = crate::root_cache::pin_cache_path(path)? {
        let (parent, name) = access.parent()?;
        let child = parent.repair_create_dir(&name)?;
        return child.repair_check_boundary(&access.root, &[]);
    }
    crate::root_cache::refuse_new_durable_cache_entry(path)?;
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
    if let Some(access) = crate::root_cache::pin_cache_path(path)? {
        let (parent, name) = access.parent()?;
        let mut file = parent.repair_create_file(&name)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            file.set_permissions(fs::Permissions::from_mode(DIR_MODE))?;
        }
        return file.write_all(contents);
    }
    executable_options()
        .write(true)
        .create(true)
        .truncate(true)
        .open(path)?
        .write_all(contents)
}

pub fn create(path: impl AsRef<Path>) -> io::Result<File> {
    if let Some(access) = crate::root_cache::pin_cache_path(path.as_ref())? {
        let (parent, name) = access.parent()?;
        return parent.repair_create_file(&name);
    }
    crate::root_cache::refuse_new_durable_cache_entry(path.as_ref())?;
    options().write(true).create(true).truncate(true).open(path)
}

pub fn write(path: impl AsRef<Path>, contents: impl AsRef<[u8]>) -> io::Result<()> {
    create(path)?.write_all(contents.as_ref())
}

/// std::fs::copy copies source permissions too, which would expose snapshots of
/// public files. Stream into an owner-only destination instead on Unix.
pub(crate) fn copy(from: impl AsRef<Path>, to: impl AsRef<Path>) -> io::Result<u64> {
    if let Some(access) = crate::root_cache::pin_cache_path(to.as_ref())? {
        let (parent, name) = access.parent()?;
        return io::copy(
            &mut File::open(from)?,
            &mut parent.repair_create_file(&name)?,
        );
    }
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
    // With SQLITE_OPEN_URI, a name starting with `file:` is a URI, not a
    // filesystem path, so the cache-location checks must look at the file it
    // names. Handing them the URI text instead skips them on Unix (its
    // "ancestors" are relative names that do not exist) and fails outright on
    // Windows, where a name containing `:` or `?` is invalid (os error 123),
    // not missing.
    if flags.contains(rusqlite::OpenFlags::SQLITE_OPEN_URI) {
        match sqlite_uri_target(path) {
            Some(SqliteUriTarget::File(file)) => return prepare_sqlite_file(&file, flags, true),
            Some(SqliteUriTarget::NoFile) => return Ok(()),
            None => {}
        }
    }
    prepare_sqlite_file(path, flags, false)
}

/// `from_uri` is set when `path` was decoded from a `file:` URI. Such opens
/// get the cache-location checks but, as before those checks existed, no
/// reservation or permission repair outside a cache root: the URI callers
/// prepare the plain path themselves before opening it.
fn prepare_sqlite_file(
    path: &Path,
    flags: rusqlite::OpenFlags,
    from_uri: bool,
) -> rusqlite::Result<()> {
    let access = if flags.contains(rusqlite::OpenFlags::SQLITE_OPEN_MEMORY) {
        None
    } else {
        crate::root_cache::pin_cache_path(path)
            .map_err(|error| rusqlite::Error::ToSqlConversionFailure(Box::new(error)))?
    };
    if let Some(access) = access {
        let result = (|| -> io::Result<()> {
            access.root.repair_validate_descendants(&access.relative)?;
            let (parent, name) = access.parent()?;
            let file = if flags.contains(rusqlite::OpenFlags::SQLITE_OPEN_CREATE) {
                match parent.repair_new_file(&name) {
                    Ok(file) => file,
                    Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                        parent.repair_read_file(&name)?
                    }
                    Err(error) => return Err(error),
                }
            } else {
                parent.repair_read_file(&name)?
            };
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                if file.metadata()?.permissions().mode() & 0o077 != 0 {
                    file.set_permissions(fs::Permissions::from_mode(FILE_MODE))?;
                }
            }
            #[cfg(not(unix))]
            let _ = file;
            Ok(())
        })();
        return result.map_err(|error| rusqlite::Error::ToSqlConversionFailure(Box::new(error)));
    }
    crate::root_cache::check_cache_access(path)
        .map_err(|error| rusqlite::Error::ToSqlConversionFailure(Box::new(error)))?;
    if flags.intersects(
        rusqlite::OpenFlags::SQLITE_OPEN_CREATE | rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE,
    ) && !flags.contains(rusqlite::OpenFlags::SQLITE_OPEN_MEMORY)
    {
        crate::root_cache::refuse_durable_cache_write(path)
            .map_err(|error| rusqlite::Error::ToSqlConversionFailure(Box::new(error)))?;
    }
    #[cfg(unix)]
    if !from_uri
        && !flags.contains(rusqlite::OpenFlags::SQLITE_OPEN_MEMORY)
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
    let _ = (path, flags, from_uri);
    Ok(())
}

/// What a SQLite `file:` URI opens (see <https://sqlite.org/uri.html>).
#[derive(Debug, PartialEq, Eq)]
enum SqliteUriTarget {
    /// A database file at this filesystem path.
    File(PathBuf),
    /// An in-memory or temporary database: nothing on disk to check.
    NoFile,
}

/// Decode a SQLite `file:` URI into the database it opens, or `None` when
/// `path` is not such a URI. Covers the forms AFT builds for read-only opens
/// (`file:///C:/...`, `file:///abs/...`, `file:relative`) with percent-encoding
/// and a query string.
fn sqlite_uri_target(path: &Path) -> Option<SqliteUriTarget> {
    let text = path.to_str()?;
    let rest = text.strip_prefix("file:")?;
    let end = rest.find(['?', '#']).unwrap_or(rest.len());
    let (encoded, tail) = rest.split_at(end);
    let query = tail
        .strip_prefix('?')
        .map(|query| query.split('#').next().unwrap_or_default())
        .unwrap_or_default();
    if query
        .split('&')
        .any(|param| param.eq_ignore_ascii_case("mode=memory"))
    {
        return Some(SqliteUriTarget::NoFile);
    }
    // `file://authority/path`: SQLite accepts an empty or `localhost`
    // authority; any other host is kept as a network (UNC) name.
    let encoded = match encoded.strip_prefix("//") {
        Some(after) => {
            let slash = after.find('/').unwrap_or(after.len());
            let (authority, path) = after.split_at(slash);
            if authority.is_empty() || authority.eq_ignore_ascii_case("localhost") {
                path.to_string()
            } else {
                format!("//{after}")
            }
        }
        None => encoded.to_string(),
    };
    let decoded = percent_decode(&encoded);
    if decoded.is_empty() || decoded == ":memory:" {
        return Some(SqliteUriTarget::NoFile);
    }
    #[cfg(windows)]
    let decoded = {
        // `/C:/dir/db` names drive C:; SQLite drops the leading slash too.
        let bytes = decoded.as_bytes();
        let decoded = if bytes.len() >= 3
            && bytes[0] == b'/'
            && bytes[1].is_ascii_alphabetic()
            && bytes[2] == b':'
        {
            decoded[1..].to_string()
        } else if decoded.starts_with(r"/\\") {
            // `/\\?\C:\...` is a verbatim (`\\?\`) or UNC path whose
            // backslashes the URI kept percent-encoded, as the call-graph
            // store's read-only URI spells long and resolved paths. SQLite
            // drops the leading slash before opening it.
            decoded[1..].to_string()
        } else {
            decoded
        };
        // Restore the separators the URI form replaced: a verbatim `\\?\`
        // path only accepts backslashes.
        decoded.replace('/', "\\")
    };
    Some(SqliteUriTarget::File(PathBuf::from(decoded)))
}

fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        let hex = |byte: u8| (byte as char).to_digit(16);
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            if let (Some(high), Some(low)) = (hex(bytes[index + 1]), hex(bytes[index + 2])) {
                decoded.push((high * 16 + low) as u8);
                index += 3;
                continue;
            }
        }
        decoded.push(bytes[index]);
        index += 1;
    }
    String::from_utf8_lossy(&decoded).into_owned()
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

#[cfg(test)]
mod sqlite_uri_tests {
    use super::{sqlite_uri_target, SqliteUriTarget};
    use std::path::{Path, PathBuf};

    fn target(uri: &str) -> Option<SqliteUriTarget> {
        sqlite_uri_target(Path::new(uri))
    }

    #[test]
    fn plain_paths_are_not_uris() {
        assert_eq!(target("/var/aft/callgraph/x.sqlite"), None);
        assert_eq!(target(r"C:\Users\aft\x.sqlite"), None);
        assert_eq!(target(":memory:"), None);
    }

    #[test]
    fn memory_and_temporary_uris_name_no_file() {
        assert_eq!(target("file::memory:"), Some(SqliteUriTarget::NoFile));
        assert_eq!(
            target("file::memory:?cache=shared"),
            Some(SqliteUriTarget::NoFile)
        );
        assert_eq!(
            target("file:shared-db?mode=memory&cache=shared"),
            Some(SqliteUriTarget::NoFile)
        );
        assert_eq!(target("file:?mode=ro"), Some(SqliteUriTarget::NoFile));
    }

    #[test]
    fn relative_uri_decodes_percent_escapes() {
        assert_eq!(
            target("file:dir/name%20with%20space%231.sqlite?mode=ro"),
            Some(SqliteUriTarget::File(PathBuf::from(if cfg!(windows) {
                r"dir\name with space#1.sqlite"
            } else {
                "dir/name with space#1.sqlite"
            })))
        );
    }

    #[cfg(unix)]
    #[test]
    fn absolute_unix_uri_names_its_path() {
        assert_eq!(
            target("file:///private/var/aft/db%231.sqlite?mode=ro"),
            Some(SqliteUriTarget::File(PathBuf::from(
                "/private/var/aft/db#1.sqlite"
            )))
        );
        assert_eq!(
            target("file://localhost/tmp/x.sqlite"),
            Some(SqliteUriTarget::File(PathBuf::from("/tmp/x.sqlite")))
        );
    }

    #[cfg(windows)]
    #[test]
    fn windows_uris_name_drive_verbatim_and_unc_paths() {
        assert_eq!(
            target("file:///C:/Users/RUNNER~1/name%20x/db.sqlite?mode=ro"),
            Some(SqliteUriTarget::File(PathBuf::from(
                r"C:\Users\RUNNER~1\name x\db.sqlite"
            )))
        );
        // A verbatim path keeps its prefix, spelled with backslashes again.
        assert_eq!(
            target("file:////%3F/C:/Users/aft/db.sqlite?mode=ro"),
            Some(SqliteUriTarget::File(PathBuf::from(
                r"\\?\C:\Users\aft\db.sqlite"
            )))
        );
        // The call-graph store's read-only URI keeps a verbatim path's
        // backslashes percent-encoded after one leading slash.
        assert_eq!(
            target("file:///%5C%5C%3F%5CC:%5CUsers%5Caft%5Cdb%231.sqlite?mode=ro"),
            Some(SqliteUriTarget::File(PathBuf::from(
                r"\\?\C:\Users\aft\db#1.sqlite"
            )))
        );
        assert_eq!(
            target("file://server/share/db.sqlite"),
            Some(SqliteUriTarget::File(PathBuf::from(
                r"\\server\share\db.sqlite"
            )))
        );
    }
}
