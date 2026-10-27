//! Sealed call-graph donors.
//!
//! A base's derived call graph is built privately before the base pointer can
//! name it. Before that pointer moves, the builder seals the file: it switches
//! it to `journal_mode=DELETE`, checkpoints, syncs it, proves that no `-wal`,
//! `-shm` or `-journal` file remains, and atomically publishes a seal record
//! next to it (`<database>-seal`). From then on the file never changes:
//!
//! * every reader opens it with SQLite `immutable=1` (no locks, no WAL);
//! * a write open through `IdentityConnection` is refused while the seal
//!   record exists (see `refuse_sealed_write`);
//! * a worker checkout clones it into a fresh, never-reused private name
//!   (`clonefile` on APFS, `FICLONE` reflink on Linux, otherwise a plain copy
//!   whose reason is logged) and patches the clone with the manifest diff.
//!
//! A live SQLite database is never cloned or copied here: `clone_sealed`
//! refuses a donor whose seal record is missing or no longer matches.

use std::io::{Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

/// Appended to a sealed database's path to name its seal record. The
/// generation sweep treats it as one of the database's sibling files, so the
/// record is reclaimed together with its donor.
pub(crate) const SEAL_SUFFIX: &str = "-seal";
const SEAL_FORMAT: u32 = 1;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) struct SealRecord {
    pub format: u32,
    /// BLAKE3 of the sealed file's bytes, as hex.
    pub fingerprint: String,
    pub size: u64,
    pub journal_mode: String,
}

/// How a worker's private copy of a donor was made.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum CloneMechanism {
    /// APFS `clonefile(2)`: copy-on-write, no data copied.
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    Clonefile,
    /// Linux `FICLONE` reflink: copy-on-write, no data copied.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    Reflink,
    /// The filesystem cannot share extents; the sealed bytes were copied.
    /// Safe because a sealed file never changes. Carries the reason.
    Copy(String),
}

/// Why a worker graph was built cold instead of from a sealed donor. This is
/// the closed list callers report; ordinary fixtures never produce one.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum ColdReason {
    /// The base has no sealed donor (for example, a base published before
    /// sealing existed).
    NoSealedDonor,
    /// The donor's seal record is missing, unreadable or does not match.
    SealInvalid(String),
    /// The donor's recorded manifest fingerprint is not the base's.
    BaseFingerprintMismatch,
}

/// One donor decision for a worker graph, recorded per member directory so
/// tests can count reuse without reading global state of other fixtures.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum DonorEvent {
    Sealed,
    Cloned(CloneMechanism),
    Cold(ColdReason),
}

static EVENTS: Mutex<Vec<(PathBuf, DonorEvent)>> = Mutex::new(Vec::new());

pub(crate) fn record(dir: &Path, event: DonorEvent) {
    match &event {
        DonorEvent::Cloned(CloneMechanism::Copy(reason)) => log::info!(
            "views donor clone fell back to a copy dir={} reason={reason}",
            dir.display()
        ),
        DonorEvent::Cold(reason) => log::info!(
            "views worker graph built cold dir={} reason={reason:?}",
            dir.display()
        ),
        _ => {}
    }
    EVENTS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .push((dir.to_path_buf(), event));
}

/// Donor events recorded for directories under `prefix`.
#[cfg(test)]
pub(crate) fn events_under(prefix: &Path) -> Vec<DonorEvent> {
    EVENTS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .iter()
        .filter(|(dir, _)| dir.starts_with(prefix))
        .map(|(_, event)| event.clone())
        .collect()
}

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

fn sibling(database: &Path, suffix: &str) -> PathBuf {
    let mut name = database.as_os_str().to_owned();
    name.push(suffix);
    PathBuf::from(name)
}

fn error(step: &str, reason: impl std::fmt::Display) -> String {
    format!("donor {step}: {reason}")
}

/// SQLite header bytes 18 and 19 are the write and read format versions:
/// 1 means a rollback journal, 2 means WAL.
fn header_journal_mode(database: &Path) -> Result<&'static str, String> {
    let mut header = [0u8; 20];
    std::fs::File::open(database)
        .and_then(|mut file| file.read_exact(&mut header))
        .map_err(|e| error("header", e))?;
    match (header[18], header[19]) {
        (1, 1) => Ok("delete"),
        (2, 2) => Ok("wal"),
        other => Err(error(
            "header",
            format!("unknown format versions {other:?}"),
        )),
    }
}

