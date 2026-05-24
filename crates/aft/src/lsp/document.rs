use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// Per-document state tracked for LSP synchronization.
///
/// We track `mtime` and `size` so we can detect when a file has been changed
/// outside the AFT pipeline (another tool, another session, manual edit) and
/// resync the LSP server before issuing diagnostic requests. Without this,
/// `ensure_file_open` would skip already-open files and return diagnostics
/// computed from stale in-memory content.
#[derive(Debug, Clone)]
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
}

/// Tracks document state for LSP synchronization.
///
/// LSP requires:
/// 1. didOpen before didChange (document must be opened first)
/// 2. Version numbers must be monotonically increasing
/// 3. Full content sent with each change (TextDocumentSyncKind::Full)
///
/// This store ALSO records (mtime, size) at sync time so callers can detect
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

    /// Check if a document is already opened (tracked).
    pub fn is_open(&self, path: &Path) -> bool {
        self.entries.contains_key(path)
    }

    /// Open a new document, recording the current on-disk metadata. Returns
    /// the initial version (0). If the file's metadata cannot be read, the
    /// document is still tracked but `mtime`/`size` will be `None`, which
    /// causes `is_stale_on_disk()` to conservatively report stale on the
    /// next check (forcing a resync).
    pub fn open(&mut self, path: PathBuf) -> i32 {
        let (mtime, size) = read_metadata(&path);
        let entry = DocumentEntry {
            version: 0,
            mtime,
            size,
        };
        self.entries.insert(path, entry);
        0
    }

    /// Bump the version for an already-open document and refresh the
    /// recorded mtime/size from disk (the caller is presumed to be sending
    /// a `didChange` with fresh content right after this call). Returns the
    /// new version, or `None` if the document is not open.
    pub fn bump_version(&mut self, path: &Path) -> Option<i32> {
        let (new_mtime, new_size) = read_metadata(path);
        let entry = self.entries.get_mut(path)?;
        entry.version += 1;
        entry.mtime = new_mtime;
        entry.size = new_size;
        Some(entry.version)
    }

    /// Get current version, or None if not open.
    pub fn version(&self, path: &Path) -> Option<i32> {
        self.entries.get(path).map(|e| e.version)
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
        let Some(entry) = self.entries.get(path) else {
            // Not open at all — caller should `open` instead of asking about
            // staleness. We still return true so the caller doesn't act on
            // stale assumptions.
            return true;
        };
        let (current_mtime, current_size) = read_metadata(path);

        match (entry.mtime, current_mtime) {
            (Some(prev), Some(now)) if prev == now => {
                // Same mtime — only stale if size somehow drifted (rare; a
                // touch with same content but different length implies real
                // drift even at same timestamp).
                entry.size != current_size
            }
            (Some(_), Some(_)) => true, // mtimes differ
            // Either we didn't record before or can't read now — be safe.
            _ => true,
        }
    }
}

/// Read filesystem metadata, returning `(mtime, size)`. Both fields are
/// `None` if the path cannot be statted or if the platform doesn't support
/// the queried metadata (rare).
fn read_metadata(path: &Path) -> (Option<SystemTime>, Option<u64>) {
    match std::fs::metadata(path) {
        Ok(meta) => {
            let mtime = meta.modified().ok();
            let size = Some(meta.len());
            (mtime, size)
        }
        Err(_) => (None, None),
    }
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
    }
}
