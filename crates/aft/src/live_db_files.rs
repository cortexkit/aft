pub use aft_base::live_db_files::*;

#[cfg(test)]
#[allow(unused_imports)]
use std::collections::{BTreeMap, HashMap, HashSet};
#[cfg(test)]
#[allow(unused_imports)]
use std::ffi::OsStr;
#[cfg(test)]
#[allow(unused_imports)]
use std::path::{Path, PathBuf};
#[cfg(test)]
#[allow(unused_imports)]
use std::sync::{Mutex, OnceLock, RwLock};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{SqliteStore, TrackedConnection};
    // Only the POSIX-only tests below open raw connections.
    #[cfg(unix)]
    use rusqlite::Connection;

    #[cfg(unix)]
    #[test]
    fn reused_sidecar_inode_does_not_block_source_freshness() {
        let dir = tempfile::tempdir().unwrap();
        let database = dir.path().join("registered.cache");
        let source = dir.path().join("source.rs");
        std::fs::write(&database, "database").unwrap();
        std::fs::write(&source, "source").unwrap();
        let metadata = std::fs::metadata(&source).unwrap();
        let Ok(created) = metadata.created() else {
            // Without birth times the registry conservatively retains its
            // device/inode protection, including possible reuse false positives.
            return;
        };
        let key = note_open(&database, SqliteStore::AftDb.label());
        // Model an unlinked sidecar's recycled inode without relying on the
        // filesystem allocator to reuse a particular inode during the test.
        let old = LiveFileIdentity {
            file: metadata_identity(&metadata).unwrap(),
            created: Some(created - std::time::Duration::from_secs(1)),
        };
        {
            let mut databases = open_databases().lock().unwrap();
            retain_identities(databases.get_mut(&key).unwrap(), [old]);
        }
        let result = crate::cache_freshness::collect(&source);
        note_close(&key, SqliteStore::AftDb.label());
        assert!(
            result.is_ok(),
            "recycled sidecar inode denied source: {result:?}"
        );
    }

    #[test]
    fn sidecars_created_after_registration_are_live_by_name() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let database = root.join("unconventional.cache");
        std::fs::write(&database, "database").unwrap();
        let key = note_open(&database, SqliteStore::AftDb.label());
        let sidecars = &database_files(&key)[1..];
        for path in sidecars {
            assert!(!path.exists());
            std::fs::write(path, "created after registration").unwrap();
        }
        let probe = crate::project_content::Probe::new(&root);
        let counts = stat_probe::count(|| {
            for path in sidecars {
                assert!(
                    is_live_sqlite_file(path),
                    "late sidecar was not live: {path:?}"
                );
            }
        });
        assert!(
            counts.is_empty(),
            "named sidecars needed identity stats: {counts:?}"
        );
        for path in sidecars {
            assert_eq!(
                crate::project_content::read(path).unwrap_err().kind(),
                std::io::ErrorKind::PermissionDenied
            );
        }
        assert!(probe.paths().is_empty());
        note_close(&key, SqliteStore::AftDb.label());
        for path in database_files(&key) {
            assert!(!is_registered_file_path(&path));
        }
    }

    #[cfg(unix)]
    #[test]
    fn cached_sidecar_identity_survives_rename_until_last_database_close() {
        let dir = tempfile::tempdir().unwrap();
        let database = dir.path().join("registered.cache");
        let sidecar = dir.path().join("registered.cache-shm");
        std::fs::write(&database, "database").unwrap();
        std::fs::write(&sidecar, "sidecar").unwrap();
        let key = note_open(&database, SqliteStore::AftDb.label());
        note_open(&database, SqliteStore::BlobStore.label());
        let alias = dir.path().join("innocent.rs");
        std::fs::rename(&sidecar, &alias).unwrap();
        assert!(is_live_sqlite_file(&alias));
        note_close(&key, SqliteStore::AftDb.label());
        assert!(is_live_sqlite_file(&alias));
        note_close(&key, SqliteStore::BlobStore.label());
        assert!(!is_live_sqlite_file(&alias));
    }

    #[cfg(unix)]
    #[test]
    fn shared_database_identities_remain_live_until_all_registered_paths_close() {
        let dir = tempfile::tempdir().unwrap();
        let first = dir.path().join("first.cache");
        let second = dir.path().join("second.cache");
        let alias = dir.path().join("alias.rs");
        std::fs::write(&first, "database").unwrap();
        std::fs::hard_link(&first, &second).unwrap();
        std::fs::hard_link(&first, &alias).unwrap();
        let first_key = note_open(&first, SqliteStore::AftDb.label());
        let second_key = note_open(&second, SqliteStore::BlobStore.label());
        note_close(&first_key, SqliteStore::AftDb.label());
        assert!(is_live_sqlite_file(&alias));
        note_close(&second_key, SqliteStore::BlobStore.label());
        assert!(!is_live_sqlite_file(&alias));
    }

    #[cfg(unix)]
    fn shm_path(path: &Path) -> PathBuf {
        PathBuf::from(format!("{}-shm", path.display()))
    }

    /// The mechanism, stated as an executable fact about SQLite rather than as
    /// prose: replace the database file at a path while a connection still has
    /// that path's WAL-index mapped, and the next connection truncates the
    /// WAL-index under the first one.
    ///
    /// SQLite maps a WAL-index in 32 KiB regions and re-extends the file to
    /// cover region 0 immediately after truncating it, so the visible damage is
    /// the file shrinking back to one region while the first connection still
    /// holds pointers into region 1. Reading through those pointers is a read
    /// past the end of a file-backed mapping, which the kernel answers with a
    /// page-aligned SIGBUS. That is why the fixture first grows the WAL past
    /// the 4062 frames region 0 can index: with only region 0 mapped, the
    /// re-extension hides the truncation behind a page of zeroes instead.
    ///
    /// The first connection is deliberately never used or dropped after the
    /// truncation: touching it is the fault. Leaking one handle for the rest of
    /// the test binary is the price of observing the state safely.
    // Windows refuses to unlink or rename a database while a connection holds
    // it open, so the replaced-under-a-live-connection precondition cannot form
    // there; the hazard this registry reports is a POSIX one.
    #[cfg(unix)]
    #[test]
    fn replacing_the_database_file_truncates_the_wal_index_under_a_live_mapping() {
        /// SQLite maps the WAL-index one 32 KiB region at a time.
        const WAL_INDEX_REGION_BYTES: u64 = 32 * 1024;

        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("replaced.sqlite");

        let first = Connection::open(&path).expect("open first connection");
        // A small page size keeps the WAL small in bytes while still producing
        // the thousands of frames needed to reach the second WAL-index region.
        first
            .pragma_update(None, "page_size", 512)
            .expect("small pages");
        first
            .pragma_update(None, "journal_mode", "WAL")
            .expect("first connection WAL");
        first
            .pragma_update(None, "synchronous", "OFF")
            .expect("unsynced fixture writes");
        first
            .pragma_update(None, "wal_autocheckpoint", 0)
            .expect("no autocheckpoint");
        first
            .execute_batch("CREATE TABLE t(id INTEGER PRIMARY KEY, payload BLOB);")
            .expect("seed");

        // One transaction per row, so every commit appends WAL frames and the
        // log grows past what a single WAL-index region can address.
        let mut mapped_bytes = 0;
        for id in 0..20_000_i64 {
            first
                .execute(
                    "INSERT INTO t(id, payload) VALUES (?1, zeroblob(400))",
                    [id],
                )
                .expect("append a WAL frame");
            if id % 256 == 0 {
                mapped_bytes = std::fs::metadata(shm_path(&path))
                    .expect("WAL-index exists")
                    .len();
                if mapped_bytes > WAL_INDEX_REGION_BYTES {
                    break;
                }
            }
        }
        assert!(
            mapped_bytes > WAL_INDEX_REGION_BYTES,
            "fixture never mapped a second WAL-index region (got {mapped_bytes} bytes); \
             without one the truncation is hidden by SQLite re-extending region 0"
        );
        let first_identity = identity_of(&path).expect("first database identity");

        // Replace the database file at the same path, leaving the WAL-index
        // exactly where it is. This is the shape being hunted: a new database
        // file inode under an unchanged `-shm` inode.
        std::fs::remove_file(&path).expect("remove the database file");
        let second = Connection::open(&path).expect("open second connection");
        second
            .pragma_update(None, "journal_mode", "WAL")
            .expect("second connection WAL");
        second
            .execute_batch("CREATE TABLE t(id INTEGER PRIMARY KEY);")
            .expect("write through the replacement");
        let second_identity = identity_of(&path).expect("second database identity");
        assert_ne!(
            first_identity, second_identity,
            "fixture did not actually replace the database file"
        );

        let after_bytes = std::fs::metadata(shm_path(&path))
            .expect("WAL-index still exists")
            .len();
        assert!(
            after_bytes < mapped_bytes,
            "the second connection did not truncate the WAL-index the first one has mapped \
             (before={mapped_bytes} bytes, after={after_bytes} bytes)"
        );

        drop(second);
        // Dropping `first` would make SQLite read the truncated mapping during
        // its close-time checkpoint, so the handle is deliberately leaked.
        std::mem::forget(first);
    }

    // Windows refuses to unlink or rename a database while a connection holds
    // it open, so the replaced-under-a-live-connection precondition cannot form
    // there; the hazard this registry reports is a POSIX one.
    #[cfg(unix)]
    #[test]
    fn a_database_file_replaced_under_a_live_connection_is_reported_with_both_openers() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("reported.sqlite");

        let first = TrackedConnection::open(&path, SqliteStore::CallgraphGeneration)
            .expect("open first tracked connection");
        first
            .execute_batch("CREATE TABLE t(id INTEGER PRIMARY KEY);")
            .expect("seed");
        assert!(
            hazards_for_path(&path).is_empty(),
            "a single connection is not a hazard"
        );

        std::fs::remove_file(&path).expect("remove the database file");
        let second = TrackedConnection::open(&path, SqliteStore::AftDb)
            .expect("open second tracked connection");
        second
            .execute_batch("CREATE TABLE t(id INTEGER PRIMARY KEY);")
            .expect("seed the replacement");

        let hazards = hazards_for_path(&path);
        let replaced = hazards
            .iter()
            .find(|hazard| hazard.kind == HazardKind::Replaced)
            .unwrap_or_else(|| {
                panic!("replacing the database file was not reported; hazards={hazards:?}")
            });
        assert!(
            replaced
                .detail
                .contains(SqliteStore::CallgraphGeneration.label())
                && replaced.detail.contains(SqliteStore::AftDb.label()),
            "the report must name both connections: {}",
            replaced.detail
        );

        drop(second);
        drop(first);
        assert_eq!(open_connections(&path), 0);
    }

    #[test]
    fn removing_a_database_file_under_an_open_connection_is_reported() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("removed.sqlite");
        let connection = TrackedConnection::open(&path, SqliteStore::InspectScopeCache)
            .expect("open tracked connection");
        connection
            .execute_batch("CREATE TABLE t(id INTEGER PRIMARY KEY);")
            .expect("seed");
        assert_eq!(open_connections(&path), 1);

        guard_replacement(&path, "fixture sweep");

        let hazards = hazards_for_path(&path);
        let mutated = hazards
            .iter()
            .find(|hazard| hazard.kind == HazardKind::MutatedWhileOpen)
            .unwrap_or_else(|| panic!("the removal was not reported; hazards={hazards:?}"));
        assert!(
            mutated.detail.contains("fixture sweep")
                && mutated
                    .detail
                    .contains(SqliteStore::InspectScopeCache.label()),
            "the report must name the action and the open connection: {}",
            mutated.detail
        );

        drop(connection);
        // A removal after the last close is ordinary housekeeping, not a hazard.
        let before = hazards_for_path(&path).len();
        guard_replacement(&path, "fixture sweep after close");
        assert_eq!(hazards_for_path(&path).len(), before);
    }

    // Windows refuses to unlink or rename a database while a connection holds
    // it open, so the replaced-under-a-live-connection precondition cannot form
    // there; the hazard this registry reports is a POSIX one.
    #[cfg(unix)]
    #[test]
    fn closing_a_connection_after_its_database_file_is_gone_still_clears_the_registry() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("vanished.sqlite");
        let connection =
            TrackedConnection::open(&path, SqliteStore::BlobStore).expect("open tracked");
        connection
            .execute_batch("CREATE TABLE t(id INTEGER PRIMARY KEY);")
            .expect("seed");
        std::fs::remove_file(&path).expect("remove the database file");
        drop(connection);
        assert_eq!(
            open_connections(&path),
            0,
            "the close must be filed under the key the open used"
        );
    }
}
