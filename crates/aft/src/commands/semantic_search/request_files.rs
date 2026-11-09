//! The candidate files of one search request, each read from disk at most
//! once.
//!
//! One `aft_search` request used to read the same candidate file in several
//! phases: pattern verification, the exact pass, lexical exactness, the
//! literal-presence check, matching-line and declaration-line selection, the
//! code-reference check, the reranker's candidate text and the snippets. Each
//! phase opened and read the file again, and decoded it again.
//!
//! [`RequestFiles`] keeps the bytes the first phase read, with the stat that
//! phase took before reading when it took one, and serves every later phase
//! of the same request from them. Every phase of one request therefore sees
//! the same version of a file, taken when it was first read; caches that live
//! across requests (the exact memo, prepared text by digest, the rerank memo)
//! keep their own freshness rules and take the digest from here.
//!
//! The set lives for one request: [`install`] at the start of the request
//! returns a guard, and [`current`] finds the set from the request's own
//! thread or Tokio task, the way `search_b2::embed_counter` attributes
//! embeddings. Rayon workers are other threads, so [`current`] does not see
//! the set there: a parallel section takes it on the request thread and
//! hands it to its tasks explicitly.
//!
//! Bytes are kept up to [`REQUEST_FILES_BUDGET_BYTES`] per request. A phase
//! that read a file only to reject it (no match) releases it again, and past
//! the budget a file is served but not kept. Either way a later phase that
//! needs the file reads it again, which the report counts as a repeat read.

use std::collections::HashMap;
use std::io;
use std::marker::PhantomData;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock};
use std::thread::ThreadId;
use std::time::SystemTime;

use parking_lot::Mutex;

/// Most bytes one request keeps for later phases.
pub(crate) const REQUEST_FILES_BUDGET_BYTES: usize = 32 * 1024 * 1024;

/// Size, modification time and file identity of a file when a phase read it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct FileStat {
    pub size: u64,
    pub modified: Option<SystemTime>,
    /// The inode on Unix; `None` where the platform has no stable id here.
    pub file_id: Option<u64>,
}

impl FileStat {
    pub(crate) fn of(metadata: &std::fs::Metadata) -> Self {
        Self {
            size: metadata.len(),
            modified: metadata.modified().ok(),
            #[cfg(unix)]
            file_id: Some(std::os::unix::fs::MetadataExt::ino(metadata)),
            #[cfg(not(unix))]
            file_id: None,
        }
    }
}

/// One file's bytes as a phase read them, with what later phases derive
/// from them computed once.
pub(crate) struct RequestFile {
    bytes: Vec<u8>,
    stat: Option<FileStat>,
    digest: OnceLock<blake3::Hash>,
    /// The lossy decoding when the bytes are not valid UTF-8; `None` when they
    /// are, and the text is the bytes themselves.
    lossy: OnceLock<Option<String>>,
}

impl RequestFile {
    fn new(bytes: Vec<u8>, stat: Option<FileStat>) -> Self {
        Self {
            bytes,
            stat,
            digest: OnceLock::new(),
            lossy: OnceLock::new(),
        }
    }

    /// Bytes read outside any request's set (no request is running), in the
    /// same shape so callers handle both cases alike.
    pub(crate) fn unshared(bytes: Vec<u8>, stat: Option<FileStat>) -> Self {
        Self::new(bytes, stat)
    }

    pub(crate) fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// The stat the reading phase took before it read the bytes, so a file
    /// changed after it can only look newer than these bytes, never older.
    pub(crate) fn stat(&self) -> Option<FileStat> {
        self.stat
    }

    /// Blake3 of the bytes, the digest the exact lane's caches are keyed by.
    pub(crate) fn digest(&self) -> blake3::Hash {
        *self.digest.get_or_init(|| blake3::hash(&self.bytes))
    }

    /// The bytes as text when they are valid UTF-8 (what `read_to_string`
    /// accepts).
    pub(crate) fn utf8(&self) -> Option<&str> {
        std::str::from_utf8(&self.bytes).ok()
    }

    /// The bytes decoded as `String::from_utf8_lossy` decodes them, built
    /// once.
    pub(crate) fn lossy_text(&self) -> &str {
        let lossy = self
            .lossy
            .get_or_init(|| match String::from_utf8_lossy(&self.bytes) {
                std::borrow::Cow::Borrowed(_) => None,
                std::borrow::Cow::Owned(text) => Some(text),
            });
        match lossy {
            Some(text) => text,
            None => std::str::from_utf8(&self.bytes).unwrap_or_default(),
        }
    }

