//! In-process ledger projections, with file signatures for other-process writes.
use super::{PathStatusStore, PathStatusSummary};
use parking_lot::Mutex;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock, Weak};
use std::time::{Duration, Instant, SystemTime};

type Files = [Option<(u64, Option<SystemTime>)>; 2];

fn files(path: &Path) -> Files {
    let metadata = |path: &Path| {
        std::fs::metadata(path)
            .ok()
            .map(|metadata| (metadata.len(), metadata.modified().ok()))
    };
    [metadata(path), metadata(&path.with_extension("sqlite-wal"))]
}

#[derive(Debug)]
struct Projection {
    revision: u64,
    files: Option<Files>,
    read_at: Instant,
    summary: Option<Arc<PathStatusSummary>>,
}

#[derive(Debug)]
struct Reader {
    identity: Option<crate::db::file_identity::FileIdentity>,
    store: PathStatusStore,
}

#[derive(Debug)]
pub(crate) struct Health {
    path: PathBuf,
    projection: Mutex<Projection>,
    reader: Mutex<Option<Reader>>,
}

type Registry = Mutex<HashMap<PathBuf, Weak<Health>>>;
static REGISTRY: OnceLock<Registry> = OnceLock::new();

impl Health {
    pub(crate) fn for_view(view_dir: &Path) -> Arc<Self> {
        Self::for_path(&view_dir.join("derived.sqlite"))
    }

    pub(super) fn for_path(path: &Path) -> Arc<Self> {
        let mut registry = REGISTRY.get_or_init(Default::default).lock();
        if let Some(health) = registry.get(path).and_then(Weak::upgrade) {
            return health;
        }
        let health = Arc::new(Self {
            path: path.to_path_buf(),
            projection: Mutex::new(Projection {
                revision: 0,
                files: None,
                read_at: Instant::now(),
                summary: None,
            }),
            reader: Mutex::new(None),
        });
        registry.insert(path.to_path_buf(), Arc::downgrade(&health));
        health
    }

    pub(super) fn existing(path: &Path) -> Option<Arc<Self>> {
        REGISTRY
            .get_or_init(Default::default)
            .lock()
            .get(path)
            .and_then(Weak::upgrade)
    }

    /// Mark the projection dirty after a successful commit. Writers do no
    /// health queries; the next snapshot coalesces all intervening batches.
    pub(super) fn committed(&self) {
        let mut projection = self.projection.lock();
        projection.revision = projection.revision.wrapping_add(1);
        projection.files = None;
    }

    pub(crate) fn snapshot(&self) -> (u64, Option<Arc<PathStatusSummary>>) {
        let current_files = files(&self.path);
        let revision = {
            let projection = self.projection.lock();
            if projection.files == Some(current_files)
                && projection.read_at.elapsed() < Duration::from_secs(30)
            {
                return (projection.revision, projection.summary.clone());
            }
            projection.revision
        };
        // Another process can write the ledger or remove its view directory.
        // Re-read unknown signatures next tick, with a 30-second backstop for
        // changes that leave file sizes and modification times unchanged.
        #[cfg(test)]
        super::HEALTH_READS.with(|reads| reads.set(reads.get() + 1));
        let summary = {
            let mut reader = self.reader.lock();
            let identity = crate::db::file_identity::identity_of(&self.path);
            if reader
                .as_ref()
                .is_some_and(|reader| reader.identity != identity)
                || current_files[0].is_none()
            {
                // A removed or replaced store must not leave us querying the
                // old inode through a still-open SQLite connection.
                *reader = None;
            }
            if reader.is_none() {
                *reader = PathStatusStore::open_at(&self.path).ok().and_then(|store| {
                    store
                        .connection
                        .pragma_update(None, "query_only", true)
                        .ok()?;
                    Some(Reader {
                        identity: crate::db::file_identity::identity_of(&self.path),
                        store,
                    })
                });
            }
            // health_summary commits (or rolls back) its read transaction
            // before returning. An idle reader must never pin the WAL.
            reader
                .as_ref()
                .and_then(|reader| reader.store.health_summary().ok())
                .map(Arc::new)
        };
        let after = files(&self.path);
        let mut projection = self.projection.lock();
        if projection.revision != revision {
            // A concurrent commit wins. Do not label the older read with the
            // writer's version: it must be reconsidered on the next tick.
            return (revision, summary);
        }
        projection.revision = projection.revision.wrapping_add(1);
        projection.files = (summary.is_some() && current_files == after).then_some(after);
        projection.read_at = Instant::now();
        projection.summary = summary.clone();
        (projection.revision, summary)
    }
}

