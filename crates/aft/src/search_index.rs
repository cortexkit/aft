use std::cell::Cell;
use std::collections::{BTreeMap, BTreeSet, BinaryHeap, HashMap, HashSet};
use std::fs::{self, File, OpenOptions};
use std::io::{BufReader, BufWriter, Cursor, Read, Seek, SeekFrom, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::{
    atomic::{AtomicBool, AtomicUsize, Ordering},
    Arc, Mutex, OnceLock, RwLock, RwLockReadGuard, TryLockError, Weak,
};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use globset::{Glob, GlobSet, GlobSetBuilder};
use ignore::WalkBuilder;
use rayon::prelude::*;
use regex::bytes::Regex;
use regex_syntax::hir::{Hir, HirKind};
use serde::{Deserialize, Serialize};

use crate::cache_freshness::{self, FileFreshness, FreshnessVerdict};
use crate::fs_lock;
use crate::pattern_compile::{self, CompileOpts, CompileResult, CompiledPattern, LiteralSearch};

pub use crate::commands::semantic_search::{exact_lane, memo};

#[derive(Debug, Clone, PartialEq)]
pub struct ExactPassMatch {
    pub path: PathBuf,
    pub symbol_range: Option<crate::commands::semantic_search::comparator::SymbolOffsetRange>,
    pub evidence: crate::commands::semantic_search::evidence_descriptor::EvidenceDescriptor,
    pub content_digest: String,
}

/// Which evidence a caller keeps from an exact pass.
///
/// The pass reads every file that could hold kept evidence, so the scope sets
/// how much of the corpus it reads. A declaration named after a query word can
/// sit in any file that mentions that word, while a verbatim phrase or an
/// all-words window needs every one of its words in the file. A caller that
/// drops declaration evidence therefore lets the pass skip the files that only
/// mention one or two query words, which for a prose query is most of the
/// corpus. Files that are read are verified exactly as under [`Self::All`], so
/// the evidence a caller keeps is the same under any scope that keeps it.
///
/// Defined here rather than in the exact lane module because that module is
/// compiled at two paths, and both must name the same type.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
pub enum ExactEvidenceScope {
    /// Declarations named after a query word, verbatim phrase evidence (E1)
    /// and all-words window evidence (E2).
    #[default]
    All,
    /// Phrase and window evidence; the caller drops declaration evidence.
    PhraseAndWindow,
    /// Phrase evidence only; the caller drops window and declaration evidence.
    Phrase,
    /// Phrase and window evidence, plus declaration evidence only where the
    /// declaration's text contains one of these words, which the caller takes
    /// from the identifier-shaped words of the query (`snake_case`,
    /// `camelCase`, `a::b`).
    DefinitionsMentioning(Vec<String>),
}

pub(crate) const DEFAULT_MAX_FILE_SIZE: u64 = 1_048_576;
// The warm change budget admits at most 512 Git-diff or stat-stale paths.
// Larger refreshes belong under the shared build limit, not the disk-load lane.
const WARM_GIT_DIFF_PATH_LIMIT: usize = 512;
const CACHE_MAGIC: u32 = 0x3144_4958; // "XID1" little-endian
const INDEX_MAGIC: &[u8; 8] = b"AFTIDX01";
const LOOKUP_MAGIC: &[u8; 8] = b"AFTLKP01";
const SPILL_MAGIC: &[u8; 8] = b"AFTSPI01";
const FILE_TRIGRAM_COUNT_MAGIC: &[u8; 8] = b"AFTFTC01";
const INDEX_VERSION: u32 = 4;
/// Highest trigram cache format this build reads (and the one it writes).
pub const INDEX_FORMAT_VERSION: u32 = INDEX_VERSION;
const SPIMI_SOFT_LIMIT_BYTES: usize = 128 * 1024 * 1024;
const SPIMI_HARD_LIMIT_BYTES: usize = 256 * 1024 * 1024;
const SPILL_RECORD_ESTIMATED_BYTES: usize = 16;
const DELTA_COMPACT_SOFT_FILES: usize = 1_000;
const DELTA_COMPACT_HARD_FILES: usize = 5_000;
const DELTA_COMPACT_SOFT_BYTES: usize = 32 * 1024 * 1024;
const DELTA_COMPACT_HARD_BYTES: usize = 128 * 1024 * 1024;
const EOF_SENTINEL: u8 = 0;
const MAX_ENTRIES: usize = 10_000_000;
const MIN_FILE_ENTRY_BYTES: usize = 57;
const LOOKUP_ENTRY_BYTES: usize = 16;
const POSTING_BYTES: usize = 6;
const ARTIFACT_CACHE_KEY_MEMO_FILE: &str = "cache-keys.json";
const ARTIFACT_CACHE_KEY_MEMO_EVICTION_AGE: Duration = Duration::from_secs(7 * 24 * 60 * 60);
const ARTIFACT_CACHE_KEY_MEMO_READ_REFRESH_AGE: Duration = Duration::from_secs(24 * 60 * 60);
const LEGACY_IDENTITY_MEMO_MAX_AGE: Duration = Duration::from_secs(10 * 60);
const LEGACY_IDENTITY_MEMO_MAX_STORAGE_ROOTS: usize = 4;
#[cfg(test)]
const INDEX_ORPHAN_MIN_AGE: Duration = Duration::from_secs(14 * 24 * 60 * 60);
#[cfg(test)]
const INDEX_ORPHAN_SWEEP_LIMIT: usize = 200;
const TRANSIENT_SEARCH_CACHE_PREFIX: &str = "aft-search-cache.";
/// Our scratch caches live in one directory of our own inside the system
/// temporary directory. The sweep below reads whatever directory it is given,
/// and the shared `$TMPDIR` is a fleet dumping ground: on this box it held
/// 290,697 entries and one bare listing of it took ~870 s, which pinned six of
/// eight executor workers inside a configure sweep and starved route binds
/// (2026-09-19). Owning a subdirectory makes the sweep O(our entries).
const TRANSIENT_SEARCH_CACHE_DIR_NAME: &str = "aft-search-caches";
/// A streaming build owns its temporary cache for minutes, not days. Use the
/// same conservative age-only predicate as other interrupted-build reapers so
/// a recycled PID can never protect abandoned data.
const TRANSIENT_SEARCH_CACHE_MIN_AGE: Duration = Duration::from_secs(24 * 60 * 60);
const TRANSIENT_SEARCH_CACHE_SWEEP_LIMIT: usize = 200;
/// Raw directory entries examined in one sweep, whether or not they are ours.
/// The reap limit above bounds what we act on; this bounds what we *read*, so
/// a foreign directory with hundreds of thousands of entries cannot hold the
/// calling worker for the length of a full listing.
const TRANSIENT_SEARCH_CACHE_SCAN_LIMIT: usize = 2_048;
const TRANSIENT_SEARCH_CACHE_SWEEP_BUDGET: Duration = Duration::from_secs(5);
static CACHE_LOCK_ACQUIRE_MUTEX: Mutex<()> = Mutex::new(());
static ARTIFACT_CACHE_KEY_MEMO_STATE: OnceLock<Mutex<ArtifactCacheKeyMemoState>> = OnceLock::new();
#[cfg(test)]
static INDEX_ORPHAN_SWEEP_CURSORS: OnceLock<Mutex<HashMap<PathBuf, String>>> = OnceLock::new();
static TRANSIENT_SEARCH_CACHE_SWEEP_CURSORS: OnceLock<Mutex<HashMap<PathBuf, String>>> =
    OnceLock::new();
static TRANSIENT_SEARCH_CACHE_BUILD_LOCKS: OnceLock<Mutex<HashMap<PathBuf, Weak<Mutex<()>>>>> =
    OnceLock::new();

#[cfg(debug_assertions)]
thread_local! {
    static POSTINGS_FOR_TRIGRAM_CALLS: Cell<usize> = const { Cell::new(0) };
}

#[cfg(debug_assertions)]
#[doc(hidden)]
pub fn reset_postings_for_trigram_count_for_debug() {
    POSTINGS_FOR_TRIGRAM_CALLS.with(|calls| calls.set(0));
}

#[cfg(debug_assertions)]
#[doc(hidden)]
pub fn postings_for_trigram_count_for_debug() -> usize {
    POSTINGS_FOR_TRIGRAM_CALLS.with(Cell::get)
}

#[cfg(test)]
type RootCommitProbeOverride =
    Arc<dyn Fn(&Path) -> Option<RootCommitProbe> + Send + Sync + 'static>;

#[cfg(test)]
static GIT_ROOT_COMMIT_PROBE_OVERRIDE: OnceLock<Mutex<Option<RootCommitProbeOverride>>> =
    OnceLock::new();

#[derive(Clone, Debug, Deserialize, Serialize)]
struct ArtifactCacheKeyMemoEntry {
    key: String,
    git_root_commit: String,
    recorded_at_ms: u64,
}

#[derive(Default)]
struct ArtifactCacheKeyMemoState {
    by_storage_root: BTreeMap<PathBuf, BTreeMap<String, ArtifactCacheKeyMemoEntry>>,
}

#[cfg(test)]
type AfterLegacyMemoRename = Box<dyn FnOnce(&Path)>;
#[cfg(test)]
thread_local! {
    static LEGACY_MEMO_BUILDS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    static LEGACY_MEMO_CANONICALIZATIONS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    static AFTER_LEGACY_MEMO_RENAME: std::cell::RefCell<Option<AfterLegacyMemoRename>> = const { std::cell::RefCell::new(None) };
}

#[derive(Default)]
#[cfg(test)]
struct IndexOrphanSweepSummary {
    scanned: usize,
    removed: usize,
    skipped_derived: usize,
    skipped_memo: usize,
    skipped_fresh: usize,
    skipped_live: usize,
    skipped_locked: usize,
    skipped_unreadable: usize,
    budget_exhausted: bool,
}

#[derive(Default)]
struct TransientSearchCacheSweepSummary {
    scanned: usize,
    /// Raw directory entries read, including entries that are not ours. The
    /// reaping numbers below cannot show a listing that never reached us.
    examined: usize,
    removed: usize,
    bytes: u64,
    skipped_fresh: usize,
    skipped_unreadable: usize,
    budget_exhausted: bool,
}

#[derive(Clone, Debug)]
struct TransientSearchCacheName {
    key: String,
    pid: u32,
}

enum TransientSearchCacheWalk {
    Complete(u64),
    BudgetExceeded,
    Failed,
}

pub(crate) const INTERACTIVE_ARTIFACT_READ_BUDGET: Duration = Duration::from_millis(250);
/// Borrowed snapshots are rerooted before grep's candidate/file-walk budgets
/// begin. Cap that parse separately so per-record path containment checks cannot
/// occupy an interactive lane for the size of an arbitrary foreign corpus.
pub(crate) const BORROWED_INDEX_LOAD_BUDGET: Duration = Duration::from_secs(1);
const BORROWED_INDEX_CHECKPOINT_INTERVAL: usize = 64;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum BorrowedIndexLoadStop {
    BudgetExceeded,
    Cancelled,
    /// The snapshot was written for a different part of the checkout than the
    /// loading root covers (see `checkout_position`).
    CoverageMismatch,
}

#[derive(Debug)]
pub(crate) enum BorrowedIndexLoad {
    Loaded(SearchIndex, bool),
    Stopped(BorrowedIndexLoadStop),
    Invalid,
}

/// What reconciling a borrowed snapshot against this checkout's files found.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct BorrowedReconcileSummary {
    /// Files the ignore-aware walk found on disk.
    pub(crate) walked: usize,
    /// Snapshot records whose bytes still match disk (kept on shared postings).
    pub(crate) unchanged: usize,
    /// The subset of `unchanged` that needed a content hash to prove it.
    pub(crate) hashed_unchanged: usize,
    /// Snapshot records whose content differs and were re-indexed in RAM.
    pub(crate) reindexed: usize,
    /// Files on disk that the snapshot does not contain.
    pub(crate) added: usize,
    /// Snapshot records with no file on disk any more.
    pub(crate) removed: usize,
    pub(crate) walk: Duration,
    pub(crate) verify: Duration,
    pub(crate) apply: Duration,
}

struct BorrowedIndexLoadBudget<'a> {
    keep_going: &'a dyn Fn() -> bool,
    started_at: Instant,
    duration: Duration,
    max_records: usize,
    stop: Cell<Option<BorrowedIndexLoadStop>>,
}

impl<'a> BorrowedIndexLoadBudget<'a> {
    fn new(max_records: usize, duration: Duration, keep_going: &'a dyn Fn() -> bool) -> Self {
        Self {
            keep_going,
            started_at: Instant::now(),
            duration,
            max_records,
            stop: Cell::new(None),
        }
    }

    fn preflight(&self, records: usize) -> bool {
        if records > self.max_records {
            self.stop.set(Some(BorrowedIndexLoadStop::BudgetExceeded));
            return false;
        }
        self.checkpoint()
    }

    fn preflight_lookup_bytes(&self, bytes: u64, file_records: usize) -> bool {
        let lookup_records = self.max_records.saturating_sub(file_records);
        let max_bytes = 20u64.saturating_add(
            u64::try_from(lookup_records)
                .unwrap_or(u64::MAX)
                .saturating_mul(LOOKUP_ENTRY_BYTES as u64),
        );
        if bytes > max_bytes {
            self.stop.set(Some(BorrowedIndexLoadStop::BudgetExceeded));
            return false;
        }
        self.checkpoint()
    }

    fn checkpoint_at(&self, record: usize) -> bool {
        if record % BORROWED_INDEX_CHECKPOINT_INTERVAL == 0 {
            self.checkpoint()
        } else {
            true
        }
    }

    fn checkpoint(&self) -> bool {
        if !(self.keep_going)()
            || crate::executor::current_job_cancellation()
                .is_some_and(|token| token.cancel_requested_before_commit())
        {
            self.stop.set(Some(BorrowedIndexLoadStop::Cancelled));
            return false;
        }
        if self.started_at.elapsed() >= self.duration {
            self.stop.set(Some(BorrowedIndexLoadStop::BudgetExceeded));
            return false;
        }
        true
    }
}

/// Read an artifact pointer without allowing a writer to strand an interactive request.
///
/// Index refreshes publish through `RwLock`s and can legitimately hold a write guard while
/// validating or replacing a large artifact. Search must treat that contention as temporary
/// unavailability and use its bounded fallback rather than waiting for transport timeout.
pub(crate) fn try_read_with_budget<T>(
    lock: &RwLock<T>,
    budget: Duration,
) -> Option<RwLockReadGuard<'_, T>> {
    let deadline = Instant::now() + budget;
    loop {
        match lock.try_read() {
            Ok(guard) => return Some(guard),
            Err(TryLockError::Poisoned(poisoned)) => return Some(poisoned.into_inner()),
            Err(TryLockError::WouldBlock) => {
                let now = Instant::now();
                if now >= deadline {
                    return None;
                }
                std::thread::sleep((deadline - now).min(Duration::from_millis(1)));
            }
        }
    }
}

pub struct CacheLock {
    _guard: Option<fs_lock::LockGuard>,
}

impl CacheLock {
    pub fn acquire(cache_dir: &Path, project_root: &Path) -> std::io::Result<Self> {
        Self::acquire_with_timeout(cache_dir, project_root, Duration::from_secs(2))
    }

    pub fn try_acquire_for_shutdown(
        cache_dir: &Path,
        project_root: &Path,
    ) -> std::io::Result<Self> {
        // Graceful shutdown gets one short best-effort lock attempt so a
        // sibling writer cannot hold process exit open.
        Self::acquire_with_timeout(cache_dir, project_root, Duration::from_millis(25))
    }

    fn acquire_with_timeout(
        cache_dir: &Path,
        project_root: &Path,
        timeout: Duration,
    ) -> std::io::Result<Self> {
        let path = cache_dir.join("cache.lock");
        if !artifact_write_allowed(project_root, cache_dir, &path) {
            return Ok(Self { _guard: None });
        }
        crate::private_storage::open_keyed_dir(cache_dir, "index")?;
        let _acquire_guard = CACHE_LOCK_ACQUIRE_MUTEX
            .lock()
            .map_err(|_| std::io::Error::other("search cache lock acquisition mutex poisoned"))?;
        fs_lock::try_acquire(&path, timeout)
            .map(|guard| Self {
                _guard: Some(guard),
            })
            .map_err(|error| match error {
                fs_lock::AcquireError::Timeout => {
                    std::io::Error::other("timed out acquiring search cache lock")
                }
                fs_lock::AcquireError::Io(error) => error,
            })
    }
}

fn artifact_write_allowed(project_root: &Path, cache_dir: &Path, write_path: &Path) -> bool {
    let artifact_key = cache_dir
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default();
    crate::root_cache::ArtifactAccess::for_root(project_root).allows_write(artifact_key, write_path)
        // A cache written by a newer build is never replaced (and no build or
        // lock is started for it): that build's data outlives a rollback.
        && SearchIndex::check_disk_cache_format(cache_dir).is_ok()
}

#[derive(Clone, Debug)]
pub struct SearchIndex {
    base: Option<Arc<BasePostings>>,
    delta: Arc<DeltaState>,
    // This reverse lookup is writer-only. Snapshots never read it, so exclusive
    // SearchIndex write access keeps it synchronized with the versioned postings.
    delta_file_trigrams: HashMap<u32, Vec<u32>>,
    // Base IDs cannot be reused while immutable base postings still refer to
    // them. Delta IDs can: removal first erases every posting for the ID, and
    // snapshots retain the old tables through copy-on-write.
    free_delta_file_ids: Vec<u32>,
    pub files: Arc<Vec<FileEntry>>,
    pub path_to_id: Arc<HashMap<PathBuf, u32>>,
    pub ready: bool,
    /// Set when a cold build was refused because this root may not write the
    /// shared cache artifact. The index stays empty and `ready` stays false so
    /// grep/glob keep serving through the bounded fallback walk, but health must
    /// not report "building" for it: nothing will ever produce a real index here
    /// until write access changes, so it is a terminal settled state.
    pub build_denied: bool,
    project_root: PathBuf,
    git_head: Option<String>,
    max_file_size: u64,
    ignore_rules_fingerprint: String,
    pub file_trigram_count: Arc<Vec<u32>>,
    unindexed_files: Arc<HashSet<u32>>,
    base_file_count: u32,
    delta_packed_bytes: usize,
    compaction_state: Arc<Mutex<CompactionState>>,
}

// A query must observe postings and superseded base files from one version;
// mixing versions can hide both an old base posting and its delta replacement.
#[derive(Clone, Debug, Default)]
struct DeltaState {
    postings: HashMap<u32, Vec<Posting>>,
    superseded: HashSet<u32>,
    /// Paths of the superseded base files. Removing a file clears its entry's
    /// path, so without this record an edited base file could not be told
    /// apart from a deleted one when counting how far the delta has moved this
    /// index away from the snapshot it loaded.
    superseded_paths: HashSet<PathBuf>,
}

#[derive(Clone, Debug)]
struct BasePostings {
    file: Arc<File>,
    postings_blob_start: u64,
    postings_blob_len: u64,
    lookup: Arc<Vec<LookupEntry>>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct LookupEntry {
    trigram: u32,
    offset: u64,
    count: u32,
}

#[derive(Clone, Debug, Default)]
struct CompactionState {
    running: bool,
    requested_again: bool,
    buffered_paths: Vec<PathBuf>,
}

#[derive(Clone, Debug)]
pub struct SearchIndexSnapshot {
    base: Option<Arc<BasePostings>>,
    delta: Arc<DeltaState>,
    files: Arc<Vec<FileEntry>>,
    path_to_id: Arc<HashMap<PathBuf, u32>>,
    ready: bool,
    project_root: PathBuf,
    max_file_size: u64,
    file_trigram_count: Arc<Vec<u32>>,
    unindexed_files: Arc<HashSet<u32>>,
}

#[derive(Clone, Debug, Default)]
pub struct LexicalRankResult {
    pub files: Vec<(PathBuf, f32)>,
    pub engine_capped: bool,
}

impl SearchIndex {
    /// Number of indexed files.
    pub fn file_count(&self) -> usize {
        self.files.len()
    }

    /// Number of unique trigrams in the combined base index and delta postings.
    pub fn trigram_count(&self) -> usize {
        let base_count = self.base.as_ref().map_or(0, |base| base.lookup.len());
        let Some(base) = &self.base else {
            return self.delta.postings.len();
        };
        base_count
            + self
                .delta
                .postings
                .keys()
                .filter(|trigram| base.lookup_entry(**trigram).is_none())
                .count()
    }

    /// Cheap counters that move whenever the walk in [`Self::estimated_memory`]
    /// would most likely give a different answer, read without walking.
    /// `None` while compaction holds its lock (the estimate is then `busy`).
    pub(crate) fn memory_shape(&self) -> Option<[u64; 11]> {
        let compaction = self.compaction_state.try_lock().ok()?;
        Some([
            self.files.len() as u64,
            self.path_to_id.len() as u64,
            self.delta.postings.len() as u64,
            self.delta_file_trigrams.len() as u64,
            self.file_trigram_count.len() as u64,
            self.unindexed_files.len() as u64,
            self.delta.superseded.len() as u64,
            self.base
                .as_ref()
                .map_or(0, |base| base.lookup.len() as u64 + 1),
            self.delta_packed_bytes as u64,
            compaction.buffered_paths.len() as u64,
            self.files.as_ptr() as usize as u64,
        ])
    }

    /// Estimate resident trigram-index bytes. Base posting lists stay on disk
    /// and are read with `pread`; only the resident base lookup table, delta
    /// postings, superseded mask, and file tables are included here.
    pub fn estimated_memory(&self) -> crate::memory::MemoryEstimate {
        let Ok(compaction) = self.compaction_state.try_lock() else {
            return crate::memory::MemoryEstimate::busy();
        };
        if self.base.is_none()
            && self.delta.postings.is_empty()
            && self.delta_file_trigrams.is_empty()
            && self.files.is_empty()
            && self.path_to_id.is_empty()
            && self.file_trigram_count.is_empty()
            && self.unindexed_files.is_empty()
            && self.delta.superseded.is_empty()
            && compaction.buffered_paths.is_empty()
        {
            return crate::memory::MemoryEstimate::estimated(0)
                .count("files", 0)
                .count("delta_trigrams", 0)
                .count("delta_postings", 0)
                .count("superseded_files", 0)
                .count("unindexed_files", 0)
                .count("base_lookup_entries", 0)
                .count_u64("delta_packed_bytes", 0)
                .count_u64("base_postings_resident_bytes", 0);
        }
        let delta_posting_count = self
            .delta
            .postings
            .values()
            .map(Vec::len)
            .fold(0usize, usize::saturating_add);
        let delta_postings_bytes = crate::memory::usize_to_u64(delta_posting_count)
            .saturating_mul(std::mem::size_of::<Posting>() as u64)
            .saturating_add(
                crate::memory::usize_to_u64(self.delta.postings.len())
                    .saturating_mul(std::mem::size_of::<u32>() as u64),
            );
        let delta_file_trigram_count = self
            .delta_file_trigrams
            .values()
            .map(Vec::len)
            .fold(0usize, usize::saturating_add);
        let delta_file_table_bytes = crate::memory::usize_to_u64(delta_file_trigram_count)
            .saturating_mul(std::mem::size_of::<u32>() as u64)
            .saturating_add(
                crate::memory::usize_to_u64(self.delta_file_trigrams.len())
                    .saturating_mul(std::mem::size_of::<u32>() as u64),
            );
        let files_bytes = crate::memory::usize_to_u64(self.files.len())
            .saturating_mul(std::mem::size_of::<FileEntry>() as u64)
            .saturating_add(
                self.files
                    .iter()
                    .map(|entry| crate::memory::path_bytes(&entry.path))
                    .fold(0u64, u64::saturating_add),
            );
        let path_table_bytes = self.path_to_id.iter().fold(0u64, |bytes, (path, _)| {
            bytes
                .saturating_add(std::mem::size_of::<u32>() as u64)
                .saturating_add(std::mem::size_of::<PathBuf>() as u64)
                .saturating_add(crate::memory::path_bytes(path))
        });
        let file_count_table_bytes = crate::memory::usize_to_u64(self.file_trigram_count.len())
            .saturating_mul(std::mem::size_of::<u32>() as u64);
        let masks_bytes = crate::memory::usize_to_u64(
            self.delta
                .superseded
                .len()
                .saturating_add(self.unindexed_files.len()),
        )
        .saturating_mul(std::mem::size_of::<u32>() as u64);
        let base_lookup_bytes = self
            .base
            .as_ref()
            .map(|base| {
                crate::memory::usize_to_u64(base.lookup.len())
                    .saturating_mul(std::mem::size_of::<LookupEntry>() as u64)
            })
            .unwrap_or(0);
        let compaction_bytes = compaction
            .buffered_paths
            .iter()
            .map(|path| {
                (std::mem::size_of::<PathBuf>() as u64)
                    .saturating_add(crate::memory::path_bytes(path))
            })
            .fold(0u64, u64::saturating_add);
        let metadata_bytes = crate::memory::path_bytes(&self.project_root)
            .saturating_add(
                self.git_head
                    .as_ref()
                    .map(|head| crate::memory::usize_to_u64(head.len()))
                    .unwrap_or(0),
            )
            .saturating_add(crate::memory::usize_to_u64(
                self.ignore_rules_fingerprint.len(),
            ));
        let estimated_bytes = delta_postings_bytes
            .saturating_add(delta_file_table_bytes)
            .saturating_add(files_bytes)
            .saturating_add(path_table_bytes)
            .saturating_add(file_count_table_bytes)
            .saturating_add(masks_bytes)
            .saturating_add(base_lookup_bytes)
            .saturating_add(compaction_bytes)
            .saturating_add(metadata_bytes);
        crate::memory::MemoryEstimate::estimated(estimated_bytes)
            .count("files", self.files.len())
            .count("delta_trigrams", self.delta.postings.len())
            .count("delta_postings", delta_posting_count)
            .count("superseded_files", self.delta.superseded.len())
            .count("unindexed_files", self.unindexed_files.len())
            .count(
                "base_lookup_entries",
                self.base
                    .as_ref()
                    .map(|base| base.lookup.len())
                    .unwrap_or(0),
            )
            .count_u64("delta_packed_bytes", self.delta_packed_bytes as u64)
            .count_u64("base_postings_resident_bytes", 0)
    }

    /// True when `write_to_disk` would persist changes beyond the current base.
    /// This covers pure deletions and unindexed file additions, which do not
    /// always populate `delta_file_trigrams`.
    pub(crate) fn has_pending_disk_changes(&self) -> bool {
        !self.delta.postings.is_empty()
            || !self.delta.superseded.is_empty()
            || self.path_to_id.len() != self.base_file_count as usize
    }

    /// Returns an immutable snapshot for queries. Callers must obtain the
    /// snapshot while holding the RwLock that protects the SearchIndex, then
    /// drop the guard before running expensive operations such as grep, glob, or
    /// lexical ranking.
    pub fn snapshot(&self) -> SearchIndexSnapshot {
        SearchIndexSnapshot {
            base: self.base.clone(),
            delta: Arc::clone(&self.delta),
            files: Arc::clone(&self.files),
            path_to_id: Arc::clone(&self.path_to_id),
            ready: self.ready,
            project_root: self.project_root.clone(),
            max_file_size: self.max_file_size,
            file_trigram_count: Arc::clone(&self.file_trigram_count),
            unindexed_files: Arc::clone(&self.unindexed_files),
        }
    }

    /// Compute distinct query trigrams from literal tokens.
    pub fn query_trigrams_from_tokens(tokens: &[&str]) -> Vec<u32> {
        query_trigrams_from_tokens(tokens)
    }

    /// Score-rank file candidates by lexical relevance to query trigrams.
    pub fn lexical_rank(
        &self,
        query_trigrams: &[u32],
        candidate_filter: Option<&dyn Fn(&Path) -> bool>,
        max_files: usize,
    ) -> Vec<(PathBuf, f32)> {
        self.snapshot()
            .lexical_rank_with_stats(query_trigrams, candidate_filter, max_files)
            .files
    }

    pub fn is_ready(&self) -> bool {
        self.ready
    }

    /// Evaluates lexical candidates and scores them up to the specified depth limit, applying an optional candidate filter.
    pub fn lexical_rank_at_depth(
        &self,
        query_trigrams: &[u32],
        candidate_filter: Option<&dyn Fn(&Path) -> bool>,
        depth: usize,
    ) -> LexicalRankResult {
        self.snapshot()
            .lexical_rank_at_depth(query_trigrams, candidate_filter, depth)
    }

    /// Discovers and ranks lexical candidates up to the specified depth limit, applying an optional candidate filter.
    pub fn lexical_discovery_and_rank_at_depth(
        &self,
        query_trigrams: &[u32],
        candidate_filter: Option<&dyn Fn(&Path) -> bool>,
        depth: usize,
    ) -> LexicalRankResult {
        self.snapshot()
            .lexical_rank_at_depth(query_trigrams, candidate_filter, depth)
    }

    /// Whole-corpus exact pass over the trigram index in ready mode.
    pub fn whole_corpus_exact_pass(
        &self,
        query: &str,
        search_root: &Path,
        candidate_filter: Option<&dyn Fn(&Path) -> bool>,
    ) -> Vec<ExactPassMatch> {
        self.snapshot()
            .whole_corpus_exact_pass(query, search_root, candidate_filter)
    }

    /// Score-rank file candidates and report whether the pre-filter step that
    /// collects candidates reached its internal size limit before ranking.
    pub fn lexical_rank_with_stats(
        &self,
        query_trigrams: &[u32],
        candidate_filter: Option<&dyn Fn(&Path) -> bool>,
        max_files: usize,
    ) -> LexicalRankResult {
        self.snapshot()
            .lexical_rank_with_stats(query_trigrams, candidate_filter, max_files)
    }
}

impl SearchIndexSnapshot {
    pub(crate) fn file_modified(&self, path: &Path) -> Option<SystemTime> {
        self.path_to_id
            .get(path)
            .and_then(|id| self.files.get(*id as usize))
            .map(|file| file.modified)
    }
    /// Number of unique trigrams in the combined base index and delta postings.
    pub fn trigram_count(&self) -> usize {
        let base_count = self.base.as_ref().map_or(0, |base| base.lookup.len());
        let Some(base) = &self.base else {
            return self.delta.postings.len();
        };
        base_count
            + self
                .delta
                .postings
                .keys()
                .filter(|trigram| base.lookup_entry(**trigram).is_none())
                .count()
    }

    /// How this snapshot holds `path`, for the benchmark-only search recall
    /// audit: `indexed` (its trigrams are searchable), `unindexed` (the file
    /// is known but its content was not indexed, for example because it is
    /// too large), or `absent`. The audit needs this to tell "never indexed"
    /// apart from "indexed but not retrieved".
    pub(crate) fn audit_index_state(&self, path: &Path) -> &'static str {
        match self.path_to_id.get(path) {
            Some(file_id) if !self.is_active_file(*file_id) => "absent",
            Some(file_id) if self.unindexed_files.contains(file_id) => "unindexed",
            Some(_) => "indexed",
            None => "absent",
        }
    }

    pub(crate) fn has_file_in_scope(&self, search_root: &Path) -> bool {
        let search_root = canonicalize_for_search_membership(search_root);
        self.files.iter().any(|file| {
            !file.path.as_os_str().is_empty() && is_within_search_root(&search_root, &file.path)
        })
    }

    pub(crate) fn has_file_in_scope_with_filters(
        &self,
        search_root: &Path,
        filters: &PathFilters,
    ) -> bool {
        let search_root = canonicalize_for_search_membership(search_root);
        self.files.iter().any(|file| {
            !file.path.as_os_str().is_empty()
                && is_within_search_root(&search_root, &file.path)
                && filters.matches(&self.project_root, &file.path)
        })
    }

    /// Score-rank file candidates and report whether the pre-filter step that
    /// collects candidates reached its internal size limit before ranking.
    pub fn lexical_rank_with_stats(
        &self,
        query_trigrams: &[u32],
        candidate_filter: Option<&dyn Fn(&Path) -> bool>,
        max_files: usize,
    ) -> LexicalRankResult {
        if query_trigrams.is_empty() || max_files == 0 {
            return LexicalRankResult::default();
        }

        let mut non_zero: Vec<(u32, usize)> = query_trigrams
            .iter()
            .filter_map(|trigram| {
                let posting_count = self.posting_count(*trigram);
                (posting_count > 0).then_some((*trigram, posting_count))
            })
            .collect();
        if non_zero.is_empty() {
            return LexicalRankResult::default();
        }

        non_zero.sort_unstable_by_key(|(_, posting_count)| *posting_count);
        let selected_count = non_zero.len().min(3);
        let candidate_cap = if selected_count == 3 { 200 } else { 500 };

        // Candidate discovery needs only the three rarest trigrams, while scoring
        // needs every query trigram. Materialize all lists once per query so both
        // phases reuse the same disk-backed postings instead of rereading them for
        // every candidate. Memory remains bounded by the query's posting lists.
        let postings_by_trigram = materialize_query_postings(self, query_trigrams);
        let mut candidate_ids = BTreeSet::new();
        for (trigram, _) in non_zero.iter().take(selected_count) {
            if let Some(postings) = postings_by_trigram.get(trigram) {
                candidate_ids.extend(postings.iter().copied());
            }
        }
        let pre_filter_candidate_count = candidate_ids.len();
        let engine_capped = pre_filter_candidate_count > candidate_cap;
        let filtered_candidates = candidate_ids
            .into_iter()
            .filter_map(|file_id| {
                self.files
                    .get(file_id as usize)
                    .map(|entry| (file_id, entry))
            })
            .filter(|(_, entry)| {
                if let Some(filter) = candidate_filter {
                    filter(&entry.path)
                } else {
                    true
                }
            })
            .collect::<Vec<_>>();

        let mut ranked = Vec::new();
        for (file_id, entry) in filtered_candidates.into_iter().take(candidate_cap) {
            let score =
                lexical_score_from_postings(self, query_trigrams, &postings_by_trigram, file_id);
            if score > 0.0 {
                ranked.push((entry.path.clone(), score));
            }
        }

        #[cfg(test)]
        crate::search_hot_path_measurements::record(|counts| {
            counts.candidates_sorted += ranked.len()
        });
        ranked.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        ranked.truncate(max_files);
        LexicalRankResult {
            files: ranked,
            engine_capped,
        }
    }

    /// Evaluates lexical candidates and scores them up to the specified depth limit, applying an optional candidate filter.
    pub fn lexical_rank_at_depth(
        &self,
        query_trigrams: &[u32],
        candidate_filter: Option<&dyn Fn(&Path) -> bool>,
        depth: usize,
    ) -> LexicalRankResult {
        if query_trigrams.is_empty() || depth == 0 {
            return LexicalRankResult::default();
        }

        let mut non_zero: Vec<(u32, usize)> = query_trigrams
            .iter()
            .filter_map(|trigram| {
                let posting_count = self.posting_count(*trigram);
                (posting_count > 0).then_some((*trigram, posting_count))
            })
            .collect();
        if non_zero.is_empty() {
            return LexicalRankResult::default();
        }

        non_zero.sort_unstable_by_key(|(_, posting_count)| *posting_count);
        let postings_by_trigram = materialize_query_postings(self, query_trigrams);

        let initial_selected = non_zero.len().min(3);
        let mut candidate_ids = BTreeSet::new();
        for (trigram, _) in non_zero.iter().take(initial_selected) {
            if let Some(postings) = postings_by_trigram.get(trigram) {
                candidate_ids.extend(postings.iter().copied());
            }
        }
        if candidate_ids.len() < depth {
            for (trigram, _) in non_zero.iter().skip(initial_selected) {
                if candidate_ids.len() >= depth {
                    break;
                }
                if let Some(postings) = postings_by_trigram.get(trigram) {
                    candidate_ids.extend(postings.iter().copied());
                }
            }
        }

        let candidate_cap = depth;
        let pre_filter_candidate_count = candidate_ids.len();
        let engine_capped = pre_filter_candidate_count > candidate_cap;

        let filtered_candidates = candidate_ids
            .into_iter()
            .filter_map(|file_id| {
                self.files
                    .get(file_id as usize)
                    .map(|entry| (file_id, entry))
            })
            .filter(|(_, entry)| {
                if let Some(filter) = candidate_filter {
                    filter(&entry.path)
                } else {
                    true
                }
            })
            .collect::<Vec<_>>();

        let mut ranked = Vec::new();
        for (file_id, entry) in filtered_candidates.into_iter().take(candidate_cap) {
            let score =
                lexical_score_from_postings(self, query_trigrams, &postings_by_trigram, file_id);
            if score > 0.0 {
                ranked.push((entry.path.clone(), score));
            }
        }

        #[cfg(test)]
        crate::search_hot_path_measurements::record(|counts| {
            counts.candidates_sorted += ranked.len()
        });
        ranked.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        ranked.truncate(depth);
        LexicalRankResult {
            files: ranked,
            engine_capped,
        }
    }

    /// Score the union of the three rarest nonempty postings without ranking
    /// each posting merely to discover its membership. Trigram-value ties and
    /// candidate filtering match the canonical lexical lane's discovery rules.
    pub(crate) fn lexical_selected_pool(
        &self,
        query_trigrams: &[u32],
        candidate_filter: Option<&dyn Fn(&Path) -> bool>,
    ) -> Vec<(PathBuf, f32)> {
        let mut seen = HashSet::with_capacity(query_trigrams.len());
        let unique = query_trigrams
            .iter()
            .copied()
            .filter(|trigram| seen.insert(*trigram))
            .collect::<Vec<_>>();
        let postings = materialize_query_postings(self, &unique);
        let mut memberships = postings
            .iter()
            .filter(|(_, ids)| !ids.is_empty())
            .collect::<Vec<_>>();
        memberships.sort_unstable_by_key(|(trigram, ids)| (ids.len(), **trigram));
        let selected = memberships
            .iter()
            .take(3)
            .flat_map(|(_, ids)| ids.iter().copied())
            .collect::<BTreeSet<_>>();
        let mut ranked = selected
            .into_iter()
            .filter_map(|id| {
                let entry = self.files.get(id as usize)?;
                if candidate_filter.is_some_and(|include| !include(&entry.path)) {
                    return None;
                }
                let score = lexical_score_from_postings(self, &unique, &postings, id);
                (score > 0.0).then(|| (entry.path.clone(), score))
            })
            .collect::<Vec<_>>();
        #[cfg(test)]
        crate::search_hot_path_measurements::record(|counts| {
            counts.candidates_sorted += ranked.len()
        });
        ranked.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        ranked
    }

    /// Discovers and ranks lexical candidates up to the specified depth limit, applying an optional candidate filter.
    pub fn lexical_discovery_and_rank_at_depth(
        &self,
        query_trigrams: &[u32],
        candidate_filter: Option<&dyn Fn(&Path) -> bool>,
        depth: usize,
    ) -> LexicalRankResult {
        self.lexical_rank_at_depth(query_trigrams, candidate_filter, depth)
    }

    /// Whole-corpus exact pass over the trigram index in ready mode.
    /// Intersects trigrams for the verbatim phrase (E1) and for content tokens within a 3-line window (E2),
    /// verifies matches against file content, and records content digests.
    pub fn whole_corpus_exact_pass(
        &self,
        query: &str,
        search_root: &Path,
        candidate_filter: Option<&dyn Fn(&Path) -> bool>,
    ) -> Vec<ExactPassMatch> {
        self.whole_corpus_exact_pass_scoped(
            query,
            search_root,
            candidate_filter,
            &ExactEvidenceScope::All,
        )
    }

    /// [`Self::whole_corpus_exact_pass`] that skips the files unable to hold
    /// any evidence `scope` keeps. Every remaining file is verified as before,
    /// so the kept evidence is identical to the unscoped pass.
    pub fn whole_corpus_exact_pass_scoped(
        &self,
        query: &str,
        search_root: &Path,
        candidate_filter: Option<&dyn Fn(&Path) -> bool>,
        scope: &ExactEvidenceScope,
    ) -> Vec<ExactPassMatch> {
        let phrase = exact_lane::exact_phrase(query);
        let norm_phrase = exact_lane::normalize_exact_phrase(phrase);
        let content_tokens = exact_lane::exact_verification_tokens(query);
        let literal = crate::search_b2::router::is_hyphenated_literal(query);
        let mut literal_postings = HashMap::new();
        let member = exact_lane::qualified_member_name(query);

        let mut candidate_file_ids = BTreeSet::new();

        // (a) Verbatim phrase trigrams
        if phrase.len() >= 3 {
            let phrase_query = decompose_regex(&regex::escape(phrase));
            candidate_file_ids.extend(self.candidates(&phrase_query));
        }

        // (b) Content tokens trigrams (for E2 window match)
        if content_tokens.len() >= 2 {
            for token in &content_tokens {
                if token.len() >= 3 {
                    match self.files_possibly_containing(token, &mut literal_postings) {
                        Some(ids) => candidate_file_ids.extend(ids.iter().copied()),
                        None => candidate_file_ids.extend(self.active_file_ids()),
                    }
                }
            }
        }

        // (c) A `receiver.member` query: the member's declaration does not
        // contain the receiver, so files holding the member name are
        // candidates too. Verification gives them definition evidence only
        // when they declare it.
        let member_file_ids = member.as_ref().map(|member| {
            let member_query = decompose_regex(&regex::escape(&member.member));
            self.candidates(&member_query)
                .into_iter()
                .collect::<BTreeSet<_>>()
        });
        if let Some(ids) = &member_file_ids {
            candidate_file_ids.extend(ids.iter().copied());
        }

        // If no candidate trigrams (e.g. short query), check active files. A
        // hyphenated literal can only match through its own phrase trigrams,
        // so an empty candidate set already proves no file contains it.
        if candidate_file_ids.is_empty()
            && (phrase.len() < 3 || (content_tokens.is_empty() && !literal))
        {
            candidate_file_ids.extend(self.active_file_ids());
        }

        let unscoped_candidates = candidate_file_ids.len();
        if let Some(bound) =
            self.exact_evidence_bound(scope, &norm_phrase, &content_tokens, &mut literal_postings)
        {
            // The evidence bound is built from the query's own words, which a
            // member's declaration does not contain, so member files stay.
            candidate_file_ids.retain(|file_id| {
                bound.binary_search(file_id).is_ok()
                    || member_file_ids
                        .as_ref()
                        .is_some_and(|ids| ids.contains(file_id))
            });
        }

        let started = Instant::now();
        let candidate_count = candidate_file_ids.len();
        let mut files_read = 0usize;
        let mut bytes_read = 0usize;
        let mut files_hashed = 0usize;
        let mut matches = Vec::new();
        let search_root_canon = canonicalize_for_search_membership(search_root);

        for file_id in candidate_file_ids {
            let Some(file_entry) = self.files.get(file_id as usize) else {
                continue;
            };
            if file_entry.path.as_os_str().is_empty() {
                continue;
            }

            // Canonicalizing costs a filesystem call per file, so it is only
            // done when the stored path alone does not place the file in scope.
            let in_scope = is_within_search_root(search_root, &file_entry.path)
                || is_within_search_root(&search_root_canon, &file_entry.path)
                || is_within_search_root(
                    &search_root_canon,
                    &canonicalize_for_search_membership(&file_entry.path),
                );
            if !in_scope {
                continue;
            }

            if let Some(filter) = candidate_filter {
                if !filter(&file_entry.path) {
                    continue;
                }
            }

            files_read += 1;
            let SearchCorpusEligibility::Eligible(file) =
                read_search_corpus_file(&file_entry.path, self.max_file_size)
            else {
                continue;
            };
            bytes_read += file.bytes.len();
            let text = String::from_utf8_lossy(&file.bytes);

            if let Some(candidates) = exact_lane::verify_exact_matches_in_text_with_member(
                &file_entry.path,
                &text,
                &norm_phrase,
                &content_tokens,
                member.as_ref().map(|member| member.member.as_str()),
            ) {
                // Only a matching file needs its digest: the memo re-checks
                // the digests of the files it serves, never of the rest.
                files_hashed += 1;
                let digest = blake3::hash(&file.bytes).to_hex().to_string();
                for mut cand in candidates {
                    cand.evidence.generated = file.generated;
                    matches.push(ExactPassMatch {
                        path: cand.path,
                        symbol_range: cand.symbol_range,
                        evidence: cand.evidence,
                        content_digest: digest.clone(),
                    });
                }
            }
        }

        crate::slog_debug!(
            "exact pass: {} of {} candidate files kept for {:?}, {} read, {} bytes, {} hashed, {} matches in {:?}",
            candidate_count,
            unscoped_candidates,
            scope,
            files_read,
            bytes_read,
            files_hashed,
            matches.len(),
            started.elapsed()
        );
        matches
    }

    /// The sorted ids of every file that can hold evidence `scope` keeps, or
    /// `None` when the index cannot rule any file out.
    ///
    /// Each kind of evidence needs certain words in the file, compared ASCII
    /// case-insensitively like the trigram index: a verbatim phrase needs every
    /// word of the phrase, a window needs every query word, and a declaration
    /// needs the query word it is named after plus, when the scope lists
    /// identifier words, one of those words. Phrase words are checked one by
    /// one because the phrase is compared after collapsing whitespace, so a
    /// hit may span a line break that the phrase's own trigrams would miss.
    fn exact_evidence_bound(
        &self,
        scope: &ExactEvidenceScope,
        norm_phrase: &str,
        content_tokens: &[String],
        literal_postings: &mut HashMap<String, Option<Vec<u32>>>,
    ) -> Option<Vec<u32>> {
        let (keeps_window, declaration_mentions) = match scope {
            ExactEvidenceScope::All => return None,
            ExactEvidenceScope::Phrase => (false, None),
            ExactEvidenceScope::PhraseAndWindow => (true, None),
            ExactEvidenceScope::DefinitionsMentioning(tokens) => (true, Some(tokens.as_slice())),
        };

        let mut bound = Vec::new();
        if !norm_phrase.is_empty() {
            let phrase_files =
                self.files_possibly_containing_all(norm_phrase.split(' '), literal_postings)?;
            bound = union_sorted_ids(&bound, &phrase_files);
        }
        if keeps_window && content_tokens.len() >= 2 {
            let window_files = self.files_possibly_containing_all(
                content_tokens.iter().map(String::as_str),
                literal_postings,
            )?;
            bound = union_sorted_ids(&bound, &window_files);
        }
        if let Some(mentions) = declaration_mentions {
            if !content_tokens.is_empty() && !mentions.is_empty() {
                let named = self.files_possibly_containing_any(
                    content_tokens.iter().map(String::as_str),
                    literal_postings,
                );
                let mentioning = self.files_possibly_containing_any(
                    mentions.iter().map(String::as_str),
                    literal_postings,
                );
                let declaration_files = match (named, mentioning) {
                    (Some(named), Some(mentioning)) => intersect_sorted_ids(&named, &mentioning),
                    (Some(files), None) | (None, Some(files)) => files,
                    (None, None) => return None,
                };
                bound = union_sorted_ids(&bound, &declaration_files);
            }
        }
        Some(bound)
    }

    /// Files containing every one of `words`, or `None` when no word is long
    /// enough to have a trigram.
    fn files_possibly_containing_all<'a>(
        &self,
        words: impl Iterator<Item = &'a str>,
        literal_postings: &mut HashMap<String, Option<Vec<u32>>>,
    ) -> Option<Vec<u32>> {
        let mut files: Option<Vec<u32>> = None;
        for word in words {
            let Some(word_files) = self.files_possibly_containing(word, literal_postings) else {
                continue;
            };
            files = Some(match files {
                Some(files) => intersect_sorted_ids(&files, word_files),
                None => word_files.clone(),
            });
            if files.as_ref().is_some_and(Vec::is_empty) {
                break;
            }
        }
        files
    }

    /// Files containing at least one of `words`, or `None` when some word is
    /// too short to have a trigram and so could be in any file.
    fn files_possibly_containing_any<'a>(
        &self,
        words: impl Iterator<Item = &'a str>,
        literal_postings: &mut HashMap<String, Option<Vec<u32>>>,
    ) -> Option<Vec<u32>> {
        let mut files = Vec::new();
        for word in words {
            let word_files = self.files_possibly_containing(word, literal_postings)?;
            files = union_sorted_ids(&files, word_files);
        }
        Some(files)
    }

    /// The sorted ids of files that may contain `literal`, ASCII
    /// case-insensitively, from its trigram postings; `None` when the literal
    /// has no trigram. Lookups are cached per pass because one word can serve
    /// several kinds of evidence.
    fn files_possibly_containing<'a>(
        &self,
        literal: &str,
        literal_postings: &'a mut HashMap<String, Option<Vec<u32>>>,
    ) -> Option<&'a Vec<u32>> {
        literal_postings
            .entry(literal.to_string())
            .or_insert_with(|| {
                let query = decompose_regex(&regex::escape(literal));
                (!query.and_trigrams.is_empty() || !query.or_groups.is_empty())
                    .then(|| self.candidates(&query))
            })
            .as_ref()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Posting {
    pub file_id: u32,
    pub next_mask: u8,
    pub loc_mask: u8,
}

#[derive(Clone, Debug)]
pub struct FileEntry {
    pub path: PathBuf,
    pub size: u64,
    pub modified: SystemTime,
    pub content_hash: blake3::Hash,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GrepMatch {
    pub file: PathBuf,
    pub line: u32,
    pub column: u32,
    pub line_text: String,
    pub match_text: String,
}

#[derive(Clone, Debug)]
pub struct GrepResult {
    pub matches: Vec<GrepMatch>,
    pub total_matches: usize,
    pub files_searched: usize,
    pub files_with_matches: usize,
    pub index_status: IndexStatus,
    pub truncated: bool,
    pub fully_degraded: bool,
    pub engine_capped: bool,
    /// True when a fallback directory walk stopped early due to file-count or time budget.
    pub walk_truncated: bool,
    /// Foreign filesystem mounts skipped by a fallback walk before they were opened.
    pub skipped_foreign_mounts: usize,
    /// Indexed candidate files that were not on disk when grep went to read
    /// them: stale index entries. They can never produce a match; the count
    /// only discloses that the index listed them.
    pub missing_on_disk: usize,
    /// True when the scan stopped at its wall-clock budget before every
    /// candidate file was searched, so matches may be missing.
    pub scan_deadline_reached: bool,
    /// Files read straight from disk rather than through the trigram index:
    /// the files a fallback walk searched, or a file named as the search path.
    pub files_read_directly: usize,
    /// Why a fallback walk stopped before reaching every file, when it did.
    /// `None` for an index answer and for a walk that covered every file.
    pub walk_bound: Option<WalkBound>,
}

/// The limit that stopped a fallback directory walk early. Files the walk
/// never reached were not searched, so a missing match is not evidence of
/// absence.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WalkBound {
    /// The walk's wall-clock budget ran out.
    TimeBudget,
    /// The walk found more eligible files than it may search; the value is
    /// that file limit.
    FileLimit(usize),
    /// The request was cancelled while the walk was running.
    Cancelled,
}

/// Bounds on how much work [`SearchIndexSnapshot::collect_grep_matches_by_file`]
/// may do. They limit the files examined and the time spent, never the number
/// of matches found, so the caller can rank every file that was examined.
#[derive(Clone, Copy, Debug)]
pub(crate) struct GrepExaminationLimits {
    /// Most candidate files read and verified.
    pub max_files: usize,
    /// Wall-clock time after which no further file is started.
    pub budget: Duration,
    /// Most matching lines kept per file, apart from lines the caller asks to
    /// keep. Every matching line is still counted.
    pub max_lines_per_file: usize,
}

/// Every matching line found in one examined file.
#[derive(Clone, Debug)]
pub(crate) struct GrepFileMatches {
    pub path: PathBuf,
    pub modified: SystemTime,
    /// The first `max_lines_per_file` matching lines, and any later line the
    /// caller asked to keep, in file order.
    pub matches: Vec<GrepMatch>,
    /// All matching lines in the file, including the ones not kept.
    pub matched_lines: usize,
}

/// Result of [`SearchIndexSnapshot::collect_grep_matches_by_file`].
#[derive(Clone, Debug)]
pub(crate) struct GrepFileCollection {
    /// Files with at least one match, in examination order.
    pub files: Vec<GrepFileMatches>,
    /// Candidate files the trigram index admitted inside the search scope.
    pub candidate_files: usize,
    /// Candidate files actually read and verified.
    pub files_examined: usize,
    /// True when the file-count bound, the time budget or a cancellation left
    /// candidate files unexamined.
    pub examination_capped: bool,
    /// True when the pattern yielded no trigram, so every indexed file was a
    /// candidate.
    pub fully_degraded: bool,
    pub index_status: IndexStatus,
    pub missing_on_disk: usize,
}

#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct GrepQueryPhaseTimings {
    pub trigram_lookup: Duration,
    pub pread_verify: Duration,
    pub post_filter: Duration,
    pub candidate_count: usize,
    pub bytes_verified: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IndexStatus {
    Ready,
    Building,
    Fallback,
    Disabled,
}

impl IndexStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            IndexStatus::Ready => "Ready",
            IndexStatus::Building => "Building",
            IndexStatus::Fallback => "Fallback",
            IndexStatus::Disabled => "Disabled",
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct RegexQuery {
    pub and_trigrams: Vec<u32>,
    pub or_groups: Vec<Vec<u32>>,
    pub(crate) and_filters: HashMap<u32, PostingFilter>,
    pub(crate) or_filters: Vec<HashMap<u32, PostingFilter>>,
}

#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct PostingFilter {
    next_mask: u8,
    loc_mask: u8,
}

#[derive(Clone, Copy)]
pub(crate) struct SearchFileMetadata {
    size: u64,
    modified: SystemTime,
}

struct PreparedIndexedFile {
    metadata: SearchFileMetadata,
    content_hash: blake3::Hash,
    trigram_map: BTreeMap<u32, PostingFilter>,
}

pub(crate) struct SearchCorpusFile {
    metadata: SearchFileMetadata,
    pub(crate) bytes: Vec<u8>,
    pub(crate) generated: bool,
}

pub(crate) enum SearchCorpusEligibility {
    Eligible(SearchCorpusFile),
    Unindexed(SearchFileMetadata),
    Skipped,
}

enum PreparedSearchPath {
    Indexed(PreparedIndexedFile),
    Unindexed(SearchFileMetadata),
    Skipped,
}

/// Prepared outside the index write lock. Applying this value only changes RAM
/// tables; path resolution, metadata, bytes, hashing and trigram extraction have
/// already finished.
pub(crate) struct PreparedSearchUpdate {
    path: PathBuf,
    file: PreparedSearchPath,
}

pub(crate) fn prepare_search_update(
    path: &Path,
    max_file_size: u64,
    remove: bool,
) -> PreparedSearchUpdate {
    let path = canonicalize_existing_or_deleted_path_with_memo(
        path,
        &mut ParentCanonicalizationMemo::default(),
    );
    let file = if remove {
        PreparedSearchPath::Skipped
    } else {
        prepare_search_path(&path, max_file_size)
    };
    PreparedSearchUpdate { path, file }
}

#[derive(Clone, Debug, Default)]
struct QueryBuild {
    and_runs: Vec<Vec<u8>>,
    or_groups: Vec<Vec<Vec<u8>>>,
}

pub type GrepPathExclusion = fn(&Path, &Path) -> bool;

/// Which matching lines a grep keeps once it passes its result cap.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum GrepScanOrder {
    /// Files are searched in parallel and the first lines found are kept:
    /// fast, but which lines are kept past the cap depends on scheduling.
    #[default]
    Arrival,
    /// Files are sorted by path and searched in batches of
    /// [`PATH_ORDER_BATCH_FILES`], in parallel within a batch. The scan stops
    /// after the first batch that brings the kept lines to the cap, and the
    /// kept lines are the first ones by path, then line. Whole batches finish
    /// before the cut, so the same lines are kept whatever order files finish
    /// in; only a scan stopped early (its deadline, cancellation, or a
    /// caller's file limit) can keep different lines.
    Path,
}

/// Files per batch of a path-ordered grep scan. Each batch ends in a barrier
/// where the scan checks the cap, so a batch should hold many files per
/// worker thread (one slow file then idles the others for only a small part
/// of the batch) while a scan past the cap reads at most one batch of files
/// it does not need. 256 is the batch the fallback walk already hands to its
/// parallel search: 16 or more files per thread on machines with up to 16
/// threads.
pub(crate) const PATH_ORDER_BATCH_FILES: usize = 256;

/// Rows the files of one path-ordered batch may hold together, on top of the
/// lines already kept. Every file of a batch may keep at most its share of
/// this budget (and never more than the cap still needs), so a batch of files
/// that each match on most of their lines cannot hold hundreds of times the
/// rows the cap keeps before the batch is cut. A file that has more matching
/// lines than its share, at a point where the cap still needs more than that
/// share from it, is searched again on its own for exactly the lines still
/// needed; the kept lines are the same as if every file had kept all the
/// cap needs. A share is never below one line, so a budget smaller than the
/// batch can be passed by up to one row per file.
///
/// 1,048,576 rows is 4,096 lines per file in a full batch of 256. Measured
/// on release builds: a capped grep of 256 files of 100,000 matching lines
/// peaks at about 460 MB (walk) and 200 MB (index) with this budget, against
/// 8.7 GB and 3.0 GB when every file kept all the cap needs, and it is no
/// slower on real projects, where only the few files matching on more than
/// 4,096 lines are ever searched twice. Keeping batches 256 files wide (rather
/// than narrowing them while the cap still needs many lines) keeps a scan of
/// many files with few matches each as fast as before.
pub(crate) const PATH_ORDER_BATCH_ROW_BUDGET: usize = 1 << 20;

/// Most lines one file of a path-ordered batch of `batch_files` files keeps:
/// its share of `row_budget` (at least one line), and never more than the
/// `remaining` lines the cap still needs.
pub(crate) fn path_order_file_allowance(
    remaining: usize,
    batch_files: usize,
    row_budget: usize,
) -> usize {
    (row_budget / batch_files.max(1)).max(1).min(remaining)
}

/// Adds one path-ordered batch to `kept`, which holds the lines kept from
/// earlier batches, until `kept` holds `max_results` lines.
///
/// `per_file` holds, for each file of the batch in path order, the rows it
/// kept under `allowance` and whether it has matching lines past them. When
/// such a file is reached while the cap needs more lines from it than it
/// kept, `search_again(file, need)` searches it on its own for at most
/// `need` lines and returns the same pair. `held(rows)` is told how many rows
/// are held at once (kept plus not yet taken) whenever that number grows.
///
/// Returns whether matching lines exist past the ones kept from this batch.
pub(crate) fn keep_path_ordered_batch<T>(
    kept: &mut Vec<T>,
    max_results: usize,
    allowance: usize,
    per_file: Vec<(Vec<T>, bool)>,
    mut search_again: impl FnMut(usize, usize) -> (Vec<T>, bool),
    mut held: impl FnMut(usize),
) -> bool {
    let mut not_taken: usize = per_file.iter().map(|(rows, _)| rows.len()).sum();
    held(kept.len() + not_taken);
    let mut lines_past = false;
    for (file, (rows, more)) in per_file.into_iter().enumerate() {
        not_taken -= rows.len();
        let need = max_results.saturating_sub(kept.len());
        if need == 0 {
            lines_past |= more || !rows.is_empty();
            continue;
        }
        let (rows, more) = if more && rows.len() == allowance && allowance < need {
            // The file stopped at its share of the batch budget, and the cap
            // needs more of its lines than that.
            drop(rows);
            let again = search_again(file, need);
            held(kept.len() + not_taken + again.0.len());
            again
        } else {
            (rows, more)
        };
        lines_past |= more || rows.len() > need;
        kept.extend(rows.into_iter().take(need));
    }
    lines_past
}

/// Records the most rows a path-ordered scan of a search root held at once,
/// so tests can prove the scan's memory stays within its row budget
/// whatever the files hold. Outside tests it does nothing.
pub(crate) fn note_path_order_held_rows(search_root: &Path, rows: usize) {
    #[cfg(test)]
    path_order_held_rows_for_test::note(search_root, rows);
    #[cfg(not(test))]
    let _ = (search_root, rows);
}

/// Test-only record of the most rows a path-ordered scan held at once, by
/// search root.
#[cfg(test)]
pub(crate) mod path_order_held_rows_for_test {
    use std::collections::HashMap;
    use std::path::{Path, PathBuf};
    use std::sync::{Mutex, OnceLock};

    static PEAKS: OnceLock<Mutex<HashMap<PathBuf, usize>>> = OnceLock::new();

    pub(crate) fn note(root: &Path, rows: usize) {
        if let Ok(mut peaks) = PEAKS.get_or_init(Default::default).lock() {
            let peak = peaks.entry(root.to_path_buf()).or_default();
            *peak = (*peak).max(rows);
        }
    }

    /// The most rows held at once since the last call, and resets it.
    pub(crate) fn take(root: &Path) -> usize {
        PEAKS
            .get_or_init(Default::default)
            .lock()
            .ok()
            .and_then(|mut peaks| peaks.remove(root))
            .unwrap_or(0)
    }
}

#[derive(Clone, Debug, Default)]
pub(crate) struct PathFilters {
    includes: Option<GlobSet>,
    excludes: Option<GlobSet>,
}

#[derive(Clone, Debug)]
pub(crate) struct SearchScope {
    pub root: PathBuf,
    pub use_index: bool,
}

#[derive(Clone, Debug)]
struct SharedGrepMatch {
    file: Arc<PathBuf>,
    line: u32,
    column: u32,
    line_text: String,
    match_text: String,
}

#[derive(Clone, Debug)]
enum SearchMatcher {
    Literal(LiteralSearch),
    Regex(Regex),
}

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
enum IgnoreRulesLoadPolicy {
    Strict,
    BorrowTolerant,
}

impl SearchIndex {
    pub fn new() -> Self {
        SearchIndex {
            base: None,
            delta: Arc::new(DeltaState::default()),
            delta_file_trigrams: HashMap::new(),
            free_delta_file_ids: Vec::new(),
            files: Arc::new(Vec::new()),
            path_to_id: Arc::new(HashMap::new()),
            ready: false,
            build_denied: false,
            project_root: PathBuf::new(),
            git_head: None,
            max_file_size: DEFAULT_MAX_FILE_SIZE,
            ignore_rules_fingerprint: String::new(),
            file_trigram_count: Arc::new(Vec::new()),
            unindexed_files: Arc::new(HashSet::new()),
            base_file_count: 0,
            delta_packed_bytes: 0,
            compaction_state: Arc::new(Mutex::new(CompactionState::default())),
        }
    }

    pub fn build(root: &Path) -> Self {
        Self::build_with_limit(root, DEFAULT_MAX_FILE_SIZE)
    }

    /// An empty in-memory corpus to populate through a bounded disk walk.
    /// Marking it ready prevents query lanes from starting an unbounded build;
    /// the caller must disclose the walk's coverage separately.
    pub(crate) fn empty_ready_for_root(root: &Path) -> Self {
        Self {
            project_root: root.to_path_buf(),
            ready: true,
            ..Self::new()
        }
    }

    pub fn build_with_limit(root: &Path, max_file_size: u64) -> Self {
        let started = Instant::now();
        let cache_dir = transient_search_cache_dir(root);
        // The streaming index keeps its postings in cache.bin, so this scratch
        // directory cannot be replaced by a purely in-memory build. A directory
        // embeds this process's PID, so only threads in this process can share it;
        // serialize those threads and clear the previous files before rebuilding
        // instead of minting an unbounded timestamped path.
        let build_lock = transient_search_cache_build_lock(&cache_dir);
        let _build_guard = build_lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Err(error) = truncate_transient_search_cache_dir(&cache_dir) {
            log::warn!(
                "search index: could not reset transient cache {} ({}); falling back to bounded in-memory delta",
                cache_dir.display(),
                error
            );
            return Self::build_in_memory(root, max_file_size, started);
        }
        Self::build_with_limit_to_cache_dir(root, max_file_size, &cache_dir)
    }

    pub fn build_with_limit_to_cache_dir(
        root: &Path,
        max_file_size: u64,
        cache_dir: &Path,
    ) -> Self {
        Self::build_with_limit_to_cache_dir_cancellable(root, max_file_size, cache_dir, &|| true)
            .expect("uncancellable search build")
    }

    /// Cancelled refreshes must neither publish a partial index nor start a
    /// second, in-memory walk through the streaming-error fallback.
    pub(crate) fn build_with_limit_to_cache_dir_cancellable(
        root: &Path,
        max_file_size: u64,
        cache_dir: &Path,
        keep_going: &(dyn Fn() -> bool + Sync),
    ) -> Option<Self> {
        if !keep_going() {
            return None;
        }
        let started = std::time::Instant::now();
        // Persistent builders already receive configure's keyed directory.
        // Its identity must also label build events when Git changed since bind.
        let key = cache_dir
            .parent()
            .filter(|parent| parent.file_name().is_some_and(|name| name == "index"))
            .and_then(|_| cache_dir.file_name())
            .and_then(|name| name.to_str())
            .map(str::to_owned)
            .unwrap_or_else(|| artifact_cache_key_for_build(root));
        let scope =
            crate::logging::IndexBuildScope::new(crate::logging::IndexPlane::Search, root, key);
        let _index_build = crate::logging::install_index_build(scope.clone());
        let mut failure_guard = crate::logging::IndexBuildFailureGuard::new();
        crate::logging::log_index_event(crate::logging::IndexEvent::from_scope(
            crate::logging::IndexEventKind::BuildStarted,
            &scope,
        ));
        if !artifact_write_allowed(root, cache_dir, &cache_dir.join("cache.bin")) {
            // Write-denied roots cannot persist or materialize a real index.
            // Return an empty index flagged as build-denied (ready stays false
            // so grep/glob keep using the bounded fallback walk). Health reads
            // the flag to avoid reporting a permanent "building" state for a
            // build that was never going to run here.
            crate::slog_info!(
                    "search index cold build denied: {} may not write the cache artifact at {}; reporting build-denied instead of building",
                    root.display(),
                    cache_dir.display()
                );
            crate::logging::log_index_event(
                crate::logging::IndexEvent::from_scope(
                    crate::logging::IndexEventKind::BuildFailed,
                    &scope,
                )
                .field("reason", "denied"),
            );
            failure_guard.disarm();
            let mut index = Self::new();
            index.project_root = root.to_path_buf();
            index.max_file_size = max_file_size;
            index.build_denied = true;
            return Some(index);
        }
        crate::logging::log_index_event(
            crate::logging::IndexEvent::from_scope(
                crate::logging::IndexEventKind::BuildProgress,
                &scope,
            )
            .field("stage", "streaming")
            .field("completed", 0)
            .field("total", 1)
            .field("elapsed_ms", scope.elapsed_ms()),
        );
        match build_streaming_index(root, max_file_size, cache_dir, keep_going) {
            Ok((mut index, indexed)) => {
                index.ready = true;
                crate::slog_info!(
                    "search index cold streaming build: {} files, {} trigrams, {} ms (pool={})",
                    indexed,
                    index.trigram_count(),
                    started.elapsed().as_millis(),
                    search_index_build_pool_size()
                );
                crate::logging::log_index_event(
                    crate::logging::IndexEvent::from_scope(
                        crate::logging::IndexEventKind::BuildProgress,
                        &scope,
                    )
                    .field("stage", "streaming")
                    .field("completed", 1)
                    .field("total", 1)
                    .field("elapsed_ms", scope.elapsed_ms()),
                );
                crate::logging::log_index_event(
                    crate::logging::IndexEvent::from_scope(
                        crate::logging::IndexEventKind::BuildReady,
                        &scope,
                    )
                    .field("elapsed_ms", scope.elapsed_ms())
                    .field("files", indexed)
                    .field("trigrams", index.trigram_count()),
                );
                failure_guard.disarm();
                Some(index)
            }
            Err(error) => {
                if error.kind() == std::io::ErrorKind::Interrupted || !keep_going() {
                    crate::logging::log_index_event(crate::logging::IndexEvent::from_scope(
                        crate::logging::IndexEventKind::BuildCancelled,
                        &scope,
                    ));
                    failure_guard.disarm();
                    return None;
                }
                log::warn!(
                        "search index: streaming build failed ({}); falling back to bounded in-memory delta",
                        error
                    );
                crate::logging::log_index_event(
                    crate::logging::IndexEvent::from_scope(
                        crate::logging::IndexEventKind::BuildProgress,
                        &scope,
                    )
                    .field("stage", "fallback")
                    .field("completed", 0)
                    .field("total", 1)
                    .field("elapsed_ms", scope.elapsed_ms()),
                );
                let Some(index) =
                    Self::build_in_memory_cancellable(root, max_file_size, started, keep_going)
                else {
                    crate::logging::log_index_event(crate::logging::IndexEvent::from_scope(
                        crate::logging::IndexEventKind::BuildCancelled,
                        &scope,
                    ));
                    failure_guard.disarm();
                    return None;
                };
                crate::logging::log_index_event(
                    crate::logging::IndexEvent::from_scope(
                        crate::logging::IndexEventKind::BuildReady,
                        &scope,
                    )
                    .field("elapsed_ms", scope.elapsed_ms())
                    .field("files", index.files.len())
                    .field("trigrams", index.trigram_count()),
                );
                failure_guard.disarm();
                Some(index)
            }
        }
    }

    fn build_in_memory(root: &Path, max_file_size: u64, started: Instant) -> Self {
        Self::build_in_memory_cancellable(root, max_file_size, started, &|| true)
            .expect("uncancellable in-memory search build")
    }

    fn build_in_memory_cancellable(
        root: &Path,
        max_file_size: u64,
        started: Instant,
        keep_going: &(dyn Fn() -> bool + Sync),
    ) -> Option<Self> {
        let _progress = crate::cold_build_limiter::progress::start(
            root,
            "trigram build",
            None,
            crate::cold_build_limiter::progress::StartLog::Info,
        );
        crate::cold_build_limiter::progress::phase("enumerating", None);
        let project_root = fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
        let mut index = SearchIndex {
            project_root: project_root.clone(),
            max_file_size,
            ignore_rules_fingerprint: ignore_rules_fingerprint_cancellable(
                &project_root,
                keep_going,
            )?,
            ..SearchIndex::new()
        };
        let filters = PathFilters::default();
        let paths = walk_build_files_cancellable(&index.project_root, &filters, keep_going)?;
        crate::cold_build_limiter::progress::phase("indexing", Some(paths.len()));
        let indexed = index.ingest_paths_parallel_cancellable(&paths, keep_going);
        if !keep_going() {
            return None;
        }
        index.git_head = current_git_head(&index.project_root);
        index.ready = true;
        crate::slog_info!(
            "search index fallback build: {} files, {} trigrams, {} ms (pool={})",
            indexed,
            index.trigram_count(),
            started.elapsed().as_millis(),
            search_index_build_pool_size()
        );
        Some(index)
    }

    /// Serial cold build for tests and parity checks against [`build_with_limit`].
    #[cfg(test)]
    pub fn build_with_limit_serial(root: &Path, max_file_size: u64) -> Self {
        let project_root = fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
        let mut index = SearchIndex {
            project_root: project_root.clone(),
            max_file_size,
            ignore_rules_fingerprint: ignore_rules_fingerprint(&project_root),
            ..SearchIndex::new()
        };
        let filters = PathFilters::default();
        for path in walk_project_files(&project_root, &filters) {
            index.update_file(&path);
        }
        index.git_head = current_git_head(&project_root);
        index.ready = true;
        index
    }

    fn ingest_paths_parallel_cancellable(
        &mut self,
        paths: &[PathBuf],
        keep_going: &(dyn Fn() -> bool + Sync),
    ) -> usize {
        let max_file_size = self.max_file_size;
        let pool_size = search_index_build_pool_size();
        let chunk_size = pool_size.saturating_mul(4).clamp(1, 32);
        let pool = match rayon::ThreadPoolBuilder::new()
            .num_threads(pool_size)
            .thread_name(|index| format!("aft-search-build-{index}"))
            .stack_size(8 * 1024 * 1024)
            .build()
        {
            Ok(pool) => Some(pool),
            Err(error) => {
                log::warn!(
                    "search index: bounded build pool unavailable ({error}); using global pool"
                );
                None
            }
        };

        let mut indexed = 0usize;
        for chunk in paths.chunks(chunk_size) {
            if !keep_going() {
                break;
            }
            let prepare_chunk = || -> Vec<PreparedSearchPath> {
                chunk
                    .par_iter()
                    .map(|path| {
                        if keep_going() {
                            prepare_search_path(path, max_file_size)
                        } else {
                            PreparedSearchPath::Skipped
                        }
                    })
                    .collect()
            };
            let prepared = match &pool {
                Some(pool) => pool.install(prepare_chunk),
                None => prepare_chunk(),
            };

            for (path, prepared) in chunk.iter().zip(prepared) {
                let inserted = match prepared {
                    PreparedSearchPath::Indexed(file) => self.index_prepared_new_file(path, file),
                    PreparedSearchPath::Unindexed(metadata) => {
                        self.track_unindexed_file_with_metadata(path, metadata)
                    }
                    PreparedSearchPath::Skipped => false,
                };
                if inserted {
                    indexed += 1;
                }
            }
            crate::cold_build_limiter::progress::advance(chunk.len());
        }

        indexed
    }

    pub fn index_file(&mut self, path: &Path, content: &[u8]) {
        self.remove_file(path);
        let metadata = metadata_for_indexed_content(path, content.len() as u64);
        self.index_file_with_metadata(path, content, metadata);
    }

    fn index_file_with_metadata(
        &mut self,
        path: &Path,
        content: &[u8],
        metadata: SearchFileMetadata,
    ) -> bool {
        self.index_prepared_new_file(
            path,
            PreparedIndexedFile {
                metadata,
                content_hash: cache_freshness::hash_bytes(content),
                trigram_map: trigram_filter_map(content, true),
            },
        )
    }

    fn index_prepared_new_file(&mut self, path: &Path, file: PreparedIndexedFile) -> bool {
        let file_id = match self.allocate_file_id_with_metadata(path, file.metadata) {
            Some(file_id) => file_id,
            None => return false,
        };
        if let Some(entry) = Arc::make_mut(&mut self.files).get_mut(file_id as usize) {
            entry.content_hash = file.content_hash;
        }

        let mut file_trigrams = Vec::with_capacity(file.trigram_map.len());
        let delta = Arc::make_mut(&mut self.delta);
        for (trigram, filter) in file.trigram_map {
            let postings = delta.postings.entry(trigram).or_default();
            insert_delta_posting(
                postings,
                Posting {
                    file_id,
                    next_mask: filter.next_mask,
                    loc_mask: filter.loc_mask,
                },
            );
            file_trigrams.push(trigram);
        }

        let trigram_count = file_trigrams.len() as u32;
        self.delta_packed_bytes = self
            .delta_packed_bytes
            .saturating_add(file_trigrams.len().saturating_mul(POSTING_BYTES));
        self.delta_file_trigrams.insert(file_id, file_trigrams);
        ensure_count_slot(Arc::make_mut(&mut self.file_trigram_count), file_id);
        if let Some(count) = Arc::make_mut(&mut self.file_trigram_count).get_mut(file_id as usize) {
            *count = trigram_count;
        }
        Arc::make_mut(&mut self.unindexed_files).remove(&file_id);
        self.update_compaction_flags(Some(path));
        true
    }

    pub fn remove_file(&mut self, path: &Path) {
        let mut canonical_parents = ParentCanonicalizationMemo::default();
        self.remove_file_with_canonicalization_memo(path, &mut canonical_parents);
    }

    fn remove_file_with_canonicalization_memo(
        &mut self,
        path: &Path,
        canonical_parents: &mut ParentCanonicalizationMemo,
    ) {
        let canonical_path = self.respell_under_stored_root(
            canonicalize_existing_or_deleted_path_with_memo(path, canonical_parents),
        );
        self.remove_resolved_file(path, &canonical_path);
    }

    /// Spell a filesystem-canonical path the way this index's stored keys are
    /// spelled, so an exact key lookup can find it.
    ///
    /// On Windows, `fs::canonicalize` returns a verbatim (`\\?\`) path, but an
    /// index loaded by a read-only caller keeps a plain root and plain keys.
    /// Only the lookup key is respelled here; the stored root and the keys it
    /// owns keep the caller's convention. Elsewhere canonical paths already
    /// match the stored keys, so the path is returned unchanged.
    fn respell_under_stored_root(&self, path: PathBuf) -> PathBuf {
        #[cfg(windows)]
        if !self.project_root.as_os_str().is_empty() {
            let root = crate::windows_path::normalize_windows_path(&self.project_root);
            let normalized = crate::windows_path::normalize_windows_path(&path);
            if let Ok(relative) = normalized.strip_prefix(&root) {
                return self.project_root.join(relative);
            }
        }
        path
    }

    fn remove_resolved_file(&mut self, path: &Path, canonical_path: &Path) {
        let file_id = {
            let path_to_id = Arc::make_mut(&mut self.path_to_id);
            if let Some(file_id) = path_to_id.remove(path) {
                file_id
            } else if canonical_path != path {
                let Some(file_id) = path_to_id.remove(canonical_path) else {
                    return;
                };
                file_id
            } else {
                return;
            }
        };

        if file_id < self.base_file_count {
            let superseded_path = self
                .files
                .get(file_id as usize)
                .map(|entry| entry.path.clone())
                .filter(|path| !path.as_os_str().is_empty());
            let delta = Arc::make_mut(&mut self.delta);
            delta.superseded.insert(file_id);
            if let Some(path) = superseded_path {
                delta.superseded_paths.insert(path);
            }
        }

        if let Some(trigrams) = self.delta_file_trigrams.remove(&file_id) {
            self.delta_packed_bytes = self
                .delta_packed_bytes
                .saturating_sub(trigrams.len().saturating_mul(POSTING_BYTES));
            let delta = Arc::make_mut(&mut self.delta);
            for trigram in trigrams {
                let should_remove = if let Some(postings) = delta.postings.get_mut(&trigram) {
                    postings.retain(|posting| posting.file_id != file_id);
                    postings.is_empty()
                } else {
                    false
                };

                if should_remove {
                    delta.postings.remove(&trigram);
                }
            }
        }

        Arc::make_mut(&mut self.unindexed_files).remove(&file_id);
        if let Some(file) = Arc::make_mut(&mut self.files).get_mut(file_id as usize) {
            file.path = PathBuf::new();
            file.size = 0;
            file.modified = UNIX_EPOCH;
            file.content_hash = cache_freshness::zero_hash();
        }
        if file_id >= self.base_file_count {
            self.free_delta_file_ids.push(file_id);
        }
        if let Some(count) = Arc::make_mut(&mut self.file_trigram_count).get_mut(file_id as usize) {
            *count = 0;
        }
        self.update_compaction_flags(Some(path));
    }

    pub fn update_file(&mut self, path: &Path) {
        let mut canonical_parents = ParentCanonicalizationMemo::default();
        self.update_file_with_canonicalization_memo(path, &mut canonical_parents);
    }

    fn update_file_with_canonicalization_memo(
        &mut self,
        path: &Path,
        canonical_parents: &mut ParentCanonicalizationMemo,
    ) {
        // Watcher paths may use an alias of the configured root (for example,
        // macOS /var versus /private/var). Keep the replacement under the same
        // canonical key as the initial corpus build so scoped queries can see it.
        let canonical_path =
            canonicalize_existing_or_deleted_path_with_memo(path, canonical_parents);
        let file = prepare_search_path(&canonical_path, self.max_file_size);
        self.apply_search_update(PreparedSearchUpdate {
            path: canonical_path,
            file,
        });
    }

    pub(crate) fn max_file_size(&self) -> u64 {
        self.max_file_size
    }

    /// Whether `path` belongs to this index's corpus: it lies under the
    /// index root in a form [`Self::write_to_disk`] can store as a relative
    /// path. An index without a root (built in memory) admits every path.
    pub(crate) fn admits_path(&self, path: &Path) -> bool {
        self.project_root.as_os_str().is_empty()
            || storable_relative_path(&self.project_root, path).is_some()
    }

    pub(crate) fn apply_search_update(&mut self, update: PreparedSearchUpdate) {
        let PreparedSearchUpdate { path, file } = update;
        // Prepared updates use filesystem-canonical paths, but borrowed indexes
        // can have a plain Windows root. Match the stored root's spelling before
        // replacing its exact path key.
        let path = self.respell_under_stored_root(path);
        if !self.admits_path(&path) {
            // AFT's own writes report every path they touched, and a session
            // may edit files in a sibling checkout or a temporary directory.
            // Such a file is not part of this root's corpus, and one stored
            // entry outside the root would make every later `write_to_disk`
            // refuse the whole cache.
            return;
        }
        self.remove_resolved_file(&path, &path);
        match file {
            PreparedSearchPath::Indexed(file) => {
                self.index_prepared_new_file(&path, file);
            }
            PreparedSearchPath::Unindexed(metadata) => {
                self.track_unindexed_file_with_metadata(&path, metadata);
            }
            PreparedSearchPath::Skipped => {}
        }
    }

    pub fn grep(
        &self,
        pattern: &str,
        case_sensitive: bool,
        include: &[String],
        exclude: &[String],
        search_root: &Path,
        max_results: usize,
    ) -> GrepResult {
        self.snapshot().grep(
            pattern,
            case_sensitive,
            include,
            exclude,
            search_root,
            max_results,
        )
    }

    pub fn search_grep(
        &self,
        pattern: &CompiledPattern,
        include: &[String],
        exclude: &[String],
        search_root: &Path,
        max_results: usize,
    ) -> GrepResult {
        self.snapshot()
            .search_grep(pattern, include, exclude, search_root, max_results)
    }

    pub fn glob(&self, pattern: &str, search_root: &Path) -> Vec<PathBuf> {
        self.snapshot().glob(pattern, search_root)
    }

    pub fn candidates(&self, query: &RegexQuery) -> Vec<u32> {
        self.snapshot().candidates(query)
    }

    /// Persist the current base+delta to `cache.bin`.
    ///
    /// Borrow-only roots (linked worktrees, including those with
    /// `worktree.ram_overlay`) never take this path: `artifact_write_allowed`
    /// fail-closes before any bytes are written.
    pub fn write_to_disk(&mut self, cache_dir: &Path, git_head: Option<&str>) -> bool {
        if !artifact_write_allowed(&self.project_root, cache_dir, &cache_dir.join("cache.bin")) {
            return false;
        }
        let Some(plan) = CacheWritePlan::from_index(self, git_head) else {
            return false;
        };

        // A rewrite that folds a delta into an existing base is attributed
        // separately from a first build so the census can tell them apart.
        let domain = if self.base.is_some() {
            crate::write_ledger::Domain::SearchIndexDelta
        } else {
            crate::write_ledger::Domain::SearchIndexBuild
        };
        let write_result = {
            let mut sources = self.compaction_record_sources(Arc::clone(&plan.id_map));
            write_cache_file_from_sources(cache_dir, &plan, &mut sources, domain, &|| true)
        };

        match write_result {
            Ok(base) => {
                self.base = Some(Arc::new(base));
                self.delta = Arc::new(DeltaState::default());
                self.delta_file_trigrams.clear();
                self.free_delta_file_ids.clear();
                self.delta_packed_bytes = 0;
                self.base_file_count = u32::try_from(plan.files.len()).unwrap_or(u32::MAX);
                self.files = Arc::new(plan.files);
                self.path_to_id = Arc::new(plan.path_to_id);
                self.unindexed_files = Arc::new(plan.unindexed_files);
                self.file_trigram_count = Arc::new(plan.file_trigram_count);
                self.git_head = plan.git_head.filter(|head| !head.is_empty());
                self.ignore_rules_fingerprint = plan.ignore_fingerprint;
                true
            }
            Err(error) => {
                log::warn!("search index: failed to write disk cache: {}", error);
                false
            }
        }
    }

    /// Cheap validity probe for a persisted trigram cache: the cache file
    /// exists and its headers carry this build's magic and index version, so
    /// a reload would read it rather than rebuild. Reads a few header bytes
    /// only; it does not load or verify postings.
    pub(crate) fn disk_cache_header_valid(cache_dir: &Path) -> bool {
        if crate::root_cache::check_cache_access(cache_dir).is_err() {
            return false;
        }
        (|| -> Option<bool> {
            let cache_file = open_cache_file_read(&cache_dir.join("cache.bin")).ok()?;
            if cache_file.metadata().ok()?.len() < 16 {
                return Some(false);
            }
            let mut reader = BufReader::new(cache_file);
            if read_u32(&mut reader).ok()? != CACHE_MAGIC
                || read_u32(&mut reader).ok()? != INDEX_VERSION
            {
                return Some(false);
            }
            let _postings_len_total = read_u64(&mut reader).ok()?;
            let mut magic = [0u8; 8];
            reader.read_exact(&mut magic).ok()?;
            Some(&magic == INDEX_MAGIC && read_u32(&mut reader).ok()? == INDEX_VERSION)
        })()
        .unwrap_or(false)
    }

    pub fn read_from_disk(cache_dir: &Path, current_canonical_root: &Path) -> Option<Self> {
        crate::private_storage::tighten_keyed_dir(cache_dir, "index");
        Self::read_from_disk_with_options(cache_dir, current_canonical_root, true)
    }

    /// Refuse, by name, a `cache.bin` whose outer header carries a format
    /// version above [`INDEX_FORMAT_VERSION`] (or any cache while the storage
    /// root's reader floor is above it). Reads only the 8-byte header. An
    /// absent file, or one that is not a trigram cache at all, passes: those
    /// are ordinary misses and rebuilds.
    pub(crate) fn check_disk_cache_format(
        cache_dir: &Path,
    ) -> Result<(), crate::persisted_format::UnsupportedPersistedFormat> {
        let cache_path = cache_dir.join("cache.bin");
        let version = (|| {
            let mut reader = BufReader::new(open_cache_file_read(&cache_path).ok()?);
            if read_u32(&mut reader).ok()? != CACHE_MAGIC {
                return None;
            }
            read_u32(&mut reader).ok().map(u64::from)
        })();
        crate::persisted_format::gate(
            crate::persisted_format::PersistedStore::SearchIndex,
            &cache_path,
            &cache_path,
            version,
        )
    }

    pub(crate) fn read_from_disk_borrow_tolerant_cancellable(
        cache_dir: &Path,
        current_canonical_root: &Path,
        max_records: usize,
        duration: Duration,
        keep_going: &dyn Fn() -> bool,
    ) -> BorrowedIndexLoad {
        let load_started = Instant::now();
        let budget = BorrowedIndexLoadBudget::new(max_records, duration, keep_going);
        let outcome = match Self::read_from_disk_with_policy(
            cache_dir,
            current_canonical_root,
            false,
            IgnoreRulesLoadPolicy::BorrowTolerant,
            Some(&budget),
        ) {
            Some((index, ignore_rules_differ)) => {
                BorrowedIndexLoad::Loaded(index, ignore_rules_differ)
            }
            None => budget
                .stop
                .get()
                .map(BorrowedIndexLoad::Stopped)
                .unwrap_or(BorrowedIndexLoad::Invalid),
        };
        let outcome_label = match &outcome {
            BorrowedIndexLoad::Loaded(_, ignore_rules_differ) if *ignore_rules_differ => "partial",
            BorrowedIndexLoad::Loaded(_, _) => "ready",
            BorrowedIndexLoad::Stopped(BorrowedIndexLoadStop::BudgetExceeded) => "budget_stopped",
            BorrowedIndexLoad::Stopped(BorrowedIndexLoadStop::CoverageMismatch) => {
                "coverage_mismatch"
            }
            BorrowedIndexLoad::Stopped(BorrowedIndexLoadStop::Cancelled)
            | BorrowedIndexLoad::Invalid => "denied",
        };
        // Log the key of the artifact we opened. Adding Git to a folder can
        // produce a different key than the one under which its index was saved.
        let key = cache_dir.file_name().unwrap_or_default().to_string_lossy();
        let build_id = crate::logging::in_flight_build_id(
            crate::logging::IndexPlane::Search,
            current_canonical_root,
        )
        .unwrap_or_else(crate::logging::mint_index_build_id);
        crate::logging::log_index_event(
            crate::logging::IndexEvent::new(
                crate::logging::IndexEventKind::ArtifactLoaded,
                crate::logging::IndexPlane::Search,
                build_id,
                current_canonical_root,
                key,
            )
            .field("outcome", outcome_label)
            .field("borrowed", "true")
            .field(
                "elapsed_ms",
                load_started.elapsed().as_millis().min(u64::MAX as u128) as u64,
            ),
        );
        crate::logging::note_tool_call_wait(
            crate::run_tool_call::WaitingOn::ArtifactLoad,
            None,
            load_started.elapsed().as_millis().min(u64::MAX as u128) as u64,
        );
        outcome
    }

    fn read_from_disk_with_options(
        cache_dir: &Path,
        current_canonical_root: &Path,
        allow_legacy_repair: bool,
    ) -> Option<Self> {
        Self::read_from_disk_with_policy(
            cache_dir,
            current_canonical_root,
            allow_legacy_repair,
            IgnoreRulesLoadPolicy::Strict,
            None,
        )
        .map(|(index, _)| index)
    }

    fn read_from_disk_with_policy(
        cache_dir: &Path,
        current_canonical_root: &Path,
        allow_legacy_repair: bool,
        ignore_rules_load_policy: IgnoreRulesLoadPolicy,
        borrowed_load_budget: Option<&BorrowedIndexLoadBudget>,
    ) -> Option<(Self, bool)> {
        crate::root_cache::check_cache_access(cache_dir).ok()?;
        debug_assert!(current_canonical_root.is_absolute());
        // A newer build's cache is refused by name before its payload is read;
        // callers see a miss, and every write path refuses to replace it.
        if Self::check_disk_cache_format(cache_dir).is_err() {
            return None;
        }
        let cache_path = cache_dir.join("cache.bin");
        let cache_file = open_cache_file_read(&cache_path).ok()?;
        let file_len = cache_file.metadata().ok()?.len();
        if file_len < 16 {
            return None;
        }

        let mut reader = BufReader::new(cache_file.try_clone().ok()?);
        if read_u32(&mut reader).ok()? != CACHE_MAGIC {
            return None;
        }
        if read_u32(&mut reader).ok()? != INDEX_VERSION {
            return None;
        }
        let postings_len_total = read_u64(&mut reader).ok()?;
        let postings_section_start = reader.stream_position().ok()?;
        let postings_section_end = postings_section_start.checked_add(postings_len_total)?;
        if postings_len_total < 4 || postings_section_end > file_len {
            return None;
        }
        let postings_body_end = postings_section_end.checked_sub(4)?;

        let mut magic = [0u8; 8];
        reader.read_exact(&mut magic).ok()?;
        if &magic != INDEX_MAGIC {
            return None;
        }
        if read_u32(&mut reader).ok()? != INDEX_VERSION {
            return None;
        }

        let head_len = read_u32(&mut reader).ok()? as usize;
        let root_len = read_u32(&mut reader).ok()? as usize;
        let ignore_fingerprint_len = read_u32(&mut reader).ok()? as usize;
        let max_file_size = read_u64(&mut reader).ok()?;
        let file_count = read_u32(&mut reader).ok()? as usize;
        if file_count > MAX_ENTRIES {
            return None;
        }
        if borrowed_load_budget.is_some_and(|budget| !budget.preflight(file_count)) {
            return None;
        }

        if !reader_has_remaining(&mut reader, postings_body_end, head_len).ok()? {
            return None;
        }
        let mut head_bytes = vec![0u8; head_len];
        reader.read_exact(&mut head_bytes).ok()?;
        let git_head = String::from_utf8(head_bytes)
            .ok()
            .filter(|head| !head.is_empty());

        if !reader_has_remaining(&mut reader, postings_body_end, root_len).ok()? {
            return None;
        }
        let mut root_bytes = vec![0u8; root_len];
        reader.read_exact(&mut root_bytes).ok()?;
        let stored_project_root = PathBuf::from(String::from_utf8(root_bytes).ok()?);
        // Resolve aliases before deriving cached keys so scoped queries can see
        // both cached and new files. Keep the caller's Windows prefix convention:
        // configure supplies verbatim roots, while read-only callers may supply
        // plain canonical roots for disk-walk lookups shared with the semantic index.
        let project_root = fs::canonicalize(current_canonical_root)
            .unwrap_or_else(|_| current_canonical_root.to_path_buf());
        #[cfg(windows)]
        let project_root = match current_canonical_root.components().next() {
            Some(Component::Prefix(prefix)) if prefix.kind().is_verbatim() => project_root,
            _ => crate::windows_path::normalize_windows_path(&project_root),
        };
        // Snapshot paths are stored relative to the root that wrote them and
        // are re-rooted onto the loading root. That is only sound when both
        // roots sit at the same place in their checkouts: a snapshot written
        // by a session in a subfolder, loaded at a checkout's top level (or
        // the reverse), would present partial coverage as a complete index.
        // Older versions published subfolder snapshots under the repository
        // key, so such artifacts can still be on disk.
        if !stored_project_root.as_os_str().is_empty()
            && checkout_position(&stored_project_root) != checkout_position(&project_root)
        {
            if let Some(budget) = borrowed_load_budget {
                budget
                    .stop
                    .set(Some(BorrowedIndexLoadStop::CoverageMismatch));
            }
            return None;
        }

        if !reader_has_remaining(&mut reader, postings_body_end, ignore_fingerprint_len).ok()? {
            return None;
        }
        let mut ignore_fingerprint_bytes = vec![0u8; ignore_fingerprint_len];
        reader.read_exact(&mut ignore_fingerprint_bytes).ok()?;
        let stored_ignore_rules_fingerprint = String::from_utf8(ignore_fingerprint_bytes).ok()?;
        let current_ignore_rules_fingerprint = ignore_rules_fingerprint(&project_root);
        let ignore_rules_differ =
            stored_ignore_rules_fingerprint != current_ignore_rules_fingerprint;
        if ignore_rules_differ && ignore_rules_load_policy == IgnoreRulesLoadPolicy::Strict {
            return None;
        }

        let mut files = Vec::with_capacity(file_count);
        let mut path_to_id = HashMap::new();
        let mut unindexed_files = HashSet::new();
        let mut canonical_parents = ParentCanonicalizationMemo::default();

        for file_id in 0..file_count {
            if borrowed_load_budget.is_some_and(|budget| !budget.checkpoint_at(file_id)) {
                return None;
            }
            if !reader_has_remaining(&mut reader, postings_body_end, MIN_FILE_ENTRY_BYTES).ok()? {
                return None;
            }
            let mut unindexed = [0u8; 1];
            reader.read_exact(&mut unindexed).ok()?;
            let path_len = read_u32(&mut reader).ok()? as usize;
            let size = read_u64(&mut reader).ok()?;
            let secs = read_u64(&mut reader).ok()?;
            let nanos = read_u32(&mut reader).ok()?;
            let mut hash_bytes = [0u8; 32];
            reader.read_exact(&mut hash_bytes).ok()?;
            let content_hash = blake3::Hash::from_bytes(hash_bytes);
            if nanos >= 1_000_000_000 {
                return None;
            }
            if !reader_has_remaining(&mut reader, postings_body_end, path_len).ok()? {
                return None;
            }
            let mut path_bytes = vec![0u8; path_len];
            reader.read_exact(&mut path_bytes).ok()?;
            let relative_path = PathBuf::from(String::from_utf8(path_bytes).ok()?);
            let full_path = cached_path_under_root_with_memo(
                &project_root,
                &relative_path,
                &mut canonical_parents,
            )?;
            let file_id_u32 = u32::try_from(file_id).ok()?;

            files.push(FileEntry {
                path: full_path.clone(),
                size,
                modified: UNIX_EPOCH + Duration::new(secs, nanos),
                content_hash,
            });
            path_to_id.insert(full_path, file_id_u32);
            if unindexed[0] == 1 {
                unindexed_files.insert(file_id_u32);
            }
        }

        if !reader_has_remaining(&mut reader, postings_body_end, 8).ok()? {
            return None;
        }
        let postings_blob_len = read_u64(&mut reader).ok()?;
        let postings_blob_start = reader.stream_position().ok()?;
        let postings_blob_end = postings_blob_start.checked_add(postings_blob_len)?;
        if postings_blob_end > postings_body_end || postings_blob_len % POSTING_BYTES as u64 != 0 {
            return None;
        }

        let lookup_section_start = postings_section_end;
        if lookup_section_start >= file_len {
            return None;
        }
        let lookup_section_len = file_len.checked_sub(lookup_section_start)?;
        if borrowed_load_budget
            .is_some_and(|budget| !budget.preflight_lookup_bytes(lookup_section_len, file_count))
        {
            return None;
        }
        let mut lookup_file = cache_file.try_clone().ok()?;
        lookup_file
            .seek(SeekFrom::Start(lookup_section_start))
            .ok()?;
        let mut lookup_bytes = Vec::new();
        lookup_file.read_to_end(&mut lookup_bytes).ok()?;
        if lookup_bytes.len() < 4 {
            return None;
        }
        verify_crc32_bytes_slice(&lookup_bytes).ok()?;
        let lookup_body_len = lookup_bytes.len().checked_sub(4)?;
        let mut lookup_reader = BufReader::new(Cursor::new(&lookup_bytes));
        let mut lookup_magic = [0u8; 8];
        lookup_reader.read_exact(&mut lookup_magic).ok()?;
        if &lookup_magic != LOOKUP_MAGIC {
            return None;
        }
        if read_u32(&mut lookup_reader).ok()? != INDEX_VERSION {
            return None;
        }
        let entry_count = read_u32(&mut lookup_reader).ok()? as usize;
        if entry_count > MAX_ENTRIES {
            return None;
        }
        let total_records = file_count.checked_add(entry_count)?;
        if borrowed_load_budget.is_some_and(|budget| !budget.preflight(total_records)) {
            return None;
        }
        let remaining_lookup = remaining_bytes(&mut lookup_reader, lookup_body_len)?;
        let minimum_lookup_bytes = entry_count.checked_mul(LOOKUP_ENTRY_BYTES)?;
        if minimum_lookup_bytes > remaining_lookup {
            return None;
        }

        let mut lookup = Vec::with_capacity(entry_count);
        let mut previous_trigram = None;
        for lookup_index in 0..entry_count {
            if borrowed_load_budget.is_some_and(|budget| {
                !budget.checkpoint_at(file_count.saturating_add(lookup_index))
            }) {
                return None;
            }
            let trigram = read_u32(&mut lookup_reader).ok()?;
            let offset = read_u64(&mut lookup_reader).ok()?;
            let count = read_u32(&mut lookup_reader).ok()?;
            if count as usize > MAX_ENTRIES {
                return None;
            }
            if previous_trigram.is_some_and(|previous| previous >= trigram) {
                return None;
            }
            previous_trigram = Some(trigram);
            let bytes_len = (count as u64).checked_mul(POSTING_BYTES as u64)?;
            let end = offset.checked_add(bytes_len)?;
            if end > postings_blob_len {
                return None;
            }
            lookup.push(LookupEntry {
                trigram,
                offset,
                count,
            });
        }

        let base = BasePostings {
            file: Arc::new(cache_file),
            postings_blob_start,
            postings_blob_len,
            lookup: Arc::new(lookup),
        };

        if borrowed_load_budget.is_some_and(|budget| !budget.checkpoint()) {
            return None;
        }
        let (file_trigram_count, migrated_counts) = match read_file_trigram_count_extension(
            &base,
            postings_blob_end,
            postings_body_end,
            file_count,
        ) {
            Ok(Some(counts)) => (counts, false),
            Ok(None) => (
                compute_file_trigram_counts_from_base(&base, file_count, borrowed_load_budget)
                    .ok()?,
                true,
            ),
            Err(_) => return None,
        };

        let mut index = SearchIndex {
            base: Some(Arc::new(base)),
            delta: Arc::new(DeltaState::default()),
            delta_file_trigrams: HashMap::new(),
            free_delta_file_ids: Vec::new(),
            files: Arc::new(files),
            path_to_id: Arc::new(path_to_id),
            ready: false,
            build_denied: false,
            project_root,
            git_head,
            max_file_size,
            ignore_rules_fingerprint: current_ignore_rules_fingerprint,
            file_trigram_count: Arc::new(file_trigram_count),
            unindexed_files: Arc::new(unindexed_files),
            base_file_count: u32::try_from(file_count).ok()?,
            delta_packed_bytes: 0,
            compaction_state: Arc::new(Mutex::new(CompactionState::default())),
        };

        if migrated_counts && allow_legacy_repair {
            if let Ok(_lock) = CacheLock::acquire(cache_dir, current_canonical_root) {
                let head = index.git_head.clone();
                index.write_to_disk(cache_dir, head.as_deref());
            }
        }

        Some((index, ignore_rules_differ))
    }

    pub fn stored_git_head(&self) -> Option<&str> {
        self.git_head.as_deref()
    }

    /// Size and content hash of every file the index currently serves, keyed
    /// by its path relative to the index root with `/` separators. For a
    /// borrowed snapshot reconciled with this checkout (see
    /// `reconcile_borrowed_snapshot_with_disk`) and kept current by watcher
    /// events, these describe this checkout's files without reading the disk.
    /// Files held unindexed (binary, or over the size limit) carry the zero
    /// hash: their content was never hashed, so only their size is known.
    pub(crate) fn live_file_identities(&self) -> HashMap<String, (u64, blake3::Hash)> {
        self.files
            .iter()
            // A removed file keeps its slot with an empty path.
            .filter(|entry| !entry.path.as_os_str().is_empty())
            .filter_map(|entry| {
                let relative = entry.path.strip_prefix(&self.project_root).ok()?;
                let relative = relative.to_string_lossy().replace('\\', "/");
                Some((relative, (entry.size, entry.content_hash)))
            })
            .collect()
    }

    /// Count source files whose current stat no longer matches this persisted
    /// snapshot. Borrowed readers use this bounded-to-metadata probe to disclose
    /// a stale standing snapshot without restoring the owner's strict hash census.
    pub(crate) fn borrowed_stat_mismatch_count(&self) -> usize {
        self.files
            .iter()
            .filter(|entry| {
                let Ok(metadata) = fs::metadata(&entry.path) else {
                    return true;
                };
                metadata.len() != entry.size
                    || metadata.modified().unwrap_or(UNIX_EPOCH) != entry.modified
            })
            .count()
    }

    pub(crate) fn configured_max_file_size(&self) -> u64 {
        self.max_file_size
    }

    pub(crate) fn set_ready(&mut self, ready: bool) {
        self.ready = ready;
    }

    pub(crate) fn verify_against_disk_with_strategy(
        &mut self,
        current_head: Option<String>,
        verify_strategy: cache_freshness::VerifyStrategy,
    ) -> bool {
        self.git_head = current_head;
        let changed = verify_file_mtimes(self, verify_strategy);
        self.ready = true;
        changed
    }

    pub(crate) fn verify_against_disk_with_warm_budget(
        &mut self,
        current_head: Option<String>,
        verify_strategy: cache_freshness::VerifyStrategy,
    ) -> Option<bool> {
        let changed = verify_file_mtimes_with_budget(
            self,
            verify_strategy,
            HashSet::new(),
            WARM_GIT_DIFF_PATH_LIMIT,
        )?;
        self.git_head = current_head;
        self.ready = true;
        Some(changed)
    }

    /// Bring a borrowed (read-only) snapshot in line with the files that are
    /// actually on disk under this index's root, entirely in RAM.
    ///
    /// A linked worktree reads the home checkout's shared `cache.bin`, which
    /// describes the home checkout, not this worktree. Branch commits, new
    /// files, uncommitted edits and untracked files that already existed when
    /// the snapshot was opened never produce a watcher event, so without this
    /// pass they stay invisible to search while the index reports ready.
    ///
    /// The pass walks the root and compares each snapshot record's size and
    /// mtime with disk. A record whose size and mtime both match is trusted.
    /// A same-size record with a different mtime (every file of a fresh
    /// checkout) is content-hashed against the snapshot's stored hash, in a
    /// bounded parallel pool, and only a real content difference re-indexes
    /// it. Differing, added and removed files go into the RAM delta; the
    /// shared file on disk is never written.
    ///
    /// `keep_going` is polled between phases and while applying changes.
    /// Returning false abandons the pass and yields `None`; the caller must
    /// then discard the index, because it is only partially reconciled.
    pub(crate) fn reconcile_borrowed_snapshot_with_disk(
        &mut self,
        keep_going: &dyn Fn() -> bool,
    ) -> Option<BorrowedReconcileSummary> {
        const APPLY_CHECKPOINT_INTERVAL: usize = 256;
        let mut summary = BorrowedReconcileSummary::default();

        let walk_started = Instant::now();
        let current_files = walk_project_files(&self.project_root, &PathFilters::default());
        let current_file_set: HashSet<&PathBuf> = current_files.iter().collect();
        summary.walked = current_files.len();
        summary.walk = walk_started.elapsed();
        if !keep_going() {
            return None;
        }

        let verify_started = Instant::now();
        let mut removed_paths = Vec::new();
        let mut to_verify = Vec::new();
        for (file_id, entry) in self.files.iter().enumerate() {
            if entry.path.as_os_str().is_empty() {
                continue; // tombstoned entry
            }
            if !current_file_set.contains(&entry.path) {
                removed_paths.push(entry.path.clone());
                continue;
            }
            to_verify.push((
                file_id,
                entry.path.clone(),
                FileFreshness {
                    mtime: entry.modified,
                    size: entry.size,
                    content_hash: entry.content_hash,
                },
            ));
        }
        let verdicts = cache_freshness::verify_files_bounded(
            to_verify,
            cache_freshness::VerifyStrategy::StatFirst,
        );
        summary.verify = verify_started.elapsed();
        if !keep_going() {
            return None;
        }

        let apply_started = Instant::now();
        let mut stale_paths = Vec::new();
        {
            let files = Arc::make_mut(&mut self.files);
            for (file_id, path, verdict) in verdicts {
                match verdict {
                    FreshnessVerdict::HotFresh => summary.unchanged += 1,
                    FreshnessVerdict::ContentFresh {
                        new_mtime,
                        new_size,
                    } => {
                        // Same bytes as the snapshot: keep serving the shared
                        // postings and remember the local stat so a later
                        // watcher event compares against this checkout.
                        if let Some(entry) = files.get_mut(file_id) {
                            entry.modified = new_mtime;
                            entry.size = new_size;
                        }
                        summary.unchanged += 1;
                        summary.hashed_unchanged += 1;
                    }
                    FreshnessVerdict::Stale | FreshnessVerdict::Deleted => stale_paths.push(path),
                }
            }
        }

        let mut canonical_parents = ParentCanonicalizationMemo::default();
        let mut applied = 0usize;
        let mut checkpoint = || {
            applied += 1;
            applied % APPLY_CHECKPOINT_INTERVAL != 0 || keep_going()
        };
        for path in &removed_paths {
            if !checkpoint() {
                return None;
            }
            self.remove_file_with_canonicalization_memo(path, &mut canonical_parents);
            summary.removed += 1;
        }
        for path in &stale_paths {
            if !checkpoint() {
                return None;
            }
            // `update_file` drops the file when it vanished after the walk.
            self.update_file_with_canonicalization_memo(path, &mut canonical_parents);
            summary.reindexed += 1;
        }
        for path in &current_files {
            if self.path_to_id.contains_key(path) {
                continue;
            }
            if !checkpoint() {
                return None;
            }
            self.update_file_with_canonicalization_memo(path, &mut canonical_parents);
            summary.added += 1;
        }
        summary.apply = apply_started.elapsed();
        Some(summary)
    }

    /// True when `path` is answered from the shared snapshot's postings rather
    /// than from the in-RAM delta.
    #[cfg(test)]
    pub(crate) fn serves_path_from_shared_base_for_test(&self, path: &Path) -> bool {
        self.path_to_id.get(path).is_some_and(|file_id| {
            *file_id < self.base_file_count && !self.delta.superseded.contains(file_id)
        })
    }

    #[cfg(debug_assertions)]
    #[doc(hidden)]
    pub fn verify_against_disk_for_debug(&mut self, current_head: Option<String>) {
        let _ = self.verify_against_disk_with_strategy(
            current_head,
            cache_freshness::VerifyStrategy::Strict,
        );
    }

    #[cfg(test)]
    pub(crate) fn rebuild_or_refresh(
        root: &Path,
        max_file_size: u64,
        current_head: Option<String>,
        baseline: Option<SearchIndex>,
        cache_dir: Option<&Path>,
    ) -> Self {
        Self::rebuild_or_refresh_with_strategy(
            root,
            max_file_size,
            current_head,
            baseline,
            cache_dir,
            cache_freshness::VerifyStrategy::Strict,
        )
    }

    pub(crate) fn rebuild_or_refresh_with_strategy(
        root: &Path,
        max_file_size: u64,
        current_head: Option<String>,
        baseline: Option<SearchIndex>,
        cache_dir: Option<&Path>,
        verify_strategy: cache_freshness::VerifyStrategy,
    ) -> Self {
        if let Some(index) = baseline.and_then(|baseline| {
            Self::refresh_cached_with_diff_limit(
                baseline,
                root,
                max_file_size,
                current_head,
                verify_strategy,
                None,
            )
        }) {
            return index;
        }

        match cache_dir {
            Some(cache_dir) => {
                SearchIndex::build_with_limit_to_cache_dir(root, max_file_size, cache_dir)
            }
            None => SearchIndex::build_with_limit(root, max_file_size),
        }
    }

    /// Verify or update an existing cache without falling through to a full
    /// project build. The caller can acquire a cold-build permit on a miss.
    pub(crate) fn refresh_cached_with_strategy(
        baseline: Self,
        root: &Path,
        max_file_size: u64,
        current_head: Option<String>,
        verify_strategy: cache_freshness::VerifyStrategy,
    ) -> Option<Self> {
        Self::refresh_cached_with_diff_limit(
            baseline,
            root,
            max_file_size,
            current_head,
            verify_strategy,
            Some(WARM_GIT_DIFF_PATH_LIMIT),
        )
    }

    fn refresh_cached_with_diff_limit(
        mut baseline: Self,
        root: &Path,
        max_file_size: u64,
        current_head: Option<String>,
        verify_strategy: cache_freshness::VerifyStrategy,
        max_changed_paths: Option<usize>,
    ) -> Option<Self> {
        if baseline.max_file_size != max_file_size {
            return None;
        }
        // Cached file keys use the loading root's lexical spelling. Replacing
        // only the root with a Windows verbatim path makes the walk report every
        // cached file as removed and re-added, incorrectly exhausting the budget.
        // Keep that spelling; a different checkout must be re-rooted at load time.
        if canonicalize_or_normalize(&baseline.project_root) != canonicalize_or_normalize(root) {
            return None;
        }
        let current_ignore_rules_fingerprint = ignore_rules_fingerprint(&baseline.project_root);
        if baseline.ignore_rules_fingerprint != current_ignore_rules_fingerprint {
            return None;
        }
        baseline.ignore_rules_fingerprint = current_ignore_rules_fingerprint;

        if let Some(limit) = max_changed_paths {
            let diff_paths = if baseline.git_head == current_head || current_head.is_none() {
                HashSet::new()
            } else {
                git_diff_paths(
                    &baseline.project_root,
                    baseline.git_head.as_deref()?,
                    current_head.as_deref()?,
                    Some(limit),
                )?
            };
            verify_file_mtimes_with_budget(&mut baseline, verify_strategy, diff_paths, limit)?;
            baseline.git_head = current_head;
            baseline.ready = true;
            return Some(baseline);
        }

        if baseline.git_head == current_head || current_head.is_none() {
            // HEAD matches, but files may have changed on disk since the index was
            // last written (e.g., uncommitted edits, stash pop, manual file changes
            // while OpenCode was closed). Verify mtimes and re-index stale files.
            // Non-git projects also use this per-file (path, mtime, size)
            // fingerprint so unchanged trees reuse the disk cache instead of
            // rebuilding every configure.
            baseline.git_head = current_head;
            let _ = verify_file_mtimes(&mut baseline, verify_strategy);
            baseline.ready = true;
            return Some(baseline);
        }

        if let (Some(previous), Some(current)) = (baseline.git_head.clone(), current_head.clone()) {
            let project_root = baseline.project_root.clone();
            if apply_git_diff_updates(&mut baseline, &project_root, &previous, &current, None) {
                baseline.git_head = Some(current);
                let _ = verify_file_mtimes(&mut baseline, verify_strategy);
                baseline.ready = true;
                return Some(baseline);
            }
        }
        None
    }

    fn allocate_file_id_with_metadata(
        &mut self,
        path: &Path,
        metadata: SearchFileMetadata,
    ) -> Option<u32> {
        let file_id = match self.free_delta_file_ids.pop() {
            Some(file_id) => file_id,
            None => u32::try_from(self.files.len()).ok()?,
        };
        let file = FileEntry {
            path: path.to_path_buf(),
            size: metadata.size,
            modified: metadata.modified,
            content_hash: cache_freshness::zero_hash(),
        };
        let files = Arc::make_mut(&mut self.files);
        if let Some(slot) = files.get_mut(file_id as usize) {
            *slot = file;
        } else {
            files.push(file);
        }
        Arc::make_mut(&mut self.path_to_id).insert(path.to_path_buf(), file_id);
        ensure_count_slot(Arc::make_mut(&mut self.file_trigram_count), file_id);
        Some(file_id)
    }

    fn track_unindexed_file_with_metadata(
        &mut self,
        path: &Path,
        metadata: SearchFileMetadata,
    ) -> bool {
        let Some(file_id) = self.allocate_file_id_with_metadata(path, metadata) else {
            return false;
        };
        Arc::make_mut(&mut self.unindexed_files).insert(file_id);
        if let Some(count) = Arc::make_mut(&mut self.file_trigram_count).get_mut(file_id as usize) {
            *count = 0;
        }
        true
    }

    fn active_file_ids(&self) -> Vec<u32> {
        self.snapshot().active_file_ids()
    }

    #[cfg(test)]
    fn postings_for_trigram(&self, trigram: u32, filter: Option<PostingFilter>) -> Vec<u32> {
        self.snapshot().postings_for_trigram(trigram, filter)
    }

    /// The `(next_mask, loc_mask)` of every not-yet-compacted posting of
    /// `path`, by trigram, so tests elsewhere can compare against what this
    /// index actually stored.
    #[cfg(test)]
    pub(crate) fn delta_posting_masks(&self, path: &Path) -> BTreeMap<u32, (u8, u8)> {
        let Some(&file_id) = self.path_to_id.get(path) else {
            return BTreeMap::new();
        };
        self.delta
            .postings
            .iter()
            .filter_map(|(&trigram, postings)| {
                postings
                    .iter()
                    .find(|posting| posting.file_id == file_id)
                    .map(|posting| (trigram, (posting.next_mask, posting.loc_mask)))
            })
            .collect()
    }

    fn update_compaction_flags(&mut self, changed_path: Option<&Path>) {
        let delta_files = self.delta_file_trigrams.len();
        let hard = delta_files >= DELTA_COMPACT_HARD_FILES
            || self.delta_packed_bytes >= DELTA_COMPACT_HARD_BYTES;
        let soft = delta_files >= DELTA_COMPACT_SOFT_FILES
            || self.delta_packed_bytes >= DELTA_COMPACT_SOFT_BYTES;
        if let Ok(mut state) = self.compaction_state.lock() {
            if state.running {
                if let Some(path) = changed_path {
                    state.buffered_paths.push(path.to_path_buf());
                }
                if soft || hard {
                    state.requested_again = true;
                }
            } else if hard || (soft && !state.requested_again) {
                state.requested_again = true;
            }
        }
    }

    fn compaction_record_sources(
        &self,
        id_map: Arc<HashMap<u32, u32>>,
    ) -> Vec<Box<dyn PostingRecordSource>> {
        let mut sources: Vec<Box<dyn PostingRecordSource>> = Vec::new();
        if let Some(base) = self.base.clone() {
            sources.push(Box::new(BaseRecordSource::new(
                base,
                Arc::clone(&id_map),
                Arc::clone(&self.delta),
            )));
        }

        let mut delta_records = Vec::new();
        for (&trigram, postings) in &self.delta.postings {
            for posting in postings {
                let Some(mapped_file_id) = id_map.get(&posting.file_id).copied() else {
                    continue;
                };
                delta_records.push(SpillRecord {
                    trigram,
                    file_id: mapped_file_id,
                    next_mask: posting.next_mask,
                    loc_mask: posting.loc_mask,
                });
            }
        }
        if !delta_records.is_empty() {
            delta_records.sort_unstable_by_key(|record| (record.trigram, record.file_id));
            sources.push(Box::new(VecRecordSource::new(delta_records)));
        }
        sources
    }
}

impl BasePostings {
    fn lookup_entry(&self, trigram: u32) -> Option<LookupEntry> {
        self.lookup
            .binary_search_by_key(&trigram, |entry| entry.trigram)
            .ok()
            .and_then(|index| self.lookup.get(index).copied())
    }

    fn read_posting_bytes(&self, entry: LookupEntry) -> std::io::Result<Vec<u8>> {
        let bytes_len = (entry.count as usize)
            .checked_mul(POSTING_BYTES)
            .ok_or_else(|| std::io::Error::other("posting list too large"))?;
        let offset = self
            .postings_blob_start
            .checked_add(entry.offset)
            .ok_or_else(|| std::io::Error::other("posting offset overflow"))?;
        let end = entry
            .offset
            .checked_add(bytes_len as u64)
            .ok_or_else(|| std::io::Error::other("posting offset overflow"))?;
        if end > self.postings_blob_len {
            return Err(std::io::Error::other("posting list exceeds blob"));
        }
        let mut bytes = vec![0u8; bytes_len];
        pread_exact(&self.file, offset, &mut bytes)?;
        Ok(bytes)
    }

    fn for_each_posting(
        &self,
        entry: LookupEntry,
        mut visit: impl FnMut(u32, u8, u8) -> bool,
    ) -> std::io::Result<()> {
        let bytes = self.read_posting_bytes(entry)?;
        for chunk in bytes.chunks_exact(POSTING_BYTES) {
            if !visit(
                u32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]),
                chunk[4],
                chunk[5],
            ) {
                break;
            }
        }
        Ok(())
    }

    fn read_postings(&self, entry: LookupEntry) -> std::io::Result<Vec<Posting>> {
        let bytes = self.read_posting_bytes(entry)?;
        let mut postings = Vec::with_capacity(entry.count as usize);
        for chunk in bytes.chunks_exact(POSTING_BYTES) {
            postings.push(Posting {
                file_id: u32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]),
                next_mask: chunk[4],
                loc_mask: chunk[5],
            });
        }
        Ok(postings)
    }
}

impl SearchIndexSnapshot {
    pub fn grep(
        &self,
        pattern: &str,
        case_sensitive: bool,
        include: &[String],
        exclude: &[String],
        search_root: &Path,
        max_results: usize,
    ) -> GrepResult {
        match pattern_compile::compile(
            pattern,
            CompileOpts {
                case_insensitive: !case_sensitive,
                ..CompileOpts::default()
            },
        ) {
            CompileResult::Ok(compiled) => {
                self.search_grep(&compiled, include, exclude, search_root, max_results)
            }
            CompileResult::InvalidPattern { .. } | CompileResult::UnsupportedSyntax { .. } => {
                self.empty_grep_result()
            }
        }
    }

    pub fn search_grep(
        &self,
        pattern: &CompiledPattern,
        include: &[String],
        exclude: &[String],
        search_root: &Path,
        max_results: usize,
    ) -> GrepResult {
        self.search_grep_profiled(pattern, include, exclude, search_root, max_results, None)
            .0
    }

    pub(crate) fn search_grep_bounded(
        &self,
        pattern: &CompiledPattern,
        include: &[String],
        exclude: &[String],
        search_root: &Path,
        max_results: usize,
        path_exclusion: Option<GrepPathExclusion>,
        max_files: usize,
        budget: Duration,
    ) -> GrepResult {
        let filters = build_path_filters(include, exclude).unwrap_or_default();
        let query = decompose_grep_pattern(pattern);
        self.search_grep_profiled_with_filters_and_query_and_limits(
            pattern,
            &query,
            &filters,
            search_root,
            max_results,
            path_exclusion,
            Some(max_files),
            budget,
            GrepScanOrder::Arrival,
            PATH_ORDER_BATCH_ROW_BUDGET,
        )
        .0
    }

    pub(crate) fn search_grep_profiled(
        &self,
        pattern: &CompiledPattern,
        include: &[String],
        exclude: &[String],
        search_root: &Path,
        max_results: usize,
        path_exclusion: Option<GrepPathExclusion>,
    ) -> (GrepResult, GrepQueryPhaseTimings) {
        let filters = build_path_filters(include, exclude).unwrap_or_default();
        self.search_grep_profiled_with_filters(
            pattern,
            &filters,
            search_root,
            max_results,
            path_exclusion,
        )
    }

    pub(crate) fn search_grep_profiled_with_filters(
        &self,
        pattern: &CompiledPattern,
        filters: &PathFilters,
        search_root: &Path,
        max_results: usize,
        path_exclusion: Option<GrepPathExclusion>,
    ) -> (GrepResult, GrepQueryPhaseTimings) {
        let query_started = Instant::now();
        let query = decompose_grep_pattern(pattern);
        let query_decomposition = query_started.elapsed();
        let (result, mut timings) = self.search_grep_profiled_with_filters_and_query(
            pattern,
            &query,
            filters,
            search_root,
            max_results,
            path_exclusion,
        );
        timings.trigram_lookup += query_decomposition;
        (result, timings)
    }

    /// Search with a query decomposed once by the caller and reused across roots.
    pub(crate) fn search_grep_profiled_with_filters_and_query(
        &self,
        pattern: &CompiledPattern,
        query: &RegexQuery,
        filters: &PathFilters,
        search_root: &Path,
        max_results: usize,
        path_exclusion: Option<GrepPathExclusion>,
    ) -> (GrepResult, GrepQueryPhaseTimings) {
        self.search_grep_profiled_with_filters_and_query_in_order(
            pattern,
            query,
            filters,
            search_root,
            max_results,
            path_exclusion,
            GrepScanOrder::Arrival,
        )
    }

    /// [`Self::search_grep_profiled_with_filters_and_query`] with a choice
    /// of which lines are kept past the result cap.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn search_grep_profiled_with_filters_and_query_in_order(
        &self,
        pattern: &CompiledPattern,
        query: &RegexQuery,
        filters: &PathFilters,
        search_root: &Path,
        max_results: usize,
        path_exclusion: Option<GrepPathExclusion>,
        order: GrepScanOrder,
    ) -> (GrepResult, GrepQueryPhaseTimings) {
        self.search_grep_profiled_with_filters_and_query_in_order_with_row_budget(
            pattern,
            query,
            filters,
            search_root,
            max_results,
            path_exclusion,
            order,
            PATH_ORDER_BATCH_ROW_BUDGET,
        )
    }

    /// [`Self::search_grep_profiled_with_filters_and_query_in_order`] with
    /// the rows a path-ordered batch may hold set by the caller, for callers
    /// that run several of these scans at once and share one budget.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn search_grep_profiled_with_filters_and_query_in_order_with_row_budget(
        &self,
        pattern: &CompiledPattern,
        query: &RegexQuery,
        filters: &PathFilters,
        search_root: &Path,
        max_results: usize,
        path_exclusion: Option<GrepPathExclusion>,
        order: GrepScanOrder,
        row_budget: usize,
    ) -> (GrepResult, GrepQueryPhaseTimings) {
        self.search_grep_profiled_with_filters_and_query_and_limits(
            pattern,
            query,
            filters,
            search_root,
            max_results,
            path_exclusion,
            None,
            INDEXED_GREP_SCAN_BUDGET,
            order,
            row_budget,
        )
    }

    /// Indexed grep over `search_root`. `max_files` bounds how many candidate
    /// files are read; `scan_budget` bounds the wall-clock time of the scan,
    /// checked before each file and at each match occurrence inside it. A scan
    /// that runs out of budget returns what it found with `engine_capped` and
    /// `scan_deadline_reached` set, never an empty success.
    fn search_grep_profiled_with_filters_and_query_and_limits(
        &self,
        pattern: &CompiledPattern,
        query: &RegexQuery,
        filters: &PathFilters,
        search_root: &Path,
        max_results: usize,
        path_exclusion: Option<GrepPathExclusion>,
        max_files: Option<usize>,
        scan_budget: Duration,
        order: GrepScanOrder,
        row_budget: usize,
    ) -> (GrepResult, GrepQueryPhaseTimings) {
        self.search_grep_profiled_with_filters_and_query_and_limits_with_deadline(
            pattern,
            query,
            filters,
            search_root,
            max_results,
            path_exclusion,
            max_files,
            scan_budget,
            || GrepScanDeadline::after_with_clock(scan_budget, Instant::now),
            order,
            row_budget,
        )
    }

    fn search_grep_profiled_with_filters_and_query_and_limits_with_deadline(
        &self,
        pattern: &CompiledPattern,
        query: &RegexQuery,
        filters: &PathFilters,
        search_root: &Path,
        max_results: usize,
        path_exclusion: Option<GrepPathExclusion>,
        max_files: Option<usize>,
        scan_budget: Duration,
        make_deadline: impl FnOnce() -> GrepScanDeadline,
        order: GrepScanOrder,
        row_budget: usize,
    ) -> (GrepResult, GrepQueryPhaseTimings) {
        let _io_scope = crate::bounded_io::enter_if_absent(scan_budget);
        let matcher = match pattern {
            CompiledPattern::Literal(literal) => SearchMatcher::Literal(literal.clone()),
            CompiledPattern::Regex { compiled, .. } => SearchMatcher::Regex(compiled.clone()),
        };

        let search_root = canonicalize_for_search_membership(search_root);

        let job_cancellation = crate::executor::current_job_cancellation();
        let trigram_started = Instant::now();
        let fully_degraded = query.and_trigrams.is_empty() && query.or_groups.is_empty();
        let candidate_ids = self.candidates(query);
        let trigram_lookup = trigram_started.elapsed();

        let candidate_filter_started = Instant::now();
        let mut candidate_files: Vec<&FileEntry> = candidate_ids
            .into_iter()
            .filter_map(|file_id| self.files.get(file_id as usize))
            .filter(|file| !file.path.as_os_str().is_empty())
            .filter(|file| is_within_search_root(&search_root, &file.path))
            .filter(|file| {
                path_exclusion.is_none_or(|exclude| !exclude(&file.path, &self.project_root))
            })
            .filter(|file| filters.matches(&self.project_root, &file.path))
            .collect();
        // Tests hand the candidates over reversed to show a path-ordered
        // scan does not depend on the order the index lists them in.
        #[cfg(test)]
        if crate::grep_executor::scan_order_for_test::get(&search_root) == Some(true) {
            candidate_files.reverse();
        }
        if order == GrepScanOrder::Path {
            candidate_files.sort_by(|left, right| left.path.cmp(&right.path));
        }
        let candidate_count = candidate_files.len();
        let candidate_filter = candidate_filter_started.elapsed();

        let total_matches = AtomicUsize::new(0);
        let files_searched = AtomicUsize::new(0);
        let files_with_matches = AtomicUsize::new(0);
        let bytes_verified = AtomicUsize::new(0);
        let missing_on_disk = AtomicUsize::new(0);
        let truncated = AtomicBool::new(false);
        let engine_capped = AtomicBool::new(false);
        let stop_after = max_results.saturating_mul(2);
        let stop_scan = Arc::new(AtomicBool::new(false));
        let deadline = make_deadline();
        let io_budget = crate::bounded_io::current();
        let floor_disclosure = crate::context::confinement::capture();
        let verification_claims = AtomicUsize::new(0);
        let claim_verification = || {
            let Some(max_files) = max_files else {
                return true;
            };
            verification_claims.fetch_add(1, Ordering::Relaxed) < max_files
        };

        let pread_started = Instant::now();
        let mut matches = if order == GrepScanOrder::Path {
            // Path-ordered batches: each file keeps at most its share of the
            // batch's row budget, and never more than the lines still missing
            // from the cap when its batch starts, which is all it can
            // contribute to the first lines by path. Rows are added in batch
            // order, so `kept` stays sorted by path, then line.
            let mut kept: Vec<SharedGrepMatch> = Vec::new();
            let batches = candidate_files.chunks(PATH_ORDER_BATCH_FILES);
            let batch_count = batches.len();
            for (batch_index, batch) in batches.enumerate() {
                if deadline.reached()
                    || job_cancellation
                        .as_ref()
                        .is_some_and(|token| token.cancel_requested_before_commit())
                {
                    engine_capped.store(true, Ordering::Relaxed);
                    break;
                }
                let remaining = max_results.saturating_sub(kept.len());
                let allowance = path_order_file_allowance(remaining, batch.len(), row_budget);
                let per_file: Vec<(Vec<SharedGrepMatch>, bool)> = batch
                    .par_iter()
                    .map_init(
                        || crate::bounded_io::enter(io_budget.clone()),
                        |_scope, file| {
                            if !claim_verification() {
                                truncated.store(true, Ordering::Relaxed);
                                engine_capped.store(true, Ordering::Relaxed);
                                return (Vec::new(), false);
                            }
                            let file_matches = AtomicUsize::new(0);
                            let file_truncated = AtomicBool::new(false);
                            let rows = search_candidate_file(
                                file,
                                &floor_disclosure,
                                &matcher,
                                allowance,
                                allowance,
                                &file_matches,
                                &files_searched,
                                &files_with_matches,
                                &bytes_verified,
                                &file_truncated,
                                &engine_capped,
                                &missing_on_disk,
                                None,
                                job_cancellation.as_ref(),
                                &deadline,
                            );
                            total_matches
                                .fetch_add(file_matches.load(Ordering::Relaxed), Ordering::Relaxed);
                            (rows, file_truncated.load(Ordering::Relaxed))
                        },
                    )
                    .collect();
                let search_again = |file: usize, need: usize| {
                    // The file was already counted as searched (and as
                    // matching) by the batch; only its match count changes.
                    let file_matches = AtomicUsize::new(0);
                    let file_truncated = AtomicBool::new(false);
                    let rows = search_candidate_file(
                        batch[file],
                        &floor_disclosure,
                        &matcher,
                        need,
                        need,
                        &file_matches,
                        &AtomicUsize::new(0),
                        &AtomicUsize::new(0),
                        &AtomicUsize::new(0),
                        &file_truncated,
                        &engine_capped,
                        &AtomicUsize::new(0),
                        None,
                        job_cancellation.as_ref(),
                        &deadline,
                    );
                    // The batch counted `allowance + 1` matches for it.
                    total_matches.fetch_add(
                        file_matches
                            .load(Ordering::Relaxed)
                            .saturating_sub(allowance + 1),
                        Ordering::Relaxed,
                    );
                    (rows, file_truncated.load(Ordering::Relaxed))
                };
                let lines_past = keep_path_ordered_batch(
                    &mut kept,
                    max_results,
                    allowance,
                    per_file,
                    search_again,
                    |rows| note_path_order_held_rows(&search_root, rows),
                );
                if lines_past {
                    truncated.store(true, Ordering::Relaxed);
                }
                if kept.len() >= max_results {
                    if batch_index + 1 < batch_count {
                        // Later files were not read.
                        truncated.store(true, Ordering::Relaxed);
                        engine_capped.store(true, Ordering::Relaxed);
                    }
                    break;
                }
            }
            kept
        } else if candidate_files.len() > 10 {
            candidate_files
                .par_iter()
                .map_init(
                    || crate::bounded_io::enter(io_budget.clone()),
                    |_scope, file| {
                        if grep_scan_should_stop(
                            Some(&stop_scan),
                            &truncated,
                            &total_matches,
                            stop_after,
                            job_cancellation.as_ref(),
                            &deadline,
                        ) {
                            engine_capped.store(true, Ordering::Relaxed);
                            return Vec::new();
                        }
                        if !claim_verification() {
                            truncated.store(true, Ordering::Relaxed);
                            engine_capped.store(true, Ordering::Relaxed);
                            stop_scan.store(true, Ordering::Relaxed);
                            return Vec::new();
                        }
                        search_candidate_file(
                            file,
                            &floor_disclosure,
                            &matcher,
                            max_results,
                            stop_after,
                            &total_matches,
                            &files_searched,
                            &files_with_matches,
                            &bytes_verified,
                            &truncated,
                            &engine_capped,
                            &missing_on_disk,
                            Some(&stop_scan),
                            job_cancellation.as_ref(),
                            &deadline,
                        )
                    },
                )
                .reduce(Vec::new, |mut left, mut right| {
                    // When concatenating partial match lists from parallel file
                    // searches, simply append the chunks. The stop checks in
                    // each worker decide whether the result cap was reached.
                    left.append(&mut right);
                    left
                })
        } else {
            let mut matches = Vec::new();
            for file in candidate_files {
                if !claim_verification() {
                    truncated.store(true, Ordering::Relaxed);
                    engine_capped.store(true, Ordering::Relaxed);
                    break;
                }
                matches.extend(search_candidate_file(
                    file,
                    &floor_disclosure,
                    &matcher,
                    max_results,
                    stop_after,
                    &total_matches,
                    &files_searched,
                    &files_with_matches,
                    &bytes_verified,
                    &truncated,
                    &engine_capped,
                    &missing_on_disk,
                    None,
                    job_cancellation.as_ref(),
                    &deadline,
                ));

                if should_stop_search(&truncated, &total_matches, stop_after) || deadline.reached()
                {
                    engine_capped.store(true, Ordering::Relaxed);
                    break;
                }
            }
            matches
        };
        let scan_deadline_reached = deadline.reached();
        if scan_deadline_reached && max_files.is_some() {
            // Bounded callers read `truncated` as "more may exist"; running out
            // of time leaves candidate files unread just as the file bound does.
            truncated.store(true, Ordering::Relaxed);
        }
        let pread_verify = pread_started.elapsed();

        let post_filter_started = Instant::now();
        sort_shared_grep_matches_by_cached_mtime_desc(&mut matches, &self.project_root, |path| {
            self.path_to_id
                .get(path)
                .and_then(|file_id| self.files.get(*file_id as usize))
                .map(|file| file.modified)
        });

        let matches = matches
            .into_iter()
            .map(|matched| GrepMatch {
                file: matched.file.as_ref().clone(),
                line: matched.line,
                column: matched.column,
                line_text: matched.line_text,
                match_text: matched.match_text,
            })
            .collect();

        let result = GrepResult {
            total_matches: total_matches.load(Ordering::Relaxed),
            matches,
            files_searched: files_searched.load(Ordering::Relaxed),
            files_with_matches: files_with_matches.load(Ordering::Relaxed),
            index_status: if self.ready {
                IndexStatus::Ready
            } else {
                IndexStatus::Building
            },
            truncated: truncated.load(Ordering::Relaxed),
            fully_degraded,
            engine_capped: engine_capped.load(Ordering::Relaxed),
            walk_truncated: false,
            skipped_foreign_mounts: 0,
            missing_on_disk: missing_on_disk.load(Ordering::Relaxed),
            scan_deadline_reached,
            files_read_directly: 0,
            walk_bound: None,
        };
        let post_filter = candidate_filter + post_filter_started.elapsed();
        let phases = GrepQueryPhaseTimings {
            trigram_lookup,
            pread_verify,
            post_filter,
            candidate_count,
            bytes_verified: bytes_verified.load(Ordering::Relaxed),
        };
        (result, phases)
    }

    /// Find every matching line in the candidate files the trigram index admits,
    /// grouped by file, for a caller that ranks files before cutting a page.
    ///
    /// Candidates come from the whole index: the trigram prefilter, with one
    /// posting union per alternation branch, admits files; the pattern itself
    /// decides every match, so the result has no false matches. Unlike
    /// [`Self::search_grep`], nothing stops at a match count. The work is
    /// bounded instead by `limits`: candidates are sorted by
    /// `examine_priority` (lower first) and then by path, and at most
    /// `limits.max_files` of them are read, none started after the time
    /// budget. When a bound leaves files unexamined, `examination_capped`
    /// reports it, so the caller can say what its ranking covered.
    pub(crate) fn collect_grep_matches_by_file(
        &self,
        pattern: &CompiledPattern,
        filters: &PathFilters,
        search_root: &Path,
        path_exclusion: Option<GrepPathExclusion>,
        limits: GrepExaminationLimits,
        examine_priority: &(dyn Fn(&Path) -> u8 + Sync),
        keep_past_limit: &(dyn Fn(&GrepMatch) -> bool + Sync),
    ) -> GrepFileCollection {
        let matcher = match pattern {
            CompiledPattern::Literal(literal) => SearchMatcher::Literal(literal.clone()),
            CompiledPattern::Regex { compiled, .. } => SearchMatcher::Regex(compiled.clone()),
        };
        let query = decompose_grep_pattern(pattern);
        let search_root = canonicalize_for_search_membership(search_root);
        let fully_degraded = query.and_trigrams.is_empty() && query.or_groups.is_empty();

        let mut candidates: Vec<(u8, &FileEntry)> = self
            .candidates(&query)
            .into_iter()
            .filter_map(|file_id| self.files.get(file_id as usize))
            .filter(|file| !file.path.as_os_str().is_empty())
            .filter(|file| is_within_search_root(&search_root, &file.path))
            .filter(|file| {
                path_exclusion.is_none_or(|exclude| !exclude(&file.path, &self.project_root))
            })
            .filter(|file| filters.matches(&self.project_root, &file.path))
            .map(|file| (examine_priority(&file.path), file))
            .collect();
        candidates.sort_by(|(left_priority, left), (right_priority, right)| {
            left_priority
                .cmp(right_priority)
                .then_with(|| left.path.cmp(&right.path))
        });
        let candidate_files = candidates.len();
        let examined_slice = &candidates[..candidate_files.min(limits.max_files)];

        let job_cancellation = crate::executor::current_job_cancellation();
        let started = Instant::now();
        let stopped_early = AtomicBool::new(false);
        let io_budget = crate::bounded_io::current();
        let files_examined = AtomicUsize::new(0);
        let missing_on_disk = AtomicUsize::new(0);
        let files: Vec<GrepFileMatches> = examined_slice
            .par_iter()
            .map_init(
                || crate::bounded_io::enter(io_budget.clone()),
                |_scope, (_, file)| {
                    if started.elapsed() >= limits.budget
                        || job_cancellation
                            .as_ref()
                            .is_some_and(|token| token.cancel_requested_before_commit())
                    {
                        stopped_early.store(true, Ordering::Relaxed);
                        return None;
                    }
                    let content = match read_indexed_file_bytes(
                        &file.path,
                        started.checked_add(limits.budget),
                    ) {
                        Ok(content) => content,
                        Err(error) => {
                            if error.kind() == std::io::ErrorKind::TimedOut {
                                stopped_early.store(true, Ordering::Relaxed);
                            }
                            if io_error_means_missing_on_disk(&error) {
                                missing_on_disk.fetch_add(1, Ordering::Relaxed);
                            }
                            files_examined.fetch_add(1, Ordering::Relaxed);
                            return None;
                        }
                    };
                    files_examined.fetch_add(1, Ordering::Relaxed);
                    if is_binary_bytes(&content) {
                        return None;
                    }
                    let (matches, matched_lines) = matching_lines_in_content(
                        &file.path,
                        &content,
                        &matcher,
                        limits.max_lines_per_file,
                        keep_past_limit,
                    );
                    (matched_lines > 0).then(|| GrepFileMatches {
                        path: file.path.clone(),
                        modified: file.modified,
                        matches,
                        matched_lines,
                    })
                },
            )
            .filter_map(|file| file)
            .collect();

        GrepFileCollection {
            files,
            candidate_files,
            files_examined: files_examined.load(Ordering::Relaxed),
            examination_capped: candidate_files > examined_slice.len()
                || stopped_early.load(Ordering::Relaxed),
            fully_degraded,
            index_status: if self.ready {
                IndexStatus::Ready
            } else {
                IndexStatus::Building
            },
            missing_on_disk: missing_on_disk.load(Ordering::Relaxed),
        }
    }

    fn empty_grep_result(&self) -> GrepResult {
        GrepResult {
            matches: Vec::new(),
            total_matches: 0,
            files_searched: 0,
            files_with_matches: 0,
            index_status: if self.ready {
                IndexStatus::Ready
            } else {
                IndexStatus::Building
            },
            truncated: false,
            fully_degraded: false,
            engine_capped: false,
            walk_truncated: false,
            skipped_foreign_mounts: 0,
            missing_on_disk: 0,
            scan_deadline_reached: false,
            files_read_directly: 0,
            walk_bound: None,
        }
    }

    pub fn glob(&self, pattern: &str, search_root: &Path) -> Vec<PathBuf> {
        self.glob_profiled(pattern, search_root, true).0
    }

    pub(crate) fn glob_profiled(
        &self,
        pattern: &str,
        search_root: &Path,
        sort_by_mtime: bool,
    ) -> (Vec<PathBuf>, bool, usize) {
        let filters = match build_path_filters(&[pattern.to_string()], &[]) {
            Ok(filters) => filters,
            Err(_) => return (Vec::new(), false, 0),
        };
        // Indexed entries carry the project root's spelling as configured,
        // which on Windows can differ from the canonical form in ways
        // normalize_path does not reconcile (8.3 short names such as
        // RUNNER~1 against runneradmin). Containment is therefore checked
        // against both the root as passed and its canonical form, and the
        // pattern is matched relative to whichever root contained the entry.
        let raw_root = search_root.to_path_buf();
        let canonical_root = canonicalize_for_search_membership(search_root);
        let file_scope = search_root.is_file();
        let containing_root = |path: &Path| -> Option<&Path> {
            if is_within_search_root(&raw_root, path) {
                Some(raw_root.as_path())
            } else if is_within_search_root(&canonical_root, path) {
                Some(canonical_root.as_path())
            } else {
                None
            }
        };
        let entries_visited = self.files.len();
        let mut scope_has_files = false;
        let mut entries = self
            .files
            .iter()
            .filter(|file| !file.path.as_os_str().is_empty())
            .filter_map(|file| {
                let root = containing_root(&file.path)?;
                Some((file, root))
            })
            // Match the glob pattern relative to the search root, not the
            // project root. When a caller passes `{path: "src", pattern:
            // "a.rs"}`, the pattern must match `a.rs` (the path relative to
            // `src`), not `src/a.rs` (the path relative to the project root).
            .filter(|(file, root)| {
                // A file scope must match its basename, not the empty path
                // relative to itself. Containment above still excludes siblings.
                let filter_root = if file_scope {
                    root.parent().unwrap_or(root)
                } else {
                    root
                };
                filters.matches(filter_root, &file.path)
            })
            .map(|(file, _)| {
                scope_has_files = true;
                (file.path.clone(), file.modified)
            })
            .collect::<Vec<_>>();

        if sort_by_mtime {
            entries.sort_by(|(left_path, left_mtime), (right_path, right_mtime)| {
                right_mtime
                    .cmp(left_mtime)
                    .then_with(|| left_path.cmp(right_path))
            });
        }

        (
            entries.into_iter().map(|(path, _)| path).collect(),
            scope_has_files,
            entries_visited,
        )
    }

    pub fn candidates(&self, query: &RegexQuery) -> Vec<u32> {
        if query.and_trigrams.is_empty() && query.or_groups.is_empty() {
            return self.active_file_ids();
        }

        let mut and_trigrams = query.and_trigrams.clone();
        and_trigrams.sort_unstable_by_key(|trigram| self.posting_count(*trigram));

        let mut current: Option<Vec<u32>> = None;

        for trigram in and_trigrams {
            let filter = query.and_filters.get(&trigram).copied();
            let matches = self.postings_for_trigram(trigram, filter);
            current = Some(match current.take() {
                Some(existing) => intersect_sorted_ids(&existing, &matches),
                None => matches,
            });

            if current.as_ref().is_some_and(|ids| ids.is_empty()) {
                break;
            }
        }

        let mut current = current.unwrap_or_else(|| self.active_file_ids());

        for (index, group) in query.or_groups.iter().enumerate() {
            let mut group_matches = Vec::new();
            let filters = query.or_filters.get(index);

            for trigram in group {
                let filter = filters.and_then(|filters| filters.get(trigram).copied());
                let matches = self.postings_for_trigram(*trigram, filter);
                if group_matches.is_empty() {
                    group_matches = matches;
                } else {
                    group_matches = union_sorted_ids(&group_matches, &matches);
                }
            }

            current = intersect_sorted_ids(&current, &group_matches);
            if current.is_empty() {
                break;
            }
        }

        let mut unindexed = self
            .unindexed_files
            .iter()
            .copied()
            .filter(|file_id| self.is_active_file(*file_id))
            .collect::<Vec<_>>();
        if !unindexed.is_empty() {
            unindexed.sort_unstable();
            current = union_sorted_ids(&current, &unindexed);
        }

        current
    }

    fn posting_count(&self, trigram: u32) -> usize {
        let base_count = self
            .base
            .as_ref()
            .and_then(|base| base.lookup_entry(trigram))
            .map_or(0usize, |entry| entry.count as usize);
        base_count.saturating_add(self.delta.postings.get(&trigram).map_or(0usize, Vec::len))
    }

    fn active_file_ids(&self) -> Vec<u32> {
        let mut ids: Vec<u32> = self.path_to_id.values().copied().collect();
        ids.retain(|file_id| self.is_active_file(*file_id));
        ids.sort_unstable();
        ids
    }

    fn is_active_file(&self, file_id: u32) -> bool {
        if self.delta.superseded.contains(&file_id) {
            return false;
        }
        self.files
            .get(file_id as usize)
            .map(|file| !file.path.as_os_str().is_empty())
            .unwrap_or(false)
    }

    fn postings_for_trigram(&self, trigram: u32, filter: Option<PostingFilter>) -> Vec<u32> {
        #[cfg(debug_assertions)]
        POSTINGS_FOR_TRIGRAM_CALLS.with(|calls| calls.set(calls.get().saturating_add(1)));

        let mut matches = Vec::new();

        if let Some(base_entry) = self
            .base
            .as_ref()
            .and_then(|base| base.lookup_entry(trigram))
        {
            if let Some(base) = &self.base {
                matches.reserve(base_entry.count as usize);
                let _ = base.for_each_posting(base_entry, |file_id, next_mask, loc_mask| {
                    if crate::executor::current_job_cancelled() {
                        return false;
                    }
                    if self.delta.superseded.contains(&file_id) {
                        return true;
                    }
                    let posting = Posting {
                        file_id,
                        next_mask,
                        loc_mask,
                    };
                    if !posting_matches_filter(&posting, filter) {
                        return true;
                    }
                    if self.is_active_file(file_id) {
                        matches.push(file_id);
                    }
                    true
                });
            }
        }

        if let Some(postings) = self.delta.postings.get(&trigram) {
            matches.reserve(postings.len());
            for (posting_index, posting) in postings.iter().enumerate() {
                if posting_index % BORROWED_INDEX_CHECKPOINT_INTERVAL == 0
                    && crate::executor::current_job_cancelled()
                {
                    break;
                }
                if !posting_matches_filter(posting, filter) {
                    continue;
                }
                if self.is_active_file(posting.file_id) {
                    matches.push(posting.file_id);
                }
            }
        }

        if matches.len() > 1 {
            matches.sort_unstable();
            matches.dedup();
        }
        matches
    }
}

fn posting_matches_filter(posting: &Posting, filter: Option<PostingFilter>) -> bool {
    if let Some(filter) = filter {
        // next_mask is a bloom filter: the character following this trigram in
        // the query must also appear after this trigram somewhere in the file.
        if filter.next_mask != 0 && posting.next_mask & filter.next_mask == 0 {
            return false;
        }
        // loc_mask is persisted for future adjacency checks. It is intentionally
        // not used as a single-trigram filter because query positions do not
        // correspond to file positions.
    }
    true
}

/// What happened to one match occurrence handed to a candidate-file scan.
enum MatchVisit {
    /// The scan must end: a stop flag, the result cap, cancellation or the
    /// deadline.
    Stop,
    /// The occurrence is on a line that already produced a match.
    SameLine,
    /// The occurrence was the first on its line; skipping to the next line
    /// avoids searching the rest of that line again.
    Recorded { next_line_start: usize },
}

fn search_candidate_file(
    file: &FileEntry,
    floor_disclosure: &crate::context::confinement::Fence,
    matcher: &SearchMatcher,
    max_results: usize,
    stop_after: usize,
    total_matches: &AtomicUsize,
    files_searched: &AtomicUsize,
    files_with_matches: &AtomicUsize,
    bytes_verified: &AtomicUsize,
    truncated: &AtomicBool,
    engine_capped: &AtomicBool,
    missing_on_disk: &AtomicUsize,
    stop_scan: Option<&Arc<AtomicBool>>,
    job_cancellation: Option<&crate::executor::JobCancellation>,
    deadline: &GrepScanDeadline,
) -> Vec<SharedGrepMatch> {
    if grep_scan_should_stop(
        stop_scan,
        truncated,
        total_matches,
        stop_after,
        job_cancellation,
        deadline,
    ) {
        engine_capped.store(true, Ordering::Relaxed);
        return Vec::new();
    }

    let content = match read_indexed_file_bytes(&file.path, deadline.read_at) {
        Ok(content) => content,
        Err(error) => {
            if error.kind() == std::io::ErrorKind::PermissionDenied {
                floor_disclosure.record_matched_source_exclusion(&file.path);
            }
            if error.kind() == std::io::ErrorKind::TimedOut {
                deadline.reached.store(true, Ordering::Relaxed);
                engine_capped.store(true, Ordering::Relaxed);
            }
            if io_error_means_missing_on_disk(&error) {
                missing_on_disk.fetch_add(1, Ordering::Relaxed);
            }
            return Vec::new();
        }
    };
    bytes_verified.fetch_add(content.len(), Ordering::Relaxed);
    // Defense in depth: even though indexing tries to filter binaries via
    // The index classifies bytes at ingestion; we double-check at
    // query time. content_inspector is fast (~bytes-per-cycle on a small
    // preview) and this guarantees we never surface matches inside binary
    // files even if the indexer somehow let one through (e.g. file changed
    // between indexing and query).
    if is_binary_bytes(&content) {
        return Vec::new();
    }
    files_searched.fetch_add(1, Ordering::Relaxed);

    let shared_path = Arc::new(file.path.clone());
    let mut matches = Vec::new();
    let mut line_starts = None;
    // Grep reports one match per line: the first one. Occurrences arrive in
    // increasing offset order, so any occurrence before the start of the line
    // after the last reported one is on that same line. Checking that offset
    // first means a line with many occurrences (minified code, single-line
    // JSON, lockfiles) costs one line lookup and one line text in total, not
    // one per occurrence.
    let mut reported_line_end = 0usize;
    let mut matched_this_file = false;

    {
        let mut visit = |offset: usize, end: usize| -> MatchVisit {
            if grep_scan_should_stop(
                stop_scan,
                truncated,
                total_matches,
                stop_after,
                job_cancellation,
                deadline,
            ) {
                engine_capped.store(true, Ordering::Relaxed);
                return MatchVisit::Stop;
            }
            if offset < reported_line_end {
                return MatchVisit::SameLine;
            }

            let line_starts = line_starts.get_or_insert_with(|| line_starts_bytes(&content));
            let (line, column, line_text, next_line_start) =
                line_details_bytes(&content, line_starts, offset);
            reported_line_end = next_line_start;

            matched_this_file = true;
            let match_number = total_matches.fetch_add(1, Ordering::Relaxed) + 1;
            if match_number > max_results {
                truncated.store(true, Ordering::Relaxed);
                signal_grep_scan_cap(stop_scan, total_matches, stop_after);
                return MatchVisit::Stop;
            }

            matches.push(SharedGrepMatch {
                file: shared_path.clone(),
                line,
                column,
                line_text,
                match_text: String::from_utf8_lossy(&content[offset..end]).into_owned(),
            });
            MatchVisit::Recorded { next_line_start }
        };

        match matcher {
            SearchMatcher::Literal(literal) => {
                let needle = &literal.needle;
                if !needle.contains(&b'\n') {
                    let lowered;
                    let haystack: &[u8] = if literal.case_insensitive_ascii {
                        lowered = content.to_ascii_lowercase();
                        &lowered
                    } else {
                        &content
                    };
                    let finder = memchr::memmem::Finder::new(needle);
                    let mut start = 0;
                    while start <= haystack.len() {
                        let Some(position) = finder.find(&haystack[start..]) else {
                            break;
                        };
                        let offset = start + position;
                        start = match visit(offset, offset + needle.len()) {
                            MatchVisit::Stop => break,
                            MatchVisit::SameLine => offset + 1,
                            MatchVisit::Recorded { next_line_start } => {
                                next_line_start.max(offset + 1)
                            }
                        };
                    }
                }
            }
            SearchMatcher::Regex(regex) => {
                pattern_compile::for_each_line_match(regex, &content, |start, end| {
                    !matches!(visit(start, end), MatchVisit::Stop)
                });
            }
        }
    }

    if matched_this_file {
        files_with_matches.fetch_add(1, Ordering::Relaxed);
    }

    matches
}

/// Every line of `content` the matcher matches, in file order: the first
/// `max_lines` of them, plus any later line `keep_past_limit` accepts, each
/// with its first match on the line, and the count of all of them. Match
/// positions and line numbering are the same as [`search_candidate_file`]
/// reports, so a line found here is a line grep finds.
fn matching_lines_in_content(
    path: &Path,
    content: &[u8],
    matcher: &SearchMatcher,
    max_lines: usize,
    keep_past_limit: &(dyn Fn(&GrepMatch) -> bool + Sync),
) -> (Vec<GrepMatch>, usize) {
    let mut matches = Vec::new();
    let mut matched_lines = 0usize;
    let mut line_starts: Option<Vec<usize>> = None;
    // Start of the line after the last recorded one. Offsets arrive in
    // increasing order, so an offset before it is on the line just recorded;
    // only that line's first match is kept, as grep does, and the repeat is
    // dropped before any line text is built for it.
    let mut reported_line_end = 0usize;
    let mut record = |start: usize, end: usize| -> Option<usize> {
        if start < reported_line_end {
            return None;
        }
        let line_starts = line_starts.get_or_insert_with(|| line_starts_bytes(content));
        let (line, column, line_text, next_line_start) =
            line_details_bytes(content, line_starts, start);
        reported_line_end = next_line_start;
        matched_lines += 1;
        let grep_match = GrepMatch {
            file: path.to_path_buf(),
            line,
            column,
            line_text,
            match_text: String::from_utf8_lossy(&content[start..end]).into_owned(),
        };
        if matched_lines <= max_lines || keep_past_limit(&grep_match) {
            matches.push(grep_match);
        }
        Some(next_line_start)
    };
    match matcher {
        SearchMatcher::Literal(literal) => {
            let needle = &literal.needle;
            if !needle.contains(&b'\n') {
                let lowered;
                let haystack: &[u8] = if literal.case_insensitive_ascii {
                    lowered = content.to_ascii_lowercase();
                    &lowered
                } else {
                    content
                };
                let finder = memchr::memmem::Finder::new(needle);
                let mut start = 0;
                while start <= haystack.len() {
                    let Some(position) = finder.find(&haystack[start..]) else {
                        break;
                    };
                    let offset = start + position;
                    start = record(offset, offset + needle.len())
                        .map_or(offset + 1, |next_line_start| {
                            next_line_start.max(offset + 1)
                        });
                }
            }
        }
        SearchMatcher::Regex(regex) => {
            pattern_compile::for_each_line_match(regex, content, |start, end| {
                record(start, end);
                true
            });
        }
    }
    (matches, matched_lines)
}

fn should_stop_search(
    truncated: &AtomicBool,
    total_matches: &AtomicUsize,
    stop_after: usize,
) -> bool {
    truncated.load(Ordering::Relaxed) && total_matches.load(Ordering::Relaxed) >= stop_after
}

/// Wall-clock bound on one indexed grep scan whose caller sets none. A grep
/// holds its root's reader slot while it runs, and a configure for that root
/// waits for readers, so an unbounded scan locks every later bind on the root
/// out of its tools. It gets the same budget as the index-unavailable walk.
pub(crate) const INDEXED_GREP_SCAN_BUDGET: Duration = crate::grep_executor::FALLBACK_WALK_BUDGET;

/// The wall-clock point a grep scan must stop at, and whether the scan
/// stopped there. The scan loops check it per candidate file and per match
/// occurrence, so one huge file cannot carry a scan past it.
pub(crate) struct GrepScanDeadline {
    /// `None` when the budget is too large to represent as an instant, which
    /// leaves the scan unbounded in time.
    at: Option<Instant>,
    /// File reads use real time, even when a test injects a scan clock.
    read_at: Option<Instant>,
    now: fn() -> Instant,
    reached: AtomicBool,
}

impl GrepScanDeadline {
    fn after_with_clock(budget: Duration, now: fn() -> Instant) -> Self {
        let at = now()
            .checked_add(budget)
            .map(|at| crate::bounded_io::current().map_or(at, |budget| at.min(budget.deadline)));
        Self {
            at,
            read_at: at,
            now,
            reached: AtomicBool::new(false),
        }
    }

    /// True once the deadline has passed; records that the scan hit it.
    fn passed(&self) -> bool {
        if self.reached.load(Ordering::Relaxed) {
            return true;
        }
        if self.at.is_some_and(|at| (self.now)() >= at) {
            self.reached.store(true, Ordering::Relaxed);
            return true;
        }
        false
    }

    /// Whether a scan stopped because of this deadline.
    pub(crate) fn reached(&self) -> bool {
        self.reached.load(Ordering::Relaxed)
    }
}

fn grep_scan_should_stop(
    stop_scan: Option<&Arc<AtomicBool>>,
    truncated: &AtomicBool,
    total_matches: &AtomicUsize,
    stop_after: usize,
    job_cancellation: Option<&crate::executor::JobCancellation>,
    deadline: &GrepScanDeadline,
) -> bool {
    job_cancellation.is_some_and(|token| token.cancel_requested_before_commit())
        || stop_scan.is_some_and(|flag| flag.load(Ordering::Relaxed))
        || should_stop_search(truncated, total_matches, stop_after)
        || deadline.passed()
}

fn signal_grep_scan_cap(
    stop_scan: Option<&Arc<AtomicBool>>,
    total_matches: &AtomicUsize,
    stop_after: usize,
) {
    if let Some(flag) = stop_scan {
        if total_matches.load(Ordering::Relaxed) >= stop_after {
            flag.store(true, Ordering::Relaxed);
        }
    }
}

fn search_file_metadata(metadata: &fs::Metadata) -> SearchFileMetadata {
    SearchFileMetadata {
        size: metadata.len(),
        modified: metadata.modified().unwrap_or(UNIX_EPOCH),
    }
}

fn metadata_for_indexed_content(path: &Path, size_hint: u64) -> SearchFileMetadata {
    fs::metadata(path)
        .ok()
        .map(|metadata| search_file_metadata(&metadata))
        .unwrap_or(SearchFileMetadata {
            size: size_hint,
            modified: UNIX_EPOCH,
        })
}

fn prepare_search_path(path: &Path, max_file_size: u64) -> PreparedSearchPath {
    // The trigram index does not consume the generated flag. Other corpus
    // readers still request it, preserving their eligibility policy.
    match read_search_corpus_file_inner(path, max_file_size, false) {
        SearchCorpusEligibility::Eligible(file) => {
            PreparedSearchPath::Indexed(PreparedIndexedFile {
                metadata: file.metadata,
                content_hash: cache_freshness::hash_bytes(&file.bytes),
                trigram_map: trigram_filter_map(&file.bytes, true),
            })
        }
        SearchCorpusEligibility::Unindexed(metadata) => PreparedSearchPath::Unindexed(metadata),
        SearchCorpusEligibility::Skipped => PreparedSearchPath::Skipped,
    }
}

pub(crate) fn read_search_corpus_file(path: &Path, max_file_size: u64) -> SearchCorpusEligibility {
    read_search_corpus_file_inner(path, max_file_size, true)
}

fn read_search_corpus_file_inner(
    path: &Path,
    max_file_size: u64,
    classify_generated: bool,
) -> SearchCorpusEligibility {
    if crate::project_content::is_excluded(path) {
        return SearchCorpusEligibility::Skipped;
    }
    #[cfg(test)]
    cache_freshness::record_metadata_call(path);
    let metadata = match fs::metadata(path) {
        Ok(metadata) if metadata.is_file() => search_file_metadata(&metadata),
        _ => return SearchCorpusEligibility::Skipped,
    };
    read_search_corpus_file_with_metadata(path, max_file_size, classify_generated, metadata)
}

fn read_search_corpus_file_with_metadata(
    path: &Path,
    max_file_size: u64,
    classify_generated: bool,
    metadata: SearchFileMetadata,
) -> SearchCorpusEligibility {
    #[cfg(test)]
    AUDIT_IO_PROBE.with(|probe| {
        if let Some(probe) = probe.borrow_mut().as_mut() {
            probe();
        }
    });
    if metadata.size > max_file_size {
        return SearchCorpusEligibility::Unindexed(metadata);
    }

    #[cfg(test)]
    crate::search_hot_path_measurements::record_file_read();
    // Bound the read itself as well as admission: the file can grow after stat.
    let mut bytes =
        Vec::with_capacity(usize::try_from(metadata.size.min(max_file_size)).unwrap_or(0));
    #[cfg(test)]
    audit_record(|work| work.corpus_opens += 1);
    #[cfg(test)]
    cache_freshness::record_corpus_read(path);
    let read = crate::project_content::open(path).and_then(|file| {
        file.take(max_file_size.saturating_add(1))
            .read_to_end(&mut bytes)
    });
    if read.is_err() {
        return SearchCorpusEligibility::Skipped;
    }
    if bytes.len() as u64 > max_file_size || is_binary_bytes(&bytes) {
        return SearchCorpusEligibility::Unindexed(metadata);
    }

    #[cfg(test)]
    if classify_generated {
        audit_record(|work| work.corpus_opens += 1);
    }
    SearchCorpusEligibility::Eligible(SearchCorpusFile {
        metadata,
        generated: classify_generated && crate::inspect::is_generated_file(path, path),
        bytes,
    })
}

/// Returns the worker pool size for cold search-index builds: half of available
/// cores, capped at 8 to keep the same limit used by the callgraph store.
fn search_index_build_pool_size() -> usize {
    std::thread::available_parallelism()
        .map(|parallelism| parallelism.get())
        .unwrap_or(1)
        .div_ceil(2)
        .clamp(1, 8)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct SpillRecord {
    trigram: u32,
    file_id: u32,
    next_mask: u8,
    loc_mask: u8,
}

struct CacheWritePlan {
    project_root: PathBuf,
    git_head: Option<String>,
    ignore_fingerprint: String,
    max_file_size: u64,
    files: Vec<FileEntry>,
    path_to_id: HashMap<PathBuf, u32>,
    unindexed_files: HashSet<u32>,
    file_trigram_count: Vec<u32>,
    id_map: Arc<HashMap<u32, u32>>,
}

impl CacheWritePlan {
    fn from_index(index: &SearchIndex, git_head: Option<&str>) -> Option<Self> {
        let active_ids = index.active_file_ids();
        let mut id_map = HashMap::with_capacity(active_ids.len());
        for (new_id, old_id) in active_ids.iter().enumerate() {
            let new_id = u32::try_from(new_id).ok()?;
            id_map.insert(*old_id, new_id);
        }

        let mut files = Vec::with_capacity(active_ids.len());
        let mut path_to_id = HashMap::with_capacity(active_ids.len());
        let mut unindexed_files = HashSet::new();
        let mut file_trigram_count = Vec::with_capacity(active_ids.len());
        for old_id in active_ids {
            let new_id = *id_map.get(&old_id)?;
            let file = index.files.get(old_id as usize)?.clone();
            if file.path.as_os_str().is_empty() {
                continue;
            }
            path_to_id.insert(file.path.clone(), new_id);
            if index.unindexed_files.contains(&old_id) {
                unindexed_files.insert(new_id);
            }
            file_trigram_count.push(
                index
                    .file_trigram_count
                    .get(old_id as usize)
                    .copied()
                    .unwrap_or(0),
            );
            files.push(file);
        }

        Some(Self {
            project_root: index.project_root.clone(),
            git_head: git_head.map(ToOwned::to_owned),
            ignore_fingerprint: if index.ignore_rules_fingerprint.is_empty() {
                ignore_rules_fingerprint(&index.project_root)
            } else {
                index.ignore_rules_fingerprint.clone()
            },
            max_file_size: index.max_file_size,
            files,
            path_to_id,
            unindexed_files,
            file_trigram_count,
            id_map: Arc::new(id_map),
        })
    }
}

trait PostingRecordSource {
    fn next_record(&mut self) -> std::io::Result<Option<SpillRecord>>;
}

struct VecRecordSource {
    records: Arc<Vec<SpillRecord>>,
    index: usize,
    end: usize,
}

impl VecRecordSource {
    fn new(records: Vec<SpillRecord>) -> Self {
        Self {
            end: records.len(),
            records: Arc::new(records),
            index: 0,
        }
    }
}

impl PostingRecordSource for VecRecordSource {
    fn next_record(&mut self) -> std::io::Result<Option<SpillRecord>> {
        if self.index == self.end {
            return Ok(None);
        }
        let record = self.records.get(self.index).copied();
        if record.is_some() {
            self.index += 1;
        }
        Ok(record)
    }
}

const SEARCH_BUILD_RECORD_BATCH: usize = 8192;

fn check_search_build(keep_going: &dyn Fn() -> bool) -> std::io::Result<()> {
    if keep_going() {
        Ok(())
    } else {
        Err(std::io::Error::new(
            std::io::ErrorKind::Interrupted,
            "search refresh superseded",
        ))
    }
}

fn sorted_record_sources(
    mut records: Vec<SpillRecord>,
    keep_going: &dyn Fn() -> bool,
) -> std::io::Result<Vec<Box<dyn PostingRecordSource>>> {
    // Sort bounded runs so cancellation need not wait for the entire in-memory
    // posting block.
    // The normal merge restores the same global (trigram, file_id) ordering.
    for run in records.chunks_mut(SEARCH_BUILD_RECORD_BATCH) {
        check_search_build(keep_going)?;
        run.sort_unstable_by_key(|record| (record.trigram, record.file_id));
    }
    check_search_build(keep_going)?;
    let records = Arc::new(records);
    Ok((0..records.len())
        .step_by(SEARCH_BUILD_RECORD_BATCH)
        .map(|index| {
            Box::new(VecRecordSource {
                records: records.clone(),
                index,
                end: (index + SEARCH_BUILD_RECORD_BATCH).min(records.len()),
            }) as Box<dyn PostingRecordSource>
        })
        .collect())
}

struct SpillSegmentSource {
    reader: BufReader<File>,
    remaining_records: u64,
    current_trigram: u32,
    remaining_in_group: u32,
}

impl SpillSegmentSource {
    fn open(path: &Path) -> std::io::Result<Self> {
        let mut reader = BufReader::new(File::open(path)?);
        let mut magic = [0u8; 8];
        reader.read_exact(&mut magic)?;
        if &magic != SPILL_MAGIC {
            return Err(std::io::Error::other("invalid search spill magic"));
        }
        if read_u32(&mut reader)? != INDEX_VERSION {
            return Err(std::io::Error::other("invalid search spill version"));
        }
        let remaining_records = read_u64(&mut reader)?;
        Ok(Self {
            reader,
            remaining_records,
            current_trigram: 0,
            remaining_in_group: 0,
        })
    }
}

impl PostingRecordSource for SpillSegmentSource {
    fn next_record(&mut self) -> std::io::Result<Option<SpillRecord>> {
        if self.remaining_records == 0 {
            return Ok(None);
        }
        if self.remaining_in_group == 0 {
            self.current_trigram = read_u32(&mut self.reader)?;
            self.remaining_in_group = read_u32(&mut self.reader)?;
            if self.remaining_in_group == 0 {
                return Err(std::io::Error::other("empty search spill group"));
            }
        }
        let mut file_id = [0u8; 4];
        self.reader.read_exact(&mut file_id)?;
        let mut masks = [0u8; 2];
        self.reader.read_exact(&mut masks)?;
        self.remaining_in_group -= 1;
        self.remaining_records -= 1;
        Ok(Some(SpillRecord {
            trigram: self.current_trigram,
            file_id: u32::from_le_bytes(file_id),
            next_mask: masks[0],
            loc_mask: masks[1],
        }))
    }
}

struct BaseRecordSource {
    base: Arc<BasePostings>,
    id_map: Arc<HashMap<u32, u32>>,
    delta: Arc<DeltaState>,
    lookup_index: usize,
    current: Vec<SpillRecord>,
    current_index: usize,
}

impl BaseRecordSource {
    fn new(
        base: Arc<BasePostings>,
        id_map: Arc<HashMap<u32, u32>>,
        delta: Arc<DeltaState>,
    ) -> Self {
        Self {
            base,
            id_map,
            delta,
            lookup_index: 0,
            current: Vec::new(),
            current_index: 0,
        }
    }

    fn load_next_group(&mut self) -> std::io::Result<bool> {
        while let Some(entry) = self.base.lookup.get(self.lookup_index).copied() {
            self.lookup_index += 1;
            let postings = self.base.read_postings(entry)?;
            self.current.clear();
            self.current_index = 0;
            for posting in postings {
                if self.delta.superseded.contains(&posting.file_id) {
                    continue;
                }
                let Some(mapped_file_id) = self.id_map.get(&posting.file_id).copied() else {
                    continue;
                };
                self.current.push(SpillRecord {
                    trigram: entry.trigram,
                    file_id: mapped_file_id,
                    next_mask: posting.next_mask,
                    loc_mask: posting.loc_mask,
                });
            }
            if !self.current.is_empty() {
                return Ok(true);
            }
        }
        Ok(false)
    }
}

impl PostingRecordSource for BaseRecordSource {
    fn next_record(&mut self) -> std::io::Result<Option<SpillRecord>> {
        if self.current_index >= self.current.len() && !self.load_next_group()? {
            return Ok(None);
        }
        let record = self.current[self.current_index];
        self.current_index += 1;
        Ok(Some(record))
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct HeapItem {
    record: SpillRecord,
    source_index: usize,
}

impl Ord for HeapItem {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        other
            .record
            .trigram
            .cmp(&self.record.trigram)
            .then_with(|| other.record.file_id.cmp(&self.record.file_id))
            .then_with(|| other.source_index.cmp(&self.source_index))
    }
}

impl PartialOrd for HeapItem {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

#[cfg(test)]
type RefreshReadProbe = (Arc<AtomicUsize>, Arc<AtomicBool>);
#[cfg(test)]
static REFRESH_READ_PROBES: OnceLock<Mutex<BTreeMap<PathBuf, RefreshReadProbe>>> = OnceLock::new();
#[cfg(test)]
thread_local! {
    static FAIL_STREAMING_BUILD: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

#[cfg(test)]
fn note_refresh_read_for_test(root: &Path) {
    if let Some((reads, cancelled)) = REFRESH_READ_PROBES
        .get_or_init(Mutex::default)
        .lock()
        .unwrap()
        .get(root)
    {
        reads.fetch_add(1, Ordering::SeqCst);
        cancelled.store(true, Ordering::SeqCst);
    }
}

fn build_streaming_index(
    root: &Path,
    max_file_size: u64,
    cache_dir: &Path,
    keep_going: &(dyn Fn() -> bool + Sync),
) -> std::io::Result<(SearchIndex, usize)> {
    let _progress = crate::cold_build_limiter::progress::start(
        root,
        "trigram build",
        None,
        crate::cold_build_limiter::progress::StartLog::Info,
    );
    crate::cold_build_limiter::progress::phase("enumerating", None);
    #[cfg(test)]
    if FAIL_STREAMING_BUILD.replace(false) {
        return Err(std::io::Error::other("injected streaming cache failure"));
    }
    crate::private_storage::open_keyed_dir(cache_dir, "index")?;
    sweep_stale_search_build_dirs(cache_dir);
    let project_root = fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    let ignore_fingerprint = ignore_rules_fingerprint_cancellable(&project_root, keep_going)
        .ok_or(std::io::ErrorKind::Interrupted)?;
    let filters = PathFilters::default();
    let paths =
        walk_build_files_cancellable(&project_root, &filters, keep_going).ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::Interrupted, "search refresh superseded")
        })?;
    crate::cold_build_limiter::progress::phase("indexing", Some(paths.len()));
    let pool_size = search_index_build_pool_size();
    let chunk_size = pool_size.saturating_mul(4).clamp(1, 32);
    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(pool_size)
        .thread_name(|index| format!("aft-search-build-{index}"))
        .stack_size(8 * 1024 * 1024)
        .build()
        .ok();

    let spill_dir = create_spill_dir(cache_dir)?;
    let spill_counter = crate::write_ledger::register(
        crate::write_ledger::Domain::SearchIndexBuild,
        project_root.display().to_string(),
    );
    let mut spill_paths = Vec::new();
    let mut spill_seq = 0usize;
    let mut block: Vec<SpillRecord> = Vec::new();
    let mut files = Vec::new();
    let mut path_to_id = HashMap::new();
    let mut unindexed_files = HashSet::new();
    let mut file_trigram_count = Vec::new();
    let mut indexed = 0usize;

    let build_result = (|| -> std::io::Result<BasePostings> {
        for chunk in paths.chunks(chunk_size) {
            if !keep_going() {
                return Err(std::io::ErrorKind::Interrupted.into());
            }
            let prepare_chunk = || -> Vec<PreparedSearchPath> {
                chunk
                    .par_iter()
                    .map(|path| {
                        if keep_going() {
                            let prepared = prepare_search_path(path, max_file_size);
                            #[cfg(test)]
                            note_refresh_read_for_test(&project_root);
                            prepared
                        } else {
                            PreparedSearchPath::Skipped
                        }
                    })
                    .collect()
            };
            let prepared = match &pool {
                Some(pool) => pool.install(prepare_chunk),
                None => prepare_chunk(),
            };

            for (path, prepared) in chunk.iter().zip(prepared) {
                match prepared {
                    PreparedSearchPath::Indexed(file) => {
                        let file_id = u32::try_from(files.len())
                            .map_err(|_| std::io::Error::other("too many files to index"))?;
                        files.push(FileEntry {
                            path: path.clone(),
                            size: file.metadata.size,
                            modified: file.metadata.modified,
                            content_hash: file.content_hash,
                        });
                        path_to_id.insert(path.clone(), file_id);
                        file_trigram_count.push(file.trigram_map.len() as u32);
                        for (trigram, filter) in file.trigram_map {
                            block.push(SpillRecord {
                                trigram,
                                file_id,
                                next_mask: filter.next_mask,
                                loc_mask: filter.loc_mask,
                            });
                        }
                        indexed += 1;
                    }
                    PreparedSearchPath::Unindexed(metadata) => {
                        let file_id = u32::try_from(files.len())
                            .map_err(|_| std::io::Error::other("too many files to index"))?;
                        files.push(FileEntry {
                            path: path.clone(),
                            size: metadata.size,
                            modified: metadata.modified,
                            content_hash: cache_freshness::zero_hash(),
                        });
                        path_to_id.insert(path.clone(), file_id);
                        unindexed_files.insert(file_id);
                        file_trigram_count.push(0);
                        indexed += 1;
                    }
                    PreparedSearchPath::Skipped => {}
                }

                let block_bytes = block.len().saturating_mul(SPILL_RECORD_ESTIMATED_BYTES);
                if block_bytes >= SPIMI_SOFT_LIMIT_BYTES || block_bytes >= SPIMI_HARD_LIMIT_BYTES {
                    let path =
                        flush_spill_segment(&spill_dir, spill_seq, &mut block, &spill_counter)?;
                    spill_paths.push(path);
                    spill_seq += 1;
                }
            }
            crate::cold_build_limiter::progress::advance(chunk.len());
        }

        crate::cold_build_limiter::progress::phase("publishing", None);
        if !keep_going() {
            return Err(std::io::ErrorKind::Interrupted.into());
        }
        let mut sources = sorted_record_sources(std::mem::take(&mut block), keep_going)?;
        for path in &spill_paths {
            check_search_build(keep_going)?;
            sources.push(Box::new(SpillSegmentSource::open(path)?));
        }

        let plan = CacheWritePlan {
            project_root: project_root.clone(),
            git_head: current_git_head(&project_root),
            ignore_fingerprint: ignore_fingerprint.clone(),
            max_file_size,
            files: files.clone(),
            path_to_id: path_to_id.clone(),
            unindexed_files: unindexed_files.clone(),
            file_trigram_count: file_trigram_count.clone(),
            id_map: Arc::new(
                (0..files.len())
                    .filter_map(|id| {
                        let id = u32::try_from(id).ok()?;
                        Some((id, id))
                    })
                    .collect(),
            ),
        };
        write_cache_file_from_sources(
            cache_dir,
            &plan,
            &mut sources,
            crate::write_ledger::Domain::SearchIndexBuild,
            keep_going,
        )
    })();

    let _ = fs::remove_dir_all(&spill_dir);
    let base = build_result?;
    let base_file_count =
        u32::try_from(files.len()).map_err(|_| std::io::Error::other("too many files to index"))?;
    let git_head = current_git_head(&project_root);
    let index = SearchIndex {
        base: Some(Arc::new(base)),
        delta: Arc::new(DeltaState::default()),
        delta_file_trigrams: HashMap::new(),
        free_delta_file_ids: Vec::new(),
        files: Arc::new(files),
        path_to_id: Arc::new(path_to_id),
        ready: false,
        build_denied: false,
        project_root,
        git_head,
        max_file_size,
        ignore_rules_fingerprint: ignore_fingerprint,
        file_trigram_count: Arc::new(file_trigram_count),
        unindexed_files: Arc::new(unindexed_files),
        base_file_count,
        delta_packed_bytes: 0,
        compaction_state: Arc::new(Mutex::new(CompactionState::default())),
    };
    Ok((index, indexed))
}

/// Write `cache.bin` through a temp file and credit the bytes to the write
/// census under `domain`. Crediting here, where the bytes are produced, covers
/// every caller (cold streaming builds and base+delta rewrites alike); a build
/// that credits nothing shows up in the census as unexplained daemon writes.
fn write_cache_file_from_sources(
    cache_dir: &Path,
    plan: &CacheWritePlan,
    sources: &mut [Box<dyn PostingRecordSource>],
    domain: crate::write_ledger::Domain,
    keep_going: &dyn Fn() -> bool,
) -> std::io::Result<BasePostings> {
    check_search_build(keep_going)?;
    crate::private_storage::open_keyed_dir(cache_dir, "index")?;
    sweep_stale_search_build_dirs(cache_dir);
    let cache_path = cache_dir.join("cache.bin");
    let tmp_cache = cache_dir.join(format!(
        "cache.bin.tmp.{}.{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or(Duration::ZERO)
            .as_nanos()
    ));

    let write_result = (|| -> std::io::Result<BasePostings> {
        let raw = crate::private_storage::options()
            .write(true)
            .create_new(true)
            .open(&tmp_cache)?;
        let mut writer = BufWriter::new(raw);
        write_u32(&mut writer, CACHE_MAGIC)?;
        write_u32(&mut writer, INDEX_VERSION)?;
        let postings_len_patch = writer.stream_position()?;
        write_u64(&mut writer, 0)?;

        let postings_section_start = writer.stream_position()?;
        let postings_header = build_postings_header_bytes(plan, keep_going)?;
        write_search_build_bytes(&mut writer, &postings_header, keep_going)?;
        let postings_blob_len_patch = writer.stream_position()?;
        write_u64(&mut writer, 0)?;
        let postings_blob_start = writer.stream_position()?;

        let (lookup_entries, postings_blob_len) =
            merge_sources_to_writer(sources, &mut writer, keep_going)?;
        let extension = build_file_trigram_count_extension(&plan.file_trigram_count, keep_going)?;
        write_search_build_bytes(&mut writer, &extension, keep_going)?;
        let postings_crc_end = writer.stream_position()?;

        writer.flush()?;
        writer.seek(SeekFrom::Start(postings_blob_len_patch))?;
        write_u64(&mut writer, postings_blob_len)?;
        writer.flush()?;

        let checksum = crc32_file_range(
            &tmp_cache,
            postings_section_start,
            postings_crc_end.saturating_sub(postings_section_start),
            keep_going,
        )?;
        writer.seek(SeekFrom::Start(postings_crc_end))?;
        writer.write_all(&checksum.to_le_bytes())?;
        let postings_section_end = writer.stream_position()?;
        let postings_len_total = postings_section_end.saturating_sub(postings_section_start);
        writer.seek(SeekFrom::Start(postings_len_patch))?;
        write_u64(&mut writer, postings_len_total)?;
        writer.seek(SeekFrom::Start(postings_section_end))?;

        let lookup_blob = build_lookup_section_bytes(&lookup_entries, keep_going)?;
        write_search_build_bytes(&mut writer, &lookup_blob, keep_going)?;
        let file_bytes = writer.stream_position()?;
        writer.flush()?;
        // CRC-checked caches rebuild from source after power loss. The rename,
        // not a sync, keeps concurrent readers from seeing a partial write.
        drop(writer);

        check_search_build(keep_going)?;
        fs::rename(&tmp_cache, &cache_path)?;
        // Credit the bytes handed to the kernel; this cache is not a durable log.
        crate::write_ledger::credit(
            domain,
            plan.project_root.display().to_string(),
            file_bytes,
            file_bytes,
        );
        let file = open_cache_file_read(&cache_path)?;
        Ok(BasePostings {
            file: Arc::new(file),
            postings_blob_start,
            postings_blob_len,
            lookup: Arc::new(lookup_entries),
        })
    })();

    if write_result.is_err() {
        let _ = fs::remove_file(&tmp_cache);
    }
    write_result
}

fn merge_sources_to_writer(
    sources: &mut [Box<dyn PostingRecordSource>],
    writer: &mut BufWriter<File>,
    keep_going: &dyn Fn() -> bool,
) -> std::io::Result<(Vec<LookupEntry>, u64)> {
    let mut heap = BinaryHeap::new();
    for (source_index, source) in sources.iter_mut().enumerate() {
        check_search_build(keep_going)?;
        if let Some(record) = source.next_record()? {
            heap.push(HeapItem {
                record,
                source_index,
            });
        }
    }

    let mut lookup_entries = Vec::new();
    let mut postings_blob_len = 0u64;
    let mut current_trigram: Option<u32> = None;
    let mut current_offset = 0u64;
    let mut current_count = 0u32;

    while let Some(item) = heap.pop() {
        if postings_blob_len % (SEARCH_BUILD_RECORD_BATCH * POSTING_BYTES) as u64 == 0 {
            check_search_build(keep_going)?;
        }
        let record = item.record;
        if current_trigram != Some(record.trigram) {
            if let Some(trigram) = current_trigram {
                lookup_entries.push(LookupEntry {
                    trigram,
                    offset: current_offset,
                    count: current_count,
                });
            }
            current_trigram = Some(record.trigram);
            current_offset = postings_blob_len;
            current_count = 0;
        }

        writer.write_all(&record.file_id.to_le_bytes())?;
        writer.write_all(&[record.next_mask, record.loc_mask])?;
        postings_blob_len = postings_blob_len
            .checked_add(POSTING_BYTES as u64)
            .ok_or_else(|| std::io::Error::other("postings blob too large"))?;
        current_count = current_count
            .checked_add(1)
            .ok_or_else(|| std::io::Error::other("posting list too large"))?;

        if let Some(next) = sources[item.source_index].next_record()? {
            heap.push(HeapItem {
                record: next,
                source_index: item.source_index,
            });
        }
    }

    if let Some(trigram) = current_trigram {
        lookup_entries.push(LookupEntry {
            trigram,
            offset: current_offset,
            count: current_count,
        });
    }

    check_search_build(keep_going)?;
    Ok((lookup_entries, postings_blob_len))
}

fn write_search_build_bytes(
    writer: &mut impl Write,
    bytes: &[u8],
    keep_going: &dyn Fn() -> bool,
) -> std::io::Result<()> {
    for chunk in bytes.chunks(64 * 1024) {
        check_search_build(keep_going)?;
        writer.write_all(chunk)?;
    }
    Ok(())
}

fn build_postings_header_bytes(
    plan: &CacheWritePlan,
    keep_going: &dyn Fn() -> bool,
) -> std::io::Result<Vec<u8>> {
    check_search_build(keep_going)?;
    let mut writer = BufWriter::new(Cursor::new(Vec::new()));
    writer.write_all(INDEX_MAGIC)?;
    write_u32(&mut writer, INDEX_VERSION)?;

    let head = plan.git_head.as_deref().unwrap_or_default();
    let root = plan.project_root.to_string_lossy();
    let head_len = u32::try_from(head.len())
        .map_err(|_| std::io::Error::other("git head too large to cache"))?;
    let root_len = u32::try_from(root.len())
        .map_err(|_| std::io::Error::other("project root too large to cache"))?;
    let ignore_fingerprint_len = u32::try_from(plan.ignore_fingerprint.len())
        .map_err(|_| std::io::Error::other("ignore fingerprint too large to cache"))?;
    let file_count = u32::try_from(plan.files.len())
        .map_err(|_| std::io::Error::other("too many files to cache"))?;

    write_u32(&mut writer, head_len)?;
    write_u32(&mut writer, root_len)?;
    write_u32(&mut writer, ignore_fingerprint_len)?;
    write_u64(&mut writer, plan.max_file_size)?;
    write_u32(&mut writer, file_count)?;
    writer.write_all(head.as_bytes())?;
    writer.write_all(root.as_bytes())?;
    writer.write_all(plan.ignore_fingerprint.as_bytes())?;

    for (file_id, file) in plan.files.iter().enumerate() {
        if file_id % SEARCH_BUILD_RECORD_BATCH == 0 {
            check_search_build(keep_going)?;
        }
        let file_id =
            u32::try_from(file_id).map_err(|_| std::io::Error::other("too many files to cache"))?;
        let path = storable_relative_path(&plan.project_root, &file.path).ok_or_else(|| {
            std::io::Error::other(format!(
                "refusing to cache path outside project root: {}",
                file.path.display()
            ))
        })?;
        let path = path.to_string_lossy();
        let path_len = u32::try_from(path.len())
            .map_err(|_| std::io::Error::other("cached path too large"))?;
        let modified = file
            .modified
            .duration_since(UNIX_EPOCH)
            .unwrap_or(Duration::ZERO);
        let unindexed = if plan.unindexed_files.contains(&file_id) {
            1u8
        } else {
            0u8
        };

        writer.write_all(&[unindexed])?;
        write_u32(&mut writer, path_len)?;
        write_u64(&mut writer, file.size)?;
        write_u64(&mut writer, modified.as_secs())?;
        write_u32(&mut writer, modified.subsec_nanos())?;
        writer.write_all(file.content_hash.as_bytes())?;
        writer.write_all(path.as_bytes())?;
    }

    writer.flush()?;
    Ok(writer
        .into_inner()
        .map_err(|error| std::io::Error::other(error.to_string()))?
        .into_inner())
}

fn build_lookup_section_bytes(
    lookup_entries: &[LookupEntry],
    keep_going: &dyn Fn() -> bool,
) -> std::io::Result<Vec<u8>> {
    let mut writer = BufWriter::new(Cursor::new(Vec::new()));
    let entry_count = u32::try_from(lookup_entries.len())
        .map_err(|_| std::io::Error::other("too many lookup entries to cache"))?;
    writer.write_all(LOOKUP_MAGIC)?;
    write_u32(&mut writer, INDEX_VERSION)?;
    write_u32(&mut writer, entry_count)?;
    for (index, entry) in lookup_entries.iter().enumerate() {
        if index % SEARCH_BUILD_RECORD_BATCH == 0 {
            check_search_build(keep_going)?;
        }
        write_u32(&mut writer, entry.trigram)?;
        write_u64(&mut writer, entry.offset)?;
        write_u32(&mut writer, entry.count)?;
    }
    writer.flush()?;
    let mut lookup_blob = writer
        .into_inner()
        .map_err(|error| std::io::Error::other(error.to_string()))?
        .into_inner();
    let mut hasher = crc32fast::Hasher::new();
    for chunk in lookup_blob.chunks(64 * 1024) {
        check_search_build(keep_going)?;
        hasher.update(chunk);
    }
    let checksum = hasher.finalize();
    lookup_blob.extend_from_slice(&checksum.to_le_bytes());
    Ok(lookup_blob)
}

fn build_file_trigram_count_extension(
    counts: &[u32],
    keep_going: &dyn Fn() -> bool,
) -> std::io::Result<Vec<u8>> {
    let mut writer = BufWriter::new(Cursor::new(Vec::new()));
    writer.write_all(FILE_TRIGRAM_COUNT_MAGIC)?;
    write_u32(&mut writer, INDEX_VERSION)?;
    write_u32(
        &mut writer,
        u32::try_from(counts.len())
            .map_err(|_| std::io::Error::other("too many file trigram counts"))?,
    )?;
    for (index, count) in counts.iter().enumerate() {
        if index % SEARCH_BUILD_RECORD_BATCH == 0 {
            check_search_build(keep_going)?;
        }
        write_u32(&mut writer, *count)?;
    }
    writer.flush()?;
    Ok(writer
        .into_inner()
        .map_err(|error| std::io::Error::other(error.to_string()))?
        .into_inner())
}

fn flush_spill_segment(
    spill_dir: &Path,
    seq: usize,
    block: &mut Vec<SpillRecord>,
    counter: &crate::write_ledger::Counter,
) -> std::io::Result<PathBuf> {
    if block.is_empty() {
        return Err(std::io::Error::other(
            "refusing to write empty search spill",
        ));
    }
    block.sort_unstable_by_key(|record| (record.trigram, record.file_id));
    let path = spill_dir.join(format!("segment.{seq:06}.bin"));
    let mut writer = BufWriter::new(crate::private_storage::create(&path)?);
    writer.write_all(SPILL_MAGIC)?;
    write_u32(&mut writer, INDEX_VERSION)?;
    write_u64(
        &mut writer,
        u64::try_from(block.len()).map_err(|_| std::io::Error::other("search spill too large"))?,
    )?;

    let mut index = 0usize;
    while index < block.len() {
        let trigram = block[index].trigram;
        let group_start = index;
        while index < block.len() && block[index].trigram == trigram {
            index += 1;
        }
        write_u32(&mut writer, trigram)?;
        write_u32(
            &mut writer,
            u32::try_from(index - group_start)
                .map_err(|_| std::io::Error::other("search spill group too large"))?,
        )?;
        for record in &block[group_start..index] {
            writer.write_all(&record.file_id.to_le_bytes())?;
            writer.write_all(&[record.next_mask, record.loc_mask])?;
        }
    }
    let segment_bytes = writer.stream_position()?;
    writer.flush()?;
    // Spill segments are read and deleted by this build. Kernel visibility is
    // enough; they have no recovery role after the process exits.
    counter.credit(segment_bytes, segment_bytes);
    block.clear();
    Ok(path)
}

fn create_spill_dir(cache_dir: &Path) -> std::io::Result<PathBuf> {
    let dir = cache_dir.join(format!(
        "search-build.tmp.{}.{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or(Duration::ZERO)
            .as_nanos()
    ));
    crate::private_storage::create_dir_all(&dir)?;
    Ok(dir)
}

fn sweep_stale_search_build_dirs(cache_dir: &Path) {
    // A vanished mounted child can make ReadDir::drop panic after closedir
    // returns ENXIO, aborting the daemon. Do not recurse through a foreign
    // temporary directory while cleaning stale search-index builds.
    let Ok(boundary) = crate::walk_boundary::DeviceBoundary::for_root(cache_dir) else {
        return;
    };
    let Ok(entries) = fs::read_dir(cache_dir) else {
        return;
    };
    for entry in entries.flatten() {
        let file_name = entry.file_name();
        let path = entry.path();
        if file_name.to_string_lossy().starts_with("search-build.tmp.") {
            if boundary.should_descend(&path).unwrap_or(false) {
                let _ = fs::remove_dir_all(path);
            } else {
                crate::slog_warn!(
                    "search-index sweep skipped foreign filesystem mount {}",
                    path.display()
                );
            }
        }
    }
}

fn transient_search_cache_home() -> PathBuf {
    std::env::temp_dir().join(TRANSIENT_SEARCH_CACHE_DIR_NAME)
}

fn transient_search_cache_dir(root: &Path) -> PathBuf {
    transient_search_cache_home().join(format!(
        "{TRANSIENT_SEARCH_CACHE_PREFIX}{}.{}",
        artifact_cache_key_for_build(root),
        std::process::id()
    ))
}

/// Return the process-local mutex for a process/root transient cache directory.
/// The PID in the directory name isolates separate processes, while this registry
/// prevents two threads from truncating the same directory during concurrent builds.
fn transient_search_cache_build_lock(cache_dir: &Path) -> Arc<Mutex<()>> {
    let locks = TRANSIENT_SEARCH_CACHE_BUILD_LOCKS.get_or_init(|| Mutex::new(HashMap::new()));
    let mut locks = locks
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(lock) = locks.get(cache_dir).and_then(Weak::upgrade) {
        return lock;
    }
    if locks.len() >= 1_024 {
        locks.retain(|_, lock| lock.strong_count() > 0);
    }
    let lock = Arc::new(Mutex::new(()));
    locks.insert(cache_dir.to_path_buf(), Arc::downgrade(&lock));
    lock
}

/// Reuse one scratch directory for this process/root pair. The streaming index
/// opens cache.bin as its postings store, so its previous files must be removed
/// only while the process-local cache lock is held.
fn truncate_transient_search_cache_dir(cache_dir: &Path) -> std::io::Result<()> {
    crate::private_storage::open_keyed_dir(cache_dir, "index")?;
    for entry in fs::read_dir(cache_dir)? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        let path = entry.path();
        if file_type.is_dir() {
            fs::remove_dir_all(path)?;
        } else {
            fs::remove_file(path)?;
        }
    }
    Ok(())
}

/// Reap stale timestamped caches from older AFT versions and stable caches left
/// by processes that exited without a future in-process rebuild. The directory
/// name contains a PID for diagnostics, but PID liveness is intentionally not
/// consulted: a bare PID can be recycled, while age remains trustworthy.
pub(crate) fn sweep_transient_search_cache_dirs() {
    // Our own directory, which holds one entry per process/root pair.
    let root = transient_search_cache_home();
    let summary = sweep_transient_search_cache_dirs_with_limits(
        &root,
        TRANSIENT_SEARCH_CACHE_MIN_AGE,
        TRANSIENT_SEARCH_CACHE_SWEEP_BUDGET,
        TRANSIENT_SEARCH_CACHE_SWEEP_LIMIT,
    );
    crate::slog_info!(
        "transient search cache sweep root={} scanned={} examined={} removed={} bytes={} skipped_fresh={} skipped_unreadable={} budget_exhausted={}",
        root.display(),
        summary.scanned,
        summary.examined,
        summary.removed,
        summary.bytes,
        summary.skipped_fresh,
        summary.skipped_unreadable,
        summary.budget_exhausted
    );
    sweep_legacy_transient_search_cache_dirs(&root);
}

/// Reap caches left at the top level of `$TMPDIR` by versions that wrote there
/// directly.
///
/// Runs inline, under the same iterator bound as the sweep above: the bound is
/// what makes reading a directory we do not own safe, and it is cheaper than
/// owning a thread. A legacy entry that sits beyond the scan limit is simply
/// reaped by a later pass once the directory shrinks; the entries are one
/// scratch directory per process and root, so nothing accumulates while we wait.
///
/// It does not run on a thread of its own on purpose: a detached thread that
/// outlives a test process aborted the storm suite on macOS (SIGBUS, train 132)
/// because it was still inside the sweep while the process tore its globals
/// down.
fn sweep_legacy_transient_search_cache_dirs(home: &Path) {
    let root = std::env::temp_dir();
    if root == home {
        return;
    }
    let summary = sweep_transient_search_cache_dirs_with_limits(
        &root,
        TRANSIENT_SEARCH_CACHE_MIN_AGE,
        TRANSIENT_SEARCH_CACHE_SWEEP_BUDGET,
        TRANSIENT_SEARCH_CACHE_SWEEP_LIMIT,
    );
    if summary.scanned > 0 || summary.removed > 0 {
        crate::slog_info!(
            "legacy transient search cache sweep root={} scanned={} examined={} removed={} bytes={} budget_exhausted={}",
            root.display(),
            summary.scanned,
            summary.examined,
            summary.removed,
            summary.bytes,
            summary.budget_exhausted
        );
    }
}

fn sweep_transient_search_cache_dirs_with_limits(
    root: &Path,
    min_age: Duration,
    wall_clock_budget: Duration,
    entry_limit: usize,
) -> TransientSearchCacheSweepSummary {
    let deadline = Instant::now() + wall_clock_budget;
    let boundary = match crate::walk_boundary::DeviceBoundary::for_root(root) {
        Ok(boundary) => boundary,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return TransientSearchCacheSweepSummary::default();
        }
        Err(error) => {
            crate::slog_warn!(
                "cannot establish filesystem boundary for transient search cache sweep {}: {}",
                root.display(),
                error
            );
            return TransientSearchCacheSweepSummary {
                skipped_unreadable: 1,
                ..TransientSearchCacheSweepSummary::default()
            };
        }
    };
    // Read entries under an explicit bound. The directory may be shared with
    // the rest of the machine, and a full listing of a fat one costs minutes on
    // the worker that calls this; the reap limit further down cannot help,
    // because it only bounds what we act on after the listing is complete.
    let mut examined = 0usize;
    let mut scan_truncated = false;
    let mut entries = match fs::read_dir(root) {
        Ok(iterator) => {
            let mut collected = Vec::new();
            for entry in iterator {
                if examined >= TRANSIENT_SEARCH_CACHE_SCAN_LIMIT || Instant::now() >= deadline {
                    scan_truncated = true;
                    break;
                }
                examined += 1;
                let Ok(entry) = entry else {
                    continue;
                };
                let Ok(file_type) = entry.file_type() else {
                    continue;
                };
                let name = entry.file_name().to_string_lossy().into_owned();
                if file_type.is_dir() && parse_transient_search_cache_name(&name).is_some() {
                    collected.push((name, entry.path()));
                }
            }
            collected
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(error) => {
            crate::slog_warn!(
                "cannot read transient search cache sweep directory {}: {}",
                root.display(),
                error
            );
            return TransientSearchCacheSweepSummary {
                examined,
                skipped_unreadable: 1,
                ..TransientSearchCacheSweepSummary::default()
            };
        }
    };
    entries.sort_by(|left, right| left.0.cmp(&right.0));

    let cursor_store =
        TRANSIENT_SEARCH_CACHE_SWEEP_CURSORS.get_or_init(|| Mutex::new(HashMap::new()));
    let last_name = cursor_store
        .lock()
        .ok()
        .and_then(|cursors| cursors.get(root).cloned());
    if let Some(start) = last_name
        .as_deref()
        .and_then(|last| entries.iter().position(|(name, _)| name.as_str() > last))
    {
        entries.rotate_left(start);
    }

    let now = SystemTime::now();
    let mut summary = TransientSearchCacheSweepSummary {
        examined,
        budget_exhausted: scan_truncated,
        ..TransientSearchCacheSweepSummary::default()
    };
    let mut cursor_name = last_name;
    for (processed, (name, path)) in entries.into_iter().enumerate() {
        if processed >= entry_limit || Instant::now() >= deadline {
            summary.budget_exhausted = true;
            break;
        }
        summary.scanned += 1;
        cursor_name = Some(name.clone());
        if !boundary.should_descend(&path).unwrap_or(false) {
            summary.skipped_unreadable += 1;
            continue;
        }
        let modified = match fs::metadata(&path).and_then(|metadata| metadata.modified()) {
            Ok(modified) => modified,
            Err(_) => {
                summary.skipped_unreadable += 1;
                continue;
            }
        };
        if now.duration_since(modified).unwrap_or(Duration::ZERO) <= min_age {
            summary.skipped_fresh += 1;
            continue;
        }
        let bytes = match transient_search_cache_dir_bytes(&path, &boundary, deadline) {
            TransientSearchCacheWalk::Complete(bytes) => bytes,
            TransientSearchCacheWalk::BudgetExceeded => {
                summary.budget_exhausted = true;
                break;
            }
            TransientSearchCacheWalk::Failed => {
                summary.skipped_unreadable += 1;
                continue;
            }
        };
        let Some(parsed) = parse_transient_search_cache_name(&name) else {
            summary.skipped_unreadable += 1;
            continue;
        };
        match fs::remove_dir_all(&path) {
            Ok(()) => {
                summary.removed += 1;
                summary.bytes = summary.bytes.saturating_add(bytes);
                crate::slog_info!(
                    "transient search cache sweep reaped dir={} key={} pid={} bytes={}",
                    path.display(),
                    parsed.key,
                    parsed.pid,
                    bytes
                );
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound && !path.exists() => {
                summary.removed += 1;
                summary.bytes = summary.bytes.saturating_add(bytes);
                crate::slog_info!(
                    "transient search cache sweep reaped dir={} key={} pid={} bytes={}",
                    path.display(),
                    parsed.key,
                    parsed.pid,
                    bytes
                );
            }
            Err(_) => summary.skipped_unreadable += 1,
        }
    }

    if let Ok(mut cursors) = cursor_store.lock() {
        if summary.budget_exhausted {
            if let Some(cursor_name) = cursor_name {
                cursors.insert(root.to_path_buf(), cursor_name);
            }
        } else {
            cursors.remove(root);
        }
    }
    if summary.removed > 0 {}
    summary
}

fn parse_transient_search_cache_name(name: &str) -> Option<TransientSearchCacheName> {
    let mut parts = name.strip_prefix(TRANSIENT_SEARCH_CACHE_PREFIX)?.split('.');
    let key = parts.next()?;
    if !artifact_key_looks_valid(key) {
        return None;
    }
    let pid = parts.next()?.parse::<u32>().ok().filter(|pid| *pid != 0)?;
    match (parts.next(), parts.next()) {
        (None, None) => {}
        (Some(nanos), None) if nanos.parse::<u128>().is_ok() => {}
        _ => return None,
    }
    Some(TransientSearchCacheName {
        key: key.to_owned(),
        pid,
    })
}

fn transient_search_cache_dir_bytes(
    directory: &Path,
    boundary: &crate::walk_boundary::DeviceBoundary,
    deadline: Instant,
) -> TransientSearchCacheWalk {
    if Instant::now() >= deadline {
        return TransientSearchCacheWalk::BudgetExceeded;
    }
    let entries = match fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(_) => return TransientSearchCacheWalk::Failed,
    };
    let mut bytes = 0u64;
    for entry in entries {
        if Instant::now() >= deadline {
            return TransientSearchCacheWalk::BudgetExceeded;
        }
        let entry = match entry {
            Ok(entry) => entry,
            Err(_) => return TransientSearchCacheWalk::Failed,
        };
        let file_type = match entry.file_type() {
            Ok(file_type) => file_type,
            Err(_) => return TransientSearchCacheWalk::Failed,
        };
        let path = entry.path();
        if file_type.is_file() {
            let metadata = match entry.metadata() {
                Ok(metadata) => metadata,
                Err(_) => return TransientSearchCacheWalk::Failed,
            };
            bytes = bytes.saturating_add(metadata.len());
        } else if file_type.is_dir() {
            if !boundary.should_descend(&path).unwrap_or(false) {
                return TransientSearchCacheWalk::Failed;
            }
            match transient_search_cache_dir_bytes(&path, boundary, deadline) {
                TransientSearchCacheWalk::Complete(nested) => {
                    bytes = bytes.saturating_add(nested);
                }
                other => return other,
            }
        } else {
            return TransientSearchCacheWalk::Failed;
        }
    }
    TransientSearchCacheWalk::Complete(bytes)
}

fn read_file_trigram_count_extension(
    base: &BasePostings,
    extension_start: u64,
    postings_body_end: u64,
    file_count: usize,
) -> std::io::Result<Option<Vec<u32>>> {
    if extension_start >= postings_body_end {
        return Ok(None);
    }
    let extension_len = postings_body_end - extension_start;
    if extension_len < 16 {
        return Ok(None);
    }
    let mut header = [0u8; 16];
    pread_exact(&base.file, extension_start, &mut header)?;
    if &header[..8] != FILE_TRIGRAM_COUNT_MAGIC {
        return Ok(None);
    }
    let version = u32::from_le_bytes([header[8], header[9], header[10], header[11]]);
    if version != INDEX_VERSION {
        return Err(std::io::Error::other("invalid file trigram count version"));
    }
    let count = u32::from_le_bytes([header[12], header[13], header[14], header[15]]) as usize;
    if count != file_count {
        return Err(std::io::Error::other("file trigram count length mismatch"));
    }
    let counts_len = count
        .checked_mul(4)
        .ok_or_else(|| std::io::Error::other("file trigram count extension too large"))?;
    if 16u64 + counts_len as u64 > extension_len {
        return Err(std::io::Error::other(
            "truncated file trigram count extension",
        ));
    }
    let mut bytes = vec![0u8; counts_len];
    pread_exact(&base.file, extension_start + 16, &mut bytes)?;
    let mut counts = Vec::with_capacity(count);
    for chunk in bytes.chunks_exact(4) {
        counts.push(u32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]));
    }
    Ok(Some(counts))
}

fn compute_file_trigram_counts_from_base(
    base: &BasePostings,
    file_count: usize,
    borrowed_load_budget: Option<&BorrowedIndexLoadBudget>,
) -> std::io::Result<Vec<u32>> {
    let mut counts = vec![0u32; file_count];
    for (entry_index, entry) in base.lookup.iter().copied().enumerate() {
        if borrowed_load_budget.is_some_and(|budget| !budget.checkpoint_at(entry_index)) {
            return Err(std::io::Error::other("borrowed index load stopped"));
        }
        for posting in base.read_postings(entry)? {
            let Some(count) = counts.get_mut(posting.file_id as usize) else {
                return Err(std::io::Error::other("posting references missing file"));
            };
            *count = count.saturating_add(1);
        }
    }
    Ok(counts)
}

fn ensure_count_slot(counts: &mut Vec<u32>, file_id: u32) {
    let len = file_id as usize + 1;
    if counts.len() < len {
        counts.resize(len, 0);
    }
}

fn reader_has_remaining<R: Seek>(
    reader: &mut R,
    absolute_end: u64,
    len: usize,
) -> std::io::Result<bool> {
    let position = reader.stream_position()?;
    Ok(position <= absolute_end && (len as u64) <= absolute_end - position)
}

fn crc32_file_range(
    path: &Path,
    start: u64,
    len: u64,
    keep_going: &dyn Fn() -> bool,
) -> std::io::Result<u32> {
    let mut file = File::open(path)?;
    file.seek(SeekFrom::Start(start))?;
    let mut hasher = crc32fast::Hasher::new();
    let mut remaining = len;
    let mut buffer = vec![0u8; 1024 * 1024];
    while remaining > 0 {
        check_search_build(keep_going)?;
        let read_len = buffer.len().min(remaining as usize);
        let bytes_read = file.read(&mut buffer[..read_len])?;
        if bytes_read == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "truncated cache while checksumming",
            ));
        }
        hasher.update(&buffer[..bytes_read]);
        remaining -= bytes_read as u64;
    }
    Ok(hasher.finalize())
}

fn open_cache_file_read(path: &Path) -> std::io::Result<File> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        const FILE_SHARE_READ: u32 = 0x0000_0001;
        const FILE_SHARE_WRITE: u32 = 0x0000_0002;
        const FILE_SHARE_DELETE: u32 = 0x0000_0004;
        options.share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE);
    }
    options.open(path)
}

#[cfg(unix)]
fn pread_exact(file: &File, mut offset: u64, mut buffer: &mut [u8]) -> std::io::Result<()> {
    use std::os::unix::fs::FileExt;
    while !buffer.is_empty() {
        let bytes_read = file.read_at(buffer, offset)?;
        if bytes_read == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "short pread from search cache",
            ));
        }
        offset += bytes_read as u64;
        let (_, rest) = buffer.split_at_mut(bytes_read);
        buffer = rest;
    }
    Ok(())
}

#[cfg(windows)]
fn pread_exact(file: &File, mut offset: u64, mut buffer: &mut [u8]) -> std::io::Result<()> {
    use std::os::windows::fs::FileExt;
    while !buffer.is_empty() {
        let bytes_read = file.seek_read(buffer, offset)?;
        if bytes_read == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "short pread from search cache",
            ));
        }
        offset += bytes_read as u64;
        let (_, rest) = buffer.split_at_mut(bytes_read);
        buffer = rest;
    }
    Ok(())
}

/// Insert a delta posting without disturbing the sorted-id invariant required by
/// posting-list intersection. Git diff order is independent of file ID order, so
/// re-sorting the entire list after every inversion scales poorly for shared trigrams.
fn insert_delta_posting(postings: &mut Vec<Posting>, posting: Posting) {
    if postings
        .last()
        .is_none_or(|last| last.file_id < posting.file_id)
    {
        postings.push(posting);
        return;
    }

    let insertion_index = postings.partition_point(|existing| existing.file_id < posting.file_id);
    debug_assert!(postings
        .get(insertion_index)
        .is_none_or(|existing| existing.file_id != posting.file_id));
    postings.insert(insertion_index, posting);
}

#[cfg(test)]
fn insert_delta_posting_full_sort_reference(postings: &mut Vec<Posting>, posting: Posting) {
    postings.push(posting);
    if postings.len() > 1
        && postings[postings.len() - 2].file_id > postings[postings.len() - 1].file_id
    {
        postings.sort_unstable_by_key(|posting| posting.file_id);
    }
}

fn intersect_sorted_ids(left: &[u32], right: &[u32]) -> Vec<u32> {
    let mut merged = Vec::with_capacity(left.len().min(right.len()));
    let mut left_index = 0;
    let mut right_index = 0;

    while left_index < left.len() && right_index < right.len() {
        if (left_index + right_index) % BORROWED_INDEX_CHECKPOINT_INTERVAL == 0
            && crate::executor::current_job_cancelled()
        {
            break;
        }
        match left[left_index].cmp(&right[right_index]) {
            std::cmp::Ordering::Less => left_index += 1,
            std::cmp::Ordering::Greater => right_index += 1,
            std::cmp::Ordering::Equal => {
                merged.push(left[left_index]);
                left_index += 1;
                right_index += 1;
            }
        }
    }

    merged
}

fn union_sorted_ids(left: &[u32], right: &[u32]) -> Vec<u32> {
    let mut merged = Vec::with_capacity(left.len() + right.len());
    let mut left_index = 0;
    let mut right_index = 0;

    while left_index < left.len() && right_index < right.len() {
        if (left_index + right_index) % BORROWED_INDEX_CHECKPOINT_INTERVAL == 0
            && crate::executor::current_job_cancelled()
        {
            break;
        }
        match left[left_index].cmp(&right[right_index]) {
            std::cmp::Ordering::Less => {
                merged.push(left[left_index]);
                left_index += 1;
            }
            std::cmp::Ordering::Greater => {
                merged.push(right[right_index]);
                right_index += 1;
            }
            std::cmp::Ordering::Equal => {
                merged.push(left[left_index]);
                left_index += 1;
                right_index += 1;
            }
        }
    }

    if !crate::executor::current_job_cancelled() {
        merged.extend_from_slice(&left[left_index..]);
        merged.extend_from_slice(&right[right_index..]);
    }
    merged
}

pub(crate) fn decompose_grep_pattern(pattern: &CompiledPattern) -> RegexQuery {
    let raw_pattern = pattern.raw_pattern_for_trigrams();
    match pattern {
        CompiledPattern::Regex {
            case_insensitive: true,
            ..
        } => {
            // RegexBuilder applies this flag outside the raw pattern. Parse the
            // same effective regex so Unicode folds such as `K` matching `K` do
            // not become false mandatory trigrams.
            decompose_regex(&format!("(?i:{raw_pattern})"))
        }
        _ => decompose_regex(&raw_pattern),
    }
}

pub fn decompose_regex(pattern: &str) -> RegexQuery {
    let hir = match regex_syntax::parse(pattern) {
        Ok(hir) => hir,
        Err(_) => return RegexQuery::default(),
    };

    let build = build_query(&hir);
    build.into_query()
}

pub fn pack_trigram(a: u8, b: u8, c: u8) -> u32 {
    ((a as u32) << 16) | ((b as u32) << 8) | c as u32
}

pub fn normalize_char(c: u8) -> u8 {
    c.to_ascii_lowercase()
}

fn scan_trigrams(content: &[u8], mut visit: impl FnMut(u32, u8, usize)) {
    if content.len() < 3 {
        return;
    }

    for start in 0..=content.len() - 3 {
        let trigram = pack_trigram(
            normalize_char(content[start]),
            normalize_char(content[start + 1]),
            normalize_char(content[start + 2]),
        );
        let next_char = content.get(start + 3).copied().unwrap_or(EOF_SENTINEL);
        visit(trigram, next_char, start);
    }
}

pub fn extract_trigrams(content: &[u8]) -> Vec<(u32, u8, usize)> {
    let mut trigrams = Vec::with_capacity(content.len().saturating_sub(2));
    scan_trigrams(content, |trigram, next_char, position| {
        trigrams.push((trigram, next_char, position));
    });
    trigrams
}

fn trigram_filter_map(content: &[u8], include_eof_next_char: bool) -> BTreeMap<u32, PostingFilter> {
    let mut filters: BTreeMap<u32, PostingFilter> = BTreeMap::new();
    scan_trigrams(content, |trigram, next_char, position| {
        let entry = filters.entry(trigram).or_default();
        if include_eof_next_char || next_char != EOF_SENTINEL {
            entry.next_mask |= mask_for_next_char(next_char);
        }
        entry.loc_mask |= mask_for_position(position);
    });
    filters
}

/// The posting fold this index stores for one file's bytes: each distinct
/// trigram in ascending order with its next-character mask (end of file
/// included) and its position mask, as `(trigram, next_mask, loc_mask)`.
/// Per-checkout trigram payloads are built from it so that their postings
/// match the index's exactly.
pub(crate) fn file_posting_fold(content: &[u8]) -> impl Iterator<Item = (u32, u8, u8)> {
    trigram_filter_map(content, true)
        .into_iter()
        .map(|(trigram, filter)| (trigram, filter.next_mask, filter.loc_mask))
}

pub fn query_trigrams_from_tokens(tokens: &[&str]) -> Vec<u32> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for token in tokens {
        scan_trigrams(token.as_bytes(), |trigram, _, _| {
            if seen.insert(trigram) {
                out.push(trigram);
            }
        });
    }
    out
}

pub fn lexical_score(index: &SearchIndex, query_trigrams: &[u32], file_id: u32) -> f32 {
    lexical_score_snapshot(&index.snapshot(), query_trigrams, file_id)
}

fn materialize_query_postings(
    index: &SearchIndexSnapshot,
    query_trigrams: &[u32],
) -> HashMap<u32, Vec<u32>> {
    let mut postings_by_trigram = HashMap::with_capacity(query_trigrams.len());
    for &trigram in query_trigrams {
        postings_by_trigram
            .entry(trigram)
            .or_insert_with(|| index.postings_for_trigram(trigram, None));
    }
    postings_by_trigram
}

fn lexical_score_snapshot(
    index: &SearchIndexSnapshot,
    query_trigrams: &[u32],
    file_id: u32,
) -> f32 {
    let postings_by_trigram = materialize_query_postings(index, query_trigrams);
    lexical_score_from_postings(index, query_trigrams, &postings_by_trigram, file_id)
}

fn lexical_score_from_postings(
    index: &SearchIndexSnapshot,
    query_trigrams: &[u32],
    postings_by_trigram: &HashMap<u32, Vec<u32>>,
    file_id: u32,
) -> f32 {
    #[cfg(test)]
    crate::search_hot_path_measurements::record(|counts| counts.score_evaluations += 1);
    if query_trigrams.is_empty() {
        return 0.0;
    }

    let mut hits = 0u32;
    for &trigram in query_trigrams {
        if postings_by_trigram
            .get(&trigram)
            .is_some_and(|postings| postings.binary_search(&file_id).is_ok())
        {
            hits += 1;
        }
    }

    if hits == 0 {
        return 0.0;
    }

    let file_trigram_count = index
        .file_trigram_count
        .get(file_id as usize)
        .copied()
        .unwrap_or(1)
        .max(1) as f32;
    (hits as f32) / (1.0 + file_trigram_count.ln())
}

#[cfg(test)]
fn lexical_rank_with_stats_reference(
    index: &SearchIndexSnapshot,
    query_trigrams: &[u32],
    candidate_filter: Option<&dyn Fn(&Path) -> bool>,
    max_files: usize,
) -> LexicalRankResult {
    if query_trigrams.is_empty() || max_files == 0 {
        return LexicalRankResult::default();
    }

    let mut non_zero: Vec<(u32, usize)> = query_trigrams
        .iter()
        .filter_map(|trigram| {
            let posting_count = index.posting_count(*trigram);
            (posting_count > 0).then_some((*trigram, posting_count))
        })
        .collect();
    if non_zero.is_empty() {
        return LexicalRankResult::default();
    }

    non_zero.sort_unstable_by_key(|(_, posting_count)| *posting_count);
    let selected_count = non_zero.len().min(3);
    let candidate_cap = if selected_count == 3 { 200 } else { 500 };

    let mut candidate_ids = BTreeSet::new();
    for (trigram, _) in non_zero.iter().take(selected_count) {
        candidate_ids.extend(index.postings_for_trigram(*trigram, None));
    }
    let pre_filter_candidate_count = candidate_ids.len();
    let engine_capped = pre_filter_candidate_count > candidate_cap;
    let filtered_candidates = candidate_ids
        .into_iter()
        .filter_map(|file_id| {
            index
                .files
                .get(file_id as usize)
                .map(|entry| (file_id, entry))
        })
        .filter(|(_, entry)| {
            candidate_filter
                .map(|filter| filter(&entry.path))
                .unwrap_or(true)
        })
        .collect::<Vec<_>>();

    let mut ranked = Vec::new();
    for (file_id, entry) in filtered_candidates.into_iter().take(candidate_cap) {
        let score = lexical_score_snapshot_reference(index, query_trigrams, file_id);
        if score > 0.0 {
            ranked.push((entry.path.clone(), score));
        }
    }

    ranked.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    ranked.truncate(max_files);
    LexicalRankResult {
        files: ranked,
        engine_capped,
    }
}

#[cfg(test)]
fn lexical_score_snapshot_reference(
    index: &SearchIndexSnapshot,
    query_trigrams: &[u32],
    file_id: u32,
) -> f32 {
    if query_trigrams.is_empty() {
        return 0.0;
    }

    let mut hits = 0u32;
    for &trigram in query_trigrams {
        let postings = index.postings_for_trigram(trigram, None);
        if postings.binary_search(&file_id).is_ok() {
            hits += 1;
        }
    }

    if hits == 0 {
        return 0.0;
    }

    let file_trigram_count = index
        .file_trigram_count
        .get(file_id as usize)
        .copied()
        .unwrap_or(1)
        .max(1) as f32;
    (hits as f32) / (1.0 + file_trigram_count.ln())
}

/// Resolve a cache directory using configure's registered key when available.
/// Persistent writers should pass their chosen key to `resolve_cache_dir_with_key`
/// directly; callers without a configured project fall back to a Git probe.
pub fn resolve_cache_dir(project_root: &Path, storage_dir: Option<&Path>) -> PathBuf {
    resolve_cache_dir_with_key(&artifact_cache_key_for_build(project_root), storage_dir)
}

pub(crate) fn build_path_filters(
    include: &[String],
    exclude: &[String],
) -> Result<PathFilters, String> {
    Ok(PathFilters {
        includes: build_globset(include)?,
        excludes: build_globset(exclude)?,
    })
}

pub(crate) fn walk_project_files(root: &Path, filters: &PathFilters) -> Vec<PathBuf> {
    walk_project_files_from_inner(root, root, filters, None, false)
        .expect("unbounded project walk cannot exceed a file limit")
}

fn walk_build_files_cancellable(
    root: &Path,
    filters: &PathFilters,
    keep_going: &dyn Fn() -> bool,
) -> Option<Vec<PathBuf>> {
    let mut paths = Vec::new();
    let mut walker = project_walk_builder(root).build();
    loop {
        if !keep_going() {
            return None;
        }
        let Some(entry) = walker.next() else {
            return Some(paths);
        };
        let Ok(entry) = entry else {
            continue;
        };
        if entry.file_type().is_some_and(|kind| kind.is_file())
            && filters.matches(root, entry.path())
        {
            paths.push(entry.into_path());
        }
    }
}

pub fn walk_project_files_bounded_default(
    root: &Path,
    max_files: usize,
) -> Result<Vec<PathBuf>, usize> {
    walk_project_files_from_inner(root, root, &PathFilters::default(), Some(max_files), true)
}

pub(crate) fn walk_project_files_bounded_matching<F>(
    root: &Path,
    filters: &PathFilters,
    max_files: usize,
    matches_file: F,
) -> Result<Vec<PathBuf>, usize>
where
    F: Fn(&Path) -> bool,
{
    walk_project_files_from_inner_matching(root, root, filters, Some(max_files), matches_file, true)
}

pub fn walk_project_files_bounded_default_matching<F>(
    root: &Path,
    max_files: usize,
    matches_file: F,
) -> Result<Vec<PathBuf>, usize>
where
    F: Fn(&Path) -> bool,
{
    walk_project_files_from_inner_matching(
        root,
        root,
        &PathFilters::default(),
        Some(max_files),
        matches_file,
        true,
    )
}

pub(crate) fn walk_project_files_from(
    filter_root: &Path,
    search_root: &Path,
    filters: &PathFilters,
) -> Vec<PathBuf> {
    walk_project_files_from_inner(filter_root, search_root, filters, None, true)
        .expect("unbounded project walk cannot exceed a file limit")
}

fn walk_project_files_from_inner(
    filter_root: &Path,
    search_root: &Path,
    filters: &PathFilters,
    max_files: Option<usize>,
    sort_by_mtime: bool,
) -> Result<Vec<PathBuf>, usize> {
    walk_project_files_from_inner_matching(
        filter_root,
        search_root,
        filters,
        max_files,
        |_| true,
        sort_by_mtime,
    )
}

pub(crate) fn project_walk_builder(search_root: &Path) -> WalkBuilder {
    project_walk_builder_with_admission(
        search_root,
        crate::project_content::is_excluded_with_file_type,
    )
}

fn project_walk_builder_with_admission(
    search_root: &Path,
    excludes: fn(&Path, Option<&std::fs::FileType>) -> bool,
) -> WalkBuilder {
    crate::views::io::WORK
        .full_walks
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    project_walk_builder_pruned(search_root, excludes, |_| true)
}

/// The project walk with one more condition, `descend`, on every entry below
/// the root. Every decision the walk makes about an entry it does visit is
/// unchanged: the ignore rules, the excluded directories and the content
/// exclusions are the same builder settings and the same entry predicate.
/// Only the set of entries visited shrinks.
fn project_walk_builder_pruned(
    search_root: &Path,
    excludes: fn(&Path, Option<&std::fs::FileType>) -> bool,
    descend: impl Fn(&Path) -> bool + Send + Sync + 'static,
) -> WalkBuilder {
    let mut builder = WalkBuilder::new(search_root);
    // A disappearing child mount can make ReadDir::drop panic on ENXIO and abort
    // the daemon, so never open directories outside this walk root's filesystem.
    builder.same_file_system(true).hidden(false);
    crate::context::apply_project_ignore_rules(&mut builder, search_root).filter_entry(
        move |entry| project_walk_admits_entry(entry, excludes) && descend(entry.path()),
    );
    builder
}

/// The membership rule the project walk applies to each entry on top of the
/// ignore rules: content exclusions, skipped directory names and OS metadata
/// files.
fn project_walk_admits_entry(
    entry: &ignore::DirEntry,
    excludes: fn(&Path, Option<&std::fs::FileType>) -> bool,
) -> bool {
    let file_type = entry.file_type();
    if excludes(entry.path(), file_type.as_ref()) {
        return false;
    }
    let name = entry.file_name().to_string_lossy();
    if file_type.as_ref().is_some_and(std::fs::FileType::is_dir) {
        return !matches!(
            name.as_ref(),
            "node_modules"
                | "target"
                | "venv"
                | ".venv"
                | ".git"
                | "__pycache__"
                | ".tox"
                | "dist"
                | "build"
        );
    }
    !crate::os_metadata::is_os_metadata_file_name(entry.file_name())
}

/// What `project_walk_lists` found: the files the walk lists, and the places
/// it could not look.
#[derive(Debug, Default)]
pub(crate) struct ProjectWalkListing {
    pub listed: BTreeSet<PathBuf>,
    /// Paths the walk could not read (a directory it could not list, an
    /// ignore file it could not read), with the error. Whether a file at or
    /// under one of them is listed is unknown, not "no".
    pub gaps: Vec<(PathBuf, String)>,
}

impl ProjectWalkListing {
    /// The gap that leaves `file`'s listing unknown, if any.
    pub(crate) fn gap_for(&self, file: &Path) -> Option<&(PathBuf, String)> {
        self.gaps.iter().find(|(gap, _)| file.starts_with(gap))
    }
}

/// The path an `ignore` walk error is about; `None` when it names none.
fn walk_error_path(error: &ignore::Error) -> Option<&Path> {
    match error {
        ignore::Error::WithPath { path, .. } => Some(path),
        ignore::Error::WithDepth { err, .. } | ignore::Error::WithLineNumber { err, .. } => {
            walk_error_path(err)
        }
        ignore::Error::Loop { child, .. } => Some(child),
        ignore::Error::Partial(errors) => errors.iter().find_map(walk_error_path),
        _ => None,
    }
}

/// Which of `files` (absolute paths under `root`, joined onto `root` as given)
/// the project walk lists, without walking the rest of the project. It runs
/// the same walk as `walk_project_files(root, &PathFilters::default())`,
/// pruned to the directories that lead to `files`, so each answer is the
/// walk's own decision: built-in skipped directories, content exclusions and
/// every `.gitignore`/`.aftignore` from the root down to the file.
///
/// Unlike `walk_project_files`, which drops what it cannot read, this records
/// walk errors as gaps: when a directory or ignore file on the way could not
/// be read, the walk cannot tell whether a file beneath it would be listed.
pub(crate) fn project_walk_lists(root: &Path, files: &BTreeSet<PathBuf>) -> ProjectWalkListing {
    let mut listing = ProjectWalkListing::default();
    if files.is_empty() {
        return listing;
    }
    let leads_to_a_file = files
        .iter()
        .flat_map(|file| {
            file.ancestors()
                .take_while(|ancestor| ancestor.starts_with(root))
                .map(Path::to_path_buf)
        })
        .collect::<HashSet<_>>();
    let walk = project_walk_builder_pruned(
        root,
        crate::project_content::is_excluded_with_file_type,
        move |path| leads_to_a_file.contains(path),
    )
    .build();
    for item in walk {
        let entry = match item {
            Ok(entry) => entry,
            Err(error) => {
                let path = walk_error_path(&error).unwrap_or(root).to_path_buf();
                listing.gaps.push((path, error.to_string()));
                continue;
            }
        };
        // An ignore file below this entry could not be read, so the rules
        // for everything under it are unknown.
        if let Some(error) = entry.error() {
            listing
                .gaps
                .push((entry.path().to_path_buf(), error.to_string()));
        }
        if entry.file_type().is_some_and(|kind| kind.is_file()) && files.contains(entry.path()) {
            listing.listed.insert(entry.into_path());
        }
    }
    listing
}

/// `project_walk_lists` answers per file with the project walk's own rules,
/// so it must agree with `walk_project_files` on every file of a tree.
#[cfg(test)]
mod project_walk_lists_tests {
    use super::*;

    fn every_file(dir: &Path, files: &mut BTreeSet<PathBuf>) {
        for entry in fs::read_dir(dir).unwrap() {
            let entry = entry.unwrap();
            let kind = entry.file_type().unwrap();
            if kind.is_dir() {
                if entry.file_name() != ".git" {
                    every_file(&entry.path(), files);
                }
            } else if kind.is_file() {
                files.insert(entry.path());
            }
        }
    }

    /// A directory the walk cannot list is recorded as a gap naming it, so
    /// files beneath it are unknown rather than classified as excluded.
    #[cfg(unix)]
    #[test]
    fn project_walk_lists_reports_an_unlistable_directory_as_a_gap() {
        use std::os::unix::fs::PermissionsExt as _;
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let locked = root.join("locked");
        fs::create_dir(&locked).unwrap();
        fs::write(locked.join("inner.py"), "x = 1\n").unwrap();
        fs::write(root.join("open.py"), "x = 1\n").unwrap();
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o311)).unwrap();
        let listable = fs::read_dir(&locked).is_ok();
        let files = BTreeSet::from([locked.join("inner.py"), root.join("open.py")]);
        let listing = project_walk_lists(&root, &files);
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).unwrap();
        if listable {
            eprintln!("skipped: this process lists directories without read permission (root)");
            return;
        }
        assert_eq!(listing.listed, BTreeSet::from([root.join("open.py")]));
        let (gap, _) = listing
            .gap_for(&locked.join("inner.py"))
            .expect("unlistable directory reported as a gap");
        assert_eq!(gap, &locked);
        assert!(listing.gap_for(&root.join("open.py")).is_none());
    }

    #[test]
    fn project_walk_lists_agrees_with_the_walk_on_every_file() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        for git in [false, true] {
            let root = root.join(if git { "git" } else { "plain" });
            let files = [
                (".gitignore", "ignored.py\n*.log\n/self-ignored/\n"),
                (".aftignore", "aftignored.py\n/.aftignore\n"),
                ("src/kept.py", "x = 1\n"),
                ("src/trace.log", "log\n"),
                ("ignored.py", "x = 1\n"),
                ("aftignored.py", "x = 1\n"),
                ("self-ignored/inner.py", "x = 1\n"),
                ("dist/bundle.js", "var x;\n"),
                ("build/out.js", "var x;\n"),
                ("node_modules/pkg/index.js", "var x;\n"),
                ("target/debug/gen.rs", "fn f() {}\n"),
                ("pkg/__pycache__/m.py", "x = 1\n"),
                ("pkg/.gitignore", "generated.py\n!keep.log\n"),
                ("pkg/generated.py", "x = 1\n"),
                ("pkg/keep.log", "log\n"),
                ("pkg/kept.py", "x = 1\n"),
                ("pkg/deep/generated.py", "x = 1\n"),
                ("pkg/deep/.aftignore", "local.py\n"),
                ("pkg/deep/local.py", "x = 1\n"),
                ("pkg/deep/other.py", "x = 1\n"),
                ("fixtures/data.db", "SQLite format 3\0"),
                (".DS_Store", ""),
            ];
            for (relative, content) in files {
                let path = root.join(relative);
                fs::create_dir_all(path.parent().unwrap()).unwrap();
                fs::write(path, content).unwrap();
            }
            if git {
                fs::create_dir_all(root.join(".git")).unwrap();
            }
            let walked = walk_project_files(&root, &PathFilters::default())
                .into_iter()
                .collect::<BTreeSet<_>>();
            let mut all = BTreeSet::new();
            every_file(&root, &mut all);
            assert_eq!(all.len(), files.len(), "fixture files: {all:?}");
            // The fixture exercises both answers.
            assert!(walked.contains(&root.join("pkg/deep/other.py")));
            assert!(!walked.contains(&root.join("pkg/deep/local.py")));
            for file in &all {
                let listed = project_walk_lists(&root, &BTreeSet::from([file.clone()])).listed;
                assert_eq!(
                    listed.contains(file),
                    walked.contains(file),
                    "git={git} {}",
                    file.display()
                );
            }
            let listing = project_walk_lists(&root, &all);
            assert!(listing.gaps.is_empty(), "git={git} {:?}", listing.gaps);
            assert_eq!(listing.listed, walked, "git={git}");
        }
    }
}

#[cfg(all(test, unix))]
mod membership_admission_tests {
    use super::*;
    use crate::project_content::{is_excluded_with_file_type, legacy_walker_excludes};

    fn inventory(root: &Path, legacy: bool) -> (Vec<PathBuf>, usize) {
        let mut paths = Vec::new();
        let counts = crate::db::file_identity::stat_probe::count_candidates(|| {
            let builder = project_walk_builder_with_admission(
                root,
                if legacy {
                    legacy_walker_excludes
                } else {
                    is_excluded_with_file_type
                },
            );
            for entry in builder.build() {
                paths.push(entry.unwrap().into_path());
            }
        });
        paths.sort();
        let stats = counts
            .iter()
            .filter(|(path, _)| path.starts_with(root))
            .map(|(_, count)| count)
            .sum();
        (paths, stats)
    }

    #[test]
    fn membership_admission_identity_matches_legacy() {
        use std::os::unix::{fs::symlink, net::UnixListener};
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        for dir in [".git", "models.db", "vendor/nested/.git", "state"] {
            fs::create_dir_all(root.join(dir)).unwrap();
        }
        fs::write(root.join(".gitignore"), "ignored.rs\n*.log\n").unwrap();
        fs::write(root.join(".aftignore"), "aftignored.rs\n").unwrap();
        for name in [
            "source.rs",
            "untracked.rs",
            "ignored.rs",
            "aftignored.rs",
            "live.db",
            "models.db/source.rs",
            "vendor/nested/kept.log",
            "vendor/nested/ignored.rs",
            "state/generated.rs",
        ] {
            fs::write(root.join(name), "fn source() {}\n").unwrap();
        }
        fs::write(root.join("vendor/nested/.gitignore"), "ignored.rs\n").unwrap();
        crate::project_content::register_storage_root(&root.join("state"));
        symlink("state/generated.rs", root.join("storage-alias.rs")).unwrap();
        symlink("source.rs", root.join("source-alias.rs")).unwrap();
        symlink("absent", root.join("dangling.rs")).unwrap();
        let database = crate::db::file_identity::IdentityConnection::open(
            root.join("open.cache"),
            "membership-parity",
        )
        .unwrap();
        database
            .execute_batch("PRAGMA journal_mode=WAL; CREATE TABLE probe(value TEXT);")
            .unwrap();
        fs::hard_link(root.join("open.cache"), root.join("hard-alias.rs")).unwrap();
        fs::hard_link(root.join("open.cache-shm"), root.join("shm-alias.rs")).unwrap();
        symlink("hard-alias.rs", root.join("sqlite-alias.rs")).unwrap();
        let fifo =
            std::ffi::CString::new(root.join("pipe.rs").as_os_str().as_encoded_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
        let _socket = UnixListener::bind(root.join("socket.rs")).unwrap();

        let (old, old_stats) = inventory(&root, true);
        let (new, new_stats) = inventory(&root, false);
        assert_eq!(
            new, old,
            "admission, including symlink identity, must match"
        );
        assert!(!new.contains(&root.join("storage-alias.rs")));
        assert!(!new.contains(&root.join("sqlite-alias.rs")));
        let members = new
            .iter()
            .filter(|path| fs::symlink_metadata(path).unwrap().is_file())
            .map(|path| path.strip_prefix(&root).unwrap().to_path_buf())
            .collect::<Vec<_>>();
        assert_eq!(
            members,
            [
                ".aftignore",
                ".gitignore",
                "models.db/source.rs",
                "source.rs",
                "untracked.rs",
                "vendor/nested/.gitignore",
                "vendor/nested/kept.log"
            ]
            .map(PathBuf::from)
        );
        println!("mixed membership fixture: legacy={old_stats} lstat, optimized={new_stats} lstat");
        assert_eq!(
            (old_stats, new_stats),
            (22, 15),
            "only redundant lstat calls should disappear"
        );
    }

    /// The membership walker's stat budget, asserted exactly on a private tree
    /// that nothing else touches between its walks.
    #[test]
    fn membership_walker_stat_counts() {
        use std::os::unix::fs::symlink;
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        for dir in [".git", "src/nested", "target"] {
            fs::create_dir_all(root.join(dir)).unwrap();
        }
        fs::write(root.join(".gitignore"), "ignored.rs\n").unwrap();
        for name in [
            "lib.rs",
            "ignored.rs",
            "src/main.rs",
            "src/nested/deep.rs",
            "target/built.rs",
        ] {
            fs::write(root.join(name), "fn source() {}\n").unwrap();
        }
        symlink("lib.rs", root.join("lib-alias.rs")).unwrap();

        let (old, old_stats) = inventory(&root, true);
        let (new, new_stats) = inventory(&root, false);
        assert_eq!(new, old, "admission must match the legacy walker");
        assert!(
            new_stats > 0 && new_stats <= old_stats,
            "legacy={old_stats} optimized={new_stats}"
        );
        let expected = [".gitignore", "lib.rs", "src/main.rs", "src/nested/deep.rs"]
            .map(|name| root.join(name))
            .to_vec();
        use crate::views::first_load::{
            ConfiguredMembershipWalker, MembershipWalker, MEMBERSHIP_WALKS,
        };
        MEMBERSHIP_WALKS.with(|walks| walks.set(0));
        let counts = crate::db::file_identity::stat_probe::count_candidates(|| {
            for _ in 0..3 {
                let mut files = ConfiguredMembershipWalker.files(&root).unwrap();
                files.sort();
                assert_eq!(files, expected);
            }
        });
        let walks = MEMBERSHIP_WALKS.with(|walks| walks.get());
        assert_eq!(walks, 3, "each membership request walks once");
        let stats = counts
            .iter()
            .filter(|(path, _)| path.starts_with(&root))
            .map(|(_, count)| count)
            .sum::<usize>();
        assert_eq!(
            stats,
            new_stats * walks,
            "each walk spends exactly the optimized admission's stat calls"
        );
    }

    /// Measures the stat calls of the real working checkout. It asserts
    /// nothing exact: an editor or another job may change files between its
    /// walks. Run with `--ignored --nocapture` to read the counts.
    #[test]
    #[ignore = "benchmark of the live checkout; prints counts only"]
    fn membership_checkout_stat_counts_benchmark() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .parent()
            .unwrap();
        let (old, old_stats) = inventory(root, true);
        let (new, new_stats) = inventory(root, false);
        println!(
            "checkout membership: legacy {} entries / {old_stats} lstat, optimized {} entries / {new_stats} lstat, saved={}",
            old.len(),
            new.len(),
            old_stats.saturating_sub(new_stats)
        );
        use crate::views::first_load::{
            ConfiguredMembershipWalker, MembershipWalker, MEMBERSHIP_WALKS,
        };
        MEMBERSHIP_WALKS.with(|walks| walks.set(0));
        let counts = crate::db::file_identity::stat_probe::count_candidates(|| {
            for _ in 0..3 {
                let _ = ConfiguredMembershipWalker.files(root);
            }
        });
        let walks = MEMBERSHIP_WALKS.with(|walks| walks.get());
        let stats = counts
            .iter()
            .filter(|(path, _)| path.starts_with(root))
            .map(|(_, count)| count)
            .sum::<usize>();
        println!("three checkout membership requests: {walks} full walks, {stats} candidate identity lstat calls");
    }
}

fn walk_project_files_from_inner_matching<F>(
    filter_root: &Path,
    search_root: &Path,
    filters: &PathFilters,
    max_files: Option<usize>,
    matches_file: F,
    sort_by_mtime: bool,
) -> Result<Vec<PathBuf>, usize>
where
    F: Fn(&Path) -> bool,
{
    let builder = project_walk_builder(search_root);

    let mut files = Vec::new();
    for entry in builder.build().filter_map(|entry| entry.ok()) {
        if !entry
            .file_type()
            .map_or(false, |file_type| file_type.is_file())
        {
            continue;
        }
        let path = entry.into_path();
        if filters.matches(filter_root, &path) && matches_file(&path) {
            files.push(path);
            if max_files.is_some_and(|limit| files.len() > limit) {
                return Err(files.len());
            }
        }
    }

    if sort_by_mtime {
        sort_paths_by_mtime_desc(&mut files, filter_root);
    }
    Ok(files)
}

pub(crate) fn read_searchable_text(path: &Path) -> Option<String> {
    let bytes = crate::bounded_io::read(path, None).ok()?;
    if is_binary_bytes(&bytes) {
        return None;
    }
    String::from_utf8(bytes).ok()
}

/// Read an indexed file for grep verification. Keep the original I/O error so
/// callers distinguish a missing entry from blocked access or a scan timeout.
fn read_indexed_file_bytes(path: &Path, deadline: Option<Instant>) -> std::io::Result<Vec<u8>> {
    crate::project_content::read_bounded(path, deadline, false)
}

/// Whether an I/O error says the path does not exist (including a path whose
/// parent is now a regular file). Permission and other errors do not count:
/// a path that could not be checked is never reported as missing.
fn io_error_means_missing_on_disk(error: &std::io::Error) -> bool {
    matches!(
        error.kind(),
        std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
    )
}

/// Whether `path` is known to be absent from disk. One `stat`; an unreadable
/// path counts as present.
pub(crate) fn path_missing_on_disk(path: &Path) -> bool {
    #[cfg(test)]
    audit_record(|work| work.presence_checks += 1);
    crate::bounded_io::metadata(path).is_err_and(|error| io_error_means_missing_on_disk(&error))
}

/// Extra paths one listing may `stat` beyond its page while replacing entries
/// that turned out to be missing on disk. Keeps the check bounded even when an
/// index is badly out of date.
pub(crate) const MISSING_ON_DISK_REFILL_BUDGET: usize = 256;

/// Result of checking a ranked list against the disk, see
/// [`take_present_on_disk`].
#[derive(Debug)]
pub(crate) struct PresentOnDisk<T> {
    /// Up to `limit` items whose path exists, in their original order.
    pub present: Vec<T>,
    /// Items dropped because their path is not on disk.
    pub dropped: usize,
    /// Items after the last one examined. They were neither checked nor
    /// returned: either the page was full or the `stat` budget ran out.
    pub unexamined: usize,
    /// True when the `stat` budget ran out before the page was full, so
    /// `unexamined` items may hold present paths that were not returned.
    pub budget_exhausted: bool,
}

/// Backstop against a stale index: keep the first `limit` items whose path
/// is on disk, dropping the ones that are not. Only items that would be
/// returned are checked (one `stat` each, never a walk), plus at most
/// [`MISSING_ON_DISK_REFILL_BUDGET`] replacements. Order among present items
/// is unchanged, so ranking is unaffected. Drops are logged at debug level
/// with the reason; callers report `dropped` in their result.
pub(crate) fn take_present_on_disk<T>(
    items: impl IntoIterator<Item = T>,
    limit: usize,
    path_of: impl Fn(&T) -> &Path,
    surface: &str,
) -> PresentOnDisk<T> {
    let stat_budget = limit.saturating_add(MISSING_ON_DISK_REFILL_BUDGET);
    let mut present = Vec::new();
    let mut dropped = 0usize;
    let mut unexamined = 0usize;
    let mut stats = 0usize;
    let mut budget_exhausted = false;
    let mut dropped_examples = Vec::new();
    for item in items {
        if present.len() >= limit {
            unexamined += 1;
            continue;
        }
        if stats >= stat_budget {
            budget_exhausted = true;
            unexamined += 1;
            continue;
        }
        stats += 1;
        if path_missing_on_disk(path_of(&item)) {
            dropped += 1;
            if dropped_examples.len() < 3 {
                dropped_examples.push(path_of(&item).display().to_string());
            }
        } else {
            present.push(item);
        }
    }
    if dropped > 0 {
        crate::slog_debug!(
            "{surface}: dropped {dropped} indexed path(s) that are not on disk in this checkout (stale index entry), e.g. {}",
            dropped_examples.join(", ")
        );
    }
    PresentOnDisk {
        present,
        dropped,
        unexamined,
        budget_exhausted,
    }
}

pub(crate) fn relative_to_root(root: &Path, path: &Path) -> PathBuf {
    path.strip_prefix(root)
        .map(PathBuf::from)
        .unwrap_or_else(|_| path.to_path_buf())
}

pub(crate) fn cache_relative_path(root: &Path, path: &Path) -> Option<PathBuf> {
    let normalized_root = normalize_path(root);
    let normalized_path = normalize_path(path);
    let relative = normalized_path.strip_prefix(&normalized_root).ok()?;
    validate_cached_relative_path(relative)
}

/// The relative path a disk cache stores for `path` under `root`: taken as
/// spelled, or else after resolving `path` on disk (an alias spelling such as
/// macOS `/var` for `/private/var`). `None` for a path outside `root`.
/// Admission into an index and the cache writer share this one rule, so an
/// admitted path can always be written.
pub(crate) fn storable_relative_path(root: &Path, path: &Path) -> Option<PathBuf> {
    cache_relative_path(root, path).or_else(|| {
        fs::canonicalize(path)
            .ok()
            .and_then(|canonical| cache_relative_path(root, &canonical))
    })
}

/// Canonical parent directories recur throughout one artifact reload. Cache only
/// that reload's results: a corpus commonly has many files per directory, while
/// retaining results across loads would need filesystem-change invalidation.
#[derive(Default)]
pub(crate) struct ParentCanonicalizationMemo {
    canonical_parents: HashMap<PathBuf, Option<PathBuf>>,
}

#[cfg(test)]
#[derive(Debug, Default)]
struct CachedPathCanonicalizationWork {
    canonicalize_calls: usize,
    parent_lookups: usize,
    distinct_parents: HashSet<PathBuf>,
}

#[cfg(test)]
thread_local! {
    // Reload decoders are serial; isolate their counters from concurrent tests.
    static CACHED_PATH_CANONICALIZATION_WORK: std::cell::RefCell<Option<CachedPathCanonicalizationWork>> = const { std::cell::RefCell::new(None) };
}

impl ParentCanonicalizationMemo {
    fn canonicalize_parent_with<F>(
        &mut self,
        parent: &Path,
        canonicalize: &mut F,
    ) -> Option<PathBuf>
    where
        F: FnMut(&Path) -> std::io::Result<PathBuf>,
    {
        #[cfg(test)]
        CACHED_PATH_CANONICALIZATION_WORK.with(|slot| {
            if let Some(work) = slot.borrow_mut().as_mut() {
                work.parent_lookups += 1;
                work.distinct_parents.insert(parent.to_path_buf());
            }
        });
        self.canonical_parents
            .entry(parent.to_path_buf())
            .or_insert_with(|| canonicalize(parent).ok())
            .clone()
    }
}

#[cfg(test)]
fn cached_path_under_root(root: &Path, relative_path: &Path) -> Option<PathBuf> {
    let mut canonical_parents = ParentCanonicalizationMemo::default();
    cached_path_under_root_with_memo(root, relative_path, &mut canonical_parents)
}

pub(crate) fn cached_path_under_root_with_memo(
    root: &Path,
    relative_path: &Path,
    canonical_parents: &mut ParentCanonicalizationMemo,
) -> Option<PathBuf> {
    let mut canonicalize = |path: &Path| {
        #[cfg(test)]
        CACHED_PATH_CANONICALIZATION_WORK.with(|slot| {
            if let Some(work) = slot.borrow_mut().as_mut() {
                work.canonicalize_calls += 1;
            }
        });
        fs::canonicalize(path)
    };
    cached_path_under_root_with_memo_and_canonicalizer(
        root,
        relative_path,
        canonical_parents,
        &mut canonicalize,
    )
}

fn cached_path_under_root_with_memo_and_canonicalizer<F>(
    root: &Path,
    relative_path: &Path,
    canonical_parents: &mut ParentCanonicalizationMemo,
    canonicalize: &mut F,
) -> Option<PathBuf>
where
    F: FnMut(&Path) -> std::io::Result<PathBuf>,
{
    let relative = validate_cached_relative_path(relative_path)?;
    let normalized_root = normalize_path(root);
    let full_path = normalize_path(&normalized_root.join(relative));

    match fs::symlink_metadata(&full_path) {
        Ok(metadata) if !metadata.file_type().is_symlink() => {
            let parent = full_path.parent()?;
            let file_name = full_path.file_name()?;
            let canonical_path = canonical_parents
                .canonicalize_parent_with(parent, canonicalize)?
                .join(file_name);
            cached_path_if_contained(
                &normalized_root,
                &full_path,
                &canonical_path,
                canonical_parents,
                canonicalize,
            )
        }
        Ok(_) => {
            let canonical_path = canonicalize(&full_path).ok()?;
            cached_path_if_contained(
                &normalized_root,
                &full_path,
                &canonical_path,
                canonical_parents,
                canonicalize,
            )
        }
        Err(_) => {
            // If canonicalization fails, such as when an entry was deleted,
            // preserve the normalized lexical path as the fallback. Still
            // canonicalize its parent to seed the per-load memo so later sibling
            // entries do not repeat the realpath call for that directory.
            if let Some(parent) = full_path.parent() {
                let _ = canonical_parents.canonicalize_parent_with(parent, canonicalize);
            }
            is_within_search_root(&normalized_root, &full_path).then_some(full_path)
        }
    }
}

fn cached_path_if_contained<F>(
    normalized_root: &Path,
    full_path: &Path,
    canonical_path: &Path,
    canonical_parents: &mut ParentCanonicalizationMemo,
    canonicalize: &mut F,
) -> Option<PathBuf>
where
    F: FnMut(&Path) -> std::io::Result<PathBuf>,
{
    // Normalize only the containment operands. The returned path remains in the
    // cache's established lexical form because path_to_id and semantic-cache
    // consumers use that exact key. The memo avoids canonicalization work only;
    // every record still takes this containment check.
    if is_within_search_root(normalized_root, canonical_path) {
        return Some(full_path.to_path_buf());
    }

    let canonical_root =
        canonical_parents.canonicalize_parent_with(normalized_root, canonicalize)?;
    is_within_search_root(&canonical_root, canonical_path).then_some(full_path.to_path_buf())
}

pub(crate) fn validate_cached_relative_path(path: &Path) -> Option<PathBuf> {
    if path.is_absolute() {
        return None;
    }

    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Normal(part) => normalized.push(part),
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => return None,
        }
    }
    (!normalized.as_os_str().is_empty()).then_some(normalized)
}

/// Sort paths newest-first by mtime, falling back to normalized display-path order.
///
/// The stable root keeps equal-mtime ordering independent of an absolute project
/// directory, so equivalent projects produce the same relative-path order. Root
/// and candidate copies are canonicalized only for key construction so Windows
/// verbatim and clean forms compare alike without changing the returned paths.
/// Metadata and display keys are snapshotted before sorting: the comparator must
/// remain a total order even if files change or disappear during the sort.
pub(crate) fn sort_paths_by_mtime_desc(paths: &mut [PathBuf], stable_root: &Path) {
    sort_paths_by_mtime_desc_with_key_normalization(paths, stable_root, true, None);
}

/// Sort paths emitted by one filesystem walk without resolving every path again.
///
/// Walk entries already share one resolved root and do not traverse symlinks, so
/// lexical normalization produces the same display tie-break key. Calling
/// `canonicalize` for every match only repeats filesystem lookups the walk just
/// performed.
pub(crate) fn sort_walked_paths_by_mtime_desc(paths: &mut [PathBuf], stable_root: &Path) {
    sort_paths_by_mtime_desc_with_key_normalization(paths, stable_root, false, None);
}

/// Indexed paths already have canonical keys and mtimes. Unindexed roots still
/// use the disk-backed ordering, so mixed-root glob retains the same contract.
pub(crate) fn sort_paths_by_cached_mtime_desc(
    paths: &mut [PathBuf],
    stable_root: &Path,
    mtimes: &HashMap<PathBuf, SystemTime>,
) {
    sort_paths_by_mtime_desc_with_key_normalization(paths, stable_root, true, Some(mtimes));
}

fn sort_paths_by_mtime_desc_with_key_normalization(
    paths: &mut [PathBuf],
    stable_root: &Path,
    canonicalize_paths: bool,
    cached_mtimes: Option<&HashMap<PathBuf, SystemTime>>,
) {
    use std::collections::HashMap;
    let stable_root = crate::inspect::job::canonicalize_normalized(stable_root);
    let mut mtimes: HashMap<PathBuf, Option<SystemTime>> = HashMap::with_capacity(paths.len());
    let mut display_paths: HashMap<PathBuf, String> = HashMap::with_capacity(paths.len());
    for path in paths.iter() {
        mtimes.entry(path.clone()).or_insert_with(|| {
            cached_mtimes
                .and_then(|times| times.get(path))
                .copied()
                .or_else(|| path_modified_time(path))
        });
        display_paths.entry(path.clone()).or_insert_with(|| {
            let resolved = if path.is_absolute() {
                path.clone()
            } else {
                stable_root.join(path)
            };
            let comparison_path = if canonicalize_paths
                && !cached_mtimes.is_some_and(|times| times.contains_key(path))
            {
                #[cfg(test)]
                audit_record(|work| work.sort_canonicalizes += 1);
                crate::inspect::job::canonicalize_normalized(&resolved)
            } else {
                crate::inspect::job::normalize_path(&resolved)
            };
            normalized_display_sort_key(Some(&stable_root), &comparison_path)
        });
    }
    paths.sort_by(|left, right| {
        let left_mtime = mtimes.get(left).and_then(|v| *v);
        let right_mtime = mtimes.get(right).and_then(|v| *v);
        let left_display = display_paths
            .get(left)
            .map(String::as_bytes)
            .unwrap_or_default();
        let right_display = display_paths
            .get(right)
            .map(String::as_bytes)
            .unwrap_or_default();
        right_mtime
            .cmp(&left_mtime)
            .then_with(|| left_display.cmp(right_display))
            .then_with(|| left.cmp(right))
    });
}

/// See `sort_paths_by_mtime_desc` for why mtimes are snapshotted ahead of
/// the sort. Same fix, applied to grep matches that share files.
pub(crate) fn sort_grep_matches_by_mtime_desc(matches: &mut [GrepMatch], project_root: &Path) {
    use std::collections::HashMap;
    let mut mtimes: HashMap<PathBuf, Option<SystemTime>> = HashMap::new();
    let mut display_paths: HashMap<PathBuf, String> = HashMap::with_capacity(matches.len());
    for m in matches.iter() {
        mtimes.entry(m.file.clone()).or_insert_with(|| {
            let resolved = resolve_match_path(project_root, &m.file);
            path_modified_time(&resolved)
        });
        display_paths
            .entry(m.file.clone())
            .or_insert_with(|| normalized_display_sort_key(Some(project_root), &m.file));
    }
    matches.sort_by(|left, right| {
        let left_mtime = mtimes.get(&left.file).and_then(|v| *v);
        let right_mtime = mtimes.get(&right.file).and_then(|v| *v);
        let left_display = display_paths
            .get(&left.file)
            .map(String::as_bytes)
            .unwrap_or_default();
        let right_display = display_paths
            .get(&right.file)
            .map(String::as_bytes)
            .unwrap_or_default();
        // The display-path tiebreak makes complete result sets deterministic.
        // If a parallel grep stops early after hitting a cap, the capped subset
        // can still depend on which worker reaches the cap first.
        right_mtime
            .cmp(&left_mtime)
            .then_with(|| left_display.cmp(right_display))
            .then_with(|| left.line.cmp(&right.line))
            .then_with(|| left.column.cmp(&right.column))
    });
}

/// See `sort_paths_by_mtime_desc` for why mtimes are snapshotted ahead of
/// the sort. The cached lookup function `modified_for_path` is fast (in-memory
/// table from the search index), but it can still return different values if
/// the file is modified mid-sort. Snapshot once.
fn sort_shared_grep_matches_by_cached_mtime_desc<F>(
    matches: &mut [SharedGrepMatch],
    project_root: &Path,
    modified_for_path: F,
) where
    F: Fn(&Path) -> Option<SystemTime>,
{
    use std::collections::HashMap;
    let mut mtimes: HashMap<PathBuf, Option<SystemTime>> = HashMap::with_capacity(matches.len());
    let mut display_paths: HashMap<PathBuf, String> = HashMap::with_capacity(matches.len());
    for m in matches.iter() {
        let path = m.file.as_path().to_path_buf();
        mtimes
            .entry(path.clone())
            .or_insert_with(|| modified_for_path(&path));
        display_paths
            .entry(path.clone())
            .or_insert_with(|| normalized_display_sort_key(Some(project_root), &path));
    }
    matches.sort_by(|left, right| {
        let left_mtime = mtimes.get(left.file.as_path()).and_then(|v| *v);
        let right_mtime = mtimes.get(right.file.as_path()).and_then(|v| *v);
        let left_display = display_paths
            .get(left.file.as_path())
            .map(String::as_bytes)
            .unwrap_or_default();
        let right_display = display_paths
            .get(right.file.as_path())
            .map(String::as_bytes)
            .unwrap_or_default();
        // The display-path tiebreak makes complete result sets deterministic.
        // If a parallel grep stops early after hitting a cap, the capped subset
        // can still depend on which worker reaches the cap first.
        right_mtime
            .cmp(&left_mtime)
            .then_with(|| left_display.cmp(right_display))
            .then_with(|| left.line.cmp(&right.line))
            .then_with(|| left.column.cmp(&right.column))
    });
}

pub(crate) fn resolve_search_scope(project_root: &Path, path: Option<&str>) -> SearchScope {
    // Keep the returned scope path in the historical canonical/lexical form.
    // Only `is_within_search_root` normalizes its operands for the membership
    // comparison; callers pass this path on to filesystem and display logic.
    let resolved_project_root = canonicalize_or_normalize(project_root);
    let root = match path {
        Some(path) => {
            let path = PathBuf::from(path);
            if path.is_absolute() {
                canonicalize_or_normalize(&path)
            } else {
                normalize_path(&resolved_project_root.join(path))
            }
        }
        None => resolved_project_root.clone(),
    };

    let use_index = is_within_search_root(&resolved_project_root, &root);
    SearchScope { root, use_index }
}

pub(crate) fn is_binary_bytes(content: &[u8]) -> bool {
    content_inspector::inspect(content).is_binary()
}

pub(crate) fn current_git_head(root: &Path) -> Option<String> {
    run_git(root, &["rev-parse", "HEAD"])
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum ArtifactCacheKeyFailure {
    IdentityUnavailable,
    GitUnavailable,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ArtifactCacheKeyProbeError {
    root: PathBuf,
    detail: String,
    failure: ArtifactCacheKeyFailure,
}

impl ArtifactCacheKeyProbeError {
    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn detail(&self) -> &str {
        &self.detail
    }

    pub(crate) fn provisional_git_reason(&self) -> Option<String> {
        if self.failure != ArtifactCacheKeyFailure::GitUnavailable {
            return None;
        }
        let mut reason = format!(
            "{GIT_IDENTITY_UNAVAILABLE_REASON}: {}; indexes and views stay off until this folder's next bind",
            self.detail
        );
        if self.detail.contains("dubious ownership") {
            let repository = crate::root_identity::nearest_git_marker(&self.root)
                .ok()
                .flatten()
                .unwrap_or_else(|| self.root.clone());
            reason.push_str(&format!(
                "; if you trust this repository, run git config --global --add safe.directory '{}'",
                repository.to_string_lossy().replace('\'', "'\\''")
            ));
        }
        Some(reason)
    }
}

impl std::fmt::Display for ArtifactCacheKeyProbeError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "artifact cache key probe failed for {}: {}",
            self.root.display(),
            self.detail
        )
    }
}

impl std::error::Error for ArtifactCacheKeyProbeError {}

pub fn artifact_cache_key(project_root: &Path) -> String {
    let key = match repo_root_commit_with_retry(project_root) {
        RootCommitResolution::Commit(root_commit) => {
            session_artifact_key_from_git_identity(project_root, &root_commit)
        }
        RootCommitResolution::NotARepo => artifact_path_identity_key(project_root),
        RootCommitResolution::Failed(detail) => {
            crate::slog_warn!(
                "artifact cache key: git root-commit probe failed after retries ({}); \
                 falling back to path identity for {}",
                detail,
                project_root.display()
            );
            artifact_path_identity_key(project_root)
        }
        RootCommitResolution::Cancelled => artifact_path_identity_key(project_root),
    };
    record_derived_cache_key(project_root, &key);
    key
}

/// Reuse the artifact key registered during configure when one exists; otherwise
/// derive a key from Git at `project_root`. This fallback is for standalone build
/// APIs whose callers have not configured a project or supplied a storage path.
pub(crate) fn artifact_cache_key_for_build(project_root: &Path) -> String {
    crate::root_cache::configured_artifact_key(project_root)
        .unwrap_or_else(|| artifact_cache_key(project_root))
}

pub fn artifact_cache_key_with_memo(
    probe_root: &Path,
    memo_root: &Path,
    storage_root: &Path,
    git_common_dir: Option<&Path>,
) -> Result<String, ArtifactCacheKeyProbeError> {
    artifact_cache_key_with_memo_profiled(
        probe_root,
        memo_root,
        storage_root,
        git_common_dir,
        &mut ArtifactCacheKeyTimings::default(),
    )
}

/// Resolve a reader's key from the same identity records as a session bind,
/// without creating or refreshing identity metadata. A busy legacy database
/// leaves the identity unresolved rather than publishing a provisional key.
pub(crate) fn artifact_cache_key_read_only(
    project_root: &Path,
    storage_root: &Path,
) -> Result<String, ArtifactCacheKeyProbeError> {
    resolve_root_identity(
        project_root,
        project_root,
        storage_root,
        None,
        &mut ArtifactCacheKeyTimings::default(),
        false,
    )
    .map(|identity| identity.key(project_root, false))
}

/// Wall times overlap: memo_hold includes load, pruning and writes, while
/// rev_list includes child execution and waiting for the child to exit.
#[derive(Debug, Default)]
pub(crate) struct ArtifactCacheKeyTimings {
    pub(crate) git_usable: Duration,
    pub(crate) rev_list: Duration,
    /// Git processes actually started, including unborn-branch verification.
    pub(crate) git_spawns: usize,
    pub(crate) retry_wait: Duration,
    pub(crate) memo_wait: Duration,
    pub(crate) memo_hold: Duration,
    pub(crate) memo_load: Duration,
    pub(crate) memo_prune: Duration,
    pub(crate) memo_write: Duration,
    pub(crate) legacy_memo: Duration,
    pub(crate) context_memo_wait: Duration,
    pub(crate) context_cache_hit: bool,
}

fn time_cache_key_work<T>(elapsed: &mut Duration, run: impl FnOnce() -> T) -> T {
    let started = Instant::now();
    let result = run();
    *elapsed += started.elapsed();
    result
}

pub(crate) fn artifact_cache_key_with_memo_profiled(
    probe_root: &Path,
    memo_root: &Path,
    storage_root: &Path,
    git_common_dir: Option<&Path>,
    timings: &mut ArtifactCacheKeyTimings,
) -> Result<String, ArtifactCacheKeyProbeError> {
    let identity = resolve_root_identity(
        probe_root,
        memo_root,
        storage_root,
        git_common_dir,
        timings,
        true,
    )?;
    let key = identity.key(probe_root, false);
    record_derived_cache_key(memo_root, &key);
    Ok(key)
}

/// The artifact key a session bind uses, or `None` while the root's identity
/// is provisional (see [`resolve_root_identity_for_bind`]). A provisional key
/// is not recorded anywhere, so the next bind derives the key again.
pub(crate) fn artifact_cache_key_for_bind_profiled(
    probe_root: &Path,
    memo_root: &Path,
    storage_root: &Path,
    git_common_dir: Option<&Path>,
    timings: &mut ArtifactCacheKeyTimings,
) -> Result<Option<String>, ArtifactCacheKeyProbeError> {
    let bound = resolve_root_identity_for_bind(
        probe_root,
        memo_root,
        storage_root,
        git_common_dir,
        timings,
        true,
        true,
    )?;
    if bound.provisional {
        return Ok(None);
    }
    let key = bound.identity.key(probe_root, false);
    record_derived_cache_key(memo_root, &key);
    Ok(Some(key))
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct LegacyMemoStamp {
    identity: crate::file_identity::FileIdentity,
    len: u64,
    modified: SystemTime,
    #[cfg(unix)]
    changed: (i64, i64),
}

impl LegacyMemoStamp {
    fn new(metadata: &fs::Metadata, identity: crate::file_identity::FileIdentity) -> Option<Self> {
        if identity == crate::file_identity::FileIdentity::Unavailable {
            return None;
        }
        #[cfg(unix)]
        use std::os::unix::fs::MetadataExt;
        Some(Self {
            identity,
            len: metadata.len(),
            modified: metadata.modified().ok()?,
            #[cfg(unix)]
            changed: (metadata.ctime(), metadata.ctime_nsec()),
        })
    }

    fn at(path: &Path) -> Option<Self> {
        let (metadata, identity) = crate::file_identity::metadata(path).ok()?;
        Self::new(&metadata, identity)
    }

    fn of_file(file: &File) -> Option<Self> {
        Self::new(
            &file.metadata().ok()?,
            crate::file_identity::FileIdentity::of_file(file),
        )
    }
}

struct LegacyMemoIndex {
    stamp: Option<LegacyMemoStamp>,
    built_at: Instant,
    entries: Vec<(String, ArtifactCacheKeyMemoEntry, PathBuf)>,
    aliases: BTreeMap<PathBuf, Vec<usize>>,
}

fn canonical_legacy_memo_spelling(spelling: &str) -> PathBuf {
    #[cfg(test)]
    LEGACY_MEMO_CANONICALIZATIONS.with(|count| count.set(count.get() + 1));
    crate::root_identity::canonical_root(Path::new(spelling))
}

impl LegacyMemoIndex {
    fn build(
        entries: BTreeMap<String, ArtifactCacheKeyMemoEntry>,
        stamp: Option<LegacyMemoStamp>,
    ) -> Self {
        #[cfg(test)]
        LEGACY_MEMO_BUILDS.with(|count| count.set(count.get() + 1));
        let mut aliases: BTreeMap<PathBuf, Vec<usize>> = BTreeMap::new();
        let entries = entries
            .into_iter()
            .enumerate()
            .map(|(index, (spelling, entry))| {
                let canonical = canonical_legacy_memo_spelling(&spelling);
                aliases.entry(canonical.clone()).or_default().push(index);
                (spelling, entry, canonical)
            })
            .collect();
        Self {
            stamp,
            built_at: Instant::now(),
            entries,
            aliases,
        }
    }

    fn apply_write(
        &mut self,
        entries: &BTreeMap<String, ArtifactCacheKeyMemoEntry>,
        stamp: LegacyMemoStamp,
    ) {
        let mut previous = std::mem::take(&mut self.entries).into_iter().peekable();
        let mut aliases: BTreeMap<PathBuf, Vec<usize>> = BTreeMap::new();
        let mut updated = Vec::with_capacity(entries.len());
        for (spelling, entry) in entries {
            while previous.peek().is_some_and(|(old, _, _)| old < spelling) {
                previous.next();
            }
            let old = previous
                .peek()
                .is_some_and(|(old, _, _)| old == spelling)
                .then(|| previous.next().unwrap());
            let canonical = match old {
                Some((_, old, canonical))
                    if old.key == entry.key
                        && old.git_root_commit == entry.git_root_commit
                        && old.recorded_at_ms == entry.recorded_at_ms =>
                {
                    canonical
                }
                _ => canonical_legacy_memo_spelling(spelling),
            };
            aliases
                .entry(canonical.clone())
                .or_default()
                .push(updated.len());
            updated.push((spelling.clone(), entry.clone(), canonical));
        }
        // Recreate bucket membership in spelling order after removals/changes.
        // Lookup folds these current entries, so conflict and validation results
        // are recomputed, not retained from an earlier version of a bucket.
        self.entries = updated;
        self.aliases = aliases;
        self.stamp = Some(stamp);
        // Keep built_at: writes must not postpone discovery of new aliases among
        // unchanged spellings by extending their ten-minute lifetime.
    }

    fn lookup(
        &self,
        root: &Path,
        raw_key: &str,
    ) -> std::io::Result<Option<ArtifactCacheKeyMemoEntry>> {
        let canonical = crate::root_identity::canonical_root(root);
        let mut candidates = self.aliases.get(&canonical).cloned().unwrap_or_default();
        if let Ok(raw) = self
            .entries
            .binary_search_by(|(spelling, _, _)| spelling.as_str().cmp(raw_key))
        {
            if let Err(position) = candidates.binary_search(&raw) {
                candidates.insert(position, raw);
            }
        }
        let mut found: Option<&ArtifactCacheKeyMemoEntry> = None;
        for index in candidates {
            let (spelling, entry, indexed_root) = &self.entries[index];
            let current = crate::root_identity::canonical_root(Path::new(spelling));
            // Keep the exact-spelling shortcut while its path mapping is
            // unchanged, but not after it moves away from the query root.
            if current != canonical && !(spelling == raw_key && current == *indexed_root) {
                continue;
            }
            if !artifact_key_looks_valid(&entry.key) || entry.git_root_commit.trim().is_empty() {
                return Err(std::io::Error::other(
                    "legacy repository identity memo is invalid",
                ));
            }
            if found.is_some_and(|previous| previous.git_root_commit != entry.git_root_commit) {
                return Err(std::io::Error::other(
                    "canonical root aliases have conflicting repository identities",
                ));
            }
            found = Some(entry);
        }
        Ok(found.cloned())
    }
}

#[derive(Default)]
struct LegacyMemoSlotState {
    snapshot: Mutex<Option<LegacyMemoIndex>>,
    // A tiny mirror lets registry cleanup inspect age without waiting for the
    // snapshot lock, which can be held during filesystem work.
    snapshot_built_at: Mutex<Option<Instant>>,
}

type LegacyMemoSlot = Arc<LegacyMemoSlotState>;

struct LegacyMemoSlotGuard<'a> {
    snapshot: std::sync::MutexGuard<'a, Option<LegacyMemoIndex>>,
    snapshot_built_at: &'a Mutex<Option<Instant>>,
}

impl LegacyMemoSlotState {
    fn lock(&self) -> std::sync::LockResult<LegacyMemoSlotGuard<'_>> {
        match self.snapshot.lock() {
            Ok(snapshot) => Ok(LegacyMemoSlotGuard {
                snapshot,
                snapshot_built_at: &self.snapshot_built_at,
            }),
            Err(error) => Err(std::sync::PoisonError::new(LegacyMemoSlotGuard {
                snapshot: error.into_inner(),
                snapshot_built_at: &self.snapshot_built_at,
            })),
        }
    }

    fn snapshot_expired(&self, now: Instant) -> bool {
        self.snapshot_built_at
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .is_some_and(|built_at| {
                now.saturating_duration_since(built_at) >= LEGACY_IDENTITY_MEMO_MAX_AGE
            })
    }
}

impl std::ops::Deref for LegacyMemoSlotGuard<'_> {
    type Target = Option<LegacyMemoIndex>;
    fn deref(&self) -> &Self::Target {
        &self.snapshot
    }
}

impl std::ops::DerefMut for LegacyMemoSlotGuard<'_> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.snapshot
    }
}

impl Drop for LegacyMemoSlotGuard<'_> {
    fn drop(&mut self) {
        // Publish age as the snapshot lock is released, without requiring the
        // registry to acquire the lock used for filesystem work.
        *self
            .snapshot_built_at
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) =
            self.snapshot.as_ref().map(|index| index.built_at);
    }
}

/// Retain at most four storage-root indexes, ordered by last slot acquisition,
/// and remove expired snapshots on every registry touch. A representative
/// 3,000-entry, 1.09 MB memo measured about 3.3 MiB of retained index heap on
/// 64-bit macOS; pathname lengths affect that footprint. In-flight callers may
/// keep an evicted slot alive until their current operation finishes.
#[derive(Default)]
struct LegacyMemoIndexes {
    // Least recently used first; the cache is deliberately tiny.
    roots: Vec<(PathBuf, LegacyMemoSlot)>,
}

impl LegacyMemoIndexes {
    fn slot(&mut self, path: PathBuf) -> (LegacyMemoSlot, Vec<LegacyMemoSlot>) {
        let now = Instant::now();
        let mut retired = Vec::new();
        self.roots.retain(|(_, slot)| {
            if slot.snapshot_expired(now) {
                retired.push(slot.clone());
                false
            } else {
                true
            }
        });
        if let Some(position) = self.roots.iter().position(|(root, _)| root == &path) {
            let entry = self.roots.remove(position);
            let slot = entry.1.clone();
            self.roots.push(entry);
            return (slot, retired);
        }
        let slot = LegacyMemoSlot::default();
        self.roots.push((path, slot.clone()));
        if self.roots.len() > LEGACY_IDENTITY_MEMO_MAX_STORAGE_ROOTS {
            retired.push(self.roots.remove(0).1);
        }
        (slot, retired)
    }
}

#[cfg(test)]
thread_local! {
    static LEGACY_MEMO_INDEXES_OVERRIDE: std::cell::RefCell<Option<Arc<Mutex<LegacyMemoIndexes>>>> = const { std::cell::RefCell::new(None) };
}

fn legacy_identity_memo_indexes() -> Arc<Mutex<LegacyMemoIndexes>> {
    #[cfg(test)]
    if let Some(indexes) = LEGACY_MEMO_INDEXES_OVERRIDE.with(|slot| slot.borrow().clone()) {
        return indexes;
    }
    static INDEXES: OnceLock<Arc<Mutex<LegacyMemoIndexes>>> = OnceLock::new();
    INDEXES.get_or_init(Default::default).clone()
}

fn legacy_identity_memo_slot(storage: &Path) -> LegacyMemoSlot {
    let indexes = legacy_identity_memo_indexes();
    let (slot, retired) = {
        indexes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .slot(artifact_cache_key_memo_path(storage))
    };
    // Release the registry before freeing an index's strings and path tables.
    drop(retired);
    slot
}

/// Cache canonical spellings by the memo file's identity, length and times;
/// apply AFT writes incrementally and rebuild after ten minutes even if the file
/// is unchanged.
/// Revalidate matched spellings on every lookup so a moved alias cannot return
/// its former root's identity. Newly matching spellings, including new conflicts,
/// can be missed until refresh; the age limit bounds that discovery delay.
fn legacy_identity_memo(
    root: &Path,
    raw_key: &str,
    storage: &Path,
) -> std::io::Result<Option<ArtifactCacheKeyMemoEntry>> {
    let path = artifact_cache_key_memo_path(storage);
    let slot = legacy_identity_memo_slot(storage);
    // Only this storage's first build is serialized. Do not take the ordinary
    // process memo mutex while resolving every legacy spelling's filesystem path.
    let mut cached = slot
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let stamp = LegacyMemoStamp::at(&path);
    if let Some(index) = cached.as_ref().filter(|index| {
        stamp.is_some()
            && index.stamp == stamp
            && index.built_at.elapsed() < LEGACY_IDENTITY_MEMO_MAX_AGE
    }) {
        return index.lookup(root, raw_key);
    }
    *cached = None;
    let mut file = match File::open(&path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    let before = LegacyMemoStamp::of_file(&file);
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    let entries = serde_json::from_slice(&bytes).map_err(std::io::Error::other)?;
    let index = LegacyMemoIndex::build(entries, before);
    let result = index.lookup(root, raw_key);
    // Read and canonicalization can race a writer. Return the opened snapshot,
    // as a full scan would, but never cache it under a replacement's stamp.
    if before.is_some()
        && before == LegacyMemoStamp::of_file(&file)
        && before == LegacyMemoStamp::at(&path)
    {
        *cached = Some(index);
    }
    result
}

#[cfg(test)]
fn legacy_identity_memo_full_scan(
    root: &Path,
    raw_key: &str,
    storage: &Path,
) -> std::io::Result<Option<ArtifactCacheKeyMemoEntry>> {
    let entries: BTreeMap<String, ArtifactCacheKeyMemoEntry> =
        match fs::read(artifact_cache_key_memo_path(storage)) {
            Ok(bytes) => serde_json::from_slice(&bytes).map_err(std::io::Error::other)?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error),
        };
    let canonical = crate::root_identity::canonical_root(root);
    let mut found: Option<ArtifactCacheKeyMemoEntry> = None;
    for (spelling, entry) in entries {
        if spelling != raw_key
            && crate::root_identity::canonical_root(Path::new(&spelling)) != canonical
        {
            continue;
        }
        if !artifact_key_looks_valid(&entry.key) || entry.git_root_commit.trim().is_empty() {
            return Err(std::io::Error::other(
                "legacy repository identity memo is invalid",
            ));
        }
        if found
            .as_ref()
            .is_some_and(|previous| previous.git_root_commit != entry.git_root_commit)
        {
            return Err(std::io::Error::other(
                "canonical root aliases have conflicting repository identities",
            ));
        }
        found = Some(entry);
    }
    Ok(found)
}

/// Session configure and StandingRoots::reconcile use the same stored path-or-Git
/// identity. With `persist=false`, health reports inspect it without creating a record.
pub(crate) fn resolve_root_identity(
    probe_root: &Path,
    memo_root: &Path,
    storage_root: &Path,
    git_common_dir: Option<&Path>,
    timings: &mut ArtifactCacheKeyTimings,
    persist: bool,
) -> Result<crate::root_identity::Identity, ArtifactCacheKeyProbeError> {
    resolve_root_identity_with_recheck(
        probe_root,
        memo_root,
        storage_root,
        git_common_dir,
        timings,
        persist,
        true,
    )
}

/// `force` distinguishes a real bind from background standing reconciliation.
/// A provisional identity is an error here: only a session bind may use one,
/// and only through [`resolve_root_identity_for_bind`].
#[allow(clippy::too_many_arguments)]
pub(crate) fn resolve_root_identity_with_recheck(
    probe_root: &Path,
    memo_root: &Path,
    storage_root: &Path,
    git_common_dir: Option<&Path>,
    timings: &mut ArtifactCacheKeyTimings,
    persist: bool,
    force: bool,
) -> Result<crate::root_identity::Identity, ArtifactCacheKeyProbeError> {
    let bound = resolve_root_identity_for_bind(
        probe_root,
        memo_root,
        storage_root,
        git_common_dir,
        timings,
        persist,
        force,
    )?;
    if bound.provisional {
        return Err(ArtifactCacheKeyProbeError {
            root: memo_root.to_path_buf(),
            detail: PROVISIONAL_IDENTITY_REASON.to_string(),
            failure: ArtifactCacheKeyFailure::IdentityUnavailable,
        });
    }
    Ok(bound.identity)
}

/// Why a root has no settled artifact identity yet. Shown as the root's
/// degraded reason and as the detail of a refused identity lookup. A bound
/// root retries the identity in the background while this reason stands; see
/// [`crate::provisional_identity_retry`].
pub(crate) const PROVISIONAL_IDENTITY_REASON: &str =
    "identity provisional: aft.db was busy at bind; indexes start once the database is free";
/// Replaces [`PROVISIONAL_IDENTITY_REASON`] once the background retry gives up.
pub(crate) const PROVISIONAL_IDENTITY_EXHAUSTED_REASON: &str = "identity provisional: aft.db stayed busy or unreadable for 5 minutes after bind, so this folder has no search, semantic or call-graph index. Close whatever holds aft.db open (usually another AFT process in a long write), then reopen this folder's session to bind it again";
pub(crate) const GIT_IDENTITY_UNAVAILABLE_REASON: &str =
    "identity provisional: git_identity_unavailable";

/// Like [`resolve_root_identity_with_recheck`], but a busy aft.db yields a
/// provisional identity instead of an error. The caller must not store any
/// artifact, ownership or retention record under a provisional identity.
#[allow(clippy::too_many_arguments)]
pub(crate) fn resolve_root_identity_for_bind(
    probe_root: &Path,
    memo_root: &Path,
    storage_root: &Path,
    git_common_dir: Option<&Path>,
    timings: &mut ArtifactCacheKeyTimings,
    persist: bool,
    force: bool,
) -> Result<crate::root_identity::BoundIdentity, ArtifactCacheKeyProbeError> {
    // A `storage.cache_dir` cache root keeps no identity records; resolve and
    // save them on the durable root that owns it, where configure memoizes
    // the key. Any other root, and every root without `cache_dir`, is used
    // as given.
    let storage_root: &Path = &crate::root_cache::identity_storage_root(storage_root);
    let mut git_unavailable = false;
    let mut choose = |pin: crate::root_identity::LegacyPin,
                      saved: Option<&crate::root_identity::Identity>|
     -> std::io::Result<crate::root_identity::IdentityChoice> {
        use crate::root_identity::{Identity, IdentityChoice, LegacyPin};
        let pinned_path = pin == LegacyPin::Path;
        // While an older pin cannot be read, the identity chosen here is only
        // temporary, so neither refresh nor write the legacy memo from it.
        let persist = persist && pin != LegacyPin::Unreadable;
        let memo_root_key = artifact_cache_key_memo_root_key(memo_root);
        let canonical_key =
            artifact_cache_key_memo_root_key(&crate::root_identity::canonical_root(probe_root));
        // Cached lookups refresh old entries on disk. Health uses the direct,
        // non-refreshing reader instead of this mutating cache lookup.
        let cached = (persist && crate::developer_tools::git_usable())
            .then(|| {
                lookup_artifact_cache_key_memo_profiled(storage_root, &canonical_key, timings)
                    .or_else(|| {
                        lookup_artifact_cache_key_memo_profiled(
                            storage_root,
                            &memo_root_key,
                            timings,
                        )
                    })
            })
            .flatten();
        let memo = if cached.is_some() && !pinned_path {
            cached
        } else {
            match time_cache_key_work(&mut timings.legacy_memo, || {
                legacy_identity_memo(probe_root, &memo_root_key, storage_root)
            }) {
                Ok(memo) => memo,
                Err(error) if crate::developer_tools::git_usable() && !pinned_path => {
                    crate::slog_warn!(
                        "legacy identity memo unavailable; verify the repository with Git: {error}"
                    );
                    None
                }
                Err(error) => return Err(error),
            }
        };
        if pinned_path {
            if memo.is_some() {
                return Err(std::io::Error::other("a standing path identity conflicts with an existing repository memo; explicit identity migration is required"));
            }
            return Ok(IdentityChoice::retained(Identity::Path));
        }
        if !crate::developer_tools::git_usable() {
            return match memo {
                Some(entry) => {
                    let identity = Identity::git(probe_root, entry.git_root_commit)?;
                    if crate::root_identity::nearest_git_marker(probe_root)?.is_none()
                        && identity.key(probe_root, false) != entry.key
                    {
                        return Err(std::io::Error::other(
                            "legacy repository scope cannot be recovered without Git",
                        ));
                    }
                    Ok(IdentityChoice::retained(identity))
                }
                None => Ok(IdentityChoice::retained(Identity::Path)),
            };
        }
        let marker = match crate::root_identity::nearest_git_marker(probe_root) {
            Ok(Some(_)) => GitMarkerState::Present,
            Ok(None) => root_git_marker_state(probe_root, git_common_dir),
            Err(_) if saved.is_some() => {
                return Ok(IdentityChoice::retained(saved.unwrap().clone()))
            }
            Err(error) => return Err(error),
        };
        if marker == GitMarkerState::Absent && memo.is_none() && saved.is_none() {
            return Ok(IdentityChoice::retained(Identity::Path));
        }
        match repo_root_commit_with_retry_profiled(probe_root, timings) {
            RootCommitResolution::Commit(root_commit) => {
                let key = session_artifact_key_from_git_identity(probe_root, &root_commit);
                if persist {
                    if let Err(error) = record_artifact_cache_key_memo_profiled(
                        storage_root,
                        &memo_root_key,
                        &key,
                        &root_commit,
                        timings,
                    ) {
                        crate::slog_warn!(
                            "artifact cache key: could not update legacy Git memo: {error}"
                        );
                    }
                }
                Identity::git(probe_root, root_commit).map(IdentityChoice::verified)
            }
            // A verified non-repository or unborn replacement must not inherit
            // the committed repository identity retained in an older memo.
            RootCommitResolution::NotARepo => Ok(IdentityChoice::verified(Identity::Path)),
            RootCommitResolution::Failed(_) if saved.is_some() => {
                Ok(IdentityChoice::retained(saved.unwrap().clone()))
            }
            RootCommitResolution::Failed(_) if memo.is_some() => {
                Identity::git(probe_root, memo.unwrap().git_root_commit)
                    .map(IdentityChoice::retained)
            }
            RootCommitResolution::Failed(detail) => {
                git_unavailable = true;
                Err(std::io::Error::other(detail))
            }
            RootCommitResolution::Cancelled => {
                Err(std::io::Error::other("artifact cache key probe cancelled"))
            }
        }
    };
    let result = if persist {
        crate::root_identity::bind_with_recheck(probe_root, storage_root, force, choose)
    } else {
        match crate::root_identity::read(probe_root, storage_root) {
            Ok(Some(identity)) => Ok(crate::root_identity::BoundIdentity {
                identity,
                provisional: false,
            }),
            Ok(None) => {
                crate::root_identity::select(probe_root, storage_root, |pin| choose(pin, None))
            }
            Err(error) => Err(error),
        }
    };
    result.map_err(|error| ArtifactCacheKeyProbeError {
        root: memo_root.to_path_buf(),
        failure: if git_unavailable {
            ArtifactCacheKeyFailure::GitUnavailable
        } else {
            ArtifactCacheKeyFailure::IdentityUnavailable
        },
        detail: if git_unavailable {
            error.to_string()
        } else {
            format!("{error}; an existing identity was not replaced. Restore unreadable identity metadata or perform an explicit identity migration. {}", if crate::developer_tools::git_usable() { "" } else { crate::developer_tools::MISSING_DEVELOPER_TOOLS_REASON })
        },
    })
}

/// In-process root→key map recorded at every successful cache-key
/// derivation, for paths that must NEVER block. Unlike the persisted memo
/// (git identities only, lazily loaded from disk), this covers path-identity
/// roots too and never touches disk or spawns.
static DERIVED_CACHE_KEYS: OnceLock<RwLock<HashMap<PathBuf, String>>> = OnceLock::new();

fn record_derived_cache_key(project_root: &Path, key: &str) {
    let map = DERIVED_CACHE_KEYS.get_or_init(|| RwLock::new(HashMap::new()));
    if let Ok(mut map) = map.write() {
        map.insert(project_root.to_path_buf(), key.to_string());
    }
}

/// Non-blocking cache-key lookup for the channel-0 health reply path:
/// consults only the in-process derivation map — no git subprocess, no disk
/// read, try-lock only. Returns `None` for a root that has not derived a key
/// this process (callers treat that as "annotation unavailable").
///
/// Why this exists: `artifact_cache_key()` spawns a git probe (up to 3 execs
/// + backoff sleeps), and on a host whose exec path is stalling — the exact
/// condition health probes exist to ride out — a per-root spawn loop pushes
/// the health reply past the supervisor's deadline and reads as module death
/// (2026-08-08 second outage). The health-path rule is not just
/// try-lock-only: NOTHING on the reply path may block, and exec is worse
/// than any lock.
pub fn artifact_cache_key_memoized_only(project_root: &Path) -> Option<String> {
    let map = DERIVED_CACHE_KEYS.get()?;
    let map = map.try_read().ok()?;
    map.get(project_root).cloned()
}

/// `storage_dir` is the cache artifact root (see
/// [`crate::bash_background::cache_artifact_root`]), not the durable root.
pub fn resolve_cache_dir_with_key(project_key: &str, storage_dir: Option<&Path>) -> PathBuf {
    crate::bash_background::cache_artifact_root(storage_dir)
        .join("index")
        .join(project_key)
}

/// Keep Git-top-level artifact derivation shared with standing roots byte-for-byte.
pub(crate) fn artifact_key_from_git_identity(root_commit: &str) -> String {
    artifact_hash16(root_commit.as_bytes())
}

/// Artifact key for a session rooted at `project_root` inside the Git
/// repository whose canonical root-commit identity is `root_commit`.
///
/// A session at a checkout's top level uses the whole-repository key, which
/// the home checkout publishes and its linked worktrees borrow. A session
/// opened in a subfolder indexes only that subfolder, so it must not publish
/// under the whole-repository key: its snapshot would replace the full one and
/// every borrower would silently lose the files outside the subfolder. It
/// gets a key of its own instead, derived like a standing root below a
/// checkout (`scoped_key::scoped_v1_key` over the repository identity and the
/// forward-slash path below the top level). The same subfolder of the home
/// checkout and of a linked worktree therefore still share one artifact.
///
/// The key is cut to the 16 hex characters every other session key has,
/// because session storage (orphan sweeps, the key memo, transient caches)
/// only recognises that shape. The full-length scoped key stays reserved for
/// standing roots, so a session never co-writes a standing root's artifact.
pub(crate) fn session_artifact_key_from_git_identity(
    project_root: &Path,
    root_commit: &str,
) -> String {
    match checkout_position(project_root) {
        CheckoutPosition::TopLevel => artifact_key_from_git_identity(root_commit),
        CheckoutPosition::Subdirectory(relative) => {
            match crate::scoped_key::scoped_v1_key(root_commit, relative.as_bytes()) {
                Ok(scoped) => scoped[..16].to_string(),
                Err(_) => artifact_path_identity_key(project_root),
            }
        }
        // A subfolder whose path cannot be spelled as a logical relative path
        // still must not share the repository key; its own path is unique.
        CheckoutPosition::Unrepresentable => artifact_path_identity_key(project_root),
    }
}

/// Where a directory sits inside the Git checkout that contains it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum CheckoutPosition {
    /// The checkout's top level, or a directory with no enclosing checkout.
    TopLevel,
    /// A directory below the top level, as a forward-slash relative path.
    Subdirectory(String),
    /// A directory below the top level whose relative path is not valid
    /// Unicode or otherwise cannot be written as a logical relative path.
    Unrepresentable,
}

/// Locate `project_root` relative to the nearest enclosing checkout top level:
/// the closest directory, starting at `project_root` itself, that holds a
/// `.git` entry (a directory in a home checkout, a file in a linked worktree
/// or submodule). This reads the filesystem only; it never runs git.
///
/// Both sides of any comparison are derived through this one function from
/// the same canonicalized form, so the result is stable across the Windows
/// verbatim and plain spellings of one path.
pub(crate) fn checkout_position(project_root: &Path) -> CheckoutPosition {
    let root = canonicalize_or_normalize(project_root);
    let has_git_marker = |dir: &Path| fs::symlink_metadata(dir.join(".git")).is_ok();
    if has_git_marker(&root) {
        return CheckoutPosition::TopLevel;
    }
    for ancestor in root.ancestors().skip(1) {
        if has_git_marker(ancestor) {
            return match crate::scoped_key::scoped_relative_path(&root, ancestor) {
                Ok(relative) => CheckoutPosition::Subdirectory(relative),
                Err(_) => CheckoutPosition::Unrepresentable,
            };
        }
    }
    CheckoutPosition::TopLevel
}

/// Existing non-Git path-scope artifact derivation, exposed for the standing
/// identity classifier so it does not mint a parallel path-key family.
pub(crate) fn artifact_path_identity_key(project_root: &Path) -> String {
    let canonical_root = canonicalize_or_normalize(project_root);
    artifact_hash16(canonical_root.to_string_lossy().as_bytes())
}

#[cfg(test)]
pub(crate) fn artifact_path_identity_key_for_test(project_root: &Path) -> String {
    artifact_path_identity_key(project_root)
}

fn artifact_hash16(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};

    let mut hasher = Sha256::new();
    hasher.update(bytes);
    let digest = format!("{:x}", hasher.finalize());
    digest[..16].to_string()
}

fn artifact_cache_key_memo_root_key(root: &Path) -> String {
    root.to_string_lossy().into_owned()
}

fn artifact_cache_key_memo_path(storage_root: &Path) -> PathBuf {
    storage_root.join(ARTIFACT_CACHE_KEY_MEMO_FILE)
}

fn artifact_cache_key_memo_state() -> &'static Mutex<ArtifactCacheKeyMemoState> {
    ARTIFACT_CACHE_KEY_MEMO_STATE.get_or_init(|| Mutex::new(ArtifactCacheKeyMemoState::default()))
}

impl ArtifactCacheKeyMemoState {
    fn entries_for_storage_root(
        &mut self,
        storage_root: &Path,
        timings: &mut ArtifactCacheKeyTimings,
    ) -> &mut BTreeMap<String, ArtifactCacheKeyMemoEntry> {
        if !self.by_storage_root.contains_key(storage_root) {
            let entries = time_cache_key_work(&mut timings.memo_load, || {
                read_artifact_cache_key_memo_file(storage_root)
            });
            self.by_storage_root
                .insert(storage_root.to_path_buf(), entries);
        }
        self.by_storage_root
            .get_mut(storage_root)
            .expect("memo storage root inserted")
    }
}

#[cfg(test)]
fn lookup_artifact_cache_key_memo(
    storage_root: &Path,
    memo_root_key: &str,
) -> Option<ArtifactCacheKeyMemoEntry> {
    lookup_artifact_cache_key_memo_profiled(
        storage_root,
        memo_root_key,
        &mut ArtifactCacheKeyTimings::default(),
    )
}

fn lookup_artifact_cache_key_memo_profiled(
    storage_root: &Path,
    memo_root_key: &str,
    timings: &mut ArtifactCacheKeyTimings,
) -> Option<ArtifactCacheKeyMemoEntry> {
    let waiting = Instant::now();
    let mut state = artifact_cache_key_memo_state()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    timings.memo_wait += waiting.elapsed();
    let holding = Instant::now();
    let result = (|| {
        let entries = state.entries_for_storage_root(storage_root, timings);
        let entry = entries.get(memo_root_key)?.clone();
        let now = current_time_millis();
        if Path::new(memo_root_key).exists()
            && now.saturating_sub(entry.recorded_at_ms)
                >= ARTIFACT_CACHE_KEY_MEMO_READ_REFRESH_AGE.as_millis() as u64
        {
            // Read-only roots may reuse the same borrowed artifact key for months without
            // deriving a new key. Refresh at most daily to keep the cached key available
            // while avoiding a disk write on every failed lookup.
            let mut refreshed = entry.clone();
            refreshed.recorded_at_ms = now;
            entries.insert(memo_root_key.to_string(), refreshed);
            if let Err(error) = time_cache_key_work(&mut timings.memo_write, || {
                write_artifact_cache_key_memo_file(storage_root, entries)
            }) {
                entries.insert(memo_root_key.to_string(), entry.clone());
                crate::slog_warn!(
                    "artifact cache key: failed to refresh memo for {} in {}: {}",
                    memo_root_key,
                    storage_root.display(),
                    error
                );
            }
        }
        entries.get(memo_root_key).cloned()
    })();
    timings.memo_hold += holding.elapsed();
    result
}

#[cfg(test)]
fn record_artifact_cache_key_memo(
    storage_root: &Path,
    memo_root_key: &str,
    key: &str,
    git_root_commit: &str,
) -> std::io::Result<()> {
    record_artifact_cache_key_memo_profiled(
        storage_root,
        memo_root_key,
        key,
        git_root_commit,
        &mut ArtifactCacheKeyTimings::default(),
    )
}

fn record_artifact_cache_key_memo_profiled(
    storage_root: &Path,
    memo_root_key: &str,
    key: &str,
    git_root_commit: &str,
    timings: &mut ArtifactCacheKeyTimings,
) -> std::io::Result<()> {
    let waiting = Instant::now();
    let mut state = artifact_cache_key_memo_state()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    timings.memo_wait += waiting.elapsed();
    let holding = Instant::now();
    let result = (|| {
        let entries = state.entries_for_storage_root(storage_root, timings);
        if entries
            .get(memo_root_key)
            .is_some_and(|entry| entry.key == key && entry.git_root_commit == git_root_commit)
        {
            return Ok(());
        }
        let now = current_time_millis();
        entries.insert(
            memo_root_key.to_string(),
            ArtifactCacheKeyMemoEntry {
                key: key.to_string(),
                git_root_commit: git_root_commit.to_string(),
                recorded_at_ms: now,
            },
        );
        time_cache_key_work(&mut timings.memo_prune, || {
            prune_expired_artifact_cache_key_memo_entries(entries, now)
        });
        time_cache_key_work(&mut timings.memo_write, || {
            write_artifact_cache_key_memo_file(storage_root, entries)
        })
    })();
    timings.memo_hold += holding.elapsed();
    result
}

fn prune_expired_artifact_cache_key_memo_entries(
    entries: &mut BTreeMap<String, ArtifactCacheKeyMemoEntry>,
    now: u64,
) {
    entries.retain(|root, entry| {
        Path::new(root).exists()
            || now.saturating_sub(entry.recorded_at_ms)
                <= ARTIFACT_CACHE_KEY_MEMO_EVICTION_AGE.as_millis() as u64
    });
}

fn read_artifact_cache_key_memo_file(
    storage_root: &Path,
) -> BTreeMap<String, ArtifactCacheKeyMemoEntry> {
    let path = artifact_cache_key_memo_path(storage_root);
    let bytes = match fs::read(&path) {
        Ok(bytes) => bytes,
        Err(_) => return BTreeMap::new(),
    };
    let entries =
        match serde_json::from_slice::<BTreeMap<String, ArtifactCacheKeyMemoEntry>>(&bytes) {
            Ok(entries) => entries,
            Err(error) => {
                crate::slog_warn!(
                    "artifact cache key: ignoring corrupt memo file {}: {}",
                    path.display(),
                    error
                );
                return BTreeMap::new();
            }
        };
    entries
        .into_iter()
        .filter(|(root, entry)| {
            !root.is_empty()
                && artifact_key_looks_valid(&entry.key)
                && !entry.git_root_commit.trim().is_empty()
        })
        .collect()
}

fn write_artifact_cache_key_memo_file(
    storage_root: &Path,
    entries: &BTreeMap<String, ArtifactCacheKeyMemoEntry>,
) -> std::io::Result<()> {
    crate::private_storage::open_root(storage_root)?;
    let path = artifact_cache_key_memo_path(storage_root);
    let temp_path = storage_root.join(format!(
        ".{ARTIFACT_CACHE_KEY_MEMO_FILE}.tmp.{}.{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or(Duration::ZERO)
            .as_nanos()
    ));
    let bytes = serde_json::to_vec_pretty(entries).map_err(std::io::Error::other)?;
    let mut file = crate::private_storage::create(&temp_path)?;
    file.write_all(&bytes)?;
    // Close write handles before sampling the published times: Windows may
    // defer its last-write timestamp until then. A read handle keeps the same
    // file's identity available through rename without delaying that update.
    let identity = crate::file_identity::FileIdentity::of_file(&file);
    let published_file = File::open(&temp_path).ok().filter(|opened| {
        identity != crate::file_identity::FileIdentity::Unavailable
            && crate::file_identity::FileIdentity::of_file(opened) == identity
    });
    drop(file);
    let slot = legacy_identity_memo_slot(storage_root);
    let mut cached = slot
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let previous_stamp = LegacyMemoStamp::at(&path);
    let can_advance = cached.as_ref().is_some_and(|index| {
        previous_stamp.is_some()
            && index.stamp == previous_stamp
            && index.built_at.elapsed() < LEGACY_IDENTITY_MEMO_MAX_AGE
    });
    if let Err(error) = fs::rename(&temp_path, &path) {
        let _ = fs::remove_file(&temp_path);
        return Err(error);
    }
    #[cfg(test)]
    if let Some(hook) = AFTER_LEGACY_MEMO_RENAME.with(|slot| slot.borrow_mut().take()) {
        hook(&path);
    }
    // Keep the written file open across rename. A stamp sampled only from the
    // destination could belong to another writer's replacement, not these bytes.
    let published_stamp = published_file.as_ref().and_then(LegacyMemoStamp::of_file);
    if can_advance && published_stamp.is_some() && published_stamp == LegacyMemoStamp::at(&path) {
        cached
            .as_mut()
            .unwrap()
            .apply_write(entries, published_stamp.unwrap());
        if published_stamp != published_file.as_ref().and_then(LegacyMemoStamp::of_file)
            || published_stamp != LegacyMemoStamp::at(&path)
        {
            *cached = None;
        }
    } else {
        *cached = None;
    }
    Ok(())
}

fn artifact_key_looks_valid(key: &str) -> bool {
    key.len() == 16 && key.bytes().all(|byte| byte.is_ascii_hexdigit())
}

/// Reclaim old index directories that no configured or in-process root can use.
/// Each candidate holds both search and root-keyed writer leases until deletion,
/// so a concurrent publisher leaves the directory for a later maintenance pass.
// Retained for isolated cache-lock fixtures; production eviction is scheduled
// through storage_retention rather than inferring abandonment from payload age.
#[cfg(test)]
pub(crate) fn sweep_orphaned_index_dirs(storage_root: &crate::root_cache::StoragePaths) {
    let index_root = storage_root.artifacts.join("index");
    let referenced_keys = match referenced_artifact_cache_keys(&storage_root.durable) {
        Ok(keys) => keys,
        Err(error) => {
            crate::slog_warn!(
                "search index orphan sweep root={} scanned=0 removed=0 skipped_derived=0 skipped_memo=0 skipped_fresh=0 skipped_live=0 skipped_locked=0 skipped_unreadable=0 budget_exhausted=false memo_unreadable=true error={}",
                index_root.display(),
                error
            );
            return;
        }
    };
    let derived_keys = derived_artifact_cache_keys();
    let mut summary = IndexOrphanSweepSummary::default();
    let mut entries = match fs::read_dir(&index_root) {
        Ok(entries) => entries
            .filter_map(Result::ok)
            .filter_map(|entry| {
                let file_type = entry.file_type().ok()?;
                let name = entry.file_name().to_string_lossy().into_owned();
                (file_type.is_dir() && artifact_key_looks_valid(&name))
                    .then(|| (name, entry.path()))
            })
            .collect::<Vec<_>>(),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(_) => {
            crate::slog_warn!(
                "search index orphan sweep root={} scanned=0 removed=0 skipped_derived=0 skipped_memo=0 skipped_fresh=0 skipped_live=0 skipped_locked=0 skipped_unreadable=1 budget_exhausted=false memo_unreadable=false",
                index_root.display()
            );
            return;
        }
    };
    entries.sort_by(|left, right| left.0.cmp(&right.0));

    let cursor_store = INDEX_ORPHAN_SWEEP_CURSORS.get_or_init(|| Mutex::new(HashMap::new()));
    let last_name = cursor_store
        .lock()
        .ok()
        .and_then(|cursors| cursors.get(&index_root).cloned());
    if let Some(start) = last_name
        .as_deref()
        .and_then(|last| entries.iter().position(|(name, _)| name.as_str() > last))
    {
        entries.rotate_left(start);
    }

    let mut cursor_name = last_name;
    for (processed, (key, cache_dir)) in entries.into_iter().enumerate() {
        if processed >= INDEX_ORPHAN_SWEEP_LIMIT {
            summary.budget_exhausted = true;
            break;
        }
        summary.scanned += 1;
        cursor_name = Some(key.clone());
        if derived_keys.contains(&key) {
            summary.skipped_derived += 1;
            continue;
        }
        if referenced_keys.contains(&key) {
            summary.skipped_memo += 1;
            continue;
        }
        let newest_file = match newest_index_cache_file_mtime(&cache_dir) {
            Ok(Some(time)) => time,
            Ok(None) | Err(()) => {
                summary.skipped_unreadable += 1;
                continue;
            }
        };
        if SystemTime::now()
            .duration_since(newest_file)
            .unwrap_or(Duration::ZERO)
            <= INDEX_ORPHAN_MIN_AGE
        {
            summary.skipped_fresh += 1;
            continue;
        }

        let Some((_cache_lock, _writer_lease)) = try_acquire_index_orphan_sweep_locks(&cache_dir)
        else {
            summary.skipped_locked += 1;
            continue;
        };
        if crate::root_cache::sweep_all_read_markers(&cache_dir).protected {
            summary.skipped_live += 1;
            continue;
        }
        // An orphaned cache in a newer build's format still belongs to that
        // build; only it may decide to discard it.
        if SearchIndex::check_disk_cache_format(&cache_dir).is_err() {
            summary.skipped_unreadable += 1;
            continue;
        }
        match fs::remove_dir_all(&cache_dir) {
            Ok(()) => summary.removed += 1,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => summary.removed += 1,
            Err(_) => summary.skipped_unreadable += 1,
        }
    }

    if let Ok(mut cursors) = cursor_store.lock() {
        if summary.budget_exhausted {
            if let Some(cursor_name) = cursor_name {
                cursors.insert(index_root.clone(), cursor_name);
            }
        } else {
            cursors.remove(&index_root);
        }
    }
    if summary.removed > 0 {}
    crate::slog_info!(
        "search index orphan sweep root={} scanned={} removed={} skipped_derived={} skipped_memo={} skipped_fresh={} skipped_live={} skipped_locked={} skipped_unreadable={} budget_exhausted={}",
        index_root.display(),
        summary.scanned,
        summary.removed,
        summary.skipped_derived,
        summary.skipped_memo,
        summary.skipped_fresh,
        summary.skipped_live,
        summary.skipped_locked,
        summary.skipped_unreadable,
        summary.budget_exhausted
    );
}

/// Return durable cache keys referenced by `cache-keys.json` without deriving new
/// identities. Maintenance sweeps use this read-only snapshot as their liveness
/// boundary so a failed or corrupt memo leaves artifacts untouched.
#[cfg(test)]
pub(crate) fn referenced_artifact_cache_keys(
    storage_root: &Path,
) -> std::io::Result<HashSet<String>> {
    let state = artifact_cache_key_memo_state()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let entries = state
        .by_storage_root
        .get(storage_root)
        .cloned()
        .map(Ok)
        .unwrap_or_else(|| {
            let path = artifact_cache_key_memo_path(storage_root);
            match fs::read(path) {
                Ok(bytes) => {
                    serde_json::from_slice::<BTreeMap<String, ArtifactCacheKeyMemoEntry>>(&bytes)
                        .map_err(std::io::Error::other)
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(BTreeMap::new()),
                Err(error) => Err(error),
            }
        })?;
    Ok(entries
        .into_values()
        .filter_map(|entry| artifact_key_looks_valid(&entry.key).then_some(entry.key))
        .collect())
}

/// Return keys derived by this process. They supplement the durable memo during
/// maintenance, protecting an active root before its next memo write completes.
#[cfg(test)]
pub(crate) fn derived_artifact_cache_keys() -> HashSet<String> {
    DERIVED_CACHE_KEYS
        .get()
        .and_then(|keys| keys.try_read().ok())
        .map(|keys| keys.values().cloned().collect())
        .unwrap_or_default()
}

#[cfg(test)]
fn try_acquire_index_orphan_sweep_locks(
    cache_dir: &Path,
) -> Option<(fs_lock::LockGuard, fs_lock::LockGuard)> {
    let cache_lock = fs_lock::try_acquire(&cache_dir.join("cache.lock"), Duration::ZERO).ok()?;
    let writer_lease = fs_lock::try_acquire(
        &crate::root_cache::writer_lease_path(cache_dir),
        Duration::ZERO,
    )
    .ok()?;
    Some((cache_lock, writer_lease))
}

#[cfg(test)]
fn newest_index_cache_file_mtime(cache_dir: &Path) -> Result<Option<SystemTime>, ()> {
    let boundary = crate::walk_boundary::DeviceBoundary::for_root(cache_dir).map_err(|_| ())?;
    let mut newest = None;
    newest_index_cache_file_mtime_inner(cache_dir, &boundary, &mut newest)?;
    Ok(newest)
}

#[cfg(test)]
fn newest_index_cache_file_mtime_inner(
    directory: &Path,
    boundary: &crate::walk_boundary::DeviceBoundary,
    newest: &mut Option<SystemTime>,
) -> Result<(), ()> {
    for entry in fs::read_dir(directory).map_err(|_| ())? {
        let entry = entry.map_err(|_| ())?;
        let file_type = entry.file_type().map_err(|_| ())?;
        if file_type.is_symlink() {
            return Err(());
        }
        let path = entry.path();
        if file_type.is_dir() {
            // A mounted child can disappear while this background cache scan owns
            // its ReadDir; skip it before opening it to avoid ENXIO Drop aborts.
            if !boundary.should_descend(&path).map_err(|_| ())? {
                crate::slog_warn!(
                    "search-index cache scan skipped foreign filesystem mount {}",
                    path.display()
                );
                continue;
            }
            newest_index_cache_file_mtime_inner(&path, boundary, newest)?;
        } else if file_type.is_file() {
            let modified = entry
                .metadata()
                .map_err(|_| ())?
                .modified()
                .map_err(|_| ())?;
            if newest.is_none_or(|current| modified > current) {
                *newest = Some(modified);
            }
        } else {
            return Err(());
        }
    }
    Ok(())
}

fn current_time_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
        .as_millis()
        .min(u128::from(u64::MAX)) as u64
}

#[derive(Debug, PartialEq, Eq)]
enum GitMarkerState {
    Present,
    Absent,
    Unknown(String),
}

fn root_git_marker_state(project_root: &Path, git_common_dir: Option<&Path>) -> GitMarkerState {
    if git_common_dir.is_some() {
        return GitMarkerState::Present;
    }
    let git_marker = project_root.join(".git");
    match fs::symlink_metadata(&git_marker) {
        Ok(_) => GitMarkerState::Present,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => GitMarkerState::Absent,
        Err(error) => GitMarkerState::Unknown(format!(
            "failed to inspect git marker {}: {}",
            git_marker.display(),
            error
        )),
    }
}

/// Return true only when the normal identity probe deterministically proves
/// that the project is not a Git repository. Transient failures stay unknown.
pub(crate) fn git_root_probe_confirms_non_repo(project_root: &Path) -> bool {
    matches!(
        repo_root_commit_with_retry(project_root),
        RootCommitResolution::NotARepo
    )
}

/// Return the canonical root-commit identity used by Git artifact keys.
///
/// Scoped roots consume this exact result as an opaque byte sequence; they must
/// not independently rebuild the sorted, deduplicated root set.
pub(crate) fn canonical_git_root_commit_identity(
    project_root: &Path,
) -> Result<String, ArtifactCacheKeyProbeError> {
    match repo_root_commit_with_retry(project_root) {
        RootCommitResolution::Commit(identity) => Ok(identity),
        RootCommitResolution::NotARepo => Err(ArtifactCacheKeyProbeError {
            root: project_root.to_path_buf(),
            detail: "not a git repository".to_string(),
            failure: ArtifactCacheKeyFailure::IdentityUnavailable,
        }),
        RootCommitResolution::Failed(detail) => Err(ArtifactCacheKeyProbeError {
            root: project_root.to_path_buf(),
            detail,
            failure: ArtifactCacheKeyFailure::GitUnavailable,
        }),
        RootCommitResolution::Cancelled => Err(ArtifactCacheKeyProbeError {
            root: project_root.to_path_buf(),
            detail: "artifact cache key probe cancelled".to_string(),
            failure: ArtifactCacheKeyFailure::IdentityUnavailable,
        }),
    }
}

/// Resolve the repository root commit, retrying transient git failures.
///
/// The distinction matters because the fallback is not benign: two clones of
/// one repo that key differently (one by commit, one by path) each claim
/// artifact ownership and write the shared cache concurrently. A git
/// invocation that fails under load (spawn failure, resource exhaustion) must
/// therefore be retried, and callers that need stable identity can refuse path
/// fallback when the result is still ambiguous after retry.
fn repo_root_commit_with_retry(project_root: &Path) -> RootCommitResolution {
    repo_root_commit_with_probe(project_root, || {
        repo_root_commit_with_retry_profiled(project_root, &mut ArtifactCacheKeyTimings::default())
    })
}

fn repo_root_commit_with_probe(
    project_root: &Path,
    probe: impl FnOnce() -> RootCommitResolution,
) -> RootCommitResolution {
    // Callers on this path (the call-graph store refreshes after every edit,
    // inspect scans, scoped keys) re-derive the key for the same root many
    // times a minute. Reuse the answer while the repository files that decide
    // HEAD and its history view are unchanged.
    let stamp = RootCommitStamp::capture(project_root);
    if let Some(stamp) = stamp.as_ref() {
        if let Some(cached) = cached_root_commit(project_root, stamp) {
            return RootCommitResolution::Commit(cached);
        }
    }
    let resolution = probe();
    // A checkout or history-view change during the probe can pair the result
    // with an unrelated stamp. Recheck before caching, but return the probe's
    // result even when the repository moved. A repository without a commit
    // must also be probed again once its first commit lands.
    if let (Some(stamp), RootCommitResolution::Commit(commit)) = (stamp, &resolution) {
        if RootCommitStamp::capture(project_root).as_ref() == Some(&stamp) {
            root_commit_cache()
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .insert(project_root.to_path_buf(), (stamp, commit.clone()));
        }
    }
    resolution
}

/// Root-commit identities by project root, each with the repository state it
/// was read under.
fn root_commit_cache() -> &'static Mutex<HashMap<PathBuf, (RootCommitStamp, String)>> {
    static CACHE: OnceLock<Mutex<HashMap<PathBuf, (RootCommitStamp, String)>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

fn cached_root_commit(project_root: &Path, stamp: &RootCommitStamp) -> Option<String> {
    #[cfg(test)]
    if GIT_ROOT_COMMIT_PROBE_OVERRIDE
        .get_or_init(|| Mutex::new(None))
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .is_some()
    {
        // Tests that script the probe expect every lookup to reach it.
        return None;
    }
    root_commit_cache()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(project_root)
        .filter(|(cached, _)| cached == stamp)
        .map(|(_, commit)| commit.clone())
}

/// Cache only the digest, not Git configuration contents. Background identity
/// checks watch directory/HEAD inode and time metadata as well as history inputs.
/// An unmodeled history view has no reusable stamp, even if those paths are stable.
pub(crate) fn repository_identity_stamp(root: &Path) -> Option<String> {
    let history = RootCommitStamp::capture(root)?;
    let marker = crate::root_identity::nearest_git_marker(root)
        .map(|directory| directory.map(|directory| directory.join(".git")));
    let mut paths = Vec::new();
    if let Ok(Some(marker)) = &marker {
        paths.push(marker.clone());
        let git_dir = if fs::metadata(marker).is_ok_and(|metadata| metadata.is_dir()) {
            marker.clone()
        } else {
            fs::read_to_string(marker)
                .ok()
                .and_then(|text| {
                    text.trim_end_matches(['\r', '\n'])
                        .strip_prefix("gitdir:")
                        .map(|pointer| marker.parent().unwrap().join(pointer.trim_start()))
                })
                .unwrap_or_else(|| marker.clone())
        };
        let git_dir = crate::root_identity::canonical_root(&git_dir);
        if !paths.contains(&git_dir) {
            paths.push(git_dir.clone());
        }
        paths.push(crate::root_identity::canonical_root(&git_dir.join("HEAD")));
        paths.push(git_dir.join("commondir"));
        if let Ok(common) = fs::read_to_string(git_dir.join("commondir")) {
            paths.push(crate::root_identity::canonical_root(
                &git_dir.join(common.trim_end_matches(['\r', '\n'])),
            ));
        }
    }
    let watched: Vec<_> = paths
        .iter()
        .map(|path| (path, RootCommitFileIdentity::of(path, true)))
        .collect();
    let git_env: BTreeMap<_, _> = std::env::vars_os()
        .filter(|(name, _)| name.to_str().is_some_and(|name| name.starts_with("GIT_")))
        .collect();
    Some(
        blake3::hash(format!("{marker:?}|{watched:?}|{history:?}|{git_env:?}").as_bytes())
            .to_hex()
            .to_string(),
    )
}

/// The repository state that decides `git rev-list --max-parents=0 HEAD`.
///
/// History is immutable, so the root set changes only when HEAD names another
/// commit or the history view changes. HEAD, its loose branch ref, the local
/// config, and loose replace-ref contents are kept by content; packed refs,
/// shallow boundaries, grafts and the replace-ref directory use file identity.
/// Config includes and unmodeled Git environment overrides make `capture`
/// decline caching, so Git is asked on every lookup instead of serving a stale
/// root.
#[derive(Clone, Debug, PartialEq, Eq)]
struct RootCommitStamp {
    git_marker: Option<RootCommitFileIdentity>,
    git_marker_text: Option<Vec<u8>>,
    commondir_text: Option<Vec<u8>>,
    head: Vec<u8>,
    head_ref: Option<Vec<u8>>,
    config_contents: Vec<Option<Vec<u8>>>,
    replace_ref_contents: Vec<(PathBuf, Vec<u8>)>,
    watched: Vec<Option<RootCommitFileIdentity>>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct RootCommitFileIdentity {
    #[cfg(unix)]
    inode: (u64, u64),
    /// Volume serial number and file index: a file renamed over another is a
    /// different file, even when its size and times match because both were
    /// written within one tick of the Windows file clock (about 15.6 ms).
    /// Unix keeps the device and inode in `inode` above instead, so the
    /// digest `repository_identity_stamp` hands to root identity records
    /// (built from this type's `Debug` text) is unchanged there.
    #[cfg(not(unix))]
    identity: crate::file_identity::FileIdentity,
    /// Size, modification and status-change times. Left out for a directory
    /// whose entries change on ordinary git use (the `.git` directory gains
    /// and renames `index.lock` on every status refresh); only its
    /// replacement matters there.
    contents: Option<(u64, Option<SystemTime>, i64, i64)>,
}

impl RootCommitFileIdentity {
    fn of(path: &Path, directory_contents: bool) -> Option<Option<Self>> {
        match crate::file_identity::symlink_metadata(path) {
            Ok((metadata, identity)) => {
                // Link identity does not cover changes to the target's history
                // data, so leave linked files and directories uncached.
                if metadata.file_type().is_symlink() {
                    return None;
                }
                #[cfg(unix)]
                use std::os::unix::fs::MetadataExt;
                #[cfg(unix)]
                let changed = (metadata.ctime(), metadata.ctime_nsec());
                // Unix reads the same device and inode from `metadata` below.
                #[cfg(unix)]
                let _ = identity;
                #[cfg(not(unix))]
                let changed = (0, 0);
                Some(Some(Self {
                    #[cfg(unix)]
                    inode: (metadata.dev(), metadata.ino()),
                    #[cfg(not(unix))]
                    identity,
                    contents: (!metadata.is_dir() || directory_contents).then(|| {
                        (
                            metadata.len(),
                            metadata.modified().ok(),
                            changed.0,
                            changed.1,
                        )
                    }),
                }))
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Some(None),
            Err(_) => None,
        }
    }
}

impl RootCommitStamp {
    /// Environment variables that move git's repository, work tree or
    /// history view away from the on-disk layout this stamp reads.
    const UNMODELED_ENV: &'static [&'static str] = &[
        "GIT_DIR",
        "GIT_COMMON_DIR",
        "GIT_WORK_TREE",
        "GIT_CEILING_DIRECTORIES",
        "GIT_DISCOVERY_ACROSS_FILESYSTEM",
        "GIT_OBJECT_DIRECTORY",
        "GIT_REPLACE_REF_BASE",
        "GIT_NO_REPLACE_OBJECTS",
        "GIT_GRAFT_FILE",
        "GIT_SHALLOW_FILE",
    ];

    fn capture(project_root: &Path) -> Option<Self> {
        if Self::UNMODELED_ENV
            .iter()
            .any(|key| std::env::var_os(key).is_some())
        {
            return None;
        }
        // Git discovers the repository from the nearest ancestor holding a
        // `.git` entry: a directory in a home checkout, a `gitdir:` file in a
        // linked worktree or submodule.
        let root = canonicalize_or_normalize(project_root);
        let marker = root
            .ancestors()
            .map(|directory| directory.join(".git"))
            .find(|marker| fs::symlink_metadata(marker).is_ok())?;
        let marker_metadata = fs::symlink_metadata(&marker).ok()?;
        let (git_dir, git_marker_text) = if marker_metadata.is_dir() {
            (marker.clone(), None)
        } else if marker_metadata.is_file() {
            let text = fs::read(&marker).ok()?;
            let pointer = std::str::from_utf8(&text)
                .ok()?
                .trim()
                .strip_prefix("gitdir:")?
                .trim();
            (marker.parent()?.join(pointer), Some(text))
        } else {
            return None;
        };
        let commondir_file = git_dir.join("commondir");
        let (common_dir, commondir_text) = match fs::read(&commondir_file) {
            Ok(text) => {
                let pointer = std::str::from_utf8(&text).ok()?.trim();
                (git_dir.join(pointer), Some(text))
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => (git_dir.clone(), None),
            Err(_) => return None,
        };
        let head = fs::read(git_dir.join("HEAD")).ok()?;
        // The reftable ref backend keeps refs outside the files read here.
        if fs::symlink_metadata(common_dir.join("reftable")).is_ok() {
            return None;
        }
        let head_ref = match std::str::from_utf8(&head).ok()?.trim().strip_prefix("ref:") {
            Some(name) => {
                let name = name.trim();
                // Only a branch, which lives under the common directory;
                // per-worktree and other refs are left to git.
                if !name.starts_with("refs/heads/")
                    || name.split('/').any(|part| part.is_empty() || part == "..")
                {
                    return None;
                }
                match fs::read(common_dir.join(name)) {
                    // A symbolic branch can change its target without its
                    // own bytes changing, so the stamp could not see it.
                    Ok(target) if target.starts_with(b"ref:") => return None,
                    Ok(target) => Some(target),
                    // A packed ref: the packed-refs identity below covers it.
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
                    Err(_) => return None,
                }
            }
            None => None,
        };
        let mut config_paths = vec![common_dir.join("config")];
        let worktree_config = git_dir.join("config.worktree");
        if !config_paths.contains(&worktree_config) {
            config_paths.push(worktree_config);
        }
        let mut config_contents = Vec::with_capacity(config_paths.len());
        let mut watched = Vec::with_capacity(config_paths.len() + 4);
        for path in config_paths {
            let identity = RootCommitFileIdentity::of(&path, false)?;
            let contents = match fs::read(&path) {
                Ok(contents) => Some(contents),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
                Err(_) => return None,
            };
            // Git can include configuration from outside the repository. Its
            // contents would affect replace-ref handling without being covered
            // by this stamp, so conservatively disable caching for includes.
            if contents
                .as_deref()
                .is_some_and(config_may_include_external_files)
            {
                return None;
            }
            config_contents.push(contents);
            watched.push(identity);
        }
        let replace_dir = common_dir.join("refs").join("replace");
        let replace_ref_contents = read_loose_replace_refs(&replace_dir)?;
        for path in [
            common_dir.join("packed-refs"),
            common_dir.join("shallow"),
            common_dir.join("info").join("grafts"),
            replace_dir,
        ] {
            // The directory catches additions and removals; each loose ref's
            // bytes above catch retargeting an existing replacement.
            watched.push(RootCommitFileIdentity::of(&path, true)?);
        }
        Some(Self {
            git_marker: RootCommitFileIdentity::of(&marker, false)?,
            git_marker_text,
            commondir_text,
            head,
            head_ref,
            config_contents,
            replace_ref_contents,
            watched,
        })
    }
}

fn config_may_include_external_files(contents: &[u8]) -> bool {
    // Config section names are case-insensitive. Matching this token anywhere
    // may reject harmless text in comments or values, but cannot miss an
    // include directive whose source would not otherwise be stamped.
    contents
        .windows(b"include".len())
        .any(|window| window.eq_ignore_ascii_case(b"include"))
}

fn read_loose_replace_refs(directory: &Path) -> Option<Vec<(PathBuf, Vec<u8>)>> {
    match fs::symlink_metadata(directory) {
        Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Some(Vec::new()),
        _ => return None,
    }

    let mut replace_refs = Vec::new();
    for entry in fs::read_dir(directory).ok()? {
        let entry = entry.ok()?;
        let path = entry.path();
        let metadata = fs::symlink_metadata(&path).ok()?;
        if !metadata.is_file() || metadata.file_type().is_symlink() {
            return None;
        }
        replace_refs.push((PathBuf::from(entry.file_name()), fs::read(path).ok()?));
    }
    replace_refs.sort_unstable_by(|left, right| left.0.cmp(&right.0));
    Some(replace_refs)
}

#[cfg(test)]
thread_local! {
    /// Real root-commit git runs made by this thread, by project root.
    static ROOT_COMMIT_GIT_RUNS: std::cell::RefCell<HashMap<PathBuf, usize>> =
        std::cell::RefCell::new(HashMap::new());
}

#[cfg(test)]
pub(crate) fn root_commit_git_runs_for_test(project_root: &Path) -> usize {
    ROOT_COMMIT_GIT_RUNS.with(|runs| runs.borrow().get(project_root).copied().unwrap_or(0))
}

fn repo_root_commit_with_retry_profiled(
    project_root: &Path,
    timings: &mut ArtifactCacheKeyTimings,
) -> RootCommitResolution {
    for attempt in 0..3u32 {
        if root_commit_probe_cancelled() {
            return RootCommitResolution::Cancelled;
        }
        let probe = git_root_commit_once(project_root, timings);
        if root_commit_probe_cancelled() {
            return RootCommitResolution::Cancelled;
        }
        match probe {
            RootCommitProbe::Commit(commit) => return RootCommitResolution::Commit(commit),
            RootCommitProbe::NotARepo => return RootCommitResolution::NotARepo,
            RootCommitProbe::NoCommit => return RootCommitResolution::NotARepo,
            RootCommitProbe::Transient(detail) => {
                if attempt == 2 {
                    return RootCommitResolution::Failed(detail);
                }
                if root_commit_probe_cancelled() {
                    return RootCommitResolution::Cancelled;
                }
                time_cache_key_work(&mut timings.retry_wait, || {
                    std::thread::sleep(std::time::Duration::from_millis(50 * (attempt as u64 + 1)));
                });
                if root_commit_probe_cancelled() {
                    return RootCommitResolution::Cancelled;
                }
            }
        }
    }
    RootCommitResolution::Failed("git root-commit probe retry loop exhausted".to_string())
}

fn root_commit_probe_cancelled() -> bool {
    crate::executor::current_job_cancellation()
        .is_some_and(|token| token.cancel_requested_before_commit())
}

enum RootCommitResolution {
    Commit(String),
    NotARepo,
    Failed(String),
    Cancelled,
}

enum RootCommitProbe {
    Commit(String),
    /// Deterministic: not a git work tree.
    NotARepo,
    /// Deterministic but still git-like: a repository exists but has no commit identity yet.
    NoCommit,
    /// Ambiguous failure (spawn error, killed, unexpected git error): retry.
    Transient(String),
}

fn git_root_commit_once(
    project_root: &Path,
    timings: &mut ArtifactCacheKeyTimings,
) -> RootCommitProbe {
    #[cfg(test)]
    if let Some(override_probe) = GIT_ROOT_COMMIT_PROBE_OVERRIDE
        .get_or_init(|| Mutex::new(None))
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone()
    {
        if let Some(result) = override_probe(project_root) {
            return result;
        }
    }

    git_root_commit_once_real(project_root, timings)
}

/// Canonicalize the root set before it becomes an artifact-cache identity.
/// Grafted-history repositories can have multiple roots, and Git traversal
/// order may change after repacks or commit-graph regeneration.
fn canonicalize_root_commit_output(stdout: &[u8]) -> RootCommitProbe {
    let decoded = String::from_utf8_lossy(stdout);
    let mut roots: Vec<&str> = decoded
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect();
    roots.sort_unstable();
    roots.dedup();

    if roots.is_empty() {
        RootCommitProbe::NoCommit
    } else {
        RootCommitProbe::Commit(roots.join("\n"))
    }
}

fn git_root_commit_once_real(
    project_root: &Path,
    timings: &mut ArtifactCacheKeyTimings,
) -> RootCommitProbe {
    // Keep the availability timing, but preserve uncertainty: a disabled Git
    // cannot justify switching a repository's stored data to path identity.
    if !time_cache_key_work(&mut timings.git_usable, crate::developer_tools::git_usable) {
        return RootCommitProbe::Transient(
            crate::developer_tools::MISSING_DEVELOPER_TOOLS_REASON.to_string(),
        );
    }
    #[cfg(test)]
    ROOT_COMMIT_GIT_RUNS.with(|runs| {
        *runs
            .borrow_mut()
            .entry(project_root.to_path_buf())
            .or_default() += 1;
    });
    // The rev_list timing also covers the extra Git checks that verify an
    // unborn branch, since they are part of deciding this probe's answer, and
    // git_spawns counts every Git process the probe actually started.
    let mut spawned = 0;
    let result = time_cache_key_work(&mut timings.rev_list, || {
        crate::git_probe::root_commits(project_root, &mut spawned)
    });
    timings.git_spawns += spawned;
    match result {
        Ok(stdout) => canonicalize_root_commit_output(&stdout),
        Err(crate::git_probe::ProbeError::Spawn(error)) => {
            RootCommitProbe::Transient(format!("spawn failed: {error}"))
        }
        Err(crate::git_probe::ProbeError::Failed { kind, output }) => match kind {
            crate::git_probe::FailureKind::NotRepository => RootCommitProbe::NotARepo,
            crate::git_probe::FailureKind::NoCommit => RootCommitProbe::NoCommit,
            crate::git_probe::FailureKind::Other => {
                let stderr = String::from_utf8_lossy(&output.stderr);
                RootCommitProbe::Transient(format!(
                    "exit {:?}: {}",
                    output.status.code(),
                    stderr.trim().chars().take(200).collect::<String>()
                ))
            }
        },
    }
}

#[cfg(test)]
pub(crate) struct GitRootCommitProbeOverrideGuard {
    previous: Option<RootCommitProbeOverride>,
}

#[cfg(test)]
impl Drop for GitRootCommitProbeOverrideGuard {
    fn drop(&mut self) {
        set_git_root_commit_probe_override_for_test(self.previous.take());
    }
}

#[cfg(test)]
pub(crate) fn git_root_commit_probe_override_lock_for_test() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(test)]
pub(crate) fn force_git_root_commit_probe_transient_for_paths_for_test(
    roots: Vec<PathBuf>,
    detail: impl Into<String>,
) -> GitRootCommitProbeOverrideGuard {
    let detail = Arc::new(detail.into());
    install_git_root_commit_probe_override_for_test(move |project_root| {
        roots
            .iter()
            .any(|root| root == project_root)
            .then(|| RootCommitProbe::Transient((*detail).clone()))
    })
}

#[cfg(test)]
/// Force the git root-commit probe to return Transient for the given roots,
/// counting the attempts it makes and holding the first attempt in flight
/// until `release` delivers a message or its sender is dropped.
///
/// A transient result sends the probe round the retry ladder again, so a
/// cancelled configure must stop making attempts. Holding the first attempt
/// lets a test cancel while that attempt is known to be in flight, so the
/// attempt count before and after the cancel is fixed by the order of events
/// rather than by how fast the machine runs. Dropping the sender also releases
/// the hold, so a test that panics never leaves the probing thread parked.
pub(crate) fn force_git_root_commit_probe_held_transient_for_paths_for_test(
    roots: Vec<PathBuf>,
    attempts: Arc<AtomicUsize>,
    release: std::sync::mpsc::Receiver<()>,
) -> GitRootCommitProbeOverrideGuard {
    let release = Mutex::new(Some(release));
    install_git_root_commit_probe_override_for_test(move |project_root| {
        roots.iter().any(|root| root == project_root).then(|| {
            attempts.fetch_add(1, Ordering::SeqCst);
            let first_attempt_hold = release
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .take();
            if let Some(release) = first_attempt_hold {
                // Either outcome ends the hold: a message, or a dropped sender.
                let _ = release.recv();
            }
            RootCommitProbe::Transient("stubbed held git probe".to_string())
        })
    })
}

#[cfg(test)]
pub(crate) fn force_git_root_commit_probe_commits_for_test(
    commits_by_root: BTreeMap<PathBuf, String>,
) -> GitRootCommitProbeOverrideGuard {
    install_git_root_commit_probe_override_for_test(move |project_root| {
        commits_by_root
            .get(project_root)
            .cloned()
            .map(RootCommitProbe::Commit)
    })
}

#[cfg(test)]
fn install_git_root_commit_probe_override_for_test(
    override_probe: impl Fn(&Path) -> Option<RootCommitProbe> + Send + Sync + 'static,
) -> GitRootCommitProbeOverrideGuard {
    let previous = set_git_root_commit_probe_override_for_test(Some(Arc::new(override_probe)));
    GitRootCommitProbeOverrideGuard { previous }
}

#[cfg(test)]
fn set_git_root_commit_probe_override_for_test(
    override_probe: Option<RootCommitProbeOverride>,
) -> Option<RootCommitProbeOverride> {
    let mut slot = GIT_ROOT_COMMIT_PROBE_OVERRIDE
        .get_or_init(|| Mutex::new(None))
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    std::mem::replace(&mut *slot, override_probe)
}

/// Fingerprint corpus-shaping ignore rules that are not represented by git HEAD.
///
/// The search cache stores this value next to the file mtimes. If `.gitignore`,
/// `.aftignore`, or `.git/info/exclude` changes while AFT is not running, a
/// matching HEAD + matching file mtimes is not enough to safely reuse the old
/// cache: files that are now ignored may still be indexed. Hashing the ignore
/// files themselves makes cold-start cache reuse agree with the current walker.
pub fn ignore_rules_fingerprint(project_root: &Path) -> String {
    ignore_rules_fingerprint_cancellable(project_root, &|| true)
        .expect("uncancellable ignore fingerprint")
}

fn ignore_rules_fingerprint_cancellable(
    project_root: &Path,
    keep_going: &dyn Fn() -> bool,
) -> Option<String> {
    use sha2::{Digest, Sha256};

    let root = canonicalize_or_normalize(project_root);
    let mut files = Vec::new();
    if !collect_ignore_rule_files(&root, &mut files, keep_going) {
        return None;
    }
    if let Some(global_ignore) = ignore::gitignore::gitconfig_excludes_path() {
        if global_ignore.is_file() {
            files.push(global_ignore);
        }
    }
    // Each entry pairs the name the file is hashed under with where it is
    // read from. The two differ only for the repository's `info/exclude`.
    let mut files: Vec<(PathBuf, PathBuf)> =
        files.into_iter().map(|path| (path.clone(), path)).collect();
    let info_exclude = git_info_exclude_path(&root);
    if info_exclude.is_file() {
        // `info/exclude` lives in the git directory shared by the home
        // checkout and all of its linked worktrees: inside the home checkout's
        // root but outside every worktree's. Hashing its real location would
        // make identical rules fingerprint differently per checkout, so it is
        // always hashed as `<root>/.git/info/exclude`. That is exactly the
        // name the home checkout's top level has always used, which keeps
        // fingerprints of existing top-level artifacts unchanged. Its content
        // is still read from the real file and still drives the fingerprint.
        files.push((root.join(".git").join("info").join("exclude"), info_exclude));
    }
    files.sort();
    files.dedup();

    let mut hasher = Sha256::new();
    hasher.update(b"aft-ignore-rules-v1\0");
    for (identity, path) in files {
        if !keep_going() {
            return None;
        }
        if let Some(relative) = cache_relative_path(&root, &identity) {
            hasher.update(relative.to_string_lossy().as_bytes());
        } else {
            hasher.update(identity.to_string_lossy().as_bytes());
        }
        hasher.update(b"\0");
        match fs::read(&path) {
            Ok(bytes) => hasher.update(&bytes),
            Err(error) => hasher.update(format!("read-error:{error}").as_bytes()),
        }
        hasher.update(b"\0");
    }

    Some(format!("{:x}", hasher.finalize()))
}

fn git_info_exclude_path(root: &Path) -> PathBuf {
    run_git(
        root,
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
    )
    .map(PathBuf::from)
    .unwrap_or_else(|| root.join(".git"))
    .join("info")
    .join("exclude")
}

fn collect_ignore_rule_files(
    root: &Path,
    files: &mut Vec<PathBuf>,
    keep_going: &dyn Fn() -> bool,
) -> bool {
    let mut builder = WalkBuilder::new(root);
    // Nested ignore discovery is a background recursive walk; a disappearing
    // mount must not turn ReadDir::drop's ENXIO into a daemon abort.
    builder.same_file_system(true).hidden(false);
    // Same rules as the project walk, so rule files inside ignored trees
    // (which cannot change what is indexed) are not discovered.
    crate::context::apply_project_ignore_rules(&mut builder, root).filter_entry(|entry| {
        let name = entry.file_name().to_string_lossy();
        if entry.file_type().map_or(false, |ft| ft.is_dir()) {
            return !matches!(
                name.as_ref(),
                ".git"
                    | "node_modules"
                    | "target"
                    | "venv"
                    | ".venv"
                    | "__pycache__"
                    | ".tox"
                    | "dist"
                    | "build"
            );
        }
        true
    });

    let mut walker = builder.build();
    loop {
        if !keep_going() {
            return false;
        }
        let Some(entry) = walker.next() else {
            break;
        };
        let Ok(entry) = entry else {
            continue;
        };
        if !entry
            .file_type()
            .map_or(false, |file_type| file_type.is_file())
        {
            continue;
        }
        let file_name = entry.file_name();
        if file_name == ".gitignore" || file_name == ".aftignore" {
            files.push(entry.into_path());
        }
    }
    true
}

/// Count directories visited when discovering ignore rule files (for perf regression tests).
#[cfg(test)]
pub(crate) fn count_ignore_rule_discovery_dirs(root: &Path) -> usize {
    let mut dirs = 0usize;
    let mut builder = WalkBuilder::new(root);
    builder.same_file_system(true).hidden(false);
    crate::context::apply_project_ignore_rules(&mut builder, root);
    for entry in builder.build().filter_map(|entry| entry.ok()) {
        if entry.file_type().map_or(false, |ft| ft.is_dir()) {
            dirs += 1;
        }
    }
    dirs
}

/// Legacy stack-based discovery (pre ignore-walker fix); used only in perf tests.
#[cfg(test)]
pub(crate) fn count_ignore_rule_discovery_dirs_legacy_stack(root: &Path) -> usize {
    let mut stack = vec![root.to_path_buf()];
    let mut dirs = 0usize;
    while let Some(dir) = stack.pop() {
        dirs += 1;
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let file_name = entry.file_name();
            if file_name == ".gitignore" || file_name == ".aftignore" {
                continue;
            }
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            if !file_type.is_dir() || file_type.is_symlink() {
                continue;
            }
            if matches!(
                file_name.to_str().unwrap_or(""),
                ".git"
                    | "node_modules"
                    | "target"
                    | "venv"
                    | ".venv"
                    | "__pycache__"
                    | ".tox"
                    | "dist"
                    | "build"
            ) {
                continue;
            }
            stack.push(path);
        }
    }
    dirs
}

impl PathFilters {
    pub(crate) fn matches(&self, root: &Path, path: &Path) -> bool {
        if !crate::bash_background::registry::safe_task_entry(path) {
            return false;
        }
        if self.includes.is_none() && self.excludes.is_none() {
            return true;
        }
        #[cfg(test)]
        audit_record(|work| work.filter_keys += 1);
        // Compare normalized copies: the search root arrives non-verbatim while
        // indexed entries may carry the Windows `\\?\` form, and a raw
        // strip_prefix between the two forms fails, which read as "no match".
        let relative = to_glob_path(&relative_to_root(
            &normalize_path(root),
            &normalize_path(path),
        ));
        if self
            .includes
            .as_ref()
            .is_some_and(|includes| !includes.is_match(&relative))
        {
            return false;
        }
        if self
            .excludes
            .as_ref()
            .is_some_and(|excludes| excludes.is_match(&relative))
        {
            return false;
        }
        true
    }
}

fn canonicalize_for_search_membership(path: &Path) -> PathBuf {
    // Indexed files and requested scope roots meet in containment checks. Bare
    // `fs::canonicalize` yields a Windows verbatim (`\\?\`) path, while the
    // lexical fallback does not, so the two success/failure forms would silently
    // miss each other without this shared non-verbatim normalizer.
    crate::bounded_io::run(path, None, |path| {
        Ok(crate::inspect::job::canonicalize_normalized(&path))
    })
    .unwrap_or_else(|_| crate::inspect::job::normalize_path(path))
}

fn canonicalize_or_normalize(path: &Path) -> PathBuf {
    crate::bounded_io::canonicalize(path).unwrap_or_else(|_| normalize_path(path))
}

fn resolve_match_path(project_root: &Path, path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        project_root.join(path)
    }
}

fn path_modified_time(path: &Path) -> Option<SystemTime> {
    #[cfg(test)]
    audit_record(|work| work.sort_stats += 1);
    #[cfg(test)]
    cache_freshness::record_metadata_call(path);
    crate::bounded_io::metadata(path)
        .and_then(|metadata| metadata.modified())
        .ok()
}

fn normalized_display_sort_key(project_root: Option<&Path>, path: &Path) -> String {
    let display_path = project_root
        .and_then(|root| path.strip_prefix(root).ok())
        .unwrap_or(path);
    to_glob_path(display_path)
}

fn normalize_path(path: &Path) -> PathBuf {
    crate::path_identity::lexical_normalize(path)
}

fn canonicalize_existing_or_deleted_path_with_memo(
    path: &Path,
    canonical_parents: &mut ParentCanonicalizationMemo,
) -> PathBuf {
    let mut canonicalize = |path: &Path| fs::canonicalize(path);
    canonicalize_existing_or_deleted_path_with_memo_and_canonicalizer(
        path,
        canonical_parents,
        &mut canonicalize,
    )
}

fn canonicalize_existing_or_deleted_path_with_memo_and_canonicalizer<F>(
    path: &Path,
    canonical_parents: &mut ParentCanonicalizationMemo,
    canonicalize: &mut F,
) -> PathBuf
where
    F: FnMut(&Path) -> std::io::Result<PathBuf>,
{
    let Some(parent) = path.parent() else {
        return path.to_path_buf();
    };
    let Some(file_name) = path.file_name() else {
        return path.to_path_buf();
    };

    // A file symlink needs full resolution before it can be matched to a stored
    // key. Ordinary files and known-missing entries are determined by their
    // parent, which is shared by many records during a verification pass.
    if fs::symlink_metadata(path).is_ok_and(|metadata| metadata.file_type().is_symlink()) {
        if let Ok(canonical) = canonicalize(path) {
            return canonical;
        }
    }

    canonical_parents
        .canonicalize_parent_with(parent, canonicalize)
        .map(|canonical_parent| canonical_parent.join(file_name))
        .unwrap_or_else(|| path.to_path_buf())
}

/// Admit the union of Git-diff paths and disk-only changes before reading content.
/// Keep the stat results for verification and indexing so neither pass re-reads files.
fn verify_file_mtimes_with_budget(
    index: &mut SearchIndex,
    verify_strategy: cache_freshness::VerifyStrategy,
    mut changed_paths: HashSet<PathBuf>,
    limit: usize,
) -> Option<bool> {
    let current_files = walk_project_files_from_inner(
        &index.project_root,
        &index.project_root,
        &PathFilters::default(),
        None,
        false,
    )
    .expect("unbounded project walk cannot exceed a file limit");
    let current_file_set: HashSet<_> = current_files.iter().collect();
    let mut removed_paths = Vec::new();
    let mut to_stat = Vec::new();
    for (file_id, entry) in index.files.iter().enumerate() {
        if entry.path.as_os_str().is_empty() {
            continue;
        }
        if !current_file_set.contains(&entry.path) {
            removed_paths.push(entry.path.clone());
            changed_paths.insert(entry.path.clone());
        } else {
            to_stat.push((
                file_id,
                entry.path.clone(),
                FileFreshness {
                    mtime: entry.modified,
                    size: entry.size,
                    content_hash: entry.content_hash,
                },
            ));
        }
    }
    let new_paths: Vec<_> = current_files
        .iter()
        .filter(|path| !index.path_to_id.contains_key(*path))
        .cloned()
        .collect();
    changed_paths.extend(new_paths.iter().cloned());
    let stats = cache_freshness::stat_files_bounded(to_stat);
    for (_, path, cached, metadata) in &stats {
        if metadata.as_ref().is_none_or(|metadata| {
            metadata.len() != cached.size
                || metadata.modified().unwrap_or(UNIX_EPOCH) != cached.mtime
        }) {
            changed_paths.insert(path.clone());
        }
    }
    if changed_paths.len() > limit {
        return None;
    }

    enum VerifiedPath {
        ContentFresh(usize, SearchFileMetadata),
        Update(PreparedSearchUpdate),
    }

    // Strict verification also detects edits with restored size and mtime.
    // Keep its reads in the bounded pool even when the stat pass found no changes.
    let to_read = stats
        .into_iter()
        .filter(|(_, path, _, _)| {
            verify_strategy == cache_freshness::VerifyStrategy::Strict
                || changed_paths.contains(path)
        })
        .collect();
    let max_file_size = index.max_file_size;
    let verified = cache_freshness::verify_statted_files_bounded(
        to_read,
        |file_id, path, cached, metadata| {
            let file = match metadata {
                Some(metadata) => read_search_corpus_file_with_metadata(
                    &path,
                    max_file_size,
                    false,
                    search_file_metadata(&metadata),
                ),
                None => SearchCorpusEligibility::Skipped,
            };
            let file = match file {
                SearchCorpusEligibility::Eligible(file) => {
                    let content_hash = cache_freshness::hash_bytes(&file.bytes);
                    if content_hash == cached.content_hash {
                        return VerifiedPath::ContentFresh(file_id, file.metadata);
                    }
                    PreparedSearchPath::Indexed(PreparedIndexedFile {
                        metadata: file.metadata,
                        content_hash,
                        trigram_map: trigram_filter_map(&file.bytes, true),
                    })
                }
                SearchCorpusEligibility::Unindexed(metadata) => {
                    PreparedSearchPath::Unindexed(metadata)
                }
                SearchCorpusEligibility::Skipped => PreparedSearchPath::Skipped,
            };
            VerifiedPath::Update(PreparedSearchUpdate { path, file })
        },
    );

    let mut changed = false;
    let mut canonical_parents = ParentCanonicalizationMemo::default();
    for path in removed_paths {
        index.remove_file_with_canonicalization_memo(&path, &mut canonical_parents);
        changed = true;
    }
    for result in verified {
        match result {
            VerifiedPath::ContentFresh(file_id, metadata) => {
                let entry = &index.files[file_id];
                if metadata.modified != entry.modified || metadata.size != entry.size {
                    let entry = &mut Arc::make_mut(&mut index.files)[file_id];
                    entry.modified = metadata.modified;
                    entry.size = metadata.size;
                    changed = true;
                }
            }
            VerifiedPath::Update(update) => {
                index.apply_search_update(update);
                changed = true;
            }
        }
    }
    for path in new_paths {
        index.update_file_with_canonicalization_memo(&path, &mut canonical_parents);
        changed = true;
    }
    Some(changed)
}

/// Verify stored file mtimes against disk. Re-index any files whose mtime changed
/// since the index was last written. Also detect new files and deleted files.
fn verify_file_mtimes(
    index: &mut SearchIndex,
    verify_strategy: cache_freshness::VerifyStrategy,
) -> bool {
    let filters = PathFilters::default();
    let current_files = walk_project_files_from_inner(
        &index.project_root,
        &index.project_root,
        &filters,
        None,
        false,
    )
    .expect("unbounded project walk cannot exceed a file limit");
    let current_file_set: HashSet<&PathBuf> = current_files.iter().collect();
    let mut stale_paths = Vec::new();
    let mut removed_paths = Vec::new();
    let mut changed = false;
    let mut canonical_parents = ParentCanonicalizationMemo::default();

    let mut to_verify = Vec::new();
    for (file_id, entry) in index.files.iter().enumerate() {
        if entry.path.as_os_str().is_empty() {
            continue; // tombstoned entry
        }
        if !current_file_set.contains(&entry.path) {
            removed_paths.push(entry.path.clone());
            continue;
        }
        to_verify.push((
            file_id,
            entry.path.clone(),
            FileFreshness {
                mtime: entry.modified,
                size: entry.size,
                content_hash: entry.content_hash,
            },
        ));
    }
    // Hash against immutable snapshots in the bounded pool before changing the index.
    let verdicts = cache_freshness::verify_files_bounded(to_verify, verify_strategy);
    for (file_id, path, verdict) in verdicts {
        match verdict {
            FreshnessVerdict::HotFresh => {}
            FreshnessVerdict::ContentFresh {
                new_mtime,
                new_size,
            } => {
                let entry = &mut Arc::make_mut(&mut index.files)[file_id];
                entry.modified = new_mtime;
                entry.size = new_size;
                changed = true;
            }
            FreshnessVerdict::Stale | FreshnessVerdict::Deleted => stale_paths.push(path),
        }
    }

    for path in &removed_paths {
        index.remove_file_with_canonicalization_memo(path, &mut canonical_parents);
        changed = true;
    }

    // Re-index stale files that are still in the current walk set. If an ignore
    // rule changed while AFT was down but the fingerprint missed it, this keeps
    // warm-cache verification from resurrecting now-ignored cached entries.
    for path in &stale_paths {
        if current_file_set.contains(path) {
            index.update_file_with_canonicalization_memo(path, &mut canonical_parents);
        } else {
            index.remove_file_with_canonicalization_memo(path, &mut canonical_parents);
        }
        changed = true;
    }

    // Detect new files not in the index
    for path in current_files {
        if !index.path_to_id.contains_key(&path) {
            index.update_file_with_canonicalization_memo(&path, &mut canonical_parents);
            changed = true;
        }
    }

    if !stale_paths.is_empty() {
        crate::slog_info!(
            "search index: refreshed {} stale file(s) from disk cache",
            stale_paths.len()
        );
    }
    changed
}

fn is_within_search_root(search_root: &Path, path: &Path) -> bool {
    crate::inspect::job::normalize_path(path)
        .starts_with(crate::inspect::job::normalize_path(search_root))
}

impl QueryBuild {
    fn into_query(self) -> RegexQuery {
        let mut query = RegexQuery::default();

        for run in self.and_runs {
            add_run_to_and_query(&mut query, &run);
        }

        for group in self.or_groups {
            let mut trigrams = BTreeSet::new();
            let mut filters = HashMap::new();
            for run in group {
                for (trigram, filter) in trigram_filters(&run) {
                    trigrams.insert(trigram);
                    merge_filter(filters.entry(trigram).or_default(), filter);
                }
            }
            if !trigrams.is_empty() {
                query.or_groups.push(trigrams.into_iter().collect());
                query.or_filters.push(filters);
            }
        }

        query
    }
}

fn build_query(hir: &Hir) -> QueryBuild {
    match hir.kind() {
        HirKind::Literal(literal) => {
            if literal.0.len() >= 3 {
                QueryBuild {
                    and_runs: vec![literal.0.to_vec()],
                    or_groups: Vec::new(),
                }
            } else {
                QueryBuild::default()
            }
        }
        HirKind::Capture(capture) => build_query(&capture.sub),
        HirKind::Concat(parts) => {
            let mut build = QueryBuild::default();
            for part in parts {
                let part_build = build_query(part);
                build.and_runs.extend(part_build.and_runs);
                build.or_groups.extend(part_build.or_groups);
            }
            build
        }
        HirKind::Alternation(parts) => {
            let mut group = Vec::new();
            for part in parts {
                let Some(mut choices) = guaranteed_run_choices(part) else {
                    return QueryBuild::default();
                };
                group.append(&mut choices);
            }
            if group.is_empty() {
                QueryBuild::default()
            } else {
                QueryBuild {
                    and_runs: Vec::new(),
                    or_groups: vec![group],
                }
            }
        }
        HirKind::Repetition(repetition) => {
            if repetition.min == 0 {
                QueryBuild::default()
            } else {
                build_query(&repetition.sub)
            }
        }
        HirKind::Empty | HirKind::Class(_) | HirKind::Look(_) => QueryBuild::default(),
    }
}

fn guaranteed_run_choices(hir: &Hir) -> Option<Vec<Vec<u8>>> {
    match hir.kind() {
        HirKind::Literal(literal) => {
            if literal.0.len() >= 3 {
                Some(vec![literal.0.to_vec()])
            } else {
                None
            }
        }
        HirKind::Capture(capture) => guaranteed_run_choices(&capture.sub),
        HirKind::Concat(parts) => {
            let mut runs = Vec::new();
            for part in parts {
                if let Some(mut part_runs) = guaranteed_run_choices(part) {
                    runs.append(&mut part_runs);
                }
            }
            if runs.is_empty() {
                None
            } else {
                Some(runs)
            }
        }
        HirKind::Alternation(parts) => {
            let mut runs = Vec::new();
            for part in parts {
                let Some(mut part_runs) = guaranteed_run_choices(part) else {
                    return None;
                };
                runs.append(&mut part_runs);
            }
            if runs.is_empty() {
                None
            } else {
                Some(runs)
            }
        }
        HirKind::Repetition(repetition) => {
            if repetition.min == 0 {
                None
            } else {
                guaranteed_run_choices(&repetition.sub)
            }
        }
        HirKind::Empty | HirKind::Class(_) | HirKind::Look(_) => None,
    }
}

fn add_run_to_and_query(query: &mut RegexQuery, run: &[u8]) {
    for (trigram, filter) in trigram_filters(run) {
        if !query.and_trigrams.contains(&trigram) {
            query.and_trigrams.push(trigram);
        }
        merge_filter(query.and_filters.entry(trigram).or_default(), filter);
    }
}

fn trigram_filters(run: &[u8]) -> Vec<(u32, PostingFilter)> {
    trigram_filter_map(run, false).into_iter().collect()
}

fn merge_filter(target: &mut PostingFilter, filter: PostingFilter) {
    target.next_mask |= filter.next_mask;
    target.loc_mask |= filter.loc_mask;
}

fn mask_for_next_char(next_char: u8) -> u8 {
    let bit = (normalize_char(next_char).wrapping_mul(31) & 7) as u32;
    1u8 << bit
}

fn mask_for_position(position: usize) -> u8 {
    1u8 << (position % 8)
}

fn build_globset(patterns: &[String]) -> Result<Option<GlobSet>, String> {
    if patterns.is_empty() {
        return Ok(None);
    }

    let mut builder = GlobSetBuilder::new();
    for pattern in patterns {
        let glob = Glob::new(pattern).map_err(|error| error.to_string())?;
        builder.add(glob);
    }
    builder.build().map(Some).map_err(|error| error.to_string())
}

fn read_u32<R: Read>(reader: &mut R) -> std::io::Result<u32> {
    let mut buffer = [0u8; 4];
    reader.read_exact(&mut buffer)?;
    Ok(u32::from_le_bytes(buffer))
}

fn read_u64<R: Read>(reader: &mut R) -> std::io::Result<u64> {
    let mut buffer = [0u8; 8];
    reader.read_exact(&mut buffer)?;
    Ok(u64::from_le_bytes(buffer))
}

fn write_u32<W: Write>(writer: &mut W, value: u32) -> std::io::Result<()> {
    writer.write_all(&value.to_le_bytes())
}

fn write_u64<W: Write>(writer: &mut W, value: u64) -> std::io::Result<()> {
    writer.write_all(&value.to_le_bytes())
}

fn verify_crc32_bytes_slice(bytes: &[u8]) -> std::io::Result<()> {
    let Some((body, stored)) = bytes.split_last_chunk::<4>() else {
        return Err(std::io::Error::other("search index checksum missing"));
    };
    let expected = u32::from_le_bytes(*stored);
    let actual = crc32fast::hash(body);
    if actual != expected {
        return Err(std::io::Error::other("search index checksum mismatch"));
    }
    Ok(())
}

fn remaining_bytes<R: Seek>(reader: &mut R, total_len: usize) -> Option<usize> {
    let pos = usize::try_from(reader.stream_position().ok()?).ok()?;
    total_len.checked_sub(pos)
}

fn run_git(root: &Path, args: &[&str]) -> Option<String> {
    const GIT_PROBE_TIMEOUT: Duration = Duration::from_secs(2);

    if !crate::developer_tools::git_usable() {
        return None;
    }
    let mut child = crate::effective_path::new_command("git")
        .build()
        .arg("-C")
        .arg(root)
        .args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .ok()?;
    let deadline = Instant::now() + GIT_PROBE_TIMEOUT;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(10)),
            Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    };
    if !status.success() {
        return None;
    }
    let mut stdout = Vec::new();
    child.stdout.take()?.read_to_end(&mut stdout).ok()?;
    let value = String::from_utf8(stdout).ok()?;
    let value = value.trim().to_string();
    (!value.is_empty()).then_some(value)
}

/// Run `git diff --name-status -z -M --relative from..to` from `root` and
/// return its raw output.
///
/// The output is read NUL-delimited on purpose. Without `-z`, git quotes any
/// name that holds non-ASCII bytes, a tab, a quote or a backslash
/// (`core.quotePath`), so `café.rs` arrives as `"caf\303\251.rs"`. That
/// string is not a real path: the edited file would be counted once under the
/// quoted name and again under its real name from the disk walk, and the
/// committed edit itself would never be matched to the cached file.
///
/// `--relative` matters when the search root is a subdirectory of the
/// repository, such as one package of a monorepo opened on its own. Git
/// otherwise names paths from the repository's top level (`packages/app/x.rs`)
/// and lists changes anywhere in the repository; joined onto the search root
/// those become paths that do not exist. With `--relative`, run from the root
/// through `-C`, git lists only changes under the root, relative to it. At the
/// top level of the repository it changes nothing.
fn read_git_diff(
    root: &Path,
    from: &str,
    to: &str,
    max_changed_paths: Option<usize>,
) -> Option<Vec<u8>> {
    if !crate::developer_tools::git_usable() {
        return None;
    }
    let diff_range = format!("{}..{}", from, to);
    let output = match crate::effective_path::new_command("git")
        .build()
        .arg("-C")
        .arg(root)
        .args([
            "diff",
            "--name-status",
            "-z",
            "-M",
            "--relative",
            &diff_range,
        ])
        .output()
    {
        Ok(output) => output,
        Err(_) => return None,
    };

    if !output.status.success() {
        return None;
    }

    let diff = output.stdout;
    if max_changed_paths.is_some_and(|limit| !git_diff_within_path_limit(&diff, limit)) {
        return None;
    }
    Some(diff)
}

/// One record of `git diff --name-status -z` output, borrowing its raw bytes.
struct GitNameStatusRecord<'a> {
    /// Status letters, such as `M`, `A`, `D` or `R100` for a rename.
    status: &'a [u8],
    /// One path, or the old path then the new path for a rename or copy.
    paths: Vec<&'a [u8]>,
}

/// Split `git diff --name-status -z` output into records.
///
/// In this format every field ends with a NUL byte: a status field, then one
/// path field, or two (old then new) when the status is a rename (`R`) or a
/// copy (`C`). Paths are never quoted or escaped. Returns `None` for output
/// that does not follow this shape, so callers fall back to a full refresh
/// instead of acting on a misread diff.
fn parse_git_name_status_z(diff: &[u8]) -> Option<Vec<GitNameStatusRecord<'_>>> {
    let body = match diff.split_last() {
        None => return Some(Vec::new()),
        Some((0, body)) => body,
        Some(_) => return None,
    };
    let mut fields = body.split(|byte| *byte == 0);
    let mut records = Vec::new();
    while let Some(status) = fields.next() {
        let path_count = match status.first()? {
            b'R' | b'C' => 2,
            _ => 1,
        };
        let paths = (0..path_count)
            .map(|_| fields.next().filter(|path| !path.is_empty()))
            .collect::<Option<Vec<_>>>()?;
        records.push(GitNameStatusRecord { status, paths });
    }
    Some(records)
}

/// Turn one raw path from git's `-z` output into a relative path.
///
/// Git writes the bytes it stores for the name. On Unix those bytes are the
/// file name itself; git for Windows stores and prints names as UTF-8, so a
/// name that is not UTF-8 there is reported as `None`.
#[cfg(unix)]
fn git_output_path(bytes: &[u8]) -> Option<PathBuf> {
    use std::os::unix::ffi::OsStrExt;
    Some(PathBuf::from(std::ffi::OsStr::from_bytes(bytes)))
}

#[cfg(not(unix))]
fn git_output_path(bytes: &[u8]) -> Option<PathBuf> {
    std::str::from_utf8(bytes).ok().map(PathBuf::from)
}

fn git_diff_paths(
    root: &Path,
    from: &str,
    to: &str,
    max_changed_paths: Option<usize>,
) -> Option<HashSet<PathBuf>> {
    let diff = read_git_diff(root, from, to, max_changed_paths)?;
    let records = parse_git_name_status_z(&diff)?;
    let mut canonical_parents = ParentCanonicalizationMemo::default();
    let mut paths = HashSet::new();
    for record in &records {
        for raw_path in &record.paths {
            let relative = git_output_path(raw_path)?;
            if let Some(path) =
                cached_path_under_root_with_memo(root, &relative, &mut canonical_parents)
            {
                paths.insert(path);
            }
        }
    }
    Some(paths)
}

fn apply_git_diff_updates(
    index: &mut SearchIndex,
    root: &Path,
    from: &str,
    to: &str,
    max_changed_paths: Option<usize>,
) -> bool {
    let Some(diff) = read_git_diff(root, from, to, max_changed_paths) else {
        return false;
    };
    let Some(records) = parse_git_name_status_z(&diff) else {
        return false;
    };
    // Resolve every path before touching the index, so a name that cannot be
    // decoded leaves the index as it was and the caller rebuilds instead.
    let Some(records) = records
        .iter()
        .map(|record| {
            let paths = record
                .paths
                .iter()
                .map(|raw_path| git_output_path(raw_path))
                .collect::<Option<Vec<_>>>()?;
            Some((record.status, paths))
        })
        .collect::<Option<Vec<_>>>()
    else {
        return false;
    };
    let mut canonical_parents = ParentCanonicalizationMemo::default();

    for (status, paths) in records {
        let resolved: Vec<Option<PathBuf>> = paths
            .iter()
            .map(|path| cached_path_under_root_with_memo(root, path, &mut canonical_parents))
            .collect();
        let path = match (status.first(), resolved.as_slice()) {
            (Some(b'R'), [Some(old_path), Some(new_path)]) => {
                index.remove_file_with_canonicalization_memo(old_path, &mut canonical_parents);
                index.update_file_with_canonicalization_memo(new_path, &mut canonical_parents);
                continue;
            }
            // A copy leaves its source unchanged; only the new path changed.
            (Some(b'C'), [_, Some(new_path)]) => new_path,
            (Some(b'R' | b'C'), _) => continue,
            (_, [Some(path)]) => path,
            _ => continue,
        };
        if status.first() == Some(&b'D') || !path.exists() {
            index.remove_file_with_canonicalization_memo(path, &mut canonical_parents);
        } else {
            index.update_file_with_canonicalization_memo(path, &mut canonical_parents);
        }
    }

    true
}

fn git_diff_within_path_limit(diff: &[u8], limit: usize) -> bool {
    // Decide before touching source files. Each record is a status field plus
    // one or two path fields, so more than twice the limit in fields already
    // means more than `limit` paths; otherwise count the paths exactly, which
    // includes both the old and the new path of a rename. Output that cannot
    // be parsed is never admitted.
    let fields = diff.iter().filter(|byte| **byte == 0).count();
    fields <= limit.saturating_mul(2)
        && parse_git_name_status_z(diff).is_some_and(|records| {
            records
                .iter()
                .map(|record| record.paths.len())
                .sum::<usize>()
                <= limit
        })
}

fn line_starts_bytes(content: &[u8]) -> Vec<usize> {
    let mut starts = vec![0usize];
    for (index, byte) in content.iter().copied().enumerate() {
        if byte == b'\n' {
            starts.push(index + 1);
        }
    }
    starts
}

#[cfg(test)]
thread_local! {
    /// Called on every `line_details_bytes` call on this thread, so tests can
    /// count line-detail work or act at a chosen point inside a scan.
    static LINE_DETAILS_PROBE: std::cell::RefCell<Option<Box<dyn FnMut()>>> =
        const { std::cell::RefCell::new(None) };
    static GREP_SCAN_CLOCK_ADVANCE: Cell<Duration> = const { Cell::new(Duration::ZERO) };
}

#[cfg(test)]
fn grep_scan_test_now() -> Instant {
    Instant::now() + GREP_SCAN_CLOCK_ADVANCE.with(Cell::get)
}

/// Line number, 1-based character column and printable text of the line
/// holding `offset`, plus the byte offset where the next line starts (the
/// end of `content` for the last line). The text is bounded the way grep
/// prints it, see [`bounded_grep_line_text_bytes`].
fn line_details_bytes(
    content: &[u8],
    line_starts: &[usize],
    offset: usize,
) -> (u32, u32, String, usize) {
    #[cfg(test)]
    LINE_DETAILS_PROBE.with(|probe| {
        if let Some(probe) = probe.borrow_mut().as_mut() {
            probe();
        }
    });

    let line_index = match line_starts.binary_search(&offset) {
        Ok(index) => index,
        Err(index) => index.saturating_sub(1),
    };
    let line_start = line_starts.get(line_index).copied().unwrap_or(0);
    // Every entry after the first in `line_starts` follows a newline, so the
    // next entry, when there is one, is one byte past this line's newline.
    let (line_end, next_line_start) = match line_starts.get(line_index + 1) {
        Some(&next) => (next - 1, next),
        None => (content.len(), content.len()),
    };
    let mut line_slice = &content[line_start..line_end];
    if line_slice.ends_with(b"\r") {
        line_slice = &line_slice[..line_slice.len() - 1];
    }
    let line_text = bounded_grep_line_text_bytes(line_slice);
    let column = String::from_utf8_lossy(&content[line_start..offset])
        .chars()
        .count() as u32
        + 1;
    (line_index as u32 + 1, column, line_text, next_line_start)
}

/// The text grep keeps for a matching line. A line of at most
/// `GREP_MAX_LINE_CHARS` characters is kept whole. A longer line is kept as
/// exactly what the grep renderer prints for it: its first
/// `GREP_MAX_LINE_CHARS` characters and the truncation marker. Every renderer
/// cuts at that length or shorter, so the printed output is the same as for
/// the whole line, while a match on a multi-megabyte minified line no longer
/// decodes and copies the whole line.
pub(crate) fn bounded_grep_line_text(line: &str) -> String {
    use crate::commands::grep::{GREP_LINE_TRUNCATED_MARKER, GREP_MAX_LINE_CHARS};
    match line.char_indices().nth(GREP_MAX_LINE_CHARS) {
        None => line.to_string(),
        Some((cut, _)) => format!("{}{}", &line[..cut], GREP_LINE_TRUNCATED_MARKER),
    }
}

/// [`bounded_grep_line_text`] for raw line bytes, decoded lossily as the
/// whole line would be. Only a prefix of the line is decoded: a UTF-8
/// character is at most 4 bytes and an invalid byte decodes to one
/// replacement character, so the first `(GREP_MAX_LINE_CHARS + 2) * 4` bytes
/// hold at least `GREP_MAX_LINE_CHARS + 1` characters that decode exactly as
/// in the whole line (only a sequence cut by the prefix end, at most its last
/// 3 bytes, can decode differently). A line longer than that prefix therefore
/// always has more than `GREP_MAX_LINE_CHARS` characters and is cut.
fn bounded_grep_line_text_bytes(line: &[u8]) -> String {
    const PREFIX_BYTES: usize = (crate::commands::grep::GREP_MAX_LINE_CHARS + 2) * 4;
    let prefix = &line[..line.len().min(PREFIX_BYTES)];
    bounded_grep_line_text(&String::from_utf8_lossy(prefix))
}

fn to_glob_path(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

// Thread-local operation counters keep concurrent tests from charging each
// other's filesystem work. They observe the call sites, not elapsed time.
#[cfg(test)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct AuditWork {
    pub cancel_probes: usize,
    pub corpus_opens: usize,
    pub sort_stats: usize,
    pub sort_canonicalizes: usize,
    pub watcher_global: usize,
    pub watcher_head: usize,
    pub filter_keys: usize,
    pub presence_checks: usize,
}

#[cfg(test)]
thread_local! {
    static AUDIT_WORK: std::cell::Cell<AuditWork> = std::cell::Cell::new(AuditWork::default());
    pub(crate) static AUDIT_IO_PROBE: std::cell::RefCell<Option<Box<dyn FnMut()>>> = const { std::cell::RefCell::new(None) };
    pub(crate) static AUDIT_CANCEL_ELAPSED: std::cell::Cell<Option<Duration>> = const { std::cell::Cell::new(None) };
}

#[cfg(test)]
pub(crate) struct HelperStateScope(AuditWork, Option<Box<dyn FnMut()>>, Option<Duration>);
#[cfg(test)]
impl Drop for HelperStateScope {
    fn drop(&mut self) {
        AUDIT_WORK.with(|work| work.set(self.0));
        AUDIT_IO_PROBE.with(|probe| *probe.borrow_mut() = self.1.take());
        AUDIT_CANCEL_ELAPSED.with(|elapsed| elapsed.set(self.2));
    }
}
#[cfg(test)]
pub(crate) fn helper_state_scope() -> HelperStateScope {
    HelperStateScope(
        AUDIT_WORK.with(|work| work.replace(AuditWork::default())),
        AUDIT_IO_PROBE.with(|probe| probe.replace(None)),
        AUDIT_CANCEL_ELAPSED.with(|elapsed| elapsed.replace(None)),
    )
}

#[cfg(test)]
pub(crate) fn audit_cancel_elapsed(started: Instant) -> Duration {
    AUDIT_CANCEL_ELAPSED.with(|slot| slot.get().unwrap_or_else(|| started.elapsed()))
}

#[cfg(test)]
pub(crate) fn audit_set_io_probe(probe: Option<Box<dyn FnMut()>>) {
    AUDIT_IO_PROBE.with(|slot| *slot.borrow_mut() = probe);
}

#[cfg(test)]
pub(crate) fn audit_record(record: impl FnOnce(&mut AuditWork)) {
    AUDIT_WORK.with(|slot| {
        let mut work = slot.get();
        record(&mut work);
        slot.set(work);
    });
}

#[cfg(test)]
pub(crate) fn audit_work_reset() {
    AUDIT_WORK.with(|slot| slot.set(AuditWork::default()));
}

#[cfg(test)]
pub(crate) fn audit_work() -> AuditWork {
    AUDIT_WORK.with(std::cell::Cell::get)
}

#[cfg(test)]
mod tests {
    struct IsolatedLegacyMemoIndexes {
        indexes: Arc<Mutex<super::LegacyMemoIndexes>>,
        previous: Option<Arc<Mutex<super::LegacyMemoIndexes>>>,
    }

    impl Drop for IsolatedLegacyMemoIndexes {
        fn drop(&mut self) {
            super::LEGACY_MEMO_INDEXES_OVERRIDE.with(|slot| {
                *slot.borrow_mut() = self.previous.take();
            });
        }
    }

    // Isolate cache-policy tests from unrelated parallel tests' storage
    // roots while exercising the same slot-selection and lookup code.
    fn isolated_legacy_memo_indexes() -> IsolatedLegacyMemoIndexes {
        let indexes = Arc::new(Mutex::new(super::LegacyMemoIndexes::default()));
        install_legacy_memo_indexes(indexes)
    }

    fn install_legacy_memo_indexes(
        indexes: Arc<Mutex<super::LegacyMemoIndexes>>,
    ) -> IsolatedLegacyMemoIndexes {
        let previous = super::LEGACY_MEMO_INDEXES_OVERRIDE
            .with(|slot| slot.borrow_mut().replace(indexes.clone()));
        IsolatedLegacyMemoIndexes { indexes, previous }
    }

    #[test]
    fn legacy_identity_memo_retains_four_storage_roots_by_recency() {
        let cache = isolated_legacy_memo_indexes();
        let fixture = tempfile::tempdir().unwrap();
        let roots: Vec<_> = (0..5)
            .map(|index| {
                let storage = fixture.path().join(format!("storage-{index}"));
                fs::create_dir(&storage).unwrap();
                let query = storage.join("checkout");
                let memo = BTreeMap::from([(
                    query.to_string_lossy().into_owned(),
                    legacy_memo_entry(&format!("commit-{index}")),
                )]);
                fs::write(
                    super::artifact_cache_key_memo_path(&storage),
                    serde_json::to_vec(&memo).unwrap(),
                )
                .unwrap();
                (storage, query)
            })
            .collect();
        super::LEGACY_MEMO_BUILDS.with(|count| count.set(0));
        for (storage, query) in &roots[..4] {
            super::legacy_identity_memo(query, &query.to_string_lossy(), storage).unwrap();
        }
        let (storage, query) = &roots[0];
        super::legacy_identity_memo(query, &query.to_string_lossy(), storage).unwrap();
        let (storage, query) = &roots[4];
        super::legacy_identity_memo(query, &query.to_string_lossy(), storage).unwrap();
        assert_eq!(super::LEGACY_IDENTITY_MEMO_MAX_STORAGE_ROOTS, 4);
        let indexes = cache.indexes.lock().unwrap();
        assert_eq!(
            indexes.roots.len(),
            4,
            "five touched storage roots must retain only four indexes"
        );
        assert!(
            !indexes
                .roots
                .iter()
                .any(|(path, _)| path == &super::artifact_cache_key_memo_path(&roots[1].0)),
            "the refreshed first root must survive instead of the least recently used second root"
        );
        drop(indexes);
        assert_eq!(super::LEGACY_MEMO_BUILDS.with(|count| count.get()), 5);
        let (storage, query) = &roots[1];
        let actual = legacy_memo_value(super::legacy_identity_memo(
            query,
            &query.to_string_lossy(),
            storage,
        ));
        let expected = legacy_memo_value(super::legacy_identity_memo_full_scan(
            query,
            &query.to_string_lossy(),
            storage,
        ));
        assert_eq!(actual, expected);
        assert_eq!(
            super::LEGACY_MEMO_BUILDS.with(|count| count.get()),
            6,
            "an evicted root must rebuild on its next lookup"
        );
        assert_eq!(cache.indexes.lock().unwrap().roots.len(), 4);
    }

    #[test]
    fn legacy_identity_memo_registry_touch_drops_expired_snapshot() {
        let cache = isolated_legacy_memo_indexes();
        let storage = tempfile::tempdir().unwrap();
        let query = storage.path().join("checkout");
        let raw = query.to_string_lossy().into_owned();
        let memo = BTreeMap::from([(raw.clone(), legacy_memo_entry("root-commit"))]);
        fs::write(
            super::artifact_cache_key_memo_path(storage.path()),
            serde_json::to_vec(&memo).unwrap(),
        )
        .unwrap();
        super::LEGACY_MEMO_BUILDS.with(|count| count.set(0));
        let first = legacy_memo_value(super::legacy_identity_memo(&query, &raw, storage.path()));
        let slot = super::legacy_identity_memo_slot(storage.path());
        slot.lock().unwrap().as_mut().unwrap().built_at =
            Instant::now() - super::LEGACY_IDENTITY_MEMO_MAX_AGE;
        let released = Arc::downgrade(&slot);
        drop(slot);
        super::legacy_identity_memo_slot(&storage.path().join("different-storage"));
        assert!(
            !cache
                .indexes
                .lock()
                .unwrap()
                .roots
                .iter()
                .any(|(path, _)| path == &super::artifact_cache_key_memo_path(storage.path())),
            "touching another root must remove an expired snapshot"
        );
        assert!(
            released.upgrade().is_none(),
            "the expired index must actually be released, not merely ignored"
        );
        assert_eq!(
            legacy_memo_value(super::legacy_identity_memo(&query, &raw, storage.path())),
            first
        );
        assert_eq!(super::LEGACY_MEMO_BUILDS.with(|count| count.get()), 2);
    }

    #[test]
    fn legacy_identity_memo_registry_touch_does_not_wait_for_busy_snapshot() {
        let cache = isolated_legacy_memo_indexes();
        let storage = tempfile::tempdir().unwrap();
        let query = storage.path().join("checkout");
        let raw = query.to_string_lossy().into_owned();
        let memo = BTreeMap::from([(raw.clone(), legacy_memo_entry("root-commit"))]);
        fs::write(
            super::artifact_cache_key_memo_path(storage.path()),
            serde_json::to_vec(&memo).unwrap(),
        )
        .unwrap();
        super::legacy_identity_memo(&query, &raw, storage.path()).unwrap();
        let slot = super::legacy_identity_memo_slot(storage.path());
        slot.lock().unwrap().as_mut().unwrap().built_at =
            Instant::now() - super::LEGACY_IDENTITY_MEMO_MAX_AGE;
        let held = slot.lock().unwrap();
        let shared = cache.indexes.clone();
        let other = storage.path().join("different-storage");
        let (finished, receiver) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            let _cache = install_legacy_memo_indexes(shared);
            super::legacy_identity_memo_slot(&other);
            finished.send(()).unwrap();
        });
        let result = receiver.recv_timeout(Duration::from_secs(2));
        drop(held);
        worker.join().unwrap();
        assert!(
            result.is_ok(),
            "registry cleanup waited for a snapshot's filesystem-work lock"
        );
        assert!(
            !cache
                .indexes
                .lock()
                .unwrap()
                .roots
                .iter()
                .any(|(path, _)| path == &super::artifact_cache_key_memo_path(storage.path())),
            "even a busy expired snapshot must lose the registry's reference"
        );
    }

    /// Measure retained allocator bytes in an isolated native invocation. JSON
    /// bytes are prepared before the first sample; parsing and resolved-path
    /// ownership are included, while temporary allocations freed by build are not.
    #[cfg(target_os = "macos")]
    #[test]
    #[ignore = "manual isolated native legacy memo retained-heap measurement"]
    fn legacy_identity_memo_index_retained_heap_probe() {
        fn heap() -> u64 {
            crate::memory::AllocatorObservation::measure()
                .snapshot
                .bytes_in_use
                .expect("macOS allocator in-use bytes")
        }
        let fixture = tempfile::tempdir().unwrap();
        let memo: BTreeMap<_, _> = (0..3000)
            .map(|index| {
                let parent = fixture
                    .path()
                    .join(format!("removed/checkouts/worker-{index:04}"));
                let padding =
                    "p".repeat(240_usize.saturating_sub(parent.to_string_lossy().len() + 9));
                let path = parent.join(padding).join("project");
                (
                    path.to_string_lossy().into_owned(),
                    super::ArtifactCacheKeyMemoEntry {
                        key: "0123456789abcdef".into(),
                        git_root_commit: "0123456789abcdef0123456789abcdef01234567".into(),
                        recorded_at_ms: crate::pins::now_ms(),
                    },
                )
            })
            .collect();
        let bytes = serde_json::to_vec(&memo).unwrap();
        drop(memo);
        assert!(
            (1_000_000..1_200_000).contains(&bytes.len()),
            "representative memo must be about one MB: {}",
            bytes.len()
        );
        heap();
        let before = heap();
        let entries = serde_json::from_slice(&bytes).unwrap();
        let index = super::LegacyMemoIndex::build(entries, None);
        std::hint::black_box(&index);
        let retained = heap();
        assert_eq!(index.entries.len(), 3000);
        assert_eq!(index.aliases.len(), 3000);
        drop(index);
        let released = heap();
        assert!(retained > before);
        assert!(
            released <= before + 128 * 1024,
            "dropping the measured index must release its retained heap"
        );
        eprintln!("legacy_memo_heap_probe entries=3000 json_bytes={} before_in_use={before} retained_in_use={retained} dropped_in_use={released} retained_delta={} released_delta={}", bytes.len(), retained - before, retained - released);
    }

    fn legacy_memo_entry(commit: &str) -> super::ArtifactCacheKeyMemoEntry {
        super::ArtifactCacheKeyMemoEntry {
            key: "0123456789abcdef".into(),
            git_root_commit: commit.into(),
            recorded_at_ms: 1,
        }
    }

    fn legacy_memo_value(
        result: std::io::Result<Option<super::ArtifactCacheKeyMemoEntry>>,
    ) -> Result<Option<(String, String, u64)>, (std::io::ErrorKind, String)> {
        result
            .map(|entry| {
                entry.map(|entry| (entry.key, entry.git_root_commit, entry.recorded_at_ms))
            })
            .map_err(|error| (error.kind(), error.to_string()))
    }

    #[test]
    fn legacy_identity_memo_rebuilds_on_same_size_same_time_replacement() {
        let _cache = isolated_legacy_memo_indexes();
        let storage = tempfile::tempdir().unwrap();
        let root = storage.path().join("checkout");
        let raw = root.to_string_lossy().into_owned();
        let mut entries = BTreeMap::from([(raw.clone(), legacy_memo_entry("old"))]);
        super::write_artifact_cache_key_memo_file(storage.path(), &entries).unwrap();
        assert_eq!(
            super::legacy_identity_memo(&root, &raw, storage.path())
                .unwrap()
                .unwrap()
                .git_root_commit,
            "old"
        );
        entries.get_mut(&raw).unwrap().git_root_commit = "new".into();
        let bytes = serde_json::to_vec_pretty(&entries).unwrap();
        crate::file_identity::replace_keeping_times(
            &super::artifact_cache_key_memo_path(storage.path()),
            &bytes,
        );
        assert_eq!(
            super::legacy_identity_memo(&root, &raw, storage.path())
                .unwrap()
                .unwrap()
                .git_root_commit,
            "new"
        );
    }

    #[test]
    fn legacy_identity_memo_own_write_advances_snapshot() {
        let _cache = isolated_legacy_memo_indexes();
        let storage = tempfile::tempdir().unwrap();
        let root = storage.path().join("checkout");
        let raw = root.to_string_lossy().into_owned();
        let mut entries = BTreeMap::from([(raw.clone(), legacy_memo_entry("old"))]);
        super::write_artifact_cache_key_memo_file(storage.path(), &entries).unwrap();
        super::LEGACY_MEMO_BUILDS.with(|count| count.set(0));
        super::LEGACY_MEMO_CANONICALIZATIONS.with(|count| count.set(0));
        super::legacy_identity_memo(&root, &raw, storage.path()).unwrap();
        let slot = super::legacy_identity_memo_slot(storage.path());
        let built_at = slot.lock().unwrap().as_ref().unwrap().built_at;
        entries.get_mut(&raw).unwrap().git_root_commit = "new".into();
        super::write_artifact_cache_key_memo_file(storage.path(), &entries).unwrap();
        assert_eq!(
            super::legacy_identity_memo(&root, &raw, storage.path())
                .unwrap()
                .unwrap()
                .git_root_commit,
            "new"
        );
        assert_eq!(super::LEGACY_MEMO_BUILDS.with(|count| count.get()), 1,
            "an own write must update the existing index rather than force the next root to rebuild it");
        assert_eq!(
            slot.lock().unwrap().as_ref().unwrap().built_at,
            built_at,
            "own writes must not extend the age of unchanged canonical spellings"
        );
        assert_eq!(
            super::LEGACY_MEMO_CANONICALIZATIONS.with(|count| count.get()),
            2
        );
        let added = storage.path().join("new-checkout");
        let added_raw = added.to_string_lossy().into_owned();
        entries.insert(added_raw.clone(), legacy_memo_entry("added"));
        super::write_artifact_cache_key_memo_file(storage.path(), &entries).unwrap();
        assert_eq!(
            super::legacy_identity_memo(&added, &added_raw, storage.path())
                .unwrap()
                .unwrap()
                .git_root_commit,
            "added"
        );
        assert_eq!(
            super::LEGACY_MEMO_CANONICALIZATIONS.with(|count| count.get()),
            3
        );
        entries.remove(&raw);
        super::write_artifact_cache_key_memo_file(storage.path(), &entries).unwrap();
        assert!(super::legacy_identity_memo(&root, &raw, storage.path())
            .unwrap()
            .is_none());
        super::write_artifact_cache_key_memo_file(storage.path(), &entries).unwrap();
        assert_eq!(super::LEGACY_MEMO_BUILDS.with(|count| count.get()), 1);
        assert_eq!(
            super::LEGACY_MEMO_CANONICALIZATIONS.with(|count| count.get()),
            3,
            "removed and unchanged spellings need no canonicalization on own writes"
        );
    }

    #[test]
    fn legacy_identity_memo_own_write_rebuilds_uncertain_or_old_snapshot() {
        let _cache = isolated_legacy_memo_indexes();
        for reason in ["expired", "unknown", "external"] {
            let storage = tempfile::tempdir().unwrap();
            let root = storage.path().join("checkout");
            let raw = root.to_string_lossy().into_owned();
            let mut entries = BTreeMap::from([(raw.clone(), legacy_memo_entry("old"))]);
            super::write_artifact_cache_key_memo_file(storage.path(), &entries).unwrap();
            super::LEGACY_MEMO_BUILDS.with(|count| count.set(0));
            super::legacy_identity_memo(&root, &raw, storage.path()).unwrap();
            let slot = super::legacy_identity_memo_slot(storage.path());
            match reason {
                "expired" => {
                    slot.lock().unwrap().as_mut().unwrap().built_at =
                        Instant::now() - super::LEGACY_IDENTITY_MEMO_MAX_AGE
                }
                "unknown" => slot.lock().unwrap().as_mut().unwrap().stamp = None,
                _ => {
                    let external = BTreeMap::from([(raw.clone(), legacy_memo_entry("ext"))]);
                    crate::file_identity::replace_keeping_times(
                        &super::artifact_cache_key_memo_path(storage.path()),
                        &serde_json::to_vec_pretty(&external).unwrap(),
                    );
                }
            }
            entries.get_mut(&raw).unwrap().git_root_commit = "new".into();
            super::write_artifact_cache_key_memo_file(storage.path(), &entries).unwrap();
            let current = super::legacy_identity_memo_slot(storage.path());
            assert!(
                current.lock().unwrap().is_none(),
                "{reason} snapshot must not be advanced"
            );
            if reason == "expired" {
                assert!(
                    !Arc::ptr_eq(&slot, &current),
                    "expiry removes the registry entry even while an old caller holds its slot"
                );
            }
            assert_eq!(
                super::legacy_identity_memo(&root, &raw, storage.path())
                    .unwrap()
                    .unwrap()
                    .git_root_commit,
                "new"
            );
            assert_eq!(
                super::LEGACY_MEMO_BUILDS.with(|count| count.get()),
                2,
                "{reason}"
            );
        }
    }

    #[test]
    fn legacy_identity_memo_own_write_does_not_stamp_an_external_replacement() {
        let _cache = isolated_legacy_memo_indexes();
        let storage = tempfile::tempdir().unwrap();
        let root = storage.path().join("checkout");
        let raw = root.to_string_lossy().into_owned();
        let mut entries = BTreeMap::from([(raw.clone(), legacy_memo_entry("old"))]);
        super::write_artifact_cache_key_memo_file(storage.path(), &entries).unwrap();
        super::legacy_identity_memo(&root, &raw, storage.path()).unwrap();
        let external = BTreeMap::from([(raw.clone(), legacy_memo_entry("ext"))]);
        let replacement = serde_json::to_vec_pretty(&external).unwrap();
        super::AFTER_LEGACY_MEMO_RENAME.with(|slot| {
            *slot.borrow_mut() = Some(Box::new(move |path| {
                crate::file_identity::replace_keeping_times(path, &replacement);
            }))
        });
        entries.get_mut(&raw).unwrap().git_root_commit = "own".into();
        super::write_artifact_cache_key_memo_file(storage.path(), &entries).unwrap();
        assert!(super::legacy_identity_memo_slot(storage.path())
            .lock()
            .unwrap()
            .is_none());
        let indexed = super::legacy_identity_memo(&root, &raw, storage.path());
        assert_eq!(
            indexed.as_ref().unwrap().as_ref().unwrap().git_root_commit,
            "ext"
        );
        assert_eq!(
            legacy_memo_value(indexed),
            legacy_memo_value(super::legacy_identity_memo_full_scan(
                &root,
                &raw,
                storage.path()
            ))
        );
    }

    #[test]
    fn legacy_identity_memo_rechecks_in_place_edits_deletion_and_invalid_json() {
        let _cache = isolated_legacy_memo_indexes();
        let storage = tempfile::tempdir().unwrap();
        let root = storage.path().join("checkout");
        let raw = root.to_string_lossy().into_owned();
        let path = super::artifact_cache_key_memo_path(storage.path());
        let mut entries = BTreeMap::from([(raw.clone(), legacy_memo_entry("old"))]);
        super::write_artifact_cache_key_memo_file(storage.path(), &entries).unwrap();
        super::legacy_identity_memo(&root, &raw, storage.path()).unwrap();
        let modified = fs::metadata(&path).unwrap().modified().unwrap();
        entries.get_mut(&raw).unwrap().git_root_commit = "new".into();
        fs::write(&path, serde_json::to_vec_pretty(&entries).unwrap()).unwrap();
        File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_times(fs::FileTimes::new().set_modified(modified + Duration::from_secs(1)))
            .unwrap();
        assert_eq!(
            super::legacy_identity_memo(&root, &raw, storage.path())
                .unwrap()
                .unwrap()
                .git_root_commit,
            "new"
        );
        fs::remove_file(&path).unwrap();
        assert!(super::legacy_identity_memo(&root, &raw, storage.path())
            .unwrap()
            .is_none());
        fs::write(&path, b"not json").unwrap();
        assert_eq!(
            legacy_memo_value(super::legacy_identity_memo(&root, &raw, storage.path())),
            legacy_memo_value(super::legacy_identity_memo_full_scan(
                &root,
                &raw,
                storage.path()
            ))
        );
    }

    #[cfg(unix)]
    #[test]
    fn legacy_identity_memo_revalidates_retargeted_matches() {
        let _cache = isolated_legacy_memo_indexes();
        for conflict in [false, true] {
            let storage = tempfile::tempdir().unwrap();
            let root = storage.path().join("checkout");
            let other = storage.path().join("other");
            fs::create_dir(&root).unwrap();
            fs::create_dir(&other).unwrap();
            let alias = storage.path().join("alias");
            std::os::unix::fs::symlink(&root, &alias).unwrap();
            let raw = root.to_string_lossy().into_owned();
            let alias_raw = alias.to_string_lossy().into_owned();
            let mut entries =
                BTreeMap::from([(alias_raw.clone(), legacy_memo_entry("alias-commit"))]);
            if conflict {
                entries.insert(raw.clone(), legacy_memo_entry("root-commit"));
            }
            super::write_artifact_cache_key_memo_file(storage.path(), &entries).unwrap();
            let first = super::legacy_identity_memo(&root, &raw, storage.path());
            if conflict {
                assert!(first
                    .err()
                    .unwrap()
                    .to_string()
                    .contains("conflicting repository identities"));
            } else {
                assert_eq!(first.unwrap().unwrap().git_root_commit, "alias-commit");
            }
            fs::remove_file(&alias).unwrap();
            std::os::unix::fs::symlink(&other, &alias).unwrap();
            let result = super::legacy_identity_memo(&root, &raw, storage.path()).unwrap();
            if conflict {
                assert_eq!(result.unwrap().git_root_commit, "root-commit");
            } else {
                assert!(
                    result.is_none(),
                    "retargeted alias must not return its former identity"
                );
                assert!(
                    super::legacy_identity_memo(&root, &alias_raw, storage.path())
                        .unwrap()
                        .is_none(),
                    "raw-spelling shortcut must not bypass revalidation"
                );
            }
        }
    }

    #[cfg(unix)]
    #[test]
    fn legacy_identity_memo_age_refresh_discovers_new_aliases() {
        let _cache = isolated_legacy_memo_indexes();
        for conflict in [false, true] {
            let storage = tempfile::tempdir().unwrap();
            let root = storage.path().join("checkout");
            fs::create_dir(&root).unwrap();
            let alias = storage.path().join("formerly-missing");
            let raw = root.to_string_lossy().into_owned();
            let mut entries = BTreeMap::from([(
                alias.to_string_lossy().into_owned(),
                legacy_memo_entry("root-commit"),
            )]);
            if conflict {
                entries.insert(raw.clone(), legacy_memo_entry("other-commit"));
            }
            super::write_artifact_cache_key_memo_file(storage.path(), &entries).unwrap();
            let before =
                legacy_memo_value(super::legacy_identity_memo(&root, &raw, storage.path()));
            assert_eq!(
                before,
                legacy_memo_value(super::legacy_identity_memo_full_scan(
                    &root,
                    &raw,
                    storage.path()
                ))
            );
            std::os::unix::fs::symlink(&root, &alias).unwrap();
            assert_eq!(legacy_memo_value(super::legacy_identity_memo(&root, &raw, storage.path())), before,
                "new aliases and conflicts are discovered at refresh rather than by rescanning every miss");
            let slot = super::legacy_identity_memo_slot(storage.path());
            slot.lock().unwrap().as_mut().unwrap().built_at =
                Instant::now() - super::LEGACY_IDENTITY_MEMO_MAX_AGE;
            let refreshed = super::legacy_identity_memo(&root, &raw, storage.path());
            if conflict {
                assert!(refreshed
                    .err()
                    .unwrap()
                    .to_string()
                    .contains("conflicting repository identities"));
            } else {
                assert_eq!(refreshed.unwrap().unwrap().git_root_commit, "root-commit");
            }
        }
    }

    #[test]
    fn legacy_identity_memo_index_matches_randomized_full_scan() {
        let _cache = isolated_legacy_memo_indexes();
        fn next(seed: &mut u64) -> u64 {
            *seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
            *seed
        }
        let storage = tempfile::tempdir().unwrap();
        let roots: Vec<_> = (0..9)
            .map(|index| {
                let root = storage.path().join(format!("checkout-{index}"));
                fs::create_dir(&root).unwrap();
                root
            })
            .collect();
        let mut queries = roots.clone();
        let mut spellings = Vec::new();
        for (index, root) in roots[..8].iter().enumerate() {
            spellings.push(root.clone());
            spellings.push(root.join("."));
            let missing = storage.path().join(format!("gone-{index}/nested/project"));
            queries.push(missing.clone());
            spellings.push(missing);
            #[cfg(unix)]
            {
                let alias = storage.path().join(format!("symlink-{index}"));
                std::os::unix::fs::symlink(root, &alias).unwrap();
                spellings.push(alias.clone());
                spellings.push(alias.join("missing/project"));
                queries.push(root.join("missing/project"));
            }
        }
        let mut seed = 0x615f_726f_6f74_u64;
        for round in 0..32 {
            let mut entries = BTreeMap::new();
            for _ in 0..48 {
                let spelling = &spellings[next(&mut seed) as usize % spellings.len()];
                let mut entry = legacy_memo_entry(&format!("root-{}", next(&mut seed) % 3));
                entry.recorded_at_ms = next(&mut seed);
                entry.key = format!("{:016x}", next(&mut seed));
                if next(&mut seed) % 11 == 0 {
                    entry.key = "invalid".into();
                }
                entries.insert(spelling.to_string_lossy().into_owned(), entry);
            }
            let fixed = roots[8].to_string_lossy().into_owned();
            let alias = roots[8].join(".").to_string_lossy().into_owned();
            entries.insert(fixed.clone(), legacy_memo_entry("fixed-root"));
            let mut second = legacy_memo_entry("fixed-root");
            second.key = "fedcba9876543210".into();
            match round % 4 {
                0 => second.git_root_commit = "conflicting-root".into(),
                1 => {}
                2 => second.git_root_commit = " ".into(),
                _ => second.key = "invalid".into(),
            }
            entries.insert(alias.clone(), second);
            for write in 0..4 {
                let selected = spellings[next(&mut seed) as usize % spellings.len()]
                    .to_string_lossy()
                    .into_owned();
                match write {
                    0 => {}
                    1 => {
                        entries.insert(selected, legacy_memo_entry("changed-root"));
                    }
                    2 => {
                        entries.remove(&selected);
                    }
                    _ => {
                        entries.insert(alias.clone(), legacy_memo_entry("fixed-root"));
                    }
                }
                super::write_artifact_cache_key_memo_file(storage.path(), &entries).unwrap();
                for root in &queries {
                    let raw = root.to_string_lossy();
                    let indexed =
                        legacy_memo_value(super::legacy_identity_memo(root, &raw, storage.path()));
                    let scanned = legacy_memo_value(super::legacy_identity_memo_full_scan(
                        root,
                        &raw,
                        storage.path(),
                    ));
                    assert_eq!(
                        indexed, scanned,
                        "round={round} write={write} root={root:?}"
                    );
                }
                let unrelated = storage.path().join("unrelated-query");
                assert_eq!(
                    legacy_memo_value(super::legacy_identity_memo(
                        &unrelated,
                        &fixed,
                        storage.path()
                    )),
                    legacy_memo_value(super::legacy_identity_memo_full_scan(
                        &unrelated,
                        &fixed,
                        storage.path()
                    )),
                    "exact raw spellings must retain the full scan's shortcut"
                );
            }
        }
    }

    #[test]
    fn legacy_identity_memo_repeated_misses_share_snapshot() {
        let _cache = isolated_legacy_memo_indexes();
        let storage = tempfile::tempdir().unwrap();
        let root = storage.path().join("new-checkout");
        let entries = BTreeMap::from([(
            storage
                .path()
                .join("removed/checkouts/old")
                .to_string_lossy()
                .into_owned(),
            super::ArtifactCacheKeyMemoEntry {
                key: "0123456789abcdef".into(),
                git_root_commit: "root-commit".into(),
                recorded_at_ms: 1,
            },
        )]);
        fs::write(
            super::artifact_cache_key_memo_path(storage.path()),
            serde_json::to_vec(&entries).unwrap(),
        )
        .unwrap();
        super::LEGACY_MEMO_BUILDS.with(|count| count.set(0));
        for _ in 0..2 {
            assert!(
                super::legacy_identity_memo(&root, "new-checkout", storage.path())
                    .unwrap()
                    .is_none()
            );
        }
        assert_eq!(
            super::LEGACY_MEMO_BUILDS.with(|count| count.get()),
            1,
            "unchanged memo misses must share the canonical-alias snapshot"
        );
    }

    /// Measures the compatibility lookup independently of Git and durable writes.
    /// Missing checkout spellings exercise the canonical-ancestor fallback used
    /// when old worktrees have been removed. No live cache memo is read.
    #[test]
    #[ignore = "manual native legacy identity memo timing probe"]
    fn legacy_identity_memo_native_probe() {
        let _cache = isolated_legacy_memo_indexes();
        let fixture = tempfile::tempdir().unwrap();
        let root = fixture.path().join("checkout");
        fs::create_dir(&root).unwrap();
        let raw_key = super::artifact_cache_key_memo_root_key(&root);
        for entries in [0, 256, 2048, 8192] {
            let memo: BTreeMap<_, _> = (0..entries)
                .map(|index| {
                    (
                        fixture
                            .path()
                            .join(format!("removed/checkouts/worker-{index}/project"))
                            .to_string_lossy()
                            .into_owned(),
                        super::ArtifactCacheKeyMemoEntry {
                            key: "0123456789abcdef".into(),
                            git_root_commit: "0123456789abcdef0123456789abcdef01234567".into(),
                            recorded_at_ms: crate::pins::now_ms(),
                        },
                    )
                })
                .collect();
            let bytes = serde_json::to_vec(&memo).unwrap();
            fs::write(super::artifact_cache_key_memo_path(fixture.path()), &bytes).unwrap();
            super::LEGACY_MEMO_BUILDS.with(|count| count.set(0));
            for sample in 0..8 {
                let started = std::time::Instant::now();
                assert!(super::legacy_identity_memo(&root, &raw_key, fixture.path())
                    .unwrap()
                    .is_none());
                eprintln!(
                    "legacy_memo_probe entries={entries} bytes={} sample={sample} phase={} builds={} elapsed_us={}",
                    bytes.len(),
                    if sample == 0 { "first" } else { "repeated_miss" },
                    super::LEGACY_MEMO_BUILDS.with(|count| count.get()),
                    started.elapsed().as_micros()
                );
            }
        }
    }

    /// Model serial lookups of previously unseen checkout roots: warm the alias
    /// index, record another checkout through the memo writer, then query a new root.
    /// Storage and checkout spellings are isolated fixtures, never live state.
    #[test]
    #[ignore = "manual native own-write and new-root memo timing probe"]
    fn legacy_identity_memo_own_write_native_probe() {
        let _cache = isolated_legacy_memo_indexes();
        for entries in [2048, 8192] {
            let storage = tempfile::tempdir().unwrap();
            let memo: BTreeMap<_, _> = (0..entries)
                .map(|index| {
                    (
                        storage
                            .path()
                            .join(format!("removed/checkouts/worker-{index}/project"))
                            .to_string_lossy()
                            .into_owned(),
                        super::ArtifactCacheKeyMemoEntry {
                            key: "0123456789abcdef".into(),
                            git_root_commit: "0123456789abcdef0123456789abcdef01234567".into(),
                            recorded_at_ms: crate::pins::now_ms(),
                        },
                    )
                })
                .collect();
            fs::write(
                super::artifact_cache_key_memo_path(storage.path()),
                serde_json::to_vec(&memo).unwrap(),
            )
            .unwrap();
            super::LEGACY_MEMO_BUILDS.with(|count| count.set(0));
            super::LEGACY_MEMO_CANONICALIZATIONS.with(|count| count.set(0));
            let first = storage.path().join("initial-new-root");
            assert!(
                super::legacy_identity_memo(&first, &first.to_string_lossy(), storage.path())
                    .unwrap()
                    .is_none()
            );
            for sample in 0..8 {
                let bound = storage.path().join(format!("bound-{sample}"));
                let next = storage.path().join(format!("next-{sample}"));
                fs::create_dir(&bound).unwrap();
                fs::create_dir(&next).unwrap();
                let started = Instant::now();
                let mut timings = super::ArtifactCacheKeyTimings::default();
                super::record_artifact_cache_key_memo_profiled(
                    storage.path(),
                    &bound.to_string_lossy(),
                    &format!("{sample:016x}"),
                    "0123456789abcdef0123456789abcdef01234567",
                    &mut timings,
                )
                .unwrap();
                let write_elapsed = started.elapsed();
                let lookup = Instant::now();
                assert!(super::legacy_identity_memo(
                    &next,
                    &next.to_string_lossy(),
                    storage.path()
                )
                .unwrap()
                .is_none());
                eprintln!("own_write_probe entries={entries} sample={sample} write_us={} next_lookup_us={} total_us={} builds={} canonicalizations={}",
                    write_elapsed.as_micros(), lookup.elapsed().as_micros(), started.elapsed().as_micros(),
                    super::LEGACY_MEMO_BUILDS.with(|count| count.get()),
                    super::LEGACY_MEMO_CANONICALIZATIONS.with(|count| count.get()));
            }
        }
    }

    fn publication_records(count: usize) -> Vec<SpillRecord> {
        (0..count as u32)
            .rev()
            .map(|trigram| SpillRecord {
                trigram,
                file_id: 0,
                next_mask: (trigram % 255) as u8,
                loc_mask: (trigram % 127) as u8,
            })
            .collect()
    }

    #[test]
    fn issue645_sorted_runs_match_global_sort_cache_bytes() {
        let temp = tempfile::tempdir().unwrap();
        let first = tempfile::tempdir().unwrap();
        let second = tempfile::tempdir().unwrap();
        fs::write(temp.path().join("source.rs"), "fn marker() {}\n").unwrap();
        let index = SearchIndex::build_with_limit_serial(temp.path(), 1024 * 1024);
        let plan = CacheWritePlan::from_index(&index, None).unwrap();
        let records = publication_records(SEARCH_BUILD_RECORD_BATCH * 3 + 17);
        let mut global = records.clone();
        global.sort_unstable_by_key(|record| (record.trigram, record.file_id));
        let mut reference: Vec<Box<dyn PostingRecordSource>> =
            vec![Box::new(VecRecordSource::new(global))];
        let mut runs = sorted_record_sources(records, &|| true).unwrap();
        for (dir, sources) in [(first.path(), &mut reference), (second.path(), &mut runs)] {
            write_cache_file_from_sources(
                dir,
                &plan,
                sources,
                crate::write_ledger::Domain::SearchIndexBuild,
                &|| true,
            )
            .unwrap();
        }
        assert_eq!(
            fs::read(first.path().join("cache.bin")).unwrap(),
            fs::read(second.path().join("cache.bin")).unwrap()
        );
    }

    #[test]
    fn issue645_publication_sort_merge_and_checksum_cancel_promptly() {
        let calls = AtomicUsize::new(0);
        let result =
            sorted_record_sources(publication_records(SEARCH_BUILD_RECORD_BATCH * 3), &|| {
                calls.fetch_add(1, Ordering::SeqCst) < 1
            });
        assert!(matches!(result, Err(error) if error.kind() == std::io::ErrorKind::Interrupted));
        assert_eq!(calls.load(Ordering::SeqCst), 2);

        struct CancellingSource {
            reads: Arc<AtomicUsize>,
        }
        impl PostingRecordSource for CancellingSource {
            fn next_record(&mut self) -> std::io::Result<Option<SpillRecord>> {
                let reads = self.reads.fetch_add(1, Ordering::SeqCst);
                Ok(
                    (reads < SEARCH_BUILD_RECORD_BATCH * 3).then_some(SpillRecord {
                        trigram: reads as u32,
                        file_id: 0,
                        next_mask: 0,
                        loc_mask: 0,
                    }),
                )
            }
        }
        let temp = tempfile::tempdir().unwrap();
        let output = temp.path().join("postings");
        let reads = Arc::new(AtomicUsize::new(0));
        let mut sources: Vec<Box<dyn PostingRecordSource>> = vec![Box::new(CancellingSource {
            reads: reads.clone(),
        })];
        let mut writer = BufWriter::new(File::create(&output).unwrap());
        let result = merge_sources_to_writer(&mut sources, &mut writer, &|| {
            reads.load(Ordering::SeqCst) < 2
        });
        assert!(matches!(result, Err(error) if error.kind() == std::io::ErrorKind::Interrupted));
        assert!(reads.load(Ordering::SeqCst) <= SEARCH_BUILD_RECORD_BATCH + 1);

        let bytes = vec![42; 3 * 1024 * 1024];
        fs::write(&output, &bytes).unwrap();
        let calls = AtomicUsize::new(0);
        let result = crc32_file_range(&output, 0, bytes.len() as u64, &|| {
            calls.fetch_add(1, Ordering::SeqCst) < 1
        });
        assert_eq!(result.unwrap_err().kind(), std::io::ErrorKind::Interrupted);
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        assert_eq!(
            crc32_file_range(&output, 0, bytes.len() as u64, &|| true).unwrap(),
            crc32fast::hash(&bytes)
        );
    }

    #[test]
    fn issue645_cancelled_publication_preserves_cache() {
        use super::*;
        let temp = tempfile::tempdir().unwrap();
        let cache = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        fs::write(root.join("source.rs"), "fn original_marker() {}\n").unwrap();
        let original = SearchIndex::build_with_limit_to_cache_dir(&root, 1024 * 1024, cache.path());
        let before = fs::read(cache.path().join("cache.bin")).unwrap();
        fs::write(root.join("source.rs"), "fn replacement_marker() {}\n").unwrap();
        let completed_cache = tempfile::tempdir().unwrap();
        let _completed =
            SearchIndex::build_with_limit_to_cache_dir(&root, 1024 * 1024, completed_cache.path());
        let completed_size = fs::metadata(completed_cache.path().join("cache.bin"))
            .unwrap()
            .len();
        // The full serialized size becomes visible only after the final flush,
        // so the second case cancels at the last checkpoint before rename.
        for (phase, min_bytes) in [("writing", 0), ("before rename", completed_size)] {
            let cancelled = AtomicBool::new(false);
            let result = SearchIndex::build_with_limit_to_cache_dir_cancellable(
                &root,
                1024 * 1024,
                cache.path(),
                &|| {
                    let stop = fs::read_dir(cache.path()).unwrap().flatten().any(|entry| {
                        entry
                            .file_name()
                            .to_string_lossy()
                            .starts_with("cache.bin.tmp.")
                            && entry.metadata().unwrap().len() >= min_bytes
                    });
                    cancelled.fetch_or(stop, Ordering::SeqCst);
                    !cancelled.load(Ordering::SeqCst)
                },
            );
            assert!(
                result.is_none(),
                "a build cancelled {phase} must not publish"
            );
            assert!(cancelled.load(Ordering::SeqCst), "did not reach {phase}");
            assert_eq!(fs::read(cache.path().join("cache.bin")).unwrap(), before);
            assert_eq!(
                fs::read_dir(cache.path()).unwrap().count(),
                1,
                "no temporary files leak"
            );
        }
        assert_eq!(
            original
                .candidates(&decompose_regex("original_marker"))
                .len(),
            1
        );
    }

    #[test]
    fn issue645_cancelled_fallback_logs_cancelled_not_failed() {
        use super::*;
        let temp = tempfile::tempdir().unwrap();
        let cache = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        fs::write(root.join("source.rs"), "fn marker() {}\n").unwrap();
        // A malformed cache path is rejected at admission, before streaming.
        // Fail after admission so this still exercises cancellation in fallback.
        FAIL_STREAMING_BUILD.set(true);
        let probes = AtomicUsize::new(0);
        let (result, events) = crate::logging::capture_index_events(|| {
            SearchIndex::build_with_limit_to_cache_dir_cancellable(
                &root,
                1024 * 1024,
                cache.path(),
                &|| probes.fetch_add(1, Ordering::SeqCst) < 2,
            )
        });
        assert!(result.is_none());
        let root_field = format!("root={}", crate::logging::normalize_index_root(&root));
        let events = events
            .into_iter()
            .filter(|event| event.contains(&root_field))
            .collect::<Vec<_>>();
        assert!(
            events.iter().any(|event| event.contains("stage=fallback")),
            "{events:?}"
        );
        assert!(
            events
                .iter()
                .any(|event| event.contains("kind=build_cancelled")),
            "{events:?}"
        );
        assert!(
            !events
                .iter()
                .any(|event| event.contains("kind=build_failed")),
            "{events:?}"
        );
    }

    #[test]
    fn issue645_superseded_search_build_stops_reading_files() {
        use super::*;
        let temp = tempfile::tempdir().unwrap();
        let cache = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        for index in 0..256 {
            fs::write(root.join(format!("source-{index}.rs")), "fn marker() {}\n").unwrap();
        }
        let reads = Arc::new(AtomicUsize::new(0));
        let cancelled = Arc::new(AtomicBool::new(false));
        REFRESH_READ_PROBES
            .get_or_init(Mutex::default)
            .lock()
            .unwrap()
            .insert(root.clone(), (reads.clone(), cancelled.clone()));
        let result = SearchIndex::build_with_limit_to_cache_dir_cancellable(
            &root,
            1024 * 1024,
            cache.path(),
            &|| !cancelled.load(Ordering::SeqCst),
        );
        REFRESH_READ_PROBES
            .get()
            .unwrap()
            .lock()
            .unwrap()
            .remove(&root);
        let count = reads.load(Ordering::SeqCst);
        assert!(
            result.is_none(),
            "superseded builds cannot publish a partial index; files read={count}"
        );
        assert!(
            (1..=32).contains(&count),
            "must stop within the active chunk, read {count}/256 files"
        );
        assert!(
            !cache.path().join("cache.bin").exists(),
            "cancelled build must not persist"
        );
    }

    // Frozen from c71c33a96, including its SHA-256 and path/session key formulas.
    // Git probing and memo codecs are shared, but no record-aware resolver or
    // current top-level/path key hasher may supply the compatibility expectation.
    fn base_c71_cache_key(
        probe_root: &Path,
        memo_root: &Path,
        storage_root: &Path,
        git_common_dir: Option<&Path>,
        timings: &mut ArtifactCacheKeyTimings,
    ) -> Result<String, ArtifactCacheKeyProbeError> {
        fn base_hash16(bytes: &[u8]) -> String {
            use sha2::{Digest, Sha256};
            let mut hasher = Sha256::new();
            hasher.update(bytes);
            format!("{:x}", hasher.finalize())[..16].to_string()
        }
        fn artifact_path_identity_key(root: &Path) -> String {
            base_hash16(canonicalize_or_normalize(root).to_string_lossy().as_bytes())
        }
        fn session_artifact_key_from_git_identity(root: &Path, root_commit: &str) -> String {
            match checkout_position(root) {
                CheckoutPosition::TopLevel => base_hash16(root_commit.as_bytes()),
                CheckoutPosition::Subdirectory(relative) => {
                    match crate::scoped_key::scoped_v1_key(root_commit, relative.as_bytes()) {
                        Ok(scoped) => scoped[..16].to_string(),
                        Err(_) => artifact_path_identity_key(root),
                    }
                }
                CheckoutPosition::Unrepresentable => artifact_path_identity_key(root),
            }
        }
        let memo_root_key = artifact_cache_key_memo_root_key(memo_root);
        let git_marker_state = root_git_marker_state(probe_root, git_common_dir);
        if git_marker_state == GitMarkerState::Absent {
            return Ok(artifact_path_identity_key(probe_root));
        }

        match repo_root_commit_with_retry_profiled(probe_root, timings) {
            RootCommitResolution::Commit(root_commit) => {
                let key = session_artifact_key_from_git_identity(probe_root, &root_commit);
                record_derived_cache_key(memo_root, &key);
                if let Err(error) = record_artifact_cache_key_memo_profiled(
                    storage_root,
                    &memo_root_key,
                    &key,
                    &root_commit,
                    timings,
                ) {
                    crate::slog_warn!(
                        "artifact cache key: failed to persist memo for {} in {}: {}",
                        memo_root.display(),
                        storage_root.display(),
                        error
                    );
                }
                Ok(key)
            }
            RootCommitResolution::NotARepo => Ok(artifact_path_identity_key(probe_root)),
            RootCommitResolution::Failed(detail) => {
                if let Some(entry) =
                    lookup_artifact_cache_key_memo_profiled(storage_root, &memo_root_key, timings)
                {
                    // Re-derive from the memoized repository identity rather than
                    // trusting the memoized key: memos written by older versions
                    // hold the whole-repository key even for subfolder roots, and
                    // reusing it would let a subfolder session overwrite the
                    // repository's shared artifact again.
                    let key =
                        session_artifact_key_from_git_identity(probe_root, &entry.git_root_commit);
                    crate::slog_warn!(
                        "artifact cache key: probe failed, using memoized identity (key {}) for {}",
                        key,
                        memo_root.display()
                    );
                    return Ok(key);
                }

                match git_marker_state {
                    GitMarkerState::Absent => Ok(artifact_path_identity_key(probe_root)),
                    GitMarkerState::Present => Err(ArtifactCacheKeyProbeError {
                        root: memo_root.to_path_buf(),
                        detail,
                        failure: ArtifactCacheKeyFailure::GitUnavailable,
                    }),
                    GitMarkerState::Unknown(marker_detail) => Err(ArtifactCacheKeyProbeError {
                        root: memo_root.to_path_buf(),
                        detail: format!("{detail}; {marker_detail}"),
                        failure: ArtifactCacheKeyFailure::GitUnavailable,
                    }),
                }
            }
            RootCommitResolution::Cancelled => Err(ArtifactCacheKeyProbeError {
                root: memo_root.to_path_buf(),
                detail: "artifact cache key probe cancelled".to_string(),
                failure: ArtifactCacheKeyFailure::IdentityUnavailable,
            }),
        }
    }

    fn upgrade_fixture_git(
        root: &Path,
        args: &[&str],
        author: &serde_json::Value,
    ) -> std::process::Output {
        let mut command = std::process::Command::new("git");
        crate::test_env::apply_hermetic_git_env(command.current_dir(root));
        let output = command
            .args(args)
            .env("GIT_AUTHOR_NAME", author["name"].as_str().unwrap())
            .env("GIT_COMMITTER_NAME", author["name"].as_str().unwrap())
            .env("GIT_AUTHOR_EMAIL", author["email"].as_str().unwrap())
            .env("GIT_COMMITTER_EMAIL", author["email"].as_str().unwrap())
            .env("GIT_AUTHOR_DATE", author["date"].as_str().unwrap())
            .env("GIT_COMMITTER_DATE", author["date"].as_str().unwrap())
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "fixture git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        output
    }

    fn assert_upgrade_identity_fixture(name: &str, repeat_bind: bool) {
        let _probe_lock = git_root_commit_probe_override_lock_for_test();
        assert!(
            crate::developer_tools::git_usable(),
            "upgrade fixtures require usable Git"
        );
        let fixture: serde_json::Value = serde_json::from_str(include_str!(
            "../tests/fixtures/root_identity_upgrade/cases.json"
        ))
        .unwrap();
        let case = fixture["cases"]
            .as_array()
            .unwrap()
            .iter()
            .find(|case| case["name"].as_str() == Some(name))
            .unwrap();
        let directory = tempfile::tempdir().unwrap();
        let repository = directory.path().join("repository");
        for (relative, content) in fixture["files"].as_object().unwrap() {
            let path = repository.join(relative);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, content.as_str().unwrap()).unwrap();
        }
        let plain = case["layout"] == "plain";
        let author = &fixture["commit"];
        if !plain {
            upgrade_fixture_git(&repository, &["init", "-q"], author);
            upgrade_fixture_git(&repository, &["add", "."], author);
            upgrade_fixture_git(
                &repository,
                &["commit", "-q", "-m", author["message"].as_str().unwrap()],
                author,
            );
        }
        let root = if case["layout"] == "linked_worktree" {
            let linked = directory.path().join("linked");
            upgrade_fixture_git(
                &repository,
                &[
                    "worktree",
                    "add",
                    "--detach",
                    linked.to_str().unwrap(),
                    "HEAD",
                ],
                author,
            );
            assert!(linked.join(".git").is_file());
            linked
        } else {
            repository.clone()
        };
        let root = fs::canonicalize(root).unwrap();
        if plain {
            assert!(
                root.ancestors().all(|parent| !parent.join(".git").exists()),
                "plain fixture must be outside any repository"
            );
        } else if case["layout"] == "repository" {
            assert!(root.join(".git").is_dir());
        }
        let common = (!plain).then(|| fs::canonicalize(repository.join(".git")).unwrap());
        let base_storage = directory.path().join("base-storage");
        let upgraded_storage = directory.path().join("upgraded-storage");
        if case["legacy_memo"] == true {
            let seed = base_c71_cache_key(
                &root,
                &root,
                &directory.path().join("seed-storage"),
                common.as_deref(),
                &mut ArtifactCacheKeyTimings::default(),
            )
            .unwrap();
            let output =
                upgrade_fixture_git(&root, &["rev-list", "--max-parents=0", "HEAD"], author);
            let entry = ArtifactCacheKeyMemoEntry {
                key: seed,
                git_root_commit: String::from_utf8(output.stdout).unwrap().trim().to_string(),
                recorded_at_ms: current_time_millis(),
            };
            for storage in [&base_storage, &upgraded_storage] {
                write_cache_key_memo(
                    storage,
                    &BTreeMap::from([(artifact_cache_key_memo_root_key(&root), entry.clone())]),
                );
            }
            assert_eq!(
                fs::read(base_storage.join(ARTIFACT_CACHE_KEY_MEMO_FILE)).unwrap(),
                fs::read(upgraded_storage.join(ARTIFACT_CACHE_KEY_MEMO_FILE)).unwrap()
            );
        }
        // Separate stores keep the old oracle's memo writes out of the upgrade input.
        let expected = base_c71_cache_key(
            &root,
            &root,
            &base_storage,
            common.as_deref(),
            &mut ArtifactCacheKeyTimings::default(),
        )
        .unwrap();
        assert!(!base_storage.join("root-identities").exists());
        assert!(crate::root_identity::read(&root, &upgraded_storage)
            .unwrap()
            .is_none());
        let actual =
            artifact_cache_key_with_memo(&root, &root, &upgraded_storage, common.as_deref())
                .unwrap();
        assert_eq!(
            actual, expected,
            "first upgrade bind changed the base key for {name}"
        );
        assert!(crate::root_identity::read(&root, &upgraded_storage)
            .unwrap()
            .is_some());
        if repeat_bind {
            assert!(matches!(
                crate::root_identity::read(&root, &upgraded_storage).unwrap(),
                Some(crate::root_identity::Identity::Git { .. })
            ));
            let next =
                artifact_cache_key_with_memo(&root, &root, &upgraded_storage, common.as_deref())
                    .unwrap();
            assert_eq!(
                next, expected,
                "record-derived key changed on the next bind"
            );
        }
    }

    #[test]
    fn upgrade_identity_repository_top_level_matches_base() {
        assert_upgrade_identity_fixture("repository_top_level", false);
    }

    #[test]
    fn upgrade_identity_linked_worktree_matches_base() {
        assert_upgrade_identity_fixture("linked_worktree", false);
    }

    #[test]
    fn upgrade_identity_plain_folder_matches_base() {
        assert_upgrade_identity_fixture("plain_folder", false);
    }

    #[test]
    fn upgrade_identity_legacy_git_memo_matches_base() {
        assert_upgrade_identity_fixture("legacy_git_memo", false);
    }

    #[test]
    fn upgrade_identity_written_repository_record_keeps_base_key() {
        assert_upgrade_identity_fixture("repository_top_level", true);
    }

    #[test]
    fn semantic_namespaces_share_type_identity() {
        use std::any::TypeId;
        assert_eq!(
            TypeId::of::<super::exact_lane::ExactLane>(),
            TypeId::of::<crate::commands::semantic_search::exact_lane::ExactLane>()
        );
        assert_eq!(
            TypeId::of::<super::memo::ExactMemoStore>(),
            TypeId::of::<crate::commands::semantic_search::memo::ExactMemoStore>()
        );
    }

    #[test]
    fn storage_and_sqlite_files_never_reach_corpus_open() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let storage = root.join("task-storage");
        fs::create_dir_all(storage.join("nested")).unwrap();
        {
            let _env = crate::test_env::without_storage_override();
            assert_eq!(crate::bash_background::storage_dir(Some(&storage)), storage);
        }
        fs::write(root.join("source.rs"), "fn user_source() {}\n").unwrap();
        for path in [
            storage.join("aft.db"),
            storage.join("aft.db-wal"),
            storage.join("aft.db-shm"),
            storage.join("nested/internal.rs"),
            root.join("other.sqlite"),
            root.join("other.sqlite-wal"),
            root.join("other.sqlite-shm"),
        ] {
            fs::write(path, "must not open this as project content\n").unwrap();
        }
        let probe = crate::project_content::Probe::new(&root);
        let index = SearchIndex::build_with_limit(&root, DEFAULT_MAX_FILE_SIZE);
        let opened = probe.paths();
        assert!(
            opened.contains(&root.join("source.rs")),
            "source was not indexed: {opened:?}"
        );
        assert!(
            opened.iter().all(|path| path == &root.join("source.rs")),
            "indexer opened storage or SQLite files: {opened:?}"
        );
        drop(index);
        for path in [
            storage.join("aft.db-shm"),
            storage.join("nested/internal.rs"),
        ] {
            audit_work_reset();
            let _ = read_search_corpus_file(&path, DEFAULT_MAX_FILE_SIZE);
            assert_eq!(
                audit_work().corpus_opens,
                0,
                "direct corpus read opened {path:?}"
            );
        }
    }

    #[test]
    fn warm_git_diff_budget_counts_both_rename_paths() {
        let mut diff = (0..510)
            .map(|i| format!("M\0file_{i}.rs\0"))
            .collect::<String>();
        diff.push_str("R100\0old.rs\0new.rs\0");
        assert!(git_diff_within_path_limit(diff.as_bytes(), 512));
        diff.push_str("A\0extra.rs\0");
        assert!(!git_diff_within_path_limit(diff.as_bytes(), 512));
        assert!(git_diff_within_path_limit(b"", 512));
    }

    #[test]
    fn git_name_status_z_keeps_special_names_raw_and_pairs_rename_paths() {
        let diff = b"M\0caf\xc3\xa9.rs\0R087\0old name.rs\0new\tname.rs\0A\0say \"hi\".rs\0";
        let records = parse_git_name_status_z(diff).expect("well-formed output");
        let parsed: Vec<(&[u8], Vec<&[u8]>)> = records
            .iter()
            .map(|record| (record.status, record.paths.clone()))
            .collect();
        assert_eq!(
            parsed,
            vec![
                (&b"M"[..], vec!["café.rs".as_bytes()]),
                (
                    &b"R087"[..],
                    vec![&b"old name.rs"[..], &b"new\tname.rs"[..]]
                ),
                (&b"A"[..], vec![&b"say \"hi\".rs"[..]]),
            ]
        );
        assert_eq!(
            git_output_path("café.rs".as_bytes()),
            Some(PathBuf::from("café.rs"))
        );
        assert!(parse_git_name_status_z(b"").is_some_and(|records| records.is_empty()));
        // A rename missing its new path, or output cut before its final NUL,
        // is rejected rather than half-applied.
        assert!(parse_git_name_status_z(b"R100\0old.rs\0").is_none());
        assert!(parse_git_name_status_z(b"M\0file.rs").is_none());
        assert!(!git_diff_within_path_limit(b"R100\0old.rs\0", 512));
    }

    #[test]
    fn perf_audit_empty_filters_skip_path_keys() {
        let root = Path::new("/project");
        audit_work_reset();
        for i in 0..1000 {
            assert!(PathFilters::default().matches(root, &root.join(format!("src/file_{i}.rs"))));
        }
        assert_eq!(audit_work().filter_keys, 0);
        let filters = build_path_filters(&["**/*.rs".to_string()], &[]).unwrap();
        assert!(filters.matches(root, &root.join("src/main.rs")));
        assert!(!filters.matches(root, &root.join("src/main.txt")));
    }
    #[test]
    fn perf_audit_cancellation_probe_budget() {
        let dir = tempfile::tempdir().unwrap();
        let token = crate::executor::JobCancellation::new().with_root(dir.path());
        let _guard = crate::executor::install_job_cancellation(token.clone());
        AUDIT_CANCEL_ELAPSED.with(|slot| slot.set(Some(Duration::ZERO)));
        audit_work_reset();
        for _ in 0..100_000 {
            assert!(!crate::executor::current_job_cancelled());
        }
        let work = audit_work();
        assert_eq!(work.cancel_probes, 391, "{work:?}");
        fs::remove_dir(dir.path()).unwrap();
        assert!((0..256).any(|_| crate::executor::current_job_cancelled()));
        assert!(token.cancel_already_requested());
        AUDIT_CANCEL_ELAPSED.with(|slot| slot.set(None));
    }

    #[test]
    fn cancellation_time_probe_and_explicit_signal_are_prompt() {
        let dir = tempfile::tempdir().unwrap();
        let token = crate::executor::JobCancellation::new().with_root(dir.path());
        assert!(!token.cancel_requested_before_commit());
        fs::remove_dir(dir.path()).unwrap();
        AUDIT_CANCEL_ELAPSED.with(|slot| slot.set(Some(Duration::from_millis(50))));
        assert!(token.cancel_requested_before_commit());
        AUDIT_CANCEL_ELAPSED.with(|slot| slot.set(None));
        let dir = tempfile::tempdir().unwrap();
        let token = crate::executor::JobCancellation::new().with_root(dir.path());
        assert!(!token.cancel_requested_before_commit());
        token.request_cancel();
        assert!(token.cancel_requested_before_commit());
    }

    #[test]
    fn perf_audit_corpus_opens_once() {
        let dir = tempfile::tempdir().unwrap();
        for i in 0..256 {
            let path = dir.path().join(format!("source_{i:03}.rs"));
            fs::write(
                &path,
                format!("// generated by fixture\nfn marker_{i}() {{}}\n"),
            )
            .unwrap();
        }
        audit_work_reset();
        for i in 0..256 {
            let path = dir.path().join(format!("source_{i:03}.rs"));
            assert!(matches!(
                prepare_search_path(&path, DEFAULT_MAX_FILE_SIZE),
                PreparedSearchPath::Indexed(_)
            ));
        }
        let work = audit_work();
        assert_eq!(work.corpus_opens, 256, "{work:?}");
    }

    #[test]
    fn corpus_reader_preserves_generated_and_binary_eligibility() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("source.rs");
        for bytes in [
            b"// generated by fixture\nfn marker() {}\n".as_slice(),
            b"fn marker() {}\n",
            b"\xff\n// generated by fixture\n",
        ] {
            fs::write(&path, bytes).unwrap();
            let expected_generated = crate::inspect::is_generated_file(&path, &path);
            match read_search_corpus_file(&path, DEFAULT_MAX_FILE_SIZE) {
                SearchCorpusEligibility::Eligible(file) => {
                    assert_eq!(file.bytes, bytes);
                    assert_eq!(file.generated, expected_generated);
                }
                _ => panic!("text remains eligible"),
            }
        }
        fs::write(&path, b"binary\0content").unwrap();
        assert!(matches!(
            read_search_corpus_file(&path, DEFAULT_MAX_FILE_SIZE),
            SearchCorpusEligibility::Unindexed(_)
        ));
        fs::write(&path, "oversize content").unwrap();
        assert!(matches!(
            read_search_corpus_file(&path, 3),
            SearchCorpusEligibility::Unindexed(_)
        ));
        fs::remove_file(&path).unwrap();
        assert!(matches!(
            read_search_corpus_file(&path, DEFAULT_MAX_FILE_SIZE),
            SearchCorpusEligibility::Skipped
        ));
    }

    #[test]
    fn perf_audit_build_walk_skips_mtime_sort() {
        let dir = tempfile::tempdir().unwrap();
        for i in 0..256 {
            fs::write(
                dir.path().join(format!("source_{i:03}.rs")),
                "fn marker() {}\n",
            )
            .unwrap();
        }
        audit_work_reset();
        let paths = walk_project_files(dir.path(), &PathFilters::default());
        assert_eq!(paths.len(), 256);
        let work = audit_work();
        assert_eq!(work.sort_stats, 0, "{work:?}");
        assert_eq!(work.sort_canonicalizes, 0, "{work:?}");
    }

    #[test]
    fn perf_audit_delta_slots_stay_bounded() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("edited.rs");
        fs::write(&path, "fn old_marker() {}\n").unwrap();
        let mut index = SearchIndex::build(dir.path());
        let before = index.snapshot();
        for i in 0..256 {
            fs::write(&path, format!("fn new_marker_{i}() {{}}\n")).unwrap();
            index.update_file(&path);
        }
        assert!(
            index.files.len() <= 2,
            "{} file slots for one live file",
            index.files.len()
        );
        assert_eq!(before.candidates(&decompose_regex("old_marker")).len(), 1);
        assert_eq!(
            index.candidates(&decompose_regex("new_marker_255")).len(),
            1
        );
        let reference = SearchIndex::build_with_limit_serial(dir.path(), DEFAULT_MAX_FILE_SIZE);
        assert_eq!(
            index
                .grep("new_marker_255", true, &[], &[], dir.path(), 100)
                .matches,
            reference
                .grep("new_marker_255", true, &[], &[], dir.path(), 100)
                .matches
        );
    }

    #[test]
    fn durability_search_cache_write_count() {
        let project = tempfile::tempdir().unwrap();
        let cache = tempfile::tempdir().unwrap();
        fs::write(project.path().join("source.txt"), "abcdef").unwrap();
        let mut index = SearchIndex::build(project.path());
        crate::durability::take();
        assert!(index.write_to_disk(cache.path(), None));
        let events = crate::durability::take();
        assert_eq!(crate::durability::sync_count(&events), 0, "{events:?}");
        assert!(SearchIndex::read_from_disk(cache.path(), project.path()).is_some());
    }

    #[test]
    fn durability_search_spill_write_count() {
        let dir = tempfile::tempdir().unwrap();
        let mut block = vec![SpillRecord {
            trigram: 1,
            file_id: 0,
            next_mask: 0,
            loc_mask: 0,
        }];
        let counter = crate::write_ledger::register(
            crate::write_ledger::Domain::SearchIndexBuild,
            "durability-spill",
        );
        crate::durability::take();
        let path = flush_spill_segment(dir.path(), 0, &mut block, &counter).unwrap();
        assert!(path.is_file());
        assert!(block.is_empty());
        let events = crate::durability::take();
        assert_eq!(crate::durability::sync_count(&events), 0, "{events:?}");
    }
    use std::process::Command;

    use super::*;

    /// Unmatched leading `..` components must survive comparison normalization:
    /// dropping them makes a path that escapes upward look like a nearby child.
    #[test]
    fn comparison_adapter_preserves_relative_parent_components() {
        for (input, expected) in [("../../x", "../../x"), ("a/../../x", "../x")] {
            assert_eq!(
                super::normalize_path(std::path::Path::new(input)),
                std::path::PathBuf::from(expected),
                "{input}"
            );
        }
    }

    /// A repository with a corrupt HEAD makes Git print its ordinary "not a git
    /// repository" message. That is damaged metadata, not proof of absence, so
    /// the root-commit probe must stay retryable instead of choosing the
    /// path-derived cache identity reserved for real non-repositories.
    #[test]
    fn corrupt_root_commit_metadata_is_not_path_identity_fallback() {
        assert!(crate::developer_tools::git_usable());
        let dir = tempfile::tempdir().unwrap();
        crate::git_probe::init_repository_with_corrupt_head_for_test(dir.path());
        assert!(matches!(
            git_root_commit_once_real(dir.path(), &mut ArtifactCacheKeyTimings::default()),
            RootCommitProbe::Transient(_)
        ));
    }

    /// A `.git` folder without HEAD (for example only `info/exclude`) is not a
    /// repository to Git, so it keeps the path-derived identity of a plain folder.
    #[test]
    fn stub_git_directory_root_commit_is_not_a_repository() {
        assert!(crate::developer_tools::git_usable());
        let _probe_lock = git_root_commit_probe_override_lock_for_test();
        let dir = tempfile::tempdir().unwrap();
        crate::git_probe::write_stub_git_directory_for_test(dir.path());
        assert!(matches!(
            git_root_commit_once_real(dir.path(), &mut ArtifactCacheKeyTimings::default()),
            RootCommitProbe::NotARepo
        ));
        assert!(matches!(
            repo_root_commit_with_retry(dir.path()),
            RootCommitResolution::NotARepo
        ));
    }

    /// A verified repository without commits keeps its intentional mapping to
    /// the path-derived identity.
    #[test]
    fn unborn_root_commit_retains_the_intentional_non_repository_mapping() {
        assert!(crate::developer_tools::git_usable());
        let _probe_lock = git_root_commit_probe_override_lock_for_test();
        let dir = tempfile::tempdir().unwrap();
        assert!(Command::new("git")
            .args(["init", "--quiet"])
            .arg(dir.path())
            .status()
            .unwrap()
            .success());
        let mut timings = ArtifactCacheKeyTimings::default();
        assert!(matches!(
            git_root_commit_once_real(dir.path(), &mut timings),
            RootCommitProbe::NoCommit
        ));
        // rev-list, then git-dir, verify, symbolic-ref and two git-path checks.
        assert_eq!(timings.git_spawns, 6);
        assert!(matches!(
            repo_root_commit_with_retry(dir.path()),
            RootCommitResolution::NotARepo
        ));
    }

    fn transient_search_cache_test_dir(root: &Path, name: &str, contents: &[u8]) -> PathBuf {
        let path = root.join(name);
        fs::create_dir_all(&path).expect("create transient cache fixture");
        fs::write(path.join("cache.bin"), contents).expect("write transient cache fixture");
        path
    }

    fn age_transient_search_cache_test_dir(path: &Path) {
        let old = SystemTime::now()
            .checked_sub(TRANSIENT_SEARCH_CACHE_MIN_AGE + Duration::from_secs(1))
            .expect("construct aged transient cache time");
        filetime::set_file_mtime(path, filetime::FileTime::from_system_time(old))
            .expect("age transient cache fixture");
    }

    #[test]
    fn transient_search_cache_name_parses_stable_and_legacy_layouts() {
        let key = "0123456789abcdef";
        let stable = format!("{TRANSIENT_SEARCH_CACHE_PREFIX}{key}.42");
        let legacy = format!("{TRANSIENT_SEARCH_CACHE_PREFIX}{key}.42.123456789");
        assert_eq!(
            parse_transient_search_cache_name(&stable)
                .expect("stable transient cache name")
                .pid,
            42
        );
        assert_eq!(
            parse_transient_search_cache_name(&legacy)
                .expect("legacy transient cache name")
                .key,
            key
        );
        assert!(parse_transient_search_cache_name(&format!(
            "{TRANSIENT_SEARCH_CACHE_PREFIX}{key}.42.not-a-nanosecond"
        ))
        .is_none());
    }

    #[test]
    fn transient_builds_reuse_one_cache_dir_and_truncate_previous_contents() {
        let project = tempfile::tempdir().expect("project tempdir");
        let root = fs::canonicalize(project.path()).expect("canonical project root");
        fs::write(root.join("source.rs"), "pub fn transient_cache() {}\n")
            .expect("write project source");
        let key = artifact_cache_key(&root);
        let prefix = format!("{TRANSIENT_SEARCH_CACHE_PREFIX}{key}.");
        // Our own directory, not the shared one: a listing of `$TMPDIR` costs
        // whatever the rest of the machine left in it (~870 s at 290k entries
        // on a fleet box), which is the same trap the product hit.
        let temp_root = transient_search_cache_home();
        let _ = fs::create_dir_all(&temp_root);
        let remove_matching = || {
            let Ok(entries) = fs::read_dir(&temp_root) else {
                return;
            };
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().into_owned();
                if name.starts_with(&prefix) && entry.file_type().is_ok_and(|kind| kind.is_dir()) {
                    let _ = fs::remove_dir_all(entry.path());
                }
            }
        };
        remove_matching();

        let cache_dir = transient_search_cache_dir(&root);
        let first = SearchIndex::build(&root);
        assert!(first.ready, "first transient build must complete");
        let stale_file = cache_dir.join("must-be-truncated");
        fs::write(&stale_file, "old scratch data").expect("plant stale scratch file");
        let second = SearchIndex::build(&root);
        assert!(second.ready, "second transient build must complete");

        let matching = fs::read_dir(&temp_root)
            .expect("read system temp directory")
            .flatten()
            .filter(|entry| {
                entry.file_name().to_string_lossy().starts_with(&prefix)
                    && entry.file_type().is_ok_and(|kind| kind.is_dir())
            })
            .count();
        assert!(
            matching <= 1,
            "two transient builds may retain at most one process/root cache directory"
        );
        assert!(
            !stale_file.exists(),
            "reusing a transient cache directory must clear stale scratch files"
        );
        drop((first, second));
        remove_matching();
    }

    #[test]
    fn transient_search_cache_sweep_reaps_aged_legacy_dir_and_counts_bytes() {
        let root = tempfile::tempdir().expect("temporary sweep root");
        let legacy = transient_search_cache_test_dir(
            root.path(),
            "aft-search-cache.1111111111111111.4242.123456789",
            b"legacy cache bytes",
        );
        age_transient_search_cache_test_dir(&legacy);

        let summary = sweep_transient_search_cache_dirs_with_limits(
            root.path(),
            TRANSIENT_SEARCH_CACHE_MIN_AGE,
            Duration::from_secs(1),
            10,
        );

        assert!(
            !legacy.exists(),
            "an aged legacy transient cache must be reaped"
        );
        assert_eq!(summary.removed, 1);
        assert_eq!(summary.bytes, b"legacy cache bytes".len() as u64);
    }

    #[test]
    fn transient_search_cache_sweep_age_guard_spares_fresh_dir_until_aged() {
        let root = tempfile::tempdir().expect("temporary sweep root");
        let fresh = transient_search_cache_test_dir(
            root.path(),
            "aft-search-cache.2222222222222222.4242",
            b"fresh cache bytes",
        );

        let fresh_summary = sweep_transient_search_cache_dirs_with_limits(
            root.path(),
            TRANSIENT_SEARCH_CACHE_MIN_AGE,
            Duration::from_secs(1),
            10,
        );
        assert!(fresh.exists(), "the age guard must protect a fresh cache");
        assert_eq!(fresh_summary.skipped_fresh, 1);

        age_transient_search_cache_test_dir(&fresh);
        let aged_summary = sweep_transient_search_cache_dirs_with_limits(
            root.path(),
            TRANSIENT_SEARCH_CACHE_MIN_AGE,
            Duration::from_secs(1),
            10,
        );
        assert!(
            !fresh.exists(),
            "aging the same directory must make the age guard reap it"
        );
        assert_eq!(aged_summary.removed, 1);
    }

    #[test]
    fn transient_search_cache_sweep_resumes_after_entry_budget() {
        let root = tempfile::tempdir().expect("temporary sweep root");
        let first = transient_search_cache_test_dir(
            root.path(),
            "aft-search-cache.3333333333333333.4242",
            b"first cache bytes",
        );
        let second = transient_search_cache_test_dir(
            root.path(),
            "aft-search-cache.4444444444444444.4242",
            b"second cache bytes",
        );
        age_transient_search_cache_test_dir(&first);
        age_transient_search_cache_test_dir(&second);

        let first_pass = sweep_transient_search_cache_dirs_with_limits(
            root.path(),
            TRANSIENT_SEARCH_CACHE_MIN_AGE,
            Duration::from_secs(1),
            1,
        );
        assert!(
            first_pass.budget_exhausted,
            "the one-entry cap must stop the pass"
        );
        assert_eq!(first_pass.removed, 1);
        assert_eq!(
            [first.exists(), second.exists()]
                .into_iter()
                .filter(|exists| *exists)
                .count(),
            1,
            "the entry budget must leave one aged directory for a later pass"
        );

        let second_pass = sweep_transient_search_cache_dirs_with_limits(
            root.path(),
            TRANSIENT_SEARCH_CACHE_MIN_AGE,
            Duration::from_secs(1),
            10,
        );
        assert_eq!(second_pass.removed, 1);
        assert!(!first.exists() && !second.exists());
    }

    /// Our scratch must not sit loose in the shared temporary directory, whose
    /// listing is the fleet's, not ours.
    #[test]
    fn transient_cache_lives_in_a_directory_of_our_own() {
        let root = tempfile::tempdir().expect("temporary project root");
        let cache_dir = transient_search_cache_dir(root.path());

        let parent = cache_dir.parent().expect("cache dir has a parent");
        assert_eq!(
            parent.file_name().and_then(|name| name.to_str()),
            Some(TRANSIENT_SEARCH_CACHE_DIR_NAME)
        );
        assert_eq!(parent.parent(), Some(std::env::temp_dir().as_path()));
    }

    /// The sweep reads a directory it does not own the size of. A listing must
    /// cost the caller a bounded number of entries whatever is in there: the
    /// unbounded version pinned six executor workers for minutes on a $TMPDIR
    /// holding 290,697 entries and starved route binds (2026-09-19).
    #[test]
    fn sweep_stops_reading_a_fat_directory_at_the_scan_limit() {
        let root = tempfile::tempdir().expect("temporary sweep root");
        let litter = TRANSIENT_SEARCH_CACHE_SCAN_LIMIT + 256;
        for index in 0..litter {
            fs::write(root.path().join(format!("foreign-{index}")), b"x")
                .expect("write foreign temp entry");
        }

        let summary = sweep_transient_search_cache_dirs_with_limits(
            root.path(),
            TRANSIENT_SEARCH_CACHE_MIN_AGE,
            TRANSIENT_SEARCH_CACHE_SWEEP_BUDGET,
            TRANSIENT_SEARCH_CACHE_SWEEP_LIMIT,
        );

        assert!(
            summary.examined <= TRANSIENT_SEARCH_CACHE_SCAN_LIMIT,
            "read {} of {litter} entries; the scan limit is {TRANSIENT_SEARCH_CACHE_SCAN_LIMIT}",
            summary.examined
        );
        assert!(
            summary.budget_exhausted,
            "a truncated listing must say so rather than read as a complete pass"
        );
        // Nothing of ours was in there, so a bounded read is not a licence to
        // report work it did not do.
        assert_eq!(summary.scanned, 0);
        assert_eq!(summary.removed, 0);
    }

    /// The cache writer stores paths relative to the root and refuses the
    /// whole cache if one entry lies outside it. An update for an outside
    /// path must therefore never enter the index, whichever caller sends it.
    #[test]
    fn update_for_a_path_outside_the_root_is_not_admitted() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let base = fs::canonicalize(dir.path()).expect("canonical temp dir");
        let project = base.join("project");
        let sibling = base.join("project-sibling");
        fs::create_dir_all(&project).expect("create project dir");
        fs::create_dir_all(&sibling).expect("create sibling dir");
        fs::write(project.join("inside.rs"), "fn inside() {}").expect("write inside");
        let outside = sibling.join("outside.rs");
        fs::write(&outside, "fn outside() {}").expect("write outside");

        let mut index = SearchIndex::build(&project);
        let files_before = index.path_to_id.len();
        index.update_file(&outside);
        assert!(!index.path_to_id.contains_key(&outside));
        assert_eq!(index.path_to_id.len(), files_before);
        let added = project.join("added.rs");
        fs::write(&added, "fn added() {}").expect("write added");
        index.update_file(&added);
        assert!(
            index.path_to_id.contains_key(&added),
            "in-root updates still apply"
        );
        assert!(index.write_to_disk(&base.join("cache"), None));
    }

    fn lexical_rank_mixed_storage_fixture() -> (tempfile::TempDir, SearchIndex) {
        let dir = tempfile::tempdir().expect("create temp dir");
        let project = dir.path().join("project");
        fs::create_dir_all(&project).expect("create project dir");
        for index in 0..205 {
            fs::write(
                project.join(format!("file_{index:03}.txt")),
                format!("abcdefghij sharedalpha marker_{index}"),
            )
            .expect("write base fixture file");
        }

        let cache_dir = dir.path().join("cache");
        let mut built = SearchIndex::build(&project);
        assert!(built.write_to_disk(&cache_dir, None));
        let mut index = SearchIndex::read_from_disk(&cache_dir, &project).expect("load base index");

        let replaced = project.join("file_000.txt");
        index.remove_file(&replaced);
        index.index_file(&replaced, b"abcdefghij sharedalpha replacement_delta");

        let removed = project.join("file_001.txt");
        index.remove_file(&removed);

        let added = project.join("added_delta.txt");
        fs::write(&added, "abcdefghij sharedalpha added_delta").expect("write delta file");
        index.index_file(&added, b"abcdefghij sharedalpha added_delta");

        assert!(index.base.is_some());
        assert!(!index.delta.postings.is_empty());
        assert!(!index.delta.superseded.is_empty());
        (dir, index)
    }

    fn postings_for_trigram_materialized_reference(
        index: &SearchIndexSnapshot,
        trigram: u32,
        filter: Option<PostingFilter>,
    ) -> Vec<u32> {
        let mut matches = Vec::new();
        if let Some(base_entry) = index
            .base
            .as_ref()
            .and_then(|base| base.lookup_entry(trigram))
        {
            if let Some(base) = &index.base {
                if let Ok(postings) = base.read_postings(base_entry) {
                    matches.reserve(postings.len());
                    for posting in postings {
                        if index.delta.superseded.contains(&posting.file_id) {
                            continue;
                        }
                        if !posting_matches_filter(&posting, filter) {
                            continue;
                        }
                        if index.is_active_file(posting.file_id) {
                            matches.push(posting.file_id);
                        }
                    }
                }
            }
        }
        if let Some(postings) = index.delta.postings.get(&trigram) {
            matches.reserve(postings.len());
            for posting in postings {
                if !posting_matches_filter(posting, filter) {
                    continue;
                }
                if index.is_active_file(posting.file_id) {
                    matches.push(posting.file_id);
                }
            }
        }
        if matches.len() > 1 {
            matches.sort_unstable();
            matches.dedup();
        }
        matches
    }

    fn assert_rank_matches_reference(
        index: &SearchIndex,
        query_trigrams: &[u32],
        candidate_filter: Option<&dyn Fn(&Path) -> bool>,
        max_files: usize,
    ) -> LexicalRankResult {
        let snapshot = index.snapshot();
        let expected = lexical_rank_with_stats_reference(
            &snapshot,
            query_trigrams,
            candidate_filter,
            max_files,
        );
        let actual = snapshot.lexical_rank_with_stats(query_trigrams, candidate_filter, max_files);
        assert_eq!(actual.files, expected.files);
        assert_eq!(actual.engine_capped, expected.engine_capped);
        actual
    }

    #[test]
    fn cached_path_under_root_allows_missing_lexical_child() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let project = dir.path().join("project");
        fs::create_dir_all(&project).expect("create project dir");
        let root = fs::canonicalize(&project).expect("canonicalize project");

        let path = cached_path_under_root(&root, Path::new("future/file.rs"))
            .expect("missing child should fall back to lexical validation");

        assert_eq!(path, root.join("future/file.rs"));
    }

    fn count_cached_path_canonicalizations<T>(
        reload: impl FnOnce() -> T,
    ) -> (T, CachedPathCanonicalizationWork) {
        CACHED_PATH_CANONICALIZATION_WORK.with(|slot| {
            assert!(slot.borrow().is_none());
            *slot.borrow_mut() = Some(CachedPathCanonicalizationWork::default());
        });
        let result = reload();
        let work = CACHED_PATH_CANONICALIZATION_WORK
            .with(|slot| slot.borrow_mut().take().expect("reload counters"));
        (result, work)
    }

    fn measure_warm_reload_canonicalizations(root: &Path) {
        use crate::parser::{FileParser, SymbolCache};
        use crate::semantic_index::SemanticIndex;

        let root = fs::canonicalize(root).expect("canonical project root");
        let storage = tempfile::tempdir().expect("cache storage");
        let search_dir = storage.path().join("search");
        fs::create_dir_all(&search_dir).unwrap();
        let mut search = SearchIndex::build_with_limit(&root, DEFAULT_MAX_FILE_SIZE);
        let paths: Vec<_> = search.files.iter().map(|file| file.path.clone()).collect();
        assert!(!paths.is_empty());
        assert!(search.write_to_disk(&search_dir, None));

        let mut parser = FileParser::new();
        for path in &paths {
            if crate::parser::detect_language(path).is_some() {
                // Unsupported or malformed source need not produce a symbol cache entry.
                let _ = parser.extract_symbols(path);
            }
        }
        let shared = parser.symbol_cache();
        let mut symbols = shared.write().unwrap();
        symbols.set_project_root(root.clone());
        let symbol_count = symbols.len();
        assert!(symbol_count > 0);
        crate::symbol_cache_disk::write_to_disk(&symbols, storage.path(), "reload").unwrap();
        drop(symbols);

        // Use real source chunks but deterministic vectors: no embedding service is needed.
        let semantic = SemanticIndex::build(
            &root,
            &paths,
            &mut |texts: Vec<String>| Ok(texts.iter().map(|_| vec![1.0, 0.0]).collect()),
            128,
        )
        .expect("build semantic fixture");
        assert!(semantic.entry_count() > 0);
        assert!(semantic.write_to_disk(storage.path(), "reload"));

        let (restored_search, search_work) = count_cached_path_canonicalizations(|| {
            SearchIndex::read_from_disk(&search_dir, &root).expect("reload search")
        });
        assert_eq!(restored_search.file_count(), paths.len());
        let (restored_symbols, symbol_work) = count_cached_path_canonicalizations(|| {
            let mut restored = SymbolCache::new();
            restored.load_from_disk(storage.path(), "reload", &root)
        });
        assert_eq!(restored_symbols, symbol_count);
        let (restored_semantic, semantic_work) = count_cached_path_canonicalizations(|| {
            SemanticIndex::read_from_disk(storage.path(), "reload", &root, false, None)
                .expect("reload semantic")
        });
        assert_eq!(restored_semantic.entry_count(), semantic.entry_count());

        // Re-publish the same files as a delta to cover tombstones and embedded snapshots.
        semantic.extend_dirty_paths_for_test([paths[0].clone()]);
        assert!(semantic.write_to_disk(storage.path(), "reload"));
        let (segmented, segment_work) = count_cached_path_canonicalizations(|| {
            SemanticIndex::read_from_disk(storage.path(), "reload", &root, false, None)
                .expect("reload semantic segments")
        });
        assert_eq!(segmented.entry_count(), semantic.entry_count());
        let bytes = fs::read(storage.path().join("semantic/reload/semantic.bin")).unwrap();
        let (from_bytes, bytes_work) = count_cached_path_canonicalizations(|| {
            SemanticIndex::from_bytes(&bytes, &root).expect("decode semantic bytes")
        });
        assert_eq!(from_bytes.entry_count(), semantic.entry_count());

        let measurements = [
            ("search", search_work),
            ("symbols", symbol_work),
            ("semantic", semantic_work),
            ("semantic_segments", segment_work),
            ("semantic_bytes", bytes_work),
        ];
        for (loader, work) in &measurements {
            eprintln!(
                "warm reload {loader}: files={}, canonicalize_calls={}, parent_lookups={}, distinct_parents={}",
                paths.len(), work.canonicalize_calls, work.parent_lookups, work.distinct_parents.len()
            );
        }
        for (loader, work) in measurements {
            assert!(work.parent_lookups > work.distinct_parents.len());
            assert_eq!(
                work.canonicalize_calls,
                work.distinct_parents.len(),
                "{loader} must share one parent memo across the whole reload"
            );
        }
    }

    #[test]
    fn warm_reload_canonicalizes_each_parent_once() {
        let project = tempfile::tempdir().unwrap();
        for directory in ["src", "src/nested", "tests"] {
            fs::create_dir_all(project.path().join(directory)).unwrap();
            for file in 0..4 {
                fs::write(
                    project
                        .path()
                        .join(directory)
                        .join(format!("file{file}.rs")),
                    "pub fn first() {}\npub fn second() {}\n",
                )
                .unwrap();
            }
        }
        measure_warm_reload_canonicalizations(project.path());
    }

    #[test]
    #[ignore = "profiles real repository sources and persists temporary caches"]
    fn warm_reload_repository_canonicalization_counts() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        measure_warm_reload_canonicalizations(&root);
    }

    #[cfg(unix)]
    fn cached_path_before_reload_memo(root: &Path, relative_path: &Path) -> Option<PathBuf> {
        // Resolve each parent afresh, as before reload-wide memo sharing, so this
        // reference checks returned spellings without using the optimized helper.
        let relative = validate_cached_relative_path(relative_path)?;
        let root = normalize_path(root);
        let full = normalize_path(&root.join(relative));
        let canonical = match fs::symlink_metadata(&full) {
            Ok(metadata) if !metadata.file_type().is_symlink() => fs::canonicalize(full.parent()?)
                .ok()?
                .join(full.file_name()?),
            Ok(_) => fs::canonicalize(&full).ok()?,
            Err(_) => return is_within_search_root(&root, &full).then_some(full),
        };
        if is_within_search_root(&root, &canonical) {
            Some(full)
        } else {
            let canonical_root = fs::canonicalize(&root).ok()?;
            is_within_search_root(&canonical_root, &canonical).then_some(full)
        }
    }

    #[cfg(unix)]
    fn assert_reload_path_identity(project: &Path) {
        let root = fs::canonicalize(project).unwrap();
        let source = root.join("MixedCase");
        let outside = tempfile::tempdir().unwrap();
        fs::create_dir_all(&source).unwrap();
        fs::write(source.join("File.rs"), "pub fn identity() {}\n").unwrap();
        fs::write(outside.path().join("secret.rs"), "secret").unwrap();
        std::os::unix::fs::symlink(&source, root.join("linked")).unwrap();
        std::os::unix::fs::symlink(outside.path(), root.join("escape")).unwrap();
        std::os::unix::fs::symlink(source.join("File.rs"), root.join("file-link.rs")).unwrap();
        std::os::unix::fs::symlink(
            outside.path().join("secret.rs"),
            root.join("secret-link.rs"),
        )
        .unwrap();
        let alias_directory = tempfile::tempdir().unwrap();
        let alias = alias_directory.path().join("root-alias");
        std::os::unix::fs::symlink(&root, &alias).unwrap();
        let mut roots = vec![root.clone(), alias];
        #[cfg(target_os = "macos")]
        {
            let var_alias = Path::new("/").join(root.strip_prefix("/private").unwrap());
            assert!(var_alias.starts_with("/var/"));
            roots.push(var_alias);
        }
        let case_insensitive = source.join("file.rs").exists();
        if case_insensitive {
            roots.push(
                root.parent()
                    .unwrap()
                    .join(root.file_name().unwrap().to_str().unwrap().to_uppercase()),
            );
        }
        let relatives = [
            "MixedCase/File.rs",
            "mixedcase/file.rs",
            "MixedCase/./File.rs",
            "linked/File.rs",
            "linked/missing.rs",
            "escape/secret.rs",
            "escape/missing.rs",
            "file-link.rs",
            "secret-link.rs",
            "missing/child.rs",
            "../outside.rs",
            "/absolute.rs",
        ];
        let mut comparisons = 0;
        for spelling in &roots {
            let mut memo = ParentCanonicalizationMemo::default();
            // Repeat with the same memo to exercise hits as well as misses.
            for _ in 0..2 {
                for relative in relatives {
                    let old = cached_path_before_reload_memo(spelling, Path::new(relative));
                    let new =
                        cached_path_under_root_with_memo(spelling, Path::new(relative), &mut memo);
                    assert_eq!(
                        new.as_deref().map(Path::as_os_str),
                        old.as_deref().map(Path::as_os_str),
                        "returned bytes differ for root={spelling:?}, relative={relative:?}"
                    );
                    comparisons += 1;
                }
            }
        }
        assert!(cached_path_before_reload_memo(&root, Path::new("escape/secret.rs")).is_none());
        eprintln!("reload identity: {comparisons} byte-identical results, case_insensitive={case_insensitive}, roots={}", roots.len());
    }

    #[cfg(unix)]
    #[test]
    fn warm_reload_path_identity_matches_old_resolver() {
        #[cfg(target_os = "macos")]
        let project = tempfile::Builder::new()
            .prefix("aft-identity-")
            .tempdir_in("/var/tmp")
            .unwrap();
        #[cfg(not(target_os = "macos"))]
        let project = tempfile::tempdir().unwrap();
        assert_reload_path_identity(project.path());
    }

    #[test]
    fn deleted_path_uses_one_parent_canonicalization() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let project = dir.path().join("project");
        let source = project.join("src");
        fs::create_dir_all(&source).expect("create source dir");
        let deleted = source.join("deleted.rs");
        let expected = fs::canonicalize(&source)
            .expect("canonicalize source dir")
            .join("deleted.rs");
        let mut canonical_parents = ParentCanonicalizationMemo::default();
        let mut canonicalize_calls = 0;
        let mut canonicalize = |path: &Path| {
            canonicalize_calls += 1;
            fs::canonicalize(path)
        };

        let resolved = canonicalize_existing_or_deleted_path_with_memo_and_canonicalizer(
            &deleted,
            &mut canonical_parents,
            &mut canonicalize,
        );

        assert_eq!(resolved, expected);
        assert_eq!(canonicalize_calls, 1, "only the parent is canonicalized");
    }

    #[cfg(unix)]
    #[test]
    fn cached_path_memo_handles_mixed_entries_without_relaxing_symlink_checks() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let project = dir.path().join("project");
        let source = project.join("src");
        let outside = dir.path().join("outside");
        fs::create_dir_all(&source).expect("create source dir");
        fs::create_dir_all(&outside).expect("create outside dir");
        fs::write(source.join("present.rs"), "present").expect("write present file");
        fs::write(outside.join("secret.rs"), "secret").expect("write outside file");
        std::os::unix::fs::symlink(source.join("present.rs"), project.join("inside.rs"))
            .expect("create in-root symlink");
        std::os::unix::fs::symlink(outside.join("secret.rs"), project.join("outside.rs"))
            .expect("create escaping symlink");
        let root = fs::canonicalize(&project).expect("canonicalize project");
        let mut canonical_parents = ParentCanonicalizationMemo::default();
        let mut canonicalized_paths = Vec::new();
        let mut canonicalize = |path: &Path| {
            canonicalized_paths.push(path.to_path_buf());
            fs::canonicalize(path)
        };

        assert_eq!(
            cached_path_under_root_with_memo_and_canonicalizer(
                &root,
                Path::new("src/present.rs"),
                &mut canonical_parents,
                &mut canonicalize,
            ),
            Some(root.join("src/present.rs"))
        );
        assert_eq!(
            cached_path_under_root_with_memo_and_canonicalizer(
                &root,
                Path::new("src/deleted.rs"),
                &mut canonical_parents,
                &mut canonicalize,
            ),
            Some(root.join("src/deleted.rs"))
        );
        assert_eq!(
            cached_path_under_root_with_memo_and_canonicalizer(
                &root,
                Path::new("inside.rs"),
                &mut canonical_parents,
                &mut canonicalize,
            ),
            Some(root.join("inside.rs"))
        );
        assert!(cached_path_under_root_with_memo_and_canonicalizer(
            &root,
            Path::new("outside.rs"),
            &mut canonical_parents,
            &mut canonicalize,
        )
        .is_none());

        assert_eq!(
            canonicalized_paths,
            vec![
                root.join("src"),
                root.join("inside.rs"),
                root.join("outside.rs"),
                root.clone(),
            ],
            "siblings reuse their parent while symlinks still resolve individually"
        );
    }

    #[cfg(unix)]
    #[test]
    fn cached_path_under_root_rejects_symlink_escape() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let project = dir.path().join("project");
        let outside = dir.path().join("outside");
        fs::create_dir_all(&project).expect("create project dir");
        fs::create_dir_all(&outside).expect("create outside dir");
        fs::write(outside.join("secret.txt"), "secret").expect("write outside file");
        std::os::unix::fs::symlink(&outside, project.join("link")).expect("create symlink");
        let root = fs::canonicalize(&project).expect("canonicalize project");

        assert!(cached_path_under_root(&root, Path::new("link/secret.txt")).is_none());
    }

    #[test]
    fn trigram_memory_estimate_is_zero_when_empty_and_nonzero_when_populated() {
        let mut index = SearchIndex::new();
        assert_eq!(index.estimated_memory().estimated_bytes, Some(0));
        index.index_file(Path::new("memory-estimate.rs"), b"fn memory_estimate() {}");
        let estimate = index.estimated_memory();
        assert!(estimate.estimated_bytes.unwrap() > 0);
        assert!(estimate.counts["delta_postings"] > 0);
        assert_eq!(estimate.counts["base_postings_resident_bytes"], 0);
    }

    #[test]
    fn extract_trigrams_tracks_next_char_and_position() {
        let trigrams = extract_trigrams(b"Rust");
        assert_eq!(trigrams.len(), 2);
        assert_eq!(trigrams[0], (pack_trigram(b'r', b'u', b's'), b't', 0));
        assert_eq!(
            trigrams[1],
            (pack_trigram(b'u', b's', b't'), EOF_SENTINEL, 1)
        );
    }

    #[test]
    fn index_file_trigram_filters_match_legacy_extraction() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let path = dir.path().join("sample.txt");
        let content = b"Rust rust RUST\nxy";
        fs::write(&path, content).expect("write sample");

        let mut expected = BTreeMap::new();
        for (trigram, next_char, position) in extract_trigrams(content) {
            let entry: &mut PostingFilter = expected.entry(trigram).or_default();
            entry.next_mask |= mask_for_next_char(next_char);
            entry.loc_mask |= mask_for_position(position);
        }

        let mut index = SearchIndex::new();
        index.project_root = dir.path().to_path_buf();
        index.index_file(&path, content);

        let file_id = *index.path_to_id.get(&path).expect("file indexed");
        let file_trigrams = index
            .delta_file_trigrams
            .get(&file_id)
            .expect("delta file trigrams");
        assert_eq!(file_trigrams, &expected.keys().copied().collect::<Vec<_>>());
        for (trigram, filter) in expected {
            let postings = index
                .delta
                .postings
                .get(&trigram)
                .expect("delta posting list");
            assert_eq!(postings.len(), 1);
            assert_eq!(postings[0].file_id, file_id);
            assert_eq!(postings[0].next_mask, filter.next_mask);
            assert_eq!(postings[0].loc_mask, filter.loc_mask);
        }
    }

    #[test]
    fn decompose_regex_extracts_literals_and_alternations() {
        let query = decompose_regex("abc(def|ghi)xyz");
        assert!(query.and_trigrams.contains(&pack_trigram(b'a', b'b', b'c')));
        assert!(query.and_trigrams.contains(&pack_trigram(b'x', b'y', b'z')));
        assert_eq!(query.or_groups.len(), 1);
        assert!(query.or_groups[0].contains(&pack_trigram(b'd', b'e', b'f')));
        assert!(query.or_groups[0].contains(&pack_trigram(b'g', b'h', b'i')));
    }

    #[test]
    fn candidates_intersect_posting_lists() {
        let mut index = SearchIndex::new();
        let dir = tempfile::tempdir().expect("create temp dir");
        let alpha = dir.path().join("alpha.txt");
        let beta = dir.path().join("beta.txt");
        fs::write(&alpha, "abcdef").expect("write alpha");
        fs::write(&beta, "abcxyz").expect("write beta");
        index.project_root = dir.path().to_path_buf();
        index.index_file(&alpha, b"abcdef");
        index.index_file(&beta, b"abcxyz");

        let query = RegexQuery {
            and_trigrams: vec![
                pack_trigram(b'a', b'b', b'c'),
                pack_trigram(b'd', b'e', b'f'),
            ],
            ..RegexQuery::default()
        };

        let candidates = index.candidates(&query);
        assert_eq!(candidates.len(), 1);
        assert_eq!(index.files[candidates[0] as usize].path, alpha);
    }

    #[test]
    fn candidates_apply_bloom_filters() {
        let mut index = SearchIndex::new();
        let dir = tempfile::tempdir().expect("create temp dir");
        let file = dir.path().join("sample.txt");
        fs::write(&file, "abcd efgh").expect("write sample");
        index.project_root = dir.path().to_path_buf();
        index.index_file(&file, b"abcd efgh");

        let trigram = pack_trigram(b'a', b'b', b'c');
        let matching_filter = PostingFilter {
            next_mask: mask_for_next_char(b'd'),
            loc_mask: mask_for_position(0),
        };
        let non_matching_filter = PostingFilter {
            next_mask: mask_for_next_char(b'z'),
            loc_mask: mask_for_position(0),
        };

        assert_eq!(
            index
                .postings_for_trigram(trigram, Some(matching_filter))
                .len(),
            1
        );
        assert!(index
            .postings_for_trigram(trigram, Some(non_matching_filter))
            .is_empty());
    }

    #[test]
    fn direct_base_decode_matches_materialized_reference_for_all_storage_and_filters() {
        let (_dir, index) = lexical_rank_mixed_storage_fixture();
        let snapshot = index.snapshot();
        let base_only = pack_trigram(b'm', b'a', b'r');
        let base_and_delta = pack_trigram(b'a', b'b', b'c');

        assert!(snapshot.delta.postings.get(&base_only).is_none());
        assert!(snapshot.delta.postings.contains_key(&base_and_delta));
        assert!(!snapshot.delta.superseded.is_empty());

        let filters = [
            None,
            Some(PostingFilter::default()),
            Some(PostingFilter {
                next_mask: mask_for_next_char(b'd'),
                loc_mask: 0,
            }),
            Some(PostingFilter {
                next_mask: mask_for_next_char(b'z'),
                loc_mask: 0,
            }),
            Some(PostingFilter {
                next_mask: 0,
                loc_mask: mask_for_position(0),
            }),
            Some(PostingFilter {
                next_mask: mask_for_next_char(b'd'),
                loc_mask: mask_for_position(17),
            }),
            Some(PostingFilter {
                next_mask: mask_for_next_char(b'z'),
                loc_mask: mask_for_position(17),
            }),
        ];

        for trigram in [base_only, base_and_delta] {
            for filter in filters {
                let expected =
                    postings_for_trigram_materialized_reference(&snapshot, trigram, filter);
                let actual = snapshot.postings_for_trigram(trigram, filter);
                assert_eq!(
                    actual, expected,
                    "trigram={trigram:#08x}, filter={filter:?}"
                );
                assert!(actual
                    .iter()
                    .all(|file_id| !snapshot.delta.superseded.contains(file_id)));
            }
        }

        let unfiltered = snapshot.postings_for_trigram(base_and_delta, None);
        let loc_only = snapshot.postings_for_trigram(
            base_and_delta,
            Some(PostingFilter {
                next_mask: 0,
                loc_mask: mask_for_position(31),
            }),
        );
        assert_eq!(loc_only, unfiltered);
    }

    #[test]
    fn base_delta_readd_masks_base_and_keeps_postings_sorted() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let project = dir.path().join("project");
        fs::create_dir_all(&project).expect("create project dir");
        let project = fs::canonicalize(&project).expect("canonical project root");
        let a = project.join("a.txt");
        let b = project.join("b.txt");
        fs::write(&a, "abc old").expect("write a");
        fs::write(&b, "abc base").expect("write b");

        let mut built = SearchIndex::build(&project);
        let cache_dir = dir.path().join("cache");
        built.write_to_disk(&cache_dir, None);
        let mut index = SearchIndex::read_from_disk(&cache_dir, &project).expect("load base");
        assert_eq!(index.base_file_count, 2);

        let old_a_id = *index.path_to_id.get(&a).expect("original a id");
        let b_id = *index.path_to_id.get(&b).expect("original b id");
        index.remove_file(&a);
        index.index_file(&a, b"abc new");
        let new_id = *index.path_to_id.get(&a).expect("re-added file id");
        assert!(new_id >= index.base_file_count);
        let abc = pack_trigram(b'a', b'b', b'c');
        let ids = index.postings_for_trigram(abc, None);
        assert_eq!(ids, {
            let mut expected = vec![b_id, new_id];
            expected.sort_unstable();
            expected
        });
        assert!(!ids.contains(&old_a_id));
    }

    #[test]
    fn live_file_identities_track_edits_deletions_and_additions_by_relative_path() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let project = dir.path().join("project");
        fs::create_dir_all(project.join("sub")).expect("create project dir");
        let project = fs::canonicalize(project).expect("canonicalize project");
        let edited = project.join("sub/edited.txt");
        let deleted = project.join("deleted.txt");
        let binary = project.join("blob.bin");
        fs::write(&edited, "abc old").expect("write edited");
        fs::write(&deleted, "abc gone").expect("write deleted");
        fs::write(&binary, b"\0\x01\x02binary").expect("write binary");

        let mut built = SearchIndex::build(&project);
        let cache_dir = dir.path().join("cache");
        assert!(built.write_to_disk(&cache_dir, None));
        let mut index = SearchIndex::read_from_disk(&cache_dir, &project).expect("load base");
        let identity = |text: &str| {
            (
                text.len() as u64,
                cache_freshness::hash_bytes(text.as_bytes()),
            )
        };
        let live = index.live_file_identities();
        assert_eq!(live.len(), 3, "{live:?}");
        assert_eq!(live["sub/edited.txt"], identity("abc old"));
        assert_eq!(live["deleted.txt"], identity("abc gone"));
        // A binary file is tracked but never hashed.
        assert_eq!(live["blob.bin"].1, cache_freshness::zero_hash());

        // Editing a file twice supersedes its base entry and then its first
        // delta entry; only the newest content is reported.
        fs::write(&edited, "abc new").expect("edit once");
        index.update_file(&edited);
        fs::write(&edited, "abc newer").expect("edit twice");
        index.update_file(&edited);
        fs::remove_file(&deleted).expect("delete file");
        index.remove_file(&deleted);
        let added = project.join("added.txt");
        fs::write(&added, "abc added").expect("write added");
        index.update_file(&added);

        let live = index.live_file_identities();
        assert_eq!(live.len(), 3, "{live:?}");
        assert_eq!(live["sub/edited.txt"], identity("abc newer"));
        assert_eq!(live["added.txt"], identity("abc added"));
        assert!(!live.contains_key("deleted.txt"));
    }

    #[test]
    fn snapshot_started_before_edit_keeps_coherent_pre_edit_postings() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let project = dir.path().join("project");
        fs::create_dir_all(&project).expect("create project dir");
        let project = fs::canonicalize(project).expect("canonicalize project");
        let file = project.join("source.txt");
        fs::write(&file, "old_generation marker").expect("write old source");

        let mut built = SearchIndex::build(&project);
        let cache_dir = dir.path().join("cache");
        assert!(built.write_to_disk(&cache_dir, None));
        let mut index = SearchIndex::read_from_disk(&cache_dir, &project).expect("load base index");
        index.ready = true;
        let old_file_id = *index.path_to_id.get(&file).expect("old file id");
        let index = std::sync::RwLock::new(index);

        let before_edit = {
            let guard = index.read().expect("read index");
            let snapshot = guard.snapshot();
            assert!(Arc::ptr_eq(&snapshot.delta, &guard.delta));
            snapshot
        };

        fs::write(&file, "new_generation marker").expect("write new source");
        index.write().expect("write index").update_file(&file);

        let after_edit = index.read().expect("read updated index").snapshot();
        let new_file_id = *after_edit.path_to_id.get(&file).expect("new file id");
        assert!(!Arc::ptr_eq(&before_edit.delta, &after_edit.delta));
        assert!(!before_edit.delta.superseded.contains(&old_file_id));
        assert!(after_edit.delta.superseded.contains(&old_file_id));

        let old_trigram = pack_trigram(b'o', b'l', b'd');
        let new_trigram = pack_trigram(b'n', b'e', b'w');
        assert_eq!(
            before_edit.postings_for_trigram(old_trigram, None),
            vec![old_file_id]
        );
        assert!(before_edit
            .postings_for_trigram(new_trigram, None)
            .is_empty());
        assert!(after_edit
            .postings_for_trigram(old_trigram, None)
            .is_empty());
        assert_eq!(
            after_edit.postings_for_trigram(new_trigram, None),
            vec![new_file_id]
        );

        let dirty_result = index.read().expect("read dirty index").grep(
            "new_generation",
            true,
            &[],
            &[],
            &project,
            100,
        );
        let reference_cache = dir.path().join("reference-cache");
        let reference = SearchIndex::build_with_limit_to_cache_dir(
            &project,
            DEFAULT_MAX_FILE_SIZE,
            &reference_cache,
        );
        let reference_result = reference.grep("new_generation", true, &[], &[], &project, 100);
        assert_eq!(dirty_result.matches, reference_result.matches);
        assert_eq!(dirty_result.total_matches, reference_result.total_matches);
    }

    #[test]
    fn lexical_rank_cached_postings_match_reference_for_base_delta_and_superseded_files() {
        let (_dir, index) = lexical_rank_mixed_storage_fixture();
        let long_query = query_trigrams_from_tokens(&["abcdefghij"]);
        assert!(long_query.len() > 3);
        let long_result = assert_rank_matches_reference(&index, &long_query, None, 1_000);
        assert!(long_result.engine_capped);

        let short_query = query_trigrams_from_tokens(&["abc"]);
        assert_eq!(short_query.len(), 1);
        assert_rank_matches_reference(&index, &short_query, None, 100);

        let mixed_query = query_trigrams_from_tokens(&["sharedalpha", "absentzzz"]);
        let production_only = |path: &Path| !path.ends_with("file_002.txt");
        assert_rank_matches_reference(&index, &mixed_query, Some(&production_only), 25);

        let mut duplicate_query = query_trigrams_from_tokens(&["abcdefghij"]);
        duplicate_query.push(duplicate_query[0]);
        assert_rank_matches_reference(&index, &duplicate_query, None, 40);
    }

    #[cfg(debug_assertions)]
    #[test]
    fn lexical_rank_reads_each_distinct_query_posting_list_once() {
        let (_dir, index) = lexical_rank_mixed_storage_fixture();
        let mut query = query_trigrams_from_tokens(&["abcdefghij", "sharedalpha"]);
        query.push(query[0]);
        let distinct_trigrams = query.iter().copied().collect::<HashSet<_>>().len();

        reset_postings_for_trigram_count_for_debug();
        let result = index
            .snapshot()
            .lexical_rank_with_stats(&query, None, 1_000);

        assert!(result.files.len() > 1);
        assert_eq!(
            postings_for_trigram_count_for_debug(),
            distinct_trigrams,
            "candidate discovery and scoring must share query-local posting lists"
        );
    }

    #[test]
    fn borrow_only_root_skips_shared_lock_persist_and_streaming_spills() {
        let dir = tempfile::tempdir().expect("temp dir");
        let project = dir.path().join("project");
        fs::create_dir_all(&project).expect("project dir");
        fs::write(project.join("source.txt"), "borrow only search index").expect("source file");
        let project_key = "shared-artifact-key".to_string();
        let cache_dir = dir.path().join("index").join(&project_key);
        crate::root_cache::configure_artifact_access(&project, &project_key, true);

        let _lock = CacheLock::acquire(&cache_dir, &project).expect("borrow-only lock downgrade");
        assert!(!cache_dir.join("cache.lock").exists());

        let mut index =
            SearchIndex::build_with_limit_to_cache_dir(&project, DEFAULT_MAX_FILE_SIZE, &cache_dir);
        assert!(!index.ready);
        index.write_to_disk(&cache_dir, None);

        assert!(!cache_dir.join("cache.bin").exists());
        assert!(!cache_dir.exists());
    }

    #[test]
    fn write_to_disk_compacts_base_and_delta() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let project = dir.path().join("project");
        fs::create_dir_all(&project).expect("create project dir");
        let file = project.join("src.txt");
        fs::write(&file, "abcdef").expect("write source");
        let mut index = SearchIndex::build(&project);
        let cache_dir = dir.path().join("cache");
        index.write_to_disk(&cache_dir, None);
        fs::write(&file, "abcxyz").expect("edit source");
        index.update_file(&file);
        assert!(!index.delta.postings.is_empty());
        index.write_to_disk(&cache_dir, None);
        assert!(index.delta.postings.is_empty());
        assert!(index.delta.superseded.is_empty());
        assert_eq!(
            index.postings_for_trigram(pack_trigram(b'a', b'b', b'c'), None),
            vec![0]
        );
        assert!(index
            .postings_for_trigram(pack_trigram(b'd', b'e', b'f'), None)
            .is_empty());
    }

    #[test]
    fn legacy_cache_without_file_trigram_count_migrates_streaming_counts() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let project = dir.path().join("project");
        fs::create_dir_all(&project).expect("create project dir");
        fs::write(project.join("src.txt"), "abcdef").expect("write source");
        let cache_dir = dir.path().join("cache");
        let mut index = SearchIndex::build(&project);
        index.write_to_disk(&cache_dir, None);
        let cache_path = cache_dir.join("cache.bin");
        strip_file_trigram_count_extension(&cache_path);
        assert!(!cache_has_file_trigram_count_extension(&cache_path));

        let loaded = SearchIndex::read_from_disk(&cache_dir, &project).expect("load legacy cache");
        assert_eq!(loaded.file_trigram_count.as_ref(), &[4]);
        assert!(loaded.delta.postings.is_empty());
        assert!(cache_has_file_trigram_count_extension(&cache_path));
    }

    #[test]
    fn compaction_flags_buffer_paths_while_running() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let project = dir.path().join("project");
        fs::create_dir_all(&project).expect("create project dir");
        let project = fs::canonicalize(&project).expect("canonical project root");
        fs::write(project.join("src.txt"), "abcdef").expect("write source");
        for root in [
            project.clone(),
            crate::inspect::job::normalize_path(&project),
        ] {
            let file = root.join("src.txt");
            let mut index = SearchIndex::new();
            index.project_root = root;
            {
                let mut state = index.compaction_state.lock().expect("compaction state");
                state.running = true;
            }
            index.update_file(&file);
            assert!(index.path_to_id.contains_key(&file));
            let state = index.compaction_state.lock().expect("compaction state");
            assert!(state.requested_again || !index.delta.postings.is_empty());
            assert!(state.buffered_paths.contains(&file));
        }
    }

    fn cache_has_file_trigram_count_extension(cache_path: &Path) -> bool {
        file_trigram_count_extension_range(cache_path).is_some()
    }

    fn strip_file_trigram_count_extension(cache_path: &Path) {
        let mut bytes = fs::read(cache_path).expect("read cache");
        let (start, end) = file_trigram_count_extension_range_from_bytes(&bytes)
            .expect("file trigram count extension");
        bytes.drain(start..end);
        let postings_len_total = u64::from_le_bytes(bytes[8..16].try_into().unwrap())
            - u64::try_from(end - start).unwrap();
        bytes[8..16].copy_from_slice(&postings_len_total.to_le_bytes());
        let checksum_pos = 16 + usize::try_from(postings_len_total).unwrap() - 4;
        let checksum = crc32fast::hash(&bytes[16..checksum_pos]);
        bytes[checksum_pos..checksum_pos + 4].copy_from_slice(&checksum.to_le_bytes());
        fs::write(cache_path, bytes).expect("write legacy cache");
    }

    fn file_trigram_count_extension_range(cache_path: &Path) -> Option<(usize, usize)> {
        let bytes = fs::read(cache_path).ok()?;
        file_trigram_count_extension_range_from_bytes(&bytes)
    }

    fn file_trigram_count_extension_range_from_bytes(bytes: &[u8]) -> Option<(usize, usize)> {
        let postings_len_total = u64::from_le_bytes(bytes.get(8..16)?.try_into().ok()?) as usize;
        let postings_start = 16usize;
        let postings_end = postings_start.checked_add(postings_len_total)?;
        let postings_body_end = postings_end.checked_sub(4)?;
        let mut reader = Cursor::new(&bytes[postings_start..postings_body_end]);
        let mut magic = [0u8; 8];
        reader.read_exact(&mut magic).ok()?;
        if &magic != INDEX_MAGIC {
            return None;
        }
        read_u32(&mut reader).ok()?;
        let head_len = read_u32(&mut reader).ok()? as u64;
        let root_len = read_u32(&mut reader).ok()? as u64;
        let ignore_len = read_u32(&mut reader).ok()? as u64;
        read_u64(&mut reader).ok()?;
        let file_count = read_u32(&mut reader).ok()? as usize;
        let skip = head_len.checked_add(root_len)?.checked_add(ignore_len)?;
        reader.seek(SeekFrom::Current(skip as i64)).ok()?;
        for _ in 0..file_count {
            let mut unindexed = [0u8; 1];
            reader.read_exact(&mut unindexed).ok()?;
            let path_len = read_u32(&mut reader).ok()? as u64;
            read_u64(&mut reader).ok()?;
            read_u64(&mut reader).ok()?;
            read_u32(&mut reader).ok()?;
            let mut hash = [0u8; 32];
            reader.read_exact(&mut hash).ok()?;
            reader.seek(SeekFrom::Current(path_len as i64)).ok()?;
        }
        let postings_blob_len = read_u64(&mut reader).ok()? as usize;
        let extension_start = postings_start
            .checked_add(reader.position() as usize)?
            .checked_add(postings_blob_len)?;
        if extension_start + 16 > postings_body_end {
            return None;
        }
        if bytes.get(extension_start..extension_start + 8)? != FILE_TRIGRAM_COUNT_MAGIC {
            return None;
        }
        let count = u32::from_le_bytes(
            bytes[extension_start + 12..extension_start + 16]
                .try_into()
                .ok()?,
        ) as usize;
        let extension_end = extension_start
            .checked_add(16)?
            .checked_add(count.checked_mul(4)?)?;
        (extension_end <= postings_body_end).then_some((extension_start, extension_end))
    }

    #[test]
    fn disk_round_trip_preserves_postings_and_files() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let project = dir.path().join("project");
        fs::create_dir_all(&project).expect("create project dir");
        let file = project.join("src.txt");
        fs::write(&file, "abcdef").expect("write source");

        let mut index = SearchIndex::build(&project);
        index.git_head = Some("deadbeef".to_string());
        let cache_dir = dir.path().join("cache");
        let head = index.git_head.clone();
        index.write_to_disk(&cache_dir, head.as_deref());

        let loaded =
            SearchIndex::read_from_disk(&cache_dir, &project).expect("load index from disk");
        assert_eq!(loaded.stored_git_head(), Some("deadbeef"));
        assert_eq!(loaded.files.len(), 1);
        assert_eq!(
            relative_to_root(&loaded.project_root, &loaded.files[0].path),
            PathBuf::from("src.txt")
        );
        assert_eq!(loaded.trigram_count(), index.trigram_count());
        assert_eq!(
            loaded.postings_for_trigram(pack_trigram(b'a', b'b', b'c'), None),
            vec![0]
        );
        assert_eq!(
            loaded.file_trigram_count.as_ref(),
            index.file_trigram_count.as_ref()
        );
    }

    #[test]
    fn cache_path_helpers_reject_absolute_and_parent_paths() {
        let root = PathBuf::from("/tmp/aft-project");

        assert_eq!(
            cache_relative_path(&root, &root.join("src/lib.rs")),
            Some(PathBuf::from("src/lib.rs"))
        );
        assert!(cache_relative_path(&root, Path::new("/tmp/outside.rs")).is_none());
        assert!(cached_path_under_root(&root, Path::new("../outside.rs")).is_none());
        assert!(cached_path_under_root(&root, Path::new("/tmp/outside.rs")).is_none());
        assert_eq!(
            cached_path_under_root(&root, Path::new("src/./lib.rs")),
            Some(root.join("src/lib.rs"))
        );
    }

    fn git_command_for_test(root: &Path) -> Command {
        let mut command = Command::new("git");
        crate::test_env::apply_hermetic_git_env(command.arg("-C").arg(root));
        command
    }

    #[test]
    fn refresh_after_head_change_removes_renames_and_detects_local_files() {
        let _git_env = crate::test_env::hermetic_git_env_guard();
        let dir = tempfile::tempdir().expect("create temp dir");
        let project = dir.path().join("project");
        fs::create_dir_all(&project).expect("create project dir");
        let canonical_project = fs::canonicalize(&project).expect("canonical project");
        fs::write(project.join("old.txt"), "old token\n").expect("write old");
        fs::write(project.join("unchanged.txt"), "before\n").expect("write unchanged");

        let mut init = Command::new("git");
        crate::test_env::apply_hermetic_git_env(init.arg("init").arg(&project))
            .status()
            .expect("git init");
        for args in [
            ["config", "user.email", "aft@example.invalid"],
            ["config", "user.name", "AFT Test"],
        ] {
            git_command_for_test(&project)
                .args(args)
                .status()
                .expect("git config");
        }
        git_command_for_test(&project)
            .args(["add", "."])
            .status()
            .expect("git add initial");
        git_command_for_test(&project)
            .args(["commit", "-m", "initial"])
            .status()
            .expect("git commit initial");
        let previous = run_git(&project, &["rev-parse", "HEAD"]).expect("previous head");
        let mut baseline = SearchIndex::build(&project);
        baseline.git_head = Some(previous.clone());

        fs::rename(project.join("old.txt"), project.join("new.txt")).expect("rename file");
        git_command_for_test(&project)
            .args(["add", "-A"])
            .status()
            .expect("git add rename");
        git_command_for_test(&project)
            .args(["commit", "-m", "rename"])
            .status()
            .expect("git commit rename");
        let current = run_git(&project, &["rev-parse", "HEAD"]).expect("current head");

        fs::write(project.join("unchanged.txt"), "after local edit\n").expect("local edit");
        fs::write(project.join("untracked.txt"), "untracked token\n").expect("untracked");

        let refreshed = SearchIndex::rebuild_or_refresh(
            &project,
            DEFAULT_MAX_FILE_SIZE,
            Some(current),
            Some(baseline),
            None,
        );

        assert!(!refreshed
            .path_to_id
            .contains_key(&canonical_project.join("old.txt")));
        assert!(refreshed
            .path_to_id
            .contains_key(&canonical_project.join("new.txt")));
        assert!(refreshed
            .path_to_id
            .contains_key(&canonical_project.join("untracked.txt")));
        let matches = refreshed.grep("after local edit", true, &[], &[], &canonical_project, 10);
        assert_eq!(matches.matches.len(), 1);
    }

    /// File names that `git diff` quotes unless it is run with `-z`: non-ASCII
    /// bytes everywhere, plus a tab, double quotes and a backslash where the
    /// platform allows them in a file name. The space is never quoted and
    /// serves as a control.
    fn names_git_quotes() -> Vec<&'static str> {
        let mut names = vec!["café.rs", "with space.rs"];
        if cfg!(unix) {
            names.extend(["tab\tname.rs", "say \"hi\".rs", "back\\slash.rs"]);
        }
        names
    }

    /// Commit everything in the working tree, rehashing every file.
    ///
    /// `git add` normally skips a file whose size and mtime match what the
    /// index recorded, unless the file was written so close to the index that
    /// the mtime cannot be trusted. On Linux and macOS git also compares the
    /// change time, which every write updates; git for Windows has no such
    /// time to compare. So on Windows an edit that keeps the size and restores
    /// the old mtime is not staged, and the commit fails with nothing to
    /// commit, unless the index is emptied first so every file is hashed
    /// again.
    fn git_commit_all_for_test(project: &Path, message: &str) {
        assert!(git_command_for_test(project)
            .args(["read-tree", "--empty"])
            .status()
            .expect("git read-tree")
            .success());
        assert!(git_command_for_test(project)
            .args(["add", "-A"])
            .status()
            .expect("git add")
            .success());
        assert!(git_command_for_test(project)
            .args(["commit", "--quiet", "-m", message])
            .status()
            .expect("git commit")
            .success());
    }

    /// Move a file's mtime an hour into the past, so git trusts its recorded
    /// size and mtime for the file instead of hashing it again. A same-size
    /// edit that restores this mtime is then invisible to a plain `git add`
    /// on Windows every time, not only when the index happens to be written
    /// in a later clock tick than the file.
    fn backdate_for_test(path: &Path) {
        let an_hour_ago = std::time::SystemTime::now() - Duration::from_secs(3600);
        File::options()
            .write(true)
            .open(path)
            .and_then(|file| file.set_modified(an_hour_ago))
            .expect("backdate mtime");
    }

    /// A repository whose first commit holds every file already in `project`.
    /// `core.quotePath` is set explicitly so git quotes non-ASCII names no
    /// matter what the surrounding configuration says.
    fn init_quoting_git_repo(project: &Path) -> String {
        let mut init = Command::new("git");
        assert!(crate::test_env::apply_hermetic_git_env(
            init.arg("init").arg("--quiet").arg(project)
        )
        .status()
        .expect("git init")
        .success());
        for args in [
            ["config", "user.email", "aft@example.invalid"],
            ["config", "user.name", "AFT Test"],
            ["config", "core.quotePath", "true"],
        ] {
            assert!(git_command_for_test(project)
                .args(args)
                .status()
                .expect("git config")
                .success());
        }
        git_commit_all_for_test(project, "initial");
        run_git(project, &["rev-parse", "HEAD"]).expect("initial head")
    }

    #[test]
    fn warm_reload_counts_each_git_quoted_path_once_between_heads() {
        let _git_env = crate::test_env::hermetic_git_env_guard();
        let dir = tempfile::tempdir().expect("create temp dir");
        let project = dir.path().join("project");
        fs::create_dir_all(&project).expect("create project dir");
        let canonical_project = fs::canonicalize(&project).expect("canonical project");
        let names = names_git_quotes();
        for (ordinal, name) in names.iter().enumerate() {
            fs::write(project.join(name), format!("// {ordinal}\n")).expect("write file");
        }
        let previous = init_quoting_git_repo(&project);
        let mut baseline = SearchIndex::build(&project);
        baseline.git_head = Some(previous.clone());

        // Ordinary edits change size and mtime, so the disk walk reports each
        // file too. The git diff must name the same paths, not quoted copies.
        for (ordinal, name) in names.iter().enumerate() {
            fs::write(
                project.join(name),
                format!("// {ordinal} QuotedPathNeedle after the edit\n"),
            )
            .expect("edit file");
        }
        git_commit_all_for_test(&project, "edit special names");
        let current = run_git(&project, &["rev-parse", "HEAD"]).expect("current head");

        let expected: HashSet<PathBuf> = names
            .iter()
            .map(|name| cached_path_under_root(&project, Path::new(name)).expect("under root"))
            .collect();
        assert_eq!(
            git_diff_paths(&project, &previous, &current, Some(names.len())),
            Some(expected)
        );
        // A budget of exactly one path per edited file is enough only when no
        // file is counted twice.
        let refreshed = SearchIndex::refresh_cached_with_diff_limit(
            baseline,
            &project,
            DEFAULT_MAX_FILE_SIZE,
            Some(current.clone()),
            cache_freshness::VerifyStrategy::StatFirst,
            Some(names.len()),
        )
        .expect("each edited file must count once against the warm budget");
        assert_eq!(refreshed.git_head.as_deref(), Some(current.as_str()));
        let result = refreshed.grep("QuotedPathNeedle", true, &[], &[], &canonical_project, 10);
        assert_eq!(result.total_matches, names.len());
    }

    #[test]
    fn warm_reload_reads_committed_edit_to_git_quoted_path_with_unchanged_stat() {
        let _git_env = crate::test_env::hermetic_git_env_guard();
        let dir = tempfile::tempdir().expect("create temp dir");
        let project = dir.path().join("project");
        fs::create_dir_all(&project).expect("create project dir");
        let canonical_project = fs::canonicalize(&project).expect("canonical project");
        let names = names_git_quotes();
        for (ordinal, name) in names.iter().enumerate() {
            fs::write(project.join(name), format!("// {ordinal} QuotedOld\n")).expect("write");
            backdate_for_test(&project.join(name));
        }
        let previous = init_quoting_git_repo(&project);
        let mut baseline = SearchIndex::build(&project);
        baseline.git_head = Some(previous.clone());

        // Same size and the original mtime: the stat pass sees nothing, so
        // only the git diff can tell the refresh to reread these files.
        for (ordinal, name) in names.iter().enumerate() {
            let path = project.join(name);
            let modified = fs::metadata(&path)
                .and_then(|metadata| metadata.modified())
                .expect("original mtime");
            fs::write(&path, format!("// {ordinal} QuotedNew\n")).expect("edit file");
            File::options()
                .write(true)
                .open(&path)
                .and_then(|file| file.set_modified(modified))
                .expect("restore mtime");
        }
        git_commit_all_for_test(&project, "same-size edits");
        let current = run_git(&project, &["rev-parse", "HEAD"]).expect("current head");
        assert_ne!(previous, current, "git must record the same-size edits");

        let refreshed = SearchIndex::refresh_cached_with_diff_limit(
            baseline,
            &project,
            DEFAULT_MAX_FILE_SIZE,
            Some(current),
            cache_freshness::VerifyStrategy::StatFirst,
            Some(WARM_GIT_DIFF_PATH_LIMIT),
        )
        .expect("warm refresh");
        let new = refreshed.grep("QuotedNew", true, &[], &[], &canonical_project, 10);
        let mut found: Vec<_> = new
            .matches
            .iter()
            .map(|matched| matched.file.file_name().unwrap().to_owned())
            .collect();
        found.sort();
        let mut expected: Vec<_> = names.iter().map(std::ffi::OsString::from).collect();
        expected.sort();
        assert_eq!(found, expected, "every committed edit must be searchable");
        let old = refreshed.grep("QuotedOld", true, &[], &[], &canonical_project, 10);
        assert_eq!(old.total_matches, 0, "no stale content may remain");
    }

    #[test]
    fn warm_reload_from_repository_subdirectory_counts_and_rereads_only_paths_under_root() {
        let _git_env = crate::test_env::hermetic_git_env_guard();
        let dir = tempfile::tempdir().expect("create temp dir");
        let repo = dir.path().join("repo");
        // The search root is one package of a larger repository.
        let project = repo.join("packages").join("app");
        let outside = repo.join("other").join("outside.rs");
        let inside = project.join("inside.rs");
        fs::create_dir_all(&project).expect("create project dir");
        fs::create_dir_all(outside.parent().unwrap()).expect("create sibling dir");
        fs::write(&inside, "// SubdirOld\n").expect("write inside");
        backdate_for_test(&inside);
        fs::write(&outside, "// OutsideOld\n").expect("write outside");
        let canonical_project = fs::canonicalize(&project).expect("canonical project");
        init_quoting_git_repo(&repo);
        let previous = run_git(&project, &["rev-parse", "HEAD"]).expect("previous head");
        // One baseline for the budgeted warm lane, one for the unbudgeted refresh.
        let mut budgeted = SearchIndex::build(&project);
        budgeted.git_head = Some(previous.clone());
        let mut unbudgeted = SearchIndex::build(&project);
        unbudgeted.git_head = Some(previous.clone());

        // Same size and the original mtime inside the root, so only the git
        // diff can tell the refresh to reread it. The edit outside the root is
        // an ordinary one.
        let modified = fs::metadata(&inside)
            .and_then(|metadata| metadata.modified())
            .expect("original mtime");
        fs::write(&inside, "// SubdirNew\n").expect("edit inside");
        File::options()
            .write(true)
            .open(&inside)
            .and_then(|file| file.set_modified(modified))
            .expect("restore mtime");
        fs::write(&outside, "// OutsideNew, a longer edit\n").expect("edit outside");
        git_commit_all_for_test(&repo, "edit inside and outside the package");
        let current = run_git(&project, &["rev-parse", "HEAD"]).expect("current head");

        assert_eq!(
            git_diff_paths(&project, &previous, &current, Some(1)),
            Some(HashSet::from([cached_path_under_root(
                &project,
                Path::new("inside.rs")
            )
            .expect("under root")])),
            "only the change under the root is reported, relative to the root"
        );

        let assert_only_inside_is_indexed_and_fresh = |index: &SearchIndex, lane: &str| {
            let indexed: Vec<_> = index
                .path_to_id
                .keys()
                .filter(|path| !path.as_os_str().is_empty())
                .cloned()
                .collect();
            assert_eq!(
                indexed,
                vec![canonical_project.join("inside.rs")],
                "{lane}: nothing outside the root may enter the index"
            );
            let new = index.grep("SubdirNew", true, &[], &[], &canonical_project, 10);
            assert_eq!(
                new.total_matches, 1,
                "{lane}: the edit under the root is searchable"
            );
            let old = index.grep("SubdirOld", true, &[], &[], &canonical_project, 10);
            assert_eq!(old.total_matches, 0, "{lane}: no stale content may remain");
            let outside = index.grep("Outside", true, &[], &[], &canonical_project, 10);
            assert_eq!(
                outside.total_matches, 0,
                "{lane}: outside content stays out"
            );
        };
        // A budget of one path is enough only when the outside edit is not
        // counted.
        let refreshed = SearchIndex::refresh_cached_with_diff_limit(
            budgeted,
            &project,
            DEFAULT_MAX_FILE_SIZE,
            Some(current.clone()),
            cache_freshness::VerifyStrategy::StatFirst,
            Some(1),
        )
        .expect("the edit outside the root must not count against the warm budget");
        assert_only_inside_is_indexed_and_fresh(&refreshed, "budgeted");
        let refreshed = SearchIndex::refresh_cached_with_diff_limit(
            unbudgeted,
            &project,
            DEFAULT_MAX_FILE_SIZE,
            Some(current),
            cache_freshness::VerifyStrategy::StatFirst,
            None,
        )
        .expect("unbudgeted refresh");
        assert_only_inside_is_indexed_and_fresh(&refreshed, "unbudgeted");
    }

    #[test]
    fn read_from_disk_rejects_corrupt_lookup_checksum() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let project = dir.path().join("project");
        fs::create_dir_all(&project).expect("create project dir");
        fs::write(project.join("src.txt"), "abcdef").expect("write source");

        let mut index = SearchIndex::build(&project);
        let cache_dir = dir.path().join("cache");
        index.write_to_disk(&cache_dir, None);

        let cache_path = cache_dir.join("cache.bin");
        let mut bytes = fs::read(&cache_path).expect("read cache");
        let last = bytes.len() - 1;
        bytes[last] ^= 0xff;
        fs::write(&cache_path, bytes).expect("write corrupted cache");

        assert!(SearchIndex::read_from_disk(&cache_dir, &project).is_none());
    }

    #[test]
    fn write_to_disk_uses_temp_files_and_cleans_them_up() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let project = dir.path().join("project");
        fs::create_dir_all(&project).expect("create project dir");
        fs::write(project.join("src.txt"), "abcdef").expect("write source");

        let mut index = SearchIndex::build(&project);
        let cache_dir = dir.path().join("cache");
        index.write_to_disk(&cache_dir, None);

        assert!(cache_dir.join("cache.bin").is_file());
        assert!(fs::read_dir(&cache_dir)
            .expect("read cache dir")
            .all(|entry| !entry
                .expect("cache entry")
                .file_name()
                .to_string_lossy()
                .contains(".tmp.")));
    }

    #[test]
    fn concurrent_search_index_writes_do_not_corrupt() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let project = dir.path().join("project");
        fs::create_dir_all(&project).expect("create project dir");
        fs::write(project.join("src.txt"), "abcdef\n").expect("write source");
        let cache_dir = dir.path().join("cache");

        let a_project = project.clone();
        let a_cache = cache_dir.clone();
        let a = std::thread::spawn(move || {
            let _lock = CacheLock::acquire(&a_cache, &a_project).expect("acquire cache lock a");
            let mut index = SearchIndex::build(&a_project);
            index.write_to_disk(&a_cache, None);
        });
        let b_project = project.clone();
        let b_cache = cache_dir.clone();
        let b = std::thread::spawn(move || {
            let _lock = CacheLock::acquire(&b_cache, &b_project).expect("acquire cache lock b");
            let mut index = SearchIndex::build(&b_project);
            index.write_to_disk(&b_cache, None);
        });
        a.join().expect("writer a");
        b.join().expect("writer b");

        assert!(SearchIndex::read_from_disk(&cache_dir, &project).is_some());
    }

    #[test]
    fn search_index_atomic_rename_survives_partial_write() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let cache_dir = dir.path().join("cache");
        fs::create_dir_all(&cache_dir).expect("create cache dir");
        fs::write(cache_dir.join("cache.bin.tmp.1.1"), b"partial").expect("write partial tmp");

        assert!(SearchIndex::read_from_disk(&cache_dir, dir.path()).is_none());
    }

    fn grafted_history_test_roots() -> [&'static str; 3] {
        [
            "7e96b9e0000000000000000000000000000000",
            "1e394c20000000000000000000000000000000",
            "40587520000000000000000000000000000000",
        ]
    }

    fn artifact_key_from_root_commit_output(stdout: &[u8]) -> String {
        match canonicalize_root_commit_output(stdout) {
            RootCommitProbe::Commit(root_commit) => artifact_key_from_git_identity(&root_commit),
            RootCommitProbe::NoCommit => panic!("root commit output unexpectedly empty"),
            RootCommitProbe::NotARepo => panic!("root commit output was not a repository"),
            RootCommitProbe::Transient(detail) => panic!("root commit output failed: {detail}"),
        }
    }

    fn force_git_root_commit_probe_outputs_for_test(
        outputs_by_root: BTreeMap<PathBuf, Vec<u8>>,
    ) -> GitRootCommitProbeOverrideGuard {
        install_git_root_commit_probe_override_for_test(move |project_root| {
            outputs_by_root
                .get(project_root)
                .map(|stdout| canonicalize_root_commit_output(stdout))
        })
    }

    #[test]
    fn artifact_cache_key_canonicalizes_all_root_permutations() {
        let roots = grafted_history_test_roots();
        let mut sorted_roots = roots;
        sorted_roots.sort_unstable();
        let expected_commit = sorted_roots.join("\n");
        let expected_key = artifact_key_from_git_identity(&expected_commit);

        let permutations = [
            [roots[0], roots[1], roots[2]],
            [roots[0], roots[2], roots[1]],
            [roots[1], roots[0], roots[2]],
            [roots[1], roots[2], roots[0]],
            [roots[2], roots[0], roots[1]],
            [roots[2], roots[1], roots[0]],
        ];
        for permutation in permutations {
            let output = format!("{}\n", permutation.join("\n"));
            let RootCommitProbe::Commit(canonical) =
                canonicalize_root_commit_output(output.as_bytes())
            else {
                panic!("root permutation did not produce a commit");
            };
            assert_eq!(canonical, expected_commit);
            assert_eq!(artifact_key_from_git_identity(&canonical), expected_key);
        }
    }

    #[test]
    fn artifact_cache_key_deduplicates_repeated_roots() {
        let root = grafted_history_test_roots()[0];
        let one_root = format!("{root}\n");
        let duplicate_root = format!("{root}\n{root}\n");

        assert_eq!(
            artifact_key_from_root_commit_output(one_root.as_bytes()),
            artifact_key_from_root_commit_output(duplicate_root.as_bytes())
        );
    }

    #[test]
    fn artifact_cache_key_ignores_blank_whitespace_and_crlf_lines() {
        let roots = grafted_history_test_roots();
        let clean = format!("{}\n{}\n", roots[0], roots[1]);
        let decorated = format!(" \r\n\t{}  \r\n{}\t\r\n\r\n", roots[1], roots[0]);

        assert_eq!(
            artifact_key_from_root_commit_output(clean.as_bytes()),
            artifact_key_from_root_commit_output(decorated.as_bytes())
        );
    }

    #[test]
    fn artifact_cache_key_single_root_matches_the_old_trimmed_derivation() {
        let root = grafted_history_test_roots()[0];
        let output = format!("{root}\n");
        let old_trimmed = String::from_utf8_lossy(output.as_bytes())
            .trim()
            .to_string();
        let old_key = artifact_hash16(old_trimmed.as_bytes());

        let RootCommitProbe::Commit(canonical) = canonicalize_root_commit_output(output.as_bytes())
        else {
            panic!("single root output did not produce a commit");
        };
        assert_eq!(canonical, root);
        assert_eq!(artifact_key_from_git_identity(&canonical), old_key);
    }

    #[test]
    fn artifact_cache_key_empty_canonical_root_output_is_no_commit() {
        assert!(matches!(
            canonicalize_root_commit_output(b" \r\n\t\n\r\n"),
            RootCommitProbe::NoCommit
        ));
    }

    #[test]
    fn artifact_cache_key_memo_round_trip_uses_canonical_root_set() {
        let _probe_lock = git_root_commit_probe_override_lock_for_test();
        let dir = tempfile::tempdir().expect("create temp dir");
        let storage = dir.path().join("storage");
        let root = git_like_root(&dir, "repo");
        let roots = grafted_history_test_roots();
        let mut sorted_roots = roots;
        sorted_roots.sort_unstable();
        let canonical = sorted_roots.join("\n");
        let expected_key = artifact_key_from_git_identity(&canonical);
        let first_output = format!("{}\n{}\n{}\n", roots[2], roots[0], roots[1]);
        let second_output = format!("{}\n{}\n{}\n", roots[1], roots[2], roots[0]);

        {
            let mut outputs = BTreeMap::new();
            outputs.insert(root.clone(), first_output.into_bytes());
            let _override = force_git_root_commit_probe_outputs_for_test(outputs);
            assert_eq!(
                artifact_cache_key_with_memo(&root, &root, &storage, None)
                    .expect("first canonical key"),
                expected_key
            );
        }
        let first_memo_bytes =
            fs::read(artifact_cache_key_memo_path(&storage)).expect("read first memo bytes");

        {
            let mut outputs = BTreeMap::new();
            outputs.insert(root.clone(), second_output.into_bytes());
            let _override = force_git_root_commit_probe_outputs_for_test(outputs);
            assert_eq!(
                artifact_cache_key_with_memo(&root, &root, &storage, None)
                    .expect("second canonical key"),
                expected_key
            );
        }
        let second_memo_bytes =
            fs::read(artifact_cache_key_memo_path(&storage)).expect("read second memo bytes");
        assert_eq!(
            first_memo_bytes, second_memo_bytes,
            "an unchanged canonical memo entry should not be rewritten"
        );
        let memo = read_cache_key_memo(&storage);
        assert_eq!(
            memo.get(root.to_string_lossy().as_ref())
                .expect("canonical memo entry")
                .git_root_commit,
            canonical
        );
    }

    #[test]
    fn artifact_cache_key_replaces_old_unsorted_memo_entry() {
        let _probe_lock = git_root_commit_probe_override_lock_for_test();
        let dir = tempfile::tempdir().expect("create temp dir");
        let storage = dir.path().join("storage");
        let root = git_like_root(&dir, "repo");
        let roots = grafted_history_test_roots();
        let unsorted = format!("{}\n{}", roots[2], roots[0]);
        let mut sorted_roots = [roots[2], roots[0]];
        sorted_roots.sort_unstable();
        let canonical = sorted_roots.join("\n");
        let expected_key = artifact_key_from_git_identity(&canonical);

        let mut seeded_memo = BTreeMap::new();
        seeded_memo.insert(
            root.to_string_lossy().into_owned(),
            ArtifactCacheKeyMemoEntry {
                key: artifact_key_from_git_identity(&unsorted),
                git_root_commit: unsorted.clone(),
                recorded_at_ms: 1,
            },
        );
        fs::create_dir_all(&storage).expect("create memo storage");
        fs::write(
            artifact_cache_key_memo_path(&storage),
            serde_json::to_vec_pretty(&seeded_memo).expect("serialize seeded memo"),
        )
        .expect("write seeded memo");

        let mut outputs = BTreeMap::new();
        outputs.insert(root.clone(), unsorted.into_bytes());
        let _override = force_git_root_commit_probe_outputs_for_test(outputs);
        let key = artifact_cache_key_with_memo(&root, &root, &storage, None)
            .expect("canonical probe should replace old memo");

        assert_eq!(key, expected_key);
        let memo = read_cache_key_memo(&storage);
        let entry = memo
            .get(root.to_string_lossy().as_ref())
            .expect("replaced memo entry");
        assert_eq!(entry.key, expected_key);
        assert_eq!(entry.git_root_commit, canonical);
    }

    #[test]
    fn artifact_cache_key_reuses_root_commit_until_head_moves() {
        let _git_env = crate::test_env::hermetic_git_env_guard();
        // No scripted probe may be installed meanwhile: it bypasses the cache.
        let _probe_lock = git_root_commit_probe_override_lock_for_test();
        let dir = tempfile::tempdir().expect("create temp dir");
        let root = fs::canonicalize(dir.path()).expect("canonical root");
        let git = |args: &[&str]| {
            assert!(git_command_for_test(&root)
                .args([
                    "-c",
                    "user.name=AFT Tests",
                    "-c",
                    "user.email=aft@example.com"
                ])
                .args(args)
                .status()
                .expect("run git")
                .success());
        };
        git(&["init", "--quiet"]);
        fs::write(root.join("tracked.txt"), "one\n").unwrap();
        git(&["add", "."]);
        git(&["commit", "--quiet", "-m", "first"]);

        // Many derivations of an unchanged root (the call-graph store does
        // one per refresh) run git once.
        let first = artifact_cache_key(&root);
        for _ in 0..9 {
            assert_eq!(artifact_cache_key(&root), first);
        }
        assert_eq!(root_commit_git_runs_for_test(&root), 1);

        // Staging rewrites .git/index through index.lock, as git status
        // refreshes do. HEAD is unchanged, so git is not asked again.
        fs::write(root.join("tracked.txt"), "two\n").unwrap();
        git(&["add", "tracked.txt"]);
        assert_eq!(artifact_cache_key(&root), first);
        assert_eq!(root_commit_git_runs_for_test(&root), 1);

        // A new commit moves HEAD: git runs again, the root set is unchanged.
        git(&["commit", "--quiet", "-am", "second"]);
        assert_eq!(artifact_cache_key(&root), first);
        assert_eq!(root_commit_git_runs_for_test(&root), 2);
        assert_eq!(artifact_cache_key(&root), first);
        assert_eq!(root_commit_git_runs_for_test(&root), 2);
        let second = git_command_for_test(&root)
            .args(["rev-parse", "HEAD"])
            .output()
            .expect("rev-parse");
        let second = String::from_utf8(second.stdout).unwrap().trim().to_string();

        // An unrelated history checked out under HEAD must change the key.
        git(&["checkout", "--quiet", "--orphan", "other"]);
        git(&["commit", "--quiet", "-m", "unrelated root"]);
        let other = artifact_cache_key(&root);
        assert_ne!(other, first, "a different root commit kept the old key");
        assert_eq!(root_commit_git_runs_for_test(&root), 3);

        // Detaching HEAD at the second commit, whose history has the first
        // root, brings back the first key; the stamp sees the HEAD change.
        git(&["checkout", "--quiet", "--detach", &second]);
        assert_eq!(artifact_cache_key(&root), first);
        assert_eq!(root_commit_git_runs_for_test(&root), 4);
    }

    fn root_commit_test_git(root: &Path, args: &[&str]) -> String {
        let output = git_command_for_test(root)
            .args([
                "-c",
                "user.name=AFT Tests",
                "-c",
                "user.email=aft@example.com",
            ])
            .args(args)
            .output()
            .expect("run git");
        assert!(output.status.success(), "{output:?}");
        String::from_utf8(output.stdout).unwrap().trim().to_string()
    }

    #[test]
    fn root_commit_cache_does_not_store_a_probe_across_history_changes() {
        let _git_env = crate::test_env::hermetic_git_env_guard();
        let _probe_lock = git_root_commit_probe_override_lock_for_test();
        let dir = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(dir.path()).unwrap();
        let git = |args: &[&str]| root_commit_test_git(&root, args);
        git(&["init", "--quiet"]);
        git(&["commit", "--quiet", "--allow-empty", "-m", "first root"]);
        let first = git(&["rev-parse", "HEAD"]);
        git(&["checkout", "--quiet", "--orphan", "other"]);
        git(&["commit", "--quiet", "--allow-empty", "-m", "unrelated root"]);
        let other = git(&["rev-parse", "HEAD"]);
        assert_ne!(first, other);
        git(&["checkout", "--quiet", "--detach", &first]);

        let resolution = repo_root_commit_with_probe(&root, || {
            // Move to an unrelated history after the first stamp but before git
            // reads HEAD, leaving the second stamp to observe the new history.
            git(&["checkout", "--quiet", "--detach", &other]);
            repo_root_commit_with_retry_profiled(&root, &mut ArtifactCacheKeyTimings::default())
        });
        assert!(matches!(resolution, RootCommitResolution::Commit(commit) if commit == other));
        assert_eq!(root_commit_git_runs_for_test(&root), 1);
        assert!(
            !root_commit_cache().lock().unwrap().contains_key(&root),
            "a probe spanning different repository stamps must not be cached"
        );

        // Returning to the first stamp must not reuse the unrelated root.
        git(&["checkout", "--quiet", "--detach", &first]);
        assert!(matches!(repo_root_commit_with_retry(&root),
            RootCommitResolution::Commit(commit) if commit == first));
        assert_eq!(root_commit_git_runs_for_test(&root), 2);
        let _ = repo_root_commit_with_retry(&root);
        assert_eq!(root_commit_git_runs_for_test(&root), 2);
    }

    #[test]
    fn root_commit_cache_tracks_replace_ref_configuration() {
        let _git_env = crate::test_env::hermetic_git_env_guard();
        let _probe_lock = git_root_commit_probe_override_lock_for_test();
        let dir = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(dir.path()).unwrap();
        let git = |args: &[&str]| root_commit_test_git(&root, args);
        git(&["init", "--quiet"]);
        git(&["commit", "--quiet", "--allow-empty", "-m", "original root"]);
        let original_root = git(&["rev-parse", "HEAD"]);
        git(&["commit", "--quiet", "--allow-empty", "-m", "child"]);
        let head = git(&["rev-parse", "HEAD"]);

        git(&["checkout", "--quiet", "--orphan", "replacement"]);
        git(&[
            "commit",
            "--quiet",
            "--allow-empty",
            "-m",
            "replacement root",
        ]);
        let replacement = git(&["rev-parse", "HEAD"]);
        git(&["checkout", "--quiet", "--detach", &head]);
        git(&["replace", &head, &replacement]);
        git(&["config", "core.useReplaceRefs", "false"]);

        assert!(matches!(repo_root_commit_with_retry(&root),
            RootCommitResolution::Commit(commit) if commit == original_root));
        assert_eq!(root_commit_git_runs_for_test(&root), 1);

        // The replacement makes HEAD a root only when Git honors replace refs.
        git(&["config", "core.useReplaceRefs", "true"]);
        assert!(matches!(repo_root_commit_with_retry(&root),
            RootCommitResolution::Commit(commit) if commit == head));
        assert_eq!(root_commit_git_runs_for_test(&root), 2);
        assert!(matches!(repo_root_commit_with_retry(&root),
            RootCommitResolution::Commit(commit) if commit == head));
        assert_eq!(root_commit_git_runs_for_test(&root), 2);
    }

    #[test]
    fn root_commit_cache_bypasses_an_external_config_include() {
        let _git_env = crate::test_env::hermetic_git_env_guard();
        let _probe_lock = git_root_commit_probe_override_lock_for_test();
        let dir = tempfile::tempdir().unwrap();
        let external_config_dir = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(dir.path()).unwrap();
        let external_config = external_config_dir.path().join("included.gitconfig");
        let external_config_arg = external_config.to_str().unwrap();
        let git = |args: &[&str]| root_commit_test_git(&root, args);
        git(&["init", "--quiet"]);
        git(&["commit", "--quiet", "--allow-empty", "-m", "original root"]);
        let original_root = git(&["rev-parse", "HEAD"]);
        git(&["commit", "--quiet", "--allow-empty", "-m", "child"]);
        let head = git(&["rev-parse", "HEAD"]);

        git(&["checkout", "--quiet", "--orphan", "replacement"]);
        git(&[
            "commit",
            "--quiet",
            "--allow-empty",
            "-m",
            "replacement root",
        ]);
        let replacement = git(&["rev-parse", "HEAD"]);
        git(&["checkout", "--quiet", "--detach", &head]);
        git(&["replace", &head, &replacement]);
        git(&["config", "--local", "include.path", external_config_arg]);
        let set_external_setting = |value: &str| {
            let output = git_command_for_test(&root)
                .args([
                    "config",
                    "--file",
                    external_config_arg,
                    "core.useReplaceRefs",
                    value,
                ])
                .output()
                .expect("run git config for the included file");
            assert!(output.status.success(), "{output:?}");
        };

        set_external_setting("false");
        assert!(matches!(repo_root_commit_with_retry(&root),
            RootCommitResolution::Commit(commit) if commit == original_root));
        assert_eq!(root_commit_git_runs_for_test(&root), 1);

        set_external_setting("true");
        assert!(matches!(repo_root_commit_with_retry(&root),
            RootCommitResolution::Commit(commit) if commit == head));
        assert_eq!(root_commit_git_runs_for_test(&root), 2);
    }

    #[test]
    fn root_commit_cache_tracks_retargeted_loose_replace_refs() {
        let _git_env = crate::test_env::hermetic_git_env_guard();
        let _probe_lock = git_root_commit_probe_override_lock_for_test();
        let dir = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(dir.path()).unwrap();
        let git = |args: &[&str]| root_commit_test_git(&root, args);
        git(&["init", "--quiet"]);
        git(&["commit", "--quiet", "--allow-empty", "-m", "original root"]);
        git(&["commit", "--quiet", "--allow-empty", "-m", "original child"]);
        let head = git(&["rev-parse", "HEAD"]);

        git(&["checkout", "--quiet", "--orphan", "replacement"]);
        git(&[
            "commit",
            "--quiet",
            "--allow-empty",
            "-m",
            "replacement root",
        ]);
        let replacement_root = git(&["rev-parse", "HEAD"]);
        git(&[
            "commit",
            "--quiet",
            "--allow-empty",
            "-m",
            "replacement child",
        ]);
        let replacement_child = git(&["rev-parse", "HEAD"]);
        git(&["checkout", "--quiet", "--detach", &head]);
        git(&["replace", &head, &replacement_root]);

        assert!(matches!(repo_root_commit_with_retry(&root),
            RootCommitResolution::Commit(commit) if commit == head));
        assert_eq!(root_commit_git_runs_for_test(&root), 1);

        // Retargeting this same loose ref changes its bytes, not the directory entries.
        let replace_ref = root.join(".git/refs/replace").join(&head);
        fs::write(&replace_ref, format!("{replacement_child}\n")).unwrap();
        assert!(matches!(repo_root_commit_with_retry(&root),
            RootCommitResolution::Commit(commit) if commit == replacement_root));
        assert_eq!(root_commit_git_runs_for_test(&root), 2);
    }

    #[cfg(unix)]
    #[test]
    fn root_commit_cache_bypasses_symlinked_grafts() {
        let _git_env = crate::test_env::hermetic_git_env_guard();
        let _probe_lock = git_root_commit_probe_override_lock_for_test();
        let dir = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(dir.path()).unwrap();
        let git = |args: &[&str]| root_commit_test_git(&root, args);
        git(&["init", "--quiet"]);
        git(&["commit", "--quiet", "--allow-empty", "-m", "first"]);
        let first = git(&["rev-parse", "HEAD"]);
        git(&["commit", "--quiet", "--allow-empty", "-m", "second"]);
        let second = git(&["rev-parse", "HEAD"]);
        let shared = root.join("shared-grafts");
        fs::write(&shared, "").unwrap();
        std::os::unix::fs::symlink(&shared, root.join(".git/info/grafts")).unwrap();

        assert_eq!(
            artifact_cache_key(&root),
            artifact_key_from_git_identity(&first)
        );
        // Only the target changes: the graft turns the second commit into a root.
        fs::write(&shared, format!("{second}\n")).unwrap();
        assert_eq!(
            artifact_cache_key(&root),
            artifact_key_from_git_identity(&second)
        );
        assert_eq!(root_commit_git_runs_for_test(&root), 2);
        assert_eq!(
            artifact_cache_key(&root),
            artifact_key_from_git_identity(&second)
        );
        assert_eq!(root_commit_git_runs_for_test(&root), 3);
        assert!(!root_commit_cache().lock().unwrap().contains_key(&root));
    }

    #[test]
    fn artifact_cache_key_shared_across_clones_of_same_repo() {
        let _git_env = crate::test_env::hermetic_git_env_guard();
        let dir = tempfile::tempdir().expect("create temp dir");
        let source = dir.path().join("source");
        fs::create_dir_all(&source).expect("create source repo dir");
        fs::write(source.join("tracked.txt"), "content\n").expect("write tracked file");

        let mut init = Command::new("git");
        assert!(
            crate::test_env::apply_hermetic_git_env(init.current_dir(&source))
                .args(["init"])
                .status()
                .expect("init git repo")
                .success()
        );
        assert!(git_command_for_test(&source)
            .args(["add", "."])
            .status()
            .expect("git add")
            .success());
        assert!(git_command_for_test(&source)
            .args([
                "-c",
                "user.name=AFT Tests",
                "-c",
                "user.email=aft-tests@example.com",
                "commit",
                "-m",
                "initial",
            ])
            .status()
            .expect("git commit")
            .success());

        let clone = dir.path().join("clone");
        let mut clone_command = Command::new("git");
        assert!(crate::test_env::apply_hermetic_git_env(&mut clone_command)
            .args(["clone", "--quiet"])
            .arg(&source)
            .arg(&clone)
            .status()
            .expect("git clone")
            .success());

        let source_key = artifact_cache_key(&source);
        let clone_key = artifact_cache_key(&clone);

        assert_eq!(source_key.len(), 16);
        assert_eq!(clone_key.len(), 16);
        // Same repo (same root commit) → same cache key regardless of clone path
        assert_eq!(source_key, clone_key);
    }

    fn read_cache_key_memo(storage_root: &Path) -> BTreeMap<String, ArtifactCacheKeyMemoEntry> {
        let bytes = fs::read(artifact_cache_key_memo_path(storage_root)).expect("read memo file");
        serde_json::from_slice(&bytes).expect("parse memo file")
    }

    fn write_cache_key_memo(
        storage_root: &Path,
        entries: &BTreeMap<String, ArtifactCacheKeyMemoEntry>,
    ) {
        fs::create_dir_all(storage_root).expect("create memo storage");
        fs::write(
            artifact_cache_key_memo_path(storage_root),
            serde_json::to_vec_pretty(entries).expect("serialize memo"),
        )
        .expect("write memo");
    }

    fn git_like_root(dir: &tempfile::TempDir, name: &str) -> PathBuf {
        let root = dir.path().join(name);
        fs::create_dir_all(root.join(".git")).expect("create git marker");
        root
    }

    /// `git pack-refs` rewrites `packed-refs` by atomic replacement. When the
    /// current branch is packed and moves to another 40-character commit, the
    /// file keeps its length; a rewrite within one clock tick also keeps every
    /// time, so only the file identity shows that the root commit may differ.
    #[test]
    fn root_commit_stamp_sees_a_same_length_packed_refs_replacement() {
        let _git_env = crate::test_env::hermetic_git_env_guard();
        let directory = tempfile::tempdir().unwrap();
        let root = git_like_root(&directory, "repository");
        let git = root.join(".git");
        fs::write(git.join("HEAD"), "ref: refs/heads/main\n").unwrap();
        let packed = |commit: char| format!("{} refs/heads/main\n", commit.to_string().repeat(40));
        fs::write(git.join("packed-refs"), packed('a')).unwrap();
        let before = RootCommitStamp::capture(&root).expect("the fixture must allow caching");
        crate::file_identity::replace_keeping_times(
            &git.join("packed-refs"),
            packed('b').as_bytes(),
        );
        assert_ne!(RootCommitStamp::capture(&root), Some(before));
    }

    #[test]
    fn index_orphan_sweep_reclaims_only_aged_unreferenced_dirs() {
        let storage = tempfile::tempdir().expect("create storage root");
        let index_root = storage.path().join("index");
        let aged_orphan = index_root.join("1111111111111111");
        let fresh_orphan = index_root.join("2222222222222222");
        let memo_referenced = index_root.join("3333333333333333");
        for directory in [&aged_orphan, &fresh_orphan, &memo_referenced] {
            fs::create_dir_all(directory).expect("create index fixture directory");
            fs::write(directory.join("cache.bin"), b"fixture cache").expect("write cache fixture");
        }
        let old_time = SystemTime::now()
            .checked_sub(INDEX_ORPHAN_MIN_AGE + Duration::from_secs(1))
            .expect("construct old fixture time");
        filetime::set_file_mtime(
            aged_orphan.join("cache.bin"),
            filetime::FileTime::from_system_time(old_time),
        )
        .expect("age orphan cache");
        filetime::set_file_mtime(
            memo_referenced.join("cache.bin"),
            filetime::FileTime::from_system_time(old_time),
        )
        .expect("age referenced cache");
        let mut memo = BTreeMap::new();
        memo.insert(
            "fixture-root".to_string(),
            ArtifactCacheKeyMemoEntry {
                key: "3333333333333333".to_string(),
                git_root_commit: "fixture-commit".to_string(),
                recorded_at_ms: current_time_millis(),
            },
        );
        write_cache_key_memo(storage.path(), &memo);

        sweep_orphaned_index_dirs(&crate::root_cache::StoragePaths::same(storage.path()));

        assert!(
            !aged_orphan.exists(),
            "the aged orphan must be reclaimed by the index sweep"
        );
        assert!(fresh_orphan.exists(), "fresh index data must be retained");
        assert!(
            memo_referenced.exists(),
            "cache-keys.json references must protect an aged index directory"
        );
    }

    #[test]
    fn artifact_cache_key_memo_write_prunes_only_deleted_old_entries() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let storage = dir.path().join("storage");
        let live_old_root = dir.path().join("live-old");
        let dead_old_root = dir.path().join("dead-old");
        let dead_recent_root = dir.path().join("dead-recent");
        let written_root = dir.path().join("written");
        fs::create_dir_all(&live_old_root).expect("create live root");
        fs::create_dir_all(&written_root).expect("create written root");
        let now = current_time_millis();
        let old = now.saturating_sub(ARTIFACT_CACHE_KEY_MEMO_EVICTION_AGE.as_millis() as u64 + 1);
        let recent =
            now.saturating_sub(ARTIFACT_CACHE_KEY_MEMO_EVICTION_AGE.as_millis() as u64 / 2);
        let mut seeded = BTreeMap::new();
        for (root, key, recorded_at_ms) in [
            (&live_old_root, "1111111111111111", old),
            (&dead_old_root, "2222222222222222", old),
            (&dead_recent_root, "3333333333333333", recent),
        ] {
            seeded.insert(
                root.to_string_lossy().into_owned(),
                ArtifactCacheKeyMemoEntry {
                    key: key.to_string(),
                    git_root_commit: "fixture-commit".to_string(),
                    recorded_at_ms,
                },
            );
        }
        write_cache_key_memo(&storage, &seeded);

        record_artifact_cache_key_memo(
            &storage,
            written_root.to_string_lossy().as_ref(),
            "4444444444444444",
            "written-commit",
        )
        .expect("record memo entry");

        let memo = read_cache_key_memo(&storage);
        assert!(memo.contains_key(live_old_root.to_string_lossy().as_ref()));
        assert!(!memo.contains_key(dead_old_root.to_string_lossy().as_ref()));
        assert!(memo.contains_key(dead_recent_root.to_string_lossy().as_ref()));
        assert!(memo.contains_key(written_root.to_string_lossy().as_ref()));
    }

    #[test]
    fn artifact_cache_key_memo_prunes_hundreds_of_deleted_entries_on_next_write() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let storage = dir.path().join("storage");
        let written_root = dir.path().join("written");
        fs::create_dir_all(&written_root).expect("create written root");
        let old = current_time_millis()
            .saturating_sub(ARTIFACT_CACHE_KEY_MEMO_EVICTION_AGE.as_millis() as u64 + 1);
        let mut seeded = BTreeMap::new();
        for index in 0..400 {
            seeded.insert(
                dir.path()
                    .join(format!("dead-{index}"))
                    .to_string_lossy()
                    .into_owned(),
                ArtifactCacheKeyMemoEntry {
                    key: format!("{index:016x}"),
                    git_root_commit: format!("fixture-commit-{index}"),
                    recorded_at_ms: old,
                },
            );
        }
        write_cache_key_memo(&storage, &seeded);
        let bytes_before = fs::metadata(artifact_cache_key_memo_path(&storage))
            .expect("stat seeded memo")
            .len();

        record_artifact_cache_key_memo(
            &storage,
            written_root.to_string_lossy().as_ref(),
            "aaaaaaaaaaaaaaaa",
            "written-commit",
        )
        .expect("record memo entry");

        let memo = read_cache_key_memo(&storage);
        let bytes_after = fs::metadata(artifact_cache_key_memo_path(&storage))
            .expect("stat pruned memo")
            .len();
        assert_eq!(
            memo.len(),
            1,
            "next write must remove all stale fixture roots"
        );
        assert!(memo.contains_key(written_root.to_string_lossy().as_ref()));
        assert!(
            bytes_after < bytes_before,
            "pruning hundreds of dead entries must shrink the memo file"
        );
    }

    #[test]
    fn artifact_cache_key_memo_read_hit_refreshes_existing_root_once_per_day() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let storage = dir.path().join("storage");
        let root = dir.path().join("borrowed-root");
        fs::create_dir_all(&root).expect("create borrowed root");
        let mut seeded = BTreeMap::new();
        seeded.insert(
            root.to_string_lossy().into_owned(),
            ArtifactCacheKeyMemoEntry {
                key: "aaaaaaaaaaaaaaaa".to_string(),
                git_root_commit: "fixture-commit".to_string(),
                recorded_at_ms: 0,
            },
        );
        write_cache_key_memo(&storage, &seeded);

        let first = lookup_artifact_cache_key_memo(&storage, root.to_string_lossy().as_ref())
            .expect("memo hit");
        let persisted_first = read_cache_key_memo(&storage)
            .get(root.to_string_lossy().as_ref())
            .expect("persisted refreshed entry")
            .recorded_at_ms;
        let second = lookup_artifact_cache_key_memo(&storage, root.to_string_lossy().as_ref())
            .expect("second memo hit");
        let persisted_second = read_cache_key_memo(&storage)
            .get(root.to_string_lossy().as_ref())
            .expect("persisted entry after second hit")
            .recorded_at_ms;

        assert!(first.recorded_at_ms > 0);
        assert_eq!(first.recorded_at_ms, persisted_first);
        assert_eq!(second.recorded_at_ms, persisted_second);
        assert_eq!(persisted_first, persisted_second);
    }

    #[test]
    fn artifact_cache_key_success_writes_memo() {
        let _probe_lock = git_root_commit_probe_override_lock_for_test();
        let dir = tempfile::tempdir().expect("create temp dir");
        let storage = dir.path().join("storage");
        let root = git_like_root(&dir, "repo");
        let commit = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_string();
        let mut commits = BTreeMap::new();
        commits.insert(root.clone(), commit.clone());
        let _override = force_git_root_commit_probe_commits_for_test(commits);

        let key = artifact_cache_key_with_memo(&root, &root, &storage, None)
            .expect("cache key from successful probe");

        assert_eq!(key, artifact_key_from_git_identity(&commit));
        let memo = read_cache_key_memo(&storage);
        let entry = memo
            .get(root.to_string_lossy().as_ref())
            .expect("memo entry for root");
        assert_eq!(entry.key, key);
        assert_eq!(entry.git_root_commit, commit);
        assert!(entry.recorded_at_ms > 0);
    }

    #[test]
    fn artifact_cache_key_probe_failure_uses_memoized_key() {
        let _probe_lock = git_root_commit_probe_override_lock_for_test();
        let dir = tempfile::tempdir().expect("create temp dir");
        let storage = dir.path().join("storage");
        let root = git_like_root(&dir, "repo");
        let commit = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb".to_string();
        let expected_key = artifact_key_from_git_identity(&commit);
        {
            let mut commits = BTreeMap::new();
            commits.insert(root.clone(), commit);
            let _override = force_git_root_commit_probe_commits_for_test(commits);
            assert_eq!(
                artifact_cache_key_with_memo(&root, &root, &storage, None).expect("initial key"),
                expected_key
            );
        }
        let _override = force_git_root_commit_probe_transient_for_paths_for_test(
            vec![root.clone()],
            "spawn failed: Too many open files (os error 24)",
        );

        let rescued = artifact_cache_key_with_memo(&root, &root, &storage, None)
            .expect("memo should rescue transient probe failure");

        assert_eq!(rescued, expected_key);
        assert_ne!(rescued, artifact_path_identity_key(&root));
    }

    #[test]
    fn subfolder_session_key_is_scoped_and_shared_by_the_same_subfolder_of_every_checkout() {
        let _probe_lock = git_root_commit_probe_override_lock_for_test();
        let dir = tempfile::tempdir().expect("create temp dir");
        let home = git_like_root(&dir, "home");
        // A linked worktree's top level holds a `.git` file, not a directory.
        let worktree = dir.path().join("wt");
        fs::create_dir_all(&worktree).expect("create worktree root");
        fs::write(worktree.join(".git"), "gitdir: elsewhere\n").expect("write .git file");
        let mut roots = Vec::new();
        for root in [&home, &worktree] {
            for sub in ["", "sub1", "sub1/deeper", "sub2"] {
                let path = if sub.is_empty() {
                    root.clone()
                } else {
                    root.join(sub)
                };
                fs::create_dir_all(&path).expect("create session root");
                roots.push(path);
            }
        }
        let commit = "cccccccccccccccccccccccccccccccccccccccc".to_string();
        let _override = force_git_root_commit_probe_commits_for_test(
            roots
                .iter()
                .map(|root| (root.clone(), commit.clone()))
                .collect(),
        );

        let repository_key = artifact_key_from_git_identity(&commit);
        let key = |path: &Path| artifact_cache_key(path);
        assert_eq!(key(&home), repository_key);
        assert_eq!(key(&worktree), repository_key);
        let sub1 = key(&home.join("sub1"));
        let deeper = key(&home.join("sub1/deeper"));
        let sub2 = key(&home.join("sub2"));
        for scoped in [&sub1, &deeper, &sub2] {
            assert_ne!(
                scoped, &repository_key,
                "a subfolder must not publish the repository artifact"
            );
            assert!(
                artifact_key_looks_valid(scoped),
                "session storage needs the 16-hex shape"
            );
        }
        assert_ne!(sub1, sub2);
        assert_ne!(sub1, deeper);
        assert_eq!(
            sub1,
            crate::scoped_key::scoped_v1_key(&commit, b"sub1").unwrap()[..16]
        );
        assert_eq!(key(&worktree.join("sub1")), sub1);
        assert_eq!(key(&worktree.join("sub1/deeper")), deeper);
        assert_eq!(key(&worktree.join("sub2")), sub2);
    }

    #[test]
    fn subfolder_probe_failure_ignores_a_repository_key_memoized_by_older_versions() {
        let _probe_lock = git_root_commit_probe_override_lock_for_test();
        let dir = tempfile::tempdir().expect("create temp dir");
        let storage = dir.path().join("storage");
        let home = git_like_root(&dir, "home");
        let sub = home.join("sub1");
        fs::create_dir_all(&sub).expect("create subfolder");
        let commit = "dddddddddddddddddddddddddddddddddddddddd".to_string();
        let repository_key = artifact_key_from_git_identity(&commit);
        let mut seeded = BTreeMap::new();
        seeded.insert(
            sub.to_string_lossy().into_owned(),
            ArtifactCacheKeyMemoEntry {
                key: repository_key.clone(),
                git_root_commit: commit.clone(),
                recorded_at_ms: current_time_millis(),
            },
        );
        write_cache_key_memo(&storage, &seeded);
        let _override = force_git_root_commit_probe_transient_for_paths_for_test(
            vec![sub.clone()],
            "spawn failed: Too many open files (os error 24)",
        );

        let rescued = artifact_cache_key_with_memo(&sub, &sub, &storage, Some(&home.join(".git")))
            .expect("memo should rescue transient probe failure");

        assert_ne!(rescued, repository_key);
        assert_eq!(
            rescued,
            crate::scoped_key::scoped_v1_key(&commit, b"sub1").unwrap()[..16]
        );
    }

    #[test]
    fn checkout_position_finds_the_nearest_git_marker() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let home = git_like_root(&dir, "home");
        let nested = home.join("a/b");
        let submodule = home.join("vendor/lib");
        fs::create_dir_all(&nested).expect("create nested dir");
        fs::create_dir_all(submodule.join("src")).expect("create submodule dir");
        fs::write(submodule.join(".git"), "gitdir: ../../.git/modules/lib\n")
            .expect("write submodule marker");

        assert_eq!(checkout_position(&home), CheckoutPosition::TopLevel);
        assert_eq!(
            checkout_position(&nested),
            CheckoutPosition::Subdirectory("a/b".to_string())
        );
        assert_eq!(checkout_position(&submodule), CheckoutPosition::TopLevel);
        assert_eq!(
            checkout_position(&submodule.join("src")),
            CheckoutPosition::Subdirectory("src".to_string())
        );
    }

    #[test]
    fn artifact_cache_key_probe_failure_without_memo_rejects_git_like_root() {
        let _probe_lock = git_root_commit_probe_override_lock_for_test();
        let dir = tempfile::tempdir().expect("create temp dir");
        let storage = dir.path().join("storage");
        let root = git_like_root(&dir, "repo");
        let _override = force_git_root_commit_probe_transient_for_paths_for_test(
            vec![root.clone()],
            "spawn failed: Too many open files (os error 24)",
        );

        let error = artifact_cache_key_with_memo(&root, &root, &storage, None)
            .expect_err("git-like root without memo must not use path identity");

        assert_eq!(error.root(), root.as_path());
        assert!(error.detail().contains("Too many open files"));
    }

    #[test]
    fn artifact_cache_key_probe_failure_without_git_marker_uses_path_identity() {
        let _probe_lock = git_root_commit_probe_override_lock_for_test();
        let dir = tempfile::tempdir().expect("create temp dir");
        let storage = dir.path().join("storage");
        let root = dir.path().join("plain");
        fs::create_dir_all(&root).expect("create non-git root");
        let _override = force_git_root_commit_probe_transient_for_paths_for_test(
            vec![root.clone()],
            "spawn failed: Too many open files (os error 24)",
        );

        let key = artifact_cache_key_with_memo(&root, &root, &storage, None)
            .expect("non-git root keeps legacy path identity fallback");

        assert_eq!(key, artifact_path_identity_key(&root));
    }

    #[test]
    fn gitless_first_bind_records_sticky_path_identity() {
        let _probe_lock = git_root_commit_probe_override_lock_for_test();
        let dir = tempfile::tempdir().unwrap();
        let storage = dir.path().join("storage");
        let root = git_like_root(&dir, "repo");
        let expected = artifact_path_identity_key(&root);
        crate::developer_tools::with_git_usable_for_test(false, || {
            assert_eq!(
                artifact_cache_key_with_memo(&root, &root, &storage, None).unwrap(),
                expected
            );
        });
        assert!(fs::read_dir(storage.join("root-identities"))
            .unwrap()
            .any(|entry| entry
                .unwrap()
                .path()
                .extension()
                .is_some_and(|extension| extension == "json")));
        let mut commits = BTreeMap::new();
        commits.insert(
            root.clone(),
            "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb".into(),
        );
        let _probe = force_git_root_commit_probe_commits_for_test(commits);
        crate::developer_tools::with_git_usable_for_test(true, || {
            assert_eq!(
                artifact_cache_key_with_memo(&root, &root, &storage, None).unwrap(),
                expected,
                "installing Git must not retag a path-bound root"
            );
            let standing = crate::scoped_key::resolve_standing_root_in_storage(
                root.to_str().unwrap(),
                &storage,
                true,
            )
            .unwrap();
            assert_eq!(standing.artifact_key, expected);
            assert_eq!(standing.resolved_git_toplevel, None);
        });
    }

    #[test]
    fn usable_git_rebind_replaces_an_unrelated_repository_identity() {
        let _probe_lock = git_root_commit_probe_override_lock_for_test();
        assert!(crate::developer_tools::git_usable());
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("checkout");
        let replacement = directory.path().join("replacement");
        let fixture: serde_json::Value = serde_json::from_str(include_str!(
            "../tests/fixtures/root_identity_upgrade/cases.json"
        ))
        .unwrap();
        for (path, content) in [
            (&root, "first repository"),
            (&replacement, "unrelated repository"),
        ] {
            fs::create_dir(path).unwrap();
            fs::write(path.join("lib.rs"), content).unwrap();
            upgrade_fixture_git(path, &["init", "-q"], &fixture["commit"]);
            upgrade_fixture_git(path, &["add", "."], &fixture["commit"]);
            upgrade_fixture_git(path, &["commit", "-q", "-m", content], &fixture["commit"]);
        }
        let storage = directory.path().join("storage");
        let first = artifact_cache_key_with_memo(&root, &root, &storage, None).unwrap();
        let expected = base_c71_cache_key(
            &replacement,
            &replacement,
            &directory.path().join("base"),
            None,
            &mut ArtifactCacheKeyTimings::default(),
        )
        .unwrap();
        assert_ne!(
            first, expected,
            "the fixtures must be unrelated repositories"
        );
        fs::rename(&root, directory.path().join("archived-checkout")).unwrap();
        fs::rename(&replacement, &root).unwrap();
        crate::developer_tools::with_git_usable_for_test(false, || {
            assert_eq!(
                artifact_cache_key_with_memo(&root, &root, &storage, None).unwrap(),
                first
            );
        });
        assert_eq!(
            artifact_cache_key_with_memo(&root, &root, &storage, None).unwrap(),
            expected,
            "usable Git must not reuse the replaced repository's key"
        );
        crate::developer_tools::with_git_usable_for_test(false, || {
            assert_eq!(
                artifact_cache_key_with_memo(&root, &root, &storage, None).unwrap(),
                expected,
                "the replacement identity must be durably recorded"
            );
        });
        fs::rename(&root, directory.path().join("second-archived-checkout")).unwrap();
        fs::create_dir(&root).unwrap();
        upgrade_fixture_git(&root, &["init", "-q"], &fixture["commit"]);
        let unborn = base_c71_cache_key(
            &root,
            &root,
            &directory.path().join("unborn-base"),
            None,
            &mut ArtifactCacheKeyTimings::default(),
        )
        .unwrap();
        assert_ne!(unborn, expected);
        assert_eq!(
            artifact_cache_key_with_memo(&root, &root, &storage, None).unwrap(),
            unborn,
            "a verified unborn replacement must not reuse the legacy repository key"
        );
    }

    #[test]
    fn recorded_repository_identity_survives_gitless_restart() {
        let dir = tempfile::tempdir().unwrap();
        let storage = dir.path().join("storage");
        let root = git_like_root(&dir, "repo");
        let commit = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb".to_string();
        let expected = artifact_key_from_git_identity(&commit);
        write_cache_key_memo(
            &storage,
            &BTreeMap::from([(
                root.parent()
                    .unwrap()
                    .join(".")
                    .join("repo")
                    .to_string_lossy()
                    .into_owned(),
                ArtifactCacheKeyMemoEntry {
                    key: expected.clone(),
                    git_root_commit: commit,
                    recorded_at_ms: current_time_millis(),
                },
            )]),
        );
        crate::developer_tools::with_git_usable_for_test(false, || {
            assert_eq!(
                artifact_cache_key_with_memo(&root, &root, &storage, None).unwrap(),
                expected
            );
            fs::remove_file(storage.join(ARTIFACT_CACHE_KEY_MEMO_FILE)).unwrap();
            artifact_cache_key_memo_state()
                .lock()
                .unwrap()
                .by_storage_root
                .remove(&storage);
            assert_eq!(
                artifact_cache_key_with_memo(&root, &root, &storage, None).unwrap(),
                expected,
                "the durable repository identity must survive without the legacy memo"
            );
            assert_ne!(expected, artifact_path_identity_key(&root));
            let standing = crate::scoped_key::resolve_standing_root_in_storage(
                root.to_str().unwrap(),
                &storage,
                true,
            )
            .unwrap();
            assert_eq!(standing.artifact_key, expected);
            assert!(standing.resolved_git_toplevel.is_some());
        });
    }

    #[test]
    fn plain_standing_root_resolves_without_usable_git() {
        let root = tempfile::tempdir().unwrap();
        crate::developer_tools::with_git_usable_for_test(false, || {
            let resolved =
                crate::scoped_key::resolve_standing_root(root.path().to_str().unwrap()).unwrap();
            assert_eq!(
                resolved.artifact_key,
                artifact_path_identity_key(root.path())
            );
            assert_eq!(resolved.resolved_git_toplevel, None);
        });
        crate::developer_tools::with_git_usable_for_test(true, || {
            crate::git_probe::with_child_env_for_test(
                &[("PATH", std::ffi::OsStr::new(""))],
                || {
                    assert!(
                        crate::scoped_key::resolve_standing_root(root.path().to_str().unwrap())
                            .is_ok(),
                        "a plain folder must resolve even when any Git spawn would fail"
                    );
                },
            );
        });
    }

    #[test]
    fn saved_identity_survives_uncertain_probe_after_legacy_memo_pruning() {
        let _probe_lock = git_root_commit_probe_override_lock_for_test();
        assert!(crate::developer_tools::git_usable());
        let directory = tempfile::tempdir().unwrap();
        let root = git_like_root(&directory, "repository");
        let storage = directory.path().join("storage");
        let saved = {
            let _probe = force_git_root_commit_probe_commits_for_test(BTreeMap::from([(
                root.clone(),
                "b".repeat(40),
            )]));
            artifact_cache_key_with_memo(&root, &root, &storage, None).unwrap()
        };
        assert!(matches!(
            crate::root_identity::read(&root, &storage).unwrap(),
            Some(crate::root_identity::Identity::Git { .. })
        ));
        fs::remove_file(storage.join(ARTIFACT_CACHE_KEY_MEMO_FILE)).unwrap();
        artifact_cache_key_memo_state()
            .lock()
            .unwrap()
            .by_storage_root
            .remove(&storage);
        let _probe = force_git_root_commit_probe_transient_for_paths_for_test(
            vec![root.clone()],
            "temporary Git probe failure",
        );
        assert_eq!(
            artifact_cache_key_with_memo(&root, &root, &storage, None).unwrap(),
            saved,
            "an uncertain probe must keep the saved identity without a legacy memo"
        );
        assert!(!storage.join(ARTIFACT_CACHE_KEY_MEMO_FILE).exists());
    }

    fn certification_fixture_repository(root: &Path, content: &str, author: &serde_json::Value) {
        fs::create_dir(root).unwrap();
        fs::write(root.join("lib.rs"), content).unwrap();
        upgrade_fixture_git(root, &["init", "-q"], author);
        upgrade_fixture_git(root, &["add", "."], author);
        upgrade_fixture_git(root, &["commit", "-q", "-m", content], author);
    }

    fn certification_standing_config(root: &Path, storage: &Path) -> crate::config::Config {
        crate::config::Config {
            storage_dir: Some(storage.to_path_buf()),
            index: crate::config::IndexConfig {
                roots: vec![crate::config::IndexRootConfig {
                    path: root.display().to_string(),
                    indexes: vec![crate::config::IndexKind::Search],
                }],
            },
            ..crate::config::Config::default()
        }
    }

    #[test]
    fn unmodeled_history_never_reuses_a_standing_identity_stamp() {
        let _probe_lock = git_root_commit_probe_override_lock_for_test();
        assert!(crate::developer_tools::git_usable());
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("repository");
        let fixture: serde_json::Value = serde_json::from_str(include_str!(
            "../tests/fixtures/root_identity_upgrade/cases.json"
        ))
        .unwrap();
        certification_fixture_repository(&root, "initial history", &fixture["commit"]);
        let output = upgrade_fixture_git(
            &root,
            &["commit-tree", "HEAD^{tree}", "-m", "unrelated history"],
            &fixture["commit"],
        );
        let replacement = String::from_utf8(output.stdout).unwrap().trim().to_string();
        let included = directory.path().join("included.gitconfig");
        fs::write(&included, "[core]\nlogAllRefUpdates = true\n").unwrap();
        upgrade_fixture_git(
            &root,
            &["config", "include.path", included.to_str().unwrap()],
            &fixture["commit"],
        );
        let root = fs::canonicalize(root).unwrap();
        assert!(
            RootCommitStamp::capture(&root).is_none(),
            "the fixture must decline history caching"
        );
        let standing = crate::standing_roots::StandingRoots::default();
        let config = certification_standing_config(&root, &directory.path().join("storage"));
        let old = standing.reconcile(&config).unwrap().active_entries[0]
            .artifact_key
            .clone();
        let expected = artifact_key_from_git_identity(&replacement);
        assert_ne!(old, expected);
        let marker_before = RootCommitFileIdentity::of(&root.join(".git"), true);
        let head_before = RootCommitFileIdentity::of(&root.join(".git/HEAD"), false);
        let branch = upgrade_fixture_git(&root, &["symbolic-ref", "HEAD"], &fixture["commit"]);
        let branch = String::from_utf8(branch.stdout).unwrap();
        fs::write(
            root.join(".git").join(branch.trim()),
            format!("{replacement}\n"),
        )
        .unwrap();
        assert_eq!(
            RootCommitFileIdentity::of(&root.join(".git"), true),
            marker_before
        );
        assert_eq!(
            RootCommitFileIdentity::of(&root.join(".git/HEAD"), false),
            head_before
        );
        let before = root_commit_git_runs_for_test(&root);
        let new = standing
            .reconcile_background(&config)
            .unwrap()
            .active_entries[0]
            .artifact_key
            .clone();
        assert_eq!(
            new, expected,
            "an unmodeled branch-ref change must re-verify identity"
        );
        assert!(root_commit_git_runs_for_test(&root) > before);
        let before = root_commit_git_runs_for_test(&root);
        standing.reconcile_background(&config).unwrap();
        assert!(
            root_commit_git_runs_for_test(&root) > before,
            "unmodeled history must verify on every pass"
        );
    }

    #[test]
    fn failed_replacement_probe_does_not_certify_the_replacement_stamp() {
        let _probe_lock = git_root_commit_probe_override_lock_for_test();
        assert!(crate::developer_tools::git_usable());
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("checkout");
        let replacement = directory.path().join("replacement");
        let fixture: serde_json::Value = serde_json::from_str(include_str!(
            "../tests/fixtures/root_identity_upgrade/cases.json"
        ))
        .unwrap();
        certification_fixture_repository(&root, "first checkout", &fixture["commit"]);
        certification_fixture_repository(&replacement, "second checkout", &fixture["commit"]);
        let replacement_commit =
            upgrade_fixture_git(&replacement, &["rev-parse", "HEAD"], &fixture["commit"]);
        let expected = artifact_key_from_git_identity(
            String::from_utf8(replacement_commit.stdout).unwrap().trim(),
        );
        let root = fs::canonicalize(root).unwrap();
        let config = certification_standing_config(&root, &directory.path().join("storage"));
        let standing = crate::standing_roots::StandingRoots::default();
        let old = standing.reconcile(&config).unwrap().active_entries[0]
            .artifact_key
            .clone();
        assert_ne!(old, expected);
        fs::rename(&root, directory.path().join("archived-checkout")).unwrap();
        fs::rename(&replacement, &root).unwrap();
        {
            let _probe = force_git_root_commit_probe_transient_for_paths_for_test(
                vec![root.clone()],
                "temporary replacement probe failure",
            );
            assert_eq!(
                standing
                    .reconcile_background(&config)
                    .unwrap()
                    .active_entries[0]
                    .artifact_key,
                old
            );
        }
        let before = root_commit_git_runs_for_test(&root);
        let recovered = standing
            .reconcile_background(&config)
            .unwrap()
            .active_entries[0]
            .artifact_key
            .clone();
        assert_eq!(
            recovered, expected,
            "a failed check must not certify the replacement's metadata"
        );
        assert!(root_commit_git_runs_for_test(&root) > before);
    }

    #[test]
    fn unchanged_standing_passes_skip_git_identity_probes() {
        let _probe_lock = git_root_commit_probe_override_lock_for_test();
        assert!(crate::developer_tools::git_usable());
        let fixture: serde_json::Value = serde_json::from_str(include_str!(
            "../tests/fixtures/root_identity_upgrade/cases.json"
        ))
        .unwrap();
        for linked in [false, true] {
            let directory = tempfile::tempdir().unwrap();
            let repository = directory.path().join("repository");
            fs::create_dir(&repository).unwrap();
            fs::write(repository.join("lib.rs"), "pub fn standing_fixture() {}\n").unwrap();
            upgrade_fixture_git(&repository, &["init", "-q"], &fixture["commit"]);
            upgrade_fixture_git(&repository, &["add", "."], &fixture["commit"]);
            upgrade_fixture_git(
                &repository,
                &["commit", "-q", "-m", "standing fixture"],
                &fixture["commit"],
            );
            let root = if linked {
                let worktree = directory.path().join("worktree");
                upgrade_fixture_git(
                    &repository,
                    &[
                        "worktree",
                        "add",
                        "--detach",
                        worktree.to_str().unwrap(),
                        "HEAD",
                    ],
                    &fixture["commit"],
                );
                worktree
            } else {
                repository
            };
            let root = fs::canonicalize(root).unwrap();
            let config = crate::config::Config {
                storage_dir: Some(directory.path().join("storage")),
                index: crate::config::IndexConfig {
                    roots: vec![crate::config::IndexRootConfig {
                        path: root.display().to_string(),
                        indexes: vec![crate::config::IndexKind::Search],
                    }],
                },
                ..crate::config::Config::default()
            };
            let standing = crate::standing_roots::StandingRoots::default();
            standing.reconcile(&config).unwrap();
            assert!(
                matches!(
                    crate::root_identity::read(&root, config.storage_dir.as_deref().unwrap())
                        .unwrap(),
                    Some(crate::root_identity::Identity::Git { .. })
                ),
                "fixture must record a Git identity (linked: {linked})"
            );
            let first = root_commit_git_runs_for_test(&root);
            assert!(
                first > 0,
                "the counter must observe the initial real Git probe"
            );
            for _ in 0..8 {
                standing.reconcile_background(&config).unwrap();
            }
            assert_eq!(
                root_commit_git_runs_for_test(&root) - first,
                0,
                "unchanged standing roots must not spawn Git (linked: {linked})"
            );
            let output = upgrade_fixture_git(
                &root,
                &["rev-parse", "--git-path", "HEAD"],
                &fixture["commit"],
            );
            let head = PathBuf::from(String::from_utf8(output.stdout).unwrap().trim());
            let head = if head.is_absolute() {
                head
            } else {
                root.join(head)
            };
            let before_head_change = repository_identity_stamp(&root);
            filetime::set_file_mtime(head, filetime::FileTime::from_unix_time(60, 0)).unwrap();
            assert_ne!(
                repository_identity_stamp(&root),
                before_head_change,
                "HEAD mtime must change repository evidence (linked: {linked})"
            );
            standing.reconcile_background(&config).unwrap();
            assert_eq!(
                root_commit_git_runs_for_test(&root) - first,
                1,
                "a changed HEAD stamp must recheck Git"
            );
            standing.reconcile_background(&config).unwrap();
            assert_eq!(root_commit_git_runs_for_test(&root) - first, 1);
            standing.reconcile(&config).unwrap();
            assert_eq!(
                root_commit_git_runs_for_test(&root) - first,
                2,
                "a real bind must still verify Git"
            );
        }
    }

    #[test]
    fn artifact_cache_key_corrupt_memo_is_absent_not_a_path_identity_escape() {
        let _probe_lock = git_root_commit_probe_override_lock_for_test();
        let dir = tempfile::tempdir().expect("create temp dir");
        let storage = dir.path().join("storage");
        fs::create_dir_all(&storage).expect("create storage root");
        fs::write(artifact_cache_key_memo_path(&storage), b"not json").expect("write corrupt memo");
        let root = git_like_root(&dir, "repo");
        let _override = force_git_root_commit_probe_transient_for_paths_for_test(
            vec![root.clone()],
            "spawn failed: Too many open files (os error 24)",
        );

        let error = artifact_cache_key_with_memo(&root, &root, &storage, None)
            .expect_err("corrupt memo is treated as absent");

        assert!(error.detail().contains("Too many open files"));
    }

    /// With `storage.cache_dir` set to a cache root outside the storage root,
    /// a search index written in a newer format on the cache root is refused,
    /// and its refusal belongs to the durable storage root that owns the cache
    /// root: health and status list it there, under the project's key, and a
    /// replacement with a current-format header clears it. Paths under the
    /// durable root itself are never re-spelled.
    #[test]
    fn cache_root_index_refusal_is_listed_and_cleared_under_its_storage_root() {
        let storage = tempfile::tempdir().unwrap();
        let cache = tempfile::tempdir().unwrap();
        let mut config = crate::config::Config {
            storage_dir: Some(storage.path().to_path_buf()),
            cache_dir: Some(cache.path().to_path_buf()),
            ..crate::config::Config::default()
        };
        assert!(crate::root_cache::prepare_cache_root_for_test(&mut config).is_none());
        let durable = crate::bash_background::storage_dir(config.storage_dir.as_deref());
        let artifacts = crate::root_cache::cache_root(&config);
        assert!(!artifacts.starts_with(&durable));
        let key = "cache-dir-refusal-key";
        assert!(crate::root_cache::durable_route(&durable.join("index").join(key)).is_none());

        let cache_dir = artifacts.join("index").join(key);
        fs::create_dir_all(&cache_dir).unwrap();
        let cache_bin = cache_dir.join("cache.bin");
        let write_header = |version: u32| {
            let mut bytes = CACHE_MAGIC.to_le_bytes().to_vec();
            bytes.extend(version.to_le_bytes());
            fs::write(&cache_bin, bytes).unwrap();
        };
        let listed_under_storage = || {
            crate::persisted_format::refusals_under(&durable)
                .into_iter()
                .filter(|refusal| {
                    refusal.store == crate::persisted_format::PersistedStore::SearchIndex
                        && refusal.path == cache_bin
                })
                .count()
        };

        write_header(INDEX_FORMAT_VERSION + 1);
        crate::persisted_format::preflight_project_artifacts(&durable, &artifacts, key);
        let refused = SearchIndex::check_disk_cache_format(&cache_dir)
            .expect_err("a newer-format index on the cache root is refused");
        assert_eq!(refused.path, cache_bin);
        assert_eq!(listed_under_storage(), 1);
        let (owned, _) = crate::persisted_format::refusals_for_push(&durable, Some(key));
        assert!(
            owned.iter().any(|refusal| refusal.path == cache_bin),
            "status attributes the refusal to this project on its storage root: {owned:?}"
        );

        write_header(INDEX_FORMAT_VERSION);
        assert!(SearchIndex::check_disk_cache_format(&cache_dir).is_ok());
        assert_eq!(listed_under_storage(), 0);
        assert!(crate::persisted_format::refusal_covering(
            crate::persisted_format::PersistedStore::SearchIndex,
            &cache_bin
        )
        .is_none());
    }
    #[test]
    fn artifact_cache_key_concurrent_memo_writes_keep_valid_json() {
        let _probe_lock = git_root_commit_probe_override_lock_for_test();
        let dir = tempfile::tempdir().expect("create temp dir");
        let storage = dir.path().join("storage");
        let root_a = git_like_root(&dir, "repo-a");
        let root_b = git_like_root(&dir, "repo-b");
        let commit_a = "cccccccccccccccccccccccccccccccccccccccc".to_string();
        let commit_b = "dddddddddddddddddddddddddddddddddddddddd".to_string();
        let mut commits = BTreeMap::new();
        commits.insert(root_a.clone(), commit_a.clone());
        commits.insert(root_b.clone(), commit_b.clone());
        let _override = force_git_root_commit_probe_commits_for_test(commits);

        let storage_a = storage.clone();
        let thread_a = std::thread::spawn({
            let root_a = root_a.clone();
            move || artifact_cache_key_with_memo(&root_a, &root_a, &storage_a, None)
        });
        let storage_b = storage.clone();
        let thread_b = std::thread::spawn({
            let root_b = root_b.clone();
            move || artifact_cache_key_with_memo(&root_b, &root_b, &storage_b, None)
        });

        let key_a = thread_a.join().expect("join writer a").expect("key a");
        let key_b = thread_b.join().expect("join writer b").expect("key b");
        let memo = read_cache_key_memo(&storage);

        assert_eq!(
            memo.get(root_a.to_string_lossy().as_ref())
                .expect("root a memo")
                .key,
            key_a
        );
        assert_eq!(
            memo.get(root_b.to_string_lossy().as_ref())
                .expect("root b memo")
                .key,
            key_b
        );
        assert_eq!(key_a, artifact_key_from_git_identity(&commit_a));
        assert_eq!(key_b, artifact_key_from_git_identity(&commit_b));
    }

    #[test]
    fn git_head_unchanged_picks_up_local_edits() {
        let _git_env = crate::test_env::hermetic_git_env_guard();
        let dir = tempfile::tempdir().expect("create temp dir");
        let project = dir.path().join("repo");
        fs::create_dir_all(&project).expect("create repo dir");
        let file = project.join("tracked.txt");
        fs::write(&file, "oldtoken\n").expect("write file");
        let mut init = Command::new("git");
        assert!(
            crate::test_env::apply_hermetic_git_env(init.current_dir(&project))
                .arg("init")
                .status()
                .unwrap()
                .success()
        );
        assert!(git_command_for_test(&project)
            .args(["add", "."])
            .status()
            .unwrap()
            .success());
        assert!(git_command_for_test(&project)
            .args([
                "-c",
                "user.name=AFT Tests",
                "-c",
                "user.email=aft-tests@example.com",
                "commit",
                "-m",
                "initial"
            ])
            .status()
            .unwrap()
            .success());
        let head = current_git_head(&project);
        let mut baseline = SearchIndex::build(&project);
        baseline.git_head = head.clone();
        fs::write(&file, "newtoken\n").expect("edit tracked file");

        let refreshed = SearchIndex::rebuild_or_refresh(
            &project,
            DEFAULT_MAX_FILE_SIZE,
            head,
            Some(baseline),
            None,
        );
        let result = refreshed.grep("newtoken", true, &[], &[], &project, 10);

        assert_eq!(result.total_matches, 1);
    }

    #[test]
    fn max_file_size_change_reclassifies_unchanged_files() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let project = dir.path().join("project");
        fs::create_dir_all(&project).expect("create project dir");
        let file = project.join("file.txt");
        fs::write(&file, "unchanged-limit-token-with-enough-bytes\n").expect("write file");

        let indexed = SearchIndex::build_with_limit(&project, 128);
        assert_eq!(
            indexed
                .grep("unchanged-limit-token", true, &[], &[], &project, 10)
                .total_matches,
            1
        );
        let lowered = SearchIndex::rebuild_or_refresh(&project, 8, None, Some(indexed), None);
        let canonical_file = fs::canonicalize(&file).expect("canonical file");
        let lowered_id = *lowered
            .path_to_id
            .get(&canonical_file)
            .expect("lowered file id");
        assert!(
            lowered.unindexed_files.contains(&lowered_id),
            "lowering the limit must classify an unchanged oversized file as unindexed"
        );

        let raised = SearchIndex::rebuild_or_refresh(&project, 128, None, Some(lowered), None);
        let raised_id = *raised
            .path_to_id
            .get(&canonical_file)
            .expect("raised file id");
        assert!(
            !raised.unindexed_files.contains(&raised_id),
            "raising the limit must index a previously unindexed unchanged file"
        );
        assert_eq!(
            raised
                .grep("unchanged-limit-token", true, &[], &[], &project, 10)
                .total_matches,
            1
        );
    }

    #[test]
    fn non_git_project_reuses_cache_when_files_unchanged() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let project = dir.path().join("project");
        fs::create_dir_all(&project).expect("create project dir");
        fs::write(project.join("file.txt"), "unchangedtoken\n").expect("write file");
        let baseline = SearchIndex::build(&project);
        let baseline_file_count = baseline.file_count();

        let refreshed = SearchIndex::rebuild_or_refresh(
            &project,
            DEFAULT_MAX_FILE_SIZE,
            None,
            Some(baseline),
            None,
        );

        assert_eq!(refreshed.file_count(), baseline_file_count);
        assert_eq!(
            refreshed
                .grep("unchangedtoken", true, &[], &[], &project, 10)
                .total_matches,
            1
        );
    }

    #[test]
    fn resolve_search_scope_disables_index_for_external_path() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let project = dir.path().join("project");
        let outside = dir.path().join("outside");
        fs::create_dir_all(&project).expect("create project dir");
        fs::create_dir_all(&outside).expect("create outside dir");

        let scope = resolve_search_scope(&project, outside.to_str());

        assert_eq!(
            scope.root,
            fs::canonicalize(&outside).expect("canonicalize outside")
        );
        assert!(!scope.use_index);
    }

    #[test]
    fn grep_filters_matches_to_search_root() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let project = dir.path().join("project");
        let src = project.join("src");
        let docs = project.join("docs");
        fs::create_dir_all(&src).expect("create src dir");
        fs::create_dir_all(&docs).expect("create docs dir");
        fs::write(src.join("main.rs"), "pub struct SearchIndex;\n").expect("write src file");
        fs::write(docs.join("guide.md"), "SearchIndex guide\n").expect("write docs file");

        let index = SearchIndex::build(&project);
        let result = index.grep("SearchIndex", true, &[], &[], &src, 10);

        assert_eq!(result.files_searched, 1);
        assert_eq!(result.files_with_matches, 1);
        assert_eq!(result.matches.len(), 1);
        // Index stores canonicalized paths; on macOS /var → /private/var
        let expected = fs::canonicalize(src.join("main.rs")).expect("canonicalize");
        assert_eq!(result.matches[0].file, expected);
    }

    #[test]
    fn grep_deduplicates_multiple_matches_on_same_line() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let project = dir.path().join("project");
        let src = project.join("src");
        fs::create_dir_all(&src).expect("create src dir");
        fs::write(src.join("main.rs"), "SearchIndex SearchIndex\n").expect("write src file");

        let index = SearchIndex::build(&project);
        let result = index.grep("SearchIndex", true, &[], &[], &src, 10);

        assert_eq!(result.total_matches, 1);
        assert_eq!(result.matches.len(), 1);
    }

    #[test]
    fn indexed_regex_grep_patterns_are_line_oriented() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let project = dir.path().join("project");
        fs::create_dir_all(&project).expect("create project dir");
        let file = project.join("stderr.log");
        fs::write(&file, "\n  566 pass\nx\n").expect("write file");
        let index = SearchIndex::build(&project);
        assert!(index.is_ready());

        let whitespace = index.grep(r"^\s+[0-9]+ pass", true, &[], &[], &project, 10);
        assert_eq!(whitespace.matches.len(), 1);
        assert_eq!(whitespace.matches[0].line, 2);
        assert_eq!(whitespace.matches[0].line_text, "  566 pass");
        assert_eq!(whitespace.matches[0].match_text, "  566 pass");

        let negated_class = index.grep(r"[^x]+", true, &[], &[], &project, 10);
        assert_eq!(negated_class.matches.len(), 1);
        assert_eq!(negated_class.matches[0].line, 2);
        assert_eq!(negated_class.matches[0].match_text, "  566 pass");
        assert!(negated_class
            .matches
            .iter()
            .all(|matched| !matched.match_text.contains('\n')));

        let explicit_newline = index.grep(r"pass\nx", true, &[], &[], &project, 10);
        assert!(explicit_newline.matches.is_empty());

        let regex = match pattern_compile::compile(r"[^x]+", CompileOpts::default()) {
            CompileResult::Ok(CompiledPattern::Regex { compiled, .. }) => compiled,
            other => panic!("compile regex: {other:?}"),
        };
        let never_keep = |_matched: &GrepMatch| false;
        let (collected, collected_lines) = matching_lines_in_content(
            &file,
            b"\n  566 pass\nx\n",
            &SearchMatcher::Regex(regex),
            10,
            &never_keep,
        );
        assert_eq!(collected_lines, 1);
        assert_eq!(collected.len(), 1);
        assert_eq!(collected[0].line, 2);
        assert_eq!(collected[0].match_text, "  566 pass");

        let literal_newline = match pattern_compile::compile(
            "pass\nx",
            CompileOpts {
                literal: true,
                ..CompileOpts::default()
            },
        ) {
            CompileResult::Ok(pattern) => pattern,
            other => panic!("compile literal newline: {other:?}"),
        };
        let result = index.search_grep(&literal_newline, &[], &[], &project, 10);
        assert!(result.matches.is_empty());
    }

    #[test]
    fn indexed_cross_line_regex_rescans_starting_line() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let project = dir.path().join("project");
        std::fs::create_dir_all(&project).expect("create project dir");
        let file = project.join("cross-line.log");
        std::fs::write(&file, "foo hit\nbar\nprefix foo hit\nbar\n")
            .expect("write cross-line fixture");
        let index = SearchIndex::build(&project);
        let result = index.grep(r"foo.*\nbar|hit", true, &[], &[], &project, 10);

        assert_eq!(result.matches.len(), 2);
        assert_eq!(result.matches[0].line, 1);
        assert_eq!(result.matches[0].line_text, "foo hit");
        assert_eq!(result.matches[0].match_text, "hit");
        assert_eq!(result.matches[1].line, 3);
        assert_eq!(result.matches[1].line_text, "prefix foo hit");
        assert_eq!(result.matches[1].match_text, "hit");
    }

    #[test]
    fn grep_case_insensitive_unicode_literal_matches_indexed_file() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let project = dir.path().join("project");
        fs::create_dir_all(&project).expect("create project dir");
        let file = project.join("unicode.txt");
        fs::write(&file, "äbc\n").expect("write unicode file");

        let index = SearchIndex::build(&project);
        let result = index.grep("Äbc", false, &[], &[], &project, 10);

        assert_eq!(result.total_matches, 1);
        assert_eq!(result.matches.len(), 1);
        assert_eq!(
            result.matches[0].file,
            fs::canonicalize(file).expect("canonicalize unicode file")
        );
    }

    fn assert_ready_indexed_literal_match(needle: &str) {
        let dir = tempfile::tempdir().expect("create temp dir");
        let project = dir.path().join("project");
        fs::create_dir_all(&project).expect("create project dir");
        let file = project.join("literal-escapes.txt");
        fs::write(&file, "foo\\.bar\na\\\\b\nfoo\\tbar\nsplit\\(\"x\"\\)\n")
            .expect("write literal escape corpus");

        let index = SearchIndex::build(&project);
        let pattern = match pattern_compile::compile(
            needle,
            CompileOpts {
                literal: true,
                ..CompileOpts::default()
            },
        ) {
            CompileResult::Ok(CompiledPattern::Literal(pattern)) => {
                CompiledPattern::Literal(pattern)
            }
            other => panic!("expected literal pattern for {needle:?}, got {other:?}"),
        };
        let result = index.search_grep(&pattern, &[], &[], &project, 10);

        assert_eq!(result.index_status, IndexStatus::Ready);
        assert!(
            !result.fully_degraded,
            "{needle:?} must use trigram candidates"
        );
        assert_eq!(
            result.total_matches, 1,
            "literal {needle:?} was filtered out"
        );
        assert_eq!(result.matches.len(), 1);
        assert_eq!(
            result.matches[0].file,
            fs::canonicalize(file).expect("canonicalize corpus file")
        );
    }

    #[test]
    fn ready_index_keeps_literal_backslash_dot_candidate() {
        assert_ready_indexed_literal_match(r"foo\.bar");
    }

    thread_local! {
        static FROZEN_GREP_SCAN_START: Instant = Instant::now();
    }

    /// A scan clock that ignores real time: a fixed per-thread instant plus
    /// whatever the test has added to the injected advance.
    fn frozen_grep_scan_now() -> Instant {
        FROZEN_GREP_SCAN_START.with(|start| *start)
            + GREP_SCAN_CLOCK_ADVANCE.with(std::cell::Cell::get)
    }

    /// Resets the injected clock after a deadline test, including on panic.
    struct GrepScanClockGuard;

    impl GrepScanClockGuard {
        fn install() -> Self {
            GREP_SCAN_CLOCK_ADVANCE.with(|advance| advance.set(Duration::ZERO));
            Self
        }
    }

    impl Drop for GrepScanClockGuard {
        fn drop(&mut self) {
            GREP_SCAN_CLOCK_ADVANCE.with(|advance| advance.set(Duration::ZERO));
        }
    }

    /// Clears the line-details probe when a test ends, even on panic.
    struct LineDetailsProbeGuard;

    impl Drop for LineDetailsProbeGuard {
        fn drop(&mut self) {
            LINE_DETAILS_PROBE.with(|probe| *probe.borrow_mut() = None);
        }
    }

    fn install_line_details_probe(probe: impl FnMut() + 'static) -> LineDetailsProbeGuard {
        LINE_DETAILS_PROBE.with(|slot| *slot.borrow_mut() = Some(Box::new(probe)));
        LineDetailsProbeGuard
    }

    fn compile_grep_pattern(
        pattern: &str,
        literal: bool,
        case_insensitive: bool,
    ) -> CompiledPattern {
        match pattern_compile::compile(
            pattern,
            CompileOpts {
                literal,
                case_insensitive,
                ..CompileOpts::default()
            },
        ) {
            CompileResult::Ok(compiled) => compiled,
            other => panic!("compile {pattern:?}: {other:?}"),
        }
    }

    /// A real FIFO open in verification's first-access directory probe stands
    /// in for consent. Stale FIFO index entries are rejected in a separate test.
    #[cfg(unix)]
    #[test]
    fn indexed_grep_scan_deadline_bounds_blocked_file_open() {
        let dir = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(dir.path()).unwrap();
        let path = root.join("indexed.txt");
        fs::write(&path, "needle\n").unwrap();
        let index = SearchIndex::build_with_limit(&root, 1024 * 1024);
        let fifo = root.join("consent-probe");
        crate::bounded_io::tests::fifo(&fifo);
        let probe = crate::bounded_io::tests::block_directory_probe(&root, &fifo);
        let (tx, rx) = std::sync::mpsc::channel();
        let thread = std::thread::spawn(move || {
            tx.send(indexed_grep_with_budget(
                &index,
                &root,
                &compile_grep_pattern("needle", true, false),
                100,
                Duration::from_millis(80),
            ))
            .unwrap();
        });
        let before_writer = rx.recv_timeout(Duration::from_millis(800));
        crate::bounded_io::tests::release_fifo(&fifo);
        thread.join().unwrap();
        let result =
            before_writer.expect("indexed verification waited in open beyond its scan deadline");
        assert!(result.matches.is_empty());
        assert_eq!(
            probe.fired.load(Ordering::Relaxed),
            1,
            "verification's boundary probe must run"
        );
        assert_eq!(result.files_searched, 0);
        assert_eq!(
            result.missing_on_disk, 0,
            "blocked access is not a deleted index entry"
        );
        assert!(result.scan_deadline_reached);
    }

    #[cfg(unix)]
    #[test]
    fn indexed_grep_never_opens_a_stale_fifo_entry() {
        let dir = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(dir.path()).unwrap();
        let path = root.join("indexed.txt");
        fs::write(&path, "needle\n").unwrap();
        let index = SearchIndex::build_with_limit(&root, 1024 * 1024);
        fs::remove_file(&path).unwrap();
        crate::bounded_io::tests::fifo(&path);
        let result = indexed_grep_with_budget(
            &index,
            &root,
            &compile_grep_pattern("needle", true, false),
            100,
            Duration::from_millis(800),
        );
        assert!(result.matches.is_empty());
        assert_eq!(result.files_searched, 0);
        assert!(
            !result.scan_deadline_reached,
            "a FIFO is excluded, not opened until a deadline"
        );
    }

    /// Indexed grep over `root` with an explicit scan budget, run on the
    /// calling thread when there are at most 10 candidate files.
    fn indexed_grep_with_budget(
        index: &SearchIndex,
        root: &Path,
        pattern: &CompiledPattern,
        max_results: usize,
        budget: Duration,
    ) -> GrepResult {
        indexed_grep_with_deadline(index, root, pattern, max_results, budget, || {
            GrepScanDeadline::after_with_clock(budget, Instant::now)
        })
    }

    fn indexed_grep_with_deadline(
        index: &SearchIndex,
        root: &Path,
        pattern: &CompiledPattern,
        max_results: usize,
        budget: Duration,
        make_deadline: impl FnOnce() -> GrepScanDeadline,
    ) -> GrepResult {
        let query = decompose_grep_pattern(pattern);
        index
            .snapshot()
            .search_grep_profiled_with_filters_and_query_and_limits_with_deadline(
                pattern,
                &query,
                &PathFilters::default(),
                root,
                max_results,
                None,
                None,
                budget,
                make_deadline,
                GrepScanOrder::Arrival,
                PATH_ORDER_BATCH_ROW_BUDGET,
            )
            .0
    }

    /// A project holding one file whose first line is about 1 MiB long with
    /// 100,000 occurrences of `needle`, the first at column 7.
    fn long_line_project() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().expect("create temp dir");
        let project = fs::canonicalize(dir.path()).expect("canonicalize temp dir");
        let mut content = String::from("start ");
        for _ in 0..100_000 {
            content.push_str("needle-pad");
        }
        content.push_str("\nlast line without the word\n");
        fs::write(project.join("minified.js"), content).expect("write long line");
        (dir, project)
    }

    #[test]
    fn long_line_with_many_occurrences_builds_line_details_once_per_arm() {
        let (_dir, project) = long_line_project();
        let index = SearchIndex::build_with_limit(&project, 8 * 1024 * 1024);
        let arms = [
            ("literal", compile_grep_pattern("needle", true, false)),
            (
                "case-insensitive literal",
                compile_grep_pattern("NEEDLE", true, true),
            ),
            ("regex", compile_grep_pattern("ne+dle-pad", false, false)),
        ];
        match (&arms[0].1, &arms[1].1, &arms[2].1) {
            (
                CompiledPattern::Literal(sensitive),
                CompiledPattern::Literal(insensitive),
                CompiledPattern::Regex { .. },
            ) => {
                assert!(!sensitive.case_insensitive_ascii);
                assert!(insensitive.case_insensitive_ascii);
            }
            other => panic!("patterns must cover all three matcher arms: {other:?}"),
        }

        for (arm_index, (arm, pattern)) in arms.iter().enumerate() {
            let budget = Duration::from_secs(2);
            // Only the scan clock is frozen. Bounded file reads retain a real
            // deadline with enough slack for a runner pause between arms.
            let read_budget = Duration::from_secs(60);
            let _io_scope = crate::bounded_io::enter_if_absent(read_budget);
            let _clock = GrepScanClockGuard::install();
            let calls = std::rc::Rc::new(std::cell::Cell::new(0usize));
            let probe_calls = calls.clone();
            // The scan clock is frozen and moves only when line details are
            // built: half the budget per build. One build, the correct work,
            // never reaches the deadline however slow the runner is; a
            // regressed per-occurrence scan reaches it on its second build and
            // ends quickly, failing the count below.
            let _probe = install_line_details_probe(move || {
                probe_calls.set(probe_calls.get() + 1);
                GREP_SCAN_CLOCK_ADVANCE.with(|advance| advance.set(advance.get() + budget / 2));
            });
            let result = indexed_grep_with_deadline(&index, &project, pattern, 100, budget, || {
                let mut deadline = GrepScanDeadline::after_with_clock(budget, frozen_grep_scan_now);
                deadline.read_at = Instant::now().checked_add(read_budget);
                if arm_index == 1 {
                    // The second read must survive a pause longer than the
                    // scan budget while the scan clock remains frozen.
                    std::thread::sleep(Duration::from_secs(3));
                }
                deadline
            });

            assert_eq!(
                calls.get(),
                1,
                "{arm}: line details must be built once per matched line, not once per occurrence"
            );
            assert!(!result.scan_deadline_reached, "{arm}: scan hit its budget");
            assert_eq!(result.total_matches, 1, "{arm}");
            assert_eq!(result.matches.len(), 1, "{arm}");
            let matched = &result.matches[0];
            assert_eq!((matched.line, matched.column), (1, 7), "{arm}");
            assert!(
                matched.line_text.starts_with("start needle-pad"),
                "{arm}: {:?}",
                &matched.line_text[..40]
            );
            assert_eq!(
                matched.line_text,
                bounded_grep_line_text(&format!("start {}", "needle-pad".repeat(100_000))),
                "{arm}: the kept line text is the bounded form of the whole line"
            );
        }
    }

    #[test]
    fn matching_lines_in_content_builds_line_details_once_per_matched_line() {
        let mut content = String::from("start ");
        content.push_str(&"needle-pad".repeat(100_000));
        content.push_str("\nsecond needle line needle\n");
        let content = content.into_bytes();
        let patterns = [
            compile_grep_pattern("needle", true, false),
            compile_grep_pattern("NEEDLE", true, true),
            compile_grep_pattern("ne+dle", false, false),
        ];
        for pattern in &patterns {
            let matcher = match pattern {
                CompiledPattern::Literal(literal) => SearchMatcher::Literal(literal.clone()),
                CompiledPattern::Regex { compiled, .. } => SearchMatcher::Regex(compiled.clone()),
            };
            let calls = std::rc::Rc::new(std::cell::Cell::new(0usize));
            let probe_calls = calls.clone();
            let _probe = install_line_details_probe(move || probe_calls.set(probe_calls.get() + 1));

            let (lines, count) =
                matching_lines_in_content(Path::new("m.js"), &content, &matcher, 10, &|_| false);

            assert_eq!(calls.get(), 2, "one line-details call per matched line");
            assert_eq!(count, 2);
            let found: Vec<(u32, u32)> =
                lines.iter().map(|line| (line.line, line.column)).collect();
            assert_eq!(found, vec![(1, 7), (2, 8)]);
        }
    }

    #[test]
    fn grep_scan_stops_at_its_deadline_across_many_large_files() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let project = fs::canonicalize(dir.path()).expect("canonicalize temp dir");
        let body = "let value = needle_marker + 1;\n".repeat(8_000);
        for file in 0..24 {
            fs::write(project.join(format!("large_{file}.js")), &body).expect("write file");
        }
        let index = SearchIndex::build_with_limit(&project, 8 * 1024 * 1024);
        let pattern = compile_grep_pattern("needle_marker", true, false);

        let started = Instant::now();
        let result =
            indexed_grep_with_budget(&index, &project, &pattern, 1_000_000, Duration::ZERO);

        assert!(
            result.scan_deadline_reached,
            "a spent budget must be reported by name, not as a complete result"
        );
        assert!(result.engine_capped);
        assert!(
            result.files_searched < 24,
            "no candidate file may be read after the deadline: searched {}",
            result.files_searched
        );
        assert!(started.elapsed() < Duration::from_secs(5));

        // The same scan with room to finish reads every file and is complete.
        let full = indexed_grep_with_budget(
            &index,
            &project,
            &pattern,
            1_000_000,
            INDEXED_GREP_SCAN_BUDGET,
        );
        assert!(!full.scan_deadline_reached);
        assert_eq!(full.files_searched, 24);
        assert_eq!(full.total_matches, 24 * 8_000);
    }

    #[test]
    fn grep_scan_stops_at_its_deadline_inside_one_file() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let project = fs::canonicalize(dir.path()).expect("canonicalize temp dir");
        fs::write(project.join("many.txt"), "needle line\n".repeat(1_000)).expect("write file");
        let index = SearchIndex::build_with_limit(&project, 8 * 1024 * 1024);
        let pattern = compile_grep_pattern("needle", true, false);
        let budget = Duration::from_secs(60);
        let _clock = GrepScanClockGuard::install();
        // Keep real file reads well inside the budget, even on a slow runner.
        // Advancing the injected clock while the first line is rendered makes
        // the next occurrence stop the scan without relying on a real sleep.
        let mut advanced_clock = false;
        let _probe = install_line_details_probe(move || {
            if !advanced_clock {
                advanced_clock = true;
                GREP_SCAN_CLOCK_ADVANCE.with(|advance| advance.set(budget * 2));
            }
        });

        let result = indexed_grep_with_deadline(&index, &project, &pattern, 10_000, budget, || {
            GrepScanDeadline::after_with_clock(budget, grep_scan_test_now)
        });

        assert!(result.scan_deadline_reached);
        assert!(result.engine_capped);
        assert_eq!(
            result.matches.len(),
            1,
            "the scan must stop inside the file"
        );
    }

    #[test]
    fn grep_scan_stops_on_cancellation_inside_one_file() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let project = fs::canonicalize(dir.path()).expect("canonicalize temp dir");
        fs::write(project.join("many.txt"), "needle line\n".repeat(1_000)).expect("write file");
        let index = SearchIndex::build_with_limit(&project, 8 * 1024 * 1024);
        let pattern = compile_grep_pattern("needle", true, false);
        let token = crate::executor::JobCancellation::new();
        let _job = crate::executor::install_job_cancellation(token.clone());
        let calls = std::rc::Rc::new(std::cell::Cell::new(0usize));
        let probe_calls = calls.clone();
        // Cancel the way a client does, while the scan is in the middle of
        // the file: after its third matched line.
        let _probe = install_line_details_probe(move || {
            probe_calls.set(probe_calls.get() + 1);
            if probe_calls.get() == 3 {
                token.request_cancel();
            }
        });

        let result =
            indexed_grep_with_budget(&index, &project, &pattern, 10_000, INDEXED_GREP_SCAN_BUDGET);

        assert_eq!(calls.get(), 3, "no line is examined after the cancel");
        assert_eq!(result.matches.len(), 3);
        assert!(result.engine_capped);
        assert!(!result.scan_deadline_reached);
    }

    #[test]
    fn bounded_line_text_renders_exactly_like_the_whole_line() {
        use crate::commands::grep::{truncate_grep_line, truncate_line_text, GREP_MAX_LINE_CHARS};

        let mut invalid = "é".repeat(GREP_MAX_LINE_CHARS - 1).into_bytes();
        invalid.extend_from_slice(&[0xff, 0xe2, 0x82, b'x']);
        invalid.extend_from_slice(&"tail".repeat(2_000).into_bytes());
        let lines: Vec<Vec<u8>> = vec![
            b"short line".to_vec(),
            Vec::new(),
            "a".repeat(GREP_MAX_LINE_CHARS).into_bytes(),
            "a".repeat(GREP_MAX_LINE_CHARS + 1).into_bytes(),
            "é".repeat(GREP_MAX_LINE_CHARS + 1).into_bytes(),
            "€".repeat(5_000).into_bytes(),
            "needle-pad".repeat(100_000).into_bytes(),
            invalid,
        ];
        for line in &lines {
            let whole = String::from_utf8_lossy(line).into_owned();
            let bounded = bounded_grep_line_text_bytes(line);
            assert_eq!(truncate_grep_line(&bounded), truncate_grep_line(&whole));
            assert_eq!(truncate_line_text(&bounded), truncate_line_text(&whole));
            if whole.chars().count() <= GREP_MAX_LINE_CHARS {
                assert_eq!(bounded, whole, "a line within the bound is kept whole");
            }
            assert!(bounded.chars().count() <= GREP_MAX_LINE_CHARS + 20);
        }
    }

    #[test]
    fn ready_index_keeps_literal_double_backslash_candidate() {
        assert_ready_indexed_literal_match(r"a\\b");
    }

    #[test]
    fn ready_index_keeps_literal_backslash_t_candidate() {
        assert_ready_indexed_literal_match(r"foo\tbar");
    }

    #[test]
    fn ready_index_keeps_literal_escaped_parentheses_candidate() {
        assert_ready_indexed_literal_match(r#"split\("x"\)"#);
    }

    #[test]
    fn refresh_reindexes_same_size_edit_with_preserved_mtime() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let project = dir.path().join("project");
        fs::create_dir_all(&project).expect("create project dir");
        let file = project.join("tokens.txt");
        let original_mtime = filetime::FileTime::from_unix_time(1_700_000_000, 0);
        fs::write(&file, "alpha").expect("write original file");
        filetime::set_file_mtime(&file, original_mtime).expect("set original mtime");

        let baseline = SearchIndex::build(&project);
        fs::write(&file, "bravo").expect("write same-size edit");
        filetime::set_file_mtime(&file, original_mtime).expect("restore original mtime");

        let refreshed = SearchIndex::rebuild_or_refresh(
            &project,
            DEFAULT_MAX_FILE_SIZE,
            None,
            Some(baseline),
            None,
        );
        let result = refreshed.grep("bravo", true, &[], &[], &project, 10);
        let canonical_file = fs::canonicalize(&file).expect("canonicalize edited file");
        let refreshed_id = *refreshed
            .path_to_id
            .get(&canonical_file)
            .expect("file remains indexed");

        assert_eq!(result.total_matches, 1);
        assert!(refreshed
            .postings_for_trigram(pack_trigram(b'b', b'r', b'a'), None)
            .contains(&refreshed_id));
        assert!(!refreshed
            .postings_for_trigram(pack_trigram(b'a', b'l', b'p'), None)
            .contains(&refreshed_id));
    }

    #[test]
    fn grep_reports_total_matches_before_truncation() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let project = dir.path().join("project");
        let src = project.join("src");
        fs::create_dir_all(&src).expect("create src dir");
        fs::write(src.join("main.rs"), "SearchIndex\nSearchIndex\n").expect("write src file");

        let index = SearchIndex::build(&project);
        let result = index.grep("SearchIndex", true, &[], &[], &src, 1);

        assert_eq!(result.total_matches, 2);
        assert_eq!(result.matches.len(), 1);
        assert!(result.truncated);
    }

    #[test]
    fn glob_filters_results_to_search_root() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let project = dir.path().join("project");
        let src = project.join("src");
        let scripts = project.join("scripts");
        fs::create_dir_all(&src).expect("create src dir");
        fs::create_dir_all(&scripts).expect("create scripts dir");
        fs::write(src.join("main.rs"), "pub fn main() {}\n").expect("write src file");
        fs::write(scripts.join("tool.rs"), "pub fn tool() {}\n").expect("write scripts file");

        let index = SearchIndex::build(&project);
        let files = index.glob("**/*.rs", &src);

        assert_eq!(
            files,
            vec![fs::canonicalize(src.join("main.rs")).expect("canonicalize src file")]
        );
    }

    #[test]
    fn snapshot_reports_file_presence_without_a_filesystem_walk() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let project = dir.path().join("project");
        let src = project.join("src");
        let empty = project.join("empty");
        fs::create_dir_all(&src).expect("create src dir");
        fs::create_dir_all(&empty).expect("create empty dir");
        fs::write(src.join("main.rs"), "pub fn main() {}\n").expect("write src file");

        let index = SearchIndex::build(&project);
        let snapshot = index.snapshot();

        assert!(snapshot.has_file_in_scope(&project));
        assert!(snapshot.has_file_in_scope(&src));
        assert!(!snapshot.has_file_in_scope(&empty));
    }

    #[test]
    fn glob_includes_hidden_and_binary_files() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let project = dir.path().join("project");
        let hidden_dir = project.join(".hidden");
        fs::create_dir_all(&hidden_dir).expect("create hidden dir");
        let hidden_file = hidden_dir.join("data.bin");
        fs::write(&hidden_file, [0u8, 159, 146, 150]).expect("write binary file");

        let index = SearchIndex::build(&project);
        let files = index.glob("**/*.bin", &project);

        assert_eq!(
            files,
            vec![fs::canonicalize(hidden_file).expect("canonicalize binary file")]
        );
    }

    #[test]
    fn read_from_disk_rejects_invalid_nanos() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let cache_dir = dir.path().join("cache");
        fs::create_dir_all(&cache_dir).expect("create cache dir");

        let mut postings = Vec::new();
        postings.extend_from_slice(INDEX_MAGIC);
        postings.extend_from_slice(&INDEX_VERSION.to_le_bytes());
        postings.extend_from_slice(&0u32.to_le_bytes());
        postings.extend_from_slice(&1u32.to_le_bytes());
        postings.extend_from_slice(&DEFAULT_MAX_FILE_SIZE.to_le_bytes());
        postings.extend_from_slice(&1u32.to_le_bytes());
        postings.extend_from_slice(b"/");
        postings.push(0u8);
        postings.extend_from_slice(&1u32.to_le_bytes());
        postings.extend_from_slice(&0u64.to_le_bytes());
        postings.extend_from_slice(&0u64.to_le_bytes());
        postings.extend_from_slice(&1_000_000_000u32.to_le_bytes());
        postings.extend_from_slice(b"a");
        postings.extend_from_slice(&0u64.to_le_bytes());

        let mut lookup = Vec::new();
        lookup.extend_from_slice(LOOKUP_MAGIC);
        lookup.extend_from_slice(&INDEX_VERSION.to_le_bytes());
        lookup.extend_from_slice(&0u32.to_le_bytes());

        let postings_checksum = crc32fast::hash(&postings);
        postings.extend_from_slice(&postings_checksum.to_le_bytes());
        let lookup_checksum = crc32fast::hash(&lookup);
        lookup.extend_from_slice(&lookup_checksum.to_le_bytes());
        let mut cache = Vec::new();
        cache.extend_from_slice(&CACHE_MAGIC.to_le_bytes());
        cache.extend_from_slice(&INDEX_VERSION.to_le_bytes());
        cache.extend_from_slice(&(postings.len() as u64).to_le_bytes());
        cache.extend_from_slice(&postings);
        cache.extend_from_slice(&lookup);
        fs::write(cache_dir.join("cache.bin"), cache).expect("write cache");

        assert!(SearchIndex::read_from_disk(&cache_dir, dir.path()).is_none());
    }

    #[test]
    fn parallel_cold_build_matches_serial_index() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let project = dir.path().join("project");
        for index in 0..80 {
            let sub = project.join(format!("pkg_{index:03}"));
            fs::create_dir_all(&sub).expect("create subdir");
            fs::write(
                sub.join("lib.rs"),
                format!(
                    "pub fn unique_marker_{index}() {{ println!(\"aft_perf_marker_{index}\"); }}\n"
                ),
            )
            .expect("write lib");
        }

        let serial = SearchIndex::build_with_limit_serial(&project, DEFAULT_MAX_FILE_SIZE);
        let parallel = SearchIndex::build_with_limit(&project, DEFAULT_MAX_FILE_SIZE);

        assert_eq!(serial.file_count(), parallel.file_count());
        assert_eq!(serial.trigram_count(), parallel.trigram_count());
        assert_eq!(serial.path_to_id.len(), parallel.path_to_id.len());
        assert_eq!(
            serial.file_trigram_count.as_ref(),
            parallel.file_trigram_count.as_ref()
        );
        for (path, id) in serial.path_to_id.iter() {
            assert_eq!(parallel.path_to_id.get(path), Some(id));
        }
        for (serial_file, parallel_file) in serial.files.iter().zip(parallel.files.iter()) {
            assert_eq!(serial_file.path, parallel_file.path);
            assert_eq!(serial_file.size, parallel_file.size);
            assert_eq!(serial_file.modified, parallel_file.modified);
            assert_eq!(serial_file.content_hash, parallel_file.content_hash);
        }

        let serial_grep = serial.grep("aft_perf_marker_17", true, &[], &[], &project, 10);
        let parallel_grep = parallel.grep("aft_perf_marker_17", true, &[], &[], &project, 10);
        assert_eq!(serial_grep.matches, parallel_grep.matches);
        assert_eq!(serial_grep.total_matches, parallel_grep.total_matches);
        assert_eq!(serial_grep.files_searched, parallel_grep.files_searched);
        assert_eq!(
            serial_grep.files_with_matches,
            parallel_grep.files_with_matches
        );
    }

    #[test]
    fn ignore_rule_discovery_respects_gitignore() {
        let _git_env = crate::test_env::hermetic_git_env_guard();
        let dir = tempfile::tempdir().expect("create temp dir");
        let project = dir.path().join("project");
        fs::create_dir_all(project.join("src")).expect("mkdir src");
        fs::write(project.join("src/.gitignore"), "data/\n").expect("write gitignore");
        let data = project.join("src/data");
        fs::create_dir_all(&data).expect("mkdir data");
        for index in 0..200 {
            fs::create_dir_all(data.join(format!("d{index}"))).expect("mkdir nested");
            fs::write(data.join(format!("d{index}/f.rs")), "fn ignored() {}\n")
                .expect("write ignored file");
        }

        let mut init = Command::new("git");
        crate::test_env::apply_hermetic_git_env(init.arg("init").arg(&project))
            .status()
            .expect("git init");
        for args in [
            ["config", "user.email", "aft@example.invalid"],
            ["config", "user.name", "AFT Test"],
        ] {
            git_command_for_test(&project)
                .args(args)
                .status()
                .expect("git config");
        }
        git_command_for_test(&project)
            .args(["add", "."])
            .status()
            .expect("git add");
        git_command_for_test(&project)
            .args(["commit", "-m", "initial"])
            .status()
            .expect("git commit");

        let legacy_dirs = count_ignore_rule_discovery_dirs_legacy_stack(&project);
        let walker_dirs = count_ignore_rule_discovery_dirs(&project);
        assert!(
            legacy_dirs > walker_dirs,
            "legacy stack should descend into gitignored data/ (legacy={legacy_dirs}, walker={walker_dirs})"
        );
        assert!(
            walker_dirs < 50,
            "ignore walker should not descend deeply into ignored tree (dirs={walker_dirs})"
        );
    }

    #[test]
    fn sort_paths_by_mtime_desc_uses_root_relative_tiebreak() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let tied_mtime = filetime::FileTime::from_unix_time(1_700_000_000, 0);
        let mut paths = ["z-last.rs", "a-first.rs", "m-middle.rs"]
            .map(|name| {
                let path = dir.path().join(name);
                fs::write(&path, format!("// {name}\n")).expect("write fixture");
                filetime::set_file_mtime(&path, tied_mtime).expect("pin fixture mtime");
                path
            })
            .to_vec();

        sort_paths_by_mtime_desc(&mut paths, dir.path());

        let expected = ["a-first.rs", "m-middle.rs", "z-last.rs"]
            .map(|name| dir.path().join(name))
            .to_vec();
        assert_eq!(paths, expected);
    }

    #[test]
    fn walked_path_sort_matches_canonical_sort_for_regular_files() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let mut paths = Vec::new();
        for index in (0..40).rev() {
            let path = dir.path().join(format!("module-{index:02}.ts"));
            fs::write(&path, format!("export const value = {index};\n")).expect("write fixture");
            let mtime = filetime::FileTime::from_unix_time(1_700_000_000 + i64::from(index % 4), 0);
            filetime::set_file_mtime(&path, mtime).expect("pin fixture mtime");
            paths.push(path);
        }

        let mut canonicalized = paths.clone();
        sort_paths_by_mtime_desc(&mut canonicalized, dir.path());
        sort_walked_paths_by_mtime_desc(&mut paths, dir.path());

        assert_eq!(paths, canonicalized);
    }

    /// Manual release-mode probe for the sort paid by one fallback glob walk.
    #[test]
    #[ignore = "manual release-mode fallback glob sort performance probe"]
    fn walked_path_sort_perf_probe() {
        const DIRECTORIES: usize = 100;
        const FILES_PER_DIRECTORY: usize = 100;
        const SAMPLES: usize = 9;
        const ITERATIONS: usize = 3;

        let temp = tempfile::tempdir().expect("create tempdir");
        let tied_mtime = filetime::FileTime::from_unix_time(1_700_000_000, 0);
        let mut paths = Vec::with_capacity(DIRECTORIES * FILES_PER_DIRECTORY);
        for directory in 0..DIRECTORIES {
            let path = temp.path().join(format!("package-{directory:03}/src"));
            fs::create_dir_all(&path).expect("create package directory");
            for file in 0..FILES_PER_DIRECTORY {
                let file_path = path.join(format!("module-{file:03}.ts"));
                fs::write(&file_path, b"export const value = 1;\n").expect("write fixture");
                filetime::set_file_mtime(&file_path, tied_mtime).expect("pin fixture mtime");
                paths.push(file_path);
            }
        }

        let mut micros_per_operation = Vec::with_capacity(SAMPLES);
        for _ in 0..SAMPLES {
            let started = Instant::now();
            for _ in 0..ITERATIONS {
                let mut sample = paths.clone();
                sort_walked_paths_by_mtime_desc(&mut sample, temp.path());
                std::hint::black_box(sample);
            }
            micros_per_operation.push(started.elapsed().as_micros() / ITERATIONS as u128);
        }
        micros_per_operation.sort_unstable();
        let median = micros_per_operation[SAMPLES / 2];

        eprintln!(
            "fallback glob sort: files={} samples={SAMPLES} iterations={ITERATIONS}",
            paths.len()
        );
        eprintln!("microseconds per glob operation: {micros_per_operation:?}");
        eprintln!("median: {median}us per glob operation");
    }

    #[cfg(windows)]
    #[test]
    fn sort_paths_by_mtime_desc_normalizes_comparison_paths_without_rewriting_results() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let canonical_root = fs::canonicalize(dir.path()).expect("canonicalize tempdir");
        let tied_mtime = filetime::FileTime::from_unix_time(1_700_000_000, 0);
        let canonical_first = canonical_root.join("a-first.rs");
        let clean_last = dir.path().join("z-last.rs");
        for path in [&canonical_first, &clean_last] {
            fs::write(path, "// tied\n").expect("write fixture");
            filetime::set_file_mtime(path, tied_mtime).expect("pin fixture mtime");
        }
        let mut paths = vec![clean_last.clone(), canonical_first.clone()];

        sort_paths_by_mtime_desc(&mut paths, &canonical_root);

        assert_eq!(paths, vec![canonical_first, clean_last]);
    }

    /// Regression: v0.15.2 — sort_paths_by_mtime_desc panicked when files
    /// changed between cmp() calls.
    ///
    /// Pre-fix, the sort closure called `path_modified_time(path)` directly,
    /// which does a `stat()` syscall. If the file was deleted, modified, or
    /// touched mid-sort, the comparator returned different values for the
    /// same input pair on different invocations. Rust's slice::sort detects
    /// this and panics with "user-provided comparison function does not
    /// correctly implement a total order".
    ///
    /// CI hit this on a Pi e2e test (workflow run 24887807972) where the
    /// bridge invalidated files in parallel with grep's sort path. This
    /// test simulates the worst case: most paths don't exist (Err from
    /// fs::metadata) and sort still completes successfully.
    #[test]
    fn sort_paths_by_mtime_desc_does_not_panic_on_missing_files() {
        // Mix of existing and non-existing paths in deliberately
        // non-monotonic order — pre-fix, the sort would call stat() at
        // least N log N times and any flakiness would trigger the panic.
        let dir = tempfile::tempdir().expect("create tempdir");
        let mut paths: Vec<PathBuf> = Vec::new();
        for i in 0..30 {
            // Half exist, half don't.
            let path = if i % 2 == 0 {
                let p = dir.path().join(format!("real-{i}.rs"));
                fs::write(&p, format!("// {i}\n")).expect("write");
                p
            } else {
                dir.path().join(format!("missing-{i}.rs"))
            };
            paths.push(path);
        }

        // Run the sort many times to maximise the chance of catching any
        // residual non-determinism. Pre-fix: panic. Post-fix: stable.
        for _ in 0..50 {
            let mut copy = paths.clone();
            sort_paths_by_mtime_desc(&mut copy, dir.path());
            assert_eq!(copy.len(), paths.len());
        }
    }

    /// Regression: the indexed parallel search's reduce() combine closure must
    /// NOT set engine_capped. reduce runs on every partial-result merge in a
    /// multi-chunk parallel search (>10 candidate files), capped or not — an
    /// unconditional store there falsely reported every such grep as capped,
    /// lying to the agent that results were truncated.
    #[test]
    fn uncapped_indexed_grep_over_many_files_is_not_engine_capped() {
        let dir = tempfile::tempdir().expect("create tempdir");
        // >10 files so the parallel (reduce) branch is taken, each with exactly
        // one match, and a generous cap so the search is NOT actually capped.
        for i in 0..40 {
            fs::write(
                dir.path().join(format!("file-{i}.rs")),
                format!("fn unique_marker_{i}() {{ let _ = \"needle_token\"; }}\n"),
            )
            .expect("write");
        }
        let index = SearchIndex::build_with_limit(dir.path(), DEFAULT_MAX_FILE_SIZE);
        let result = index.grep("needle_token", false, &[], &[], dir.path(), 1000);
        assert!(
            result.matches.len() >= 40,
            "expected a match per file, got {}",
            result.matches.len()
        );
        assert!(
            !result.engine_capped,
            "an uncapped grep over >10 files must not report engine_capped"
        );
        assert!(!result.truncated, "uncapped grep must not be truncated");
    }

    /// Regression: v0.15.2 — sort_grep_matches_by_mtime_desc panicked under
    /// the same conditions as sort_paths_by_mtime_desc. See the
    /// sort_paths_... test above for the full rationale.
    #[test]
    fn sort_grep_matches_by_mtime_desc_does_not_panic_on_missing_files() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let mut matches: Vec<GrepMatch> = Vec::new();
        for i in 0..30 {
            let file = if i % 2 == 0 {
                let p = dir.path().join(format!("real-{i}.rs"));
                fs::write(&p, format!("// {i}\n")).expect("write");
                p
            } else {
                dir.path().join(format!("missing-{i}.rs"))
            };
            matches.push(GrepMatch {
                file,
                line: u32::try_from(i).unwrap_or(0),
                column: 0,
                line_text: format!("match {i}"),
                match_text: format!("match {i}"),
            });
        }

        for _ in 0..50 {
            let mut copy = matches.clone();
            sort_grep_matches_by_mtime_desc(&mut copy, dir.path());
            assert_eq!(copy.len(), matches.len());
        }
    }

    #[test]
    fn out_of_order_delta_refresh_matches_full_sort_reference() {
        const FILES: u32 = 1_024;
        let shared_trigram = pack_trigram(b's', b'h', b'r');
        let mut optimized = Vec::new();
        let mut reference = Vec::new();

        for file_id in (0..FILES).rev() {
            let posting = Posting {
                file_id,
                next_mask: 0,
                loc_mask: 0,
            };
            insert_delta_posting(&mut optimized, posting.clone());
            insert_delta_posting_full_sort_reference(&mut reference, posting);
        }

        assert_eq!(
            optimized, reference,
            "delta postings must stay file-id sorted"
        );

        let mut index = SearchIndex::new();
        let files = Arc::make_mut(&mut index.files);
        for file_id in 0..FILES {
            files.push(FileEntry {
                path: PathBuf::from(format!("/delta/file-{file_id:04}.rs")),
                size: 0,
                modified: UNIX_EPOCH,
                content_hash: cache_freshness::zero_hash(),
            });
        }
        Arc::make_mut(&mut index.delta)
            .postings
            .insert(shared_trigram, optimized);

        let actual = index.candidates(&RegexQuery {
            and_trigrams: vec![shared_trigram],
            ..RegexQuery::default()
        });
        let expected = reference
            .into_iter()
            .map(|posting| posting.file_id)
            .collect::<Vec<_>>();
        assert_eq!(
            serde_json::to_vec(&actual).expect("serialize candidates"),
            serde_json::to_vec(&expected).expect("serialize reference candidates"),
            "candidate IDs must match the full-sort reference byte-for-byte"
        );
    }

    #[test]
    #[ignore = "manual release-mode issue #219 delta insertion performance probe"]
    fn issue_219_delta_insertion_perf_probe() {
        const FILES: u32 = 1_024;
        const SAMPLES: usize = 9;
        const ITERATIONS: usize = 8;

        let reference_once = || {
            let mut postings = Vec::with_capacity(FILES as usize);
            for file_id in (0..FILES).rev() {
                insert_delta_posting_full_sort_reference(
                    &mut postings,
                    Posting {
                        file_id,
                        next_mask: 0,
                        loc_mask: 0,
                    },
                );
            }
            std::hint::black_box(postings);
        };
        let optimized_once = || {
            let mut postings = Vec::with_capacity(FILES as usize);
            for file_id in (0..FILES).rev() {
                insert_delta_posting(
                    &mut postings,
                    Posting {
                        file_id,
                        next_mask: 0,
                        loc_mask: 0,
                    },
                );
            }
            std::hint::black_box(postings);
        };

        let mut reference_ns = Vec::with_capacity(SAMPLES);
        let mut optimized_ns = Vec::with_capacity(SAMPLES);
        for sample in 0..SAMPLES {
            let measure = |operation: &dyn Fn()| {
                let started = Instant::now();
                for _ in 0..ITERATIONS {
                    operation();
                }
                started.elapsed().as_nanos() / ITERATIONS as u128
            };
            if sample % 2 == 0 {
                reference_ns.push(measure(&reference_once));
                optimized_ns.push(measure(&optimized_once));
            } else {
                optimized_ns.push(measure(&optimized_once));
                reference_ns.push(measure(&reference_once));
            }
        }
        reference_ns.sort_unstable();
        optimized_ns.sort_unstable();
        let reference_median = reference_ns[SAMPLES / 2];
        let optimized_median = optimized_ns[SAMPLES / 2];
        let speedup = reference_median as f64 / optimized_median as f64;

        eprintln!(
            "issue #219 delta insertion: files={FILES} samples={SAMPLES} iterations={ITERATIONS}"
        );
        eprintln!("full-sort ns/refresh samples: {reference_ns:?}");
        eprintln!("binary-insert ns/refresh samples: {optimized_ns:?}");
        eprintln!(
            "median: full-sort={reference_median}ns binary-insert={optimized_median}ns speedup={speedup:.2}x"
        );
    }
}

#[cfg(test)]
mod warm_reload_verification_tests {
    use super::*;

    /// `read_from_disk` resolves a root alias before deriving keys, so a caller
    /// that removes a file by the alias spelling must still reach the cached
    /// record. On Windows the alias is a case variant and the canonical
    /// fallback is verbatim while a plain-root load stores plain keys.
    #[test]
    #[cfg(any(unix, windows))]
    fn remove_file_through_root_alias_supersedes_the_cached_record() {
        let dir = tempfile::tempdir().unwrap();
        let real_root = dir.path().join("checkout");
        fs::create_dir_all(&real_root).unwrap();
        #[cfg(unix)]
        let alias_root = {
            let alias = dir.path().join("alias");
            std::os::unix::fs::symlink(&real_root, &alias).unwrap();
            alias
        };
        #[cfg(windows)]
        let alias_root = dir.path().join("CHECKOUT");
        fs::write(real_root.join("cached.txt"), "AliasRemovalNeedle\n").unwrap();
        let cache = tempfile::tempdir().unwrap();
        let mut built = SearchIndex::build(&real_root);
        assert!(built.write_to_disk(cache.path(), None));

        let mut index = SearchIndex::read_from_disk(cache.path(), &alias_root).unwrap();
        #[cfg(windows)]
        assert!(!index.project_root.to_string_lossy().starts_with(r"\\?\"));
        let stored_key = index.project_root.join("cached.txt");
        let cached_id = index.path_to_id[&stored_key];
        assert!(cached_id < index.base_file_count);

        index.remove_file(&alias_root.join("cached.txt"));

        assert!(!index.path_to_id.contains_key(&stored_key));
        assert!(index.delta.superseded.contains(&cached_id));
        assert!(index.delta.superseded_paths.contains(&stored_key));
    }

    /// Windows runners can hand out 8.3 short temp paths (`C:\Users\RUNNER~1`).
    /// Loading from such a root resolves it to the long name, so removing a
    /// file by its short spelling must still reach the long-name cached key.
    #[test]
    #[cfg(windows)]
    fn remove_file_through_short_name_root_supersedes_the_cached_record() {
        use std::os::windows::ffi::{OsStrExt, OsStringExt};

        let dir = tempfile::tempdir().unwrap();
        // Longer than eight characters, so the volume assigns it a short name
        // when 8.3 name generation is enabled.
        let real_root = dir.path().join("checkout_with_long_name");
        fs::create_dir_all(&real_root).unwrap();
        fs::write(real_root.join("cached.txt"), "ShortNameRemovalNeedle\n").unwrap();

        let wide: Vec<u16> = real_root
            .as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();
        let mut buffer = vec![0u16; 1024];
        // SAFETY: `wide` is NUL-terminated and `buffer` holds `buffer.len()` units.
        let written = unsafe {
            windows_sys::Win32::Storage::FileSystem::GetShortPathNameW(
                wide.as_ptr(),
                buffer.as_mut_ptr(),
                buffer.len() as u32,
            )
        } as usize;
        assert!(
            written > 0 && written < buffer.len(),
            "GetShortPathNameW failed"
        );
        let short_root = PathBuf::from(std::ffi::OsString::from_wide(&buffer[..written]));
        if short_root.file_name() == real_root.file_name() {
            eprintln!(
                "skipping: 8.3 short names are disabled on the volume holding {}",
                real_root.display()
            );
            return;
        }

        let cache = tempfile::tempdir().unwrap();
        let mut built = SearchIndex::build(&real_root);
        assert!(built.write_to_disk(cache.path(), None));

        let mut index = SearchIndex::read_from_disk(cache.path(), &short_root).unwrap();
        assert!(!index.project_root.to_string_lossy().starts_with(r"\\?\"));
        assert_eq!(index.project_root.file_name(), real_root.file_name());
        let stored_key = index.project_root.join("cached.txt");
        let cached_id = index.path_to_id[&stored_key];

        index.remove_file(&short_root.join("cached.txt"));

        assert!(!index.path_to_id.contains_key(&stored_key));
        assert!(index.delta.superseded.contains(&cached_id));
    }

    #[test]
    #[cfg(any(unix, windows))]
    fn warm_refresh_keeps_cached_and_new_files_visible_through_root_alias() {
        let dir = tempfile::tempdir().unwrap();
        let real_root = dir.path().join("checkout");
        fs::create_dir_all(real_root.join("src")).unwrap();
        #[cfg(unix)]
        let alias_root = {
            let alias = dir.path().join("alias");
            std::os::unix::fs::symlink(&real_root, &alias).unwrap();
            alias
        };
        #[cfg(windows)]
        let alias_root = dir.path().join("CHECKOUT");
        let canonical_root = fs::canonicalize(&real_root).unwrap();
        let plain_subfolder = crate::inspect::job::normalize_path(&canonical_root).join("src");
        #[cfg(windows)]
        assert!(!plain_subfolder.to_string_lossy().starts_with(r"\\?\"));
        assert_ne!(alias_root, canonical_root);
        assert_eq!(fs::canonicalize(&alias_root).unwrap(), canonical_root);
        let changed = real_root.join("src/changed.rs");
        fs::write(&changed, "fn before() {}\n").unwrap();
        fs::write(
            real_root.join("src/unchanged.rs"),
            "// UnchangedAliasNeedle\n",
        )
        .unwrap();
        let original_mtime =
            filetime::FileTime::from_last_modification_time(&fs::metadata(&changed).unwrap());
        let cache = tempfile::tempdir().unwrap();
        let mut built = SearchIndex::build(&real_root);
        assert!(built.write_to_disk(cache.path(), None));
        fs::write(&changed, "// ChangedAliasNeedle with different size\n").unwrap();
        filetime::set_file_mtime(&changed, original_mtime).unwrap();
        fs::write(real_root.join("src/new.rs"), "// NewAliasNeedle\n").unwrap();

        for strategy in [
            cache_freshness::VerifyStrategy::StatFirst,
            cache_freshness::VerifyStrategy::Strict,
        ] {
            assert!(built.write_to_disk(cache.path(), None));
            let baseline = SearchIndex::read_from_disk(cache.path(), &alias_root).unwrap();
            let loaded_root = crate::inspect::job::canonicalize_normalized(&alias_root);
            assert_eq!(baseline.project_root, loaded_root);
            #[cfg(windows)]
            assert!(!baseline.project_root.to_string_lossy().starts_with(r"\\?\"));
            let cached_path = loaded_root.join("src/changed.rs");
            let cached_id = baseline.path_to_id[&cached_path];
            let mut refreshed = SearchIndex::refresh_cached_with_diff_limit(
                baseline,
                &alias_root,
                DEFAULT_MAX_FILE_SIZE,
                None,
                strategy,
                Some(2),
            )
            .expect("one cached edit and one new file fit the two-path budget");
            assert!(refreshed.delta.superseded.contains(&cached_id));
            assert!(refreshed.delta.superseded_paths.contains(&cached_path));
            assert_eq!(refreshed.live_file_identities().len(), 3);
            for persisted in [false, true] {
                if persisted {
                    assert!(refreshed.write_to_disk(cache.path(), None));
                    let baseline = SearchIndex::read_from_disk(cache.path(), &alias_root).unwrap();
                    assert_eq!(baseline.project_root, loaded_root);
                    refreshed = SearchIndex::refresh_cached_with_diff_limit(
                        baseline,
                        &alias_root,
                        DEFAULT_MAX_FILE_SIZE,
                        None,
                        strategy,
                        Some(0),
                    )
                    .expect("persisted canonical keys must fit a zero-path refresh budget");
                }
                for scope in [
                    alias_root.clone(),
                    canonical_root.join("src"),
                    plain_subfolder.clone(),
                ] {
                    for needle in [
                        "ChangedAliasNeedle",
                        "UnchangedAliasNeedle",
                        "NewAliasNeedle",
                    ] {
                        assert_eq!(
                            refreshed.grep(needle, true, &[], &[], &scope, 10).matches.len(),
                            1,
                            "ready index must find {needle} through {scope:?} (persisted={persisted}, strategy={strategy:?})"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn warm_refresh_preserves_loaded_path_keys_across_root_spellings() {
        let dir = tempfile::tempdir().unwrap();
        let verbatim_root = fs::canonicalize(dir.path()).unwrap();
        let plain_root = crate::inspect::job::normalize_path(&verbatim_root);
        #[cfg(windows)]
        assert_ne!(verbatim_root, plain_root);
        fs::write(plain_root.join("unchanged.rs"), "fn unchanged() {}\n").unwrap();
        let cache = tempfile::tempdir().unwrap();
        let mut built = SearchIndex::build(&plain_root);
        assert!(built.write_to_disk(cache.path(), None));

        for loading_root in [&plain_root, &verbatim_root] {
            for refresh_root in [&plain_root, &verbatim_root] {
                for strategy in [
                    cache_freshness::VerifyStrategy::StatFirst,
                    cache_freshness::VerifyStrategy::Strict,
                ] {
                    let baseline = SearchIndex::read_from_disk(cache.path(), loading_root).unwrap();
                    let loaded_root = baseline.project_root.clone();
                    let original_keys = baseline.path_to_id.clone();
                    let refreshed = SearchIndex::refresh_cached_with_diff_limit(
                        baseline,
                        refresh_root,
                        DEFAULT_MAX_FILE_SIZE,
                        None,
                        strategy,
                        Some(0),
                    )
                    .expect("unchanged cached paths must fit a zero-path budget");
                    assert!(refreshed.is_ready());
                    assert_eq!(refreshed.project_root, loaded_root);
                    assert_eq!(refreshed.project_root, *loading_root);
                    assert_eq!(refreshed.path_to_id, original_keys);
                    assert_eq!(
                        refreshed
                            .grep("unchanged", true, &[], &[], refresh_root, 10)
                            .matches
                            .len(),
                        1
                    );
                }
            }
        }
    }

    #[test]
    fn warm_refresh_rejects_local_changes_before_source_reads() {
        let dir = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(dir.path()).unwrap();
        let touched = root.join("touched.rs");
        fs::write(&touched, "fn before() {}\n").unwrap();
        fs::write(root.join("resized.rs"), "fn before() {}\n").unwrap();
        let baseline = SearchIndex::build(&root);
        fs::write(&touched, "fn afterx() {}\n").unwrap();
        filetime::set_file_mtime(&touched, filetime::FileTime::from_unix_time(1, 0)).unwrap();
        fs::write(root.join("resized.rs"), "fn local_resize() {}\n").unwrap();
        fs::write(root.join("new.rs"), "fn local_new() {}\n").unwrap();

        audit_work_reset();
        let watch = cache_freshness::watch_verify_stats(
            &root,
            cache_freshness::VerifyScheduleForTest::SingleWorkerDrain,
        );
        let refreshed = SearchIndex::refresh_cached_with_diff_limit(
            baseline,
            &root,
            DEFAULT_MAX_FILE_SIZE,
            None,
            cache_freshness::VerifyStrategy::StatFirst,
            Some(2),
        );
        assert!(
            refreshed.is_none(),
            "three local changes exceed the two-path budget"
        );
        assert_eq!(
            audit_work().corpus_opens,
            0,
            "admission must precede source reads"
        );
        let stats = watch.take_stats();
        assert!(stats.hash_reads.is_empty());
        assert!(stats.corpus_reads.is_empty());
    }

    #[test]
    fn warm_refresh_reads_admitted_local_changes_once() {
        for (strategy, preserve_mtime) in [
            (cache_freshness::VerifyStrategy::StatFirst, false),
            (cache_freshness::VerifyStrategy::Strict, false),
            (cache_freshness::VerifyStrategy::Strict, true),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let root = fs::canonicalize(dir.path()).unwrap();
            let path = root.join("changed.rs");
            fs::write(&path, "fn before() {}\n").unwrap();
            let baseline = SearchIndex::build(&root);
            let original_mtime =
                filetime::FileTime::from_last_modification_time(&fs::metadata(&path).unwrap());
            fs::write(&path, "fn afterx() {}\n").unwrap();
            filetime::set_file_mtime(
                &path,
                if preserve_mtime {
                    original_mtime
                } else {
                    filetime::FileTime::from_unix_time(1, 0)
                },
            )
            .unwrap();
            audit_work_reset();
            let watch = cache_freshness::watch_verify_stats(
                &root,
                cache_freshness::VerifyScheduleForTest::SingleWorkerDrain,
            );
            let refreshed = SearchIndex::refresh_cached_with_diff_limit(
                baseline,
                &root,
                DEFAULT_MAX_FILE_SIZE,
                None,
                strategy,
                Some(1),
            )
            .expect("one local change fits the budget");
            assert_eq!(audit_work().corpus_opens, 1);
            let stats = watch.take_stats();
            assert_eq!(
                stats.corpus_reads.len(),
                1,
                "one content read for indexing and hashing"
            );
            assert_eq!(
                stats.calls.len(),
                1,
                "reuse the admission stat for indexing"
            );
            assert!(
                stats.hash_reads.is_empty(),
                "verification must reuse the content read for indexing"
            );
            assert_eq!(
                refreshed
                    .grep("afterx", true, &[], &[], &root, 10)
                    .matches
                    .len(),
                1
            );
        }
    }

    #[test]
    fn strict_warm_refresh_reads_in_bounded_workers_with_one_stat_and_read_per_file() {
        let dir = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(dir.path()).unwrap();
        for n in 0..32 {
            fs::write(root.join(format!("file-{n}.rs")), "fn before() {}\n").unwrap();
        }
        let baseline = SearchIndex::build(&root);
        for schedule in [
            cache_freshness::VerifyScheduleForTest::SingleWorkerDrain,
            cache_freshness::VerifyScheduleForTest::Concurrent,
        ] {
            let watch = cache_freshness::watch_verify_stats(&root, schedule);
            let refreshed = SearchIndex::refresh_cached_with_diff_limit(
                baseline.clone(),
                &root,
                DEFAULT_MAX_FILE_SIZE,
                None,
                cache_freshness::VerifyStrategy::Strict,
                Some(0),
            )
            .expect("unchanged files fit even a zero-path budget");
            let stats = watch.take_stats();
            assert_eq!(stats.pool_size, Some(2));
            assert_eq!(stats.calls.len(), 32, "one admission stat per cached file");
            let stat_paths: HashSet<_> = stats.calls.iter().map(|(path, _)| path).collect();
            assert_eq!(
                stat_paths.len(),
                32,
                "each cached file is statted exactly once"
            );
            assert_eq!(
                stats.corpus_reads.len(),
                32,
                "strict must read unchanged files"
            );
            assert!(
                stats.hash_reads.is_empty(),
                "hash the corpus read, not a second read"
            );
            let mut counts = HashMap::new();
            let mut threads = HashSet::new();
            for (path, thread) in stats.corpus_reads {
                *counts.entry(path).or_insert(0) += 1;
                threads.insert(thread);
                assert_ne!(
                    thread,
                    std::thread::current().id(),
                    "reads must run in the pool"
                );
            }
            assert_eq!(counts.len(), refreshed.path_to_id.len());
            assert!(counts.values().all(|count| *count == 1));
            assert!(
                threads.len() <= 2,
                "reads must stay within the bounded pool"
            );
            match schedule {
                cache_freshness::VerifyScheduleForTest::SingleWorkerDrain => {
                    assert_eq!(threads.len(), 1);
                }
                cache_freshness::VerifyScheduleForTest::Concurrent => {
                    assert_eq!(threads.len(), 2);
                    assert!(stats.concurrent_reads);
                }
            }
        }
    }

    #[test]
    fn warm_verify_uses_bounded_workers_and_one_stat_per_indexed_file() {
        let dir = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(dir.path()).unwrap();
        for n in 0..128 {
            fs::write(root.join(format!("file-{n}.rs")), vec![b'x'; 64 * 1024]).unwrap();
        }
        let mut index = SearchIndex::build(&root);
        for schedule in [
            cache_freshness::VerifyScheduleForTest::SingleWorkerDrain,
            cache_freshness::VerifyScheduleForTest::Concurrent,
        ] {
            // A two-worker pool may legitimately drain a small batch on one
            // worker. Check the installed pool independently of that schedule,
            // then force overlap to prove verification can run concurrently.
            let watch = cache_freshness::watch_verify_stats(&root, schedule);
            verify_file_mtimes(&mut index, cache_freshness::VerifyStrategy::Strict);
            let stats = watch.take_stats();
            assert_eq!(
                stats.pool_size,
                Some(2),
                "bounded verification must install the two-worker pool"
            );
            assert!(
                !index.path_to_id.is_empty(),
                "verification fixture has indexed files"
            );
            let mut counts = HashMap::new();
            let mut threads = HashSet::new();
            for (path, thread) in stats.calls {
                *counts.entry(path).or_insert(0) += 1;
                threads.insert(thread);
            }
            for path in index.path_to_id.keys() {
                assert_eq!(
                    counts.get(path),
                    Some(&1),
                    "indexed file stat count: {}",
                    path.display()
                );
            }
            match schedule {
                cache_freshness::VerifyScheduleForTest::SingleWorkerDrain => {
                    assert_eq!(
                        threads.len(),
                        1,
                        "forced unsplit batch drains on one worker"
                    );
                }
                cache_freshness::VerifyScheduleForTest::Concurrent => {
                    assert!(
                        stats.concurrent,
                        "two workers must take items before the first completes"
                    );
                }
            }
        }
    }

    #[test]
    fn warm_verify_matches_serial_reconciliation_for_both_strategies() {
        for strategy in [
            cache_freshness::VerifyStrategy::Strict,
            cache_freshness::VerifyStrategy::StatFirst,
        ] {
            let dir = tempfile::tempdir().unwrap();
            let root = fs::canonicalize(dir.path()).unwrap();
            for name in ["changed.rs", "deleted.rs", "touched.rs", "unchanged.rs"] {
                fs::write(root.join(name), format!("// {name}\n")).unwrap();
            }
            let mut parallel = SearchIndex::build(&root);
            let mut serial = parallel.clone();
            fs::write(root.join("changed.rs"), "// different bytes\n").unwrap();
            fs::remove_file(root.join("deleted.rs")).unwrap();
            filetime::set_file_mtime(
                root.join("touched.rs"),
                filetime::FileTime::from_unix_time(1, 0),
            )
            .unwrap();
            fs::write(root.join("new.rs"), "// new\n").unwrap();

            // Preserve the former serial algorithm as a reference for the complete
            // path reconciliation, not merely the per-file freshness verdicts.
            let walked = walk_project_files(&root, &PathFilters::default());
            let walked_set: HashSet<PathBuf> = walked.iter().cloned().collect();
            let mut stale = Vec::new();
            let mut removed = Vec::new();
            for entry in Arc::make_mut(&mut serial.files).iter_mut() {
                if entry.path.as_os_str().is_empty() {
                    continue;
                }
                if !walked_set.contains(&entry.path) {
                    removed.push(entry.path.clone());
                    continue;
                }
                let cached = FileFreshness {
                    mtime: entry.modified,
                    size: entry.size,
                    content_hash: entry.content_hash,
                };
                let verdict = match strategy {
                    cache_freshness::VerifyStrategy::Strict => {
                        cache_freshness::verify_file_strict(&entry.path, &cached)
                    }
                    cache_freshness::VerifyStrategy::StatFirst => {
                        cache_freshness::verify_file(&entry.path, &cached)
                    }
                };
                match verdict {
                    FreshnessVerdict::HotFresh => {}
                    FreshnessVerdict::ContentFresh {
                        new_mtime,
                        new_size,
                    } => {
                        entry.modified = new_mtime;
                        entry.size = new_size;
                    }
                    FreshnessVerdict::Stale | FreshnessVerdict::Deleted => {
                        stale.push(entry.path.clone())
                    }
                }
            }
            assert_eq!(removed, vec![root.join("deleted.rs")]);
            assert!(stale.contains(&root.join("changed.rs")));
            assert!(!stale.contains(&root.join("touched.rs")));
            let mut memo = ParentCanonicalizationMemo::default();
            for path in &removed {
                serial.remove_file_with_canonicalization_memo(path, &mut memo);
            }
            for path in &stale {
                if walked_set.contains(path) {
                    serial.update_file_with_canonicalization_memo(path, &mut memo);
                } else {
                    serial.remove_file_with_canonicalization_memo(path, &mut memo);
                }
            }
            for path in walked {
                if !serial.path_to_id.contains_key(&path) {
                    serial.update_file_with_canonicalization_memo(&path, &mut memo);
                }
            }
            assert!(verify_file_mtimes(&mut parallel, strategy));
            for name in ["changed.rs", "touched.rs", "unchanged.rs", "new.rs"] {
                let path = root.join(name);
                let actual = &parallel.files[parallel.path_to_id[&path] as usize];
                let expected = &serial.files[serial.path_to_id[&path] as usize];
                assert_eq!(
                    (actual.modified, actual.size, actual.content_hash),
                    (expected.modified, expected.size, expected.content_hash),
                    "{name}"
                );
            }
            assert!(!parallel.path_to_id.contains_key(&root.join("deleted.rs")));
            assert_eq!(parallel.path_to_id.len(), serial.path_to_id.len());
        }
    }

    #[test]
    fn warm_disk_verification_uses_stat_first_and_hashes_changed_stats() {
        let dir = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(dir.path()).unwrap();
        let path = root.join("warm.rs");
        fs::write(&path, "fn warm_reload() {}\n").unwrap();
        let original_mtime = filetime::FileTime::from_unix_time(1_700_000_000, 0);
        filetime::set_file_mtime(&path, original_mtime).unwrap();
        let mut index = SearchIndex::build(&root);

        cache_freshness::watch_hash_file_for_debug(&path);
        index.verify_against_disk_with_strategy(None, cache_freshness::VerifyStrategy::StatFirst);
        assert_eq!(cache_freshness::watched_hash_file_count_for_debug(), 0);

        filetime::set_file_mtime(&path, filetime::FileTime::from_unix_time(1, 0)).unwrap();
        cache_freshness::watch_hash_file_for_debug(&path);
        index.verify_against_disk_with_strategy(None, cache_freshness::VerifyStrategy::StatFirst);
        assert_eq!(cache_freshness::watched_hash_file_count_for_debug(), 1);
    }

    #[test]
    fn write_denied_cold_build_flags_build_denied_instead_of_building() {
        // A borrow-only root cannot write the shared search artifact. The cold
        // build must flag the empty index as build-denied (and leave it
        // not-ready) so health reports a settled state while grep/glob keep
        // serving through the bounded fallback walk — never a permanent
        // "building".
        let project = tempfile::tempdir().expect("project");
        let source = project.path().join("lib.rs");
        fs::write(&source, "pub fn answer() -> i32 { 42 }\n").expect("write source");
        let project_key = "shared-search-artifact".to_string();
        crate::root_cache::configure_artifact_access(project.path(), &project_key, true);

        // cache_dir.file_name() == project_key (the shared key) → write denied.
        let storage = tempfile::tempdir().expect("storage");
        let cache_dir = storage.path().join(&project_key);
        let index = SearchIndex::build_with_limit_to_cache_dir(
            project.path(),
            DEFAULT_MAX_FILE_SIZE,
            &cache_dir,
        );

        assert!(
            index.build_denied,
            "a write-denied cold build must flag build_denied so health can report a settled state"
        );
        assert!(
            !index.ready,
            "build_denied must keep ready=false so grep/glob keep using the bounded fallback walk"
        );
        assert!(
            index.files.is_empty(),
            "a write-denied build must not materialize an in-RAM index"
        );
    }
}

#[cfg(test)]
mod interactive_artifact_read_budget_tests {
    use super::*;
    use std::sync::Arc;
    use std::thread;

    #[test]
    fn contended_read_returns_within_interactive_budget() {
        let lock = Arc::new(RwLock::new(()));
        let writer = lock.write().expect("acquire test writer");
        let reader_lock = Arc::clone(&lock);
        let (result_tx, result_rx) = std::sync::mpsc::channel();
        let reader = thread::spawn(move || {
            let started = Instant::now();
            let result = try_read_with_budget(&reader_lock, Duration::from_millis(20));
            result_tx
                .send((result.is_some(), started.elapsed()))
                .unwrap();
        });

        // Keep the writer live until the reader reports degradation. A delayed
        // reader must still encounter contention, not acquire a released lock.
        let reported = result_rx.recv_timeout(Duration::from_secs(30));
        drop(writer);
        reader.join().expect("join bounded reader");
        let (acquired, elapsed) =
            reported.expect("bounded reader must return while the writer is held");
        assert!(!acquired, "a live writer must force bounded degradation");
        // The read itself must honour its 20 ms budget. The bound is 100 times
        // the budget so runner scheduling noise cannot trip it, while a read
        // that ignored the budget and waited on the writer (held up to 30 s)
        // still fails.
        assert!(
            elapsed < Duration::from_secs(2),
            "contended read ignored its 20 ms budget: {elapsed:?}"
        );
    }
}

/// The search index, its glob, and the project walker it shares with the
/// semantic collector and views membership apply `.gitignore` files the same
/// way in a plain folder as in a git repository.
#[cfg(test)]
mod project_ignore_rule_tests {
    use super::*;
    use crate::context::ignore_rules_fixture as fixture;
    use std::collections::BTreeSet;

    fn fixture_root(git: bool) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(dir.path()).unwrap();
        fixture::write(&root, git);
        (dir, root)
    }

    fn indexed_needle_files(root: &Path) -> BTreeSet<String> {
        let index = SearchIndex::build(root);
        let result = index.grep(fixture::NEEDLE, true, &[], &[], root, 1000);
        let files = result
            .matches
            .iter()
            .map(|found| found.file.clone())
            .collect::<Vec<_>>();
        fixture::relative_set(root, &files)
    }

    fn indexed_glob_files(root: &Path) -> BTreeSet<String> {
        let index = SearchIndex::build(root);
        fixture::relative_set(root, &index.glob("**/*", root))
    }

    fn walked_files(root: &Path) -> BTreeSet<String> {
        fixture::relative_set(root, &walk_project_files(root, &PathFilters::default()))
    }

    #[test]
    fn non_git_root_index_excludes_top_level_and_nested_gitignored_paths() {
        let (_dir, root) = fixture_root(false);

        let grepped = indexed_needle_files(&root);
        fixture::assert_honours_ignore_rules(&grepped, "trigram grep");
        assert_eq!(
            grepped.len(),
            fixture::VISIBLE_SOURCES.len(),
            "grep must find the needle only in the visible sources: {grepped:?}"
        );
        fixture::assert_honours_ignore_rules(&indexed_glob_files(&root), "trigram glob");
        fixture::assert_honours_ignore_rules(&walked_files(&root), "project walk");
    }

    #[test]
    fn non_git_and_git_roots_index_the_identical_file_set() {
        let (_plain_dir, plain) = fixture_root(false);
        let (_git_dir, git) = fixture_root(true);

        assert_eq!(walked_files(&plain), walked_files(&git));
        assert_eq!(indexed_needle_files(&plain), indexed_needle_files(&git));
        assert_eq!(indexed_glob_files(&plain), indexed_glob_files(&git));
    }

    /// The project walker as it was built before every walker shared
    /// `apply_project_ignore_rules`, reproduced independently with the
    /// crate's default `require_git`, so the comparison below is not two
    /// calls through the same implementation.
    fn pre_change_project_walk(root: &Path) -> BTreeSet<String> {
        let mut builder = WalkBuilder::new(root);
        builder
            .same_file_system(true)
            .hidden(false)
            .git_ignore(true)
            .git_global(true)
            .git_exclude(true)
            .add_custom_ignore_filename(".aftignore")
            .filter_entry(|entry| {
                let name = entry.file_name().to_string_lossy();
                if entry.file_type().map_or(false, |ft| ft.is_dir()) {
                    return !matches!(
                        name.as_ref(),
                        "node_modules"
                            | "target"
                            | "venv"
                            | ".venv"
                            | ".git"
                            | "__pycache__"
                            | ".tox"
                            | "dist"
                            | "build"
                    );
                }
                !crate::os_metadata::is_os_metadata_file_name(entry.file_name())
            });
        let files = builder
            .build()
            .filter_map(Result::ok)
            .filter(|entry| entry.file_type().is_some_and(|kind| kind.is_file()))
            .map(|entry| entry.into_path())
            .collect::<Vec<_>>();
        fixture::relative_set(root, &files)
    }

    #[test]
    fn git_root_index_file_set_is_unchanged_from_the_pre_change_walker() {
        let (_dir, root) = fixture_root(true);
        // A nested repository starts its own rule scope in git: the outer
        // `*.log` rule does not reach inside it. A walker that simply turned
        // `require_git` off everywhere would drop this file.
        fs::create_dir_all(root.join("vendor/nested_repo/.git")).unwrap();
        fs::write(root.join("vendor/nested_repo/kept.log"), fixture::NEEDLE).unwrap();
        fs::write(root.join("vendor/nested_repo/.gitignore"), "skipped.rs\n").unwrap();
        fs::write(root.join("vendor/nested_repo/skipped.rs"), fixture::NEEDLE).unwrap();
        fs::create_dir_all(root.join(".git/info")).unwrap();
        fs::write(root.join(".git/info/exclude"), "excluded_by_info.rs\n").unwrap();
        fs::write(root.join("excluded_by_info.rs"), fixture::NEEDLE).unwrap();

        let before = pre_change_project_walk(&root);
        let after = walked_files(&root);
        assert_eq!(after, before, "git roots must walk exactly as before");
        assert!(after.contains("vendor/nested_repo/kept.log"), "{after:?}");
        assert!(
            !after.contains("vendor/nested_repo/skipped.rs"),
            "{after:?}"
        );
        assert!(!after.contains("excluded_by_info.rs"), "{after:?}");
        fixture::assert_honours_ignore_rules(&after, "git project walk");

        assert_eq!(
            indexed_glob_files(&root),
            before,
            "the index must hold the pre-change file set"
        );
    }
}