    fn retained_bytes(&self) -> usize {
        self.bytes.capacity()
    }
}

/// A file fetched through the set.
pub(crate) struct Fetched {
    pub file: Arc<RequestFile>,
    /// This fetch read the disk: no earlier phase of the request kept the
    /// file. Only such a phase may release it again.
    pub fresh: bool,
}

#[derive(Default)]
struct Slot {
    /// Held while the file is read, so two tasks asking for the same path at
    /// once read it once.
    file: Mutex<Option<Arc<RequestFile>>>,
    disk_reads: AtomicUsize,
}

/// Which phase's matching-line answer, for one file: the query it matched and
/// the symbol range it was limited to.
type MatchingLineKey = (PathBuf, String, Option<(usize, usize)>);

/// What one request read and kept, for the slow-call line and benchmarks.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RequestFilesReport {
    /// Reads of file content from disk through the set.
    pub disk_reads: usize,
    /// Bytes those reads returned.
    pub disk_bytes: u64,
    /// Disk reads of a path the request had already read: the file was
    /// released after a rejecting check, or did not fit the budget.
    pub repeat_reads: usize,
    /// Files read but not kept because the budget was full.
    pub over_budget: usize,
    /// Fetches served from kept bytes without a disk read.
    pub served: usize,
    /// Largest number of bytes kept at once.
    pub peak_bytes: usize,
    /// Matching-line answers served from an earlier phase's answer.
    pub matching_line_hits: usize,
}

impl RequestFilesReport {
    pub const EMPTY: Self = Self {
        disk_reads: 0,
        disk_bytes: 0,
        repeat_reads: 0,
        over_budget: 0,
        served: 0,
        peak_bytes: 0,
        matching_line_hits: 0,
    };

    /// Sum with a nested request's report; peaks add too, since a nested
    /// request's set lives while its parent's does.
    pub fn add(&mut self, other: Self) {
        self.disk_reads += other.disk_reads;
        self.disk_bytes += other.disk_bytes;
        self.repeat_reads += other.repeat_reads;
        self.over_budget += other.over_budget;
        self.served += other.served;
        self.peak_bytes += other.peak_bytes;
        self.matching_line_hits += other.matching_line_hits;
    }
}

/// The candidate files of one request. See the module documentation.
pub(crate) struct RequestFiles {
    slots: Mutex<HashMap<PathBuf, Arc<Slot>>>,
    matching_lines: Mutex<HashMap<MatchingLineKey, Option<(u32, String)>>>,
    budget: usize,
    kept_bytes: AtomicUsize,
    peak_bytes: AtomicUsize,
    disk_reads: AtomicUsize,
    disk_bytes: AtomicU64,
    repeat_reads: AtomicUsize,
    over_budget: AtomicUsize,
    served: AtomicUsize,
    matching_line_hits: AtomicUsize,
}

impl Default for RequestFiles {
    fn default() -> Self {
        Self::with_budget(REQUEST_FILES_BUDGET_BYTES)
    }
}

impl RequestFiles {
    pub(crate) fn with_budget(budget: usize) -> Self {
        Self {
            slots: Mutex::new(HashMap::new()),
            matching_lines: Mutex::new(HashMap::new()),
            budget,
            kept_bytes: AtomicUsize::new(0),
            peak_bytes: AtomicUsize::new(0),
            disk_reads: AtomicUsize::new(0),
            disk_bytes: AtomicU64::new(0),
            repeat_reads: AtomicUsize::new(0),
            over_budget: AtomicUsize::new(0),
            served: AtomicUsize::new(0),
            matching_line_hits: AtomicUsize::new(0),
        }
    }

    fn slot(&self, path: &Path) -> Arc<Slot> {
        let mut slots = self.slots.lock();
        if let Some(slot) = slots.get(path) {
            return Arc::clone(slot);
        }
        let slot = Arc::new(Slot::default());
        slots.insert(path.to_path_buf(), Arc::clone(&slot));
        slot
    }

    /// The kept bytes of `path`, without reading the disk.
    pub(crate) fn kept(&self, path: &Path) -> Option<Arc<RequestFile>> {
        let slot = self.slots.lock().get(path).cloned()?;
        let file = slot.file.lock().clone()?;
        self.served.fetch_add(1, Ordering::Relaxed);
        Some(file)
    }

