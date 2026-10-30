//! Machine-local source stat hints for avoiding unchanged-file content reads.

use super::*;
use crate::file_identity::FileIdentity;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct SourceStat {
    pub size: u64,
    pub mtime_ns: i64,
    pub identity: Option<String>,
}

impl SourceStat {
    fn from_metadata(metadata: &std::fs::Metadata, identity: FileIdentity) -> Self {
        let modified = metadata.modified().ok();
        let identity = match (identity, modified) {
            (FileIdentity::Known { volume, index }, Some(time))
                if ns_to_system_time(system_time_to_ns(time)) == time =>
            {
                Some(format!("{volume}:{index}"))
            }
            _ => None,
        };
        Self {
            size: metadata.len(),
            mtime_ns: system_time_to_ns(modified.unwrap_or(UNIX_EPOCH)),
            identity,
        }
    }

    pub(super) fn at(path: &Path) -> std::io::Result<Self> {
        let (metadata, identity) = crate::file_identity::metadata(path)?;
        Ok(Self::from_metadata(&metadata, identity))
    }

    fn of_file(file: &std::fs::File) -> std::io::Result<Self> {
        let metadata = file.metadata()?;
        #[cfg(unix)]
        let identity = FileIdentity::of_unix_metadata(&metadata);
        #[cfg(not(unix))]
        let identity = FileIdentity::of_file(file);
        Ok(Self::from_metadata(&metadata, identity))
    }

    pub(super) fn matches(&self, freshness: &FileFreshness, identity: Option<&str>) -> bool {
        // Stat-first verification cannot detect an in-place rewrite that keeps
        // size, mtime and inode/file index unchanged (for example, dd with
        // conv=notrunc followed by touch -r). Atomic replacements have a new
        // identity even when their size and timestamps are preserved.
        self.identity.is_some()
            && self.identity.as_deref() == identity
            && self.size == freshness.size
            && self.mtime_ns == system_time_to_ns(freshness.mtime)
    }
}

pub(super) fn read_source(path: &Path) -> std::io::Result<(String, FileFreshness, Option<String>)> {
    read_open_source(crate::project_content::open(path)?)
}

fn read_open_source(
    mut file: std::fs::File,
) -> std::io::Result<(String, FileFreshness, Option<String>)> {
    let before = SourceStat::of_file(&file)?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    crate::views::io::WORK
        .content_read_bytes
        .fetch_add(bytes.len() as u64, AtomicOrdering::Relaxed);
    let after = SourceStat::of_file(&file)?;
    let content_hash = if bytes.len() as u64 > cache_freshness::CONTENT_HASH_SIZE_CAP {
        cache_freshness::zero_hash()
    } else {
        cache_freshness::hash_bytes(&bytes)
    };
    let source = String::from_utf8(bytes)
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
    // A path stat after reading could identify a replacement, not the file we
    // parsed. Use the same open handle, and leave the hint absent if an in-place
    // edit changed its stamp during the read.
    let identity = (before == after && after.size == source.len() as u64)
        .then_some(after.identity)
        .flatten();
    let freshness = FileFreshness {
        size: source.len() as u64,
        mtime: ns_to_system_time(after.mtime_ns),
        content_hash,
    };
    Ok((source, freshness, identity))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stat_first_unavailable_identity_is_never_unchanged() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("source.ts");
        std::fs::write(&path, "export function source() {}\n").unwrap();
        let (metadata, identity) = crate::file_identity::metadata(&path).unwrap();
        let known = SourceStat::from_metadata(&metadata, identity);
        let unavailable = SourceStat::from_metadata(&metadata, FileIdentity::Unavailable);
        assert!(unavailable.identity.is_none());
        let freshness = FileFreshness {
            size: metadata.len(),
            mtime: metadata.modified().unwrap(),
            content_hash: cache_freshness::hash_bytes(b"source"),
        };
        assert!(!unavailable.matches(&freshness, None));
        assert!(!unavailable.matches(&freshness, known.identity.as_deref()));
        assert!(!known.matches(&freshness, None));
        let stored = StoredFile {
            size: unavailable.size,
            mtime_ns: unavailable.mtime_ns,
            hash: String::new(),
            identity: None,
        };
        assert!(!stored.matches(&unavailable));
        assert!(!stored.matches(&known));
    }

    #[test]
    #[cfg(any(unix, windows))]
    fn stat_first_extraction_stamps_the_opened_file_not_a_replacement() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("source.ts");
        let source = "export function before() {}\n";
        std::fs::write(&path, source).unwrap();
        let opened = crate::project_content::open(&path).unwrap();
        crate::file_identity::replace_keeping_times(
            &path,
            source.replace("before", "after_").as_bytes(),
        );
        let (read, freshness, identity) = read_open_source(opened).unwrap();
        assert_eq!(read, source);
        assert_eq!(
            freshness.content_hash,
            cache_freshness::hash_bytes(source.as_bytes())
        );
        assert!(identity.is_some());
        assert!(!SourceStat::at(&path)
            .unwrap()
            .matches(&freshness, identity.as_deref()));
    }
}

