use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

const CONTENT_HASH_SIZE_LIMIT_BYTES: u64 = 4 * 1024 * 1024;

/// Per-document state tracked for LSP synchronization.
///
/// We track `mtime`, `size`, and (for small files) a content hash so we can
/// detect when a file has been changed
/// outside the AFT pipeline (another tool, another session, manual edit) and
/// resync the LSP server before issuing diagnostic requests. Without this,
/// `ensure_file_open` would skip already-open files and return diagnostics
/// computed from stale in-memory content.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DocumentEntry {
    /// Monotonically increasing LSP version, starts at 0.
    pub version: i32,
    /// Filesystem modification time at the moment we last synced this file
    /// to the LSP server (didOpen or didChange). `None` if the file did not
    /// exist on disk when we last synced (e.g. in-memory-only test fixtures
    /// or files mid-rename).
    pub mtime: Option<SystemTime>,
    /// Filesystem byte size at the moment we last synced. `None` for the
    /// same reasons as `mtime`.
    pub size: Option<u64>,
    /// BLAKE3 content hash captured at the last sync for files up to
    /// `CONTENT_HASH_SIZE_LIMIT_BYTES`. Large files skip hashing and rely on
    /// `(mtime, size)` to avoid expensive reads.
    pub content_hash: Option<[u8; 32]>,
}

/// Tracks document state for LSP synchronization.
///
/// LSP requires:
/// 1. didOpen before didChange (document must be opened first)
/// 2. Version numbers must be monotonically increasing
/// 3. Full content sent with each change (TextDocumentSyncKind::Full)
///
/// This store ALSO records (mtime, size, content hash) at sync time so callers can detect
/// disk drift via `is_stale_on_disk()` and resync stale entries before
/// issuing pull-diagnostic requests against potentially stale server state.
#[derive(Debug, Default)]
pub struct DocumentStore {
    entries: HashMap<PathBuf, DocumentEntry>,
}

impl DocumentStore {
    pub fn new() -> Self {
        Self {
            entries: HashMap::new(),
        }
    }

    pub fn estimated_memory(&self) -> crate::memory::MemoryEstimate {
        let bytes = self.entries.keys().fold(0u64, |bytes, path| {
            bytes
                .saturating_add(std::mem::size_of::<PathBuf>() as u64)
                .saturating_add(std::mem::size_of::<DocumentEntry>() as u64)
                .saturating_add(crate::memory::path_bytes(path))
        });
        crate::memory::MemoryEstimate::estimated(bytes).count("documents", self.entries.len())
    }

    /// Check if a document is already opened (tracked).
    pub fn is_open(&self, path: &Path) -> bool {
        self.entries.contains_key(path)
    }

    /// Open a new document, recording the current on-disk metadata/hash. Returns
    /// the initial version (0). If the file's metadata cannot be read, the
    /// document is still tracked but `mtime`/`size` will be `None`, which
    /// causes `is_stale_on_disk()` to conservatively report stale on the
    /// next check (forcing a resync).
    pub fn open(&mut self, path: PathBuf) -> i32 {
        let snapshot = DiskSnapshot::read(&path);
        self.open_with(path, &snapshot)
    }

    /// [`Self::open`] with the file's disk state already read, so a document
    /// opened in several servers reads and hashes the file once.
    pub fn open_with(&mut self, path: PathBuf, snapshot: &DiskSnapshot) -> i32 {
        let entry = DocumentEntry {
            version: 0,
            mtime: snapshot.mtime,
            size: snapshot.size,
            content_hash: snapshot.content_hash(&path),
        };
        self.entries.insert(path, entry);
        0
    }

    /// Bump the version for an already-open document and refresh the
    /// recorded mtime/size/hash from disk (the caller is presumed to be sending
    /// a `didChange` with fresh content right after this call). Returns the
    /// new version, or `None` if the document is not open.
    pub fn bump_version(&mut self, path: &Path) -> Option<i32> {
        let snapshot = DiskSnapshot::read(path);
        self.bump_version_with(path, &snapshot)
    }