fn fingerprint(database: &Path) -> Result<(String, u64), String> {
    let mut file = std::fs::File::open(database).map_err(|e| error("fingerprint", e))?;
    let mut hasher = blake3::Hasher::new();
    let mut buffer = vec![0u8; 1 << 16];
    let mut size = 0u64;
    loop {
        let read = file
            .read(&mut buffer)
            .map_err(|e| error("fingerprint", e))?;
        if read == 0 {
            break;
        }
        size += read as u64;
        hasher.update(&buffer[..read]);
    }
    Ok((hasher.finalize().to_hex().to_string(), size))
}

fn no_journal_siblings(database: &Path) -> Result<(), String> {
    for suffix in ["-wal", "-shm", "-journal"] {
        if sibling(database, suffix).exists() {
            return Err(error("seal", format!("{suffix} file remains")));
        }
    }
    Ok(())
}

/// Seals a private, fully built database that no reader can reach yet.
/// Must run before any pointer names it: the journal-mode switch needs
/// exclusive access, and the sync below opens a plain descriptor, which is
/// only safe while no SQLite connection in this process has the file open.
pub(crate) fn seal(database: &Path) -> Result<SealRecord, String> {
    {
        let connection =
            crate::db::file_identity::IdentityConnection::open(database, "views::donor::seal")
                .map_err(|e| error("open", e))?;
        connection
            .query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |_| Ok(()))
            .map_err(|e| error("checkpoint", e))?;
        let mode: String = connection
            .query_row("PRAGMA journal_mode=DELETE", [], |row| row.get(0))
            .map_err(|e| error("journal_mode", e))?;
        if !mode.eq_ignore_ascii_case("delete") {
            return Err(error("journal_mode", mode));
        }
    }
    {
        let _files = crate::db::file_identity::filesystem_guard();
        if crate::db::file_identity::open_connections(database) != 0 {
            return Err(error("seal", "the database is still open"));
        }
        std::fs::File::open(database)
            .and_then(|file| file.sync_all())
            .map_err(|e| error("fsync", e))?;
    }
    no_journal_siblings(database)?;
    let journal_mode = header_journal_mode(database)?;
    if journal_mode != "delete" {
        return Err(error("seal", format!("header names {journal_mode}")));
    }
    let (fingerprint, size) = fingerprint(database)?;
    let record = SealRecord {
        format: SEAL_FORMAT,
        fingerprint,
        size,
        journal_mode: journal_mode.into(),
    };
    let path = seal_path(database);
    let temporary = sibling(&path, &format!(".tmp.{}", super::base_refs::fresh_nonce()));
    let bytes = serde_json::to_vec(&record).map_err(|e| error("record", e))?;
    let written = (|| -> std::io::Result<()> {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)?;
        file.write_all(&bytes)?;
        file.sync_all()?;
        std::fs::rename(&temporary, &path)
    })();
    if let Err(e) = written {
        let _ = std::fs::remove_file(&temporary);
        return Err(error("record", e));
    }
    if let Some(parent) = path.parent() {
        super::sync_directory(parent).map_err(|e| error("record", e))?;
    }
    Ok(record)
}

/// Reads the seal record and checks it against the file's size, header and
/// siblings. The full fingerprint is checked by `verify_fingerprint`.
pub(crate) fn verify(database: &Path) -> Result<SealRecord, String> {
    let bytes = std::fs::read(seal_path(database)).map_err(|e| error("record", e))?;
    let record: SealRecord = serde_json::from_slice(&bytes).map_err(|e| error("record", e))?;
    if record.format != SEAL_FORMAT || record.journal_mode != "delete" {
        return Err(error("record", "unsupported seal record"));
    }
    let size = std::fs::metadata(database)
        .map_err(|e| error("metadata", e))?
        .len();
    if size != record.size {
        return Err(error("verify", "size differs from the seal record"));
    }
    no_journal_siblings(database)?;
    if header_journal_mode(database)? != "delete" {
        return Err(error("verify", "the header is not in rollback mode"));
    }
    Ok(record)
}