impl Drop for Health {
    fn drop(&mut self) {
        let mut registry = REGISTRY.get_or_init(Default::default).lock();
        if registry
            .get(&self.path)
            .is_some_and(|entry| std::ptr::eq(entry.as_ptr(), self))
        {
            registry.remove(&self.path);
        }
    }
}

#[cfg(all(test, target_os = "linux"))]
#[path = "health_bench.rs"]
mod bench;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::path_status::{SkipReason, StatusIntent};
    use std::collections::{BTreeMap, BTreeSet, HashSet};

    #[test]
    fn one_connection_per_store_across_dirty_ticks() {
        let dir = tempfile::tempdir().unwrap();
        let health = Health::for_view(dir.path());
        let mut writer = PathStatusStore::open(dir.path()).unwrap();
        let opens = super::super::STORE_OPENS.with(std::cell::Cell::get);
        for tick in 1..=5 {
            writer
                .mark_pending(format!("path-{tick}").as_bytes(), "waiting", tick)
                .unwrap();
            assert_eq!(health.snapshot().1.unwrap().pending_count, tick as usize);
        }
        assert_eq!(
            super::super::STORE_OPENS.with(std::cell::Cell::get) - opens,
            1,
            "dirty ticks must share one health connection, not reopen the store"
        );
    }

    #[test]
    fn connected_health_reader_does_not_pin_wal_checkpoints() {
        let dir = tempfile::tempdir().unwrap();
        let health = Health::for_view(dir.path());
        let mut writer = PathStatusStore::open(dir.path()).unwrap();
        health.snapshot();
        let opens = super::super::STORE_OPENS.with(std::cell::Cell::get);
        for tick in 1..=3 {
            writer
                .mark_pending(b"a", format!("tick-{tick}"), tick)
                .unwrap();
            assert_eq!(health.snapshot().1.unwrap().pending_count, 1);
            assert!(health
                .reader
                .lock()
                .as_ref()
                .unwrap()
                .store
                .connection
                .is_autocommit());
            assert_eq!(crate::db::file_identity::open_connections(writer.path()), 2);
            let checkpoint: (i64, i64, i64) = writer
                .connection
                .query_row("PRAGMA wal_checkpoint(PASSIVE)", [], |row| {
                    Ok((row.get(0)?, row.get(1)?, row.get(2)?))
                })
                .unwrap();
            assert_eq!(checkpoint.0, 0);
            assert!(
                checkpoint.1 > 0,
                "the writer must have WAL frames to checkpoint"
            );
            assert_eq!(checkpoint.1, checkpoint.2, "reader pinned WAL frames");
            let truncated: (i64, i64, i64) = writer
                .connection
                .query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |row| {
                    Ok((row.get(0)?, row.get(1)?, row.get(2)?))
                })
                .unwrap();
            assert_eq!(truncated, (0, 0, 0));
            assert_eq!(
                std::fs::metadata(dir.path().join("derived.sqlite-wal"))
                    .unwrap()
                    .len(),
                0
            );
        }
        assert_eq!(super::super::STORE_OPENS.with(std::cell::Cell::get), opens);
    }

    #[test]
    fn connected_health_reader_allows_autocheckpoint_and_wal_reset() {
        let dir = tempfile::tempdir().unwrap();
        let health = Health::for_view(dir.path());
        let mut writer = PathStatusStore::open(dir.path()).unwrap();
        assert_eq!(health.snapshot().1.unwrap().pending_count, 0);
        let opens = super::super::STORE_OPENS.with(std::cell::Cell::get);
        let threshold: u64 = writer
            .connection
            .pragma_query_value(None, "wal_autocheckpoint", |row| row.get(0))
            .unwrap();
        let page_size: u64 = writer
            .connection
            .pragma_query_value(None, "page_size", |row| row.get(0))
            .unwrap();
        assert_eq!(threshold, 1000);
        let wal = dir.path().join("derived.sqlite-wal");
        writer
            .mark_pending(b"large", "x".repeat(5 * 1024 * 1024), 1)
            .unwrap();
        let high_water = std::fs::metadata(&wal).unwrap().len();
        assert!(
            high_water > threshold * page_size,
            "the commit must exceed the production autocheckpoint threshold"
        );
        // A successful autocheckpoint makes the next write reset the WAL.
        // No explicit checkpoint runs here: an idle reader must allow the
        // writer's normal threshold and journal-size policy to reclaim it.
        writer
            .mark_pending(b"small", "after autocheckpoint", 2)
            .unwrap();
        let after_reset = std::fs::metadata(&wal).unwrap().len();
        assert!(
            after_reset <= crate::db::lifecycle::WAL_JOURNAL_SIZE_LIMIT as u64,
            "idle health reader blocked autocheckpoint/WAL reset: {high_water} -> {after_reset}"
        );
        assert_eq!(
            crate::db::file_identity::open_connections(writer.path()),
            2,
            "the health reader must remain connected throughout the writes"
        );
        assert_eq!(super::super::STORE_OPENS.with(std::cell::Cell::get), opens);
        assert_eq!(health.snapshot().1.unwrap().pending_count, 2);
        println!("connected-reader automatic WAL reset: {high_water} -> {after_reset} bytes");
    }

    #[test]
    fn unchanged_publications_do_not_dirty_health() {
        let dir = tempfile::tempdir().unwrap();
        let health = Health::for_view(dir.path());
        let mut writer = PathStatusStore::open(dir.path()).unwrap();
        health.snapshot();
        let reads = super::super::HEALTH_READS.with(std::cell::Cell::get);
        for generation in 1..=5 {
            assert_eq!(
                writer
                    .settle(&StatusIntent {
                        generation,
                        marked_through: writer.mark_epoch().unwrap(),
                        members: HashSet::new(),
                        keep_pending: BTreeSet::new(),
                        failed: BTreeMap::new(),
                        fresh_skips: BTreeMap::new(),
                    })
                    .unwrap(),
                0
            );
            assert_eq!(health.snapshot().1.unwrap().pending_count, 0);
        }
        assert_eq!(super::super::HEALTH_READS.with(std::cell::Cell::get), reads);
    }

    #[test]
    fn writer_opened_before_runtime_still_invalidates_health() {
        let dir = tempfile::tempdir().unwrap();
        let mut writer = PathStatusStore::open(dir.path()).unwrap();
        let health = Health::for_view(dir.path());
        health.snapshot();
        writer.mark_pending(b"a", "waiting", 1).unwrap();
        assert!(health.projection.lock().files.is_none());
        assert_eq!(health.snapshot().1.unwrap().pending_count, 1);
    }

    #[cfg(unix)]
    #[test]
    fn cached_reader_refreshes_after_store_file_identity_changes() {
        let dir = tempfile::tempdir().unwrap();
        let view = dir.path().join("view");
        let replacement = dir.path().join("replacement");
        let health = Health::for_view(&view);
        let mut writer = PathStatusStore::open(&view).unwrap();
        writer.mark_pending(b"old", "waiting", 1).unwrap();
        let path = writer.path().to_path_buf();
        assert_eq!(health.snapshot().1.unwrap().pending_count, 1);
        drop(writer);
        assert_eq!(crate::db::file_identity::open_connections(&path), 1);
        let old_identity = crate::db::file_identity::identity_of(&path).unwrap();
        let old_files = files(&path);

        let mut replacement_writer = PathStatusStore::open(&replacement).unwrap();
        replacement_writer
            .mark_failed(b"new-a", "failed", 2)
            .unwrap();
        replacement_writer
            .mark_failed(b"new-b", "failed", 2)
            .unwrap();
        let replacement_path = replacement_writer.path().to_path_buf();
        drop(replacement_writer);
        // Move the complete closed replacement store, including its committed
        // WAL. The cached reader remains attached to the old, unlinked files.
        for extension in ["sqlite", "sqlite-wal", "sqlite-shm"] {
            std::fs::rename(
                replacement_path.with_extension(extension),
                path.with_extension(extension),
            )
            .unwrap();
        }
        assert_ne!(
            crate::db::file_identity::identity_of(&path),
            Some(old_identity)
        );
        assert_ne!(
            files(&path),
            old_files,
            "replacement must invalidate the stat signature"
        );
        let next = health.snapshot().1.unwrap();
        assert_eq!(next.pending_count, 0, "cached reader served the old inode");
        assert_eq!(next.failed_count, 2);
        assert_eq!(crate::db::file_identity::open_connections(&path), 1);
    }

    #[cfg(unix)]
    #[test]
    fn cached_reader_discards_summary_after_store_files_are_removed() {
        let dir = tempfile::tempdir().unwrap();
        let health = Health::for_view(dir.path());
        let mut writer = PathStatusStore::open(dir.path()).unwrap();
        writer.mark_pending(b"old", "waiting", 1).unwrap();
        let path = writer.path().to_path_buf();
        assert_eq!(health.snapshot().1.unwrap().pending_count, 1);
        drop(writer);
        assert_eq!(crate::db::file_identity::open_connections(&path), 1);
        // Model an unavailable identity so only the missing-file guard can
        // retire the cached reader, rather than an identity mismatch.
        health.reader.lock().as_mut().unwrap().identity = None;
        for extension in ["sqlite", "sqlite-wal", "sqlite-shm"] {
            std::fs::remove_file(path.with_extension(extension)).unwrap();
        }
        assert!(files(&path)[0].is_none());
        // Opening an absent ledger bootstraps an empty store; its previous
        // annotations must not survive through the old connection or cache.
        let next = health.snapshot().1.unwrap();
        assert_eq!(next.pending_count, 0);
        assert_eq!(next.failed_count, 0);
        assert_eq!(crate::db::file_identity::open_connections(&path), 1);
    }

    #[test]
    fn local_batches_defer_summary_until_next_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        let health = Health::for_view(dir.path());
        let mut writer = PathStatusStore::open(dir.path()).unwrap();
        health.snapshot();
        let scans = super::super::HEALTH_SUMMARY_READS.with(std::cell::Cell::get);
        let reads = super::super::HEALTH_READS.with(std::cell::Cell::get);
        for _ in 0..20 {
            writer.mark_pending(b"a", "waiting", 1).unwrap();
        }
        writer.mark_failed(b"a", "missing payload", 2).unwrap();
        assert_eq!(
            super::super::HEALTH_SUMMARY_READS.with(std::cell::Cell::get),
            scans,
            "committed batches must not scan health tables on the writer"
        );
        assert_eq!(super::super::HEALTH_READS.with(std::cell::Cell::get), reads);
        let next = health.snapshot().1.unwrap();
        assert_eq!(next.pending_count, 0);
        assert_eq!(next.failed_count, 1);
        assert_eq!(
            super::super::HEALTH_SUMMARY_READS.with(std::cell::Cell::get),
            scans + 1,
            "all committed batches must share one health refresh"
        );
        assert_eq!(
            super::super::HEALTH_READS.with(std::cell::Cell::get),
            reads + 1
        );
        assert_eq!(health.snapshot().1.unwrap().failed_count, 1);
        assert_eq!(
            super::super::HEALTH_SUMMARY_READS.with(std::cell::Cell::get),
            scans + 1,
            "an unchanged snapshot must reuse the summary"
        );
    }

    #[test]
    fn local_writers_refresh_pending_failed_clear_and_settled_skips() {
        let dir = tempfile::tempdir().unwrap();
        let health = Health::for_view(dir.path());
        health.snapshot();
        let reads = super::super::HEALTH_READS.with(std::cell::Cell::get);
        let mut writer = PathStatusStore::open(dir.path()).unwrap();
        writer.mark_pending(b"a", "waiting", 1).unwrap();
        assert_eq!(health.snapshot().1.unwrap().pending_count, 1);
        writer
            .mark_pending_many([(&b"b"[..], "waiting")], 1)
            .unwrap();
        assert_eq!(health.snapshot().1.unwrap().pending_count, 2);
        writer.mark_failed(b"a", "missing payload", 1).unwrap();
        assert_eq!(health.snapshot().1.unwrap().failed_count, 1);
        writer.clear(b"a").unwrap();
        assert_eq!(health.snapshot().1.unwrap().failed_count, 0);
        writer.clear_many([&b"b"[..]]).unwrap();
        assert_eq!(health.snapshot().1.unwrap().pending_count, 0);
        writer
            .settle(&StatusIntent {
                generation: 2,
                marked_through: writer.mark_epoch().unwrap(),
                members: HashSet::from([b"c".to_vec()]),
                keep_pending: BTreeSet::new(),
                failed: BTreeMap::new(),
                fresh_skips: BTreeMap::from([(
                    b"c".to_vec(),
                    Some(SkipReason::UnsupportedGitMode),
                )]),
            })
            .unwrap();
        let settled = health.snapshot().1.unwrap();
        assert_eq!(
            settled.skipped_by_reason.get("unsupported_git_mode"),
            Some(&1)
        );
        assert_eq!(settled.skipped_sample[0].rel_path, b"c");
        drop(writer);
        assert_eq!(
            health.snapshot().1.unwrap().skipped_by_reason,
            settled.skipped_by_reason
        );
        assert_eq!(
            super::super::HEALTH_READS.with(std::cell::Cell::get),
            reads + 6
        );
    }

    #[test]
    fn failed_refresh_retries_on_next_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        let health = Health::for_view(dir.path());
        let writer = PathStatusStore::open(dir.path()).unwrap();
        assert!(health.snapshot().1.is_some());
        writer
            .connection
            .execute_batch("ALTER TABLE path_status RENAME COLUMN state TO broken_state")
            .unwrap();
        let reads = super::super::HEALTH_READS.with(std::cell::Cell::get);
        assert!(health.snapshot().1.is_none());
        assert!(health.snapshot().1.is_none());
        assert_eq!(
            super::super::HEALTH_READS.with(std::cell::Cell::get),
            reads + 2,
            "failed refreshes must remain dirty rather than wait for the 30-second backstop"
        );
        writer
            .connection
            .execute_batch("ALTER TABLE path_status RENAME COLUMN broken_state TO state")
            .unwrap();
        assert!(health.snapshot().1.is_some());
    }

    #[test]
    fn unknown_write_before_local_close_and_backstop_force_disk_reads() {
        let dir = tempfile::tempdir().unwrap();
        let health = Health::for_view(dir.path());
        let mut writer = PathStatusStore::open(dir.path()).unwrap();
        writer.mark_pending(b"a", "waiting", 1).unwrap();
        assert_eq!(health.snapshot().1.unwrap().pending_count, 1);
        let cached_files = health.projection.lock().files;
        let before = super::super::HEALTH_READS.with(std::cell::Cell::get);
        // Bypass the in-process publisher, as an external process does.
        let external = rusqlite::Connection::open(writer.path()).unwrap();
        external
            .execute("UPDATE path_status SET state = 'failed'", [])
            .unwrap();
        drop(external);
        drop(writer);
        assert_eq!(
            health.projection.lock().files,
            cached_files,
            "closing must never bless an external signature"
        );
        assert_eq!(health.snapshot().1.unwrap().failed_count, 1);
        assert_eq!(
            super::super::HEALTH_READS.with(std::cell::Cell::get),
            before + 1
        );
        health.projection.lock().read_at = Instant::now() - Duration::from_secs(31);
        assert_eq!(health.snapshot().1.unwrap().failed_count, 1);
        assert_eq!(
            super::super::HEALTH_READS.with(std::cell::Cell::get),
            before + 2
        );
    }

    #[test]
    fn last_local_close_preserves_signature_and_wal_threshold() {
        let dir = tempfile::tempdir().unwrap();
        let health = Health::for_view(dir.path());
        let mut writer = PathStatusStore::open(dir.path()).unwrap();
        writer.mark_pending(b"a", "waiting", 1).unwrap();
        let before = files(writer.path());
        assert!(before[1].is_some(), "committed frames remain in the WAL");
        assert!(writer
            .connection
            .db_config(rusqlite::config::DbConfig::SQLITE_DBCONFIG_NO_CKPT_ON_CLOSE)
            .unwrap());
        let threshold: u64 = writer
            .connection
            .pragma_query_value(None, "wal_autocheckpoint", |row| row.get(0))
            .unwrap();
        assert_eq!(threshold, 1000);
        drop(writer);
        assert_eq!(
            files(&health.path),
            before,
            "local close must not change health signatures"
        );
        assert_eq!(health.snapshot().1.unwrap().pending_count, 1);
        let reopened = PathStatusStore::open(dir.path()).unwrap();
        assert_eq!(
            reopened.summary().unwrap().pending_count,
            1,
            "committed WAL survives close/reopen"
        );
    }

    #[test]
    fn wal_reset_reclaims_closed_store_high_water() {
        let dir = tempfile::tempdir().unwrap();
        let wal = dir.path().join("derived.sqlite-wal");
        let configured: i64;
        {
            let mut writer = PathStatusStore::open(dir.path()).unwrap();
            configured = writer
                .connection
                .pragma_query_value(None, "journal_size_limit", |row| row.get(0))
                .unwrap();
            // Exceed the normal 1,000-page autocheckpoint in one commit. The
            // allocation remains after close until a subsequent WAL reset.
            writer
                .mark_pending(b"large", "x".repeat(5 * 1024 * 1024), 1)
                .unwrap();
        }
        let high_water = std::fs::metadata(&wal).unwrap().len();
        assert!(high_water > crate::db::lifecycle::WAL_JOURNAL_SIZE_LIMIT as u64);
        {
            let mut writer = PathStatusStore::open(dir.path()).unwrap();
            // Reopening can rebuild the WAL index. Establish a completed
            // checkpoint on this connection before testing its next reset.
            let checkpoint: (i64, i64, i64) = writer
                .connection
                .query_row("PRAGMA wal_checkpoint(PASSIVE)", [], |row| {
                    Ok((row.get(0)?, row.get(1)?, row.get(2)?))
                })
                .unwrap();
            assert_eq!(checkpoint.0, 0);
            assert_eq!(checkpoint.1, checkpoint.2);
            writer
                .mark_pending(b"small", "after checkpoint", 2)
                .unwrap();
        }
        let after_reset = std::fs::metadata(&wal).unwrap().len();
        assert!(
            after_reset <= crate::db::lifecycle::WAL_JOURNAL_SIZE_LIMIT as u64,
            "checkpointed WAL high-water allocation was retained: {high_water} -> {after_reset}"
        );
        assert_eq!(configured, crate::db::lifecycle::WAL_JOURNAL_SIZE_LIMIT);
        assert_eq!(
            PathStatusStore::open(dir.path())
                .unwrap()
                .summary()
                .unwrap()
                .pending_count,
            2
        );
        println!("closed path-status WAL reset: {high_water} -> {after_reset} bytes");
    }
}