    /// The bytes of `path`: the kept ones, or else what `read` returns, kept
    /// for later phases when they fit the budget. `read` is the calling
    /// phase's own way to read the file (its admission checks, deadline and
    /// size bounds), and returns the stat it took before reading, if any. A
    /// failed read keeps nothing, so the next phase tries its own read.
    pub(crate) fn read(
        &self,
        path: &Path,
        read: impl FnOnce(&Path) -> io::Result<(Vec<u8>, Option<FileStat>)>,
    ) -> io::Result<Fetched> {
        let slot = self.slot(path);
        let mut kept = slot.file.lock();
        if let Some(file) = kept.as_ref() {
            self.served.fetch_add(1, Ordering::Relaxed);
            return Ok(Fetched {
                file: Arc::clone(file),
                fresh: false,
            });
        }
        #[cfg(test)]
        record_disk_read(path);
        let (bytes, stat) = read(path)?;
        let file = self.record_read(&slot, &mut kept, bytes, stat);
        Ok(Fetched { file, fresh: true })
    }

    /// Keep bytes a phase read on its own (the exact memo's digest check
    /// reads with stats of its own), so later phases need not read again.
    /// Bytes kept already win: every phase sees the first read.
    pub(crate) fn keep_read(
        &self,
        path: &Path,
        bytes: Vec<u8>,
        stat: Option<FileStat>,
    ) -> Arc<RequestFile> {
        let slot = self.slot(path);
        let mut kept = slot.file.lock();
        if let Some(file) = kept.as_ref() {
            return Arc::clone(file);
        }
        self.record_read(&slot, &mut kept, bytes, stat)
    }