/// Checks the file's bytes against the recorded fingerprint.
pub(crate) fn verify_fingerprint(database: &Path, record: &SealRecord) -> Result<(), String> {
    let (fingerprint, size) = fingerprint(database)?;
    if fingerprint != record.fingerprint || size != record.size {
        return Err(error("verify", "bytes differ from the seal record"));
    }
    Ok(())
}

/// Clones a sealed donor into `destination`, which must not exist yet: a
/// clone name is fresh and never reused. On failure nothing is left behind.
pub(crate) fn clone_sealed(donor: &Path, destination: &Path) -> Result<CloneMechanism, String> {
    let record = verify(donor)?;
    if destination.exists() {
        return Err(error("clone", "the destination name was used before"));
    }
    let result = clone_file(donor, destination).and_then(|mechanism| {
        let size = std::fs::metadata(destination)
            .map_err(|e| error("clone", e))?
            .len();
        if size != record.size {
            return Err(error("clone", "the copy has the wrong size"));
        }
        if cfg!(test) {
            verify_fingerprint(destination, &record)?;
        }
        std::fs::File::open(destination)
            .and_then(|file| file.sync_all())
            .map_err(|e| error("fsync", e))?;
        if let Some(parent) = destination.parent() {
            super::sync_directory(parent).map_err(|e| error("fsync", e))?;
        }
        Ok(mechanism)
    });
    if result.is_err() {
        let _ = std::fs::remove_file(destination);
    }
    result
}

