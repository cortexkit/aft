//! File operations the formats need, behaving the same on every operating
//! system.

use std::fs::{File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::Path;

use crate::fault;

/// Creates a new file for writing, owner-only on Unix. Refuses to replace an
/// existing file: stored files are immutable and their names never reused.
///
/// On Windows the file inherits the storage folder's access rules, which AFT
/// creates for the current user only.
pub(crate) fn create_new_private(path: &Path) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.read(true).write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path)
}

/// Opens a stored file for reading.
///
/// On Windows the handle lets the file be deleted, and written, while it is
/// open, as Unix does. Without deletion, one reader holding a segment open
/// would stop reclamation from removing it. Without writing, Windows refuses
/// the open itself while the writer still has a new file open: a reader
/// taking a snapshot during a publication would fail on the manifest being
/// written instead of skipping it as an incomplete record, as on Unix.
pub(crate) fn open_read(path: &Path) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        const FILE_SHARE_READ: u32 = 0x1;
        const FILE_SHARE_WRITE: u32 = 0x2;
        const FILE_SHARE_DELETE: u32 = 0x4;
        options.share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE);
    }
    options.open(path)
}

/// Writes a new file and makes it and its directory entry durable before
/// returning. Refuses to replace an existing file.
pub(crate) fn write_new_durable(path: &Path, bytes: &[u8]) -> io::Result<()> {
    fault::point("create")?;
    let mut file = create_new_private(path)?;
    fault::point("write")?;
    file.write_all(bytes)?;
    fault::point("sync")?;
    file.sync_all()?;
    drop(file);
    sync_parent(path)
}

/// Reads a whole stored file of at most `limit` bytes, with the same sharing
/// rules as [`open_read`]; `None` when it is longer. Nothing past the limit
/// is read, so a damaged or hostile file cannot make the reader allocate
/// for its full length.
pub(crate) fn read_at_most(path: &Path, limit: u64) -> io::Result<Option<Vec<u8>>> {
    let mut bytes = Vec::new();
    open_read(path)?
        .take(limit.saturating_add(1))
        .read_to_end(&mut bytes)?;
    Ok((bytes.len() as u64 <= limit).then_some(bytes))
}

/// Removes a file; a file that is already gone counts as removed.
pub(crate) fn remove_file(path: &Path) -> io::Result<()> {
    fault::point("remove")?;
    match std::fs::remove_file(path) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        result => result,
    }
}

/// Renames `from` to `to`, which must not exist, and makes the change durable.
///
/// Every name the store renames to is new, so the rename never replaces a
/// file. That keeps it correct on volumes without atomic rename-over.
pub(crate) fn rename_new_durable(from: &Path, to: &Path) -> io::Result<()> {
    if to.exists() {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "a stored file name is never reused",
        ));
    }
    fault::point("rename")?;
    std::fs::rename(from, to)?;
    sync_parent(to)
}

/// Flushes the directory holding `path`, so a file created, renamed or
/// removed in it stays so after power loss.
pub(crate) fn sync_parent(path: &Path) -> io::Result<()> {
    match path.parent() {
        Some(parent) => sync_dir(parent),
        None => Ok(()),
    }
}

/// Flushes a directory's entries to stable storage.
///
/// Windows has no supported way to flush a directory through a normal
/// handle. NTFS journals directory changes itself, so this does nothing
/// there, and the store promises no more than that across power loss.
pub(crate) fn sync_dir(path: &Path) -> io::Result<()> {
    fault::point("sync_dir")?;
    #[cfg(unix)]
    {
        File::open(path)?.sync_all()
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        Ok(())
    }
}

/// Reads exactly `buf.len()` bytes at `offset`. Every call names its own
/// offset, so concurrent readers of one handle never depend on a shared cursor.
pub(crate) fn read_exact_at(file: &File, buf: &mut [u8], offset: u64) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::FileExt;
        file.read_exact_at(buf, offset)
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::FileExt;
        let mut done = 0;
        while done < buf.len() {
            let read = file.seek_read(&mut buf[done..], offset + done as u64)?;
            if read == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "file ended before the requested range",
                ));
            }
            done += read;
        }
        Ok(())
    }
}