pub(super) fn has_identity_column(conn: &Connection) -> Result<bool> {
    let mut statement = conn.prepare("PRAGMA table_info(files)")?;
    let columns = statement.query_map([], |row| row.get::<_, String>(1))?;
    for column in columns {
        if column? == "source_identity" {
            return Ok(true);
        }
    }
    Ok(false)
}

pub(super) fn ensure_identity_column(conn: &Connection) -> Result<()> {
    if !has_identity_column(conn)? {
        conn.execute_batch("ALTER TABLE files ADD COLUMN source_identity TEXT")?;
    }
    Ok(())
}

pub(super) fn verify_file(path: &Path, row: &FileRow) -> (FreshnessVerdict, Option<String>) {
    let before = match SourceStat::at(path) {
        Ok(stat) => stat,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return (FreshnessVerdict::Deleted, None)
        }
        Err(_) => return (FreshnessVerdict::Stale, None),
    };
    if before.matches(&row.freshness, row.source_identity.as_deref()) {
        return (FreshnessVerdict::HotFresh, before.identity);
    }
    if before.size != row.freshness.size
        || row.freshness.content_hash == cache_freshness::zero_hash()
    {
        return (FreshnessVerdict::Stale, None);
    }
    let hash = match cache_freshness::hash_file_if_small(path, before.size) {
        Ok(Some(hash)) => hash,
        _ => return (FreshnessVerdict::Stale, None),
    };
    if hash != row.freshness.content_hash || SourceStat::at(path).ok().as_ref() != Some(&before) {
        return (FreshnessVerdict::Stale, None);
    }
    (
        FreshnessVerdict::ContentFresh {
            new_mtime: ns_to_system_time(before.mtime_ns),
            new_size: before.size,
        },
        before.identity,
    )
}

#[derive(Clone, Debug)]
pub(super) struct StoredFile {
    pub size: u64,
    pub hash: String,
    pub mtime_ns: i64,
    pub identity: Option<String>,
}

impl StoredFile {
    pub(super) fn matches(&self, stat: &SourceStat) -> bool {
        stat.identity.is_some()
            && stat.identity == self.identity
            && stat.size == self.size
            && stat.mtime_ns == self.mtime_ns
    }
}

pub(super) struct ValidatedStat {
    pub rel_path: String,
    pub previous: StoredFile,
    pub observed: SourceStat,
}

impl CallGraphStore {
    pub(super) fn backfill_source_stats(
        &self,
        revision: Option<u64>,
        updates: &[ValidatedStat],
    ) -> Result<()> {
        if self.legacy_fallback
            || updates.is_empty()
            || !self.is_current()
            || self.sqlite_path.parent() != Some(self.publication_dir.as_path())
        {
            return Ok(());
        }
        // Hints are an optimization, not graph publication. Another process
        // holding the writer lease must not make a verified reader wait
        // indefinitely or force a rebuild.
        let lease = match crate::root_cache::WriterLease::acquire_shared_with_timeout(
            crate::root_cache::RootCacheDomain::Callgraph,
            &self.publication_dir,
            &self.project_key,
            &self.project_root,
            Duration::ZERO,
        ) {
            Ok(Some(lease)) => lease,
            Ok(None) | Err(crate::fs_lock::AcquireError::Timeout) => return Ok(()),
            Err(error) => return Err(error.into()),
        };
        if !lease.verify()? {
            return Ok(());
        }
        let OpenedStore { store, root_repair } = Self::open_at_path_with_root_repair(
            self.project_root.clone(),
            self.project_key.clone(),
            self.sqlite_path.clone(),
            self.generation.clone(),
            WriterConnectionRole::Refresh,
            Some(Arc::clone(&lease)),
            None,
            true,
        )?;
        if matches!(root_repair, OpenRootRepair::NeedsRebuild { .. }) || !store.is_current() {
            return Ok(());
        }
        let mut conn = store.conn.lock().expect("callgraph store mutex poisoned");
        if projection_write_revision(&conn)?.unwrap_or(0) != revision.unwrap_or(0) {
            return Ok(());
        }
        let tx = conn.transaction()?;
        {
            let mut statement = tx.prepare(
                "UPDATE files SET mtime_ns = ?2, size = ?3, source_identity = ?4
                 WHERE path = ?1 AND content_hash = ?5 AND mtime_ns = ?6
                   AND size = ?7 AND source_identity IS ?8",
            )?;
            for update in updates {
                // Do not associate a validated hash with a file replaced after
                // comparison, nor overwrite rows another refresh has changed.
                if SourceStat::at(&self.project_root.join(&update.rel_path))
                    .ok()
                    .as_ref()
                    != Some(&update.observed)
                {
                    continue;
                }
                statement.execute(params![
                    update.rel_path,
                    update.observed.mtime_ns,
                    update.observed.size as i64,
                    update.observed.identity,
                    update.previous.hash,
                    update.previous.mtime_ns,
                    update.previous.size as i64,
                    update.previous.identity,
                ])?;
            }
        }
        if lease.verify()? && store.is_current() {
            tx.commit()?;
        }
        Ok(())
    }
}