    /// [`Self::bump_version`] with the file's disk state already read.
    pub fn bump_version_with(&mut self, path: &Path, snapshot: &DiskSnapshot) -> Option<i32> {
        let content_hash = snapshot.content_hash(path);
        let entry = self.entries.get_mut(path)?;
        entry.version += 1;
        entry.mtime = snapshot.mtime;
        entry.size = snapshot.size;
        entry.content_hash = content_hash;
        Some(entry.version)
    }

    /// Get current version, or None if not open.
    pub fn version(&self, path: &Path) -> Option<i32> {
        self.entries.get(path).map(|e| e.version)
    }

    /// Record an explicit version after startup coalesced several unsent edits.
    pub(crate) fn sync_at_version(&mut self, path: PathBuf, version: i32, snapshot: &DiskSnapshot) {
        self.open_with(path.clone(), snapshot);
        self.entries
            .get_mut(&path)
            .expect("document just inserted")
            .version = version;
    }

    /// Get the full document entry, or None if not open.
    pub fn entry(&self, path: &Path) -> Option<&DocumentEntry> {
        self.entries.get(path)
    }

    /// Close a document and remove from tracking. Returns the last known
    /// version, or `None` if the document was not open.
    pub fn close(&mut self, path: &Path) -> Option<i32> {
        self.entries.remove(path).map(|e| e.version)
    }

    /// Get all open document paths.
    pub fn open_documents(&self) -> Vec<&PathBuf> {
        self.entries.keys().collect()
    }

    /// Returns true if the document is currently open AND its on-disk
    /// metadata differs from what we recorded at the last sync. Use this
    /// before issuing pull diagnostics to decide whether `bump_version` +
    /// `didChange` is needed first.
    ///
    /// Conservative semantics: returns true if the file used to have known
    /// metadata but cannot be read now (e.g. deleted or permission error),
    /// or if we never recorded metadata for the open entry.
    pub fn is_stale_on_disk(&self, path: &Path) -> bool {
        self.is_stale_against(path, &DiskSnapshot::read(path))
    }

    /// [`Self::is_stale_on_disk`] against disk state already read. The file
    /// is hashed only when its modification time and size both match the
    /// last sync: an edit that kept both (a coarse-mtime filesystem, two
    /// writes within one tick) is caught only by its content. A changed
    /// time or size already proves the file changed, so no hash is needed.
    pub fn is_stale_against(&self, path: &Path, snapshot: &DiskSnapshot) -> bool {
        let Some(entry) = self.entries.get(path) else {
            // Not open at all — caller should `open` instead of asking about
            // staleness. We still return true so the caller doesn't act on
            // stale assumptions.
            return true;
        };
        let current_mtime = snapshot.mtime;
        let current_size = snapshot.size;

        match (entry.mtime, current_mtime) {
            (Some(prev), Some(now)) if prev == now => {
                if entry.size != current_size {
                    return true;
                }

                match current_size {
                    Some(size) if size > CONTENT_HASH_SIZE_LIMIT_BYTES => false,
                    Some(_) => match (entry.content_hash, snapshot.content_hash(path)) {
                        (Some(prev_hash), Some(now_hash)) => prev_hash != now_hash,
                        _ => true,
                    },
                    None => true,
                }
            }
            (Some(_), Some(_)) => true, // mtimes differ
            // Either we didn't record before or can't read now — be safe.
            _ => true,
        }
    }
}

/// One file's disk state: metadata read at once, and for small files a
/// BLAKE3 content hash read on first use and then shared, so every server's
/// store and every staleness check in one operation reads the file once.
/// Metadata fields are `None` if the path cannot be statted or if the
/// platform doesn't support the queried metadata (rare). The hash is `None`
/// for unreadable files and files above `CONTENT_HASH_SIZE_LIMIT_BYTES`.
#[derive(Debug, Default)]
pub struct DiskSnapshot {
    mtime: Option<SystemTime>,
    size: Option<u64>,
    content_hash: std::cell::OnceCell<Option<[u8; 32]>>,
}