    fn record_read(
        &self,
        slot: &Slot,
        kept: &mut Option<Arc<RequestFile>>,
        bytes: Vec<u8>,
        stat: Option<FileStat>,
    ) -> Arc<RequestFile> {
        self.disk_reads.fetch_add(1, Ordering::Relaxed);
        self.disk_bytes
            .fetch_add(bytes.len() as u64, Ordering::Relaxed);
        if slot.disk_reads.fetch_add(1, Ordering::Relaxed) > 0 {
            self.repeat_reads.fetch_add(1, Ordering::Relaxed);
        }
        let file = Arc::new(RequestFile::new(bytes, stat));
        let size = file.retained_bytes();
        let reserved = self
            .kept_bytes
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |kept| {
                kept.checked_add(size).filter(|total| *total <= self.budget)
            });
        match reserved {
            Ok(before) => {
                self.peak_bytes.fetch_max(before + size, Ordering::Relaxed);
                *kept = Some(Arc::clone(&file));
            }
            Err(_) => {
                self.over_budget.fetch_add(1, Ordering::Relaxed);
            }
        }
        file
    }

    /// Stop keeping `path`: a phase read it fresh and rejected it, so no
    /// later phase is expected to want it.
    pub(crate) fn release(&self, path: &Path) {
        let Some(slot) = self.slots.lock().get(path).cloned() else {
            return;
        };
        if let Some(file) = slot.file.lock().take() {
            self.kept_bytes
                .fetch_sub(file.retained_bytes(), Ordering::Relaxed);
        }
    }

    /// The matching line of `path` for `query` within `range`, computed once
    /// per request: the ranking and the reranker's candidate text pick the
    /// same line from the same bytes. A `compute` that could not read the
    /// file answers `None` and leaves nothing behind, so a later phase tries
    /// again.
    pub(crate) fn matching_line(
        &self,
        path: &Path,
        query: &str,
        range: Option<(usize, usize)>,
        compute: impl FnOnce() -> io::Result<Option<(u32, String)>>,
    ) -> Option<(u32, String)> {
        let key = (path.to_path_buf(), query.to_string(), range);
        if let Some(found) = self.matching_lines.lock().get(&key) {
            self.matching_line_hits.fetch_add(1, Ordering::Relaxed);
            return found.clone();
        }
        let found = compute().ok()?;
        self.matching_lines.lock().insert(key, found.clone());
        found
    }

    pub(crate) fn report(&self) -> RequestFilesReport {
        RequestFilesReport {
            disk_reads: self.disk_reads.load(Ordering::Relaxed),
            disk_bytes: self.disk_bytes.load(Ordering::Relaxed),
            repeat_reads: self.repeat_reads.load(Ordering::Relaxed),
            over_budget: self.over_budget.load(Ordering::Relaxed),
            served: self.served.load(Ordering::Relaxed),
            peak_bytes: self.peak_bytes.load(Ordering::Relaxed),
            matching_line_hits: self.matching_line_hits.load(Ordering::Relaxed),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
enum ExecutionId {
    Task(tokio::task::Id),
    Thread(ThreadId),
}

fn current_execution() -> ExecutionId {
    tokio::task::try_id()
        .map(ExecutionId::Task)
        .unwrap_or_else(|| ExecutionId::Thread(std::thread::current().id()))
}

type Active = HashMap<ExecutionId, Vec<Arc<RequestFiles>>>;

fn active() -> &'static Mutex<Active> {
    static ACTIVE: OnceLock<Mutex<Active>> = OnceLock::new();
    ACTIVE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Keeps one request's set current on its thread or task until dropped.
#[must_use]
pub(crate) struct Guard {
    execution: ExecutionId,
    files: Arc<RequestFiles>,
    _not_send: PhantomData<Rc<()>>,
}

impl Guard {
    pub(crate) fn files(&self) -> &Arc<RequestFiles> {
        &self.files
    }
}

impl Drop for Guard {
    fn drop(&mut self) {
        super::waits::record_request_files(self.files.report());
        let mut active = active().lock();
        if let Some(stack) = active.get_mut(&self.execution) {
            if let Some(position) = stack
                .iter()
                .rposition(|files| Arc::ptr_eq(files, &self.files))
            {
                stack.remove(position);
            }
            if stack.is_empty() {
                active.remove(&self.execution);
            }
        }
    }
}

/// Start a request's set. A request nested in another (a parent folder's
/// child search) gets its own set while its guard lives.
pub(crate) fn install() -> Guard {
    let execution = current_execution();
    let files = Arc::new(RequestFiles::default());
    active()
        .lock()
        .entry(execution)
        .or_default()
        .push(Arc::clone(&files));
    Guard {
        execution,
        files,
        _not_send: PhantomData,
    }
}

/// The set of the request running on this thread or task, if any.
pub(crate) fn current() -> Option<Arc<RequestFiles>> {
    active()
        .lock()
        .get(&current_execution())
        .and_then(|stack| stack.last().cloned())
}

/// `read`, through the current request's set when there is one.
pub(crate) fn read_through(
    files: Option<&RequestFiles>,
    path: &Path,
    read: impl FnOnce(&Path) -> io::Result<(Vec<u8>, Option<FileStat>)>,
) -> io::Result<Fetched> {
    match files {
        Some(files) => files.read(path, read),
        None => {
            #[cfg(test)]
            record_disk_read(path);
            let (bytes, stat) = read(path)?;
            Ok(Fetched {
                file: Arc::new(RequestFile::new(bytes, stat)),
                fresh: true,
            })
        }
    }
}

/// Read the whole file the way `std::fs::read` does, with the stat of the
/// opened file.
pub(crate) fn read_whole_file(path: &Path) -> io::Result<(Vec<u8>, Option<FileStat>)> {
    use std::io::Read as _;
    let mut file = std::fs::File::open(path)?;
    let stat = file.metadata().ok().map(|metadata| FileStat::of(&metadata));
    let mut bytes = Vec::new();
    if let Some(stat) = stat {
        bytes.reserve(usize::try_from(stat.size).unwrap_or(0));
    }
    file.read_to_end(&mut bytes)?;
    Ok((bytes, stat))
}

/// Test probe: every read of file content a search does, by path, whether
/// through the set or through a phase's own read. Probes are keyed by a root
/// so tests running at once on other roots do not see each other's reads.
#[cfg(test)]
mod read_probe {
    use std::collections::BTreeMap;
    use std::path::{Path, PathBuf};
    use std::sync::{Arc, OnceLock};

    use parking_lot::Mutex;

    type Reads = Arc<Mutex<BTreeMap<PathBuf, usize>>>;

    fn probes() -> &'static Mutex<BTreeMap<PathBuf, Reads>> {
        static PROBES: OnceLock<Mutex<BTreeMap<PathBuf, Reads>>> = OnceLock::new();
        PROBES.get_or_init(Mutex::default)
    }

    pub(crate) struct ReadProbe {
        root: PathBuf,
        reads: Reads,
    }

    impl ReadProbe {
        pub(crate) fn new(root: &Path) -> Self {
            let reads = Reads::default();
            probes()
                .lock()
                .insert(root.to_path_buf(), Arc::clone(&reads));
            Self {
                root: root.to_path_buf(),
                reads,
            }
        }

        /// Reads per path under the root so far.
        pub(crate) fn reads(&self) -> BTreeMap<PathBuf, usize> {
            self.reads.lock().clone()
        }
    }

    impl Drop for ReadProbe {
        fn drop(&mut self) {
            probes().lock().remove(&self.root);
        }
    }

    pub(crate) fn record_disk_read(path: &Path) {
        for (root, reads) in probes().lock().iter() {
            if path.starts_with(root) {
                *reads.lock().entry(path.to_path_buf()).or_default() += 1;
            }
        }
    }
}

#[cfg(test)]
pub(crate) use read_probe::{record_disk_read, ReadProbe};

#[cfg(test)]
mod tests {
    use super::*;

    fn read_counted(
        counter: &AtomicUsize,
        bytes: &'static [u8],
    ) -> impl FnOnce(&Path) -> io::Result<(Vec<u8>, Option<FileStat>)> + '_ {
        move |_| {
            counter.fetch_add(1, Ordering::SeqCst);
            Ok((bytes.to_vec(), None))
        }
    }

    #[test]
    fn a_kept_file_is_read_once_and_released_files_are_read_again() {
        let files = RequestFiles::default();
        let reads = AtomicUsize::new(0);
        let path = Path::new("/request-files/a.rs");
        let first = files
            .read(path, read_counted(&reads, b"fn a() {}"))
            .unwrap();
        assert!(first.fresh);
        let second = files.read(path, read_counted(&reads, b"changed")).unwrap();
        assert!(!second.fresh);
        assert_eq!(second.file.bytes(), b"fn a() {}");
        assert_eq!(reads.load(Ordering::SeqCst), 1);

        files.release(path);
        assert!(files.kept(path).is_none());
        let third = files.read(path, read_counted(&reads, b"changed")).unwrap();
        assert!(third.fresh);
        assert_eq!(files.report().repeat_reads, 1);
    }

    #[test]
    fn files_past_the_budget_are_served_but_not_kept() {
        let files = RequestFiles::with_budget(8);
        let reads = AtomicUsize::new(0);
        let small = Path::new("/request-files/small");
        let large = Path::new("/request-files/large");
        files.read(small, read_counted(&reads, b"1234")).unwrap();
        let fetched = files
            .read(large, read_counted(&reads, b"123456789"))
            .unwrap();
        assert_eq!(fetched.file.bytes(), b"123456789");
        assert!(files.kept(small).is_some());
        assert!(files.kept(large).is_none());
        let report = files.report();
        assert_eq!(report.over_budget, 1);
        assert_eq!(report.peak_bytes, 4);
    }

    #[test]
    fn concurrent_fetches_of_one_path_read_it_once() {
        let files = Arc::new(RequestFiles::default());
        let reads = Arc::new(AtomicUsize::new(0));
        let path = PathBuf::from("/request-files/shared");
        std::thread::scope(|scope| {
            for _ in 0..8 {
                let files = Arc::clone(&files);
                let reads = Arc::clone(&reads);
                let path = path.clone();
                scope.spawn(move || {
                    files
                        .read(&path, |_| {
                            reads.fetch_add(1, Ordering::SeqCst);
                            std::thread::sleep(std::time::Duration::from_millis(5));
                            Ok((b"shared".to_vec(), None))
                        })
                        .unwrap()
                });
            }
        });
        assert_eq!(reads.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn lossy_text_matches_from_utf8_lossy() {
        let files = RequestFiles::default();
        let path = Path::new("/request-files/latin1");
        let fetched = files
            .read(path, |_| Ok((b"caf\xe9 ok".to_vec(), None)))
            .unwrap();
        assert_eq!(
            fetched.file.lossy_text(),
            String::from_utf8_lossy(b"caf\xe9 ok")
        );
        assert!(fetched.file.utf8().is_none());
        assert_eq!(fetched.file.digest(), blake3::hash(b"caf\xe9 ok"));
    }

    #[test]
    fn the_innermost_installed_set_is_current_until_its_guard_drops() {
        assert!(current().is_none());
        let outer = install();
        let inner = install();
        assert!(Arc::ptr_eq(&current().unwrap(), inner.files()));
        drop(inner);
        assert!(Arc::ptr_eq(&current().unwrap(), outer.files()));
        drop(outer);
        assert!(current().is_none());
    }
}