#[cfg(test)]
thread_local! {
    /// Forces the copy fallback so tests can cover it on clone-capable disks.
    static FORCE_COPY: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

#[cfg(test)]
pub(crate) fn force_copy_for_test(force: bool) {
    FORCE_COPY.with(|cell| cell.set(force));
}

fn forced_copy() -> bool {
    #[cfg(test)]
    {
        FORCE_COPY.with(std::cell::Cell::get)
    }
    #[cfg(not(test))]
    {
        false
    }
}

fn copy(donor: &Path, destination: &Path, reason: String) -> Result<CloneMechanism, String> {
    let mut source = std::fs::File::open(donor).map_err(|e| error("copy", e))?;
    let mut target = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(destination)
        .map_err(|e| error("copy", e))?;
    std::io::copy(&mut source, &mut target).map_err(|e| error("copy", e))?;
    Ok(CloneMechanism::Copy(reason))
}

#[cfg(target_os = "macos")]
fn clone_file(donor: &Path, destination: &Path) -> Result<CloneMechanism, String> {
    use std::os::unix::ffi::OsStrExt;
    if forced_copy() {
        return copy(donor, destination, "forced by test".into());
    }
    let source =
        std::ffi::CString::new(donor.as_os_str().as_bytes()).map_err(|e| error("clone", e))?;
    let target = std::ffi::CString::new(destination.as_os_str().as_bytes())
        .map_err(|e| error("clone", e))?;
    // `CLONE_NOFOLLOW` from <sys/clonefile.h>: refuse a symlinked source.
    const CLONE_NOFOLLOW: u32 = 0x0001;
    // Both C strings outlive the call.
    if unsafe { libc::clonefile(source.as_ptr(), target.as_ptr(), CLONE_NOFOLLOW) } == 0 {
        return Ok(CloneMechanism::Clonefile);
    }
    let reason = std::io::Error::last_os_error();
    copy(donor, destination, format!("clonefile: {reason}"))
}

#[cfg(target_os = "linux")]
fn clone_file(donor: &Path, destination: &Path) -> Result<CloneMechanism, String> {
    use std::os::fd::AsRawFd;
    if forced_copy() {
        return copy(donor, destination, "forced by test".into());
    }
    // A plain descriptor on the donor is safe: every opener of a sealed file
    // uses `immutable=1`, so no SQLite connection holds a POSIX lock on it
    // that closing this descriptor could release.
    let source = std::fs::File::open(donor).map_err(|e| error("clone", e))?;
    let target = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(destination)
        .map_err(|e| error("clone", e))?;
    // SAFETY: both descriptors are open for the duration of the call.
    if unsafe { libc::ioctl(target.as_raw_fd(), libc::FICLONE, source.as_raw_fd()) } == 0 {
        return Ok(CloneMechanism::Reflink);
    }
    let reason = std::io::Error::last_os_error();
    drop(target);
    let _ = std::fs::remove_file(destination);
    copy(donor, destination, format!("FICLONE: {reason}"))
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn clone_file(donor: &Path, destination: &Path) -> Result<CloneMechanism, String> {
    let reason = if forced_copy() {
        "forced by test"
    } else {
        "no copy-on-write clone on this platform"
    };
    copy(donor, destination, reason.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn database(dir: &Path) -> PathBuf {
        let path = dir.join("derived-donor.sqlite");
        let connection =
            crate::db::file_identity::IdentityConnection::open(&path, "views::donor::tests")
                .unwrap();
        connection
            .execute_batch(
                "PRAGMA journal_mode=WAL; CREATE TABLE t(v); INSERT INTO t VALUES (1), (2);",
            )
            .unwrap();
        drop(connection);
        path
    }

    #[test]
    fn sealing_leaves_a_rollback_file_with_a_record_and_refuses_write_opens() {
        let temp = tempfile::tempdir().unwrap();
        let path = database(temp.path());
        assert!(!is_sealed(&path));
        let record = seal(&path).unwrap();
        assert!(is_sealed(&path));
        assert_eq!(record.journal_mode, "delete");
        for suffix in ["-wal", "-shm", "-journal"] {
            assert!(!sibling(&path, suffix).exists(), "{suffix} remains");
        }
        assert_eq!(header_journal_mode(&path).unwrap(), "delete");
        assert_eq!(verify(&path).unwrap(), record);
        verify_fingerprint(&path, &record).unwrap();
        let refused =
            crate::db::file_identity::IdentityConnection::open(&path, "views::donor::tests")
                .unwrap_err();
        assert!(refused.to_string().contains("sealed donor"), "{refused}");
        let refused = crate::db::file_identity::IdentityConnection::open_with_flags(
            &path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE,
            "views::donor::tests",
        )
        .unwrap_err();
        assert!(refused.to_string().contains("sealed donor"), "{refused}");
        // Read-only opens stay allowed, and an immutable reader sees the rows.
        let uri = format!(
            "file:{}?mode=ro&immutable=1",
            path.to_string_lossy().replace('\\', "/")
        );
        let reader = rusqlite::Connection::open_with_flags(
            uri,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_URI,
        )
        .unwrap();
        let rows: i64 = reader
            .query_row("SELECT COUNT(*) FROM t", [], |row| row.get(0))
            .unwrap();
        assert_eq!(rows, 2);
        drop(reader);
        assert!(!sibling(&path, "-wal").exists());
    }

    #[test]
    fn clones_use_fresh_names_keep_the_donor_unchanged_and_refuse_tampering() {
        let temp = tempfile::tempdir().unwrap();
        let path = database(temp.path());
        let record = seal(&path).unwrap();
        for (name, forced) in [("clone-a.sqlite", false), ("clone-b.sqlite", true)] {
            force_copy_for_test(forced);
            let destination = temp.path().join(name);
            let mechanism = clone_sealed(&path, &destination).unwrap();
            force_copy_for_test(false);
            if forced {
                assert_eq!(mechanism, CloneMechanism::Copy("forced by test".into()));
            }
            verify_fingerprint(&destination, &record).unwrap();
            // The clone is an ordinary private database: writable, unsealed.
            assert!(!is_sealed(&destination));
            let connection = crate::db::file_identity::IdentityConnection::open(
                &destination,
                "views::donor::tests",
            )
            .unwrap();
            connection.execute("INSERT INTO t VALUES (3)", []).unwrap();
            drop(connection);
            // A name is never reused, even after a successful clone.
            assert!(clone_sealed(&path, &destination)
                .unwrap_err()
                .contains("used before"));
        }
        verify_fingerprint(&path, &record).unwrap();
        // A donor whose bytes no longer match its record is never cloned.
        std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(b"tamper")
            .unwrap();
        let destination = temp.path().join("clone-c.sqlite");
        assert!(clone_sealed(&path, &destination).is_err());
        assert!(!destination.exists());
        // An unsealed (live) database is never cloned.
        let live = temp.path().join("live");
        std::fs::create_dir_all(&live).unwrap();
        let live = database(&live);
        assert!(clone_sealed(&live, &temp.path().join("clone-d.sqlite")).is_err());
    }
}
