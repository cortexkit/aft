//! Sealed databases. A database with a seal record (`<database>-seal`) next
//! to it never changes: its readers open it with SQLite `immutable=1`, so a
//! write open of it is refused.

use std::path::{Path, PathBuf};

/// Appended to a sealed database's path to name its seal record. The
/// generation sweep treats it as one of the database's sibling files, so the
/// record is reclaimed together with its donor.
pub(crate) const SEAL_SUFFIX: &str = "-seal";

pub(crate) fn seal_path(database: &Path) -> PathBuf {
    let mut name = database.as_os_str().to_owned();
    name.push(SEAL_SUFFIX);
    PathBuf::from(name)
}

pub(crate) fn is_sealed(database: &Path) -> bool {
    seal_path(database).exists()
}

/// Write opens of a sealed database are refused: its readers open it with
/// `immutable=1`, so any change would be invisible to them or corrupt them.
pub(crate) fn refuse_sealed_write(database: &Path) -> rusqlite::Result<()> {
    if is_sealed(database) {
        return Err(rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_READONLY),
            Some(format!(
                "sealed donor {} cannot be opened for writing",
                database.display()
            )),
        ));
    }
    Ok(())
}
