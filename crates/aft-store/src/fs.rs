//! File operations the formats need, behaving the same on every operating
//! system.

use std::fs::{File, OpenOptions};
use std::io;
use std::path::Path;

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
/// On Windows the handle lets the file be deleted while it is open. Without
/// that, one reader holding a segment open would stop reclamation from
/// removing it, which Unix never does.
pub(crate) fn open_read(path: &Path) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        const FILE_SHARE_READ: u32 = 0x1;
        const FILE_SHARE_DELETE: u32 = 0x4;
        options.share_mode(FILE_SHARE_READ | FILE_SHARE_DELETE);
    }
    options.open(path)
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