impl DiskSnapshot {
    pub fn read(path: &Path) -> Self {
        match std::fs::metadata(path) {
            Ok(meta) => Self {
                mtime: meta.modified().ok(),
                size: Some(meta.len()),
                content_hash: std::cell::OnceCell::new(),
            },
            Err(_) => Self::default(),
        }
    }

    fn content_hash(&self, path: &Path) -> Option<[u8; 32]> {
        *self.content_hash.get_or_init(|| {
            let size = self.size?;
            if size > CONTENT_HASH_SIZE_LIMIT_BYTES {
                return None;
            }
            #[cfg(test)]
            CONTENT_READS.with(|reads| reads.set(reads.get() + 1));
            std::fs::read(path)
                .ok()
                .map(|bytes| *blake3::hash(&bytes).as_bytes())
        })
    }
}

#[cfg(test)]
thread_local! {
    /// Files read for a content hash on this thread.
    pub(crate) static CONTENT_READS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// Public helper: read metadata for an arbitrary path. Useful for callers
/// that want to consult disk state without going through the store.
pub fn file_metadata(path: &Path) -> io::Result<(SystemTime, u64)> {
    let meta = std::fs::metadata(path)?;
    let mtime = meta.modified()?;
    let size = meta.len();
    Ok((mtime, size))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::io::Write;
    use std::time::Duration;

    fn write_file(path: &Path, content: &str) {
        let mut f = fs::File::create(path).expect("create test file");
        f.write_all(content.as_bytes()).expect("write content");
    }

    fn advance_mtime(path: &Path, duration: Duration) {
        let modified = fs::metadata(path)
            .expect("stat test file")
            .modified()
            .expect("test file mtime");
        let advanced = modified.checked_add(duration).expect("advanced mtime");
        filetime::set_file_mtime(path, filetime::FileTime::from_system_time(advanced))
            .expect("set advanced mtime");
    }

    fn content_reads() -> usize {
        CONTENT_READS.with(std::cell::Cell::get)
    }

    #[test]
    fn a_changed_modification_time_proves_staleness_without_reading_the_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("a.rs");
        write_file(&path, "fn a() {}\n");
        let mut store = DocumentStore::new();
        store.open(path.clone());
        advance_mtime(&path, Duration::from_secs(5));

        let before = content_reads();
        assert!(store.is_stale_on_disk(&path));
        assert_eq!(content_reads() - before, 0, "a changed mtime needs no hash");
    }

    #[test]
    fn one_snapshot_hashes_the_file_once_for_every_store() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("a.rs");
        write_file(&path, "fn a() {}\n");
        let mut stores = [
            DocumentStore::new(),
            DocumentStore::new(),
            DocumentStore::new(),
        ];

        let before = content_reads();
        let snapshot = DiskSnapshot::read(&path);
        for store in &mut stores {
            store.open_with(path.clone(), &snapshot);
        }
        for store in &stores {
            assert!(!store.is_stale_against(&path, &snapshot));
        }
        assert_eq!(content_reads() - before, 1, "three stores, one read");
    }

    #[test]
    fn document_memory_estimate_is_zero_when_empty_and_nonzero_when_populated() {
        let mut store = DocumentStore::new();
        assert_eq!(store.estimated_memory().estimated_bytes, Some(0));
        store.open(PathBuf::from("/tmp/aft-document-memory"));
        let estimate = store.estimated_memory();
        assert!(estimate.estimated_bytes.unwrap() > 0);
        assert_eq!(estimate.counts["documents"], 1);
    }

    #[test]
    fn open_and_close_roundtrip() {
        let mut store = DocumentStore::new();
        let path = PathBuf::from("/tmp/aft-doc-test-doesnt-exist");
        assert!(!store.is_open(&path));

        let v = store.open(path.clone());
        assert_eq!(v, 0);
        assert!(store.is_open(&path));
        assert_eq!(store.version(&path), Some(0));

        let bumped = store.bump_version(&path);
        assert_eq!(bumped, Some(1));
        assert_eq!(store.version(&path), Some(1));

        let closed = store.close(&path);
        assert_eq!(closed, Some(1));
        assert!(!store.is_open(&path));
    }

    #[test]
    fn nonexistent_path_is_always_stale() {
        let store = DocumentStore::new();
        let path = PathBuf::from("/tmp/aft-doc-test-never-opened");
        // Not open at all → stale
        assert!(store.is_stale_on_disk(&path));
    }

    #[test]
    fn freshly_opened_real_file_is_not_stale() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("a.txt");
        write_file(&path, "hello");

        let mut store = DocumentStore::new();
        store.open(path.clone());
        assert!(!store.is_stale_on_disk(&path));
    }

    #[test]
    fn opened_then_disk_changed_is_stale() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("b.txt");
        write_file(&path, "hello");

        let mut store = DocumentStore::new();
        store.open(path.clone());
        assert!(!store.is_stale_on_disk(&path));

        write_file(&path, "hello world!");
        advance_mtime(&path, Duration::from_secs(2));

        assert!(store.is_stale_on_disk(&path));
    }

    #[test]
    fn opened_file_same_size_and_mtime_but_different_hash_is_stale() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("same-size.txt");
        write_file(&path, "hello");
        let original_mtime = fs::metadata(&path)
            .expect("stat original")
            .modified()
            .expect("original mtime");

        let mut store = DocumentStore::new();
        store.open(path.clone());
        assert!(!store.is_stale_on_disk(&path));

        write_file(&path, "jello");
        filetime::set_file_mtime(&path, filetime::FileTime::from_system_time(original_mtime))
            .expect("restore original mtime");

        assert!(
            store.is_stale_on_disk(&path),
            "same mtime+size should still be stale when content hash differs"
        );

        store.bump_version(&path);
        assert!(!store.is_stale_on_disk(&path));
    }

    #[test]
    fn opened_file_then_deleted_is_stale() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("c.txt");
        write_file(&path, "data");

        let mut store = DocumentStore::new();
        store.open(path.clone());
        assert!(!store.is_stale_on_disk(&path));

        fs::remove_file(&path).expect("remove file");
        // Cannot read metadata anymore → conservatively stale
        assert!(store.is_stale_on_disk(&path));
    }

    #[test]
    fn bump_version_refreshes_mtime() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("d.txt");
        write_file(&path, "original");

        let mut store = DocumentStore::new();
        store.open(path.clone());

        write_file(&path, "updated");
        advance_mtime(&path, Duration::from_secs(2));
        assert!(store.is_stale_on_disk(&path));

        // After bump_version, the entry should pick up the new mtime/size
        // (the caller is presumed to send didChange with the fresh content).
        store.bump_version(&path);
        assert!(!store.is_stale_on_disk(&path));
    }

    #[test]
    fn open_documents_returns_all_paths() {
        let mut store = DocumentStore::new();
        store.open(PathBuf::from("/tmp/p1"));
        store.open(PathBuf::from("/tmp/p2"));
        let docs = store.open_documents();
        assert_eq!(docs.len(), 2);
    }

    #[test]
    fn entry_returns_full_state() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("e.txt");
        write_file(&path, "abc");

        let mut store = DocumentStore::new();
        store.open(path.clone());

        let entry = store.entry(&path).expect("entry");
        assert_eq!(entry.version, 0);
        assert!(entry.mtime.is_some());
        assert_eq!(entry.size, Some(3));
        assert!(entry.content_hash.is_some());
    }
}
