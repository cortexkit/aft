//! Persistent call/reference graph sidecar.
//!
//! This SQLite-backed substrate stores raw symbols, references, and resolved
//! edges, and backs the live call-graph commands (callers, call-tree, impact,
//! trace) as well as dead-code reachability. It is self-contained: it can be
//! built and queried directly without going through the in-memory call graph.

use crate::cache_freshness::{self, FileFreshness, FreshnessVerdict};
use crate::callgraph::{self, EdgeResolution, FileCallData, TraceToSymbolCandidate};
use crate::context::SubcLifecycleAdmission;
use crate::error::AftError;
use crate::imports::{ImportForm, ImportGroup, ImportKind, ImportStatement};
use crate::parser::{grammar_for, LangId};
use crate::symbols::{Range, SymbolKind};
use rayon::prelude::*;
use rusqlite::{
    params, params_from_iter, Connection, OpenFlags, OptionalExtension, Statement, Transaction,
};
use std::collections::{hash_map::Entry, BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};
use std::fmt;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering as AtomicOrdering};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tree_sitter::{Node, Parser};

const SCHEMA_VERSION: i64 = 1;
const BACKEND_TREESITTER: &str = "treesitter";
const PROVENANCE_TREESITTER: &str = "treesitter+resolver";
const PROVENANCE_NAME_MATCH: &str = "name_match";
const PROVENANCE_TYPE_MATCH: &str = "type_match";
const NAME_MATCH_SCORE_THRESHOLD: f64 = 2.0;
const TOP_LEVEL_SYMBOL: &str = "<top-level>";
const JS_TS_EXTENSIONS: &[&str] = &["ts", "tsx", "mts", "cts", "js", "jsx", "mjs", "cjs"];
const MIGRATION_MANIFEST_VERSION: u32 = 1;
const MIGRATION_GENERATION_TAG: &str = ".migrated.";
const MIGRATION_BACKUP_PAGES_PER_STEP: i32 = 128;
const MIGRATION_BACKUP_RETRY_BUDGET: usize = 25;
const MIGRATION_BACKUP_WALL_CLOCK_BUDGET: Duration = Duration::from_secs(10);
const SQLITE_FILE_SET_SUFFIXES: &[&str] = &["", "-wal", "-shm", "-journal"];
/// Marker-protected generations older than this absolute age are reclaimed even
/// if a stale reader marker remains. Current and newest-previous generations are
/// always retained, bounding the root-keyed callgraph store to roughly two or
/// three large generations without adding user-visible configuration.
const MARKED_GENERATION_RETENTION_TTL: Duration = Duration::from_secs(6 * 60 * 60);
const REFRESH_WORKER_WARN_AFTER: Duration = Duration::from_secs(5);
const REFRESH_WORKER_FINAL_AFTER: Duration = Duration::from_secs(30);
pub const REFRESH_WORKER_GRACEFUL_SHUTDOWN_BUDGET: Duration = Duration::from_millis(100);
const REBUILD_COOLDOWN: Duration = Duration::from_secs(30);
const ROOT_REPAIR_WARN_INTERVAL: Duration = Duration::from_secs(60);
const CALLGRAPH_WRITE_METRIC_WINDOW: Duration = Duration::from_secs(60);
const CALLGRAPH_WAL_AUTOCHECKPOINT_PAGES: i64 = 4_000;
const REFRESH_IDLE_CHECKPOINT_INTERVAL: Duration = Duration::from_secs(60);

fn write_amplification_baseline_enabled() -> bool {
    std::env::var_os("AFT_CALLGRAPH_WRITE_AMP_BASELINE").is_some()
}

type ColdBuildSwapObserver = dyn Fn(&Path, &Path) + Send + Sync + 'static;

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct RebuildCooldownKey {
    callgraph_dir: PathBuf,
    project_key: String,
}

#[derive(Clone, Debug)]
struct RebuildCooldownRecord {
    project_root: PathBuf,
    published_at: Instant,
    cross_root_cooldown_armed: bool,
}

// Prevent repeated rebuilds when requests rapidly switch between project
// roots. Allow the first successful rebuild for a different root; after that
// transition, report the artifact as unavailable instead of publishing another
// complete generation. Record only successful publications in this map.
static SUCCESSFUL_REBUILDS: OnceLock<Mutex<HashMap<RebuildCooldownKey, RebuildCooldownRecord>>> =
    OnceLock::new();

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct RootRepairWarningKey {
    project_key: String,
}

#[derive(Clone, Debug)]
struct RootRepairWarningRecord {
    window_start: Instant,
    last_emitted: Instant,
    entry_count: u64,
    suppressed: u64,
}

static ROOT_REPAIR_WARNINGS: OnceLock<
    Mutex<HashMap<RootRepairWarningKey, RootRepairWarningRecord>>,
> = OnceLock::new();

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct CallgraphWriteMetricsSnapshot {
    pub commits_60s: u64,
    pub pages_or_bytes_written_60s: u64,
}

#[derive(Debug, Default)]
struct CallgraphWriteMetrics {
    window_start_ms: AtomicU64,
    commits_60s: AtomicU64,
    pages_or_bytes_written_60s: AtomicU64,
}

static CALLGRAPH_WRITE_METRICS: OnceLock<Mutex<HashMap<String, Arc<CallgraphWriteMetrics>>>> =
    OnceLock::new();

fn callgraph_write_metrics_for_key(project_key: &str) -> Arc<CallgraphWriteMetrics> {
    let metrics = CALLGRAPH_WRITE_METRICS.get_or_init(|| Mutex::new(HashMap::new()));
    let mut metrics = metrics
        .lock()
        .expect("callgraph write metrics mutex poisoned");
    Arc::clone(
        metrics
            .entry(project_key.to_string())
            .or_insert_with(|| Arc::new(CallgraphWriteMetrics::default())),
    )
}

fn roll_callgraph_write_metric_window(metrics: &CallgraphWriteMetrics, now_ms: u64) {
    let current_start = metrics.window_start_ms.load(AtomicOrdering::Acquire);
    if current_start == 0 {
        let _ = metrics.window_start_ms.compare_exchange(
            0,
            now_ms,
            AtomicOrdering::AcqRel,
            AtomicOrdering::Acquire,
        );
        return;
    }
    if now_ms.saturating_sub(current_start) < CALLGRAPH_WRITE_METRIC_WINDOW.as_millis() as u64 {
        return;
    }
    if metrics
        .window_start_ms
        .compare_exchange(
            current_start,
            now_ms,
            AtomicOrdering::AcqRel,
            AtomicOrdering::Acquire,
        )
        .is_ok()
    {
        metrics.commits_60s.store(0, AtomicOrdering::Release);
        metrics
            .pages_or_bytes_written_60s
            .store(0, AtomicOrdering::Release);
    }
}

impl CallgraphWriteMetrics {
    fn record_commit(&self, pages_or_bytes_written: u64) {
        let now_ms = unix_millis_now();
        roll_callgraph_write_metric_window(self, now_ms);
        self.commits_60s.fetch_add(1, AtomicOrdering::Relaxed);
        self.pages_or_bytes_written_60s
            .fetch_add(pages_or_bytes_written, AtomicOrdering::Relaxed);
    }

    fn snapshot(&self) -> CallgraphWriteMetricsSnapshot {
        roll_callgraph_write_metric_window(self, unix_millis_now());
        CallgraphWriteMetricsSnapshot {
            commits_60s: self.commits_60s.load(AtomicOrdering::Acquire),
            pages_or_bytes_written_60s: self
                .pages_or_bytes_written_60s
                .load(AtomicOrdering::Acquire),
        }
    }
}

pub(crate) fn callgraph_write_metrics_for_project(
    project_key: &str,
) -> CallgraphWriteMetricsSnapshot {
    callgraph_write_metrics_for_key(project_key).snapshot()
}

pub(crate) fn callgraph_write_metrics_total() -> CallgraphWriteMetricsSnapshot {
    let Some(metrics) = CALLGRAPH_WRITE_METRICS.get() else {
        return CallgraphWriteMetricsSnapshot::default();
    };
    let metrics = metrics
        .lock()
        .expect("callgraph write metrics mutex poisoned");
    metrics.values().map(|metrics| metrics.snapshot()).fold(
        CallgraphWriteMetricsSnapshot::default(),
        |total, current| CallgraphWriteMetricsSnapshot {
            commits_60s: total.commits_60s.saturating_add(current.commits_60s),
            pages_or_bytes_written_60s: total
                .pages_or_bytes_written_60s
                .saturating_add(current.pages_or_bytes_written_60s),
        },
    )
}

const ROOT_REPAIR_WARNING_TEXT: &str =
    "callgraph store root repair requires rebuild; open-only reader reports unavailable";

fn next_root_repair_warning(key: RootRepairWarningKey, now: Instant) -> Option<String> {
    let warnings = ROOT_REPAIR_WARNINGS.get_or_init(|| Mutex::new(HashMap::new()));
    let mut warnings = warnings.lock().ok()?;
    let entry = warnings.entry(key);
    let record = match entry {
        Entry::Vacant(entry) => {
            entry.insert(RootRepairWarningRecord {
                window_start: now,
                last_emitted: now,
                entry_count: 1,
                suppressed: 0,
            });
            return Some(ROOT_REPAIR_WARNING_TEXT.to_string());
        }
        Entry::Occupied(entry) => entry.into_mut(),
    };

    if now.saturating_duration_since(record.window_start) >= ROOT_REPAIR_WARN_INTERVAL {
        let suppressed = record.suppressed;
        record.window_start = now;
        record.last_emitted = now;
        record.entry_count = 1;
        record.suppressed = 0;
        return Some(if suppressed == 0 {
            ROOT_REPAIR_WARNING_TEXT.to_string()
        } else {
            format!("{ROOT_REPAIR_WARNING_TEXT} (repeated {suppressed}x in 60s)")
        });
    }

    record.entry_count = record.entry_count.saturating_add(1);
    if now.saturating_duration_since(record.last_emitted) < ROOT_REPAIR_WARN_INTERVAL {
        record.suppressed = record.suppressed.saturating_add(1);
        None
    } else {
        record.last_emitted = now;
        Some(ROOT_REPAIR_WARNING_TEXT.to_string())
    }
}

pub(crate) fn note_repair_entry(project_key: &str) -> Option<String> {
    next_root_repair_warning(
        RootRepairWarningKey {
            project_key: project_key.to_string(),
        },
        Instant::now(),
    )
}

/// Return the number of repair entries in the active 60-second window.
///
/// The window start is returned for callers that need to show freshness without
/// adding another status verdict or turning this into a user-facing setting.
pub(crate) fn repair_entry_rate(project_key: &str) -> Option<(u64, Instant)> {
    let warnings = ROOT_REPAIR_WARNINGS.get_or_init(|| Mutex::new(HashMap::new()));
    let warnings = warnings.lock().ok()?;
    let record = warnings.get(&RootRepairWarningKey {
        project_key: project_key.to_string(),
    })?;
    (Instant::now().saturating_duration_since(record.window_start) < ROOT_REPAIR_WARN_INTERVAL)
        .then_some((record.entry_count, record.window_start))
}

pub(crate) fn repair_entry_rate_total() -> u64 {
    let Ok(warnings) = ROOT_REPAIR_WARNINGS
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
    else {
        return 0;
    };
    let now = Instant::now();
    warnings
        .values()
        .filter(|record| {
            now.saturating_duration_since(record.window_start) < ROOT_REPAIR_WARN_INTERVAL
        })
        .map(|record| record.entry_count)
        .sum()
}

#[cfg(test)]
pub(crate) fn expire_repair_entry_window_for_test(project_key: &str) {
    let warnings = ROOT_REPAIR_WARNINGS.get_or_init(|| Mutex::new(HashMap::new()));
    let mut warnings = warnings.lock().unwrap();
    if let Some(record) = warnings.get_mut(&RootRepairWarningKey {
        project_key: project_key.to_string(),
    }) {
        record.window_start = Instant::now() - ROOT_REPAIR_WARN_INTERVAL;
    }
}

#[cfg(test)]
mod root_repair_warning_tests {
    use super::*;

    #[test]
    fn repair_warning_emits_once_then_reemits_with_suppressed_count() {
        let key = RootRepairWarningKey {
            project_key: "test-project".to_string(),
        };
        let first_at = Instant::now();
        let first = next_root_repair_warning(key.clone(), first_at).unwrap();
        assert_eq!(first, ROOT_REPAIR_WARNING_TEXT);
        assert!(next_root_repair_warning(key.clone(), first_at + Duration::from_secs(1)).is_none());
        assert_eq!(
            repair_entry_rate("test-project").map(|rate| rate.0),
            Some(2)
        );

        let repeated = next_root_repair_warning(key, first_at + ROOT_REPAIR_WARN_INTERVAL).unwrap();
        assert!(repeated.ends_with("(repeated 1x in 60s)"));
        expire_repair_entry_window_for_test("test-project");
        assert!(repair_entry_rate("test-project").is_none());
    }
}

#[cfg(test)]
mod write_amplification_tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    #[test]
    fn callgraph_writer_and_reader_use_bounded_normal_pragmas() {
        let temp = tempdir().unwrap();
        let root = temp.path().join("root");
        fs::create_dir_all(&root).unwrap();
        let source = root.join("main.ts");
        fs::write(&source, "export function main() {}\n").unwrap();
        let store_dir = temp.path().join("store");
        let store = CallGraphStore::open(store_dir.clone(), root.clone()).unwrap();

        let conn = store.conn.lock().unwrap();
        let synchronous: i64 = conn
            .pragma_query_value(None, "synchronous", |row| row.get(0))
            .unwrap();
        let autocheckpoint: i64 = conn
            .pragma_query_value(None, "wal_autocheckpoint", |row| row.get(0))
            .unwrap();
        assert_eq!(synchronous, 1, "NORMAL synchronous mode is value 1");
        assert_eq!(autocheckpoint, CALLGRAPH_WAL_AUTOCHECKPOINT_PAGES);
        drop(conn);
        store.cold_build(std::slice::from_ref(&source)).unwrap();
        drop(store);

        let readonly = CallGraphStore::open_readonly(store_dir, root)
            .unwrap()
            .expect("writer-created empty schema should be readable");
        let conn = readonly.inner.conn.lock().unwrap();
        let synchronous: i64 = conn
            .pragma_query_value(None, "synchronous", |row| row.get(0))
            .unwrap();
        assert_eq!(synchronous, 1);
    }

    #[test]
    fn own_refresh_skips_identical_extract_but_not_position_shift() {
        let temp = tempdir().unwrap();
        let root = temp.path().join("root");
        fs::create_dir_all(&root).unwrap();
        let source = root.join("main.ts");
        fs::write(&source, "export function main() { return 1; }\n").unwrap();
        let store = CallGraphStore::open(temp.path().join("store"), root.clone()).unwrap();
        store.cold_build(std::slice::from_ref(&source)).unwrap();
        let write_metrics = callgraph_write_metrics_for_project(store.project_key());
        assert!(write_metrics.commits_60s > 0);
        assert!(write_metrics.pages_or_bytes_written_60s > 0);

        let before = store.conn.lock().unwrap().total_changes();
        fs::write(&source, "export function main() { return 1; }\n\n").unwrap();
        let (stats, _) = store
            .refresh_files_profiled(std::slice::from_ref(&source))
            .unwrap();
        let after = store.conn.lock().unwrap().total_changes();
        assert_eq!(stats.unchanged_extract_files, 1);
        assert_eq!(stats.refreshed_own_files, 0);
        assert_eq!(
            after - before,
            2,
            "only files and backend freshness rows update"
        );

        fs::write(&source, "\nexport function main() { return 1; }\n\n").unwrap();
        let (shifted_stats, _) = store
            .refresh_files_profiled(std::slice::from_ref(&source))
            .unwrap();
        assert_eq!(shifted_stats.unchanged_extract_files, 0);
        assert_eq!(shifted_stats.refreshed_own_files, 1);
    }

    #[test]
    fn idle_checkpoint_interval_prevents_checkpoint_storms() {
        let now = Instant::now();
        assert!(idle_checkpoint_due(None, now));
        assert!(!idle_checkpoint_due(
            Some(now),
            now + Duration::from_secs(REFRESH_IDLE_CHECKPOINT_INTERVAL.as_secs() - 1),
        ));
        assert!(idle_checkpoint_due(
            Some(now),
            now + REFRESH_IDLE_CHECKPOINT_INTERVAL,
        ));
    }

    #[test]
    fn write_metrics_decay_after_the_sixty_second_window() {
        let key = format!("metrics-test-{}", now_nanos());
        let metrics = callgraph_write_metrics_for_key(&key);
        metrics.record_commit(17);
        assert_eq!(metrics.snapshot().commits_60s, 1);
        assert_eq!(metrics.snapshot().pages_or_bytes_written_60s, 17);
        metrics.window_start_ms.store(
            unix_millis_now().saturating_sub(CALLGRAPH_WRITE_METRIC_WINDOW.as_millis() as u64),
            AtomicOrdering::Release,
        );
        assert_eq!(metrics.snapshot(), CallgraphWriteMetricsSnapshot::default());
    }
}

#[cfg(test)]
type ColdBuildBeforePublishObserver = dyn Fn() + Send + Sync + 'static;
// THREAD-LOCAL, not a process-global: the observer fires synchronously on the
// thread running the cold build, and the only caller (a test) installs and
// clears it on its own thread. A process-global `Mutex<Option<...>>` raced
// across parallel tests — one test's installed observer fired during ANOTHER
// test's `cold_build_with_lease`, asserting against the wrong build's edges
// (flaked on Windows CI under parallel scheduling). Production never sets it.
thread_local! {
    static COLD_BUILD_SWAP_OBSERVER: std::cell::RefCell<Option<Arc<ColdBuildSwapObserver>>> =
        const { std::cell::RefCell::new(None) };
    #[cfg(test)]
    static COLD_BUILD_BEFORE_PUBLISH_OBSERVER: std::cell::RefCell<Option<Arc<ColdBuildBeforePublishObserver>>> =
        const { std::cell::RefCell::new(None) };
    static MIGRATION_AVAILABLE_DISK_OVERRIDE: std::cell::RefCell<Option<u64>> =
        const { std::cell::RefCell::new(None) };
    static MIGRATION_FAIL_AFTER_TEMP_COPY: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    static MIGRATION_FORCE_BACKUP_BUDGET_EXHAUSTED: std::cell::Cell<bool> =
        const { std::cell::Cell::new(false) };
    static PUBLISH_ADMISSION: std::cell::RefCell<Option<(crate::root_cache::ArtifactPublishEpoch, u64)>> =
        const { std::cell::RefCell::new(None) };
    static REFRESH_COMMIT_ADMISSION: std::cell::RefCell<Option<(SubcLifecycleAdmission, Arc<std::sync::atomic::AtomicU64>, u64)>> =
        const { std::cell::RefCell::new(None) };
}

mod dead_code_projection;
pub use dead_code_projection::project_dead_code_snapshot;
#[cfg(test)]
pub(crate) use dead_code_projection::set_projection_before_open_observer;

#[doc(hidden)]
pub fn set_cold_build_swap_observer(observer: Option<Arc<ColdBuildSwapObserver>>) {
    COLD_BUILD_SWAP_OBSERVER.with(|slot| *slot.borrow_mut() = observer);
}

#[cfg(test)]
fn set_cold_build_before_publish_observer(observer: Option<Arc<ColdBuildBeforePublishObserver>>) {
    COLD_BUILD_BEFORE_PUBLISH_OBSERVER.with(|slot| *slot.borrow_mut() = observer);
}

#[cfg(test)]
fn notify_cold_build_before_publish_observer() {
    let observer = COLD_BUILD_BEFORE_PUBLISH_OBSERVER.with(|slot| slot.borrow().clone());
    if let Some(observer) = observer {
        observer();
    }
}

#[cfg(not(test))]
fn notify_cold_build_before_publish_observer() {}

#[doc(hidden)]
pub fn set_legacy_migration_available_disk_for_test(bytes: Option<u64>) {
    MIGRATION_AVAILABLE_DISK_OVERRIDE.with(|slot| *slot.borrow_mut() = bytes);
}

#[doc(hidden)]
pub fn set_legacy_migration_fail_after_temp_copy_for_test(enabled: bool) {
    MIGRATION_FAIL_AFTER_TEMP_COPY.with(|slot| slot.set(enabled));
}

#[doc(hidden)]
pub fn set_legacy_migration_backup_budget_exhausted_for_test(enabled: bool) {
    MIGRATION_FORCE_BACKUP_BUDGET_EXHAUSTED.with(|slot| slot.set(enabled));
}

struct PublishAdmissionGuard {
    previous: Option<(crate::root_cache::ArtifactPublishEpoch, u64)>,
}

impl Drop for PublishAdmissionGuard {
    fn drop(&mut self) {
        PUBLISH_ADMISSION.with(|slot| {
            *slot.borrow_mut() = self.previous.take();
        });
    }
}

pub(crate) fn with_publish_epoch<R>(
    epoch: crate::root_cache::ArtifactPublishEpoch,
    expected: u64,
    run: impl FnOnce() -> R,
) -> R {
    let previous = PUBLISH_ADMISSION.with(|slot| slot.replace(Some((epoch, expected))));
    let _guard = PublishAdmissionGuard { previous };
    run()
}

fn publish_if_current<R>(publish: impl FnOnce() -> Result<R>) -> Result<R> {
    let admission = PUBLISH_ADMISSION.with(|slot| slot.borrow().clone());
    match admission {
        Some((epoch, expected)) => epoch
            .run_if_current(expected, publish)
            .unwrap_or(Err(CallGraphStoreError::Superseded)),
        None => publish(),
    }
}

struct RefreshCommitAdmissionGuard {
    previous: Option<(
        SubcLifecycleAdmission,
        Arc<std::sync::atomic::AtomicU64>,
        u64,
    )>,
}

impl Drop for RefreshCommitAdmissionGuard {
    fn drop(&mut self) {
        REFRESH_COMMIT_ADMISSION.with(|slot| {
            *slot.borrow_mut() = self.previous.take();
        });
    }
}

fn with_refresh_commit_admission<R>(
    lifecycle: SubcLifecycleAdmission,
    generation_flag: Arc<std::sync::atomic::AtomicU64>,
    expected_generation: u64,
    run: impl FnOnce() -> R,
) -> R {
    let previous = REFRESH_COMMIT_ADMISSION
        .with(|slot| slot.replace(Some((lifecycle, generation_flag, expected_generation))));
    let _guard = RefreshCommitAdmissionGuard { previous };
    run()
}

fn commit_incremental_if_current(tx: Transaction<'_>) -> Result<()> {
    let admission = REFRESH_COMMIT_ADMISSION.with(|slot| slot.borrow().clone());
    let commit = || {
        publish_if_current(|| {
            tx.commit()?;
            Ok(())
        })
    };
    match admission {
        Some((lifecycle, generation_flag, expected_generation)) => lifecycle
            .run_if_current(generation_flag.as_ref(), expected_generation, commit)
            .unwrap_or(Err(CallGraphStoreError::Superseded)),
        None => commit(),
    }
}

fn notify_cold_build_swap_observer(temp_path: &Path, target_path: &Path) {
    let observer = COLD_BUILD_SWAP_OBSERVER.with(|slot| slot.borrow().clone());
    if let Some(observer) = observer {
        observer(temp_path, target_path);
    }
}

#[derive(Debug)]
pub enum CallGraphStoreError {
    Io(std::io::Error),
    Sqlite(rusqlite::Error),
    Json(serde_json::Error),
    Aft(AftError),
    Lock(crate::fs_lock::AcquireError),
    MissingCallerData { file: String },
    Unavailable(String),
    Superseded,
    StaleFiles(Vec<String>),
}

impl fmt::Display for CallGraphStoreError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(formatter, "I/O error: {error}"),
            Self::Sqlite(error) => write!(formatter, "sqlite error: {error}"),
            Self::Json(error) => write!(formatter, "json error: {error}"),
            Self::Aft(error) => write!(formatter, "callgraph extraction error: {error}"),
            Self::Lock(error) => write!(formatter, "callgraph writer lease error: {error}"),
            Self::MissingCallerData { file } => {
                write!(formatter, "missing extracted caller data for {file}")
            }
            Self::Unavailable(message) => {
                write!(formatter, "callgraph store unavailable: {message}")
            }
            Self::Superseded => {
                write!(formatter, "callgraph store build superseded before publish")
            }
            Self::StaleFiles(files) => {
                write!(
                    formatter,
                    "callgraph store has stale files: {}",
                    files.join(", ")
                )
            }
        }
    }
}

impl std::error::Error for CallGraphStoreError {}

impl From<std::io::Error> for CallGraphStoreError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<rusqlite::Error> for CallGraphStoreError {
    fn from(error: rusqlite::Error) -> Self {
        Self::Sqlite(error)
    }
}

impl From<serde_json::Error> for CallGraphStoreError {
    fn from(error: serde_json::Error) -> Self {
        Self::Json(error)
    }
}

impl From<AftError> for CallGraphStoreError {
    fn from(error: AftError) -> Self {
        Self::Aft(error)
    }
}

impl From<crate::fs_lock::AcquireError> for CallGraphStoreError {
    fn from(error: crate::fs_lock::AcquireError) -> Self {
        Self::Lock(error)
    }
}

pub type Result<T> = std::result::Result<T, CallGraphStoreError>;

/// Config flag name gating whether the store is opened (default on). Production
/// commands open it through `open_if_enabled` so the substrate can be disabled
/// without code changes.
pub const CALLGRAPH_STORE_FLAG: &str = "callgraph_store";

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CallGraphStoreOptions {
    pub enabled: bool,
}

pub type PendingCallGraphStorePaths = Arc<parking_lot::Mutex<BTreeSet<PathBuf>>>;

/// Shared context state that lets the refresh worker observe a store installed
/// after its batch was opened. The worker clones the installed store Arc before
/// checking it, so no context lock guard crosses the check or enqueue call.
#[derive(Clone)]
pub(crate) struct CallgraphRefreshState {
    store: Arc<std::sync::RwLock<Option<Arc<ReadonlyCallGraphStore>>>>,
    heavy_root_work_allowed: Arc<AtomicBool>,
}

impl CallgraphRefreshState {
    pub(crate) fn new(
        store: Arc<std::sync::RwLock<Option<Arc<ReadonlyCallGraphStore>>>>,
        heavy_root_work_allowed: Arc<AtomicBool>,
    ) -> Self {
        Self {
            store,
            heavy_root_work_allowed,
        }
    }

    fn installed_store_snapshot(&self) -> Option<Arc<ReadonlyCallGraphStore>> {
        self.store
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
            .map(Arc::clone)
    }
}

type WorkspaceCratePrefixes = HashMap<String, String>;

#[derive(Clone, Debug, Default)]
struct WorkspaceCratePrefixCache(Arc<OnceLock<WorkspaceCratePrefixes>>);

const REFRESH_WORKSPACE_CACHE_ROOT_CAP: usize = 128;

pub(crate) fn invalidates_workspace_crate_prefix_cache(path: &Path) -> bool {
    path.file_name().and_then(|name| name.to_str()) == Some("Cargo.toml")
}

#[derive(Clone, Debug, Hash, PartialEq, Eq)]
struct RefreshRoot {
    callgraph_dir: PathBuf,
    project_root: PathBuf,
}

#[derive(Clone)]
pub(crate) struct CallgraphRefreshTicket {
    lifecycle: SubcLifecycleAdmission,
    generation_flag: Arc<std::sync::atomic::AtomicU64>,
    expected_generation: u64,
    publish_epoch: crate::root_cache::ArtifactPublishEpoch,
    expected_publish_epoch: u64,
}

impl CallgraphRefreshTicket {
    pub(crate) fn new(
        lifecycle: SubcLifecycleAdmission,
        generation_flag: Arc<std::sync::atomic::AtomicU64>,
        expected_generation: u64,
        publish_epoch: crate::root_cache::ArtifactPublishEpoch,
        expected_publish_epoch: u64,
    ) -> Self {
        Self {
            lifecycle,
            generation_flag,
            expected_generation,
            publish_epoch,
            expected_publish_epoch,
        }
    }

    fn is_current(&self) -> bool {
        self.lifecycle
            .is_current(self.generation_flag.as_ref(), self.expected_generation)
            && self.publish_epoch.current() == self.expected_publish_epoch
    }
}

#[derive(Clone)]
struct RefreshBatch {
    root: RefreshRoot,
    paths: BTreeSet<PathBuf>,
    pending_sinks: Vec<PendingCallGraphStorePaths>,
    refresh_states: Vec<CallgraphRefreshState>,
    ticket: Option<CallgraphRefreshTicket>,
}

impl RefreshBatch {
    fn defer(&self) {
        for sink in &self.pending_sinks {
            sink.lock().extend(self.paths.iter().cloned());
        }
    }

    fn defer_after_open_failure(&self) {
        self.defer();
        if self
            .ticket
            .as_ref()
            .is_some_and(|ticket| !ticket.is_current())
            || !self
                .refresh_states
                .iter()
                .any(|state| state.heavy_root_work_allowed.load(AtomicOrdering::SeqCst))
        {
            return;
        }

        let ready_store_installed = self.refresh_states.iter().any(|state| {
            let store = state.installed_store_snapshot();
            store.is_some_and(|store| {
                store.project_root() == self.root.project_root
                    && !store.is_legacy_fallback()
                    && store.is_current()
            })
        });
        if !ready_store_installed {
            return;
        }

        // This re-check and the ready-store install's pending-sink take form a
        // check-then-act handoff: after this defer, exactly one site observes
        // the parked paths with a ready current store, so no polling is needed.
        for sink in &self.pending_sinks {
            let paths = {
                let mut pending = sink.lock();
                self.paths
                    .iter()
                    .filter(|path| pending.remove(*path))
                    .cloned()
                    .collect::<Vec<_>>()
            };
            if paths.is_empty() {
                continue;
            }
            let _ = enqueue_callgraph_store_refresh_inner(
                self.root.callgraph_dir.clone(),
                self.root.project_root.clone(),
                paths,
                Arc::clone(sink),
                self.refresh_states.clone(),
                self.ticket.clone(),
            );
        }
    }

    fn merge(
        &mut self,
        paths: impl IntoIterator<Item = PathBuf>,
        sink: PendingCallGraphStorePaths,
        refresh_states: Vec<CallgraphRefreshState>,
        ticket: Option<CallgraphRefreshTicket>,
    ) {
        self.paths.extend(paths);
        if ticket.is_some() {
            self.ticket = ticket;
        }
        if !self
            .pending_sinks
            .iter()
            .any(|existing| Arc::ptr_eq(existing, &sink))
        {
            self.pending_sinks.push(sink);
        }
        for refresh_state in refresh_states {
            if !self.refresh_states.iter().any(|existing| {
                Arc::ptr_eq(&existing.store, &refresh_state.store)
                    && Arc::ptr_eq(
                        &existing.heavy_root_work_allowed,
                        &refresh_state.heavy_root_work_allowed,
                    )
            }) {
                self.refresh_states.push(refresh_state);
            }
        }
    }
}

#[derive(Default)]
struct RefreshQueue {
    order: VecDeque<RefreshRoot>,
    queued: HashMap<RefreshRoot, RefreshBatch>,
    active: Option<RefreshBatch>,
    shutdown_requested: bool,
}

struct RefreshWorkerShared {
    queue: Mutex<RefreshQueue>,
    wake: Condvar,
}

struct RefreshWorker {
    shared: Arc<RefreshWorkerShared>,
    thread: Mutex<Option<JoinHandle<()>>>,
}

struct RefreshWorkerWatchdog {
    first_path: PathBuf,
    batch_len: usize,
    started: Instant,
}

impl RefreshWorkerWatchdog {
    fn start(paths: &[PathBuf]) -> Self {
        Self {
            first_path: paths
                .first()
                .expect("non-empty callgraph refresh batch has a first path")
                .clone(),
            batch_len: paths.len(),
            started: Instant::now(),
        }
    }
}

impl Drop for RefreshWorkerWatchdog {
    fn drop(&mut self) {
        let elapsed = self.started.elapsed();
        if elapsed < REFRESH_WORKER_WARN_AFTER {
            return;
        }
        let path = if self.batch_len == 1 {
            self.first_path.display().to_string()
        } else {
            format!(
                "{} (+{} paths)",
                self.first_path.display(),
                self.batch_len - 1
            )
        };
        log::warn!(
            "watcher drain unit exceeded 5s: phase=callgraph path={} elapsed={}ms",
            path,
            elapsed.as_millis()
        );
        if elapsed >= REFRESH_WORKER_FINAL_AFTER {
            log::warn!(
                "watcher drain unit completed after 30s: phase=callgraph path={} elapsed={}ms",
                path,
                elapsed.as_millis()
            );
        }
    }
}

impl RefreshWorker {
    fn spawn() -> Arc<Self> {
        let shared = Arc::new(RefreshWorkerShared {
            queue: Mutex::new(RefreshQueue::default()),
            wake: Condvar::new(),
        });
        let thread_shared = Arc::clone(&shared);
        let thread = std::thread::Builder::new()
            .name("aft-callgraph-refresh".to_string())
            .spawn(move || callgraph_refresh_worker_loop(&thread_shared))
            .expect("failed to spawn callgraph refresh worker");
        Arc::new(Self {
            shared,
            thread: Mutex::new(Some(thread)),
        })
    }

    fn enqueue(
        &self,
        root: RefreshRoot,
        paths: Vec<PathBuf>,
        pending_sink: PendingCallGraphStorePaths,
        refresh_states: Vec<CallgraphRefreshState>,
        ticket: Option<CallgraphRefreshTicket>,
    ) -> bool {
        let mut queue = self
            .shared
            .queue
            .lock()
            .expect("callgraph refresh queue mutex poisoned");
        if queue.shutdown_requested {
            pending_sink.lock().extend(paths);
            return false;
        }
        if let Some(batch) = queue.queued.get_mut(&root) {
            batch.merge(paths, pending_sink, refresh_states, ticket);
        } else {
            queue.order.push_back(root.clone());
            queue.queued.insert(
                root.clone(),
                RefreshBatch {
                    root,
                    paths: paths.into_iter().collect(),
                    pending_sinks: vec![pending_sink],
                    refresh_states,
                    ticket,
                },
            );
        }
        self.shared.wake.notify_one();
        true
    }

    fn shutdown_with_budget(&self, budget: Duration) -> bool {
        let deadline = Instant::now() + budget;
        let mut queue = self
            .shared
            .queue
            .lock()
            .expect("callgraph refresh queue mutex poisoned");
        queue.shutdown_requested = true;
        self.shared.wake.notify_one();
        while (queue.active.is_some() || !queue.order.is_empty()) && Instant::now() < deadline {
            let remaining = deadline.saturating_duration_since(Instant::now());
            let (next, _) = self
                .shared
                .wake
                .wait_timeout(queue, remaining)
                .expect("callgraph refresh queue mutex poisoned while waiting for shutdown");
            queue = next;
        }
        let drained = queue.active.is_none() && queue.order.is_empty();
        if !drained {
            if let Some(active) = queue.active.as_ref() {
                active.defer();
            }
            for batch in queue.queued.values() {
                batch.defer();
            }
            queue.order.clear();
            queue.queued.clear();
        }
        drop(queue);

        if drained {
            if let Some(thread) = self
                .thread
                .lock()
                .expect("callgraph refresh worker thread mutex poisoned")
                .take()
            {
                let _ = thread.join();
            }
        }
        drained
    }
}

static CALLGRAPH_REFRESH_WORKER: OnceLock<Mutex<Option<Arc<RefreshWorker>>>> = OnceLock::new();

pub fn enqueue_callgraph_store_refresh(
    callgraph_dir: PathBuf,
    project_root: PathBuf,
    paths: Vec<PathBuf>,
    pending_sink: PendingCallGraphStorePaths,
) -> bool {
    enqueue_callgraph_store_refresh_inner(
        callgraph_dir,
        project_root,
        paths,
        pending_sink,
        Vec::new(),
        None,
    )
}

#[cfg(test)]
pub(crate) fn enqueue_callgraph_store_refresh_fenced(
    callgraph_dir: PathBuf,
    project_root: PathBuf,
    paths: Vec<PathBuf>,
    pending_sink: PendingCallGraphStorePaths,
    ticket: CallgraphRefreshTicket,
) -> bool {
    enqueue_callgraph_store_refresh_inner(
        callgraph_dir,
        project_root,
        paths,
        pending_sink,
        Vec::new(),
        Some(ticket),
    )
}

pub(crate) fn enqueue_callgraph_store_refresh_fenced_with_state(
    callgraph_dir: PathBuf,
    project_root: PathBuf,
    paths: Vec<PathBuf>,
    pending_sink: PendingCallGraphStorePaths,
    refresh_state: CallgraphRefreshState,
    ticket: CallgraphRefreshTicket,
) -> bool {
    enqueue_callgraph_store_refresh_inner(
        callgraph_dir,
        project_root,
        paths,
        pending_sink,
        vec![refresh_state],
        Some(ticket),
    )
}

fn enqueue_callgraph_store_refresh_inner(
    callgraph_dir: PathBuf,
    project_root: PathBuf,
    paths: Vec<PathBuf>,
    pending_sink: PendingCallGraphStorePaths,
    refresh_states: Vec<CallgraphRefreshState>,
    ticket: Option<CallgraphRefreshTicket>,
) -> bool {
    if paths.is_empty() {
        return true;
    }
    let slot = CALLGRAPH_REFRESH_WORKER.get_or_init(|| Mutex::new(None));
    let worker = {
        let mut worker = slot
            .lock()
            .expect("callgraph refresh worker mutex poisoned");
        Arc::clone(worker.get_or_insert_with(RefreshWorker::spawn))
    };
    worker.enqueue(
        RefreshRoot {
            callgraph_dir,
            project_root,
        },
        paths,
        pending_sink,
        refresh_states,
        ticket,
    )
}

pub fn flush_callgraph_store_refreshes_on_graceful_shutdown() -> bool {
    flush_callgraph_store_refreshes_with_budget(REFRESH_WORKER_GRACEFUL_SHUTDOWN_BUDGET)
}

#[doc(hidden)]
pub fn flush_callgraph_store_refreshes_with_budget(budget: Duration) -> bool {
    let slot = CALLGRAPH_REFRESH_WORKER.get_or_init(|| Mutex::new(None));
    let worker = slot
        .lock()
        .expect("callgraph refresh worker mutex poisoned")
        .clone();
    let Some(worker) = worker else {
        return true;
    };
    let drained = worker.shutdown_with_budget(budget);
    if drained {
        let mut current = slot
            .lock()
            .expect("callgraph refresh worker mutex poisoned");
        if current
            .as_ref()
            .is_some_and(|candidate| Arc::ptr_eq(candidate, &worker))
        {
            *current = None;
        }
    }
    drained
}

fn idle_checkpoint_due(last: Option<Instant>, now: Instant) -> bool {
    last.is_none_or(|last| now.saturating_duration_since(last) >= REFRESH_IDLE_CHECKPOINT_INTERVAL)
}

fn callgraph_refresh_worker_loop(shared: &RefreshWorkerShared) {
    // The worker owns these caches so maps are shared only by refreshes for the
    // same canonical root and disappear when the worker shuts down.
    let mut workspace_crate_prefixes = HashMap::new();
    let mut last_idle_checkpoints: HashMap<RefreshRoot, Instant> = HashMap::new();
    loop {
        let batch = {
            let mut queue = shared
                .queue
                .lock()
                .expect("callgraph refresh queue mutex poisoned");
            loop {
                if let Some(root) = queue.order.pop_front() {
                    let batch = queue
                        .queued
                        .remove(&root)
                        .expect("queued callgraph refresh root has a batch");
                    queue.active = Some(batch.clone());
                    break batch;
                }
                if queue.shutdown_requested {
                    return;
                }
                queue = shared
                    .wake
                    .wait(queue)
                    .expect("callgraph refresh queue mutex poisoned while waiting");
            }
        };

        let store = process_callgraph_refresh_batch(&batch, &mut workspace_crate_prefixes);

        let mut queue = shared
            .queue
            .lock()
            .expect("callgraph refresh queue mutex poisoned");
        queue.active = None;
        let became_idle = queue.order.is_empty();
        shared.wake.notify_all();
        drop(queue);

        if became_idle {
            let checkpoint_due = idle_checkpoint_due(
                last_idle_checkpoints.get(&batch.root).copied(),
                Instant::now(),
            );
            if checkpoint_due {
                if let Some(store) = store {
                    if store.checkpoint_wal_truncate() {
                        last_idle_checkpoints.insert(batch.root.clone(), Instant::now());
                    }
                }
            }
        }
    }
}

fn process_callgraph_refresh_batch(
    batch: &RefreshBatch,
    workspace_crate_prefixes: &mut HashMap<RefreshRoot, WorkspaceCratePrefixCache>,
) -> Option<CallGraphStore> {
    // A manifest event is an invalidation signal, not a source file to parse.
    // Drop the root's map even for a superseded batch: the filesystem changed,
    // and a later configure must never inherit crate membership from before it.
    if batch
        .paths
        .iter()
        .any(|path| invalidates_workspace_crate_prefix_cache(path))
    {
        workspace_crate_prefixes.remove(&batch.root);
    }

    let paths = batch
        .paths
        .iter()
        .filter(|path| crate::parser::detect_language(path).is_some())
        .cloned()
        .collect::<Vec<_>>();
    if paths.is_empty() {
        return None;
    }
    note_refresh_worker_batch_for_test(&batch.root.project_root);
    if batch
        .ticket
        .as_ref()
        .is_some_and(|ticket| !ticket.is_current())
    {
        // Superseded before starting: park the paths so the next configure's
        // pending replay (or unbind cleanup) decides their fate.
        batch.defer();
        return None;
    }
    let workspace_crate_prefix_cache =
        workspace_crate_prefix_cache_for_root(workspace_crate_prefixes, &batch.root);
    let _watchdog = RefreshWorkerWatchdog::start(&paths);
    let test_seam = refresh_worker_test_seam(&batch.root.project_root);
    note_refresh_worker_call_for_test(&batch.root.project_root);
    let opened = if test_seam.fail_open {
        Ok(None)
    } else {
        CallGraphStore::open_ready(
            batch.root.callgraph_dir.clone(),
            batch.root.project_root.clone(),
        )
    };
    if let Some(gate) = take_refresh_worker_test_gate(&batch.root.project_root) {
        // The gate is deliberately after open_ready so tests can hold a failed
        // open between its result and the defer that parks the batch.
        let _ = gate.held_tx.send(());
        let _ = gate.release_rx.recv_timeout(Duration::from_secs(12));
    }
    let store = match opened {
        Ok(Some(store)) => store,
        Ok(None) => {
            batch.defer_after_open_failure();
            return None;
        }
        Err(error) => {
            batch.defer_after_open_failure();
            crate::slog_warn!(
                "callgraph store writer open failed during refresh; deferred paths: {}",
                error
            );
            return None;
        }
    };
    if !test_seam.delay.is_zero() {
        std::thread::sleep(test_seam.delay);
    }
    if batch
        .ticket
        .as_ref()
        .is_some_and(|ticket| !ticket.is_current())
    {
        // This is a superseded-ticket defer, not an open-failure defer: leave
        // the paths for the replacement configure instead of self-replaying.
        batch.defer();
        return Some(store);
    }
    let refresh_result = if test_seam.fail_refresh {
        Err(CallGraphStoreError::Unavailable(
            "injected refresh worker failure".to_string(),
        ))
    } else if let Some(ticket) = &batch.ticket {
        with_publish_epoch(
            ticket.publish_epoch.clone(),
            ticket.expected_publish_epoch,
            || {
                with_refresh_commit_admission(
                    ticket.lifecycle.clone(),
                    Arc::clone(&ticket.generation_flag),
                    ticket.expected_generation,
                    || {
                        store
                            .refresh_files_with_workspace_crate_prefix_cache(
                                &paths,
                                workspace_crate_prefix_cache.clone(),
                            )
                            .map(|_| ())
                    },
                )
            },
        )
    } else {
        store
            .refresh_files_with_workspace_crate_prefix_cache(
                &paths,
                workspace_crate_prefix_cache.clone(),
            )
            .map(|_| ())
    };
    if matches!(refresh_result, Err(CallGraphStoreError::Superseded)) {
        // The commit lost the fence race: a newer configure or publication
        // owns the store now. Defer instead of stale-marking — the paths were
        // never committed, and the replacement generation re-indexes them.
        batch.defer();
        return Some(store);
    }
    if let Err(error) = refresh_result {
        crate::slog_warn!("callgraph store refresh failed: {}", error);
        match store.mark_files_stale(&paths) {
            Ok(marked) => {
                note_refresh_worker_stale_mark_for_test(&batch.root.project_root);
                crate::slog_warn!(
                    "marked {} callgraph store file(s) stale after refresh failure",
                    marked.len()
                );
            }
            Err(mark_error) => crate::slog_warn!(
                "failed to mark callgraph store files stale after refresh failure: {}",
                mark_error
            ),
        }
    } else {
        crate::logging::note_callgraph_invalidations(paths.len());
    }
    Some(store)
}

fn workspace_crate_prefix_cache_for_root(
    caches: &mut HashMap<RefreshRoot, WorkspaceCratePrefixCache>,
    root: &RefreshRoot,
) -> WorkspaceCratePrefixCache {
    if !caches.contains_key(root) && caches.len() >= REFRESH_WORKSPACE_CACHE_ROOT_CAP {
        // Eviction only costs a future rebuild; it cannot make resolution stale.
        if let Some(evicted) = caches.keys().next().cloned() {
            caches.remove(&evicted);
        }
    }
    caches.entry(root.clone()).or_default().clone()
}

#[derive(Clone, Copy, Default)]
struct RefreshWorkerTestSeam {
    delay: Duration,
    fail_refresh: bool,
    fail_open: bool,
    refresh_calls: usize,
    worker_calls: usize,
    stale_marks: usize,
}

static REFRESH_WORKER_TEST_SEAMS: OnceLock<Mutex<HashMap<PathBuf, RefreshWorkerTestSeam>>> =
    OnceLock::new();

struct RefreshWorkerTestGate {
    held_tx: crossbeam_channel::Sender<()>,
    release_rx: crossbeam_channel::Receiver<()>,
}

static REFRESH_WORKER_TEST_GATES: OnceLock<Mutex<HashMap<PathBuf, RefreshWorkerTestGate>>> =
    OnceLock::new();

#[doc(hidden)]
pub fn install_callgraph_refresh_worker_test_gate(
    project_root: PathBuf,
) -> (
    crossbeam_channel::Receiver<()>,
    crossbeam_channel::Sender<()>,
) {
    let (held_tx, held_rx) = crossbeam_channel::bounded(1);
    let (release_tx, release_rx) = crossbeam_channel::bounded(1);
    REFRESH_WORKER_TEST_GATES
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .expect("callgraph refresh test gate mutex poisoned")
        .insert(
            project_root,
            RefreshWorkerTestGate {
                held_tx,
                release_rx,
            },
        );
    (held_rx, release_tx)
}

fn take_refresh_worker_test_gate(project_root: &Path) -> Option<RefreshWorkerTestGate> {
    REFRESH_WORKER_TEST_GATES
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .expect("callgraph refresh test gate mutex poisoned")
        .remove(project_root)
}

fn refresh_worker_test_seam(project_root: &Path) -> RefreshWorkerTestSeam {
    let Some(seams) = REFRESH_WORKER_TEST_SEAMS.get() else {
        return RefreshWorkerTestSeam::default();
    };
    seams
        .lock()
        .expect("callgraph refresh test seam mutex poisoned")
        .get(project_root)
        .copied()
        .unwrap_or_default()
}

fn note_refresh_worker_batch_for_test(project_root: &Path) {
    if let Some(seams) = REFRESH_WORKER_TEST_SEAMS.get() {
        if let Some(seam) = seams
            .lock()
            .expect("callgraph refresh test seam mutex poisoned")
            .get_mut(project_root)
        {
            seam.worker_calls += 1;
        }
    }
}

fn note_refresh_worker_call_for_test(project_root: &Path) {
    if let Some(seams) = REFRESH_WORKER_TEST_SEAMS.get() {
        if let Some(seam) = seams
            .lock()
            .expect("callgraph refresh test seam mutex poisoned")
            .get_mut(project_root)
        {
            seam.refresh_calls += 1;
        }
    }
}

fn note_refresh_worker_stale_mark_for_test(project_root: &Path) {
    if let Some(seams) = REFRESH_WORKER_TEST_SEAMS.get() {
        if let Some(seam) = seams
            .lock()
            .expect("callgraph refresh test seam mutex poisoned")
            .get_mut(project_root)
        {
            seam.stale_marks += 1;
        }
    }
}

#[doc(hidden)]
pub fn set_callgraph_refresh_worker_test_seam(
    project_root: PathBuf,
    delay: Duration,
    fail_refresh: bool,
) {
    REFRESH_WORKER_TEST_SEAMS
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .expect("callgraph refresh test seam mutex poisoned")
        .insert(
            project_root,
            RefreshWorkerTestSeam {
                delay,
                fail_refresh,
                ..RefreshWorkerTestSeam::default()
            },
        );
}

#[doc(hidden)]
pub fn set_callgraph_refresh_worker_test_open_failure(project_root: PathBuf, enabled: bool) {
    if let Some(seams) = REFRESH_WORKER_TEST_SEAMS.get() {
        if let Some(seam) = seams
            .lock()
            .expect("callgraph refresh test seam mutex poisoned")
            .get_mut(&project_root)
        {
            seam.fail_open = enabled;
        }
    }
}

#[doc(hidden)]
pub fn callgraph_refresh_worker_test_counts(project_root: &Path) -> (usize, usize) {
    let seam = refresh_worker_test_seam(project_root);
    (seam.refresh_calls, seam.stale_marks)
}

#[doc(hidden)]
pub fn callgraph_refresh_worker_test_worker_calls(project_root: &Path) -> usize {
    refresh_worker_test_seam(project_root).worker_calls
}

#[doc(hidden)]
pub fn clear_callgraph_refresh_worker_test_seam(project_root: &Path) {
    if let Some(seams) = REFRESH_WORKER_TEST_SEAMS.get() {
        seams
            .lock()
            .expect("callgraph refresh test seam mutex poisoned")
            .remove(project_root);
    }
}

#[derive(Debug)]
pub struct CallGraphStore {
    project_root: PathBuf,
    project_key: String,
    /// The concrete on-disk DB file this store opened. With the generation
    /// scheme this is `<dir>/<key>.g<...>.sqlite` (resolved via the pointer) or,
    /// for a pre-generation store, the legacy `<dir>/<key>.sqlite`.
    sqlite_path: PathBuf,
    /// Root-keyed directory whose pointer controls this store. For a legacy
    /// fallback this intentionally differs from `sqlite_path.parent()`, so a
    /// newly published root-keyed generation invalidates the fallback reader.
    publication_dir: PathBuf,
    /// True only when the root-keyed read path opened data from a legacy
    /// harness partition. Writer-capable callers use this to schedule migration
    /// without making read-only/worktree callers acquire a writer lease.
    legacy_fallback: bool,
    /// The generation file NAME this store opened (e.g. `<key>.g<nanos>.<pid>.sqlite`),
    /// or `None` when it opened the legacy single-file DB. Used to detect when
    /// another process has published a newer generation so this process can
    /// drop its connection and reopen (see `current_generation`).
    generation: Option<String>,
    writer_lease: Option<Arc<crate::root_cache::WriterLease>>,
    read_marker: Option<crate::root_cache::ReadMarker>,
    // Readiness is monotonic for an open generation: builds only publish `ready=1`.
    // Failed validations are not cached, so a later successful build remains visible.
    database_ready: AtomicBool,
    write_metrics: Arc<CallgraphWriteMetrics>,
    conn: Mutex<Connection>,
}

#[derive(Debug)]
pub struct ReadonlyCallGraphStore {
    inner: CallGraphStore,
}

pub trait CallGraphRead {
    fn project_root(&self) -> &Path;
    fn project_key(&self) -> &str;
    fn sqlite_path(&self) -> &Path;
    fn is_current(&self) -> bool;
    fn edge_snapshot(&self) -> Result<BTreeSet<StoredEdge>>;
    fn indexed_file_count(&self) -> Result<usize>;
    fn node_for(&self, file_rel: &Path, symbol: &str) -> Result<StoreNode>;
    fn nodes_for(&self, file_rel: &Path, symbol: &str) -> Result<Vec<StoreNode>>;
    fn nodes_matching(&self, symbol: &str) -> Result<Vec<StoreNode>>;
    fn direct_callers_of(&self, file_rel: &Path, symbol: &str) -> Result<Vec<StoreCallSite>>;
    fn direct_caller_counts_of(
        &self,
        targets: &[(String, String)],
    ) -> Result<HashMap<(String, String), usize>>;
    fn outgoing_calls_for_symbols(
        &self,
        sources: &[(String, String)],
    ) -> Result<HashMap<(String, String), Vec<StoreCallSite>>>;
    fn callers_of(&self, file_rel: &Path, symbol: &str, depth: usize)
        -> Result<StoreCallersResult>;
    fn impact_of(&self, file_rel: &Path, symbol: &str, depth: usize) -> Result<StoreImpactResult>;
    fn outgoing_calls_of(&self, node: &StoreNode) -> Result<Vec<StoreCallSite>>;
    fn resolved_self_calls_of(&self, node: &StoreNode) -> Result<Vec<StoreCallSite>>;
    fn unresolved_calls_of(&self, node: &StoreNode) -> Result<Vec<StoreUnresolvedCall>>;
    fn call_tree(
        &self,
        file_rel: &Path,
        symbol: &str,
        depth: usize,
    ) -> Result<callgraph::CallTreeNode>;
    fn trace_to(
        &self,
        file_rel: &Path,
        symbol: &str,
        max_depth: usize,
    ) -> Result<callgraph::TraceToResult>;
    fn trace_to_symbol_candidates(&self, to_symbol: &str) -> Result<Vec<TraceToSymbolCandidate>>;
    fn trace_to_symbol(
        &self,
        file_rel: &Path,
        symbol: &str,
        to_symbol: &str,
        to_file: Option<&Path>,
        max_depth: usize,
    ) -> Result<callgraph::TraceToSymbolResult>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum OpenRootRepair {
    None,
    ReRooted,
    NeedsRebuild {
        previous_roots: Vec<String>,
        current_root: String,
        reason: String,
    },
}

struct OpenedStore {
    store: CallGraphStore,
    root_repair: OpenRootRepair,
}

#[derive(Clone, Debug)]
struct LegacyCallgraphPartition {
    harness: String,
    dir: PathBuf,
    key: String,
    bytes: u64,
    freshness: Option<SystemTime>,
}

#[derive(Clone, Debug)]
struct LegacyCallgraphTarget {
    partition: LegacyCallgraphPartition,
    sqlite_path: PathBuf,
    generation: Option<String>,
    source_bytes: u64,
    source_blake3: String,
}

#[derive(Clone, Debug)]
struct SourceFingerprint {
    bytes: u64,
    blake3: String,
}

#[derive(Clone, Debug)]
struct PublishedLegacyMigration {
    generation: String,
    migrated_bytes: u64,
}

#[derive(Debug, Clone)]
pub struct ColdBuildStats {
    pub files: usize,
    pub nodes: usize,
    pub refs: usize,
    pub edges: usize,
    pub failed_files: Vec<String>,
    pub elapsed_ms: u128,
}

#[derive(Debug, Clone)]
pub struct IncrementalStats {
    pub changed_files: Vec<String>,
    pub surface_changed: Vec<String>,
    pub deleted_files: Vec<String>,
    pub dependency_selected_refs: usize,
    pub refreshed_own_files: usize,
    pub unchanged_extract_files: usize,
}

/// Phase timings for the copy-based incremental refresh benchmark.
#[doc(hidden)]
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RefreshFilesProfile {
    pub parse: Duration,
    pub dependency_selection: Duration,
    pub row_deletes: Duration,
    pub row_inserts: Duration,
    pub dependent_parse: Duration,
    pub index_load: Duration,
    pub ref_resolution: Duration,
    pub method_dispatch: Duration,
    pub commit: Duration,
    pub total: Duration,
}

impl RefreshFilesProfile {
    pub fn report(&self) -> String {
        format!(
            "parse={}ms dependency_selection={}ms row_deletes={}ms row_inserts={}ms dependent_parse={}ms index_load={}ms ref_resolution={}ms method_dispatch={}ms commit={}ms total={}ms",
            self.parse.as_millis(),
            self.dependency_selection.as_millis(),
            self.row_deletes.as_millis(),
            self.row_inserts.as_millis(),
            self.dependent_parse.as_millis(),
            self.index_load.as_millis(),
            self.ref_resolution.as_millis(),
            self.method_dispatch.as_millis(),
            self.commit.as_millis(),
            self.total.as_millis(),
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct StoredEdge {
    pub source_file: String,
    pub source_symbol: String,
    pub target_file: String,
    pub target_symbol: String,
    pub kind: String,
    pub line: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoreNode {
    node_id: String,
    pub file: String,
    pub symbol: String,
    pub name: String,
    pub kind: String,
    pub line: u32,
    pub end_line: u32,
    pub signature: Option<String>,
    pub exported: bool,
    pub is_entry_point: bool,
    pub lang: LangId,
}

#[cfg(test)]
impl StoreNode {
    pub(crate) fn for_test(file: &str, symbol: &str, is_entry_point: bool) -> Self {
        Self {
            node_id: format!("{file}:{symbol}"),
            file: file.to_string(),
            symbol: symbol.to_string(),
            name: symbol.to_string(),
            kind: "function".to_string(),
            line: 1,
            end_line: 1,
            signature: None,
            exported: is_entry_point,
            is_entry_point,
            lang: LangId::TypeScript,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoreCallSite {
    pub caller: StoreNode,
    pub target_file: String,
    pub target_symbol: String,
    pub target: Option<StoreNode>,
    pub line: u32,
    pub byte_start: usize,
    pub byte_end: usize,
    pub resolved: bool,
    pub provenance: String,
}

impl StoreCallSite {
    pub fn approximate(&self) -> bool {
        self.provenance == PROVENANCE_NAME_MATCH
    }

    pub fn resolved_by(&self) -> &str {
        &self.provenance
    }

    pub fn supplemental_resolution(&self) -> Option<&str> {
        match self.provenance.as_str() {
            PROVENANCE_NAME_MATCH | PROVENANCE_TYPE_MATCH => Some(self.provenance.as_str()),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoreUnresolvedCall {
    pub caller: StoreNode,
    pub symbol: String,
    pub full_ref: Option<String>,
    pub line: u32,
    pub byte_start: usize,
    pub byte_end: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoreCallersResult {
    pub target: StoreNode,
    pub callers: Vec<StoreCallSite>,
    pub scanned_files: usize,
    pub depth_limited: bool,
    pub truncated: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoreImpactCaller {
    pub site: StoreCallSite,
    pub signature: Option<String>,
    pub is_entry_point: bool,
    pub call_expression: Option<String>,
    pub parameters: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoreImpactResult {
    pub target: StoreNode,
    pub parameters: Vec<String>,
    pub callers: Vec<StoreImpactCaller>,
    pub depth_limited: bool,
    pub truncated: usize,
}

#[derive(Debug, Clone)]
struct ExtractFailure {
    rel_path: String,
    freshness: Option<FileFreshness>,
}

#[derive(Debug, Clone)]
struct BuildExtractsResult {
    extracts: Vec<FileExtract>,
    failures: Vec<ExtractFailure>,
}

#[derive(Debug, Clone)]
enum StoreForwardCall {
    Resolved(StoreCallSite),
    Unresolved(StoreUnresolvedCall),
}

impl StoreForwardCall {
    fn byte_start(&self) -> usize {
        match self {
            Self::Resolved(site) => site.byte_start,
            Self::Unresolved(call) => call.byte_start,
        }
    }

    fn line(&self) -> u32 {
        match self {
            Self::Resolved(site) => site.line,
            Self::Unresolved(call) => call.line,
        }
    }
}

#[derive(Debug, Clone)]
struct FileExtract {
    rel_path: String,
    freshness: FileFreshness,
    lang: LangId,
    data: FileCallData,
    nodes: Vec<NodeRecord>,
    raw_refs: Vec<RawRef>,
    dispatch_hints: Vec<DispatchHint>,
    surface_fingerprint: String,
}

#[derive(Debug, Clone)]
struct NodeRecord {
    id: String,
    file_path: String,
    name: String,
    scoped_name: String,
    kind: String,
    range: Range,
    range_ordinal: u32,
    signature: Option<String>,
    exported: bool,
    is_default_export: bool,
    is_type_like: bool,
    is_callgraph_entry_point: bool,
}

#[derive(Debug, Clone)]
struct RawRef {
    ref_id: String,
    caller_node: Option<String>,
    caller_symbol: Option<String>,
    caller_file: String,
    kind: String,
    short_name: Option<String>,
    full_ref: Option<String>,
    module_path: Option<String>,
    import_kind: Option<String>,
    local_name: Option<String>,
    requested_name: Option<String>,
    namespace_alias: Option<String>,
    wildcard: bool,
    line: u32,
    byte_start: usize,
    byte_end: usize,
    dependencies: BTreeSet<String>,
}

#[derive(Debug, Clone)]
struct ResolvedRef {
    raw: RawRef,
    status: String,
    target_node: Option<String>,
    target_file: Option<String>,
    target_symbol: Option<String>,
    dependencies: BTreeSet<String>,
    edge: Option<EdgeRecord>,
}

#[derive(Debug, Clone)]
struct EdgeRecord {
    edge_id: String,
    source_node: String,
    target_node: Option<String>,
    target_file: String,
    target_symbol: String,
    kind: String,
    line: u32,
}

#[derive(Debug, Clone)]
struct DispatchHint {
    id: String,
    method_name: String,
    caller_node: String,
    file: String,
    line: u32,
    byte_start: usize,
    byte_end: usize,
}

#[derive(Debug, Clone)]
struct NameMatchRef {
    ref_id: String,
    caller_node: String,
    caller_file: String,
    caller_symbol: String,
    caller_signature: Option<String>,
    receiver_expression: String,
    receiver: String,
    method_name: String,
    colon_dispatch: bool,
    line: u32,
    lang: String,
}

#[derive(Debug, Clone)]
struct NameMatchCandidate {
    node_id: String,
    file_path: String,
    scoped_name: String,
    kind: String,
    // Nodes persist tree-sitter's zero-based rows; dispatch AST helpers use one-based lines.
    start_line: u32,
}

#[derive(Debug, Clone)]
struct FileRow {
    surface_fingerprint: String,
    freshness: FileFreshness,
}

#[derive(Debug, Clone)]
struct DbFileIndex {
    lang: Option<LangId>,
    exports: HashSet<String>,
    default_export: Option<String>,
    export_aliases: HashMap<String, String>,
    node_by_scoped: HashMap<String, String>,
    node_by_bare: HashMap<String, String>,
    module_targets: HashMap<String, Option<String>>,
    reexports: Vec<ReexportIndex>,
}

#[derive(Debug, Clone)]
struct ReexportIndex {
    target_file: Option<String>,
    named: HashMap<String, String>,
    wildcard: bool,
}

#[derive(Debug, Clone)]
struct ProjectIndex<'a> {
    project_root: PathBuf,
    files: HashMap<String, DbFileIndex>,
    caller_data: HashMap<String, &'a FileCallData>,
    /// Root-scoped map shared by successive refresh-worker batches. Cargo.toml
    /// watcher events replace the cache before another batch can resolve refs.
    /// Cold/direct refreshes use a private cache so each refresh builds and uses
    /// its own workspace mapping.
    workspace_crate_prefixes: WorkspaceCratePrefixCache,
}

impl ProjectIndex<'_> {
    /// Resolve a crate name to its `src` prefix, building the workspace map on
    /// first use. Refresh-worker indexes for the same root share this cache.
    fn crate_src_prefix(&self, crate_name: &str) -> Option<String> {
        self.workspace_crate_prefixes
            .0
            .get_or_init(|| build_workspace_crate_prefixes(&self.project_root))
            .get(crate_name)
            .cloned()
    }
}

impl CallGraphStore {
    pub fn open_if_enabled(
        options: CallGraphStoreOptions,
        callgraph_dir: PathBuf,
        project_root: PathBuf,
    ) -> Result<Option<Self>> {
        if !options.enabled {
            return Ok(None);
        }
        Self::open(callgraph_dir, project_root).map(Some)
    }

    pub fn open(callgraph_dir: PathBuf, project_root: PathBuf) -> Result<Self> {
        let project_key = crate::search_index::artifact_cache_key(&project_root);
        let Some(writer_lease) = acquire_writer_lease(&callgraph_dir, &project_key, &project_root)?
        else {
            return Err(CallGraphStoreError::Unavailable(
                "writer capability denied; use the read-only callgraph opener".to_string(),
            ));
        };
        std::fs::create_dir_all(&callgraph_dir)?;
        // Resolve the current generation via the pointer (falling back to the
        // legacy single-file DB). If nothing is published yet, open the legacy
        // path so a brand-new store still gets a writable DB + schema.
        let (sqlite_path, generation) = resolve_ready_target(&callgraph_dir, &project_key)
            .unwrap_or_else(|| (legacy_sqlite_path(&callgraph_dir, &project_key), None));
        let OpenedStore { store, root_repair } = Self::open_at_path(
            project_root.clone(),
            project_key,
            sqlite_path,
            generation,
            true,
            Some(Arc::clone(&writer_lease)),
            None,
        )?;
        match root_repair {
            OpenRootRepair::NeedsRebuild { .. } => {
                log_root_repair_rebuild(&root_repair);
                drop(store);
                drop(writer_lease);
                let files = crate::callgraph::walk_project_files(&project_root).collect::<Vec<_>>();
                let (store, _stats) =
                    Self::cold_build_with_lease(callgraph_dir, project_root, &files)?;
                Ok(store)
            }
            OpenRootRepair::None | OpenRootRepair::ReRooted => Ok(store),
        }
    }

    pub fn open_readonly(
        callgraph_dir: PathBuf,
        project_root: PathBuf,
    ) -> Result<Option<ReadonlyCallGraphStore>> {
        let project_key = crate::search_index::artifact_cache_key(&project_root);
        if let Some((sqlite_path, generation)) = resolve_ready_target(&callgraph_dir, &project_key)
        {
            let conn = open_readonly_connection(&sqlite_path)?;
            if !database_ready(&conn).unwrap_or(false) {
                return Ok(None);
            }
            let marker_label = generation.as_deref().unwrap_or("legacy");
            let read_marker = crate::root_cache::ReadMarker::create(&callgraph_dir, marker_label)?;
            return Ok(Some(ReadonlyCallGraphStore::from_inner(
                Self::from_connection(
                    project_root,
                    project_key,
                    sqlite_path,
                    callgraph_dir,
                    false,
                    generation,
                    None,
                    Some(read_marker),
                    conn,
                ),
            )));
        }

        let Some(target) = freshest_legacy_fallback_target(&callgraph_dir, &project_key)? else {
            return Ok(None);
        };
        crate::slog_warn!(
            "root-keyed callgraph store is empty; serving read-only fallback from legacy {} partition {}",
            target.partition.harness,
            target.sqlite_path.display()
        );
        let conn = open_readonly_connection(&target.sqlite_path)?;
        if !database_ready(&conn).unwrap_or(false) {
            return Ok(None);
        }
        let marker_label =
            legacy_read_marker_label(&target.sqlite_path, target.generation.as_deref());
        let read_marker = crate::root_cache::ReadMarker::create(&callgraph_dir, &marker_label)?;
        Ok(Some(ReadonlyCallGraphStore::from_inner(
            Self::from_connection(
                project_root,
                project_key,
                target.sqlite_path,
                callgraph_dir,
                true,
                target.generation,
                None,
                Some(read_marker),
                conn,
            ),
        )))
    }

    /// Open the currently-published ready store with write access so moved-root
    /// metadata can be repaired before projection readers consume it. Unlike
    /// [`open`], this preserves the read path's cold/mid-build behavior: if no
    /// ready generation exists, it returns `Ok(None)` instead of creating an
    /// empty legacy database. Worktree bridges must keep using [`open_readonly`].
    pub fn open_ready_repairing(
        callgraph_dir: PathBuf,
        project_root: PathBuf,
    ) -> Result<Option<Self>> {
        Self::open_ready_with_rebuild_policy(callgraph_dir, project_root, true, true)
    }

    /// Open a ready store for bounded maintenance work without repairing root
    /// metadata or starting a cold rebuild. A store that needs either action is
    /// reported as unavailable so a background build can own that work.
    pub fn open_ready(callgraph_dir: PathBuf, project_root: PathBuf) -> Result<Option<Self>> {
        Self::open_ready_with_rebuild_policy(callgraph_dir, project_root, false, false)
    }

    pub fn open_ready_no_rebuild(
        callgraph_dir: PathBuf,
        project_root: PathBuf,
    ) -> Result<Option<Self>> {
        Self::open_ready_with_rebuild_policy(callgraph_dir, project_root, false, true)
    }

    fn open_ready_with_rebuild_policy(
        callgraph_dir: PathBuf,
        project_root: PathBuf,
        allow_cold_build: bool,
        allow_root_repair: bool,
    ) -> Result<Option<Self>> {
        let project_key = crate::search_index::artifact_cache_key(&project_root);
        let Some(writer_lease) = acquire_writer_lease(&callgraph_dir, &project_key, &project_root)?
        else {
            return Ok(None);
        };
        let Some((sqlite_path, generation)) = resolve_ready_target(&callgraph_dir, &project_key)
        else {
            return Ok(None);
        };
        let OpenedStore { store, root_repair } = Self::open_at_path_with_root_repair(
            project_root.clone(),
            project_key.clone(),
            sqlite_path,
            generation,
            true,
            Some(Arc::clone(&writer_lease)),
            None,
            allow_root_repair,
        )?;
        match root_repair {
            OpenRootRepair::NeedsRebuild { .. } if allow_cold_build => {
                log_root_repair_rebuild(&root_repair);
                drop(store);
                drop(writer_lease);
                let files = crate::callgraph::walk_project_files(&project_root).collect::<Vec<_>>();
                let (store, _stats) =
                    Self::cold_build_with_lease(callgraph_dir, project_root, &files)?;
                Ok(Some(store))
            }
            OpenRootRepair::NeedsRebuild { .. } => {
                if let Some(message) = note_repair_entry(&project_key) {
                    crate::slog_warn!("{message}");
                }
                Ok(None)
            }
            OpenRootRepair::None | OpenRootRepair::ReRooted => Ok(Some(store)),
        }
    }

    pub fn cold_build_with_lease(
        callgraph_dir: PathBuf,
        project_root: PathBuf,
        files: &[PathBuf],
    ) -> Result<(Self, ColdBuildStats)> {
        Self::cold_build_with_lease_chunked(callgraph_dir, project_root, files, 0)
    }

    pub fn cold_build_with_lease_chunked(
        callgraph_dir: PathBuf,
        project_root: PathBuf,
        files: &[PathBuf],
        chunk_size: usize,
    ) -> Result<(Self, ColdBuildStats)> {
        Self::cold_build_with_lease_chunked_inner(
            callgraph_dir,
            project_root,
            files,
            chunk_size,
            false,
        )
    }

    pub(crate) fn force_cold_build_with_lease_chunked(
        callgraph_dir: PathBuf,
        project_root: PathBuf,
        files: &[PathBuf],
        chunk_size: usize,
    ) -> Result<(Self, ColdBuildStats)> {
        Self::cold_build_with_lease_chunked_inner(
            callgraph_dir,
            project_root,
            files,
            chunk_size,
            true,
        )
    }

    fn cold_build_with_lease_chunked_inner(
        callgraph_dir: PathBuf,
        project_root: PathBuf,
        files: &[PathBuf],
        chunk_size: usize,
        require_new_publication: bool,
    ) -> Result<(Self, ColdBuildStats)> {
        let project_key = crate::search_index::artifact_cache_key(&project_root);
        let Some(writer_lease) = acquire_writer_lease(&callgraph_dir, &project_key, &project_root)?
        else {
            let operation = if require_new_publication {
                "forced rebuild"
            } else {
                "cold build"
            };
            return Err(CallGraphStoreError::Unavailable(format!(
                "{operation} could not acquire writer capability"
            )));
        };
        std::fs::create_dir_all(&callgraph_dir)?;
        let (stats, generation) = Self::cold_build_publish_locked(
            &callgraph_dir,
            &project_root,
            &project_key,
            files,
            chunk_size,
            Arc::clone(&writer_lease),
        )?;
        let store = Self::open_generation(
            &callgraph_dir,
            project_root,
            project_key,
            generation,
            writer_lease,
        )?;
        Ok((store, stats))
    }

    pub fn ensure_built_with_lease(
        callgraph_dir: PathBuf,
        project_root: PathBuf,
        files: &[PathBuf],
    ) -> Result<(Self, Option<ColdBuildStats>)> {
        Self::ensure_built_with_lease_chunked(callgraph_dir, project_root, files, 0)
    }

    pub fn ensure_built_with_lease_chunked(
        callgraph_dir: PathBuf,
        project_root: PathBuf,
        files: &[PathBuf],
        chunk_size: usize,
    ) -> Result<(Self, Option<ColdBuildStats>)> {
        let project_key = crate::search_index::artifact_cache_key(&project_root);
        let Some(writer_lease) = acquire_writer_lease(&callgraph_dir, &project_key, &project_root)?
        else {
            return Err(CallGraphStoreError::Unavailable(
                "callgraph ensure could not acquire writer capability".to_string(),
            ));
        };
        std::fs::create_dir_all(&callgraph_dir)?;
        cleanup_incomplete_migrations(&callgraph_dir, &project_key);
        // Another process may have published a ready generation while we waited
        // for the lock — open it instead of rebuilding. If that generation is
        // from this same project at an older filesystem root, repair the root
        // metadata in-place while still holding the build lease. If data rows
        // contain absolute paths, publish a fresh generation under this lease
        // rather than recursively reacquiring the same lock.
        if let Some((sqlite_path, generation)) = resolve_ready_target(&callgraph_dir, &project_key)
        {
            let OpenedStore { store, root_repair } = Self::open_at_path(
                project_root.clone(),
                project_key.clone(),
                sqlite_path,
                generation,
                true,
                Some(Arc::clone(&writer_lease)),
                None,
            )?;
            match root_repair {
                OpenRootRepair::NeedsRebuild { .. } => {
                    log_root_repair_rebuild(&root_repair);
                    drop(store);
                    let (stats, generation) = Self::cold_build_publish_locked(
                        &callgraph_dir,
                        &project_root,
                        &project_key,
                        files,
                        chunk_size,
                        Arc::clone(&writer_lease),
                    )?;
                    let store = Self::open_generation(
                        &callgraph_dir,
                        project_root,
                        project_key,
                        generation,
                        writer_lease,
                    )?;
                    return Ok((store, Some(stats)));
                }
                OpenRootRepair::None | OpenRootRepair::ReRooted => {
                    return Ok((store, None));
                }
            }
        }
        if let Some(store) = try_legacy_migration_or_fallback(
            &callgraph_dir,
            &project_root,
            &project_key,
            Arc::clone(&writer_lease),
        )? {
            return Ok((store, None));
        }
        let (stats, generation) = Self::cold_build_publish_locked(
            &callgraph_dir,
            &project_root,
            &project_key,
            files,
            chunk_size,
            Arc::clone(&writer_lease),
        )?;
        let store = Self::open_generation(
            &callgraph_dir,
            project_root,
            project_key,
            generation,
            writer_lease,
        )?;
        Ok((store, Some(stats)))
    }

    /// Migrate a legacy harness-partition store without falling through to a
    /// cold build. This is used after a query has already opened a read-only
    /// fallback: the caller runs it on the same limited background lane as cold
    /// builds while queries continue using that fallback. Public so crash/retry
    /// tests can drive the migration synchronously on a thread where the
    /// thread-local failure seams apply.
    pub fn migrate_legacy_with_lease(
        callgraph_dir: PathBuf,
        project_root: PathBuf,
    ) -> Result<Option<Self>> {
        let project_key = crate::search_index::artifact_cache_key(&project_root);
        let Some(writer_lease) = acquire_writer_lease(&callgraph_dir, &project_key, &project_root)?
        else {
            return Ok(None);
        };
        std::fs::create_dir_all(&callgraph_dir)?;
        cleanup_incomplete_migrations(&callgraph_dir, &project_key);

        // Another writer may have completed the migration while this worker was
        // waiting for the lease. Adopt its root-keyed generation rather than
        // copying the legacy source a second time.
        if let Some((sqlite_path, generation)) = resolve_ready_target(&callgraph_dir, &project_key)
        {
            let OpenedStore { store, root_repair } = Self::open_at_path(
                project_root,
                project_key,
                sqlite_path,
                generation,
                true,
                Some(writer_lease),
                None,
            )?;
            return match root_repair {
                OpenRootRepair::None | OpenRootRepair::ReRooted => Ok(Some(store)),
                OpenRootRepair::NeedsRebuild { reason, .. } => {
                    Err(CallGraphStoreError::Unavailable(format!(
                        "root-keyed store discovered during legacy migration requires a cold rebuild: {reason}"
                    )))
                }
            };
        }

        let store = try_legacy_migration_or_fallback(
            &callgraph_dir,
            &project_root,
            &project_key,
            writer_lease,
        )?;
        // A disk-floor or backup-budget failure returns a readable legacy store.
        // Keep the already-resident fallback instead of sending this duplicate
        // reader through the background-install channel.
        Ok(store.filter(|store| !store.is_legacy_fallback()))
    }

    /// Build a fresh DB and publish it as a new generation, then atomically flip
    /// the `<key>.current` pointer to it. NEVER replaces an open DB file, so it
    /// succeeds even when other processes hold an older generation open (the
    /// multi-TUI Windows case). The builder owns the temp + generation files
    /// exclusively (unique pid+nanos names), so it can rename/replace them
    /// freely; only the tiny pointer is shared, and only Rust std touches it.
    ///
    /// Returns the published generation file name so callers open exactly the
    /// generation they built (avoiding a race where a concurrent build's flip
    /// would otherwise reopen a different generation).
    fn cold_build_publish_locked(
        callgraph_dir: &Path,
        project_root: &Path,
        project_key: &str,
        files: &[PathBuf],
        chunk_size: usize,
        writer_lease: Arc<crate::root_cache::WriterLease>,
    ) -> Result<(ColdBuildStats, String)> {
        if let Some((previous_root, remaining)) =
            rebuild_cooldown_denial(callgraph_dir, project_key, project_root, Instant::now())
        {
            return Err(CallGraphStoreError::Unavailable(format!(
                "cache key {project_key} was rebuilt for {} too recently; retry {} ms after the per-key cooldown",
                previous_root.display(),
                remaining.as_millis()
            )));
        }
        let generation = generation_file_name(project_key);
        let gen_path = callgraph_dir.join(&generation);
        let temp_path = callgraph_dir.join(format!(
            "{generation}.tmp.{}.{}",
            std::process::id(),
            now_nanos()
        ));
        remove_sqlite_file_set(&temp_path);

        let stats = {
            let temp_store = Self::open_at_path(
                project_root.to_path_buf(),
                project_key.to_string(),
                temp_path.clone(),
                None,
                false,
                Some(Arc::clone(&writer_lease)),
                None,
            )?
            .store;
            let stats = temp_store.cold_build_chunked(files, chunk_size)?;
            let _ = temp_store.checkpoint_wal_truncate();
            temp_store.prepare_for_atomic_swap()?;
            stats
        };

        notify_cold_build_before_publish_observer();
        let publication = publish_if_current(|| {
            verify_writer_lease(&writer_lease)?;
            // Move the finished build to its final generation path. This target is
            // brand-new and owned by us, so the rename never hits an open file.
            remove_sqlite_file_set(&gen_path);
            crate::fs_lock::rename_over(&temp_path, &gen_path)?;
            crate::fs_lock::sync_parent(&gen_path);
            remove_sqlite_sidecars(&gen_path);

            notify_cold_build_swap_observer(&temp_path, &gen_path);

            // Atomically publish the new generation, then best-effort GC old ones.
            verify_writer_lease(&writer_lease)?;
            publish_pointer(callgraph_dir, project_key, &generation)?;
            gc_old_generations(callgraph_dir, project_key, &generation);
            // Store-wide orphan sweep on the same cadence: reclaims aged build
            // temps for roots that no longer build here, which the per-root GC
            // above never reaches.
            sweep_orphaned_build_temps_store_wide(callgraph_dir);
            if let Some(storage_root) = root_storage_dir(callgraph_dir) {
                let inspect_root =
                    storage_root.join(crate::root_cache::RootCacheDomain::Inspect.as_str());
                let live_scope_keys = crate::root_cache::live_scope_keys_for_storage(&storage_root);
                crate::inspect::cache::sweep_inspect_scope_dirs(&inspect_root, &live_scope_keys);
            }
            Ok(())
        });
        if matches!(publication, Err(CallGraphStoreError::Superseded)) {
            remove_sqlite_file_set(&temp_path);
        }
        publication?;
        record_successful_rebuild(callgraph_dir, project_key, project_root, Instant::now());
        Ok((stats, generation))
    }

    /// Open a specific just-published generation (read-write, WAL) so a builder
    /// returns a store pinned to exactly what it built.
    fn open_generation(
        callgraph_dir: &Path,
        project_root: PathBuf,
        project_key: String,
        generation: String,
        writer_lease: Arc<crate::root_cache::WriterLease>,
    ) -> Result<Self> {
        let gen_path = callgraph_dir.join(&generation);
        Ok(Self::open_at_path(
            project_root,
            project_key,
            gen_path,
            Some(generation),
            true,
            Some(writer_lease),
            None,
        )?
        .store)
    }

    pub fn needs_cold_build(callgraph_dir: &Path, project_root: &Path) -> Result<bool> {
        let project_key = crate::search_index::artifact_cache_key(project_root);
        // A cold build is needed unless a ready generation (or ready legacy DB)
        // is currently published.
        Ok(resolve_ready_target(callgraph_dir, &project_key).is_none())
    }

    fn open_at_path(
        project_root: PathBuf,
        project_key: String,
        sqlite_path: PathBuf,
        generation: Option<String>,
        use_wal: bool,
        writer_lease: Option<Arc<crate::root_cache::WriterLease>>,
        read_marker: Option<crate::root_cache::ReadMarker>,
    ) -> Result<OpenedStore> {
        Self::open_at_path_with_root_repair(
            project_root,
            project_key,
            sqlite_path,
            generation,
            use_wal,
            writer_lease,
            read_marker,
            true,
        )
    }

    fn open_at_path_with_root_repair(
        project_root: PathBuf,
        project_key: String,
        sqlite_path: PathBuf,
        generation: Option<String>,
        use_wal: bool,
        writer_lease: Option<Arc<crate::root_cache::WriterLease>>,
        read_marker: Option<crate::root_cache::ReadMarker>,
        allow_root_repair: bool,
    ) -> Result<OpenedStore> {
        if let Some(lease) = writer_lease.as_ref() {
            verify_writer_lease(lease)?;
        }
        if let Some(parent) = sqlite_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut conn = Connection::open(&sqlite_path)?;
        if use_wal {
            configure_connection(&conn)?;
        } else {
            configure_build_connection(&conn)?;
        }
        if let Some(lease) = writer_lease.as_ref() {
            verify_writer_lease(lease)?;
        }
        initialize_schema(&conn)?;
        if let Some(lease) = writer_lease.as_ref() {
            verify_writer_lease(lease)?;
        }
        let root_repair = reconcile_workspace_roots(&mut conn, &project_root, allow_root_repair)?;
        let read_marker = match (read_marker, generation.as_deref(), sqlite_path.parent()) {
            (Some(marker), _, _) => Some(marker),
            (None, Some(label), Some(cache_dir)) => {
                Some(crate::root_cache::ReadMarker::create(cache_dir, label)?)
            }
            (None, _, _) => None,
        };
        let publication_dir = sqlite_path
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_default();
        let store = Self::from_connection(
            project_root,
            project_key,
            sqlite_path,
            publication_dir,
            false,
            generation,
            writer_lease,
            read_marker,
            conn,
        );
        Ok(OpenedStore { store, root_repair })
    }

    fn prepare_for_atomic_swap(&self) -> Result<()> {
        let conn = self.conn.lock().expect("callgraph store mutex poisoned");
        conn.execute_batch(self.atomic_swap_checkpoint_sql())?;
        Ok(())
    }

    fn atomic_swap_checkpoint_sql(&self) -> &'static str {
        let protected_reader = self.generation.as_deref().is_some_and(|generation| {
            self.sqlite_path
                .parent()
                .is_some_and(|dir| crate::root_cache::protected_read_marker_exists(dir, generation))
        });
        if protected_reader {
            "PRAGMA wal_checkpoint(PASSIVE); PRAGMA journal_mode=DELETE;"
        } else {
            "PRAGMA wal_checkpoint(TRUNCATE); PRAGMA journal_mode=DELETE;"
        }
    }

    fn from_connection(
        project_root: PathBuf,
        project_key: String,
        sqlite_path: PathBuf,
        publication_dir: PathBuf,
        legacy_fallback: bool,
        generation: Option<String>,
        writer_lease: Option<Arc<crate::root_cache::WriterLease>>,
        read_marker: Option<crate::root_cache::ReadMarker>,
        conn: Connection,
    ) -> Self {
        let write_metrics = callgraph_write_metrics_for_key(&project_key);
        Self {
            project_root,
            project_key,
            sqlite_path,
            publication_dir,
            legacy_fallback,
            generation,
            writer_lease,
            read_marker,
            database_ready: AtomicBool::new(false),
            write_metrics,
            conn: Mutex::new(conn),
        }
    }

    fn ensure_ready(&self, conn: &Connection) -> Result<()> {
        if self.database_ready.load(AtomicOrdering::Acquire) {
            return Ok(());
        }
        ensure_database_ready(conn)?;
        self.database_ready.store(true, AtomicOrdering::Release);
        Ok(())
    }

    pub fn project_root(&self) -> &Path {
        &self.project_root
    }

    pub fn project_key(&self) -> &str {
        &self.project_key
    }

    pub fn sqlite_path(&self) -> &Path {
        &self.sqlite_path
    }

    /// Whether this store is reading from a legacy harness partition because
    /// the root-keyed store has not published a generation yet.
    pub fn is_legacy_fallback(&self) -> bool {
        self.legacy_fallback
    }

    pub(crate) fn is_legacy_migration(&self) -> bool {
        self.generation.as_deref().is_some_and(|generation| {
            migration_generation_requires_manifest(generation)
                && migration_manifest_valid(&self.publication_dir, generation)
        })
    }

    pub fn writer_epoch_for_test(&self) -> Option<&str> {
        self.writer_lease.as_ref().map(|lease| lease.epoch())
    }

    fn verify_writer_lease(&self) -> Result<()> {
        let Some(lease) = self.writer_lease.as_ref() else {
            return Err(CallGraphStoreError::Unavailable(
                "callgraph store opened read-only; write API is unavailable".to_string(),
            ));
        };
        verify_writer_lease(lease)
    }

    fn refresh_read_marker(&self) -> Result<()> {
        if let Some(marker) = self.read_marker.as_ref() {
            marker.touch_if_due()?;
        }
        Ok(())
    }

    fn record_commit(&self, total_changes_before: u64, conn: &Connection) {
        self.write_metrics
            .record_commit(conn.total_changes().saturating_sub(total_changes_before));
    }

    fn checkpoint_wal_truncate(&self) -> bool {
        let conn = self.conn.lock().expect("callgraph store mutex poisoned");
        checkpoint_wal_truncate(&conn)
    }

    /// True if this store still reflects the currently-published generation.
    /// Cheap (one small pointer-file read). When false, another process (or a
    /// local cold rebuild) has published a newer generation and the holder
    /// should drop this store and reopen via the pointer to converge. A missing
    /// pointer keeps the current store (legacy DB still valid, or transient).
    pub fn is_current(&self) -> bool {
        let _ = self.refresh_read_marker();
        match (
            read_pointer(&self.publication_dir, &self.project_key),
            &self.generation,
        ) {
            // Even when both generations happen to have the same filename, the
            // root-keyed pointer names a different directory from the fallback.
            (Some(_), _) if self.legacy_fallback => false,
            (Some(published), Some(opened)) => &published == opened,
            // A generation now supersedes the legacy single-file DB we opened.
            (Some(_), None) => false,
            // No pointer: keep serving (legacy DB, or an anomalous pointer
            // removal where our open generation file is still valid).
            (None, _) => true,
        }
    }

    pub fn cold_build(&self, files: &[PathBuf]) -> Result<ColdBuildStats> {
        self.cold_build_chunked(files, 0)
    }

    pub fn cold_build_chunked(
        &self,
        files: &[PathBuf],
        chunk_size: usize,
    ) -> Result<ColdBuildStats> {
        let started = Instant::now();
        let bench = std::env::var("AFT_BENCH_COLD").is_ok();
        macro_rules! phase {
            ($label:expr, $t:expr) => {
                if bench {
                    eprintln!("  cold_build[{}]: {} ms", $label, $t.elapsed().as_millis());
                    let _ = std::io::Write::flush(&mut std::io::stderr());
                }
            };
        }
        let files = normalize_file_list(&self.project_root, files)?;

        if chunk_size == 0 {
            let t = Instant::now();
            let build = build_extracts_parallel(&self.project_root, &files);
            phase!("extract_parallel", t);
            let extracts = build.extracts;
            let failures = build.failures;
            let node_count = extracts.iter().map(|extract| extract.nodes.len()).sum();

            let t = Instant::now();
            let index = ProjectIndex::from_extracts(&self.project_root, &extracts);
            phase!("build_index", t);
            let t = Instant::now();
            let mut resolved_refs = Vec::new();
            for extract in &extracts {
                for raw_ref in &extract.raw_refs {
                    resolved_refs.push(resolve_ref(raw_ref.clone(), &index)?);
                }
            }
            phase!("resolve_refs", t);
            let ref_count = resolved_refs.len();
            let edge_count = resolved_refs
                .iter()
                .filter(|item| item.edge.is_some())
                .count();

            let t = Instant::now();
            self.verify_writer_lease()?;
            let mut conn = self.conn.lock().expect("callgraph store mutex poisoned");
            let total_changes_before = conn.total_changes();
            let tx = conn.transaction()?;
            clear_tables(&tx)?;
            insert_meta(&tx)?;
            drop_cold_build_secondary_indexes(&tx)?;
            {
                let workspace_root = self.project_root.display().to_string();
                let mut inserts = ColdBuildInsertStatements::new(&tx)?;
                for extract in &extracts {
                    insert_file_extract_prepared(&mut inserts, &workspace_root, extract)?;
                }
                for failure in &failures {
                    insert_backend_state_prepared(
                        &mut inserts.backend_state,
                        &workspace_root,
                        &failure.rel_path,
                        failure
                            .freshness
                            .as_ref()
                            .map(|freshness| &freshness.content_hash),
                        "stale",
                    )?;
                }
                for resolved in &resolved_refs {
                    insert_resolved_ref_prepared(&mut inserts, resolved)?;
                }
            }
            create_cold_build_secondary_indexes(&tx)?;
            let supplemental_edge_count =
                insert_method_dispatch_edges(&tx, &self.project_root, None)?;
            set_meta_ready(&tx, true)?;
            tx.commit()?;
            self.record_commit(total_changes_before, &conn);
            phase!("sqlite_insert", t);

            let elapsed_ms = started.elapsed().as_millis();
            crate::slog_info!(
                "perf callgraph_store cold_build: files={} nodes={} refs={} edges={} ms={}",
                extracts.len(),
                node_count,
                ref_count,
                edge_count + supplemental_edge_count,
                elapsed_ms
            );
            return Ok(ColdBuildStats {
                files: extracts.len(),
                nodes: node_count,
                refs: ref_count,
                edges: edge_count + supplemental_edge_count,
                failed_files: failures
                    .into_iter()
                    .map(|failure| failure.rel_path)
                    .collect(),
                elapsed_ms,
            });
        }

        // Chunked implementation: parse and resolve in batches to reduce peak
        // memory during cold build without changing the persisted graph.
        let t = Instant::now();
        self.verify_writer_lease()?;
        let mut conn = self.conn.lock().expect("callgraph store mutex poisoned");
        let total_changes_before = conn.total_changes();
        let tx = conn.transaction()?;
        clear_tables(&tx)?;
        insert_meta(&tx)?;
        drop_cold_build_secondary_indexes(&tx)?;

        let mut all_raw_refs = Vec::new();
        let mut failures = Vec::new();
        let mut node_count = 0;
        let mut files_parsed = 0;

        let mut persistent_call_data = Vec::new();
        let mut file_to_call_data_index = HashMap::new();
        let mut files_index = HashMap::new();

        let workspace_root = self.project_root.display().to_string();

        {
            let mut inserts = ColdBuildInsertStatements::new(&tx)?;
            for chunk in files.chunks(chunk_size) {
                let build = build_extracts_parallel(&self.project_root, chunk);
                failures.extend(build.failures.clone());

                for extract in build.extracts {
                    files_parsed += 1;
                    node_count += extract.nodes.len();
                    insert_file_extract_prepared(&mut inserts, &workspace_root, &extract)?;

                    let db_file_index = DbFileIndex::from_extract(&self.project_root, &extract);
                    files_index.insert(extract.rel_path.clone(), db_file_index);

                    persistent_call_data.push(extract.data);
                    let idx = persistent_call_data.len() - 1;
                    file_to_call_data_index.insert(extract.rel_path.clone(), idx);

                    all_raw_refs.push((extract.rel_path, extract.raw_refs));
                }
                for failure in &build.failures {
                    insert_backend_state_prepared(
                        &mut inserts.backend_state,
                        &workspace_root,
                        &failure.rel_path,
                        failure
                            .freshness
                            .as_ref()
                            .map(|freshness| &freshness.content_hash),
                        "stale",
                    )?;
                }
            }
        }

        let mut caller_data = HashMap::new();
        for (rel_path, idx) in &file_to_call_data_index {
            caller_data.insert(rel_path.clone(), &persistent_call_data[*idx]);
        }
        let indexed_caller_files = files_index.keys().cloned().collect::<BTreeSet<_>>();
        let index = ProjectIndex::from_parts(
            &self.project_root,
            files_index,
            caller_data,
            WorkspaceCratePrefixCache::default(),
        );

        let mut resolved_refs = Vec::new();
        for (_, raw_refs) in all_raw_refs {
            for raw_ref in raw_refs {
                resolved_refs.push(resolve_ref(raw_ref, &index)?);
            }
        }

        let ref_count = resolved_refs.len();
        let edge_count = resolved_refs
            .iter()
            .filter(|item| item.edge.is_some())
            .count();

        {
            let mut inserts = ColdBuildInsertStatements::new(&tx)?;
            for resolved in &resolved_refs {
                insert_resolved_ref_prepared(&mut inserts, resolved)?;
            }
        }
        create_cold_build_secondary_indexes(&tx)?;
        let supplemental_edge_count = insert_method_dispatch_edges_chunked(
            &tx,
            &self.project_root,
            &indexed_caller_files,
            chunk_size,
        )?;
        set_meta_ready(&tx, true)?;
        tx.commit()?;
        self.record_commit(total_changes_before, &conn);
        phase!("sqlite_insert", t);

        let elapsed_ms = started.elapsed().as_millis();
        crate::slog_info!(
            "perf callgraph_store cold_build (chunked): files={} nodes={} refs={} edges={} ms={}",
            files_parsed,
            node_count,
            ref_count,
            edge_count + supplemental_edge_count,
            elapsed_ms
        );
        Ok(ColdBuildStats {
            files: files_parsed,
            nodes: node_count,
            refs: ref_count,
            edges: edge_count + supplemental_edge_count,
            failed_files: failures
                .into_iter()
                .map(|failure| failure.rel_path)
                .collect(),
            elapsed_ms,
        })
    }

    pub fn refresh_files(&self, changed_files: &[PathBuf]) -> Result<IncrementalStats> {
        self.refresh_files_with_workspace_crate_prefix_cache(
            changed_files,
            WorkspaceCratePrefixCache::default(),
        )
    }

    fn refresh_files_with_workspace_crate_prefix_cache(
        &self,
        changed_files: &[PathBuf],
        workspace_crate_prefixes: WorkspaceCratePrefixCache,
    ) -> Result<IncrementalStats> {
        let (stats, profile) = self.refresh_files_profiled_with_workspace_crate_prefix_cache(
            changed_files,
            workspace_crate_prefixes,
        )?;
        if std::env::var_os("AFT_BENCH_REFRESH_FILES").is_some() {
            eprintln!("refresh_files phases: {}", profile.report());
        }
        Ok(stats)
    }

    /// Run an incremental refresh and return phase timings for an offline store copy.
    #[doc(hidden)]
    pub fn refresh_files_profiled(
        &self,
        changed_files: &[PathBuf],
    ) -> Result<(IncrementalStats, RefreshFilesProfile)> {
        self.refresh_files_profiled_with_workspace_crate_prefix_cache(
            changed_files,
            WorkspaceCratePrefixCache::default(),
        )
    }

    fn refresh_files_profiled_with_workspace_crate_prefix_cache(
        &self,
        changed_files: &[PathBuf],
        workspace_crate_prefixes: WorkspaceCratePrefixCache,
    ) -> Result<(IncrementalStats, RefreshFilesProfile)> {
        let total_started = Instant::now();
        let mut profile = RefreshFilesProfile::default();
        self.verify_writer_lease()?;
        let mut conn = self.conn.lock().expect("callgraph store mutex poisoned");
        let total_changes_before = conn.total_changes();
        let tx = conn.transaction()?;
        ensure_database_ready(&tx)?;
        let mut changed = Vec::new();
        let mut surface_changed = BTreeSet::new();
        let mut deleted = BTreeSet::new();
        let mut own_refresh = BTreeSet::new();
        let mut candidate_own_refresh = BTreeSet::new();
        let mut unchanged_extracts = 0usize;
        let mut selected_ref_ids = BTreeSet::new();
        let mut selected_refs_by_caller = BTreeMap::new();
        let mut changed_extracts: HashMap<String, FileExtract> = HashMap::new();

        for input in changed_files {
            let abs_path = normalize_file_path(&self.project_root, input)?;
            let rel_path = relative_path(&self.project_root, &abs_path);
            changed.push(rel_path.clone());
            let old_row = load_file_row(&tx, &rel_path)?;
            if !abs_path.exists() {
                if old_row.is_some() {
                    surface_changed.insert(rel_path.clone());
                    deleted.insert(rel_path.clone());
                    let started = Instant::now();
                    let dependent_refs = ref_ids_depending_on(&tx, &self.project_root, &rel_path)?;
                    profile.dependency_selection += started.elapsed();
                    record_dependent_refs(
                        &mut selected_ref_ids,
                        &mut selected_refs_by_caller,
                        dependent_refs,
                    );
                    let started = Instant::now();
                    delete_file_rows(&tx, &rel_path)?;
                    clear_backend_state_for_file(&tx, &self.project_root, &rel_path)?;
                    profile.row_deletes += started.elapsed();
                }
                continue;
            }

            if let Some(row) = &old_row {
                match cache_freshness::verify_file(&abs_path, &row.freshness) {
                    FreshnessVerdict::HotFresh => continue,
                    FreshnessVerdict::ContentFresh {
                        new_mtime,
                        new_size,
                    } => {
                        update_file_fresh_metadata(
                            &tx,
                            &self.project_root,
                            &rel_path,
                            &row.freshness.content_hash,
                            new_mtime,
                            new_size,
                        )?;
                        continue;
                    }
                    FreshnessVerdict::Deleted => {
                        surface_changed.insert(rel_path.clone());
                        deleted.insert(rel_path.clone());
                        let started = Instant::now();
                        let dependent_refs =
                            ref_ids_depending_on(&tx, &self.project_root, &rel_path)?;
                        profile.dependency_selection += started.elapsed();
                        record_dependent_refs(
                            &mut selected_ref_ids,
                            &mut selected_refs_by_caller,
                            dependent_refs,
                        );
                        let started = Instant::now();
                        delete_file_rows(&tx, &rel_path)?;
                        clear_backend_state_for_file(&tx, &self.project_root, &rel_path)?;
                        profile.row_deletes += started.elapsed();
                        continue;
                    }
                    FreshnessVerdict::Stale => {}
                }
            }

            let started = Instant::now();
            let extract = build_file_extract(&self.project_root, &abs_path)?;
            profile.parse += started.elapsed();
            let surface_is_changed = old_row
                .as_ref()
                .map(|row| row.surface_fingerprint != extract.surface_fingerprint)
                .unwrap_or(true);
            if surface_is_changed {
                surface_changed.insert(rel_path.clone());
                let started = Instant::now();
                let dependent_refs = ref_ids_depending_on(&tx, &self.project_root, &rel_path)?;
                profile.dependency_selection += started.elapsed();
                record_dependent_refs(
                    &mut selected_ref_ids,
                    &mut selected_refs_by_caller,
                    dependent_refs,
                );
            }
            candidate_own_refresh.insert(rel_path.clone());
            changed_extracts.insert(rel_path, extract);
        }

        let dependency_selected_refs = selected_ref_ids.len();
        let mut touched_callers: BTreeSet<String> =
            selected_refs_by_caller.keys().cloned().collect();
        touched_callers.extend(candidate_own_refresh.iter().cloned());

        let mut caller_extracts: HashMap<String, FileExtract> = HashMap::new();
        for rel_path in &touched_callers {
            if deleted.contains(rel_path) {
                continue;
            }
            if let Some(extract) = changed_extracts.get(rel_path) {
                caller_extracts.insert(rel_path.clone(), extract.clone());
                continue;
            }
            let abs_path = self.project_root.join(rel_path);
            if abs_path.exists() {
                let started = Instant::now();
                let extract = build_file_extract(&self.project_root, &abs_path)?;
                profile.dependent_parse += started.elapsed();
                caller_extracts.insert(rel_path.clone(), extract);
            }
        }

        let started = Instant::now();
        let index = ProjectIndex::from_db_and_callers(
            &tx,
            &self.project_root,
            &caller_extracts,
            workspace_crate_prefixes,
        )?;
        profile.index_load += started.elapsed();

        for rel_path in &candidate_own_refresh {
            let Some(extract) = changed_extracts.get(rel_path) else {
                continue;
            };
            if !write_amplification_baseline_enabled()
                && stored_extract_matches(&tx, rel_path, extract, &index)?
            {
                unchanged_extracts += 1;
                update_file_fresh_metadata(
                    &tx,
                    &self.project_root,
                    rel_path,
                    &extract.freshness.content_hash,
                    extract.freshness.mtime,
                    extract.freshness.size,
                )?;
                continue;
            }

            own_refresh.insert(rel_path.clone());
            let started = Instant::now();
            delete_file_rows(&tx, rel_path)?;
            profile.row_deletes += started.elapsed();
            let started = Instant::now();
            insert_file_extract(&tx, &self.project_root, extract)?;
            profile.row_inserts += started.elapsed();
        }

        let dependency_callers = touched_callers
            .iter()
            .filter(|rel_path| {
                !deleted.contains(*rel_path) && !candidate_own_refresh.contains(*rel_path)
            })
            .cloned()
            .collect::<Vec<_>>();
        for rel_path in dependency_callers {
            let Some(extract) = caller_extracts.get(&rel_path) else {
                continue;
            };
            if stored_node_ids_match_extract(&tx, &rel_path, extract)? {
                continue;
            }

            own_refresh.insert(rel_path.clone());
            let started = Instant::now();
            delete_file_rows(&tx, &rel_path)?;
            profile.row_deletes += started.elapsed();
            let started = Instant::now();
            insert_file_extract(&tx, &self.project_root, extract)?;
            profile.row_inserts += started.elapsed();
        }
        let started = Instant::now();
        for rel_path in &touched_callers {
            if deleted.contains(rel_path) {
                continue;
            }
            let Some(extract) = caller_extracts.get(rel_path) else {
                continue;
            };
            if own_refresh.contains(rel_path) {
                delete_refs_for_caller(&tx, rel_path)?;
                for raw_ref in &extract.raw_refs {
                    let resolved = resolve_ref(raw_ref.clone(), &index)?;
                    insert_resolved_ref(&tx, &resolved)?;
                }
                continue;
            }

            let selected_for_caller = selected_refs_by_caller
                .get(rel_path)
                .cloned()
                .unwrap_or_default();
            delete_ref_ids(&tx, &selected_for_caller)?;
            for raw_ref in &extract.raw_refs {
                if selected_for_caller.contains(&raw_ref.ref_id) {
                    let resolved = resolve_ref(raw_ref.clone(), &index)?;
                    insert_resolved_ref(&tx, &resolved)?;
                }
            }
        }
        profile.ref_resolution += started.elapsed();

        let started = Instant::now();
        delete_method_dispatch_edges_for_callers(&tx, &own_refresh)?;
        insert_method_dispatch_edges(&tx, &self.project_root, Some(&own_refresh))?;
        profile.method_dispatch += started.elapsed();

        let started = Instant::now();
        commit_incremental_if_current(tx)?;
        self.record_commit(total_changes_before, &conn);
        profile.commit += started.elapsed();
        profile.total = total_started.elapsed();
        Ok((
            IncrementalStats {
                changed_files: changed,
                surface_changed: surface_changed.into_iter().collect(),
                deleted_files: deleted.into_iter().collect(),
                dependency_selected_refs,
                refreshed_own_files: own_refresh.len(),
                unchanged_extract_files: unchanged_extracts,
            },
            profile,
        ))
    }

    pub fn refresh_corpus(&self, current_files: &[PathBuf]) -> Result<ColdBuildStats> {
        self.cold_build(current_files)
    }

    pub fn mark_files_stale(&self, files: &[PathBuf]) -> Result<Vec<String>> {
        self.verify_writer_lease()?;
        let mut conn = self.conn.lock().expect("callgraph store mutex poisoned");
        let total_changes_before = conn.total_changes();
        let tx = conn.transaction()?;
        let mut marked = Vec::new();
        for path in files {
            let abs_path = normalize_file_path(&self.project_root, path)?;
            let rel_path = relative_path(&self.project_root, &abs_path);
            let freshness = cache_freshness::collect(&abs_path).ok();
            mark_backend_state(
                &tx,
                &self.project_root,
                &rel_path,
                freshness.as_ref().map(|freshness| &freshness.content_hash),
                "stale",
            )?;
            marked.push(rel_path);
        }
        tx.commit()?;
        self.record_commit(total_changes_before, &conn);
        marked.sort();
        marked.dedup();
        Ok(marked)
    }

    pub fn stale_files(&self) -> Result<Vec<String>> {
        self.refresh_read_marker()?;
        let conn = self.conn.lock().expect("callgraph store mutex poisoned");
        let mut stmt = conn.prepare(
            "SELECT DISTINCT file_path FROM backend_file_state
             WHERE backend = ?1 AND workspace_root = ?2 AND status = 'stale'
             ORDER BY file_path",
        )?;
        let rows = stmt.query_map(
            params![BACKEND_TREESITTER, self.project_root.display().to_string()],
            |row| row.get::<_, String>(0),
        )?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(Into::into)
    }

    pub fn backend_status_for_file(&self, file: &Path) -> Result<Option<String>> {
        self.refresh_read_marker()?;
        let rel_path = relative_path(
            &self.project_root,
            &normalize_file_path(&self.project_root, file)?,
        );
        let conn = self.conn.lock().expect("callgraph store mutex poisoned");
        conn.query_row(
            "SELECT status FROM backend_file_state
             WHERE backend = ?1 AND workspace_root = ?2 AND file_path = ?3
             ORDER BY updated_at DESC LIMIT 1",
            params![
                BACKEND_TREESITTER,
                self.project_root.display().to_string(),
                rel_path
            ],
            |row| row.get(0),
        )
        .optional()
        .map_err(Into::into)
    }

    pub fn edge_snapshot(&self) -> Result<BTreeSet<StoredEdge>> {
        self.refresh_read_marker()?;
        let conn = self.conn.lock().expect("callgraph store mutex poisoned");
        self.ensure_ready(&conn)?;
        edge_snapshot_with_conn(&conn)
    }

    pub fn indexed_file_count(&self) -> Result<usize> {
        self.refresh_read_marker()?;
        let conn = self.conn.lock().expect("callgraph store mutex poisoned");
        self.ensure_ready(&conn)?;
        indexed_file_count(&conn)
    }

    pub fn node_for(&self, file_rel: &Path, symbol: &str) -> Result<StoreNode> {
        self.refresh_read_marker()?;
        let abs_path = normalize_file_path(&self.project_root, file_rel)?;
        let rel_path = relative_path(&self.project_root, &abs_path);
        let conn = self.conn.lock().expect("callgraph store mutex poisoned");
        self.ensure_ready(&conn)?;
        resolve_node_for_rel(&conn, &rel_path, symbol)
    }

    /// Return all positional nodes matching a legacy symbol query in a file.
    ///
    /// Consumers that need legacy compatibility can collapse these by
    /// `StoreNode::symbol` before deciding whether a query is ambiguous.
    pub fn nodes_for(&self, file_rel: &Path, symbol: &str) -> Result<Vec<StoreNode>> {
        self.refresh_read_marker()?;
        let abs_path = normalize_file_path(&self.project_root, file_rel)?;
        let rel_path = relative_path(&self.project_root, &abs_path);
        let conn = self.conn.lock().expect("callgraph store mutex poisoned");
        self.ensure_ready(&conn)?;
        nodes_for_file_matching_symbol(&conn, &rel_path, symbol)
    }

    /// Return all positional nodes matching a symbol query anywhere in the store.
    pub fn nodes_matching(&self, symbol: &str) -> Result<Vec<StoreNode>> {
        self.refresh_read_marker()?;
        let conn = self.conn.lock().expect("callgraph store mutex poisoned");
        self.ensure_ready(&conn)?;
        nodes_matching_symbol(&conn, symbol)
    }

    /// Return direct callers for an already-resolved `(file, scoped_symbol)` tuple.
    pub fn direct_callers_of(&self, file_rel: &Path, symbol: &str) -> Result<Vec<StoreCallSite>> {
        self.refresh_read_marker()?;
        let abs_path = normalize_file_path(&self.project_root, file_rel)?;
        let rel_path = relative_path(&self.project_root, &abs_path);
        let conn = self.conn.lock().expect("callgraph store mutex poisoned");
        self.ensure_ready(&conn)?;
        direct_callers_for_tuple(&conn, &rel_path, symbol)
    }

    /// Count distinct direct call sites for store-relative target tuples in bounded batches.
    pub fn direct_caller_counts_of(
        &self,
        targets: &[(String, String)],
    ) -> Result<HashMap<(String, String), usize>> {
        if targets.is_empty() {
            return Ok(HashMap::new());
        }
        self.refresh_read_marker()?;
        let conn = self.conn.lock().expect("callgraph store mutex poisoned");
        self.ensure_ready(&conn)?;
        direct_caller_counts_for_tuples(&conn, targets)
    }

    pub fn callers_of(
        &self,
        file_rel: &Path,
        symbol: &str,
        depth: usize,
    ) -> Result<StoreCallersResult> {
        let target = self.node_for(file_rel, symbol)?;
        let conn = self.conn.lock().expect("callgraph store mutex poisoned");
        self.ensure_ready(&conn)?;
        let effective_depth = depth.max(1);
        let mut visited = HashSet::new();
        let mut callers = Vec::new();
        let mut depth_limited = false;
        let mut truncated = 0usize;
        collect_callers_recursive(
            &conn,
            &target.file,
            &target.symbol,
            effective_depth,
            0,
            &mut visited,
            &mut callers,
            &mut depth_limited,
            &mut truncated,
        )?;
        Ok(StoreCallersResult {
            target,
            callers,
            scanned_files: indexed_file_count(&conn)?,
            depth_limited,
            truncated,
        })
    }

    pub fn impact_of(
        &self,
        file_rel: &Path,
        symbol: &str,
        depth: usize,
    ) -> Result<StoreImpactResult> {
        let callers = self.callers_of(file_rel, symbol, depth)?;
        let target_parameters = callers
            .target
            .signature
            .as_deref()
            .map(|signature| callgraph::extract_parameters(signature, callers.target.lang))
            .unwrap_or_default();
        let mut source_lines_by_file: HashMap<String, Option<Vec<String>>> = HashMap::new();
        for site in &callers.callers {
            source_lines_by_file
                .entry(site.caller.file.clone())
                .or_insert_with(|| {
                    read_trimmed_source_lines(&self.project_root.join(&site.caller.file))
                });
        }
        let enriched = callers
            .callers
            .iter()
            .map(|site| StoreImpactCaller {
                site: site.clone(),
                signature: site.caller.signature.clone(),
                is_entry_point: site.caller.is_entry_point,
                call_expression: source_lines_by_file
                    .get(&site.caller.file)
                    .and_then(|lines| lines.as_ref())
                    .and_then(|lines| lines.get(site.line.saturating_sub(1) as usize))
                    .cloned(),
                parameters: site
                    .caller
                    .signature
                    .as_deref()
                    .map(|signature| callgraph::extract_parameters(signature, site.caller.lang))
                    .unwrap_or_default(),
            })
            .collect();
        Ok(StoreImpactResult {
            target: callers.target,
            parameters: target_parameters,
            callers: enriched,
            depth_limited: callers.depth_limited,
            truncated: callers.truncated,
        })
    }

    pub fn outgoing_calls_of(&self, node: &StoreNode) -> Result<Vec<StoreCallSite>> {
        self.refresh_read_marker()?;
        let conn = self.conn.lock().expect("callgraph store mutex poisoned");
        self.ensure_ready(&conn)?;
        outgoing_calls_for_node(&conn, node)
    }

    /// Fetch outgoing calls for a BFS frontier without reopening the store per symbol or edge.
    pub fn outgoing_calls_for_symbols(
        &self,
        sources: &[(String, String)],
    ) -> Result<HashMap<(String, String), Vec<StoreCallSite>>> {
        if sources.is_empty() {
            return Ok(HashMap::new());
        }
        self.refresh_read_marker()?;
        let conn = self.conn.lock().expect("callgraph store mutex poisoned");
        self.ensure_ready(&conn)?;
        outgoing_calls_for_symbol_tuples(&conn, sources)
    }

    /// Return resolved direct self-call refs suppressed from the general edge table.
    pub fn resolved_self_calls_of(&self, node: &StoreNode) -> Result<Vec<StoreCallSite>> {
        self.refresh_read_marker()?;
        let conn = self.conn.lock().expect("callgraph store mutex poisoned");
        self.ensure_ready(&conn)?;
        resolved_self_calls_for_node(&conn, node)
    }

    pub fn unresolved_calls_of(&self, node: &StoreNode) -> Result<Vec<StoreUnresolvedCall>> {
        self.refresh_read_marker()?;
        let conn = self.conn.lock().expect("callgraph store mutex poisoned");
        self.ensure_ready(&conn)?;
        unresolved_calls_for_node(&conn, node)
    }

    pub fn call_tree(
        &self,
        file_rel: &Path,
        symbol: &str,
        max_depth: usize,
    ) -> Result<callgraph::CallTreeNode> {
        let node = self.node_for(file_rel, symbol)?;
        let conn = self.conn.lock().expect("callgraph store mutex poisoned");
        self.ensure_ready(&conn)?;
        let mut visited = HashSet::new();
        call_tree_inner(&conn, &node, max_depth, 0, &mut visited)
    }

    pub fn trace_to(
        &self,
        file_rel: &Path,
        symbol: &str,
        max_depth: usize,
    ) -> Result<callgraph::TraceToResult> {
        let target = self.node_for(file_rel, symbol)?;
        let conn = self.conn.lock().expect("callgraph store mutex poisoned");
        self.ensure_ready(&conn)?;
        let effective_max = if max_depth == 0 { 10 } else { max_depth };

        #[derive(Clone)]
        struct PathElem {
            node: StoreNode,
        }

        let initial = vec![PathElem {
            node: target.clone(),
        }];
        let mut complete_paths = Vec::new();
        if target.is_entry_point {
            complete_paths.push(initial.clone());
        }

        let mut queue = vec![(initial, 0usize)];
        let mut max_depth_reached = false;
        let mut truncated_paths = 0usize;

        while let Some((path, depth)) = queue.pop() {
            if depth >= effective_max {
                max_depth_reached = true;
                continue;
            }
            let Some(current) = path.last() else {
                continue;
            };
            let callers =
                direct_callers_for_tuple(&conn, &current.node.file, &current.node.symbol)?;
            if callers.is_empty() {
                if path.len() > 1 {
                    truncated_paths += 1;
                }
                continue;
            }

            let mut has_new_path = false;
            for site in callers {
                if path.iter().any(|elem| {
                    elem.node.file == site.caller.file && elem.node.symbol == site.caller.symbol
                }) {
                    continue;
                }
                has_new_path = true;
                let mut new_path = path.clone();
                new_path.push(PathElem {
                    node: site.caller.clone(),
                });
                if site.caller.is_entry_point {
                    complete_paths.push(new_path.clone());
                }
                queue.push((new_path, depth + 1));
            }
            if !has_new_path && path.len() > 1 {
                truncated_paths += 1;
            }
        }

        let mut paths: Vec<callgraph::TracePath> = complete_paths
            .into_iter()
            .map(|mut elems| {
                elems.reverse();
                let hops = elems
                    .iter()
                    .enumerate()
                    .map(|(index, elem)| callgraph::TraceHop {
                        symbol: elem.node.symbol.clone(),
                        file: elem.node.file.clone(),
                        line: elem.node.line,
                        signature: elem.node.signature.clone(),
                        is_entry_point: index == 0 && elem.node.is_entry_point,
                    })
                    .collect();
                callgraph::TracePath { hops }
            })
            .collect();
        paths.sort_by(|left, right| {
            let left_entry = left
                .hops
                .first()
                .map(|hop| hop.symbol.as_str())
                .unwrap_or("");
            let right_entry = right
                .hops
                .first()
                .map(|hop| hop.symbol.as_str())
                .unwrap_or("");
            left_entry
                .cmp(right_entry)
                .then(left.hops.len().cmp(&right.hops.len()))
        });
        let entry_points_found = paths
            .iter()
            .filter_map(|path| path.hops.first())
            .filter(|hop| hop.is_entry_point)
            .map(|hop| (hop.file.clone(), hop.symbol.clone()))
            .collect::<HashSet<_>>()
            .len();

        Ok(callgraph::TraceToResult {
            target_symbol: target.symbol,
            target_file: target.file,
            total_paths: paths.len(),
            paths,
            entry_points_found,
            max_depth_reached,
            truncated_paths,
        })
    }

    pub fn trace_to_symbol_candidates(
        &self,
        to_symbol: &str,
    ) -> Result<Vec<callgraph::TraceToSymbolCandidate>> {
        self.refresh_read_marker()?;
        let conn = self.conn.lock().expect("callgraph store mutex poisoned");
        self.ensure_ready(&conn)?;
        let mut candidates_by_file: HashMap<String, u32> = HashMap::new();
        for node in nodes_matching_symbol(&conn, to_symbol)? {
            candidates_by_file
                .entry(node.file)
                .and_modify(|line| *line = (*line).min(node.line))
                .or_insert(node.line);
        }
        let mut candidates: Vec<_> = candidates_by_file
            .into_iter()
            .map(|(file, line)| callgraph::TraceToSymbolCandidate { file, line })
            .collect();
        candidates
            .sort_by(|left, right| left.file.cmp(&right.file).then(left.line.cmp(&right.line)));
        Ok(candidates)
    }

    pub fn trace_to_symbol(
        &self,
        file_rel: &Path,
        symbol: &str,
        to_symbol: &str,
        to_file: Option<&Path>,
        max_depth: usize,
    ) -> Result<callgraph::TraceToSymbolResult> {
        let origin = self.node_for(file_rel, symbol)?;
        let target_file = to_file
            .map(|path| normalize_file_path(&self.project_root, path))
            .transpose()?
            .map(|path| relative_path(&self.project_root, &path));
        let conn = self.conn.lock().expect("callgraph store mutex poisoned");
        self.ensure_ready(&conn)?;
        let effective_max = if max_depth == 0 {
            10
        } else {
            max_depth.min(16)
        };

        let start_hop = trace_to_symbol_hop(&origin);
        if trace_to_symbol_matches_target(&origin, to_symbol, target_file.as_deref()) {
            return Ok(callgraph::TraceToSymbolResult {
                path: Some(vec![start_hop]),
                complete: true,
                reason: None,
            });
        }

        let mut queue = VecDeque::new();
        queue.push_back((origin.clone(), vec![start_hop], 0usize));
        let mut visited = HashSet::new();
        visited.insert((origin.file.clone(), origin.symbol.clone()));
        let mut max_depth_exhausted = false;

        while let Some((current, path, depth)) = queue.pop_front() {
            let callees = outgoing_calls_for_node(&conn, &current)?
                .into_iter()
                .filter_map(|site| site.target)
                .collect::<Vec<_>>();

            if depth >= effective_max {
                if callees
                    .iter()
                    .any(|node| !visited.contains(&(node.file.clone(), node.symbol.clone())))
                {
                    max_depth_exhausted = true;
                }
                continue;
            }

            for callee in callees {
                if !visited.insert((callee.file.clone(), callee.symbol.clone())) {
                    continue;
                }
                let mut next_path = path.clone();
                next_path.push(trace_to_symbol_hop(&callee));
                if trace_to_symbol_matches_target(&callee, to_symbol, target_file.as_deref()) {
                    return Ok(callgraph::TraceToSymbolResult {
                        path: Some(next_path),
                        complete: true,
                        reason: None,
                    });
                }
                queue.push_back((callee, next_path, depth + 1));
            }
        }

        if max_depth_exhausted {
            Ok(callgraph::TraceToSymbolResult {
                path: None,
                complete: false,
                reason: Some("max_depth_exhausted".to_string()),
            })
        } else {
            Ok(callgraph::TraceToSymbolResult {
                path: None,
                complete: true,
                reason: Some("no_path_found".to_string()),
            })
        }
    }
}

impl ReadonlyCallGraphStore {
    fn from_inner(inner: CallGraphStore) -> Self {
        Self { inner }
    }

    pub fn project_root(&self) -> &Path {
        self.inner.project_root()
    }

    pub fn project_key(&self) -> &str {
        self.inner.project_key()
    }

    pub fn sqlite_path(&self) -> &Path {
        self.inner.sqlite_path()
    }

    /// Report the open generation handle. SQLite-owned allocations are measured
    /// once by the process-wide SQLite allocator counters.
    pub fn estimated_memory(&self) -> crate::memory::MemoryEstimate {
        crate::memory::MemoryEstimate::partial(0).count("open_generation_handles", 1)
    }

    /// Whether this reader is temporarily serving a legacy harness partition.
    pub fn is_legacy_fallback(&self) -> bool {
        self.inner.is_legacy_fallback()
    }

    pub fn is_current(&self) -> bool {
        self.inner.is_current()
    }

    pub fn edge_snapshot(&self) -> Result<BTreeSet<StoredEdge>> {
        self.inner.edge_snapshot()
    }

    pub fn indexed_file_count(&self) -> Result<usize> {
        self.inner.indexed_file_count()
    }

    pub fn node_for(&self, file_rel: &Path, symbol: &str) -> Result<StoreNode> {
        self.inner.node_for(file_rel, symbol)
    }

    pub fn nodes_for(&self, file_rel: &Path, symbol: &str) -> Result<Vec<StoreNode>> {
        self.inner.nodes_for(file_rel, symbol)
    }

    pub fn nodes_matching(&self, symbol: &str) -> Result<Vec<StoreNode>> {
        self.inner.nodes_matching(symbol)
    }

    pub fn direct_callers_of(&self, file_rel: &Path, symbol: &str) -> Result<Vec<StoreCallSite>> {
        self.inner.direct_callers_of(file_rel, symbol)
    }

    pub fn direct_caller_counts_of(
        &self,
        targets: &[(String, String)],
    ) -> Result<HashMap<(String, String), usize>> {
        self.inner.direct_caller_counts_of(targets)
    }

    pub fn callers_of(
        &self,
        file_rel: &Path,
        symbol: &str,
        depth: usize,
    ) -> Result<StoreCallersResult> {
        self.inner.callers_of(file_rel, symbol, depth)
    }

    pub fn impact_of(
        &self,
        file_rel: &Path,
        symbol: &str,
        depth: usize,
    ) -> Result<StoreImpactResult> {
        self.inner.impact_of(file_rel, symbol, depth)
    }

    pub fn outgoing_calls_of(&self, node: &StoreNode) -> Result<Vec<StoreCallSite>> {
        self.inner.outgoing_calls_of(node)
    }

    pub fn outgoing_calls_for_symbols(
        &self,
        sources: &[(String, String)],
    ) -> Result<HashMap<(String, String), Vec<StoreCallSite>>> {
        self.inner.outgoing_calls_for_symbols(sources)
    }

    pub fn resolved_self_calls_of(&self, node: &StoreNode) -> Result<Vec<StoreCallSite>> {
        self.inner.resolved_self_calls_of(node)
    }

    pub fn unresolved_calls_of(&self, node: &StoreNode) -> Result<Vec<StoreUnresolvedCall>> {
        self.inner.unresolved_calls_of(node)
    }

    pub fn call_tree(
        &self,
        file_rel: &Path,
        symbol: &str,
        depth: usize,
    ) -> Result<callgraph::CallTreeNode> {
        self.inner.call_tree(file_rel, symbol, depth)
    }

    pub fn trace_to(
        &self,
        file_rel: &Path,
        symbol: &str,
        max_depth: usize,
    ) -> Result<callgraph::TraceToResult> {
        self.inner.trace_to(file_rel, symbol, max_depth)
    }

    pub fn trace_to_symbol_candidates(
        &self,
        to_symbol: &str,
    ) -> Result<Vec<TraceToSymbolCandidate>> {
        self.inner.trace_to_symbol_candidates(to_symbol)
    }

    pub fn trace_to_symbol(
        &self,
        file_rel: &Path,
        symbol: &str,
        to_symbol: &str,
        to_file: Option<&Path>,
        max_depth: usize,
    ) -> Result<callgraph::TraceToSymbolResult> {
        self.inner
            .trace_to_symbol(file_rel, symbol, to_symbol, to_file, max_depth)
    }
}

impl CallGraphRead for CallGraphStore {
    fn project_root(&self) -> &Path {
        CallGraphStore::project_root(self)
    }
    fn project_key(&self) -> &str {
        CallGraphStore::project_key(self)
    }
    fn sqlite_path(&self) -> &Path {
        CallGraphStore::sqlite_path(self)
    }
    fn is_current(&self) -> bool {
        CallGraphStore::is_current(self)
    }
    fn edge_snapshot(&self) -> Result<BTreeSet<StoredEdge>> {
        CallGraphStore::edge_snapshot(self)
    }
    fn indexed_file_count(&self) -> Result<usize> {
        CallGraphStore::indexed_file_count(self)
    }
    fn node_for(&self, file_rel: &Path, symbol: &str) -> Result<StoreNode> {
        CallGraphStore::node_for(self, file_rel, symbol)
    }
    fn nodes_for(&self, file_rel: &Path, symbol: &str) -> Result<Vec<StoreNode>> {
        CallGraphStore::nodes_for(self, file_rel, symbol)
    }
    fn nodes_matching(&self, symbol: &str) -> Result<Vec<StoreNode>> {
        CallGraphStore::nodes_matching(self, symbol)
    }
    fn direct_callers_of(&self, file_rel: &Path, symbol: &str) -> Result<Vec<StoreCallSite>> {
        CallGraphStore::direct_callers_of(self, file_rel, symbol)
    }
    fn direct_caller_counts_of(
        &self,
        targets: &[(String, String)],
    ) -> Result<HashMap<(String, String), usize>> {
        CallGraphStore::direct_caller_counts_of(self, targets)
    }
    fn callers_of(
        &self,
        file_rel: &Path,
        symbol: &str,
        depth: usize,
    ) -> Result<StoreCallersResult> {
        CallGraphStore::callers_of(self, file_rel, symbol, depth)
    }
    fn impact_of(&self, file_rel: &Path, symbol: &str, depth: usize) -> Result<StoreImpactResult> {
        CallGraphStore::impact_of(self, file_rel, symbol, depth)
    }
    fn outgoing_calls_of(&self, node: &StoreNode) -> Result<Vec<StoreCallSite>> {
        CallGraphStore::outgoing_calls_of(self, node)
    }
    fn outgoing_calls_for_symbols(
        &self,
        sources: &[(String, String)],
    ) -> Result<HashMap<(String, String), Vec<StoreCallSite>>> {
        CallGraphStore::outgoing_calls_for_symbols(self, sources)
    }
    fn resolved_self_calls_of(&self, node: &StoreNode) -> Result<Vec<StoreCallSite>> {
        CallGraphStore::resolved_self_calls_of(self, node)
    }
    fn unresolved_calls_of(&self, node: &StoreNode) -> Result<Vec<StoreUnresolvedCall>> {
        CallGraphStore::unresolved_calls_of(self, node)
    }
    fn call_tree(
        &self,
        file_rel: &Path,
        symbol: &str,
        depth: usize,
    ) -> Result<callgraph::CallTreeNode> {
        CallGraphStore::call_tree(self, file_rel, symbol, depth)
    }
    fn trace_to(
        &self,
        file_rel: &Path,
        symbol: &str,
        max_depth: usize,
    ) -> Result<callgraph::TraceToResult> {
        CallGraphStore::trace_to(self, file_rel, symbol, max_depth)
    }
    fn trace_to_symbol_candidates(&self, to_symbol: &str) -> Result<Vec<TraceToSymbolCandidate>> {
        CallGraphStore::trace_to_symbol_candidates(self, to_symbol)
    }
    fn trace_to_symbol(
        &self,
        file_rel: &Path,
        symbol: &str,
        to_symbol: &str,
        to_file: Option<&Path>,
        max_depth: usize,
    ) -> Result<callgraph::TraceToSymbolResult> {
        CallGraphStore::trace_to_symbol(self, file_rel, symbol, to_symbol, to_file, max_depth)
    }
}

impl<T: CallGraphRead + ?Sized> CallGraphRead for Arc<T> {
    fn project_root(&self) -> &Path {
        (**self).project_root()
    }
    fn project_key(&self) -> &str {
        (**self).project_key()
    }
    fn sqlite_path(&self) -> &Path {
        (**self).sqlite_path()
    }
    fn is_current(&self) -> bool {
        (**self).is_current()
    }
    fn edge_snapshot(&self) -> Result<BTreeSet<StoredEdge>> {
        (**self).edge_snapshot()
    }
    fn indexed_file_count(&self) -> Result<usize> {
        (**self).indexed_file_count()
    }
    fn node_for(&self, file_rel: &Path, symbol: &str) -> Result<StoreNode> {
        (**self).node_for(file_rel, symbol)
    }
    fn nodes_for(&self, file_rel: &Path, symbol: &str) -> Result<Vec<StoreNode>> {
        (**self).nodes_for(file_rel, symbol)
    }
    fn nodes_matching(&self, symbol: &str) -> Result<Vec<StoreNode>> {
        (**self).nodes_matching(symbol)
    }
    fn direct_callers_of(&self, file_rel: &Path, symbol: &str) -> Result<Vec<StoreCallSite>> {
        (**self).direct_callers_of(file_rel, symbol)
    }
    fn direct_caller_counts_of(
        &self,
        targets: &[(String, String)],
    ) -> Result<HashMap<(String, String), usize>> {
        (**self).direct_caller_counts_of(targets)
    }
    fn callers_of(
        &self,
        file_rel: &Path,
        symbol: &str,
        depth: usize,
    ) -> Result<StoreCallersResult> {
        (**self).callers_of(file_rel, symbol, depth)
    }
    fn impact_of(&self, file_rel: &Path, symbol: &str, depth: usize) -> Result<StoreImpactResult> {
        (**self).impact_of(file_rel, symbol, depth)
    }
    fn outgoing_calls_of(&self, node: &StoreNode) -> Result<Vec<StoreCallSite>> {
        (**self).outgoing_calls_of(node)
    }
    fn outgoing_calls_for_symbols(
        &self,
        sources: &[(String, String)],
    ) -> Result<HashMap<(String, String), Vec<StoreCallSite>>> {
        (**self).outgoing_calls_for_symbols(sources)
    }
    fn resolved_self_calls_of(&self, node: &StoreNode) -> Result<Vec<StoreCallSite>> {
        (**self).resolved_self_calls_of(node)
    }
    fn unresolved_calls_of(&self, node: &StoreNode) -> Result<Vec<StoreUnresolvedCall>> {
        (**self).unresolved_calls_of(node)
    }
    fn call_tree(
        &self,
        file_rel: &Path,
        symbol: &str,
        depth: usize,
    ) -> Result<callgraph::CallTreeNode> {
        (**self).call_tree(file_rel, symbol, depth)
    }
    fn trace_to(
        &self,
        file_rel: &Path,
        symbol: &str,
        max_depth: usize,
    ) -> Result<callgraph::TraceToResult> {
        (**self).trace_to(file_rel, symbol, max_depth)
    }
    fn trace_to_symbol_candidates(&self, to_symbol: &str) -> Result<Vec<TraceToSymbolCandidate>> {
        (**self).trace_to_symbol_candidates(to_symbol)
    }
    fn trace_to_symbol(
        &self,
        file_rel: &Path,
        symbol: &str,
        to_symbol: &str,
        to_file: Option<&Path>,
        max_depth: usize,
    ) -> Result<callgraph::TraceToSymbolResult> {
        (**self).trace_to_symbol(file_rel, symbol, to_symbol, to_file, max_depth)
    }
}

impl CallGraphRead for ReadonlyCallGraphStore {
    fn project_root(&self) -> &Path {
        self.project_root()
    }
    fn project_key(&self) -> &str {
        self.project_key()
    }
    fn sqlite_path(&self) -> &Path {
        self.sqlite_path()
    }
    fn is_current(&self) -> bool {
        self.is_current()
    }
    fn edge_snapshot(&self) -> Result<BTreeSet<StoredEdge>> {
        self.edge_snapshot()
    }
    fn indexed_file_count(&self) -> Result<usize> {
        self.indexed_file_count()
    }
    fn node_for(&self, file_rel: &Path, symbol: &str) -> Result<StoreNode> {
        self.node_for(file_rel, symbol)
    }
    fn nodes_for(&self, file_rel: &Path, symbol: &str) -> Result<Vec<StoreNode>> {
        self.nodes_for(file_rel, symbol)
    }
    fn nodes_matching(&self, symbol: &str) -> Result<Vec<StoreNode>> {
        self.nodes_matching(symbol)
    }
    fn direct_callers_of(&self, file_rel: &Path, symbol: &str) -> Result<Vec<StoreCallSite>> {
        self.direct_callers_of(file_rel, symbol)
    }
    fn direct_caller_counts_of(
        &self,
        targets: &[(String, String)],
    ) -> Result<HashMap<(String, String), usize>> {
        self.direct_caller_counts_of(targets)
    }
    fn callers_of(
        &self,
        file_rel: &Path,
        symbol: &str,
        depth: usize,
    ) -> Result<StoreCallersResult> {
        self.callers_of(file_rel, symbol, depth)
    }
    fn impact_of(&self, file_rel: &Path, symbol: &str, depth: usize) -> Result<StoreImpactResult> {
        self.impact_of(file_rel, symbol, depth)
    }
    fn outgoing_calls_of(&self, node: &StoreNode) -> Result<Vec<StoreCallSite>> {
        self.outgoing_calls_of(node)
    }
    fn outgoing_calls_for_symbols(
        &self,
        sources: &[(String, String)],
    ) -> Result<HashMap<(String, String), Vec<StoreCallSite>>> {
        self.outgoing_calls_for_symbols(sources)
    }
    fn resolved_self_calls_of(&self, node: &StoreNode) -> Result<Vec<StoreCallSite>> {
        self.resolved_self_calls_of(node)
    }
    fn unresolved_calls_of(&self, node: &StoreNode) -> Result<Vec<StoreUnresolvedCall>> {
        self.unresolved_calls_of(node)
    }
    fn call_tree(
        &self,
        file_rel: &Path,
        symbol: &str,
        depth: usize,
    ) -> Result<callgraph::CallTreeNode> {
        self.call_tree(file_rel, symbol, depth)
    }
    fn trace_to(
        &self,
        file_rel: &Path,
        symbol: &str,
        max_depth: usize,
    ) -> Result<callgraph::TraceToResult> {
        self.trace_to(file_rel, symbol, max_depth)
    }
    fn trace_to_symbol_candidates(&self, to_symbol: &str) -> Result<Vec<TraceToSymbolCandidate>> {
        self.trace_to_symbol_candidates(to_symbol)
    }
    fn trace_to_symbol(
        &self,
        file_rel: &Path,
        symbol: &str,
        to_symbol: &str,
        to_file: Option<&Path>,
        max_depth: usize,
    ) -> Result<callgraph::TraceToSymbolResult> {
        self.trace_to_symbol(file_rel, symbol, to_symbol, to_file, max_depth)
    }
}

fn indexed_file_count(conn: &Connection) -> Result<usize> {
    let count: i64 = conn.query_row("SELECT COUNT(*) FROM files", [], |row| row.get(0))?;
    Ok(count.max(0) as usize)
}

fn resolve_node_for_rel(conn: &Connection, rel_path: &str, symbol: &str) -> Result<StoreNode> {
    let candidates = nodes_for_file_matching_symbol(conn, rel_path, symbol)?;
    match candidates.as_slice() {
        [candidate] => Ok(candidate.clone()),
        [] => Err(AftError::SymbolNotFound {
            name: symbol.to_string(),
            file: rel_path.to_string(),
        }
        .into()),
        _ => Err(AftError::AmbiguousSymbol {
            name: symbol.to_string(),
            candidates: candidates
                .iter()
                .map(|candidate| candidate.symbol.clone())
                .collect(),
        }
        .into()),
    }
}

fn nodes_for_file_matching_symbol(
    conn: &Connection,
    rel_path: &str,
    symbol: &str,
) -> Result<Vec<StoreNode>> {
    let qualified_query = symbol.contains("::");
    let sql = if qualified_query {
        "SELECT n.id, n.file_path, n.scoped_name, n.name, n.kind, n.start_line, n.end_line,
                n.signature, n.exported, n.is_callgraph_entry_point, f.lang
         FROM nodes n JOIN files f ON f.path = n.file_path
         WHERE n.file_path = ?1 AND n.scoped_name = ?2
         ORDER BY n.scoped_name, n.start_line, n.start_col"
    } else {
        "SELECT n.id, n.file_path, n.scoped_name, n.name, n.kind, n.start_line, n.end_line,
                n.signature, n.exported, n.is_callgraph_entry_point, f.lang
         FROM nodes n JOIN files f ON f.path = n.file_path
         WHERE n.file_path = ?1 AND (n.scoped_name = ?2 OR n.name = ?2)
         ORDER BY n.scoped_name, n.start_line, n.start_col"
    };
    let mut stmt = conn.prepare(sql)?;
    let rows = stmt.query_map(params![rel_path, symbol], store_node_from_row)?;
    rows.collect::<std::result::Result<Vec<_>, _>>()
        .map_err(Into::into)
}

fn nodes_matching_symbol(conn: &Connection, symbol: &str) -> Result<Vec<StoreNode>> {
    let qualified_query = symbol.contains("::");
    let sql = if qualified_query {
        "SELECT n.id, n.file_path, n.scoped_name, n.name, n.kind, n.start_line, n.end_line,
                n.signature, n.exported, n.is_callgraph_entry_point, f.lang
         FROM nodes n JOIN files f ON f.path = n.file_path
         WHERE n.scoped_name = ?1
         ORDER BY n.file_path, n.scoped_name, n.start_line, n.start_col"
    } else {
        "SELECT n.id, n.file_path, n.scoped_name, n.name, n.kind, n.start_line, n.end_line,
                n.signature, n.exported, n.is_callgraph_entry_point, f.lang
         FROM nodes n JOIN files f ON f.path = n.file_path
         WHERE n.scoped_name = ?1 OR n.name = ?1
         ORDER BY n.file_path, n.scoped_name, n.start_line, n.start_col"
    };
    let mut stmt = conn.prepare(sql)?;
    let rows = stmt.query_map(params![symbol], store_node_from_row)?;
    rows.collect::<std::result::Result<Vec<_>, _>>()
        .map_err(Into::into)
}

fn store_node_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<StoreNode> {
    store_node_from_row_at(row, 0)
}

fn store_node_from_row_at(row: &rusqlite::Row<'_>, offset: usize) -> rusqlite::Result<StoreNode> {
    let start_line: u32 = row.get::<_, i64>(offset + 5)?.max(0) as u32;
    let end_line: u32 = row.get::<_, i64>(offset + 6)?.max(0) as u32;
    let lang_label_value: String = row.get(offset + 10)?;
    Ok(StoreNode {
        node_id: row.get(offset)?,
        file: row.get(offset + 1)?,
        symbol: row.get(offset + 2)?,
        name: row.get(offset + 3)?,
        kind: row.get(offset + 4)?,
        line: start_line.saturating_add(1),
        end_line: end_line.saturating_add(1),
        signature: row.get(offset + 7)?,
        exported: row.get::<_, i64>(offset + 8)? != 0,
        is_entry_point: row.get::<_, i64>(offset + 9)? != 0,
        lang: lang_from_label(&lang_label_value).unwrap_or(LangId::TypeScript),
    })
}

fn optional_store_node_from_row_at(
    row: &rusqlite::Row<'_>,
    offset: usize,
) -> rusqlite::Result<Option<StoreNode>> {
    if row.get::<_, Option<String>>(offset)?.is_some() {
        store_node_from_row_at(row, offset).map(Some)
    } else {
        Ok(None)
    }
}

#[allow(clippy::too_many_arguments)]
fn collect_callers_recursive(
    conn: &Connection,
    file: &str,
    symbol: &str,
    max_depth: usize,
    current_depth: usize,
    visited: &mut HashSet<(String, String)>,
    result: &mut Vec<StoreCallSite>,
    depth_limited: &mut bool,
    truncated: &mut usize,
) -> Result<()> {
    if current_depth >= max_depth {
        let omitted = direct_caller_count_for_tuple(conn, file, symbol)?;
        if omitted > 0 {
            *depth_limited = true;
            *truncated += omitted;
        }
        return Ok(());
    }

    if !visited.insert((file.to_string(), symbol.to_string())) {
        return Ok(());
    }

    let sites = direct_callers_for_tuple(conn, file, symbol)?;
    for site in sites {
        result.push(site.clone());
        if current_depth + 1 < max_depth {
            collect_callers_recursive(
                conn,
                &site.caller.file,
                &site.caller.symbol,
                max_depth,
                current_depth + 1,
                visited,
                result,
                depth_limited,
                truncated,
            )?;
        } else {
            let omitted =
                direct_caller_count_for_tuple(conn, &site.caller.file, &site.caller.symbol)?;
            if omitted > 0 {
                *depth_limited = true;
                *truncated += omitted;
            }
        }
    }
    Ok(())
}

// Each target uses two parameters; 499 stays below SQLite's legacy 999-variable limit.
const DIRECT_CALLER_COUNT_BATCH_SIZE: usize = 499;

fn direct_caller_counts_for_tuples(
    conn: &Connection,
    targets: &[(String, String)],
) -> Result<HashMap<(String, String), usize>> {
    let unique_targets = targets.iter().cloned().collect::<BTreeSet<_>>();
    let mut counts = unique_targets
        .iter()
        .cloned()
        .map(|target| (target, 0usize))
        .collect::<HashMap<_, _>>();

    let unique_targets = unique_targets.into_iter().collect::<Vec<_>>();
    for chunk in unique_targets.chunks(DIRECT_CALLER_COUNT_BATCH_SIZE) {
        let requested_values = (0..chunk.len())
            .map(|_| "(?, ?)")
            .collect::<Vec<_>>()
            .join(", ");
        let sql = format!(
            "WITH requested(target_file, target_symbol) AS (VALUES {requested_values}),
             deduped AS (
                 SELECT e.target_file, e.target_symbol, src.file_path AS caller_file, e.line
                 FROM requested requested
                 JOIN edges e
                   ON e.target_file = requested.target_file
                  AND e.target_symbol = requested.target_symbol
                  AND e.kind = 'call'
                 JOIN refs r ON r.ref_id = e.ref_id
                 JOIN nodes src ON src.id = e.source_node
                 JOIN files src_file ON src_file.path = src.file_path
                 GROUP BY e.target_file, e.target_symbol, src.file_path, e.line
             )
             SELECT target_file, target_symbol, COUNT(*)
             FROM deduped
             GROUP BY target_file, target_symbol"
        );
        let bindings = chunk
            .iter()
            .flat_map(|(file, symbol)| [file.as_str(), symbol.as_str()]);
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(params_from_iter(bindings), |row| {
            Ok((
                (row.get::<_, String>(0)?, row.get::<_, String>(1)?),
                row.get::<_, i64>(2)?,
            ))
        })?;
        for row in rows {
            let (target, count) = row?;
            counts.insert(target, usize::try_from(count).unwrap_or(usize::MAX));
        }
    }

    Ok(counts)
}

fn direct_caller_count_for_tuple(
    conn: &Connection,
    target_file: &str,
    target_symbol: &str,
) -> Result<usize> {
    let count: i64 = conn.query_row(
        "SELECT COUNT(*)
         FROM edges e
         JOIN refs r ON r.ref_id = e.ref_id
         JOIN nodes src ON src.id = e.source_node
         JOIN files src_file ON src_file.path = src.file_path
         WHERE e.kind = 'call' AND e.target_file = ?1 AND e.target_symbol = ?2",
        params![target_file, target_symbol],
        |row| row.get(0),
    )?;
    Ok(usize::try_from(count).unwrap_or(usize::MAX))
}

fn direct_callers_for_tuple(
    conn: &Connection,
    target_file: &str,
    target_symbol: &str,
) -> Result<Vec<StoreCallSite>> {
    let mut stmt = conn.prepare(
        "SELECT e.target_file, e.target_symbol, e.line,
                r.byte_start, r.byte_end, r.status, e.provenance,
                src.id, src.file_path, src.scoped_name, src.name, src.kind, src.start_line,
                src.end_line, src.signature, src.exported, src.is_callgraph_entry_point,
                src_file.lang,
                tgt.id, tgt.file_path, tgt.scoped_name, tgt.name, tgt.kind, tgt.start_line,
                tgt.end_line, tgt.signature, tgt.exported, tgt.is_callgraph_entry_point,
                tgt_file.lang
         FROM edges e
         JOIN refs r ON r.ref_id = e.ref_id
         JOIN nodes src ON src.id = e.source_node
         JOIN files src_file ON src_file.path = src.file_path
         LEFT JOIN (nodes tgt JOIN files tgt_file ON tgt_file.path = tgt.file_path)
             ON tgt.id = e.target_node
         WHERE e.kind = 'call' AND e.target_file = ?1 AND e.target_symbol = ?2
         ORDER BY e.source_node, r.byte_start, r.line, r.ref_id",
    )?;
    let rows = stmt.query_map(params![target_file, target_symbol], |row| {
        let caller = store_node_from_row_at(row, 7)?;
        let target = optional_store_node_from_row_at(row, 18)?;
        Ok(StoreCallSite {
            caller,
            target_file: row.get(0)?,
            target_symbol: row.get(1)?,
            target,
            line: row.get::<_, i64>(2)?.max(0) as u32,
            byte_start: row.get::<_, i64>(3)?.max(0) as usize,
            byte_end: row.get::<_, i64>(4)?.max(0) as usize,
            resolved: row.get::<_, String>(5)? == "resolved",
            provenance: row.get(6)?,
        })
    })?;
    rows.collect::<std::result::Result<Vec<_>, _>>()
        .map_err(Into::into)
}

// Each symbol uses two parameters; 499 stays below SQLite's legacy 999-variable limit.
const OUTGOING_SYMBOL_BATCH_SIZE: usize = 499;
// Outgoing-edge batches bind one source node per parameter.
const OUTGOING_NODE_BATCH_SIZE: usize = 999;

fn outgoing_calls_for_symbol_tuples(
    conn: &Connection,
    sources: &[(String, String)],
) -> Result<HashMap<(String, String), Vec<StoreCallSite>>> {
    let unique_sources = sources.iter().cloned().collect::<BTreeSet<_>>();
    let unique_sources = unique_sources.into_iter().collect::<Vec<_>>();
    let source_nodes_by_symbol = nodes_for_symbol_tuples(conn, &unique_sources)?;
    let source_nodes = unique_sources
        .iter()
        .flat_map(|source| source_nodes_by_symbol.get(source).into_iter().flatten())
        .cloned()
        .collect::<Vec<_>>();
    let source_nodes_by_id = source_nodes
        .iter()
        .cloned()
        .map(|node| (node.node_id.clone(), node))
        .collect::<HashMap<_, _>>();
    let mut calls_by_node: HashMap<String, Vec<StoreCallSite>> = HashMap::new();

    for chunk in source_nodes.chunks(OUTGOING_NODE_BATCH_SIZE) {
        let placeholders = (0..chunk.len()).map(|_| "?").collect::<Vec<_>>().join(", ");
        let sql = format!(
            "SELECT e.source_node,
                    e.target_file, e.target_symbol, e.line,
                    r.byte_start, r.byte_end, r.status, e.provenance,
                    CASE WHEN tgt_file.lang IS NULL THEN NULL ELSE tgt.id END,
                    tgt.file_path, tgt.scoped_name, tgt.name, tgt.kind, tgt.start_line,
                    tgt.end_line, tgt.signature, tgt.exported, tgt.is_callgraph_entry_point,
                    tgt_file.lang
             FROM edges e
             JOIN refs r ON r.ref_id = e.ref_id
             LEFT JOIN nodes tgt ON tgt.id = e.target_node
             LEFT JOIN files tgt_file ON tgt_file.path = tgt.file_path
             WHERE e.kind = 'call' AND e.source_node IN ({placeholders})
             ORDER BY e.source_node, r.byte_start, r.line, r.ref_id"
        );
        let bindings = chunk.iter().map(|node| node.node_id.as_str());
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(params_from_iter(bindings), |row| {
            let source_node_id = row.get::<_, String>(0)?;
            let caller = source_nodes_by_id
                .get(&source_node_id)
                .expect("batched outgoing row belongs to a requested source node")
                .clone();
            let target = optional_store_node_from_row_at(row, 8)?;
            Ok((
                source_node_id,
                StoreCallSite {
                    caller,
                    target_file: row.get(1)?,
                    target_symbol: row.get(2)?,
                    target,
                    line: row.get::<_, i64>(3)?.max(0) as u32,
                    byte_start: row.get::<_, i64>(4)?.max(0) as usize,
                    byte_end: row.get::<_, i64>(5)?.max(0) as usize,
                    resolved: row.get::<_, String>(6)? == "resolved",
                    provenance: row.get(7)?,
                },
            ))
        })?;
        for row in rows {
            let (source_node_id, call) = row?;
            calls_by_node.entry(source_node_id).or_default().push(call);
        }
    }

    let mut calls_by_source = HashMap::new();
    for source in &unique_sources {
        let mut calls = Vec::new();
        if let Some(nodes) = source_nodes_by_symbol.get(source) {
            for node in nodes {
                if let Some(node_calls) = calls_by_node.remove(&node.node_id) {
                    calls.extend(node_calls);
                }
            }
        }
        calls_by_source.insert(source.clone(), calls);
    }

    // Resolve each logical target once for the whole frontier. Keeping this separate
    // preserves positional-symbol representatives without a correlated lookup per edge.
    let target_tuples = calls_by_source
        .values()
        .flatten()
        .map(|call| (call.target_file.clone(), call.target_symbol.clone()))
        .collect::<Vec<_>>();
    let target_nodes = nodes_for_symbol_tuples(conn, &target_tuples)?;
    for calls in calls_by_source.values_mut() {
        for call in calls {
            if let Some(target) = target_nodes
                .get(&(call.target_file.clone(), call.target_symbol.clone()))
                .and_then(|nodes| nodes.first())
            {
                call.target = Some(target.clone());
            }
        }
    }

    Ok(calls_by_source)
}

fn nodes_for_symbol_tuples(
    conn: &Connection,
    symbols: &[(String, String)],
) -> Result<HashMap<(String, String), Vec<StoreNode>>> {
    let unique_symbols = symbols.iter().cloned().collect::<BTreeSet<_>>();
    let mut nodes_by_symbol = unique_symbols
        .iter()
        .cloned()
        .map(|symbol| (symbol, Vec::new()))
        .collect::<HashMap<_, _>>();
    let unique_symbols = unique_symbols.into_iter().collect::<Vec<_>>();

    for chunk in unique_symbols.chunks(OUTGOING_SYMBOL_BATCH_SIZE) {
        let requested_values = (0..chunk.len())
            .map(|_| "(?, ?)")
            .collect::<Vec<_>>()
            .join(", ");
        let sql = format!(
            "WITH requested(file, symbol) AS (VALUES {requested_values})
             SELECT requested.file, requested.symbol,
                    node.id, node.file_path, node.scoped_name, node.name, node.kind,
                    node.start_line, node.end_line, node.signature, node.exported,
                    node.is_callgraph_entry_point, node_file.lang
             FROM requested
             JOIN nodes node INDEXED BY idx_nodes_file
               ON node.file_path = requested.file
              AND node.scoped_name = requested.symbol
             JOIN files node_file ON node_file.path = node.file_path
             ORDER BY requested.file, requested.symbol,
                      node.scoped_name, node.start_line, node.end_line,
                      node.start_col, node.range_ordinal"
        );
        let bindings = chunk
            .iter()
            .flat_map(|(file, symbol)| [file.as_str(), symbol.as_str()]);
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(params_from_iter(bindings), |row| {
            Ok((
                (row.get::<_, String>(0)?, row.get::<_, String>(1)?),
                store_node_from_row_at(row, 2)?,
            ))
        })?;
        for row in rows {
            let (symbol, node) = row?;
            nodes_by_symbol.entry(symbol).or_default().push(node);
        }
    }

    Ok(nodes_by_symbol)
}

fn outgoing_calls_for_node(conn: &Connection, node: &StoreNode) -> Result<Vec<StoreCallSite>> {
    let mut stmt = conn.prepare(
        "SELECT e.target_file, e.target_symbol, e.line,
                r.byte_start, r.byte_end, r.status, e.provenance,
                tgt.id, tgt.file_path, tgt.scoped_name, tgt.name, tgt.kind, tgt.start_line,
                tgt.end_line, tgt.signature, tgt.exported, tgt.is_callgraph_entry_point,
                tgt_file.lang
         FROM edges e
         JOIN refs r ON r.ref_id = e.ref_id
         LEFT JOIN (nodes tgt JOIN files tgt_file ON tgt_file.path = tgt.file_path)
             ON tgt.id = e.target_node
         WHERE e.kind = 'call' AND e.source_node = ?1
         ORDER BY r.byte_start, r.line, r.ref_id",
    )?;
    let rows = stmt.query_map(params![node.node_id], |row| {
        let target = optional_store_node_from_row_at(row, 7)?;
        Ok(StoreCallSite {
            caller: node.clone(),
            target_file: row.get(0)?,
            target_symbol: row.get(1)?,
            target,
            line: row.get::<_, i64>(2)?.max(0) as u32,
            byte_start: row.get::<_, i64>(3)?.max(0) as usize,
            byte_end: row.get::<_, i64>(4)?.max(0) as usize,
            resolved: row.get::<_, String>(5)? == "resolved",
            provenance: row.get(6)?,
        })
    })?;
    rows.collect::<std::result::Result<Vec<_>, _>>()
        .map_err(Into::into)
}

fn resolved_self_calls_for_node(conn: &Connection, node: &StoreNode) -> Result<Vec<StoreCallSite>> {
    let mut stmt = conn.prepare(
        "SELECT r.target_file, r.target_symbol, r.line,
                r.byte_start, r.byte_end, r.status, r.provenance,
                tgt.id, tgt.file_path, tgt.scoped_name, tgt.name, tgt.kind, tgt.start_line,
                tgt.end_line, tgt.signature, tgt.exported, tgt.is_callgraph_entry_point,
                tgt_file.lang
         FROM refs r
         LEFT JOIN (nodes tgt JOIN files tgt_file ON tgt_file.path = tgt.file_path)
             ON tgt.id = r.target_node
         WHERE r.caller_node = ?1
           AND r.kind = 'call'
           AND r.status <> 'unresolved'
           AND r.target_file = ?2
           AND r.target_symbol = ?3
           AND r.provenance = ?4
           AND NOT EXISTS (
               SELECT 1 FROM edges e WHERE e.ref_id = r.ref_id AND e.kind = 'call'
           )
         ORDER BY r.byte_start, r.line, r.ref_id",
    )?;
    let rows = stmt.query_map(
        params![
            &node.node_id,
            &node.file,
            &node.symbol,
            PROVENANCE_TREESITTER
        ],
        |row| {
            let target = optional_store_node_from_row_at(row, 7)?;
            Ok(StoreCallSite {
                caller: node.clone(),
                target_file: row.get(0)?,
                target_symbol: row.get(1)?,
                target,
                line: row.get::<_, i64>(2)?.max(0) as u32,
                byte_start: row.get::<_, i64>(3)?.max(0) as usize,
                byte_end: row.get::<_, i64>(4)?.max(0) as usize,
                resolved: row.get::<_, String>(5)? == "resolved",
                provenance: row.get(6)?,
            })
        },
    )?;
    rows.collect::<std::result::Result<Vec<_>, _>>()
        .map_err(Into::into)
}

fn unresolved_calls_for_node(
    conn: &Connection,
    node: &StoreNode,
) -> Result<Vec<StoreUnresolvedCall>> {
    let mut stmt = conn.prepare(
        "SELECT COALESCE(short_name, full_ref, ''), full_ref, line, byte_start, byte_end
         FROM refs
         WHERE caller_node = ?1
           AND kind = 'call'
           AND status = 'unresolved'
           AND NOT EXISTS (
               SELECT 1 FROM edges e WHERE e.ref_id = refs.ref_id AND e.kind = 'call'
           )
         ORDER BY byte_start, line, ref_id",
    )?;
    let rows = stmt.query_map(params![node.node_id], |row| {
        Ok(StoreUnresolvedCall {
            caller: node.clone(),
            symbol: row.get(0)?,
            full_ref: row.get(1)?,
            line: row.get::<_, i64>(2)?.max(0) as u32,
            byte_start: row.get::<_, i64>(3)?.max(0) as usize,
            byte_end: row.get::<_, i64>(4)?.max(0) as usize,
        })
    })?;
    rows.collect::<std::result::Result<Vec<_>, _>>()
        .map_err(Into::into)
}

fn forward_calls_for_node(conn: &Connection, node: &StoreNode) -> Result<Vec<StoreForwardCall>> {
    let mut calls = Vec::new();
    calls.extend(
        outgoing_calls_for_node(conn, node)?
            .into_iter()
            .map(StoreForwardCall::Resolved),
    );
    calls.extend(
        unresolved_calls_for_node(conn, node)?
            .into_iter()
            .map(StoreForwardCall::Unresolved),
    );
    calls.sort_by(|left, right| {
        left.byte_start()
            .cmp(&right.byte_start())
            .then(left.line().cmp(&right.line()))
    });
    Ok(calls)
}

fn forward_call_count_for_node(conn: &Connection, node: &StoreNode) -> Result<usize> {
    let resolved_count: i64 = conn.query_row(
        "SELECT COUNT(*)
         FROM edges e
         JOIN refs r ON r.ref_id = e.ref_id
         WHERE e.kind = 'call' AND e.source_node = ?1",
        params![&node.node_id],
        |row| row.get(0),
    )?;
    let unresolved_count: i64 = conn.query_row(
        "SELECT COUNT(*)
         FROM refs
         WHERE caller_node = ?1
           AND kind = 'call'
           AND status = 'unresolved'
           AND NOT EXISTS (
               SELECT 1 FROM edges e WHERE e.ref_id = refs.ref_id AND e.kind = 'call'
           )",
        params![&node.node_id],
        |row| row.get(0),
    )?;
    let total = resolved_count.saturating_add(unresolved_count);
    Ok(usize::try_from(total).unwrap_or(usize::MAX))
}

fn call_tree_inner(
    conn: &Connection,
    node: &StoreNode,
    max_depth: usize,
    current_depth: usize,
    visited: &mut HashSet<(String, String)>,
) -> Result<callgraph::CallTreeNode> {
    let visit_key = (node.file.clone(), node.symbol.clone());
    if visited.contains(&visit_key) {
        return Ok(callgraph::CallTreeNode {
            name: node.symbol.clone(),
            file: node.file.clone(),
            line: node.line,
            signature: node.signature.clone(),
            resolved: true,
            children: Vec::new(),
            depth_limited: false,
            truncated: 0,
        });
    }
    visited.insert(visit_key.clone());

    let mut children = Vec::new();
    let mut depth_limited = false;
    let mut truncated = 0usize;

    if current_depth < max_depth {
        let calls = forward_calls_for_node(conn, node)?;
        for call in calls {
            match call {
                StoreForwardCall::Resolved(site) => {
                    if let Some(target) = site.target {
                        let child =
                            call_tree_inner(conn, &target, max_depth, current_depth + 1, visited)?;
                        depth_limited |= child.depth_limited;
                        truncated += child.truncated;
                        children.push(child);
                    } else {
                        children.push(callgraph::CallTreeNode {
                            name: site.target_symbol,
                            file: site.target_file,
                            line: site.line,
                            signature: None,
                            resolved: false,
                            children: Vec::new(),
                            depth_limited: false,
                            truncated: 0,
                        });
                    }
                }
                StoreForwardCall::Unresolved(call) => {
                    children.push(callgraph::CallTreeNode {
                        name: call.symbol,
                        file: call.caller.file,
                        line: call.line,
                        signature: None,
                        resolved: false,
                        children: Vec::new(),
                        depth_limited: false,
                        truncated: 0,
                    });
                }
            }
        }
    } else {
        truncated = forward_call_count_for_node(conn, node)?;
        depth_limited = truncated > 0;
    }

    visited.remove(&visit_key);
    Ok(callgraph::CallTreeNode {
        name: node.symbol.clone(),
        file: node.file.clone(),
        line: node.line,
        signature: node.signature.clone(),
        resolved: true,
        children,
        depth_limited,
        truncated,
    })
}

fn trace_to_symbol_hop(node: &StoreNode) -> callgraph::TraceToSymbolHop {
    callgraph::TraceToSymbolHop {
        symbol: node.symbol.clone(),
        file: node.file.clone(),
        line: node.line,
    }
}

fn trace_to_symbol_matches_target(
    node: &StoreNode,
    to_symbol: &str,
    to_file: Option<&str>,
) -> bool {
    if !symbol_query_matches(&node.symbol, to_symbol) {
        return false;
    }
    match to_file {
        Some(file) => node.file == file,
        None => true,
    }
}

fn symbol_query_matches(symbol: &str, query: &str) -> bool {
    symbol == query || unqualified_name(symbol) == query
}

fn read_trimmed_source_lines(path: &Path) -> Option<Vec<String>> {
    let source = std::fs::read_to_string(path).ok()?;
    Some(source.lines().map(|line| line.trim().to_string()).collect())
}

#[doc(hidden)]
pub fn live_callgraph_edge_snapshot(
    project_root: &Path,
    files: &[PathBuf],
) -> Result<BTreeSet<StoredEdge>> {
    let files = normalize_file_list(project_root, files)?;
    let mut graph = callgraph::CallGraph::new(project_root.to_path_buf());
    let mut file_data = Vec::new();
    for file in &files {
        let canon = canonicalize_path(file);
        let data = graph.build_file(&canon)?.clone();
        file_data.push((canon, data));
    }

    let mut edges = BTreeSet::new();
    for (caller_file, data) in &file_data {
        for (caller_symbol, call_sites) in &data.calls_by_symbol {
            for call_site in call_sites {
                let resolution = graph.resolve_cross_file_edge(
                    &call_site.full_callee,
                    &call_site.callee_name,
                    caller_file,
                    &data.import_block,
                );
                let (target_file, target_symbol) = match resolution {
                    EdgeResolution::Resolved { file, symbol } => (file, symbol),
                    EdgeResolution::Unresolved { callee_name } => {
                        if !callgraph::is_bare_callee(&call_site.full_callee, &callee_name) {
                            continue;
                        }
                        let Ok(target_symbol) = callgraph::resolve_symbol_query_in_data(
                            data,
                            caller_file,
                            &callee_name,
                        ) else {
                            continue;
                        };
                        (caller_file.clone(), target_symbol)
                    }
                };
                if target_file == *caller_file && target_symbol == *caller_symbol {
                    continue;
                }
                edges.insert(StoredEdge {
                    source_file: relative_path(project_root, caller_file),
                    source_symbol: caller_symbol.clone(),
                    target_file: relative_path(project_root, &target_file),
                    target_symbol,
                    kind: "call".to_string(),
                    line: call_site.line,
                });
            }
        }
    }
    Ok(edges)
}

fn rebuild_cooldown_records() -> &'static Mutex<HashMap<RebuildCooldownKey, RebuildCooldownRecord>>
{
    SUCCESSFUL_REBUILDS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn rebuild_cooldown_key(callgraph_dir: &Path, project_key: &str) -> RebuildCooldownKey {
    RebuildCooldownKey {
        callgraph_dir: std::fs::canonicalize(callgraph_dir)
            .unwrap_or_else(|_| callgraph_dir.to_path_buf()),
        project_key: project_key.to_string(),
    }
}

fn rebuild_cooldown_denial(
    callgraph_dir: &Path,
    project_key: &str,
    project_root: &Path,
    now: Instant,
) -> Option<(PathBuf, Duration)> {
    let key = rebuild_cooldown_key(callgraph_dir, project_key);
    let records = rebuild_cooldown_records()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let record = records.get(&key)?;
    if record.project_root == project_root || !record.cross_root_cooldown_armed {
        return None;
    }
    let elapsed = now.saturating_duration_since(record.published_at);
    (elapsed < REBUILD_COOLDOWN).then(|| (record.project_root.clone(), REBUILD_COOLDOWN - elapsed))
}

fn record_successful_rebuild(
    callgraph_dir: &Path,
    project_key: &str,
    project_root: &Path,
    published_at: Instant,
) {
    let key = rebuild_cooldown_key(callgraph_dir, project_key);
    let mut records = rebuild_cooldown_records()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if records.len() >= 4_096 && !records.contains_key(&key) {
        if let Some(evict) = records.keys().next().cloned() {
            records.remove(&evict);
        }
    }
    let cross_root_cooldown_armed = records.get(&key).is_some_and(|previous| {
        previous.cross_root_cooldown_armed || previous.project_root != project_root
    });
    records.insert(
        key,
        RebuildCooldownRecord {
            project_root: project_root.to_path_buf(),
            published_at,
            cross_root_cooldown_armed,
        },
    );
}

fn acquire_writer_lease(
    callgraph_dir: &Path,
    project_key: &str,
    project_root: &Path,
) -> Result<Option<Arc<crate::root_cache::WriterLease>>> {
    crate::root_cache::WriterLease::acquire_shared(
        crate::root_cache::RootCacheDomain::Callgraph,
        callgraph_dir,
        project_key,
        project_root,
    )
    .map_err(CallGraphStoreError::from)
}

fn verify_writer_lease(lease: &crate::root_cache::WriterLease) -> Result<()> {
    if lease.verify()? {
        Ok(())
    } else {
        Err(CallGraphStoreError::Unavailable(format!(
            "callgraph writer lease for key {} lost epoch {}; aborting write",
            lease.key(),
            lease.epoch()
        )))
    }
}

fn legacy_migration_completion_line(
    project_key: &str,
    method: &str,
    legacy_bytes: u64,
    migrated_bytes: u64,
) -> String {
    format!(
        "migrated root-keyed callgraph store key={project_key} method={method} legacy={legacy_bytes} migrated={migrated_bytes}"
    )
}

fn log_legacy_migration_completion(
    project_key: &str,
    method: &str,
    legacy_bytes: u64,
    migrated_bytes: u64,
) {
    crate::slog_info!(
        "{}",
        legacy_migration_completion_line(project_key, method, legacy_bytes, migrated_bytes)
    );
}

fn try_legacy_migration_or_fallback(
    callgraph_dir: &Path,
    project_root: &Path,
    project_key: &str,
    writer_lease: Arc<crate::root_cache::WriterLease>,
) -> Result<Option<CallGraphStore>> {
    let partitions = legacy_callgraph_partitions(callgraph_dir, project_key)?;
    if partitions.is_empty() {
        return Ok(None);
    }

    for partition in &partitions {
        if let Some(source) = newest_superseded_legacy_generation(partition)? {
            if !migration_disk_floor_allows(&source, callgraph_dir)? {
                return open_legacy_fallback_store(
                    callgraph_dir,
                    project_root,
                    project_key,
                    &partitions,
                );
            }
            match publish_generation_copy_migration(
                callgraph_dir,
                project_key,
                &source,
                Arc::clone(&writer_lease),
            ) {
                Ok(published) => {
                    log_legacy_migration_completion(
                        project_key,
                        "generation_copy",
                        source.source_bytes,
                        published.migrated_bytes,
                    );
                    return CallGraphStore::open_generation(
                        callgraph_dir,
                        project_root.to_path_buf(),
                        project_key.to_string(),
                        published.generation,
                        writer_lease,
                    )
                    .map(Some);
                }
                Err(error) => {
                    crate::slog_warn!(
                        "root-keyed callgraph generation-copy migration failed from {}: {}",
                        source.sqlite_path.display(),
                        error
                    );
                    return open_legacy_fallback_store(
                        callgraph_dir,
                        project_root,
                        project_key,
                        &partitions,
                    );
                }
            }
        }

        if let Some(source) = current_legacy_generation(partition)? {
            if !migration_disk_floor_allows(&source, callgraph_dir)? {
                return open_legacy_fallback_store(
                    callgraph_dir,
                    project_root,
                    project_key,
                    &partitions,
                );
            }
            match publish_backup_migration(
                callgraph_dir,
                project_key,
                &source,
                Arc::clone(&writer_lease),
            ) {
                Ok(published) => {
                    log_legacy_migration_completion(
                        project_key,
                        "sqlite_backup",
                        source.source_bytes,
                        published.migrated_bytes,
                    );
                    return CallGraphStore::open_generation(
                        callgraph_dir,
                        project_root.to_path_buf(),
                        project_key.to_string(),
                        published.generation,
                        writer_lease,
                    )
                    .map(Some);
                }
                Err(error) => {
                    crate::slog_warn!(
                        "root-keyed callgraph backup migration failed from {}: {}",
                        source.sqlite_path.display(),
                        error
                    );
                    return open_legacy_fallback_store(
                        callgraph_dir,
                        project_root,
                        project_key,
                        &partitions,
                    );
                }
            }
        }
    }

    open_legacy_fallback_store(callgraph_dir, project_root, project_key, &partitions)
}

fn open_legacy_fallback_store(
    callgraph_dir: &Path,
    project_root: &Path,
    project_key: &str,
    partitions: &[LegacyCallgraphPartition],
) -> Result<Option<CallGraphStore>> {
    let Some(target) = first_ready_legacy_target(partitions)? else {
        return Ok(None);
    };
    crate::slog_warn!(
        "root-keyed callgraph migration unavailable; serving read-only fallback from legacy {} partition {}",
        target.partition.harness,
        target.sqlite_path.display()
    );
    let conn = open_readonly_connection(&target.sqlite_path)?;
    if !database_ready(&conn).unwrap_or(false) {
        return Ok(None);
    }
    let marker_label = legacy_read_marker_label(&target.sqlite_path, target.generation.as_deref());
    let read_marker = crate::root_cache::ReadMarker::create(callgraph_dir, &marker_label)?;
    Ok(Some(CallGraphStore::from_connection(
        project_root.to_path_buf(),
        project_key.to_string(),
        target.sqlite_path,
        callgraph_dir.to_path_buf(),
        true,
        target.generation,
        None,
        Some(read_marker),
        conn,
    )))
}

fn migration_disk_floor_allows(
    source: &LegacyCallgraphTarget,
    callgraph_dir: &Path,
) -> Result<bool> {
    let available = migration_available_disk(callgraph_dir)?;
    let decision = crate::legacy_partitions::evaluate_root_keyed_copy_disk_floor(
        source.source_bytes,
        available,
    );
    if decision.should_skip_copy() {
        crate::slog_warn!(
            "{}",
            decision.warning_message(&source.sqlite_path, callgraph_dir)
        );
        return Ok(false);
    }
    Ok(true)
}

fn migration_available_disk(path: &Path) -> Result<u64> {
    if let Some(bytes) = MIGRATION_AVAILABLE_DISK_OVERRIDE.with(|slot| *slot.borrow()) {
        return Ok(bytes);
    }
    crate::legacy_partitions::available_disk_for(path).map_err(CallGraphStoreError::from)
}

fn legacy_callgraph_partitions(
    callgraph_dir: &Path,
    project_key: &str,
) -> Result<Vec<LegacyCallgraphPartition>> {
    let Some(storage_root) = root_storage_dir(callgraph_dir) else {
        return Ok(Vec::new());
    };
    let inventory = crate::legacy_partitions::inventory_legacy_partitions(&storage_root)?;
    let mut partitions = inventory
        .into_iter()
        .filter(|entry| {
            entry.kind == crate::legacy_partitions::LegacyPartitionKind::Callgraph
                && entry.key == project_key
        })
        .map(|entry| {
            let dir = if entry.path.is_dir() {
                entry.path.clone()
            } else {
                entry
                    .path
                    .parent()
                    .map(Path::to_path_buf)
                    .unwrap_or_else(|| entry.path.clone())
            };
            LegacyCallgraphPartition {
                harness: entry.harness,
                dir,
                key: entry.key,
                bytes: entry.bytes,
                freshness: entry.callgraph_pointer_mtime,
            }
        })
        .collect::<Vec<_>>();
    partitions.sort_by(|left, right| {
        right
            .freshness
            .cmp(&left.freshness)
            .then_with(|| right.bytes.cmp(&left.bytes))
            .then_with(|| left.harness.cmp(&right.harness))
    });
    Ok(partitions)
}

fn root_storage_dir(callgraph_dir: &Path) -> Option<PathBuf> {
    let domain_dir = callgraph_dir.parent()?;
    if domain_dir.file_name().and_then(|name| name.to_str()) != Some("callgraph") {
        return None;
    }
    domain_dir.parent().map(Path::to_path_buf)
}

pub(crate) fn all_legacy_partitions_migrated_for_keys(
    callgraph_dir: &Path,
    configured_keys: &BTreeSet<String>,
) -> Result<bool> {
    let Some(storage_root) = root_storage_dir(callgraph_dir) else {
        return Ok(false);
    };
    let legacy_keys = crate::legacy_partitions::inventory_legacy_partitions(&storage_root)?
        .into_iter()
        .filter(|entry| {
            entry.kind == crate::legacy_partitions::LegacyPartitionKind::Callgraph
                && configured_keys.contains(&entry.key)
        })
        .map(|entry| entry.key)
        .collect::<BTreeSet<_>>();
    if legacy_keys.is_empty() {
        return Ok(false);
    }

    for key in legacy_keys {
        let migrated_dir = storage_root.join("callgraph").join(&key);
        let Some(generation) = read_pointer(&migrated_dir, &key) else {
            return Ok(false);
        };
        if !migration_generation_requires_manifest(&generation)
            || !migration_manifest_valid(&migrated_dir, &generation)
        {
            return Ok(false);
        }
    }
    Ok(true)
}

fn newest_superseded_legacy_generation(
    partition: &LegacyCallgraphPartition,
) -> Result<Option<LegacyCallgraphTarget>> {
    let Some(current) = read_pointer(&partition.dir, &partition.key) else {
        return Ok(None);
    };
    let prefix = format!("{}.g", partition.key);
    let Ok(entries) = std::fs::read_dir(&partition.dir) else {
        return Ok(None);
    };
    let mut candidates = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if name == current
            || name.contains(".tmp.")
            || !name.starts_with(&prefix)
            || !name.ends_with(".sqlite")
        {
            continue;
        }
        let path = entry.path();
        if !db_path_ready(&path) {
            continue;
        }
        let modified = entry
            .metadata()
            .and_then(|metadata| metadata.modified())
            .unwrap_or(SystemTime::UNIX_EPOCH);
        candidates.push((modified, path, name));
    }
    candidates.sort_by(|left, right| right.0.cmp(&left.0));
    let Some((_modified, sqlite_path, generation)) = candidates.into_iter().next() else {
        return Ok(None);
    };
    let source_bytes = sqlite_file_set_size(&sqlite_path)?;
    Ok(Some(LegacyCallgraphTarget {
        partition: partition.clone(),
        sqlite_path,
        generation: Some(generation),
        source_bytes,
        source_blake3: String::new(),
    }))
}

fn current_legacy_generation(
    partition: &LegacyCallgraphPartition,
) -> Result<Option<LegacyCallgraphTarget>> {
    let Some(target) = ready_legacy_target(partition)? else {
        return Ok(None);
    };
    let has_superseded = newest_superseded_legacy_generation(partition)?.is_some();
    if has_superseded {
        return Ok(None);
    }
    Ok(Some(target))
}

fn freshest_legacy_fallback_target(
    callgraph_dir: &Path,
    project_key: &str,
) -> Result<Option<LegacyCallgraphTarget>> {
    let partitions = legacy_callgraph_partitions(callgraph_dir, project_key)?;
    first_ready_legacy_target(&partitions)
}

fn first_ready_legacy_target(
    partitions: &[LegacyCallgraphPartition],
) -> Result<Option<LegacyCallgraphTarget>> {
    for partition in partitions {
        if let Some(target) = ready_legacy_target(partition)? {
            return Ok(Some(target));
        }
    }
    Ok(None)
}

fn ready_legacy_target(
    partition: &LegacyCallgraphPartition,
) -> Result<Option<LegacyCallgraphTarget>> {
    if let Some(generation) = read_pointer(&partition.dir, &partition.key) {
        let sqlite_path = partition.dir.join(&generation);
        if sqlite_path.is_file() && db_path_ready(&sqlite_path) {
            let source_bytes = sqlite_file_set_size(&sqlite_path)?;
            return Ok(Some(LegacyCallgraphTarget {
                partition: partition.clone(),
                sqlite_path,
                generation: Some(generation),
                source_bytes,
                source_blake3: String::new(),
            }));
        }
    }

    let sqlite_path = legacy_sqlite_path(&partition.dir, &partition.key);
    if sqlite_path.is_file() && db_path_ready(&sqlite_path) {
        let source_bytes = sqlite_file_set_size(&sqlite_path)?;
        return Ok(Some(LegacyCallgraphTarget {
            partition: partition.clone(),
            sqlite_path,
            generation: None,
            source_bytes,
            source_blake3: String::new(),
        }));
    }
    Ok(None)
}

fn publish_generation_copy_migration(
    callgraph_dir: &Path,
    project_key: &str,
    source: &LegacyCallgraphTarget,
    writer_lease: Arc<crate::root_cache::WriterLease>,
) -> Result<PublishedLegacyMigration> {
    let generation = migration_generation_file_name(project_key, "copy");
    let temp_path = migration_temp_path(callgraph_dir, &generation);
    remove_sqlite_file_set(&temp_path);
    copy_sqlite_file_set(&source.sqlite_path, &temp_path)?;
    fail_after_temp_copy_for_test()?;

    let mut source = source.clone();
    let fingerprint = sqlite_file_set_fingerprint(&temp_path)?;
    source.source_blake3 = fingerprint.blake3;
    let generation = publish_migrated_generation(
        callgraph_dir,
        project_key,
        &generation,
        &temp_path,
        &source,
        fingerprint.bytes,
        writer_lease,
        "generation_copy",
    )?;
    Ok(PublishedLegacyMigration {
        generation,
        migrated_bytes: fingerprint.bytes,
    })
}

fn publish_backup_migration(
    callgraph_dir: &Path,
    project_key: &str,
    source: &LegacyCallgraphTarget,
    writer_lease: Arc<crate::root_cache::WriterLease>,
) -> Result<PublishedLegacyMigration> {
    if MIGRATION_FORCE_BACKUP_BUDGET_EXHAUSTED.with(|slot| slot.get()) {
        return Err(CallGraphStoreError::Unavailable(
            "legacy callgraph backup migration budget exhausted by test seam".to_string(),
        ));
    }

    let generation = migration_generation_file_name(project_key, "backup");
    let temp_path = migration_temp_path(callgraph_dir, &generation);
    remove_sqlite_file_set(&temp_path);

    let source_conn = open_readonly_connection(&source.sqlite_path)?;
    let mut destination = Connection::open(&temp_path)?;
    destination.busy_timeout(Duration::from_secs(5))?;
    let backup = rusqlite::backup::Backup::new(&source_conn, &mut destination)?;
    let started = Instant::now();
    let mut retries = 0;
    loop {
        match backup.step(MIGRATION_BACKUP_PAGES_PER_STEP)? {
            rusqlite::backup::StepResult::Done => break,
            rusqlite::backup::StepResult::More => std::thread::sleep(Duration::from_millis(5)),
            rusqlite::backup::StepResult::Busy | rusqlite::backup::StepResult::Locked => {
                retries += 1;
                if retries > MIGRATION_BACKUP_RETRY_BUDGET
                    || started.elapsed() > MIGRATION_BACKUP_WALL_CLOCK_BUDGET
                {
                    return Err(CallGraphStoreError::Unavailable(format!(
                        "legacy callgraph backup migration exceeded retry/wall-clock budget after {retries} retries"
                    )));
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            _ => {
                return Err(CallGraphStoreError::Unavailable(
                    "legacy callgraph backup returned an unknown step result".to_string(),
                ));
            }
        }
    }
    drop(backup);

    let integrity: String =
        destination.query_row("PRAGMA integrity_check", [], |row| row.get(0))?;
    if integrity != "ok" {
        return Err(CallGraphStoreError::Unavailable(format!(
            "legacy callgraph backup produced a database that failed integrity_check: {integrity}"
        )));
    }
    if !database_ready(&destination)? {
        return Err(CallGraphStoreError::Unavailable(
            "legacy callgraph backup produced a database without ready metadata".to_string(),
        ));
    }
    destination.execute_batch("PRAGMA optimize;")?;
    drop(destination);
    sync_file(&temp_path)?;
    fail_after_temp_copy_for_test()?;

    let mut source = source.clone();
    let fingerprint = sqlite_file_set_fingerprint(&temp_path)?;
    source.source_blake3 = fingerprint.blake3;
    let generation = publish_migrated_generation(
        callgraph_dir,
        project_key,
        &generation,
        &temp_path,
        &source,
        fingerprint.bytes,
        writer_lease,
        "sqlite_backup",
    )?;
    Ok(PublishedLegacyMigration {
        generation,
        migrated_bytes: fingerprint.bytes,
    })
}

fn publish_migrated_generation(
    callgraph_dir: &Path,
    project_key: &str,
    generation: &str,
    temp_path: &Path,
    source: &LegacyCallgraphTarget,
    migrated_bytes: u64,
    writer_lease: Arc<crate::root_cache::WriterLease>,
    method: &str,
) -> Result<String> {
    let gen_path = callgraph_dir.join(generation);
    checkpoint_sqlite_before_publication(temp_path);
    let publication = publish_if_current(|| {
        verify_writer_lease(&writer_lease)?;
        remove_sqlite_file_set(&gen_path);
        rename_sqlite_file_set(temp_path, &gen_path)?;
        crate::fs_lock::sync_parent(&gen_path);

        verify_writer_lease(&writer_lease)?;
        publish_pointer(callgraph_dir, project_key, generation)?;
        write_migration_manifest(callgraph_dir, generation, source, migrated_bytes, method)?;
        Ok(generation.to_string())
    });
    if matches!(publication, Err(CallGraphStoreError::Superseded)) {
        remove_sqlite_file_set(temp_path);
    }
    publication
}

fn copy_sqlite_file_set(source: &Path, destination: &Path) -> Result<()> {
    if let Some(parent) = destination.parent() {
        std::fs::create_dir_all(parent)?;
    }
    for suffix in SQLITE_FILE_SET_SUFFIXES {
        let source_path = sqlite_file_set_path(source, suffix);
        if !source_path.is_file() {
            continue;
        }
        let destination_path = sqlite_file_set_path(destination, suffix);
        std::fs::copy(&source_path, &destination_path)?;
        sync_file(&destination_path)?;
    }
    Ok(())
}

fn rename_sqlite_file_set(source: &Path, destination: &Path) -> Result<()> {
    for suffix in SQLITE_FILE_SET_SUFFIXES {
        let source_path = sqlite_file_set_path(source, suffix);
        if !source_path.exists() {
            continue;
        }
        let destination_path = sqlite_file_set_path(destination, suffix);
        if let Err(error) = crate::fs_lock::rename_over(&source_path, &destination_path) {
            let _ = std::fs::remove_file(&source_path);
            return Err(error.into());
        }
    }
    Ok(())
}

fn sqlite_file_set_size(path: &Path) -> Result<u64> {
    let mut bytes = 0_u64;
    for suffix in SQLITE_FILE_SET_SUFFIXES {
        let member = sqlite_file_set_path(path, suffix);
        if !member.is_file() {
            continue;
        }
        bytes = bytes.saturating_add(member.metadata()?.len());
    }
    Ok(bytes)
}

fn sqlite_file_set_fingerprint(path: &Path) -> Result<SourceFingerprint> {
    let mut hasher = blake3::Hasher::new();
    let mut bytes = 0_u64;
    let mut buffer = [0_u8; 64 * 1024];
    for suffix in SQLITE_FILE_SET_SUFFIXES {
        let member = sqlite_file_set_path(path, suffix);
        if !member.is_file() {
            continue;
        }
        hasher.update(suffix.as_bytes());
        let mut file = std::fs::File::open(&member)?;
        loop {
            let read = file.read(&mut buffer)?;
            if read == 0 {
                break;
            }
            bytes = bytes.saturating_add(read as u64);
            hasher.update(&buffer[..read]);
        }
    }
    Ok(SourceFingerprint {
        bytes,
        blake3: hash_to_hex(hasher.finalize()),
    })
}

fn sqlite_file_set_path(path: &Path, suffix: &str) -> PathBuf {
    if suffix.is_empty() {
        path.to_path_buf()
    } else {
        PathBuf::from(format!("{}{suffix}", path.display()))
    }
}

fn sync_file(path: &Path) -> Result<()> {
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)?;
    file.sync_all()?;
    Ok(())
}

fn fail_after_temp_copy_for_test() -> Result<()> {
    if MIGRATION_FAIL_AFTER_TEMP_COPY.with(|slot| slot.get()) {
        return Err(CallGraphStoreError::Unavailable(
            "legacy callgraph migration stopped after temp copy by test seam".to_string(),
        ));
    }
    Ok(())
}

fn migration_generation_file_name(project_key: &str, method: &str) -> String {
    format!(
        "{project_key}.g{}.{}{}{}.sqlite",
        now_nanos(),
        std::process::id(),
        MIGRATION_GENERATION_TAG,
        method
    )
}

fn migration_temp_path(callgraph_dir: &Path, generation: &str) -> PathBuf {
    callgraph_dir.join(format!(
        "{generation}.tmp.{}.{}",
        std::process::id(),
        now_nanos()
    ))
}

fn write_migration_manifest(
    callgraph_dir: &Path,
    generation: &str,
    source: &LegacyCallgraphTarget,
    migrated_bytes: u64,
    method: &str,
) -> Result<()> {
    let manifest_path = migration_manifest_path(callgraph_dir, generation);
    let temp_path = manifest_path.with_extension(format!(
        "migration.json.tmp.{}.{}",
        std::process::id(),
        now_nanos()
    ));
    let manifest = serde_json::json!({
        "version": MIGRATION_MANIFEST_VERSION,
        "method": method,
        "target_generation": generation,
        "source_harness": source.partition.harness,
        "source_path": source.sqlite_path.display().to_string(),
        "source_generation": source.generation,
        "source_bytes": source.source_bytes,
        "source_blake3": source.source_blake3,
        "migrated_bytes": migrated_bytes,
    });
    {
        use std::io::Write as _;
        let mut file = std::fs::File::create(&temp_path)?;
        file.write_all(serde_json::to_vec_pretty(&manifest)?.as_slice())?;
        file.write_all(b"\n")?;
        file.sync_all()?;
    }
    if let Err(error) = crate::fs_lock::rename_over(&temp_path, &manifest_path) {
        let _ = std::fs::remove_file(&temp_path);
        return Err(error.into());
    }
    crate::fs_lock::sync_parent(&manifest_path);
    Ok(())
}

fn migration_manifest_path(callgraph_dir: &Path, generation: &str) -> PathBuf {
    callgraph_dir.join(format!("{generation}.migration.json"))
}

fn migration_generation_requires_manifest(generation: &str) -> bool {
    generation.contains(MIGRATION_GENERATION_TAG)
}

fn migration_manifest_valid(callgraph_dir: &Path, generation: &str) -> bool {
    if !migration_generation_requires_manifest(generation) {
        return true;
    }
    let path = migration_manifest_path(callgraph_dir, generation);
    let Ok(bytes) = std::fs::read(path) else {
        return false;
    };
    let Ok(value) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
        return false;
    };
    value.get("version").and_then(serde_json::Value::as_u64)
        == Some(MIGRATION_MANIFEST_VERSION as u64)
        && value
            .get("target_generation")
            .and_then(serde_json::Value::as_str)
            == Some(generation)
        && value
            .get("source_bytes")
            .and_then(serde_json::Value::as_u64)
            .is_some_and(|bytes| bytes > 0)
        && value
            .get("source_blake3")
            .and_then(serde_json::Value::as_str)
            .is_some_and(|hash| hash.len() == 64)
}

fn cleanup_incomplete_migrations(callgraph_dir: &Path, project_key: &str) {
    let pointer_generation = read_pointer(callgraph_dir, project_key);
    if let Some(generation) = pointer_generation.as_deref() {
        if migration_generation_requires_manifest(generation)
            && !migration_manifest_valid(callgraph_dir, generation)
        {
            let path = callgraph_dir.join(generation);
            remove_sqlite_file_set(&path);
            let _ = std::fs::remove_file(migration_manifest_path(callgraph_dir, generation));
            let _ = std::fs::remove_file(pointer_path(callgraph_dir, project_key));
        }
    }

    let Ok(entries) = std::fs::read_dir(callgraph_dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        let path = entry.path();
        if name.contains(".tmp.") && name.starts_with(&format!("{project_key}.g")) {
            let _ = std::fs::remove_file(path);
            continue;
        }
        if name.starts_with(&format!("{project_key}.g"))
            && name.ends_with(".sqlite")
            && name.contains(MIGRATION_GENERATION_TAG)
            && pointer_generation.as_deref() != Some(&name)
            && !migration_manifest_valid(callgraph_dir, &name)
        {
            remove_sqlite_file_set(&path);
            let _ = std::fs::remove_file(migration_manifest_path(callgraph_dir, &name));
        }
    }
    crate::fs_lock::sync_parent(callgraph_dir);
}

fn legacy_read_marker_label(path: &Path, generation: Option<&str>) -> String {
    let mut hasher = blake3::Hasher::new();
    hasher.update(path.to_string_lossy().as_bytes());
    if let Some(generation) = generation {
        hasher.update(generation.as_bytes());
    }
    let digest = hash_to_hex(hasher.finalize());
    format!("legacy-{}", &digest[..16])
}

fn open_readonly_connection(path: &Path) -> Result<Connection> {
    let uri = sqlite_readonly_uri(path);
    let conn = Connection::open_with_flags(
        &uri,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI,
    )?;
    conn.pragma_update(
        None,
        "synchronous",
        if write_amplification_baseline_enabled() {
            "FULL"
        } else {
            "NORMAL"
        },
    )?;
    conn.busy_timeout(reader_busy_timeout())?;
    conn.execute_batch("PRAGMA query_only=ON;")?;
    Ok(conn)
}

fn reader_busy_timeout() -> Duration {
    let jitter = (now_nanos() % 500) as u64;
    Duration::from_millis(250 + jitter)
}

fn sqlite_readonly_uri(path: &Path) -> String {
    let raw = path.to_string_lossy().replace('\\', "/");
    let encoded = percent_encode_sqlite_uri_path(&raw);
    if raw.starts_with('/') {
        format!("file://{encoded}?mode=ro")
    } else if raw.as_bytes().get(1) == Some(&b':') {
        format!("file:///{encoded}?mode=ro")
    } else {
        format!("file:{encoded}?mode=ro")
    }
}

fn percent_encode_sqlite_uri_path(path: &str) -> String {
    let mut encoded = String::with_capacity(path.len());
    for byte in path.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' | b'/' | b':' => {
                encoded.push(byte as char)
            }
            _ => encoded.push_str(&format!("%{byte:02X}")),
        }
    }
    encoded
}

fn configure_connection(conn: &Connection) -> Result<()> {
    conn.pragma_update(None, "journal_mode", "WAL")?;
    let baseline = write_amplification_baseline_enabled();
    conn.pragma_update(
        None,
        "synchronous",
        if baseline { "FULL" } else { "NORMAL" },
    )?;
    conn.pragma_update(
        None,
        "wal_autocheckpoint",
        if baseline {
            1_000
        } else {
            CALLGRAPH_WAL_AUTOCHECKPOINT_PAGES
        },
    )?;
    conn.pragma_update(None, "busy_timeout", 5_000)?;
    Ok(())
}

fn configure_build_connection(conn: &Connection) -> Result<()> {
    conn.pragma_update(None, "journal_mode", "DELETE")?;
    conn.pragma_update(
        None,
        "synchronous",
        if write_amplification_baseline_enabled() {
            "FULL"
        } else {
            "NORMAL"
        },
    )?;
    conn.pragma_update(None, "busy_timeout", 5_000)?;
    Ok(())
}

/// A copied migration generation may carry a WAL sidecar. Checkpoint only the
/// private temporary copy before publishing it; a busy reader is harmless because
/// the next publication or cleanup pass can retry without affecting the source.
fn checkpoint_sqlite_before_publication(path: &Path) {
    let Ok(conn) = Connection::open(path) else {
        return;
    };
    let _ = conn.pragma_update(None, "synchronous", "NORMAL");
    let _ = conn.busy_timeout(Duration::from_secs(5));
    let _ = checkpoint_wal_truncate(&conn);
}

fn checkpoint_wal_truncate(conn: &Connection) -> bool {
    match conn.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |row| {
        row.get::<_, i64>(0)
    }) {
        Ok(0) => true,
        Ok(_) => false,
        Err(rusqlite::Error::SqliteFailure(error, _))
            if matches!(
                error.code,
                rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked
            ) =>
        {
            false
        }
        Err(error) => {
            log::debug!("callgraph WAL truncate checkpoint skipped: {error}");
            false
        }
    }
}

fn initialize_schema(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS files (
            path                TEXT PRIMARY KEY,
            content_hash        TEXT NOT NULL,
            mtime_ns            INTEGER NOT NULL,
            size                INTEGER NOT NULL,
            lang                TEXT NOT NULL,
            is_dead_code_root   INTEGER NOT NULL DEFAULT 0,
            is_public_api       INTEGER NOT NULL DEFAULT 0,
            surface_fingerprint TEXT NOT NULL,
            indexed_at          INTEGER NOT NULL
        );

        CREATE TABLE IF NOT EXISTS nodes (
            id                         TEXT PRIMARY KEY,
            file_path                  TEXT NOT NULL,
            name                       TEXT NOT NULL,
            scoped_name                TEXT NOT NULL,
            kind                       TEXT NOT NULL,
            start_line                 INTEGER NOT NULL,
            start_col                  INTEGER NOT NULL,
            end_line                   INTEGER NOT NULL,
            end_col                    INTEGER NOT NULL,
            range_ordinal              INTEGER NOT NULL,
            signature                  TEXT,
            exported                   INTEGER NOT NULL,
            is_default_export          INTEGER NOT NULL,
            is_type_like               INTEGER NOT NULL,
            is_callgraph_entry_point   INTEGER NOT NULL,
            provenance                 TEXT NOT NULL,
            UNIQUE(file_path, start_line, start_col, end_line, end_col, range_ordinal)
        );
        CREATE INDEX IF NOT EXISTS idx_nodes_file ON nodes(file_path);
        CREATE INDEX IF NOT EXISTS idx_nodes_name ON nodes(name);
        CREATE INDEX IF NOT EXISTS idx_nodes_scoped ON nodes(scoped_name);

        CREATE TABLE IF NOT EXISTS refs (
            ref_id          TEXT PRIMARY KEY,
            caller_node     TEXT,
            caller_file     TEXT NOT NULL,
            kind            TEXT NOT NULL,
            short_name      TEXT,
            full_ref        TEXT,
            module_path     TEXT,
            import_kind     TEXT,
            local_name      TEXT,
            requested_name  TEXT,
            namespace_alias TEXT,
            wildcard        INTEGER NOT NULL DEFAULT 0,
            line            INTEGER NOT NULL,
            byte_start      INTEGER NOT NULL,
            byte_end        INTEGER NOT NULL,
            status          TEXT NOT NULL,
            target_node     TEXT,
            target_file     TEXT,
            target_symbol   TEXT,
            provenance      TEXT NOT NULL
        );
        CREATE INDEX IF NOT EXISTS idx_refs_short_name ON refs(short_name);
        CREATE INDEX IF NOT EXISTS idx_refs_kind_caller_file ON refs(kind, caller_file);
        CREATE INDEX IF NOT EXISTS idx_refs_caller_file ON refs(caller_file);
        CREATE INDEX IF NOT EXISTS idx_refs_caller_node_kind ON refs(caller_node, kind, status);
        CREATE INDEX IF NOT EXISTS idx_refs_target_file ON refs(target_file);

        CREATE TABLE IF NOT EXISTS file_dependencies (
            file_path   TEXT NOT NULL,
            dep_file    TEXT NOT NULL,
            PRIMARY KEY(file_path, dep_file)
        );
        CREATE INDEX IF NOT EXISTS idx_file_dependencies_dep_file ON file_dependencies(dep_file);

        CREATE TABLE IF NOT EXISTS edges (
            edge_id       TEXT PRIMARY KEY,
            ref_id        TEXT NOT NULL,
            source_node   TEXT NOT NULL,
            target_node   TEXT,
            target_file   TEXT NOT NULL,
            target_symbol TEXT NOT NULL,
            kind          TEXT NOT NULL,
            line          INTEGER NOT NULL,
            provenance    TEXT NOT NULL
        );
        CREATE INDEX IF NOT EXISTS idx_edges_source_kind ON edges(source_node, kind);
        CREATE INDEX IF NOT EXISTS idx_edges_target_kind ON edges(target_node, kind);
        CREATE INDEX IF NOT EXISTS idx_edges_target_file_symbol ON edges(target_file, target_symbol, kind);
        CREATE INDEX IF NOT EXISTS idx_edges_ref_id ON edges(ref_id, kind);

        CREATE TABLE IF NOT EXISTS dispatch_hints (
            id           TEXT PRIMARY KEY,
            method_name  TEXT NOT NULL,
            caller_node  TEXT NOT NULL,
            file         TEXT NOT NULL,
            line         INTEGER NOT NULL,
            byte_start   INTEGER NOT NULL,
            byte_end     INTEGER NOT NULL,
            provenance   TEXT NOT NULL
        );
        CREATE INDEX IF NOT EXISTS idx_dispatch_hints_method ON dispatch_hints(method_name);

        CREATE TABLE IF NOT EXISTS type_ref_names (
            name TEXT PRIMARY KEY
        );

        CREATE TABLE IF NOT EXISTS backend_file_state (
            backend        TEXT NOT NULL,
            workspace_root TEXT NOT NULL,
            file_path      TEXT NOT NULL,
            content_hash   TEXT NOT NULL,
            status         TEXT NOT NULL,
            updated_at     INTEGER NOT NULL,
            PRIMARY KEY(backend, workspace_root, file_path, content_hash)
        );
        CREATE INDEX IF NOT EXISTS idx_backend_file_state_file ON backend_file_state(file_path, backend);

        CREATE TABLE IF NOT EXISTS meta (
            k TEXT PRIMARY KEY,
            v TEXT NOT NULL
        );",
    )?;
    insert_meta(conn)?;
    Ok(())
}

fn insert_meta(conn: &Connection) -> Result<()> {
    conn.execute(
        "INSERT OR REPLACE INTO meta(k, v) VALUES('schema_version', ?1)",
        params![SCHEMA_VERSION.to_string()],
    )?;
    conn.execute(
        "INSERT OR REPLACE INTO meta(k, v) VALUES('fingerprint', ?1)",
        params![schema_fingerprint()],
    )?;
    Ok(())
}

fn set_meta_ready(conn: &Connection, ready: bool) -> Result<()> {
    conn.execute(
        "INSERT OR REPLACE INTO meta(k, v) VALUES('ready', ?1)",
        params![if ready { "1" } else { "0" }],
    )?;
    Ok(())
}

fn database_ready(conn: &Connection) -> Result<bool> {
    let schema_version: Option<String> = conn
        .query_row("SELECT v FROM meta WHERE k = 'schema_version'", [], |row| {
            row.get(0)
        })
        .optional()?;
    let fingerprint: Option<String> = conn
        .query_row("SELECT v FROM meta WHERE k = 'fingerprint'", [], |row| {
            row.get(0)
        })
        .optional()?;
    let ready: Option<String> = conn
        .query_row("SELECT v FROM meta WHERE k = 'ready'", [], |row| row.get(0))
        .optional()?;

    let expected_schema = SCHEMA_VERSION.to_string();
    let expected_fingerprint = schema_fingerprint();
    Ok(schema_version.as_deref() == Some(expected_schema.as_str())
        && fingerprint.as_deref() == Some(expected_fingerprint.as_str())
        && ready.as_deref() == Some("1"))
}

fn ensure_database_ready(conn: &Connection) -> Result<()> {
    if database_ready(conn)? {
        Ok(())
    } else {
        Err(CallGraphStoreError::Unavailable(
            "database is missing, stale, or mid-build".to_string(),
        ))
    }
}

fn schema_fingerprint() -> String {
    // Bump the trailing content-version whenever the BUILD OUTPUT changes (new
    // edge sources, broader call extraction) even if the table SHAPE is
    // unchanged, so existing on-disk stores rebuild and pick up the new edges.
    // Rust scoped aliases, inline modules, reexports, and turbofish calls now add edges.
    let input =
        format!("callgraph_store:v{SCHEMA_VERSION}:positional:raw-ref:v9-rust-resolver-batch");
    hash_to_hex(blake3::hash(input.as_bytes()))
}

fn clear_tables(tx: &Transaction<'_>) -> Result<()> {
    tx.execute_batch(
        "DELETE FROM edges;
         DELETE FROM file_dependencies;
         DELETE FROM refs;
         DELETE FROM dispatch_hints;
         DELETE FROM type_ref_names;
         DELETE FROM backend_file_state;
         DELETE FROM nodes;
         DELETE FROM files;",
    )?;
    Ok(())
}

fn drop_cold_build_secondary_indexes(tx: &Transaction<'_>) -> Result<()> {
    tx.execute_batch(
        "DROP INDEX IF EXISTS idx_nodes_file;
         DROP INDEX IF EXISTS idx_nodes_name;
         DROP INDEX IF EXISTS idx_nodes_scoped;
         DROP INDEX IF EXISTS idx_refs_short_name;
         DROP INDEX IF EXISTS idx_refs_kind_caller_file;
         DROP INDEX IF EXISTS idx_refs_caller_file;
         DROP INDEX IF EXISTS idx_refs_caller_node_kind;
         DROP INDEX IF EXISTS idx_refs_target_file;
         DROP INDEX IF EXISTS idx_file_dependencies_dep_file;
         DROP INDEX IF EXISTS idx_edges_source_kind;
         DROP INDEX IF EXISTS idx_edges_target_kind;
         DROP INDEX IF EXISTS idx_edges_target_file_symbol;
         DROP INDEX IF EXISTS idx_edges_ref_id;
         DROP INDEX IF EXISTS idx_dispatch_hints_method;
         DROP INDEX IF EXISTS idx_backend_file_state_file;",
    )?;
    Ok(())
}

fn create_cold_build_secondary_indexes(tx: &Transaction<'_>) -> Result<()> {
    tx.execute_batch(
        "CREATE INDEX IF NOT EXISTS idx_nodes_file ON nodes(file_path);
         CREATE INDEX IF NOT EXISTS idx_nodes_name ON nodes(name);
         CREATE INDEX IF NOT EXISTS idx_nodes_scoped ON nodes(scoped_name);
         CREATE INDEX IF NOT EXISTS idx_refs_short_name ON refs(short_name);
         CREATE INDEX IF NOT EXISTS idx_refs_kind_caller_file ON refs(kind, caller_file);
         CREATE INDEX IF NOT EXISTS idx_refs_caller_file ON refs(caller_file);
         CREATE INDEX IF NOT EXISTS idx_refs_caller_node_kind ON refs(caller_node, kind, status);
         CREATE INDEX IF NOT EXISTS idx_refs_target_file ON refs(target_file);
         CREATE INDEX IF NOT EXISTS idx_file_dependencies_dep_file ON file_dependencies(dep_file);
         CREATE INDEX IF NOT EXISTS idx_edges_source_kind ON edges(source_node, kind);
         CREATE INDEX IF NOT EXISTS idx_edges_target_kind ON edges(target_node, kind);
         CREATE INDEX IF NOT EXISTS idx_edges_target_file_symbol ON edges(target_file, target_symbol, kind);
         CREATE INDEX IF NOT EXISTS idx_edges_ref_id ON edges(ref_id, kind);
         CREATE INDEX IF NOT EXISTS idx_dispatch_hints_method ON dispatch_hints(method_name);
         CREATE INDEX IF NOT EXISTS idx_backend_file_state_file ON backend_file_state(file_path, backend);",
    )?;
    Ok(())
}

const STORE_DATA_PATH_COLUMNS: &[(&str, &str)] = &[
    ("files", "path"),
    ("nodes", "file_path"),
    ("refs", "caller_file"),
    ("refs", "target_file"),
    ("file_dependencies", "file_path"),
    ("file_dependencies", "dep_file"),
    ("edges", "target_file"),
    ("dispatch_hints", "file"),
    ("backend_file_state", "file_path"),
];

/// Reconcile `backend_file_state.workspace_root` when the opener's project root
/// differs from what is stored. The store key is the git-root commit hash, so
/// multiple live checkouts/clones share one on-disk generation.
///
/// Cheap in-place re-root is only safe when every previously stored root path is
/// gone from disk (true move/rename). If any stale root still exists, another
/// clone is still alive and rewriting metadata would ping-pong relative rows
/// between trees (possibly on different branches). We then return
/// [`OpenRootRepair::NeedsRebuild`] so the caller cold-builds for the current
/// opener. That can make each clone rebuild on open when they alternate — bounded
/// by open frequency — but each rebuild is correct for its opener, unlike silent
/// cross-clone corruption.
fn reconcile_workspace_roots(
    conn: &mut Connection,
    project_root: &Path,
    allow_repair: bool,
) -> Result<OpenRootRepair> {
    let roots = stored_workspace_roots(conn)?;
    let current_root = project_root.display().to_string();
    if roots.is_empty() || (roots.len() == 1 && roots[0] == current_root) {
        return Ok(OpenRootRepair::None);
    }

    if let Some(sample) = sample_absolute_data_path(conn)? {
        return Ok(OpenRootRepair::NeedsRebuild {
            previous_roots: roots,
            current_root,
            reason: format!("absolute store data path row {sample}"),
        });
    }

    for stored_root in roots.iter() {
        if stored_root == &current_root {
            continue;
        }
        if Path::new(stored_root).exists() {
            let reason = format!(
                "previous root {stored_root} still exists — concurrent clone, rebuilding per-root"
            );
            return Ok(OpenRootRepair::NeedsRebuild {
                previous_roots: roots,
                current_root,
                reason,
            });
        }
    }

    if !allow_repair {
        return Ok(OpenRootRepair::NeedsRebuild {
            previous_roots: roots,
            current_root,
            reason: "workspace root metadata requires deferred repair".to_string(),
        });
    }

    publish_if_current(|| {
        let tx = conn.transaction()?;
        tx.execute(
            "UPDATE OR IGNORE backend_file_state
             SET workspace_root = ?1
             WHERE workspace_root <> ?1",
            params![&current_root],
        )?;
        tx.execute(
            "DELETE FROM backend_file_state WHERE workspace_root <> ?1",
            params![&current_root],
        )?;
        tx.commit()?;
        Ok(())
    })?;

    crate::slog_info!(
        "callgraph store re-rooted from {} to {}",
        roots.join(", "),
        current_root
    );
    Ok(OpenRootRepair::ReRooted)
}

fn stored_workspace_roots(conn: &Connection) -> Result<Vec<String>> {
    let mut stmt = conn.prepare(
        "SELECT DISTINCT workspace_root
         FROM backend_file_state
         ORDER BY workspace_root",
    )?;
    let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
    rows.collect::<std::result::Result<Vec<_>, _>>()
        .map_err(Into::into)
}

fn sample_absolute_data_path(conn: &Connection) -> Result<Option<String>> {
    for (table, column) in STORE_DATA_PATH_COLUMNS {
        let sql = format!(
            "SELECT DISTINCT {column} FROM {table} WHERE {column} IS NOT NULL AND {column} <> ''"
        );
        let mut stmt = conn.prepare(&sql)?;
        let mut rows = stmt.query([])?;
        while let Some(row) = rows.next()? {
            let value: String = row.get(0)?;
            if stored_path_is_absolute(&value) {
                return Ok(Some(format!("{table}.{column}={value}")));
            }
        }
    }
    Ok(None)
}

fn stored_path_is_absolute(value: &str) -> bool {
    if value.is_empty() {
        return false;
    }
    if Path::new(value).is_absolute() || value.starts_with('/') {
        return true;
    }
    let bytes = value.as_bytes();
    if bytes.len() >= 3
        && bytes[1] == b':'
        && (bytes[2] == b'/' || bytes[2] == b'\\')
        && bytes[0].is_ascii_alphabetic()
    {
        return true;
    }
    value.starts_with("\\\\") || value.starts_with("//")
}

fn log_root_repair_rebuild(repair: &OpenRootRepair) {
    if let OpenRootRepair::NeedsRebuild {
        previous_roots,
        current_root,
        reason,
    } = repair
    {
        crate::slog_info!(
            "callgraph store root mismatch from {} to {} requires cold rebuild: {}",
            previous_roots.join(", "),
            current_root,
            reason
        );
    }
}

/// Nanosecond clock used to make temp/generation file names unique.
fn now_nanos() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
        .as_nanos()
}

/// The pointer file `<dir>/<key>.current`. Its single line names the current
/// generation DB file. ONLY Rust std ever opens this file (never SQLite), so it
/// can always be atomically replaced via rename even on Windows — Rust opens
/// files with `FILE_SHARE_DELETE`, unlike SQLite's Win32 VFS.
fn pointer_path(callgraph_dir: &Path, project_key: &str) -> PathBuf {
    callgraph_dir.join(format!("{project_key}.current"))
}

/// The legacy single-file DB path used before the generation scheme. Still read
/// as a fallback so pre-upgrade on-disk stores keep working until the next cold
/// build publishes a generation.
fn legacy_sqlite_path(callgraph_dir: &Path, project_key: &str) -> PathBuf {
    callgraph_dir.join(format!("{project_key}.sqlite"))
}

/// A fresh, unique generation file NAME: `<key>.g<nanos>.<pid>.sqlite`. Each
/// cold build writes a brand-new generation file, so publishing NEVER replaces
/// a file another process holds open (the root Windows fix).
fn generation_file_name(project_key: &str) -> String {
    format!(
        "{project_key}.g{}.{}.sqlite",
        now_nanos(),
        std::process::id()
    )
}

/// Read the pointer; returns the generation file name if present and non-empty.
fn read_pointer(callgraph_dir: &Path, project_key: &str) -> Option<String> {
    let text = std::fs::read_to_string(pointer_path(callgraph_dir, project_key)).ok()?;
    let name = text.trim();
    if name.is_empty() {
        None
    } else {
        Some(name.to_string())
    }
}

/// True if the DB at `path` opens and reports ready (schema + fingerprint + the
/// `ready` flag). Uses a throwaway read-only connection.
fn db_path_ready(path: &Path) -> bool {
    (|| -> Result<bool> {
        let conn = open_readonly_connection(path)?;
        database_ready(&conn)
    })()
    .unwrap_or(false)
}

/// Resolve the DB file a reader/opener should use, returning `(path, generation)`
/// where `generation` is `Some(name)` for a pointer-published generation or
/// `None` for the legacy single-file DB. Returns `None` when nothing ready is
/// published (caller treats that as "needs cold build").
///
/// Handles the GC race (the pointer names a generation that was just deleted) by
/// re-reading the pointer and retrying a few times.
fn resolve_ready_target(
    callgraph_dir: &Path,
    project_key: &str,
) -> Option<(PathBuf, Option<String>)> {
    for _ in 0..5 {
        if let Some(generation) = read_pointer(callgraph_dir, project_key) {
            let gen_path = callgraph_dir.join(&generation);
            if gen_path.is_file() {
                return (migration_manifest_valid(callgraph_dir, &generation)
                    && db_path_ready(&gen_path))
                .then_some((gen_path, Some(generation)));
            }
            // Pointer names a missing generation (a GC/publish race): re-read the
            // pointer and retry rather than failing the reader.
            std::thread::sleep(Duration::from_millis(5));
            continue;
        }
        // No pointer: fall back to the legacy single-file DB if it is ready.
        let legacy = legacy_sqlite_path(callgraph_dir, project_key);
        return (legacy.is_file() && db_path_ready(&legacy)).then_some((legacy, None));
    }
    None
}

/// Atomically publish `generation` as the current store by flipping the pointer
/// file. Writes a temp file, fsyncs, then renames over the pointer — never
/// replacing an open DB file, so it succeeds cross-platform.
fn publish_pointer(callgraph_dir: &Path, project_key: &str, generation: &str) -> Result<()> {
    let pointer = pointer_path(callgraph_dir, project_key);
    let tmp = callgraph_dir.join(format!(
        "{project_key}.current.tmp.{}.{}",
        std::process::id(),
        now_nanos()
    ));
    {
        use std::io::Write as _;
        let mut file = std::fs::File::create(&tmp)?;
        file.write_all(generation.as_bytes())?;
        file.write_all(b"\n")?;
        file.sync_all()?;
    }
    if let Err(error) = crate::fs_lock::rename_over(&tmp, &pointer) {
        let _ = std::fs::remove_file(&tmp);
        return Err(error.into());
    }
    crate::fs_lock::sync_parent(&pointer);
    Ok(())
}

#[derive(Clone, Debug)]
struct GenerationGcCandidate {
    name: String,
    path: PathBuf,
    modified: SystemTime,
}

/// Best-effort GC of superseded generation files. The current pointer target and
/// newest previous generation are always retained. Older generations are removed
/// when they have no protected read marker, or after the absolute retention TTL
/// even if an ultra-stale marker remains. Stale marker files are reclaimed during
/// every sweep so dead-PID and expired cross-host readers do not pin disk forever.
fn gc_old_generations(callgraph_dir: &Path, project_key: &str, current: &str) {
    let temp_grace = Duration::from_secs(60);
    let now = SystemTime::now();
    let pointer_current =
        read_pointer(callgraph_dir, project_key).unwrap_or_else(|| current.to_string());
    let gen_prefix = format!("{project_key}.g");
    let tmp_prefixes = [
        format!("{project_key}.g"), // generation build temps (<key>.g...sqlite.tmp.*)
        format!("{project_key}.current."), // pointer publish temps (<key>.current.tmp.*)
        format!("{project_key}.sqlite.tmp."), // legacy-scheme build temps
    ];
    let Ok(entries) = std::fs::read_dir(callgraph_dir) else {
        return;
    };
    let mut gens: Vec<GenerationGcCandidate> = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy().to_string();
        let mtime = entry.metadata().and_then(|m| m.modified()).unwrap_or(now);
        let aged_out = now.duration_since(mtime).unwrap_or(Duration::ZERO) >= temp_grace;

        // Orphaned temp files from a crashed build/publish: remove once aged out.
        if name.contains(".tmp.") {
            if aged_out && tmp_prefixes.iter().any(|p| name.starts_with(p)) {
                let _ = std::fs::remove_file(entry.path());
            }
            continue;
        }

        // Superseded legacy single-file DB: best-effort delete once a generation
        // is published (ignored if another process still holds it open).
        if name == format!("{project_key}.sqlite") {
            remove_sqlite_file_set(&entry.path());
            continue;
        }

        if name.starts_with(&gen_prefix) && name.ends_with(".sqlite") {
            gens.push(GenerationGcCandidate {
                name,
                path: entry.path(),
                modified: mtime,
            });
        }
    }

    let mut superseded = gens
        .iter()
        .filter(|generation| generation.name != pointer_current)
        .collect::<Vec<_>>();
    superseded.sort_by(|left, right| {
        right
            .modified
            .cmp(&left.modified)
            .then_with(|| right.name.cmp(&left.name))
    });
    let previous = superseded.first().map(|generation| generation.name.clone());

    for generation in gens {
        let sweep = crate::root_cache::sweep_read_markers(callgraph_dir, &generation.name);
        if generation.name == pointer_current
            || Some(generation.name.as_str()) == previous.as_deref()
        {
            continue;
        }

        let age = now
            .duration_since(generation.modified)
            .unwrap_or(Duration::ZERO);
        if sweep.protected && age < MARKED_GENERATION_RETENTION_TTL {
            continue;
        }

        remove_sqlite_file_set(&generation.path);
        let _ = std::fs::remove_file(migration_manifest_path(callgraph_dir, &generation.name));
        let _ = std::fs::remove_dir_all(crate::root_cache::read_marker_dir(
            callgraph_dir,
            &generation.name,
        ));
    }
}

fn remove_sqlite_file_set(path: &Path) {
    let _ = std::fs::remove_file(path);
    remove_sqlite_sidecars(path);
}

fn remove_sqlite_sidecars(path: &Path) {
    let path_text = path.to_string_lossy();
    let _ = std::fs::remove_file(PathBuf::from(format!("{path_text}-wal")));
    let _ = std::fs::remove_file(PathBuf::from(format!("{path_text}-shm")));
    let _ = std::fs::remove_file(PathBuf::from(format!("{path_text}-journal")));
}

/// Minimum age before a cold-build temporary is treated as orphaned and deleted.
///
/// A cold build writes `<key>.g...sqlite.tmp.<pid>.<ts>` and renames it into
/// place on success; a build that dies (process kill, crash, host restart) leaves
/// the temporary behind. The largest observed cold build finishes well under a
/// day, so a temporary that has sat for 24 hours belongs to a dead build that will
/// never rename. A live build's temporary is minutes old at most.
///
/// The predicate is deliberately AGE-based, not pid-liveness. Pid reuse makes a
/// liveness check read false-positive on exactly the oldest files — the ones most
/// worth deleting: in production an orphan's embedded pid had been recycled to an
/// unrelated live process, so "is the pid alive?" answered yes for garbage. Age
/// cannot lie that way, so it is the honest orphan predicate.
const ORPHANED_BUILD_TEMP_MIN_AGE: Duration = Duration::from_secs(24 * 60 * 60);

/// Best-effort store-wide sweep of orphaned cold-build temporaries. Runs at the
/// same cadence as [`gc_old_generations`] (after a generation is published) but,
/// unlike it, is not scoped to the building root: it covers every directory in the
/// callgraph store so orphans left by a root that STOPPED building are reclaimed.
///
/// That last case is the production hole this fixes. The per-root cleanup in
/// [`gc_old_generations`] only fires when a root actually builds, so when activity
/// moves away (e.g. the root-keyed migration moved builds to a new store) the old
/// store's orphans become permanent — gigabytes accumulated in a legacy store
/// whose roots no longer built there, while the active store stayed clean. A
/// sibling root that still builds triggers this pass and cleans both layouts.
fn sweep_orphaned_build_temps_store_wide(callgraph_dir: &Path) {
    sweep_orphaned_build_temps(callgraph_dir);
    let Some(storage_root) = root_storage_dir(callgraph_dir) else {
        return;
    };
    let domain = crate::root_cache::RootCacheDomain::Callgraph.as_str();

    // Root-keyed layout: every `<storage>/callgraph/<key>` directory.
    if let Ok(entries) = std::fs::read_dir(storage_root.join(domain)) {
        for entry in entries.flatten() {
            if entry.path().is_dir() {
                sweep_orphaned_build_temps(&entry.path());
            }
        }
    }

    // Legacy per-harness layout: every `<storage>/<harness>/callgraph` directory.
    if let Ok(entries) = std::fs::read_dir(&storage_root) {
        for entry in entries.flatten() {
            let legacy_dir = entry.path().join(domain);
            if legacy_dir.is_dir() {
                sweep_orphaned_build_temps(&legacy_dir);
            }
        }
    }
}

/// Sweep one callgraph directory, removing build temporaries older than
/// [`ORPHANED_BUILD_TEMP_MIN_AGE`].
fn sweep_orphaned_build_temps(callgraph_dir: &Path) {
    sweep_orphaned_build_temps_older_than(callgraph_dir, ORPHANED_BUILD_TEMP_MIN_AGE);
}

/// Inner sweep with an explicit age threshold so tests can exercise the predicate.
/// See [`ORPHANED_BUILD_TEMP_MIN_AGE`] for why the predicate is age, not pid.
fn sweep_orphaned_build_temps_older_than(callgraph_dir: &Path, min_age: Duration) {
    let now = SystemTime::now();
    let Ok(entries) = std::fs::read_dir(callgraph_dir) else {
        return;
    };
    let mut removed_any = false;
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        // Build-temporary shape: `<key>.g...sqlite.tmp.<pid>.<ts>`. The
        // `-journal`/`-wal`/`-shm` sidecars append their suffix AFTER the temp
        // name, so they still contain `.sqlite.tmp.` and match here too. Anything
        // without that substring — a completed `.sqlite` generation, a pointer, a
        // read-marker dir — is left alone: those belong to generation GC.
        if !name.contains(".sqlite.tmp.") {
            continue;
        }
        let mtime = entry
            .metadata()
            .and_then(|meta| meta.modified())
            .unwrap_or(now);
        if now.duration_since(mtime).unwrap_or(Duration::ZERO) < min_age {
            continue;
        }
        // Deletion races a concurrent build finishing: that build renames the temp
        // into place, so the file is gone by the time we unlink. The 24h age makes
        // this overlap practically impossible, but treat a missing file as success
        // (the rename won) rather than an error, and never touch a path that does
        // not match the temporary shape above.
        match std::fs::remove_file(entry.path()) {
            Ok(()) => removed_any = true,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => {}
        }
    }
    if removed_any {
        crate::fs_lock::sync_parent(callgraph_dir);
    }
}

/// Bound the cold-build's tree-sitter pass to half the cores (cap 8) instead of
/// the global all-cores rayon pool. The store cold-build is the heaviest
/// background pass (parse-dominated) and runs on a separate thread off the
/// single-threaded request loop; left unbounded it monopolizes every core and
/// starves the bridge so interactive tools time out (the same starvation the
/// v0.35 embedder and the inspect Tier-2 pool already cap). 8MB worker stacks
/// match the main thread, since the extract walks tree-sitter ASTs.
fn build_pool_size() -> usize {
    std::thread::available_parallelism()
        .map(|parallelism| parallelism.get())
        .unwrap_or(1)
        .div_ceil(2)
        .clamp(1, 8)
}

fn build_extracts_parallel(project_root: &Path, files: &[PathBuf]) -> BuildExtractsResult {
    let extract_one = |path: &PathBuf| match build_file_extract(project_root, path) {
        Ok(extract) => Ok(extract),
        Err(error) => {
            let abs_path =
                normalize_file_path(project_root, path).unwrap_or_else(|_| path.to_path_buf());
            let rel_path = relative_path(project_root, &abs_path);
            let freshness = cache_freshness::collect(&abs_path).ok();
            log::debug!(
                "callgraph store: skipping {} during cold build: {}",
                abs_path.display(),
                error
            );
            Err(ExtractFailure {
                rel_path,
                freshness,
            })
        }
    };

    let run = || -> Vec<std::result::Result<FileExtract, ExtractFailure>> {
        files.par_iter().map(extract_one).collect()
    };

    // Run inside a dedicated bounded pool when one builds; fall back to the
    // global pool only if the bounded pool can't be constructed.
    let results = match rayon::ThreadPoolBuilder::new()
        .num_threads(build_pool_size())
        .thread_name(|index| format!("aft-callgraph-build-{index}"))
        .stack_size(8 * 1024 * 1024)
        .build()
    {
        Ok(pool) => pool.install(run),
        Err(error) => {
            log::warn!(
                "callgraph store: bounded build pool unavailable ({error}); using global pool"
            );
            run()
        }
    };

    let mut extracts = Vec::new();
    let mut failures = Vec::new();
    for result in results {
        match result {
            Ok(extract) => extracts.push(extract),
            Err(failure) => failures.push(failure),
        }
    }
    BuildExtractsResult { extracts, failures }
}

fn collect_source_freshness(path: &Path, source: &str) -> std::io::Result<FileFreshness> {
    let metadata = std::fs::metadata(path)?;
    let size = metadata.len();
    let content_hash = if size > cache_freshness::CONTENT_HASH_SIZE_CAP {
        cache_freshness::zero_hash()
    } else if source.len() as u64 == size {
        cache_freshness::hash_bytes(source.as_bytes())
    } else {
        cache_freshness::hash_file_if_small(path, size)?.unwrap_or_else(cache_freshness::zero_hash)
    };
    Ok(FileFreshness {
        mtime: metadata.modified().unwrap_or(UNIX_EPOCH),
        size,
        content_hash,
    })
}

fn build_file_extract(project_root: &Path, path: &Path) -> Result<FileExtract> {
    let abs_path = normalize_file_path(project_root, path)?;
    let rel_path = relative_path(project_root, &abs_path);
    let source = std::fs::read_to_string(&abs_path)?;
    let freshness = collect_source_freshness(&abs_path, &source)?;
    let mut data = callgraph::build_file_data_from_source(&abs_path, &source)?;
    let lang = data.lang;
    if lang == LangId::Rust {
        extend_rust_imports_with_nested_uses(&source, &mut data);
    }
    let mut nodes = build_node_records(&rel_path, &source, &data)?;
    let node_by_scoped: HashMap<String, String> = nodes
        .iter()
        .map(|node| (node.scoped_name.clone(), node.id.clone()))
        .collect();
    let import_dependencies =
        import_dependencies(project_root, &abs_path, &data.import_block.imports);
    let line_index = LineIndex::new(&source);
    let reexports = collect_reexport_refs(project_root, &abs_path, &rel_path, &source);
    let rust_reexports = if lang == LangId::Rust {
        collect_rust_pub_use_reexport_refs(
            project_root,
            &abs_path,
            &rel_path,
            &data.import_block.imports,
            &line_index,
        )
    } else {
        ReexportRefs {
            raw_refs: Vec::new(),
            surface_parts: Vec::new(),
        }
    };
    let source_less_exports = collect_source_less_export_alias_refs(&rel_path, &source);
    let mut raw_refs = Vec::new();
    raw_refs.extend(build_call_refs(
        &rel_path,
        &data,
        &node_by_scoped,
        &import_dependencies,
    ));
    raw_refs.extend(build_import_refs(
        project_root,
        &abs_path,
        &rel_path,
        &data.import_block.imports,
        &line_index,
    ));
    let mut surface_parts = reexports.surface_parts;
    surface_parts.extend(rust_reexports.surface_parts);
    surface_parts.extend(source_less_exports.surface_parts);
    raw_refs.extend(reexports.raw_refs);
    raw_refs.extend(rust_reexports.raw_refs);
    raw_refs.extend(source_less_exports.raw_refs);
    let dispatch_hints = build_dispatch_hints(&rel_path, &data, &node_by_scoped);
    let surface_fingerprint = surface_fingerprint(&mut nodes, &data, &surface_parts);

    Ok(FileExtract {
        rel_path,
        freshness,
        lang,
        data,
        nodes,
        raw_refs,
        dispatch_hints,
        surface_fingerprint,
    })
}

fn build_node_records(
    rel_path: &str,
    source: &str,
    data: &FileCallData,
) -> Result<Vec<NodeRecord>> {
    let mut records = Vec::new();
    let mut ordinal_by_range: BTreeMap<(u32, u32, u32, u32), u32> = BTreeMap::new();
    let mut metadata: Vec<_> = data.symbol_metadata.iter().collect();
    metadata.sort_by(|(left, _), (right, _)| left.cmp(right));

    for (scoped_name, meta) in metadata {
        let name = unqualified_name(scoped_name).to_string();
        let range = selection_range(source, scoped_name, &name, &meta.range);
        let range_key = (
            range.start_line,
            range.start_col,
            range.end_line,
            range.end_col,
        );
        let ordinal = ordinal_by_range.entry(range_key).or_insert(0);
        let range_ordinal = *ordinal;
        *ordinal += 1;
        let id = node_id(rel_path, &range, range_ordinal, scoped_name);
        let exported = meta.exported || data.exported_symbols.iter().any(|item| item == &name);
        let is_default_export = data
            .default_export_symbol
            .as_deref()
            .map(|default| default == scoped_name || default == name)
            .unwrap_or(false);
        records.push(NodeRecord {
            id,
            file_path: rel_path.to_string(),
            name: name.clone(),
            scoped_name: scoped_name.clone(),
            kind: symbol_kind_label(&meta.kind).to_string(),
            range,
            range_ordinal,
            signature: meta.signature.clone(),
            exported,
            is_default_export,
            is_type_like: is_type_like(&meta.kind),
            is_callgraph_entry_point: meta.entry_point_attribute.is_some()
                || callgraph::is_entry_point(scoped_name, &meta.kind, exported, data.lang),
        });
    }

    Ok(records)
}

fn selection_range(source: &str, scoped_name: &str, name: &str, fallback: &Range) -> Range {
    if scoped_name == TOP_LEVEL_SYMBOL {
        return Range {
            start_line: 0,
            start_col: 0,
            end_line: 0,
            end_col: 0,
        };
    }
    let Some(line) = source.lines().nth(fallback.start_line as usize) else {
        return fallback.clone();
    };
    let start_col = fallback.start_col as usize;
    let search_start = start_col.min(line.len());
    if let Some(offset) = line[search_start..].find(name) {
        let col = search_start + offset;
        return Range {
            start_line: fallback.start_line,
            start_col: col as u32,
            end_line: fallback.start_line,
            end_col: (col + name.len()) as u32,
        };
    }
    if let Some(offset) = line.find(name) {
        return Range {
            start_line: fallback.start_line,
            start_col: offset as u32,
            end_line: fallback.start_line,
            end_col: (offset + name.len()) as u32,
        };
    }
    Range {
        start_line: fallback.start_line,
        start_col: fallback.start_col,
        end_line: fallback.start_line,
        end_col: fallback.start_col.saturating_add(name.len() as u32),
    }
}

fn node_id(rel_path: &str, range: &Range, ordinal: u32, scoped_name: &str) -> String {
    if scoped_name == TOP_LEVEL_SYMBOL {
        return format!("top:{}", hash_to_hex(blake3::hash(rel_path.as_bytes())));
    }
    let input = format!(
        "{rel_path}:{}:{}:{}:{}:{ordinal}",
        range.start_line, range.start_col, range.end_line, range.end_col
    );
    format!("pos:{}", hash_to_hex(blake3::hash(input.as_bytes())))
}

fn build_call_refs(
    rel_path: &str,
    data: &FileCallData,
    node_by_scoped: &HashMap<String, String>,
    import_dependencies: &BTreeSet<String>,
) -> Vec<RawRef> {
    let mut refs = Vec::new();
    let mut ordinal = 0usize;
    let mut symbols: Vec<_> = data.calls_by_symbol.iter().collect();
    symbols.sort_by(|(left, _), (right, _)| left.cmp(right));
    for (caller_symbol, call_sites) in symbols {
        let caller_node = node_by_scoped.get(caller_symbol).cloned();
        for call_site in call_sites {
            ordinal += 1;
            let ref_id = ref_id(&[
                rel_path,
                "call",
                caller_symbol,
                &call_site.line.to_string(),
                &call_site.byte_start.to_string(),
                &call_site.byte_end.to_string(),
                &call_site.full_callee,
                &ordinal.to_string(),
            ]);
            refs.push(RawRef {
                ref_id,
                caller_node: caller_node.clone(),
                caller_symbol: Some(caller_symbol.clone()),
                caller_file: rel_path.to_string(),
                kind: "call".to_string(),
                short_name: Some(call_site.callee_name.clone()),
                full_ref: Some(call_site.full_callee.clone()),
                module_path: None,
                import_kind: None,
                local_name: Some(call_site.callee_name.clone()),
                requested_name: Some(call_site.callee_name.clone()),
                namespace_alias: namespace_alias(&call_site.full_callee),
                wildcard: false,
                line: call_site.line,
                byte_start: call_site.byte_start,
                byte_end: call_site.byte_end,
                dependencies: import_dependencies.clone(),
            });
        }
    }
    refs
}

fn build_import_refs(
    project_root: &Path,
    abs_path: &Path,
    rel_path: &str,
    imports: &[ImportStatement],
    line_index: &LineIndex,
) -> Vec<RawRef> {
    let mut refs = Vec::new();
    for (index, import) in imports.iter().enumerate() {
        let import_kind = import_kind_label(import.kind).to_string();
        let local_name = import_local_names(import).join(",");
        let requested_name = import_requested_names(import).join(",");
        let ref_id = ref_id(&[
            rel_path,
            "import",
            &import.byte_range.start.to_string(),
            &import.byte_range.end.to_string(),
            &import.module_path,
            &index.to_string(),
        ]);
        refs.push(RawRef {
            ref_id,
            caller_node: None,
            caller_symbol: None,
            caller_file: rel_path.to_string(),
            kind: "import".to_string(),
            short_name: None,
            full_ref: Some(import.raw_text.clone()),
            module_path: Some(import.module_path.clone()),
            import_kind: Some(import_kind),
            local_name: empty_to_none(local_name),
            requested_name: empty_to_none(requested_name),
            namespace_alias: import.namespace_import.clone(),
            wildcard: import_is_wildcard(import),
            line: line_index.byte_to_line(import.byte_range.start),
            byte_start: import.byte_range.start,
            byte_end: import.byte_range.end,
            dependencies: module_dependencies(project_root, abs_path, &import.module_path),
        });
    }
    refs
}

fn extend_rust_imports_with_nested_uses(source: &str, data: &mut FileCallData) {
    let grammar = grammar_for(LangId::Rust);
    let mut parser = Parser::new();
    if parser.set_language(&grammar).is_err() {
        return;
    }
    let Some(tree) = parser.parse(source, None) else {
        return;
    };

    let mut seen = data
        .import_block
        .imports
        .iter()
        .map(|import| (import.byte_range.start, import.byte_range.end))
        .collect::<HashSet<_>>();
    let mut nested_imports = Vec::new();
    collect_rust_use_imports(source, tree.root_node(), &mut seen, &mut nested_imports);
    if nested_imports.is_empty() {
        return;
    }

    data.import_block.imports.extend(nested_imports);
    data.import_block
        .imports
        .sort_by_key(|import| import.byte_range.start);
    data.import_block.byte_range = import_byte_range_from_imports(&data.import_block.imports);
}

fn collect_rust_use_imports(
    source: &str,
    node: Node<'_>,
    seen: &mut HashSet<(usize, usize)>,
    imports: &mut Vec<ImportStatement>,
) {
    if node.kind() == "use_declaration" {
        let range = node.byte_range();
        if seen.insert((range.start, range.end)) {
            if let Some(import) = rust_import_from_use_node(source, node) {
                imports.push(import);
            }
        }
    }

    let mut cursor = node.walk();
    if !cursor.goto_first_child() {
        return;
    }
    loop {
        collect_rust_use_imports(source, cursor.node(), seen, imports);
        if !cursor.goto_next_sibling() {
            break;
        }
    }
}

fn rust_import_from_use_node(source: &str, node: Node<'_>) -> Option<ImportStatement> {
    let raw_text = source[node.byte_range()].to_string();
    let body = rust_use_body(&raw_text)?.to_string();
    let visibility = rust_use_visibility(&raw_text);
    let names = rust_use_list_names(&body);
    let group = classify_rust_import_group(&body);
    let byte_range = node.byte_range();

    Some(ImportStatement {
        module_path: body,
        names: names.clone(),
        default_import: visibility.clone(),
        namespace_import: None,
        kind: ImportKind::Value,
        group,
        byte_range,
        raw_text,
        form: ImportForm::RustUse {
            visibility,
            named: names,
        },
    })
}

fn import_byte_range_from_imports(imports: &[ImportStatement]) -> Option<std::ops::Range<usize>> {
    let start = imports.iter().map(|import| import.byte_range.start).min()?;
    let end = imports.iter().map(|import| import.byte_range.end).max()?;
    Some(start..end)
}

fn rust_use_visibility(raw_text: &str) -> Option<String> {
    let use_pos = raw_text.find("use ")?;
    let prefix = raw_text[..use_pos].trim();
    if prefix.is_empty() {
        None
    } else {
        Some(prefix.to_string())
    }
}

fn rust_use_body(raw_text: &str) -> Option<&str> {
    let use_pos = raw_text.find("use ")?;
    Some(raw_text[use_pos + 4..].trim().trim_end_matches(';').trim())
}

fn rust_use_list_names(body: &str) -> Vec<String> {
    let Some(open) = body.find("::{") else {
        return Vec::new();
    };
    let Some(close) = body[open + 3..].find('}').map(|offset| open + 3 + offset) else {
        return Vec::new();
    };
    body[open + 3..close]
        .split(',')
        .filter_map(|spec| {
            let spec = spec.trim();
            if spec.is_empty() {
                None
            } else {
                Some(spec.to_string())
            }
        })
        .collect()
}

fn classify_rust_import_group(body: &str) -> ImportGroup {
    let first = body
        .split("::")
        .next()
        .unwrap_or(body)
        .split_whitespace()
        .next()
        .unwrap_or(body);
    match first.trim() {
        "std" | "core" | "alloc" => ImportGroup::Stdlib,
        "crate" | "self" | "super" => ImportGroup::Internal,
        _ => ImportGroup::External,
    }
}

#[derive(Debug, Clone)]
struct ReexportRefs {
    raw_refs: Vec<RawRef>,
    surface_parts: Vec<String>,
}

fn collect_reexport_refs(
    project_root: &Path,
    abs_path: &Path,
    rel_path: &str,
    source: &str,
) -> ReexportRefs {
    let mut raw_refs = Vec::new();
    let mut surface_parts = Vec::new();
    let mut search_start = 0usize;
    let mut ordinal = 0usize;
    while let Some(export_offset) = source[search_start..].find("export") {
        let start = search_start + export_offset;
        let Some(statement_end_offset) = source[start..].find(';') else {
            break;
        };
        let end = start + statement_end_offset + 1;
        let statement = &source[start..end];
        search_start = end;
        if !statement.contains(" from ") || !statement.contains(['\'', '"']) {
            continue;
        }
        let Some(module_path) = quoted_module_path(statement) else {
            continue;
        };
        ordinal += 1;
        let wildcard = statement.contains('*');
        let line = source[..start]
            .bytes()
            .filter(|byte| *byte == b'\n')
            .count() as u32
            + 1;
        let ref_id = ref_id(&[
            rel_path,
            "reexport",
            &start.to_string(),
            &end.to_string(),
            &module_path,
            &ordinal.to_string(),
        ]);
        surface_parts.push(format!("reexport\t{statement}"));
        raw_refs.push(RawRef {
            ref_id,
            caller_node: None,
            caller_symbol: None,
            caller_file: rel_path.to_string(),
            kind: "reexport".to_string(),
            short_name: None,
            full_ref: Some(statement.to_string()),
            module_path: Some(module_path.clone()),
            import_kind: Some("reexport".to_string()),
            local_name: None,
            requested_name: None,
            namespace_alias: None,
            wildcard,
            line,
            byte_start: start,
            byte_end: end,
            dependencies: module_dependencies(project_root, abs_path, &module_path),
        });
    }
    ReexportRefs {
        raw_refs,
        surface_parts,
    }
}

fn collect_rust_pub_use_reexport_refs(
    project_root: &Path,
    abs_path: &Path,
    rel_path: &str,
    imports: &[ImportStatement],
    line_index: &LineIndex,
) -> ReexportRefs {
    let mut raw_refs = Vec::new();
    let mut surface_parts = Vec::new();
    let mut ordinal = 0usize;

    for import in imports {
        let Some(visibility) = &import.default_import else {
            continue;
        };
        if !visibility.starts_with("pub") {
            continue;
        }
        let Some((module_path, named, wildcard)) = rust_pub_use_reexport_parts(import) else {
            continue;
        };
        ordinal += 1;
        let ref_id = ref_id(&[
            rel_path,
            "rust_reexport",
            &import.byte_range.start.to_string(),
            &import.byte_range.end.to_string(),
            &module_path,
            &ordinal.to_string(),
        ]);
        surface_parts.push(format!("reexport\t{}", import.raw_text));
        raw_refs.push(RawRef {
            ref_id,
            caller_node: None,
            caller_symbol: None,
            caller_file: rel_path.to_string(),
            kind: "reexport".to_string(),
            short_name: None,
            full_ref: Some(rust_reexport_statement_for_index(&named, &import.raw_text)),
            module_path: Some(module_path.clone()),
            import_kind: Some("reexport".to_string()),
            local_name: None,
            requested_name: None,
            namespace_alias: None,
            wildcard,
            line: line_index.byte_to_line(import.byte_range.start),
            byte_start: import.byte_range.start,
            byte_end: import.byte_range.end,
            dependencies: rust_module_dependencies(project_root, abs_path, &module_path),
        });
    }

    ReexportRefs {
        raw_refs,
        surface_parts,
    }
}

fn rust_pub_use_reexport_parts(
    import: &ImportStatement,
) -> Option<(String, HashMap<String, String>, bool)> {
    let body = rust_use_body(&import.raw_text).unwrap_or(import.module_path.as_str());
    let body = body.trim();
    if let Some(module_path) = body.strip_suffix("::*") {
        return Some((module_path.trim().to_string(), HashMap::new(), true));
    }

    if let Some(brace_start) = body.find("::{") {
        let module_path = body[..brace_start].trim().to_string();
        let names = rust_reexport_names_from_specs(&body[brace_start + 3..body.rfind('}')?]);
        if names.is_empty() {
            return None;
        }
        return Some((module_path, names, false));
    }

    let (module_path, spec) = body.rsplit_once("::")?;
    let names = rust_reexport_names_from_specs(spec);
    if names.is_empty() {
        return None;
    }
    Some((module_path.trim().to_string(), names, false))
}

fn rust_reexport_names_from_specs(specs: &str) -> HashMap<String, String> {
    let mut names = HashMap::new();
    for spec in specs.split(',') {
        let spec = spec.trim();
        if spec.is_empty() || spec == "self" {
            continue;
        }
        if let Some((source, local)) = spec.split_once(" as ") {
            let source = source.trim();
            let local = local.trim();
            if !source.is_empty() && !local.is_empty() && source != "self" {
                names.insert(local.to_string(), source.to_string());
            }
        } else {
            names.insert(spec.to_string(), spec.to_string());
        }
    }
    names
}

fn rust_reexport_statement_for_index(named: &HashMap<String, String>, fallback: &str) -> String {
    if named.is_empty() {
        return fallback.to_string();
    }
    let mut specs = named
        .iter()
        .map(|(local, source)| {
            if local == source {
                source.clone()
            } else {
                format!("{source} as {local}")
            }
        })
        .collect::<Vec<_>>();
    specs.sort();
    format!("pub use {{{}}};", specs.join(", "))
}

fn quoted_module_path(statement: &str) -> Option<String> {
    let quote = match (statement.find('\''), statement.find('"')) {
        (Some(single), Some(double)) if single < double => '\'',
        (Some(_), Some(_)) => '"',
        (Some(_), None) => '\'',
        (None, Some(_)) => '"',
        (None, None) => return None,
    };
    let start = statement.find(quote)? + 1;
    let end = statement[start..].find(quote)? + start;
    Some(statement[start..end].to_string())
}

#[derive(Debug, Clone)]
struct SourceLessExportRefs {
    raw_refs: Vec<RawRef>,
    surface_parts: Vec<String>,
}

fn collect_source_less_export_alias_refs(rel_path: &str, source: &str) -> SourceLessExportRefs {
    let mut raw_refs = Vec::new();
    let mut surface_parts = Vec::new();
    let mut search_start = 0usize;
    let mut ordinal = 0usize;
    while let Some(export_offset) = source[search_start..].find("export") {
        let start = search_start + export_offset;
        let Some(statement_end_offset) = source[start..].find(';') else {
            break;
        };
        let end = start + statement_end_offset + 1;
        let statement = &source[start..end];
        search_start = end;
        if statement.contains(" from ") || !statement.contains('{') || !statement.contains('}') {
            continue;
        }
        let aliases = parse_reexport_names(statement);
        if aliases.is_empty() {
            continue;
        }
        let line = source[..start]
            .bytes()
            .filter(|byte| *byte == b'\n')
            .count() as u32
            + 1;
        for (exported, source_symbol) in aliases {
            ordinal += 1;
            let ref_id = ref_id(&[
                rel_path,
                "export_alias",
                &start.to_string(),
                &end.to_string(),
                &exported,
                &source_symbol,
                &ordinal.to_string(),
            ]);
            surface_parts.push(format!("export_alias\t{source_symbol}\t{exported}"));
            raw_refs.push(RawRef {
                ref_id,
                caller_node: None,
                caller_symbol: None,
                caller_file: rel_path.to_string(),
                kind: "export_alias".to_string(),
                short_name: None,
                full_ref: Some(statement.to_string()),
                module_path: None,
                import_kind: Some("export_alias".to_string()),
                local_name: Some(exported),
                requested_name: Some(source_symbol),
                namespace_alias: None,
                wildcard: false,
                line,
                byte_start: start,
                byte_end: end,
                dependencies: BTreeSet::new(),
            });
        }
    }
    SourceLessExportRefs {
        raw_refs,
        surface_parts,
    }
}

fn build_dispatch_hints(
    rel_path: &str,
    data: &FileCallData,
    node_by_scoped: &HashMap<String, String>,
) -> Vec<DispatchHint> {
    let mut hints = Vec::new();
    let mut ordinal = 0usize;
    for (caller_symbol, call_sites) in &data.calls_by_symbol {
        let Some(caller_node) = node_by_scoped.get(caller_symbol) else {
            continue;
        };
        for call_site in call_sites {
            if !(call_site.full_callee.contains('.') || call_site.full_callee.contains("::")) {
                continue;
            }
            ordinal += 1;
            hints.push(DispatchHint {
                id: ref_id(&[
                    rel_path,
                    "dispatch",
                    caller_symbol,
                    &call_site.line.to_string(),
                    &call_site.byte_start.to_string(),
                    &call_site.byte_end.to_string(),
                    &ordinal.to_string(),
                ]),
                method_name: call_site.callee_name.clone(),
                caller_node: caller_node.clone(),
                file: rel_path.to_string(),
                line: call_site.line,
                byte_start: call_site.byte_start,
                byte_end: call_site.byte_end,
            });
        }
    }
    hints
}

fn surface_fingerprint(
    nodes: &mut [NodeRecord],
    data: &FileCallData,
    reexport_parts: &[String],
) -> String {
    nodes.sort_by(|left, right| {
        (left.file_path.as_str(), left.scoped_name.as_str())
            .cmp(&(right.file_path.as_str(), right.scoped_name.as_str()))
    });
    let mut parts = Vec::new();
    for node in nodes.iter() {
        parts.push(format!(
            "node\t{}\t{}\t{}\t{}\t{}:{}:{}:{}:{}\t{}",
            node.scoped_name,
            node.name,
            node.kind,
            node.exported,
            node.range.start_line,
            node.range.start_col,
            node.range.end_line,
            node.range.end_col,
            node.range_ordinal,
            node.signature.as_deref().unwrap_or("")
        ));
    }
    let mut exports = data.exported_symbols.clone();
    exports.sort();
    for export in exports {
        parts.push(format!("export\t{export}"));
    }
    if let Some(default_export) = &data.default_export_symbol {
        parts.push(format!("default\t{default_export}"));
    }
    let mut imports: Vec<String> = data
        .import_block
        .imports
        .iter()
        .map(|import| {
            format!(
                "import\t{}\t{:?}\t{}",
                import.module_path, import.form, import.raw_text
            )
        })
        .collect();
    imports.sort();
    parts.extend(imports);
    parts.extend(reexport_parts.iter().cloned());
    hash_to_hex(blake3::hash(parts.join("\n").as_bytes()))
}

fn resolve_ref(raw: RawRef, index: &ProjectIndex<'_>) -> Result<ResolvedRef> {
    if raw.kind != "call" {
        return Ok(ResolvedRef {
            dependencies: raw.dependencies.clone(),
            raw,
            status: "unresolved".to_string(),
            target_node: None,
            target_file: None,
            target_symbol: None,
            edge: None,
        });
    }

    let caller_file = raw.caller_file.clone();
    let caller_data = index.caller_data.get(&caller_file).ok_or_else(|| {
        CallGraphStoreError::MissingCallerData {
            file: caller_file.clone(),
        }
    })?;
    let full_ref = raw.full_ref.as_deref().unwrap_or_default();
    let short_name = raw.short_name.as_deref().unwrap_or_default();
    let mut dependencies = raw.dependencies.clone();

    let resolved = match index.lang_for(&caller_file) {
        Some(LangId::Rust) => {
            resolve_rust_target(index, &caller_file, full_ref, short_name, caller_data, &raw)
        }
        Some(LangId::TypeScript | LangId::Tsx | LangId::JavaScript) => {
            resolve_js_ts_target(index, &caller_file, full_ref, short_name, caller_data)
        }
        _ => resolve_local_target(index, &caller_file, full_ref, short_name, caller_data),
    };

    let Some((status, target_file, target_symbol)) = resolved else {
        return Ok(ResolvedRef {
            raw,
            status: "unresolved".to_string(),
            target_node: None,
            target_file: None,
            target_symbol: None,
            dependencies,
            edge: None,
        });
    };

    dependencies.insert(target_file.clone());
    let target_node = index.node_for_symbol(&target_file, &target_symbol);
    let source_node = raw.caller_node.clone();
    let edge = if let Some(source_node) = source_node {
        if target_file == caller_file
            && raw.caller_symbol.as_deref() == Some(target_symbol.as_str())
        {
            None
        } else {
            Some(EdgeRecord {
                edge_id: ref_id(&[&raw.ref_id, "edge"]),
                source_node,
                target_node: target_node.clone(),
                target_file: target_file.clone(),
                target_symbol: target_symbol.clone(),
                kind: "call".to_string(),
                line: raw.line,
            })
        }
    } else {
        None
    };

    Ok(ResolvedRef {
        raw,
        status,
        target_node,
        target_file: Some(target_file),
        target_symbol: Some(target_symbol),
        dependencies,
        edge,
    })
}

fn resolve_js_ts_target(
    index: &ProjectIndex<'_>,
    caller_file: &str,
    full_ref: &str,
    short_name: &str,
    caller_data: &FileCallData,
) -> Option<(String, String, String)> {
    if let Some((namespace, member)) = full_ref.split_once('.') {
        for import in &caller_data.import_block.imports {
            if import.namespace_import.as_deref() == Some(namespace) {
                if let Some(target_file) = index.module_target(caller_file, &import.module_path) {
                    if let Some((file, symbol)) =
                        resolve_exported_symbol(index, &target_file, member, 0)
                    {
                        return Some(("resolved".to_string(), file, symbol));
                    }
                }
            }
        }
    }

    for import in &caller_data.import_block.imports {
        for spec in &import.names {
            if crate::imports::specifier_local_name(spec) == short_name {
                if let Some(target_file) = index.module_target(caller_file, &import.module_path) {
                    let requested = crate::imports::specifier_imported_name(spec);
                    let (file, symbol) = resolve_exported_symbol(index, &target_file, requested, 0)
                        .unwrap_or_else(|| (target_file, requested.to_string()));
                    return Some(("resolved".to_string(), file, symbol));
                }
            }
        }

        if import.default_import.as_deref() == Some(short_name) {
            if let Some(target_file) = index.module_target(caller_file, &import.module_path) {
                let (file, symbol) = resolve_exported_symbol(index, &target_file, "default", 0)
                    .or_else(|| {
                        index
                            .files
                            .get(&target_file)
                            .and_then(|file| file.default_export.clone())
                            .map(|symbol| (target_file.clone(), symbol))
                    })
                    .unwrap_or_else(|| {
                        let file_name = Path::new(&target_file)
                            .file_name()
                            .and_then(|name| name.to_str())
                            .unwrap_or("unknown")
                            .to_string();
                        (target_file, format!("<default:{file_name}>"))
                    });
                return Some(("resolved".to_string(), file, symbol));
            }
        }
    }

    for import in &caller_data.import_block.imports {
        if let Some(target_file) = index.module_target(caller_file, &import.module_path) {
            if index
                .files
                .get(&target_file)
                .map(|file| file.exports.contains(short_name))
                .unwrap_or(false)
            {
                return Some(("resolved".to_string(), target_file, short_name.to_string()));
            }
        }
    }

    resolve_local_target(index, caller_file, full_ref, short_name, caller_data)
}

fn resolve_exported_symbol(
    index: &ProjectIndex<'_>,
    file: &str,
    requested: &str,
    depth: usize,
) -> Option<(String, String)> {
    let mut visited = std::collections::HashMap::new();
    resolve_exported_symbol_inner(index, file, requested, depth, &mut visited)
}

/// Re-export graphs are frequently cyclic (barrel files re-exporting each
/// other, `pub use` cycles). The depth cap alone bounds path LENGTH, not path
/// COUNT: with wildcard fan-out the walk explores branching^depth paths and a
/// single resolution can burn CPU-minutes. The memo prunes re-visits of a
/// (file, symbol) pair — but only when the earlier visit had at least as much
/// remaining depth budget (a shallower re-visit can reach leaves the deeper
/// first visit had to cut off at the cap, so plain visited-set pruning would
/// lose resolutions the capped walk finds).
fn resolve_exported_symbol_inner(
    index: &ProjectIndex<'_>,
    file: &str,
    requested: &str,
    depth: usize,
    visited: &mut std::collections::HashMap<(String, String), usize>,
) -> Option<(String, String)> {
    if depth > 16 {
        return None;
    }
    if requested != "default" {
        if let Some(source_symbol) = index
            .files
            .get(file)
            .and_then(|item| item.export_aliases.get(requested))
        {
            return Some((file.to_string(), source_symbol.clone()));
        }
        if index
            .files
            .get(file)
            .map(|item| item.exports.contains(requested))
            .unwrap_or(false)
        {
            return Some((file.to_string(), requested.to_string()));
        }
    } else if let Some(default) = index
        .files
        .get(file)
        .and_then(|item| item.default_export.clone())
    {
        return Some((file.to_string(), default));
    }

    // Memo check sits after the local-export fast paths: the common direct
    // hit never allocates the key, and a hit through the memo would have
    // returned above anyway.
    match visited.entry((file.to_string(), requested.to_string())) {
        std::collections::hash_map::Entry::Occupied(mut seen) => {
            if *seen.get() <= depth {
                return None;
            }
            seen.insert(depth);
        }
        std::collections::hash_map::Entry::Vacant(slot) => {
            slot.insert(depth);
        }
    }

    for reexport in index.reexports_for(file) {
        let mut next_requested = requested.to_string();
        let matches = if reexport.wildcard {
            true
        } else if let Some(source_name) = reexport.named.get(requested) {
            next_requested = source_name.clone();
            true
        } else {
            false
        };
        if !matches {
            continue;
        }
        if let Some(target_file) = &reexport.target_file {
            if let Some(target) = resolve_exported_symbol_inner(
                index,
                target_file,
                &next_requested,
                depth + 1,
                visited,
            ) {
                return Some(target);
            }
        }
    }
    None
}

fn resolve_rust_target(
    index: &ProjectIndex<'_>,
    caller_file: &str,
    full_ref: &str,
    short_name: &str,
    caller_data: &FileCallData,
    raw: &RawRef,
) -> Option<(String, String, String)> {
    if full_ref.contains("::") {
        if let Some((target_file, target_symbol)) =
            rust_target_for_qualified(index, caller_file, full_ref, short_name, caller_data, raw)
        {
            return Some(("resolved".to_string(), target_file, target_symbol));
        }
    }

    for import in &caller_data.import_block.imports {
        if let Some((target_file, target_symbol)) =
            rust_target_for_use(index, caller_file, import, short_name)
        {
            return Some(("resolved".to_string(), target_file, target_symbol));
        }
    }

    resolve_local_target(index, caller_file, full_ref, short_name, caller_data)
}

fn rust_target_for_qualified(
    index: &ProjectIndex<'_>,
    caller_file: &str,
    full_ref: &str,
    short_name: &str,
    caller_data: &FileCallData,
    raw: &RawRef,
) -> Option<(String, String)> {
    let mut segments: Vec<&str> = full_ref.split("::").collect();
    if segments.len() < 2 {
        return None;
    }
    segments.pop();
    let requested_symbol = rust_target_symbol(full_ref, short_name);

    for path in rust_module_path_candidates(&segments, caller_data, raw) {
        let path_refs = path.iter().map(String::as_str).collect::<Vec<_>>();
        if !matches!(path_refs.first().copied(), Some("crate" | "self" | "super")) {
            if let Some(target_file) = rust_workspace_file_for_segments(index, &path_refs) {
                return Some(rust_resolve_reexport_if_symbol_missing(
                    index,
                    target_file,
                    requested_symbol.clone(),
                ));
            }
        }

        let module_segments = rust_resolve_segments(caller_file, &path_refs)?;
        if let Some(target) =
            rust_inline_scoped_target(index, caller_file, &module_segments, &requested_symbol)
        {
            return Some(target);
        }
        if let Some(target_file) = rust_file_for_segments(index, caller_file, &module_segments) {
            return Some(rust_resolve_reexport_if_symbol_missing(
                index,
                target_file,
                requested_symbol.clone(),
            ));
        }
    }
    None
}

fn rust_target_symbol(full_ref: &str, short_name: &str) -> String {
    full_ref
        .rsplit("::")
        .next()
        .filter(|name| !name.is_empty())
        .unwrap_or(short_name)
        .to_string()
}

fn rust_resolve_reexport_if_symbol_missing(
    index: &ProjectIndex<'_>,
    target_file: String,
    target_symbol: String,
) -> (String, String) {
    if index
        .node_for_symbol(&target_file, &target_symbol)
        .is_some()
    {
        return (target_file, target_symbol);
    }
    if let Some(resolved) = resolve_exported_symbol(index, &target_file, &target_symbol, 0) {
        resolved
    } else {
        (target_file, target_symbol)
    }
}

fn rust_module_path_candidates(
    segments: &[&str],
    caller_data: &FileCallData,
    raw: &RawRef,
) -> Vec<Vec<String>> {
    let mut candidates = Vec::new();
    if let Some(first) = segments.first().copied() {
        for import in &caller_data.import_block.imports {
            if !rust_import_is_visible_to_call(import, raw) {
                continue;
            }
            let Some((local_name, mut path_segments)) = rust_module_alias_segments(import) else {
                continue;
            };
            if local_name == first {
                path_segments.extend(segments[1..].iter().map(|segment| (*segment).to_string()));
                rust_push_unique_path_candidate(&mut candidates, path_segments);
            }
        }
    }
    rust_push_unique_path_candidate(
        &mut candidates,
        segments
            .iter()
            .map(|segment| (*segment).to_string())
            .collect(),
    );
    candidates
}

fn rust_push_unique_path_candidate(candidates: &mut Vec<Vec<String>>, candidate: Vec<String>) {
    if !candidates.iter().any(|existing| existing == &candidate) {
        candidates.push(candidate);
    }
}

fn rust_import_is_visible_to_call(import: &ImportStatement, raw: &RawRef) -> bool {
    import.byte_range.start <= raw.byte_start
}

fn rust_module_alias_segments(import: &ImportStatement) -> Option<(String, Vec<String>)> {
    let path = import.module_path.trim().trim_end_matches(';').trim();
    if path.contains("::{") || path.contains('{') || path.contains('*') {
        return None;
    }
    let (path_without_alias, alias) = path
        .split_once(" as ")
        .map(|(left, right)| (left.trim(), Some(right.trim())))
        .unwrap_or((path, None));
    let segments = path_without_alias
        .split("::")
        .map(str::trim)
        .filter(|segment| !segment.is_empty())
        .collect::<Vec<_>>();
    let local_name = alias.or_else(|| segments.last().copied())?.to_string();
    if local_name.chars().next().is_some_and(char::is_uppercase) {
        return None;
    }
    Some((
        local_name,
        segments
            .into_iter()
            .map(|segment| segment.to_string())
            .collect(),
    ))
}

fn rust_inline_scoped_target(
    index: &ProjectIndex<'_>,
    caller_file: &str,
    module_segments: &[String],
    short_name: &str,
) -> Option<(String, String)> {
    let src_prefix = rust_src_prefix(caller_file);
    let mut file_paths = index.files.keys().cloned().collect::<Vec<_>>();
    file_paths.sort();
    if let Some(position) = file_paths.iter().position(|file| file == caller_file) {
        let caller = file_paths.remove(position);
        file_paths.insert(0, caller);
    }

    for file_path in file_paths {
        if index.lang_for(&file_path) != Some(LangId::Rust)
            || rust_src_prefix(&file_path) != src_prefix
        {
            continue;
        }
        let file_module_segments = rust_module_segments_for_rel(&file_path);
        if !module_segments.starts_with(&file_module_segments) {
            continue;
        }
        let scoped_segments = &module_segments[file_module_segments.len()..];
        if scoped_segments.is_empty() {
            continue;
        }
        let mut scoped_symbol = scoped_segments.join("::");
        scoped_symbol.push_str("::");
        scoped_symbol.push_str(short_name);
        if index.node_for_symbol(&file_path, &scoped_symbol).is_some() {
            return Some((file_path, scoped_symbol));
        }
    }
    None
}

fn rust_target_for_use(
    index: &ProjectIndex<'_>,
    caller_file: &str,
    import: &ImportStatement,
    short_name: &str,
) -> Option<(String, String)> {
    let path = import.module_path.trim().trim_end_matches(';');
    if let Some(brace_start) = path.find("::{") {
        let prefix = &path[..brace_start];
        if import.names.iter().any(|name| name == short_name) {
            let prefix_segments: Vec<&str> = prefix.split("::").collect();
            let module_segments = rust_resolve_segments(caller_file, &prefix_segments)?;
            let file = rust_file_for_segments(index, caller_file, &module_segments)?;
            return Some((file, short_name.to_string()));
        }
        return None;
    }

    let (path_without_alias, alias) = path
        .split_once(" as ")
        .map(|(left, right)| (left.trim(), Some(right.trim())))
        .unwrap_or((path, None));
    let segments: Vec<&str> = path_without_alias.split("::").collect();
    let imported = alias.or_else(|| segments.last().copied())?;
    if imported != short_name {
        return None;
    }
    if segments.len() < 2 {
        return None;
    }
    let module_segments = rust_resolve_segments(caller_file, &segments[..segments.len() - 1])?;
    let file = rust_file_for_segments(index, caller_file, &module_segments)?;
    Some((file, segments.last().unwrap_or(&short_name).to_string()))
}

fn rust_workspace_file_for_segments(index: &ProjectIndex<'_>, segments: &[&str]) -> Option<String> {
    let crate_name = segments.first().copied()?;
    let src_prefix = index.crate_src_prefix(crate_name)?;
    let module_segments = segments[1..]
        .iter()
        .map(|segment| segment.to_string())
        .collect::<Vec<_>>();
    rust_file_for_src_prefix(index, &src_prefix, &module_segments)
}

#[cfg(test)]
static WORKSPACE_CRATE_PREFIX_BUILD_COUNTS: OnceLock<Mutex<HashMap<PathBuf, usize>>> =
    OnceLock::new();

#[cfg(test)]
fn note_workspace_crate_prefix_build(project_root: &Path) {
    let mut counts = WORKSPACE_CRATE_PREFIX_BUILD_COUNTS
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .expect("workspace crate prefix build counts mutex poisoned");
    *counts.entry(project_root.to_path_buf()).or_default() += 1;
}

#[cfg(not(test))]
fn note_workspace_crate_prefix_build(_project_root: &Path) {}

#[cfg(test)]
fn reset_workspace_crate_prefix_build_count(project_root: &Path) {
    WORKSPACE_CRATE_PREFIX_BUILD_COUNTS
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .expect("workspace crate prefix build counts mutex poisoned")
        .remove(project_root);
}

#[cfg(test)]
fn workspace_crate_prefix_build_count(project_root: &Path) -> usize {
    WORKSPACE_CRATE_PREFIX_BUILD_COUNTS
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .expect("workspace crate prefix build counts mutex poisoned")
        .get(project_root)
        .copied()
        .unwrap_or(0)
}

/// Walk the project tree once and map every Rust crate name (package name with
/// `-` normalized to `_`, plus any explicit `[lib] name`) to its `src` prefix.
/// Replaces the previous per-ref tree walk: resolving 600k+ qualified refs no
/// longer re-walks the filesystem once per ref.
fn build_workspace_crate_prefixes(project_root: &Path) -> HashMap<String, String> {
    note_workspace_crate_prefix_build(project_root);
    let mut prefixes = HashMap::new();
    let mut stack = vec![project_root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let name = dir.file_name().and_then(|name| name.to_str()).unwrap_or("");
        if matches!(name, "target" | "node_modules" | ".git") {
            continue;
        }
        let manifest = dir.join("Cargo.toml");
        if manifest.is_file() {
            let crate_names = rust_manifest_crate_names(&manifest);
            if !crate_names.is_empty() {
                let src_prefix = relative_path(project_root, &canonicalize_path(&dir.join("src")));
                for crate_name in crate_names {
                    prefixes
                        .entry(crate_name)
                        .or_insert_with(|| src_prefix.clone());
                }
            }
        }
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            }
        }
    }
    prefixes
}

/// Extract the crate names a manifest defines: the normalized package name
/// (`-` -> `_`) and any explicit `[lib] name`. Returns both so a crate is
/// reachable by either spelling, matching the previous match semantics.
fn rust_manifest_crate_names(manifest: &Path) -> Vec<String> {
    let Ok(source) = std::fs::read_to_string(manifest) else {
        return Vec::new();
    };
    let mut in_lib = false;
    let mut package_name = None;
    let mut lib_name = None;
    for line in source.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') {
            in_lib = trimmed == "[lib]";
            continue;
        }
        let Some((key, value)) = trimmed.split_once('=') else {
            continue;
        };
        let key = key.trim();
        let value = value.trim().trim_matches('"');
        if in_lib && key == "name" {
            lib_name = Some(value.to_string());
        } else if !in_lib && key == "name" && package_name.is_none() {
            package_name = Some(value.to_string());
        }
    }
    let mut names = Vec::new();
    if let Some(lib) = lib_name {
        names.push(lib);
    }
    if let Some(package) = package_name {
        let normalized = package.replace('-', "_");
        if !names.contains(&normalized) {
            names.push(normalized);
        }
    }
    names
}

fn rust_resolve_segments(caller_file: &str, segments: &[&str]) -> Option<Vec<String>> {
    if segments.is_empty() {
        return Some(Vec::new());
    }
    let caller_segments = rust_module_segments_for_rel(caller_file);
    match segments[0] {
        "crate" => Some(segments[1..].iter().map(|item| item.to_string()).collect()),
        "self" => {
            let mut resolved = caller_segments;
            resolved.extend(segments[1..].iter().map(|item| item.to_string()));
            Some(resolved)
        }
        "super" => {
            let mut resolved = caller_segments;
            resolved.pop();
            resolved.extend(segments[1..].iter().map(|item| item.to_string()));
            Some(resolved)
        }
        _ => {
            let mut resolved = caller_segments;
            resolved.pop();
            resolved.extend(segments.iter().map(|item| item.to_string()));
            Some(resolved)
        }
    }
}

fn rust_file_for_segments(
    index: &ProjectIndex<'_>,
    caller_file: &str,
    segments: &[String],
) -> Option<String> {
    rust_file_for_src_prefix(index, &rust_src_prefix(caller_file), segments)
}

fn rust_file_for_src_prefix(
    index: &ProjectIndex<'_>,
    src_prefix: &str,
    segments: &[String],
) -> Option<String> {
    let candidate = if segments.is_empty() {
        [src_prefix, "lib.rs"].join("/")
    } else {
        format!("{}/{}.rs", src_prefix, segments.join("/"))
    };
    if index.files.contains_key(&candidate) {
        return Some(candidate);
    }
    if !segments.is_empty() {
        let mod_candidate = format!("{}/{}/mod.rs", src_prefix, segments.join("/"));
        if index.files.contains_key(&mod_candidate) {
            return Some(mod_candidate);
        }
    }
    None
}

fn rust_src_prefix(rel_path: &str) -> String {
    rel_path
        .split_once("/src/")
        .map(|(prefix, _)| format!("{prefix}/src"))
        .unwrap_or_else(|| "src".to_string())
}

fn rust_module_segments_for_rel(rel_path: &str) -> Vec<String> {
    let after_src = rel_path
        .split_once("/src/")
        .map(|(_, rest)| rest)
        .or_else(|| rel_path.strip_prefix("src/"))
        .unwrap_or(rel_path);
    if matches!(after_src, "lib.rs" | "main.rs") {
        return Vec::new();
    }
    if let Some(prefix) = after_src.strip_suffix("/mod.rs") {
        return prefix.split('/').map(|item| item.to_string()).collect();
    }
    after_src
        .strip_suffix(".rs")
        .unwrap_or(after_src)
        .split('/')
        .map(|item| item.to_string())
        .collect()
}

fn resolve_local_target(
    _index: &ProjectIndex<'_>,
    caller_file: &str,
    full_ref: &str,
    short_name: &str,
    caller_data: &FileCallData,
) -> Option<(String, String, String)> {
    if !callgraph::is_bare_callee(full_ref, short_name) {
        return None;
    }
    callgraph::resolve_symbol_query_in_data(caller_data, Path::new(caller_file), short_name)
        .ok()
        .map(|symbol| {
            (
                "resolved_local".to_string(),
                caller_file.to_string(),
                symbol,
            )
        })
}

impl<'a> ProjectIndex<'a> {
    fn from_parts(
        project_root: &Path,
        files: HashMap<String, DbFileIndex>,
        caller_data: HashMap<String, &'a FileCallData>,
        workspace_crate_prefixes: WorkspaceCratePrefixCache,
    ) -> Self {
        Self {
            project_root: project_root.to_path_buf(),
            files,
            caller_data,
            workspace_crate_prefixes,
        }
    }

    fn from_extracts(project_root: &Path, extracts: &'a [FileExtract]) -> Self {
        let mut files = HashMap::new();
        let mut caller_data = HashMap::new();
        for extract in extracts {
            let index = DbFileIndex::from_extract(project_root, extract);
            caller_data.insert(extract.rel_path.clone(), &extract.data);
            files.insert(extract.rel_path.clone(), index);
        }
        Self::from_parts(
            project_root,
            files,
            caller_data,
            WorkspaceCratePrefixCache::default(),
        )
    }

    fn from_db_and_callers(
        tx: &Transaction<'_>,
        project_root: &Path,
        caller_extracts: &'a HashMap<String, FileExtract>,
        workspace_crate_prefixes: WorkspaceCratePrefixCache,
    ) -> Result<Self> {
        let mut files = load_db_file_indexes(tx, project_root)?;
        let mut caller_data = HashMap::new();
        for (rel_path, extract) in caller_extracts {
            files.insert(
                rel_path.clone(),
                DbFileIndex::from_extract(project_root, extract),
            );
            caller_data.insert(rel_path.clone(), &extract.data);
        }
        Ok(Self::from_parts(
            project_root,
            files,
            caller_data,
            workspace_crate_prefixes,
        ))
    }

    fn lang_for(&self, rel_path: &str) -> Option<LangId> {
        self.files.get(rel_path).and_then(|file| file.lang)
    }

    fn module_target(&self, caller_file: &str, module_path: &str) -> Option<String> {
        self.files
            .get(caller_file)
            .and_then(|file| file.module_targets.get(module_path).cloned().flatten())
    }

    fn reexports_for(&self, rel_path: &str) -> &[ReexportIndex] {
        self.files
            .get(rel_path)
            .map(|file| file.reexports.as_slice())
            .unwrap_or(&[])
    }

    fn node_for_symbol(&self, rel_path: &str, symbol: &str) -> Option<String> {
        self.files.get(rel_path).and_then(|file| {
            file.node_by_scoped
                .get(symbol)
                .cloned()
                .or_else(|| file.node_by_bare.get(symbol).cloned())
        })
    }
}

impl DbFileIndex {
    fn from_extract(project_root: &Path, extract: &FileExtract) -> Self {
        let mut node_by_scoped = HashMap::new();
        let mut node_by_bare = HashMap::new();
        for node in &extract.nodes {
            node_by_scoped.insert(node.scoped_name.clone(), node.id.clone());
            node_by_bare
                .entry(node.name.clone())
                .or_insert(node.id.clone());
        }
        let mut export_aliases = HashMap::new();
        for raw_ref in &extract.raw_refs {
            if raw_ref.kind == "export_alias" {
                if let (Some(exported), Some(source_symbol)) =
                    (&raw_ref.local_name, &raw_ref.requested_name)
                {
                    export_aliases.insert(exported.clone(), source_symbol.clone());
                }
            }
        }
        let mut module_targets = HashMap::new();
        let mut reexports = Vec::new();
        for raw_ref in &extract.raw_refs {
            if !matches!(raw_ref.kind.as_str(), "import" | "reexport") {
                continue;
            }
            let Some(module_path) = &raw_ref.module_path else {
                continue;
            };
            let target_file = module_target_from_dependencies(project_root, &raw_ref.dependencies);
            module_targets
                .entry(module_path.clone())
                .or_insert_with(|| target_file.clone());
            if raw_ref.kind == "reexport" {
                reexports.push(reexport_index_from_raw(raw_ref, target_file));
            }
        }
        Self {
            lang: Some(extract.lang),
            exports: extract.data.exported_symbols.iter().cloned().collect(),
            default_export: extract.data.default_export_symbol.clone(),
            export_aliases,
            node_by_scoped,
            node_by_bare,
            module_targets,
            reexports,
        }
    }
}

fn load_db_file_indexes(
    tx: &Transaction<'_>,
    project_root: &Path,
) -> Result<HashMap<String, DbFileIndex>> {
    let mut files = HashMap::new();
    let mut stmt = tx.prepare("SELECT path, lang FROM files")?;
    let rows = stmt.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    })?;
    for row in rows {
        let (rel_path, lang) = row?;
        files.insert(
            rel_path.clone(),
            DbFileIndex {
                lang: lang_from_label(&lang),
                exports: HashSet::new(),
                default_export: None,
                export_aliases: HashMap::new(),
                node_by_scoped: HashMap::new(),
                node_by_bare: HashMap::new(),
                module_targets: HashMap::new(),
                reexports: Vec::new(),
            },
        );
    }

    let mut node_stmt = tx.prepare(
        "SELECT file_path, id, name, scoped_name, exported, is_default_export FROM nodes",
    )?;
    let nodes = node_stmt.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, String>(3)?,
            row.get::<_, i64>(4)? != 0,
            row.get::<_, i64>(5)? != 0,
        ))
    })?;
    for row in nodes {
        let (file_path, id, name, scoped_name, exported, is_default_export) = row?;
        let file = files
            .entry(file_path.clone())
            .or_insert_with(|| DbFileIndex {
                lang: None,
                exports: HashSet::new(),
                default_export: None,
                export_aliases: HashMap::new(),
                node_by_scoped: HashMap::new(),
                node_by_bare: HashMap::new(),
                module_targets: HashMap::new(),
                reexports: Vec::new(),
            });
        if exported {
            file.exports.insert(name.clone());
            file.exports.insert(scoped_name.clone());
        }
        if is_default_export {
            file.default_export = Some(scoped_name.clone());
        }
        file.node_by_scoped.insert(scoped_name, id.clone());
        file.node_by_bare.entry(name).or_insert(id);
    }
    let file_keys: HashSet<String> = files.keys().cloned().collect();
    // Persisted caller extracts supply import targets. Only reexports from other
    // files need dependency reconstruction, and their caller dependencies are
    // loaded once instead of issuing repeated SQLite queries per reference.
    let dependencies_by_file = load_file_dependencies_index(tx)?;
    let mut ref_stmt = tx.prepare(
        "SELECT ref_id, caller_file, kind, module_path, full_ref, wildcard, local_name, requested_name
         FROM refs WHERE kind IN ('reexport', 'export_alias')",
    )?;
    let ref_rows = ref_stmt.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, Option<String>>(3)?,
            row.get::<_, Option<String>>(4)?,
            row.get::<_, i64>(5)? != 0,
            row.get::<_, Option<String>>(6)?,
            row.get::<_, Option<String>>(7)?,
        ))
    })?;
    for row in ref_rows {
        let (
            ref_id,
            caller_file,
            kind,
            module_path,
            full_ref,
            wildcard,
            local_name,
            requested_name,
        ) = row?;
        if kind == "export_alias" {
            if let (Some(exported), Some(source_symbol), Some(file)) =
                (local_name, requested_name, files.get_mut(&caller_file))
            {
                file.export_aliases.insert(exported, source_symbol);
            }
            continue;
        }
        let Some(module_path) = module_path else {
            continue;
        };
        let file_deps = dependencies_by_file
            .get(&caller_file)
            .cloned()
            .unwrap_or_default();
        let deps = stored_dependencies_for_module(
            project_root,
            &caller_file,
            &module_path,
            &file_deps,
            &file_keys,
        );
        let target_file = deps
            .iter()
            .find(|dep| file_keys.contains(*dep))
            .map(|dep| relative_path(project_root, &canonicalize_path(&project_root.join(dep))));
        if let Some(file) = files.get_mut(&caller_file) {
            file.module_targets
                .entry(module_path.clone())
                .or_insert_with(|| target_file.clone());
            if kind == "reexport" {
                let raw = RawRef {
                    ref_id,
                    caller_node: None,
                    caller_symbol: None,
                    caller_file,
                    kind,
                    short_name: None,
                    full_ref,
                    module_path: Some(module_path),
                    import_kind: Some("reexport".to_string()),
                    local_name: None,
                    requested_name: None,
                    namespace_alias: None,
                    wildcard,
                    line: 0,
                    byte_start: 0,
                    byte_end: 0,
                    dependencies: deps,
                };
                file.reexports
                    .push(reexport_index_from_raw(&raw, target_file));
            }
        }
    }

    Ok(files)
}

fn stored_dependencies_for_module(
    project_root: &Path,
    caller_file: &str,
    module_path: &str,
    caller_dependencies: &BTreeSet<String>,
    indexed_files: &HashSet<String>,
) -> BTreeSet<String> {
    let caller_path = project_root.join(caller_file);
    let mut candidates = rust_module_dependencies(project_root, &caller_path, module_path);
    if module_path.starts_with('.') {
        let caller_dir = caller_path.parent().unwrap_or(project_root);
        for candidate in relative_module_candidates(&caller_dir.join(module_path)) {
            let normalized = if candidate.is_file() {
                canonicalize_path(&candidate)
            } else {
                candidate
            };
            candidates.insert(relative_path(project_root, &normalized));
        }
    }
    let exact = candidates
        .intersection(caller_dependencies)
        .filter(|dependency| indexed_files.contains(*dependency))
        .cloned()
        .collect::<BTreeSet<_>>();
    if !exact.is_empty() || module_path.starts_with('.') {
        return exact;
    }

    let module_path = rust_module_path_without_alias_or_use_list(module_path)
        .trim_matches(|character| matches!(character, '\'' | '"'));
    let package_name = module_path
        .split('/')
        .next_back()
        .unwrap_or(module_path)
        .replace('_', "-");
    let matched = caller_dependencies
        .iter()
        .filter(|dependency| indexed_files.contains(*dependency))
        .filter(|dependency| {
            dependency.as_str() == module_path
                || dependency.ends_with(&format!("/{module_path}"))
                || Path::new(dependency).components().any(|component| {
                    component.as_os_str().to_string_lossy().replace('_', "-") == package_name
                })
        })
        .cloned()
        .collect::<BTreeSet<_>>();
    if matched.len() == 1 {
        matched
    } else {
        BTreeSet::new()
    }
}

fn load_file_dependencies_index(tx: &Transaction<'_>) -> Result<HashMap<String, BTreeSet<String>>> {
    let mut by_file: HashMap<String, BTreeSet<String>> = HashMap::new();
    let mut stmt = tx.prepare("SELECT file_path, dep_file FROM file_dependencies")?;
    let rows = stmt.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    })?;
    for row in rows {
        let (file_path, dependency) = row?;
        by_file.entry(file_path).or_default().insert(dependency);
    }
    Ok(by_file)
}

struct ColdBuildInsertStatements<'stmt> {
    file: Statement<'stmt>,
    node: Statement<'stmt>,
    file_dependency: Statement<'stmt>,
    dispatch_hint: Statement<'stmt>,
    backend_state: Statement<'stmt>,
    reference: Statement<'stmt>,
    edge: Statement<'stmt>,
}

impl<'stmt> ColdBuildInsertStatements<'stmt> {
    fn new(tx: &'stmt Transaction<'_>) -> Result<Self> {
        Ok(Self {
            file: tx.prepare(
                "INSERT OR REPLACE INTO files(
                    path, content_hash, mtime_ns, size, lang, is_dead_code_root,
                    is_public_api, surface_fingerprint, indexed_at
                ) VALUES(?1, ?2, ?3, ?4, ?5, 0, 0, ?6, ?7)",
            )?,
            node: tx.prepare(
                "INSERT OR REPLACE INTO nodes(
                    id, file_path, name, scoped_name, kind, start_line, start_col,
                    end_line, end_col, range_ordinal, signature, exported,
                    is_default_export, is_type_like, is_callgraph_entry_point, provenance
                ) VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16)",
            )?,
            file_dependency: tx.prepare(
                "INSERT OR IGNORE INTO file_dependencies(file_path, dep_file) VALUES(?1, ?2)",
            )?,
            dispatch_hint: tx.prepare(
                "INSERT OR REPLACE INTO dispatch_hints(
                    id, method_name, caller_node, file, line, byte_start, byte_end, provenance
                ) VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            )?,
            backend_state: tx.prepare(
                "INSERT OR REPLACE INTO backend_file_state(
                    backend, workspace_root, file_path, content_hash, status, updated_at
                ) VALUES(?1, ?2, ?3, ?4, ?5, ?6)",
            )?,
            reference: tx.prepare(
                "INSERT OR REPLACE INTO refs(
                    ref_id, caller_node, caller_file, kind, short_name, full_ref, module_path,
                    import_kind, local_name, requested_name, namespace_alias, wildcard, line,
                    byte_start, byte_end, status, target_node, target_file, target_symbol,
                    provenance
                ) VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20)",
            )?,
            edge: tx.prepare(
                "INSERT OR REPLACE INTO edges(
                    edge_id, ref_id, source_node, target_node, target_file, target_symbol,
                    kind, line, provenance
                ) VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            )?,
        })
    }
}

fn insert_file_extract_prepared(
    statements: &mut ColdBuildInsertStatements<'_>,
    workspace_root: &str,
    extract: &FileExtract,
) -> Result<()> {
    statements.file.execute(params![
        extract.rel_path,
        hash_to_hex(extract.freshness.content_hash),
        system_time_to_ns(extract.freshness.mtime),
        extract.freshness.size as i64,
        lang_label(extract.lang),
        extract.surface_fingerprint,
        unix_seconds_now(),
    ])?;
    for node in &extract.nodes {
        statements.node.execute(params![
            node.id,
            node.file_path,
            node.name,
            node.scoped_name,
            node.kind,
            node.range.start_line as i64,
            node.range.start_col as i64,
            node.range.end_line as i64,
            node.range.end_col as i64,
            node.range_ordinal as i64,
            node.signature,
            bool_int(node.exported),
            bool_int(node.is_default_export),
            bool_int(node.is_type_like),
            bool_int(node.is_callgraph_entry_point),
            PROVENANCE_TREESITTER,
        ])?;
    }

    let mut dependencies = BTreeSet::new();
    for raw_ref in &extract.raw_refs {
        dependencies.extend(raw_ref.dependencies.iter().cloned());
    }
    for dep_file in &dependencies {
        statements
            .file_dependency
            .execute(params![extract.rel_path, dep_file])?;
    }

    for hint in &extract.dispatch_hints {
        statements.dispatch_hint.execute(params![
            hint.id,
            hint.method_name,
            hint.caller_node,
            hint.file,
            hint.line as i64,
            hint.byte_start as i64,
            hint.byte_end as i64,
            PROVENANCE_TREESITTER,
        ])?;
    }
    insert_backend_state_prepared(
        &mut statements.backend_state,
        workspace_root,
        &extract.rel_path,
        Some(&extract.freshness.content_hash),
        "fresh",
    )?;
    Ok(())
}

fn insert_backend_state_prepared(
    stmt: &mut Statement<'_>,
    workspace_root: &str,
    rel_path: &str,
    content_hash: Option<&blake3::Hash>,
    status: &str,
) -> Result<()> {
    let hash = content_hash
        .map(|hash| hash_to_hex(*hash))
        .unwrap_or_else(|| hash_to_hex(cache_freshness::zero_hash()));
    stmt.execute(params![
        BACKEND_TREESITTER,
        workspace_root,
        rel_path,
        hash,
        status,
        unix_seconds_now(),
    ])?;
    Ok(())
}

fn insert_resolved_ref_prepared(
    statements: &mut ColdBuildInsertStatements<'_>,
    resolved: &ResolvedRef,
) -> Result<()> {
    let raw = &resolved.raw;
    debug_assert!(resolved.dependencies.is_superset(&raw.dependencies));
    statements.reference.execute(params![
        raw.ref_id,
        raw.caller_node,
        raw.caller_file,
        raw.kind,
        raw.short_name,
        raw.full_ref,
        raw.module_path,
        raw.import_kind,
        raw.local_name,
        raw.requested_name,
        raw.namespace_alias,
        bool_int(raw.wildcard),
        raw.line as i64,
        raw.byte_start as i64,
        raw.byte_end as i64,
        resolved.status,
        resolved.target_node,
        resolved.target_file,
        resolved.target_symbol,
        PROVENANCE_TREESITTER,
    ])?;
    if let Some(edge) = &resolved.edge {
        statements.edge.execute(params![
            edge.edge_id,
            raw.ref_id,
            edge.source_node,
            edge.target_node,
            edge.target_file,
            edge.target_symbol,
            edge.kind,
            edge.line as i64,
            PROVENANCE_TREESITTER,
        ])?;
    }
    Ok(())
}

fn insert_file_extract(
    tx: &Transaction<'_>,
    project_root: &Path,
    extract: &FileExtract,
) -> Result<()> {
    tx.execute(
        "INSERT OR REPLACE INTO files(
            path, content_hash, mtime_ns, size, lang, is_dead_code_root,
            is_public_api, surface_fingerprint, indexed_at
        ) VALUES(?1, ?2, ?3, ?4, ?5, 0, 0, ?6, ?7)",
        params![
            extract.rel_path,
            hash_to_hex(extract.freshness.content_hash),
            system_time_to_ns(extract.freshness.mtime),
            extract.freshness.size as i64,
            lang_label(extract.lang),
            extract.surface_fingerprint,
            unix_seconds_now(),
        ],
    )?;
    for node in &extract.nodes {
        tx.execute(
            "INSERT OR REPLACE INTO nodes(
                id, file_path, name, scoped_name, kind, start_line, start_col,
                end_line, end_col, range_ordinal, signature, exported,
                is_default_export, is_type_like, is_callgraph_entry_point, provenance
            ) VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16)",
            params![
                node.id,
                node.file_path,
                node.name,
                node.scoped_name,
                node.kind,
                node.range.start_line as i64,
                node.range.start_col as i64,
                node.range.end_line as i64,
                node.range.end_col as i64,
                node.range_ordinal as i64,
                node.signature,
                bool_int(node.exported),
                bool_int(node.is_default_export),
                bool_int(node.is_type_like),
                bool_int(node.is_callgraph_entry_point),
                PROVENANCE_TREESITTER,
            ],
        )?;
    }
    let mut dependencies = BTreeSet::new();
    for raw_ref in &extract.raw_refs {
        dependencies.extend(raw_ref.dependencies.iter().cloned());
    }
    insert_file_dependencies(tx, &extract.rel_path, &dependencies)?;

    for hint in &extract.dispatch_hints {
        tx.execute(
            "INSERT OR REPLACE INTO dispatch_hints(
                id, method_name, caller_node, file, line, byte_start, byte_end, provenance
            ) VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                hint.id,
                hint.method_name,
                hint.caller_node,
                hint.file,
                hint.line as i64,
                hint.byte_start as i64,
                hint.byte_end as i64,
                PROVENANCE_TREESITTER,
            ],
        )?;
    }
    mark_backend_state(
        tx,
        project_root,
        &extract.rel_path,
        Some(&extract.freshness.content_hash),
        "fresh",
    )?;
    Ok(())
}

fn insert_file_dependencies(
    tx: &Transaction<'_>,
    file_path: &str,
    dependencies: &BTreeSet<String>,
) -> Result<()> {
    for dep_file in dependencies {
        tx.execute(
            "INSERT OR IGNORE INTO file_dependencies(file_path, dep_file) VALUES(?1, ?2)",
            params![file_path, dep_file],
        )?;
    }
    Ok(())
}

fn insert_resolved_ref(tx: &Transaction<'_>, resolved: &ResolvedRef) -> Result<()> {
    let raw = &resolved.raw;
    debug_assert!(resolved.dependencies.is_superset(&raw.dependencies));
    tx.execute(
        "INSERT OR REPLACE INTO refs(
            ref_id, caller_node, caller_file, kind, short_name, full_ref, module_path,
            import_kind, local_name, requested_name, namespace_alias, wildcard, line,
            byte_start, byte_end, status, target_node, target_file, target_symbol,
            provenance
        ) VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20)",
        params![
            raw.ref_id,
            raw.caller_node,
            raw.caller_file,
            raw.kind,
            raw.short_name,
            raw.full_ref,
            raw.module_path,
            raw.import_kind,
            raw.local_name,
            raw.requested_name,
            raw.namespace_alias,
            bool_int(raw.wildcard),
            raw.line as i64,
            raw.byte_start as i64,
            raw.byte_end as i64,
            resolved.status,
            resolved.target_node,
            resolved.target_file,
            resolved.target_symbol,
            PROVENANCE_TREESITTER,
        ],
    )?;
    if let Some(edge) = &resolved.edge {
        tx.execute(
            "INSERT OR REPLACE INTO edges(
                edge_id, ref_id, source_node, target_node, target_file, target_symbol,
                kind, line, provenance
            ) VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                edge.edge_id,
                raw.ref_id,
                edge.source_node,
                edge.target_node,
                edge.target_file,
                edge.target_symbol,
                edge.kind,
                edge.line as i64,
                PROVENANCE_TREESITTER,
            ],
        )?;
    }
    Ok(())
}

fn insert_method_dispatch_edges(
    tx: &Transaction<'_>,
    project_root: &Path,
    caller_files: Option<&BTreeSet<String>>,
) -> Result<usize> {
    let references = load_name_match_refs(tx, caller_files)?;
    if references.is_empty() {
        return Ok(0);
    }

    let mut candidates_by_name: HashMap<(String, String), Vec<NameMatchCandidate>> = HashMap::new();
    let mut source_cache: DispatchSourceCache = HashMap::new();
    let mut inserted = 0usize;
    for reference in references {
        let key = (reference.method_name.clone(), reference.lang.clone());
        let candidates = match candidates_by_name.entry(key) {
            Entry::Occupied(entry) => entry.into_mut(),
            Entry::Vacant(entry) => {
                let candidates =
                    load_name_match_candidates(tx, &reference.method_name, &reference.lang)?;
                entry.insert(candidates)
            }
        };

        match infer_receiver_type_state(project_root, &reference, &mut source_cache) {
            ReceiverTypeInference::Known(receiver_type) => {
                let Some(candidate) =
                    select_type_match_candidate(&reference, candidates.as_slice(), &receiver_type)
                else {
                    continue;
                };
                insert_method_dispatch_edge(tx, &reference, &candidate, PROVENANCE_TYPE_MATCH)?;
                inserted += 1;
                continue;
            }
            ReceiverTypeInference::RustDirectSelfField {
                receiver_type,
                declaration_file,
                module_scope,
            } => {
                let Some(candidate) = select_rust_direct_self_field_candidate(
                    project_root,
                    &reference,
                    candidates.as_slice(),
                    &receiver_type,
                    &declaration_file,
                    &module_scope,
                    &mut source_cache,
                ) else {
                    continue;
                };
                insert_method_dispatch_edge(tx, &reference, &candidate, PROVENANCE_TYPE_MATCH)?;
                inserted += 1;
                continue;
            }
            ReceiverTypeInference::KnownButUnresolved => continue,
            ReceiverTypeInference::Unknown => {}
        }

        if method_name_match_denylisted(&reference.method_name) {
            continue;
        }

        let Some(candidate) = select_name_match_candidate(&reference, candidates.as_slice()) else {
            continue;
        };
        insert_method_dispatch_edge(tx, &reference, &candidate, PROVENANCE_NAME_MATCH)?;
        inserted += 1;
    }
    Ok(inserted)
}

fn insert_method_dispatch_edges_chunked(
    tx: &Transaction<'_>,
    project_root: &Path,
    caller_files: &BTreeSet<String>,
    chunk_size: usize,
) -> Result<usize> {
    if caller_files.is_empty() {
        return Ok(0);
    }
    if chunk_size == 0 || caller_files.len() <= chunk_size {
        return insert_method_dispatch_edges(tx, project_root, Some(caller_files));
    }

    let mut inserted = 0usize;
    let mut batch = BTreeSet::new();
    for caller_file in caller_files {
        batch.insert(caller_file.clone());
        if batch.len() == chunk_size {
            inserted += insert_method_dispatch_edges(tx, project_root, Some(&batch))?;
            batch.clear();
        }
    }
    if !batch.is_empty() {
        inserted += insert_method_dispatch_edges(tx, project_root, Some(&batch))?;
    }
    Ok(inserted)
}

fn insert_method_dispatch_edge(
    tx: &Transaction<'_>,
    reference: &NameMatchRef,
    candidate: &NameMatchCandidate,
    provenance: &str,
) -> Result<()> {
    tx.execute(
        "INSERT OR REPLACE INTO edges(
            edge_id, ref_id, source_node, target_node, target_file, target_symbol,
            kind, line, provenance
        ) VALUES(?1, ?2, ?3, ?4, ?5, ?6, 'call', ?7, ?8)",
        params![
            ref_id(&[&reference.ref_id, provenance, "edge"]),
            &reference.ref_id,
            &reference.caller_node,
            &candidate.node_id,
            &candidate.file_path,
            &candidate.scoped_name,
            reference.line as i64,
            provenance,
        ],
    )?;
    Ok(())
}

fn delete_method_dispatch_edges_for_callers(
    tx: &Transaction<'_>,
    caller_files: &BTreeSet<String>,
) -> Result<()> {
    if caller_files.is_empty() {
        return Ok(());
    }

    let mut stmt = tx.prepare(
        "DELETE FROM edges
         WHERE provenance IN (?1, ?2)
           AND ref_id IN (SELECT ref_id FROM refs WHERE caller_file = ?3)",
    )?;
    for caller_file in caller_files {
        stmt.execute(params![
            PROVENANCE_NAME_MATCH,
            PROVENANCE_TYPE_MATCH,
            caller_file
        ])?;
    }
    Ok(())
}

fn load_name_match_refs(
    tx: &Transaction<'_>,
    caller_files: Option<&BTreeSet<String>>,
) -> Result<Vec<NameMatchRef>> {
    let base_sql = "SELECT r.ref_id, r.caller_node, r.caller_file, n.scoped_name,
                           n.signature, r.short_name, r.full_ref, r.line, f.lang
                    FROM refs r
                    JOIN files f ON f.path = r.caller_file
                    JOIN nodes n ON n.id = r.caller_node
                    WHERE r.kind = 'call'
                      AND r.status = 'unresolved'
                      AND r.caller_node IS NOT NULL
                      AND r.full_ref IS NOT NULL
                      AND (r.full_ref LIKE '%.%' OR r.full_ref LIKE '%::%' OR r.full_ref LIKE '%->%')
                      AND NOT EXISTS (
                          SELECT 1 FROM edges e WHERE e.ref_id = r.ref_id AND e.kind = 'call'
                      )";
    let mut references = Vec::new();

    if let Some(caller_files) = caller_files {
        if caller_files.is_empty() {
            return Ok(references);
        }
        let sql = format!(
            "{base_sql} AND r.caller_file = ?1 ORDER BY r.caller_file, r.byte_start, r.ref_id"
        );
        let mut stmt = tx.prepare(&sql)?;
        for caller_file in caller_files {
            let rows = stmt.query_map(params![caller_file], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, Option<String>>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, Option<String>>(4)?,
                    row.get::<_, Option<String>>(5)?,
                    row.get::<_, Option<String>>(6)?,
                    row.get::<_, i64>(7)?,
                    row.get::<_, String>(8)?,
                ))
            })?;
            for row in rows {
                let (
                    ref_id,
                    caller_node,
                    caller_file,
                    caller_symbol,
                    caller_signature,
                    short_name,
                    full_ref,
                    line,
                    lang,
                ) = row?;
                if let Some(reference) = name_match_ref_from_parts(
                    ref_id,
                    caller_node,
                    caller_file,
                    caller_symbol,
                    caller_signature,
                    short_name,
                    full_ref,
                    line,
                    lang,
                ) {
                    references.push(reference);
                }
            }
        }
        return Ok(references);
    }

    let sql = format!("{base_sql} ORDER BY r.caller_file, r.byte_start, r.ref_id");
    let mut stmt = tx.prepare(&sql)?;
    let rows = stmt.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, Option<String>>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, String>(3)?,
            row.get::<_, Option<String>>(4)?,
            row.get::<_, Option<String>>(5)?,
            row.get::<_, Option<String>>(6)?,
            row.get::<_, i64>(7)?,
            row.get::<_, String>(8)?,
        ))
    })?;
    for row in rows {
        let (
            ref_id,
            caller_node,
            caller_file,
            caller_symbol,
            caller_signature,
            short_name,
            full_ref,
            line,
            lang,
        ) = row?;
        if let Some(reference) = name_match_ref_from_parts(
            ref_id,
            caller_node,
            caller_file,
            caller_symbol,
            caller_signature,
            short_name,
            full_ref,
            line,
            lang,
        ) {
            references.push(reference);
        }
    }
    Ok(references)
}

#[allow(clippy::too_many_arguments)]
fn name_match_ref_from_parts(
    ref_id: String,
    caller_node: Option<String>,
    caller_file: String,
    caller_symbol: String,
    caller_signature: Option<String>,
    short_name: Option<String>,
    full_ref: Option<String>,
    line: i64,
    lang: String,
) -> Option<NameMatchRef> {
    let caller_node = caller_node?;
    let full_ref = full_ref?;
    let (receiver_expression, receiver, member, colon_dispatch) = parse_method_dispatch(&full_ref)?;
    let method_name = if member.is_empty() {
        short_name.as_deref()?.to_string()
    } else {
        member
    };
    Some(NameMatchRef {
        ref_id,
        caller_node,
        caller_file,
        caller_symbol,
        caller_signature,
        receiver_expression,
        receiver,
        method_name,
        colon_dispatch,
        line: line.max(0) as u32,
        lang,
    })
}

fn parse_method_dispatch(full_ref: &str) -> Option<(String, String, String, bool)> {
    let dot = full_ref.rfind('.').map(|index| (index, 1usize, false));
    let colon = full_ref.rfind("::").map(|index| (index, 2usize, true));
    let arrow = full_ref.rfind("->").map(|index| (index, 2usize, false));
    let (delimiter, delimiter_len, colon_dispatch) = [dot, colon, arrow]
        .into_iter()
        .flatten()
        .max_by_key(|(index, _, _)| *index)?;
    if delimiter == 0 {
        return None;
    }
    let member_start = delimiter + delimiter_len;
    if member_start >= full_ref.len() {
        return None;
    }
    let receiver_expression = full_ref[..delimiter].trim();
    let receiver = last_name_segment(receiver_expression).trim();
    let member = &full_ref[member_start..];
    if receiver.is_empty() || member.is_empty() {
        return None;
    }
    Some((
        receiver_expression.to_string(),
        receiver.to_string(),
        member.to_string(),
        colon_dispatch,
    ))
}

fn last_name_segment(value: &str) -> &str {
    value
        .rsplit(['.', ':', '/', '\\', '-', '>'])
        .find(|segment| !segment.is_empty())
        .unwrap_or(value)
}

fn load_name_match_candidates(
    tx: &Transaction<'_>,
    method_name: &str,
    lang: &str,
) -> Result<Vec<NameMatchCandidate>> {
    let mut stmt = tx.prepare(
        "SELECT n.id, n.file_path, n.scoped_name, n.kind, n.start_line
         FROM nodes n JOIN files f ON f.path = n.file_path
         WHERE n.name = ?1
           AND f.lang = ?2
           AND n.kind IN ('method', 'function')
         ORDER BY n.file_path, n.scoped_name, n.start_line, n.start_col, n.id",
    )?;
    let rows = stmt.query_map(params![method_name, lang], |row| {
        Ok(NameMatchCandidate {
            node_id: row.get(0)?,
            file_path: row.get(1)?,
            scoped_name: row.get(2)?,
            kind: row.get(3)?,
            start_line: (row.get::<_, i64>(4)?.max(0) as u32).saturating_add(1),
        })
    })?;
    rows.collect::<std::result::Result<Vec<_>, _>>()
        .map_err(Into::into)
}

struct ParsedDispatchSource {
    source: String,
    tree: tree_sitter::Tree,
}

type DispatchSourceCache = HashMap<(String, String), Option<ParsedDispatchSource>>;

#[derive(Debug, Clone, PartialEq, Eq)]
enum ReceiverTypeInference {
    Unknown,
    Known(String),
    RustDirectSelfField {
        receiver_type: String,
        declaration_file: String,
        module_scope: Vec<(usize, usize)>,
    },
    KnownButUnresolved,
}

#[cfg(test)]
fn infer_receiver_type(
    project_root: &Path,
    reference: &NameMatchRef,
    source_cache: &mut DispatchSourceCache,
) -> Option<String> {
    match infer_receiver_type_state(project_root, reference, source_cache) {
        ReceiverTypeInference::Known(receiver_type)
        | ReceiverTypeInference::RustDirectSelfField { receiver_type, .. } => Some(receiver_type),
        ReceiverTypeInference::Unknown | ReceiverTypeInference::KnownButUnresolved => None,
    }
}

fn infer_receiver_type_state(
    project_root: &Path,
    reference: &NameMatchRef,
    source_cache: &mut DispatchSourceCache,
) -> ReceiverTypeInference {
    let known = |receiver_type| ReceiverTypeInference::Known(receiver_type);
    match reference.lang.as_str() {
        "rust" => infer_rust_receiver_type(project_root, reference, source_cache),
        "java" => {
            infer_java_like_receiver_type(project_root, reference, LangId::Java, source_cache)
                .map(known)
                .unwrap_or(ReceiverTypeInference::Unknown)
        }
        "kotlin" => {
            infer_java_like_receiver_type(project_root, reference, LangId::Kotlin, source_cache)
                .map(known)
                .unwrap_or(ReceiverTypeInference::Unknown)
        }
        "cpp" => infer_cpp_receiver_type(project_root, reference, source_cache)
            .map(known)
            .unwrap_or(ReceiverTypeInference::Unknown),
        _ => ReceiverTypeInference::Unknown,
    }
}

fn parse_dispatch_source(
    project_root: &Path,
    caller_file: &str,
    lang: LangId,
) -> Option<ParsedDispatchSource> {
    let source = std::fs::read_to_string(project_root.join(caller_file)).ok()?;
    let grammar = crate::parser::grammar_for(lang);
    let mut parser = tree_sitter::Parser::new();
    parser.set_language(&grammar).ok()?;
    let tree = parser.parse(&source, None)?;
    Some(ParsedDispatchSource { source, tree })
}

fn parsed_dispatch_source<'a>(
    project_root: &Path,
    reference: &NameMatchRef,
    lang: LangId,
    source_cache: &'a mut DispatchSourceCache,
) -> Option<&'a ParsedDispatchSource> {
    parsed_dispatch_source_for_file(
        project_root,
        &reference.caller_file,
        &reference.lang,
        lang,
        source_cache,
    )
}

fn parsed_dispatch_source_for_file<'a>(
    project_root: &Path,
    file_path: &str,
    lang_label: &str,
    lang: LangId,
    source_cache: &'a mut DispatchSourceCache,
) -> Option<&'a ParsedDispatchSource> {
    let key = (file_path.to_string(), lang_label.to_string());
    source_cache
        .entry(key)
        .or_insert_with(|| parse_dispatch_source(project_root, file_path, lang))
        .as_ref()
}

fn infer_java_like_receiver_type(
    project_root: &Path,
    reference: &NameMatchRef,
    lang: LangId,
    source_cache: &mut DispatchSourceCache,
) -> Option<String> {
    if reference.colon_dispatch || !receiver_is_bare_identifier(&reference.receiver) {
        return None;
    }

    let parsed = parsed_dispatch_source(project_root, reference, lang, source_cache)?;
    let root = parsed.tree.root_node();
    let type_node = find_enclosing_java_like_type_node(root, &parsed.source, reference, lang);

    let callable_scope = type_node
        .and_then(|node| {
            find_enclosing_java_like_callable_node(node, &parsed.source, reference, lang)
        })
        .or_else(|| find_enclosing_java_like_callable_node(root, &parsed.source, reference, lang));

    if let Some(callable_scope) = callable_scope {
        if let Some(receiver_type) = infer_java_like_local_receiver_type(
            callable_scope,
            &parsed.source,
            &reference.receiver,
            reference.line.max(1),
            lang,
        ) {
            return Some(receiver_type);
        }
    }

    type_node.and_then(|node| {
        infer_java_like_field_receiver_type(node, &parsed.source, &reference.receiver, lang)
    })
}

fn infer_cpp_receiver_type(
    project_root: &Path,
    reference: &NameMatchRef,
    source_cache: &mut DispatchSourceCache,
) -> Option<String> {
    if reference.colon_dispatch || !receiver_is_bare_identifier(&reference.receiver) {
        return None;
    }

    let parsed = parsed_dispatch_source(project_root, reference, LangId::Cpp, source_cache)?;
    let root = parsed.tree.root_node();
    let scope = find_enclosing_cpp_callable_node(root, &parsed.source, reference).unwrap_or(root);
    infer_cpp_receiver_type_from_scope(
        scope,
        &parsed.source,
        &reference.receiver,
        reference.line.max(1),
    )
}

fn find_enclosing_java_like_type_node<'tree>(
    root: tree_sitter::Node<'tree>,
    source: &str,
    reference: &NameMatchRef,
    lang: LangId,
) -> Option<tree_sitter::Node<'tree>> {
    let expected_type = enclosing_type_from_scoped_name(&reference.caller_symbol)
        .and_then(|name| simple_type_name(&name));
    let line = reference.line.max(1);
    let mut best = None;
    let mut stack = vec![root];
    while let Some(node) = stack.pop() {
        if !node_contains_line(node, line) {
            continue;
        }
        if is_java_like_type_kind(node.kind(), lang) {
            let name = declaration_name(node, source);
            if expected_type
                .as_deref()
                .is_none_or(|expected| name == Some(expected))
            {
                best = tighter_node(best, node);
            }
        }
        push_named_children(node, &mut stack);
    }
    best
}

fn find_enclosing_java_like_callable_node<'tree>(
    root: tree_sitter::Node<'tree>,
    source: &str,
    reference: &NameMatchRef,
    lang: LangId,
) -> Option<tree_sitter::Node<'tree>> {
    let expected_name = reference.caller_symbol.rsplit("::").next();
    let line = reference.line.max(1);
    let mut best = None;
    let mut stack = vec![root];
    while let Some(node) = stack.pop() {
        if !node_contains_line(node, line) {
            continue;
        }
        if is_java_like_callable_kind(node.kind(), lang) {
            let name = declaration_name(node, source);
            if expected_name.is_none_or(|expected| name == Some(expected)) {
                best = tighter_node(best, node);
            }
        }
        push_named_children(node, &mut stack);
    }
    best
}

fn find_enclosing_cpp_callable_node<'tree>(
    root: tree_sitter::Node<'tree>,
    _source: &str,
    reference: &NameMatchRef,
) -> Option<tree_sitter::Node<'tree>> {
    let line = reference.line.max(1);
    let mut best = None;
    let mut stack = vec![root];
    while let Some(node) = stack.pop() {
        if !node_contains_line(node, line) {
            continue;
        }
        if node.kind() == "function_definition" {
            best = tighter_node(best, node);
        }
        push_named_children(node, &mut stack);
    }
    best
}

fn tighter_node<'tree>(
    current: Option<tree_sitter::Node<'tree>>,
    candidate: tree_sitter::Node<'tree>,
) -> Option<tree_sitter::Node<'tree>> {
    match current {
        Some(current)
            if current.start_byte() > candidate.start_byte()
                || (current.start_byte() == candidate.start_byte()
                    && current.end_byte() <= candidate.end_byte()) =>
        {
            Some(current)
        }
        _ => Some(candidate),
    }
}

fn node_contains_line(node: tree_sitter::Node<'_>, line: u32) -> bool {
    let start = node.start_position().row as u32 + 1;
    let end = node.end_position().row as u32 + 1;
    start <= line && line <= end
}

fn push_named_children<'tree>(
    node: tree_sitter::Node<'tree>,
    stack: &mut Vec<tree_sitter::Node<'tree>>,
) {
    for index in 0..node.named_child_count() {
        if let Some(child) = node.named_child(index as u32) {
            stack.push(child);
        }
    }
}

fn declaration_name<'source>(
    node: tree_sitter::Node<'_>,
    source: &'source str,
) -> Option<&'source str> {
    node.child_by_field_name("name")
        .map(|name| node_text(name, source))
        .or_else(|| {
            first_named_child_text(
                node,
                source,
                &["identifier", "type_identifier", "simple_identifier"],
            )
        })
}

fn first_named_child_text<'source>(
    node: tree_sitter::Node<'_>,
    source: &'source str,
    kinds: &[&str],
) -> Option<&'source str> {
    for index in 0..node.named_child_count() {
        let child = node.named_child(index as u32)?;
        if kinds.contains(&child.kind()) {
            return Some(node_text(child, source));
        }
    }
    None
}

fn node_text<'source>(node: tree_sitter::Node<'_>, source: &'source str) -> &'source str {
    &source[node.byte_range()]
}

fn infer_java_like_field_receiver_type(
    type_node: tree_sitter::Node<'_>,
    source: &str,
    receiver: &str,
    lang: LangId,
) -> Option<String> {
    let mut stack = Vec::new();
    push_named_children(type_node, &mut stack);
    while let Some(node) = stack.pop() {
        if is_java_like_field_kind(node.kind(), lang) {
            if let Some(receiver_type) =
                extract_java_like_declared_type(node_text(node, source), receiver, lang)
            {
                return Some(receiver_type);
            }
        }
        if is_java_like_type_kind(node.kind(), lang)
            || is_java_like_callable_kind(node.kind(), lang)
        {
            continue;
        }
        push_named_children(node, &mut stack);
    }
    None
}

fn infer_java_like_local_receiver_type(
    callable_node: tree_sitter::Node<'_>,
    source: &str,
    receiver: &str,
    call_line: u32,
    lang: LangId,
) -> Option<String> {
    let mut best: Option<(u32, String)> = None;
    let mut stack = Vec::new();
    push_named_children(callable_node, &mut stack);
    while let Some(node) = stack.pop() {
        let start_line = node.start_position().row as u32 + 1;
        if start_line > call_line {
            continue;
        }
        if is_java_like_local_kind(node.kind(), lang) {
            if let Some(receiver_type) =
                extract_java_like_declared_type(node_text(node, source), receiver, lang)
            {
                if best
                    .as_ref()
                    .is_none_or(|(best_line, _)| start_line >= *best_line)
                {
                    best = Some((start_line, receiver_type));
                }
            }
        }
        if is_java_like_type_kind(node.kind(), lang)
            || is_java_like_callable_kind(node.kind(), lang)
        {
            continue;
        }
        push_named_children(node, &mut stack);
    }
    best.map(|(_, receiver_type)| receiver_type)
}

fn is_java_like_type_kind(kind: &str, lang: LangId) -> bool {
    match lang {
        LangId::Java => matches!(
            kind,
            "class_declaration"
                | "interface_declaration"
                | "enum_declaration"
                | "record_declaration"
                | "annotation_type_declaration"
        ),
        LangId::Kotlin => matches!(kind, "class_declaration" | "object_declaration"),
        _ => false,
    }
}

fn is_java_like_callable_kind(kind: &str, lang: LangId) -> bool {
    match lang {
        LangId::Java => matches!(kind, "method_declaration" | "constructor_declaration"),
        LangId::Kotlin => kind == "function_declaration",
        _ => false,
    }
}

fn is_java_like_field_kind(kind: &str, lang: LangId) -> bool {
    match lang {
        LangId::Java => kind == "field_declaration",
        LangId::Kotlin => kind == "property_declaration",
        _ => false,
    }
}

fn is_java_like_local_kind(kind: &str, lang: LangId) -> bool {
    match lang {
        LangId::Java => kind == "local_variable_declaration",
        LangId::Kotlin => kind == "property_declaration",
        _ => false,
    }
}

fn extract_java_like_declared_type(
    declaration: &str,
    receiver: &str,
    lang: LangId,
) -> Option<String> {
    match lang {
        LangId::Java => extract_java_declared_type(declaration, receiver),
        LangId::Kotlin => extract_kotlin_declared_type(declaration, receiver),
        _ => None,
    }
}

fn extract_java_declared_type(declaration: &str, receiver: &str) -> Option<String> {
    let receiver_start = find_identifier_occurrence(declaration, receiver)?;
    let after = declaration[receiver_start + receiver.len()..].trim_start();
    if after
        .chars()
        .next()
        .is_some_and(|ch| !matches!(ch, ';' | '=' | ',' | ')' | '['))
    {
        return None;
    }

    let before = declaration[..receiver_start].trim_end();
    if before.contains(',') {
        return None;
    }
    normalize_receiver_type_name(strip_java_declaration_prefixes(before))
}

fn strip_java_declaration_prefixes(mut value: &str) -> &str {
    loop {
        value = value.trim_start();
        if let Some(stripped) = strip_leading_java_annotation(value) {
            value = stripped;
            continue;
        }
        if let Some(stripped) = strip_leading_java_modifier(value) {
            value = stripped;
            continue;
        }
        return value.trim();
    }
}

fn strip_leading_java_annotation(value: &str) -> Option<&str> {
    let value = value.trim_start();
    let mut chars = value.char_indices();
    let (_, first) = chars.next()?;
    if first != '@' {
        return None;
    }
    let mut end = first.len_utf8();
    for (index, ch) in chars {
        if !(is_code_ident_char(ch) || ch == '.') {
            end = index;
            break;
        }
        end = index + ch.len_utf8();
    }
    let rest = value[end..].trim_start();
    if let Some(stripped) = rest.strip_prefix('(') {
        let mut depth = 1usize;
        for (index, ch) in stripped.char_indices() {
            match ch {
                '(' => depth += 1,
                ')' => {
                    depth = depth.saturating_sub(1);
                    if depth == 0 {
                        return Some(stripped[index + ch.len_utf8()..].trim_start());
                    }
                }
                _ => {}
            }
        }
        return Some("");
    }
    Some(rest)
}

fn strip_leading_java_modifier(value: &str) -> Option<&str> {
    const MODIFIERS: &[&str] = &[
        "public",
        "protected",
        "private",
        "abstract",
        "static",
        "final",
        "transient",
        "volatile",
        "synchronized",
        "native",
        "strictfp",
    ];
    MODIFIERS
        .iter()
        .find_map(|modifier| strip_leading_word(value, modifier))
}

fn extract_kotlin_declared_type(declaration: &str, receiver: &str) -> Option<String> {
    let receiver_start = find_identifier_occurrence(declaration, receiver)?;
    let before = &declaration[..receiver_start];
    if find_identifier_occurrence(before, "val").is_none()
        && find_identifier_occurrence(before, "var").is_none()
    {
        return None;
    }

    let after = declaration[receiver_start + receiver.len()..].trim_start();
    if let Some(type_text) = after.strip_prefix(':') {
        return normalize_receiver_type_name(read_type_prefix(type_text));
    }
    after
        .strip_prefix('=')
        .and_then(infer_kotlin_constructor_type)
}

fn infer_kotlin_constructor_type(rhs: &str) -> Option<String> {
    let (head, rest) = read_invocation_head(rhs.trim_start(), JavaLikeInvocation::Kotlin)?;
    if rest.trim_start().starts_with('(') {
        normalize_receiver_type_name(head)
    } else {
        None
    }
}

fn read_type_prefix(value: &str) -> &str {
    let mut angle_depth = 0usize;
    for (index, ch) in value.char_indices() {
        match ch {
            '<' => angle_depth += 1,
            '>' => angle_depth = angle_depth.saturating_sub(1),
            '=' | ';' | '\n' | '\r' | '{' | ',' | ')' if angle_depth == 0 => {
                return value[..index].trim();
            }
            _ => {}
        }
    }
    value.trim()
}

fn infer_cpp_receiver_type_from_scope(
    scope: tree_sitter::Node<'_>,
    source: &str,
    receiver: &str,
    call_line: u32,
) -> Option<String> {
    let lines = source.lines().collect::<Vec<_>>();
    if lines.is_empty() {
        return None;
    }
    let scope_start = scope.start_position().row as usize;
    let call_index = (call_line as usize)
        .saturating_sub(1)
        .min(lines.len().saturating_sub(1));
    for index in (scope_start..=call_index).rev() {
        if let Some(receiver_type) = infer_cpp_receiver_type_from_line(lines[index], receiver) {
            return Some(receiver_type);
        }
    }
    None
}

fn infer_cpp_receiver_type_from_line(line: &str, receiver: &str) -> Option<String> {
    for receiver_start in identifier_occurrences(line, receiver) {
        let after = line[receiver_start + receiver.len()..].trim_start();
        if after
            .chars()
            .next()
            .is_some_and(|ch| !matches!(ch, ';' | '=' | ',' | ')' | '[' | '{' | '('))
        {
            continue;
        }
        let type_text = cpp_type_before_receiver(&line[..receiver_start])?;
        let normalized = normalize_cpp_type_name(type_text)?;
        if normalized == "auto" {
            if let Some(rhs) = after.strip_prefix('=') {
                return infer_cpp_auto_receiver_type(rhs);
            }
            continue;
        }
        return Some(normalized);
    }
    None
}

fn cpp_type_before_receiver(prefix: &str) -> Option<&str> {
    let candidate = prefix
        .rsplit([';', '{', '}', '('])
        .next()
        .unwrap_or(prefix)
        .trim();
    if candidate.is_empty() || candidate.ends_with(',') {
        None
    } else {
        Some(candidate)
    }
}

fn normalize_cpp_type_name(type_text: &str) -> Option<String> {
    let without_templates = strip_angle_groups(type_text);
    let mut cleaned = String::with_capacity(without_templates.len());
    for token in without_templates.split_whitespace() {
        if matches!(
            token,
            "const" | "volatile" | "mutable" | "typename" | "class" | "struct"
        ) {
            continue;
        }
        if !cleaned.is_empty() {
            cleaned.push(' ');
        }
        cleaned.push_str(token);
    }
    let token = cleaned
        .split_whitespace()
        .last()
        .unwrap_or(cleaned.trim())
        .trim_matches(|ch: char| !(is_code_ident_char(ch) || ch == ':' || ch == '.'))
        .trim_matches(['*', '&']);
    let simple = token.rsplit("::").next().unwrap_or(token).trim();
    if simple.is_empty() || cpp_non_type_token(simple) {
        None
    } else {
        Some(simple.to_string())
    }
}

fn infer_cpp_auto_receiver_type(rhs: &str) -> Option<String> {
    let rhs = rhs.trim_start();
    if let Some(after_new) = rhs.strip_prefix("new ") {
        return infer_cpp_constructor_type(after_new);
    }
    infer_cpp_make_template_type(rhs)
        .or_else(|| infer_cpp_constructor_type(rhs))
        .or_else(|| infer_cpp_factory_type(rhs))
}

fn infer_cpp_constructor_type(rhs: &str) -> Option<String> {
    let (head, rest) = read_invocation_head(rhs.trim_start(), JavaLikeInvocation::Cpp)?;
    let normalized = normalize_cpp_type_name(head)?;
    if !normalized
        .chars()
        .next()
        .is_some_and(|ch| ch == '_' || ch.is_ascii_uppercase())
    {
        return None;
    }
    if matches!(rest.trim_start().chars().next(), Some('(' | '{')) {
        Some(normalized)
    } else {
        None
    }
}

fn infer_cpp_make_template_type(rhs: &str) -> Option<String> {
    let (head, rest) = read_invocation_head(rhs.trim_start(), JavaLikeInvocation::Cpp)?;
    if !rest.trim_start().starts_with('(') {
        return None;
    }
    let base = head.split('<').next().unwrap_or(head);
    let base_simple = base.rsplit("::").next().unwrap_or(base);
    if !matches!(base_simple, "make_unique" | "make_shared") {
        return None;
    }
    first_angle_arg(head).and_then(normalize_cpp_type_name)
}

fn infer_cpp_factory_type(rhs: &str) -> Option<String> {
    let (head, rest) = read_invocation_head(rhs.trim_start(), JavaLikeInvocation::Cpp)?;
    if !rest.trim_start().starts_with('(') {
        return None;
    }
    let simple = head
        .split('<')
        .next()
        .unwrap_or(head)
        .rsplit("::")
        .next()
        .unwrap_or(head);
    for prefix in ["make", "create", "build"] {
        if let Some(suffix) = simple.strip_prefix(prefix) {
            if suffix
                .chars()
                .next()
                .is_some_and(|ch| ch == '_' || ch.is_ascii_uppercase())
            {
                return normalize_cpp_type_name(suffix);
            }
        }
    }
    None
}

#[derive(Debug, Clone, Copy)]
enum JavaLikeInvocation {
    Kotlin,
    Cpp,
}

fn read_invocation_head(value: &str, flavor: JavaLikeInvocation) -> Option<(&str, &str)> {
    let value = value.trim_start();
    let mut end = 0usize;
    for (index, ch) in value.char_indices() {
        let allowed_separator = match flavor {
            JavaLikeInvocation::Kotlin => ch == '.',
            JavaLikeInvocation::Cpp => ch == ':' || ch == '.',
        };
        if is_code_ident_char(ch) || allowed_separator {
            end = index + ch.len_utf8();
            continue;
        }
        break;
    }
    if end == 0 {
        return None;
    }
    let mut rest = &value[end..];
    if let Some(stripped) = rest.trim_start().strip_prefix('<') {
        let skipped = skip_balanced_angle(stripped)?;
        let rest_start = rest.len() - rest.trim_start().len();
        let angle_len = 1 + skipped;
        end += rest_start + angle_len;
        rest = &value[end..];
    }
    Some((value[..end].trim(), rest))
}

fn skip_balanced_angle(value_after_open: &str) -> Option<usize> {
    let mut depth = 1usize;
    for (index, ch) in value_after_open.char_indices() {
        match ch {
            '<' => depth += 1,
            '>' => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    return Some(index + ch.len_utf8());
                }
            }
            _ => {}
        }
    }
    None
}

fn first_angle_arg(value: &str) -> Option<&str> {
    let open = value.find('<')?;
    let inner_len = skip_balanced_angle(&value[open + 1..])?;
    let inner = &value[open + 1..open + inner_len];
    split_top_level_commas(inner).into_iter().next()
}

fn normalize_receiver_type_name(type_text: &str) -> Option<String> {
    let without_generics = strip_angle_groups(type_text);
    let cleaned = without_generics
        .replace("[]", " ")
        .replace("...", " ")
        .replace(['?', '&', '*'], " ");
    let token = cleaned
        .split_whitespace()
        .last()
        .unwrap_or(cleaned.trim())
        .trim_matches(|ch: char| !(is_code_ident_char(ch) || ch == '.' || ch == ':'));
    let token = token.rsplit("::").next().unwrap_or(token);
    let simple = token.rsplit('.').next().unwrap_or(token).trim();
    if simple.is_empty()
        || java_like_primitive_type(simple)
        || !simple
            .chars()
            .next()
            .is_some_and(|ch| ch == '_' || ch.is_ascii_uppercase())
    {
        None
    } else {
        Some(simple.to_string())
    }
}

fn simple_type_name(scoped_name: &str) -> Option<String> {
    scoped_name
        .rsplit("::")
        .find(|segment| !segment.is_empty())
        .and_then(normalize_receiver_type_name)
}

fn strip_angle_groups(value: &str) -> String {
    let mut output = String::with_capacity(value.len());
    let mut depth = 0usize;
    for ch in value.chars() {
        match ch {
            '<' => {
                if depth == 0 {
                    output.push(' ');
                }
                depth += 1;
            }
            '>' => depth = depth.saturating_sub(1),
            _ if depth == 0 => output.push(ch),
            _ => {}
        }
    }
    output
}

fn java_like_primitive_type(value: &str) -> bool {
    matches!(
        value,
        "boolean"
            | "byte"
            | "char"
            | "double"
            | "float"
            | "int"
            | "long"
            | "short"
            | "void"
            | "Boolean"
            | "Byte"
            | "Char"
            | "Double"
            | "Float"
            | "Int"
            | "Long"
            | "Short"
            | "Unit"
    )
}

fn cpp_non_type_token(value: &str) -> bool {
    matches!(
        value,
        "return"
            | "if"
            | "else"
            | "for"
            | "while"
            | "do"
            | "switch"
            | "case"
            | "default"
            | "break"
            | "continue"
            | "goto"
            | "throw"
            | "new"
            | "delete"
            | "co_await"
            | "co_yield"
            | "co_return"
            | "static_cast"
            | "const_cast"
            | "dynamic_cast"
            | "reinterpret_cast"
            | "sizeof"
            | "alignof"
            | "typeid"
            | "and"
            | "or"
            | "not"
            | "xor"
    )
}

fn receiver_is_bare_identifier(value: &str) -> bool {
    let mut chars = value.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    (first == '_' || first.is_ascii_alphabetic()) && chars.all(is_code_ident_char)
}

fn find_identifier_occurrence(value: &str, needle: &str) -> Option<usize> {
    identifier_occurrences(value, needle).into_iter().next()
}

fn identifier_occurrences(value: &str, needle: &str) -> Vec<usize> {
    value
        .match_indices(needle)
        .filter_map(|(index, _)| identifier_boundary(value, index, needle.len()).then_some(index))
        .collect()
}

fn identifier_boundary(value: &str, start: usize, len: usize) -> bool {
    let before = value[..start].chars().next_back();
    let after = value[start + len..].chars().next();
    !before.is_some_and(is_code_ident_char) && !after.is_some_and(is_code_ident_char)
}

fn strip_leading_word<'a>(value: &'a str, word: &str) -> Option<&'a str> {
    let stripped = value.strip_prefix(word)?;
    if stripped.is_empty() || stripped.chars().next().is_some_and(char::is_whitespace) {
        Some(stripped.trim_start())
    } else {
        None
    }
}

fn is_code_ident_char(ch: char) -> bool {
    ch == '_' || ch.is_ascii_alphanumeric()
}

fn infer_rust_receiver_type(
    project_root: &Path,
    reference: &NameMatchRef,
    source_cache: &mut DispatchSourceCache,
) -> ReceiverTypeInference {
    if matches!(reference.receiver.as_str(), "self" | "Self") {
        return enclosing_type_from_scoped_name(&reference.caller_symbol)
            .map(ReceiverTypeInference::Known)
            .unwrap_or(ReceiverTypeInference::Unknown);
    }

    if reference.colon_dispatch && rust_receiver_looks_type_like(&reference.receiver) {
        return ReceiverTypeInference::Known(reference.receiver.clone());
    }

    if let Some(receiver_type) = reference
        .caller_signature
        .as_deref()
        .and_then(|signature| rust_parameter_type(signature, &reference.receiver))
    {
        return ReceiverTypeInference::Known(receiver_type);
    }

    infer_rust_direct_self_field_receiver_type(project_root, reference, source_cache)
}

fn infer_rust_direct_self_field_receiver_type(
    project_root: &Path,
    reference: &NameMatchRef,
    source_cache: &mut DispatchSourceCache,
) -> ReceiverTypeInference {
    if reference.colon_dispatch {
        return ReceiverTypeInference::Unknown;
    }
    let Some(field_name) = rust_direct_self_field_name(&reference.receiver_expression) else {
        return ReceiverTypeInference::Unknown;
    };
    if field_name != reference.receiver {
        return ReceiverTypeInference::Unknown;
    }

    let Some(impl_type) = enclosing_type_from_scoped_name(&reference.caller_symbol) else {
        return ReceiverTypeInference::Unknown;
    };
    let Some(struct_name) = rust_direct_nominal_type_name(&impl_type) else {
        return ReceiverTypeInference::KnownButUnresolved;
    };
    let Some(parsed) = parsed_dispatch_source(project_root, reference, LangId::Rust, source_cache)
    else {
        return ReceiverTypeInference::Unknown;
    };
    let Some(impl_node) =
        find_enclosing_rust_impl_node(parsed.tree.root_node(), reference.line.max(1))
    else {
        return ReceiverTypeInference::Unknown;
    };
    if impl_node.child_by_field_name("trait").is_some()
        || impl_node.child_by_field_name("type_parameters").is_some()
    {
        return ReceiverTypeInference::KnownButUnresolved;
    }
    let Some(impl_target) = impl_node.child_by_field_name("type") else {
        return ReceiverTypeInference::KnownButUnresolved;
    };
    if impl_target.kind() != "type_identifier"
        || node_text(impl_target, &parsed.source) != impl_type
    {
        return ReceiverTypeInference::KnownButUnresolved;
    }

    let module_scope = rust_module_scope(impl_node);
    let Some(struct_node) = find_unique_rust_struct(
        parsed.tree.root_node(),
        &parsed.source,
        struct_name,
        &module_scope,
    ) else {
        return ReceiverTypeInference::KnownButUnresolved;
    };
    let Some(field_type) = rust_struct_field_type_node(struct_node, &parsed.source, field_name)
    else {
        return ReceiverTypeInference::KnownButUnresolved;
    };
    if field_type.kind() != "type_identifier" {
        return ReceiverTypeInference::KnownButUnresolved;
    }
    let field_type_name = node_text(field_type, &parsed.source);
    if find_unique_rust_struct(
        parsed.tree.root_node(),
        &parsed.source,
        field_type_name,
        &module_scope,
    )
    .is_none()
    {
        return ReceiverTypeInference::KnownButUnresolved;
    }

    ReceiverTypeInference::RustDirectSelfField {
        receiver_type: field_type_name.to_string(),
        declaration_file: reference.caller_file.clone(),
        module_scope,
    }
}

fn rust_direct_self_field_name(receiver_expression: &str) -> Option<&str> {
    let (base, field) = receiver_expression.split_once('.')?;
    let base = base.trim();
    let field = field.trim();
    (base == "self" && rust_direct_nominal_type_name(field).is_some()).then_some(field)
}

fn rust_direct_nominal_type_name(value: &str) -> Option<&str> {
    let name = value.rsplit("::").next()?.trim();
    (!name.is_empty()
        && !name.chars().next().is_some_and(|ch| ch.is_ascii_digit())
        && name.chars().all(is_rust_ident_char))
    .then_some(name)
}

fn find_enclosing_rust_impl_node<'tree>(
    root: tree_sitter::Node<'tree>,
    line: u32,
) -> Option<tree_sitter::Node<'tree>> {
    let mut best = None;
    let mut stack = vec![root];
    while let Some(node) = stack.pop() {
        if !node_contains_line(node, line) {
            continue;
        }
        if node.kind() == "impl_item" {
            best = tighter_node(best, node);
        }
        push_named_children(node, &mut stack);
    }
    best
}

fn rust_module_scope(node: tree_sitter::Node<'_>) -> Vec<(usize, usize)> {
    let mut scope = Vec::new();
    let mut current = node.parent();
    while let Some(parent) = current {
        if parent.kind() == "mod_item" {
            scope.push((parent.start_byte(), parent.end_byte()));
        }
        current = parent.parent();
    }
    scope.reverse();
    scope
}

fn find_unique_rust_struct<'tree>(
    root: tree_sitter::Node<'tree>,
    source: &str,
    expected_name: &str,
    module_scope: &[(usize, usize)],
) -> Option<tree_sitter::Node<'tree>> {
    let mut found = None;
    let mut stack = vec![root];
    while let Some(node) = stack.pop() {
        if node.kind() == "struct_item"
            && rust_module_scope(node) == module_scope
            && node.child_by_field_name("type_parameters").is_none()
            && declaration_name(node, source) == Some(expected_name)
        {
            if found.is_some() {
                return None;
            }
            found = Some(node);
        }
        push_named_children(node, &mut stack);
    }
    found
}

fn rust_struct_field_type_node<'tree>(
    struct_node: tree_sitter::Node<'tree>,
    source: &str,
    field_name: &str,
) -> Option<tree_sitter::Node<'tree>> {
    let fields = struct_node.child_by_field_name("body")?;
    if fields.kind() != "field_declaration_list" {
        return None;
    }
    for index in 0..fields.named_child_count() {
        let field = fields.named_child(index as u32)?;
        if field.kind() != "field_declaration"
            || declaration_name(field, source) != Some(field_name)
        {
            continue;
        }
        return field.child_by_field_name("type");
    }
    None
}

fn rust_receiver_looks_type_like(receiver: &str) -> bool {
    receiver
        .chars()
        .next()
        .is_some_and(|ch| ch == '_' || ch.is_uppercase())
}

fn enclosing_type_from_scoped_name(scoped_name: &str) -> Option<String> {
    scoped_name
        .rsplit_once("::")
        .map(|(enclosing, _)| enclosing)
        .filter(|enclosing| !enclosing.is_empty() && *enclosing != TOP_LEVEL_SYMBOL)
        .map(ToString::to_string)
}

fn rust_parameter_type(signature: &str, receiver: &str) -> Option<String> {
    let params = signature_parameter_text(signature)?;
    for param in split_top_level_commas(params) {
        let Some((pattern, type_text)) = param.split_once(':') else {
            continue;
        };
        let Some(name) = rust_parameter_name(pattern) else {
            continue;
        };
        if name == receiver {
            return normalize_rust_receiver_type(type_text);
        }
    }
    None
}

fn signature_parameter_text(signature: &str) -> Option<&str> {
    let open = signature.find('(')?;
    let mut depth = 0usize;
    for (offset, ch) in signature[open..].char_indices() {
        match ch {
            '(' => depth += 1,
            ')' => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    return Some(&signature[open + 1..open + offset]);
                }
            }
            _ => {}
        }
    }
    None
}

fn split_top_level_commas(value: &str) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut start = 0usize;
    let mut angle_depth = 0usize;
    let mut paren_depth = 0usize;
    let mut bracket_depth = 0usize;
    for (index, ch) in value.char_indices() {
        match ch {
            '<' => angle_depth += 1,
            '>' => angle_depth = angle_depth.saturating_sub(1),
            '(' => paren_depth += 1,
            ')' => paren_depth = paren_depth.saturating_sub(1),
            '[' => bracket_depth += 1,
            ']' => bracket_depth = bracket_depth.saturating_sub(1),
            ',' if angle_depth == 0 && paren_depth == 0 && bracket_depth == 0 => {
                let part = value[start..index].trim();
                if !part.is_empty() {
                    parts.push(part);
                }
                start = index + ch.len_utf8();
            }
            _ => {}
        }
    }
    let part = value[start..].trim();
    if !part.is_empty() {
        parts.push(part);
    }
    parts
}

fn rust_parameter_name(pattern: &str) -> Option<&str> {
    let mut pattern = pattern.trim();
    if let Some(stripped) = pattern.strip_prefix("mut ") {
        pattern = stripped.trim_start();
    }
    pattern
        .rsplit(|ch: char| !is_rust_ident_char(ch))
        .find(|part| !part.is_empty())
}

fn normalize_rust_receiver_type(type_text: &str) -> Option<String> {
    let mut ty = strip_leading_rust_type_modifiers(type_text);
    let owned_inner;
    if let Some(inner) = single_outer_generic_arg(ty) {
        owned_inner = inner.trim().to_string();
        ty = strip_leading_rust_type_modifiers(&owned_inner);
    }
    rust_base_type_ident(ty)
}

fn strip_leading_rust_type_modifiers(mut ty: &str) -> &str {
    loop {
        ty = ty.trim_start();
        if let Some(stripped) = ty.strip_prefix('&') {
            ty = stripped.trim_start();
            if let Some(stripped) = strip_leading_lifetime(ty) {
                ty = stripped.trim_start();
            }
            if let Some(stripped) = ty.strip_prefix("mut ") {
                ty = stripped.trim_start();
            }
            continue;
        }
        if let Some(stripped) = ty.strip_prefix("mut ") {
            ty = stripped.trim_start();
            continue;
        }
        if let Some(stripped) = ty.strip_prefix("dyn ") {
            ty = stripped.trim_start();
            continue;
        }
        if let Some(stripped) = ty.strip_prefix("impl ") {
            ty = stripped.trim_start();
            continue;
        }
        break ty.trim();
    }
}

fn strip_leading_lifetime(value: &str) -> Option<&str> {
    let mut chars = value.char_indices();
    let (_, first) = chars.next()?;
    if first != '\'' {
        return None;
    }
    for (index, ch) in chars {
        if !(ch == '_' || ch.is_ascii_alphanumeric()) {
            return Some(&value[index..]);
        }
    }
    Some("")
}

fn single_outer_generic_arg(ty: &str) -> Option<&str> {
    let ty = ty.trim();
    let open = ty.find('<')?;
    let mut depth = 0usize;
    let mut close = None;
    for (index, ch) in ty.char_indices().skip_while(|(index, _)| *index < open) {
        match ch {
            '<' => depth += 1,
            '>' => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    close = Some(index);
                    break;
                }
            }
            _ => {}
        }
    }
    let close = close?;
    if !ty[close + 1..].trim().is_empty() {
        return None;
    }
    let inner = &ty[open + 1..close];
    let args = split_top_level_commas(inner);
    match args.as_slice() {
        [arg] => Some(*arg),
        _ => None,
    }
}

fn rust_base_type_ident(ty: &str) -> Option<String> {
    let ty = ty.trim();
    let head = ty
        .split([' ', '+', '='])
        .find(|part| !part.is_empty())
        .unwrap_or(ty);
    let head = head.split('<').next().unwrap_or(head).trim();
    let ident = head
        .rsplit("::")
        .next()
        .unwrap_or(head)
        .trim_matches(|ch: char| !is_rust_ident_char(ch));
    if ident.is_empty() || ident.chars().next().is_some_and(|ch| ch.is_ascii_digit()) {
        None
    } else {
        Some(ident.to_string())
    }
}

fn is_rust_ident_char(ch: char) -> bool {
    ch == '_' || ch.is_ascii_alphanumeric()
}

fn select_rust_direct_self_field_candidate(
    project_root: &Path,
    reference: &NameMatchRef,
    candidates: &[NameMatchCandidate],
    receiver_type: &str,
    declaration_file: &str,
    declaration_scope: &[(usize, usize)],
    source_cache: &mut DispatchSourceCache,
) -> Option<NameMatchCandidate> {
    let eligible = candidates
        .iter()
        .filter(|candidate| candidate.node_id != reference.caller_node)
        .filter(|candidate| {
            type_candidate_matches(candidate, receiver_type, &reference.method_name)
        })
        .filter(|candidate| {
            rust_direct_self_field_candidate_matches_scope(
                project_root,
                candidate,
                receiver_type,
                declaration_file,
                declaration_scope,
                source_cache,
            )
        })
        .collect::<Vec<_>>();
    match eligible.as_slice() {
        [candidate] => Some((**candidate).clone()),
        _ => None,
    }
}

fn rust_direct_self_field_candidate_matches_scope(
    project_root: &Path,
    candidate: &NameMatchCandidate,
    receiver_type: &str,
    declaration_file: &str,
    declaration_scope: &[(usize, usize)],
    source_cache: &mut DispatchSourceCache,
) -> bool {
    if candidate.file_path != declaration_file {
        return false;
    }
    let Some(parsed) = parsed_dispatch_source_for_file(
        project_root,
        &candidate.file_path,
        "rust",
        LangId::Rust,
        source_cache,
    ) else {
        return false;
    };
    let Some(impl_node) =
        find_enclosing_rust_impl_node(parsed.tree.root_node(), candidate.start_line)
    else {
        return false;
    };
    if impl_node.child_by_field_name("trait").is_some()
        || impl_node.child_by_field_name("type_parameters").is_some()
    {
        return false;
    }
    let Some(impl_target) = impl_node.child_by_field_name("type") else {
        return false;
    };
    impl_target.kind() == "type_identifier"
        && node_text(impl_target, &parsed.source) == receiver_type
        && rust_module_scope(impl_node) == declaration_scope
}

fn select_type_match_candidate(
    reference: &NameMatchRef,
    candidates: &[NameMatchCandidate],
    receiver_type: &str,
) -> Option<NameMatchCandidate> {
    let candidates = candidates
        .iter()
        .filter(|candidate| candidate.node_id != reference.caller_node)
        .filter(|candidate| {
            type_candidate_matches(candidate, receiver_type, &reference.method_name)
        })
        .collect::<Vec<_>>();
    match candidates.as_slice() {
        [candidate] => Some((**candidate).clone()),
        _ => None,
    }
}

fn type_candidate_matches(
    candidate: &NameMatchCandidate,
    receiver_type: &str,
    method_name: &str,
) -> bool {
    let normalized_type = receiver_type.replace('.', "::");
    let suffix = format!("{normalized_type}::{method_name}");
    candidate.scoped_name == suffix || candidate.scoped_name.ends_with(&format!("::{suffix}"))
}

fn select_name_match_candidate(
    reference: &NameMatchRef,
    candidates: &[NameMatchCandidate],
) -> Option<NameMatchCandidate> {
    let candidates = candidates
        .iter()
        .filter(|candidate| candidate.node_id != reference.caller_node)
        .filter(|candidate| candidate_allowed_for_reference(reference, candidate))
        .collect::<Vec<_>>();
    match candidates.as_slice() {
        [] => None,
        [candidate] => Some((**candidate).clone()),
        _ => select_scored_name_match_candidate(reference, &candidates),
    }
}

fn candidate_allowed_for_reference(
    reference: &NameMatchRef,
    candidate: &NameMatchCandidate,
) -> bool {
    if !reference.colon_dispatch {
        return true;
    }

    candidate.kind == "method"
        && candidate
            .scoped_name
            .split("::")
            .any(|segment| segment == reference.receiver)
}

fn select_scored_name_match_candidate(
    reference: &NameMatchRef,
    candidates: &[&NameMatchCandidate],
) -> Option<NameMatchCandidate> {
    let receiver_words = split_camel_case(&reference.receiver);
    if receiver_words.is_empty() {
        return None;
    }

    let mut best: Option<(&NameMatchCandidate, f64)> = None;
    let mut tied_best = false;
    for candidate in candidates {
        let candidate_words = split_camel_case(&candidate.scoped_name);
        let overlap = receiver_words
            .iter()
            .filter(|receiver_word| {
                candidate_words
                    .iter()
                    .any(|candidate_word| candidate_word == *receiver_word)
            })
            .count() as f64;
        let score =
            overlap + 1.0 + compute_path_proximity(&reference.caller_file, &candidate.file_path);
        match best {
            None => {
                best = Some((*candidate, score));
                tied_best = false;
            }
            Some((_, best_score)) if score > best_score => {
                best = Some((*candidate, score));
                tied_best = false;
            }
            Some((_, best_score)) if (score - best_score).abs() < f64::EPSILON => {
                tied_best = true;
            }
            _ => {}
        }
    }

    let (candidate, score) = best?;
    if score >= NAME_MATCH_SCORE_THRESHOLD && !tied_best {
        Some(candidate.clone())
    } else {
        None
    }
}

fn method_name_match_denylisted(method_name: &str) -> bool {
    matches!(
        method_name,
        "and_then"
            | "as_bytes"
            | "as_deref"
            | "as_mut"
            | "as_ref"
            | "as_str"
            | "borrow"
            | "borrow_mut"
            | "clear"
            | "clone"
            | "collect"
            | "contains"
            | "contains_key"
            | "count"
            | "dedup"
            | "default"
            | "drain"
            | "ends_with"
            | "entry"
            | "err"
            | "expect"
            | "extend"
            | "filter"
            | "filter_map"
            | "find"
            | "from"
            | "get"
            | "get_mut"
            | "insert"
            | "into"
            | "into_iter"
            | "is_empty"
            | "is_err"
            | "is_none"
            | "is_ok"
            | "is_some"
            | "iter"
            | "iter_mut"
            | "join"
            | "len"
            | "lock"
            | "map"
            | "map_err"
            | "max"
            | "min"
            | "new"
            | "next"
            | "ok"
            | "or_default"
            | "or_else"
            | "or_insert"
            | "or_insert_with"
            | "parse"
            | "pop"
            | "position"
            | "push"
            | "read"
            | "recv"
            | "remove"
            | "replace"
            | "retain"
            | "send"
            | "sort"
            | "sort_by"
            | "split"
            | "starts_with"
            | "sum"
            | "take"
            | "to_owned"
            | "to_string"
            | "trim"
            | "try_from"
            | "try_into"
            | "unwrap"
            | "unwrap_or"
            | "unwrap_or_default"
            | "unwrap_or_else"
            | "with_capacity"
            | "write"
    )
}

fn split_camel_case(value: &str) -> Vec<String> {
    let chars = value.chars().collect::<Vec<_>>();
    let mut normalized = String::with_capacity(value.len() + 8);
    for (index, ch) in chars.iter().enumerate() {
        let previous = index.checked_sub(1).and_then(|prev| chars.get(prev));
        let next = chars.get(index + 1);
        let is_separator = ch.is_whitespace()
            || matches!(
                ch,
                '_' | '.' | ':' | '/' | '\\' | '-' | '<' | '>' | '(' | ')' | '[' | ']'
            );
        if is_separator {
            normalized.push(' ');
            continue;
        }
        let camel_boundary = previous.is_some_and(|prev| {
            (prev.is_lowercase() && ch.is_uppercase())
                || (prev.is_ascii_digit() && ch.is_alphabetic())
                || (prev.is_uppercase()
                    && ch.is_uppercase()
                    && next.is_some_and(|next| next.is_lowercase()))
        });
        if camel_boundary {
            normalized.push(' ');
        }
        normalized.push(*ch);
    }

    normalized
        .split_whitespace()
        .filter(|word| word.len() > 1)
        .map(|word| word.to_ascii_lowercase())
        .collect()
}

fn compute_path_proximity(left: &str, right: &str) -> f64 {
    let left_dirs = left
        .rsplit_once('/')
        .map(|(dir, _)| dir)
        .unwrap_or_default()
        .split('/')
        .filter(|part| !part.is_empty());
    let right_dirs = right
        .rsplit_once('/')
        .map(|(dir, _)| dir)
        .unwrap_or_default()
        .split('/')
        .filter(|part| !part.is_empty());

    let shared = left_dirs
        .zip(right_dirs)
        .take_while(|(left, right)| left == right)
        .count();
    ((shared as f64) * 0.05).min(0.5)
}

fn mark_backend_state(
    tx: &Transaction<'_>,
    project_root: &Path,
    rel_path: &str,
    content_hash: Option<&blake3::Hash>,
    status: &str,
) -> Result<()> {
    clear_backend_state_for_file(tx, project_root, rel_path)?;
    let hash = content_hash
        .map(|hash| hash_to_hex(*hash))
        .unwrap_or_else(|| hash_to_hex(cache_freshness::zero_hash()));
    tx.execute(
        "INSERT OR REPLACE INTO backend_file_state(
            backend, workspace_root, file_path, content_hash, status, updated_at
        ) VALUES(?1, ?2, ?3, ?4, ?5, ?6)",
        params![
            BACKEND_TREESITTER,
            project_root.display().to_string(),
            rel_path,
            hash,
            status,
            unix_seconds_now(),
        ],
    )?;
    Ok(())
}

fn clear_backend_state_for_file(
    tx: &Transaction<'_>,
    project_root: &Path,
    rel_path: &str,
) -> Result<()> {
    tx.execute(
        "DELETE FROM backend_file_state
         WHERE backend = ?1 AND workspace_root = ?2 AND file_path = ?3",
        params![
            BACKEND_TREESITTER,
            project_root.display().to_string(),
            rel_path
        ],
    )?;
    Ok(())
}

fn load_file_row(tx: &Transaction<'_>, rel_path: &str) -> Result<Option<FileRow>> {
    tx.query_row(
        "SELECT surface_fingerprint, content_hash, mtime_ns, size FROM files WHERE path = ?1",
        params![rel_path],
        |row| {
            let hash_text: String = row.get(1)?;
            Ok(FileRow {
                surface_fingerprint: row.get(0)?,
                freshness: FileFreshness {
                    content_hash: hash_from_hex(&hash_text)
                        .unwrap_or_else(cache_freshness::zero_hash),
                    mtime: ns_to_system_time(row.get::<_, i64>(2)?),
                    size: row.get::<_, i64>(3)? as u64,
                },
            })
        },
    )
    .optional()
    .map_err(CallGraphStoreError::from)
}

fn stored_node_ids_match_extract(
    tx: &Transaction<'_>,
    rel_path: &str,
    extract: &FileExtract,
) -> Result<bool> {
    let mut stmt = tx.prepare("SELECT id FROM nodes WHERE file_path = ?1")?;
    let rows = stmt.query_map(params![rel_path], |row| row.get::<_, String>(0))?;
    let mut stored = BTreeSet::new();
    for row in rows {
        stored.insert(row?);
    }
    let extracted = extract
        .nodes
        .iter()
        .map(|node| node.id.clone())
        .collect::<BTreeSet<_>>();
    Ok(stored == extracted)
}

/// Compare every persisted graph row that comes from this file before rewriting it.
/// Ranges and reference byte offsets are part of the key because queries expose
/// source locations; equal names and edges are not enough after a body shift.
fn stored_extract_matches(
    tx: &Transaction<'_>,
    rel_path: &str,
    extract: &FileExtract,
    index: &ProjectIndex<'_>,
) -> Result<bool> {
    let stored_file = tx
        .query_row(
            "SELECT lang, surface_fingerprint FROM files WHERE path = ?1",
            params![rel_path],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
        )
        .optional()?;
    if stored_file
        != Some((
            lang_label(extract.lang).to_string(),
            extract.surface_fingerprint.clone(),
        ))
    {
        return Ok(false);
    }

    let mut stored_nodes_stmt = tx.prepare(
        "SELECT id, file_path, name, scoped_name, kind, start_line, start_col,
                end_line, end_col, range_ordinal, signature, exported,
                is_default_export, is_type_like, is_callgraph_entry_point, provenance
         FROM nodes WHERE file_path = ?1",
    )?;
    let stored_nodes = stored_nodes_stmt
        .query_map(params![rel_path], |row| {
            Ok(serde_json::json!([
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, i64>(5)?,
                row.get::<_, i64>(6)?,
                row.get::<_, i64>(7)?,
                row.get::<_, i64>(8)?,
                row.get::<_, i64>(9)?,
                row.get::<_, Option<String>>(10)?,
                row.get::<_, i64>(11)?,
                row.get::<_, i64>(12)?,
                row.get::<_, i64>(13)?,
                row.get::<_, i64>(14)?,
                row.get::<_, String>(15)?,
            ])
            .to_string())
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let expected_nodes = extract
        .nodes
        .iter()
        .map(|node| {
            serde_json::json!([
                node.id,
                node.file_path,
                node.name,
                node.scoped_name,
                node.kind,
                node.range.start_line,
                node.range.start_col,
                node.range.end_line,
                node.range.end_col,
                node.range_ordinal,
                node.signature,
                bool_int(node.exported),
                bool_int(node.is_default_export),
                bool_int(node.is_type_like),
                bool_int(node.is_callgraph_entry_point),
                PROVENANCE_TREESITTER,
            ])
            .to_string()
        })
        .collect::<Vec<_>>();
    let mut stored_nodes = stored_nodes;
    let mut expected_nodes = expected_nodes;
    stored_nodes.sort();
    expected_nodes.sort();
    if stored_nodes != expected_nodes {
        return Ok(false);
    }

    let resolved_refs = extract
        .raw_refs
        .iter()
        .cloned()
        .map(|raw| resolve_ref(raw, index))
        .collect::<Result<Vec<_>>>()?;
    let mut stored_refs_stmt = tx.prepare(
        "SELECT ref_id, caller_node, caller_file, kind, short_name, full_ref,
                module_path, import_kind, local_name, requested_name, namespace_alias,
                wildcard, line, byte_start, byte_end, status, target_node,
                target_file, target_symbol, provenance
         FROM refs WHERE caller_file = ?1",
    )?;
    let stored_refs = stored_refs_stmt
        .query_map(params![rel_path], |row| {
            Ok(serde_json::json!([
                row.get::<_, String>(0)?,
                row.get::<_, Option<String>>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, Option<String>>(4)?,
                row.get::<_, Option<String>>(5)?,
                row.get::<_, Option<String>>(6)?,
                row.get::<_, Option<String>>(7)?,
                row.get::<_, Option<String>>(8)?,
                row.get::<_, Option<String>>(9)?,
                row.get::<_, Option<String>>(10)?,
                row.get::<_, i64>(11)?,
                row.get::<_, i64>(12)?,
                row.get::<_, i64>(13)?,
                row.get::<_, i64>(14)?,
                row.get::<_, String>(15)?,
                row.get::<_, Option<String>>(16)?,
                row.get::<_, Option<String>>(17)?,
                row.get::<_, Option<String>>(18)?,
                row.get::<_, String>(19)?,
            ])
            .to_string())
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let expected_refs = resolved_refs
        .iter()
        .map(|resolved| {
            let raw = &resolved.raw;
            serde_json::json!([
                raw.ref_id,
                raw.caller_node,
                raw.caller_file,
                raw.kind,
                raw.short_name,
                raw.full_ref,
                raw.module_path,
                raw.import_kind,
                raw.local_name,
                raw.requested_name,
                raw.namespace_alias,
                bool_int(raw.wildcard),
                raw.line,
                raw.byte_start,
                raw.byte_end,
                resolved.status,
                resolved.target_node,
                resolved.target_file,
                resolved.target_symbol,
                PROVENANCE_TREESITTER,
            ])
            .to_string()
        })
        .collect::<Vec<_>>();
    let mut stored_refs = stored_refs;
    let mut expected_refs = expected_refs;
    stored_refs.sort();
    expected_refs.sort();
    if stored_refs != expected_refs {
        return Ok(false);
    }

    let mut stored_edges_stmt = tx.prepare(
        "SELECT e.edge_id, e.ref_id, e.source_node, e.target_node,
                e.target_file, e.target_symbol, e.kind, e.line, e.provenance
         FROM edges e JOIN refs r ON r.ref_id = e.ref_id
         WHERE r.caller_file = ?1 AND e.provenance = ?2",
    )?;
    let stored_edges = stored_edges_stmt
        .query_map(params![rel_path, PROVENANCE_TREESITTER], |row| {
            Ok(serde_json::json!([
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, Option<String>>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, String>(5)?,
                row.get::<_, String>(6)?,
                row.get::<_, i64>(7)?,
                row.get::<_, String>(8)?,
            ])
            .to_string())
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let expected_edges = resolved_refs
        .iter()
        .filter_map(|resolved| {
            resolved.edge.as_ref().map(|edge| {
                serde_json::json!([
                    edge.edge_id,
                    resolved.raw.ref_id,
                    edge.source_node,
                    edge.target_node,
                    edge.target_file,
                    edge.target_symbol,
                    edge.kind,
                    edge.line,
                    PROVENANCE_TREESITTER,
                ])
                .to_string()
            })
        })
        .collect::<Vec<_>>();
    let mut stored_edges = stored_edges;
    let mut expected_edges = expected_edges;
    stored_edges.sort();
    expected_edges.sort();
    if stored_edges != expected_edges {
        return Ok(false);
    }

    let mut stored_dependencies_stmt =
        tx.prepare("SELECT dep_file FROM file_dependencies WHERE file_path = ?1")?;
    let stored_dependencies = stored_dependencies_stmt
        .query_map(params![rel_path], |row| row.get::<_, String>(0))?
        .collect::<rusqlite::Result<BTreeSet<_>>>()?;
    let expected_dependencies = extract
        .raw_refs
        .iter()
        .flat_map(|raw| raw.dependencies.iter().cloned())
        .collect::<BTreeSet<_>>();
    if stored_dependencies != expected_dependencies {
        return Ok(false);
    }

    let mut stored_hints_stmt = tx.prepare(
        "SELECT id, method_name, caller_node, file, line, byte_start, byte_end, provenance
         FROM dispatch_hints WHERE file = ?1",
    )?;
    let stored_hints = stored_hints_stmt
        .query_map(params![rel_path], |row| {
            Ok(serde_json::json!([
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, i64>(4)?,
                row.get::<_, i64>(5)?,
                row.get::<_, i64>(6)?,
                row.get::<_, String>(7)?,
            ])
            .to_string())
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let expected_hints = extract
        .dispatch_hints
        .iter()
        .map(|hint| {
            serde_json::json!([
                hint.id,
                hint.method_name,
                hint.caller_node,
                hint.file,
                hint.line,
                hint.byte_start,
                hint.byte_end,
                PROVENANCE_TREESITTER,
            ])
            .to_string()
        })
        .collect::<Vec<_>>();
    let mut stored_hints = stored_hints;
    let mut expected_hints = expected_hints;
    stored_hints.sort();
    expected_hints.sort();
    Ok(stored_hints == expected_hints)
}

fn update_file_fresh_metadata(
    tx: &Transaction<'_>,
    project_root: &Path,
    rel_path: &str,
    hash: &blake3::Hash,
    mtime: SystemTime,
    size: u64,
) -> Result<()> {
    tx.execute(
        "UPDATE files SET content_hash = ?2, mtime_ns = ?3, size = ?4, indexed_at = ?5
         WHERE path = ?1",
        params![
            rel_path,
            hash_to_hex(*hash),
            system_time_to_ns(mtime),
            size as i64,
            unix_seconds_now()
        ],
    )?;
    tx.execute(
        "UPDATE backend_file_state SET content_hash = ?3, status = 'fresh', updated_at = ?5
         WHERE backend = ?1 AND file_path = ?2 AND workspace_root = ?4",
        params![
            BACKEND_TREESITTER,
            rel_path,
            hash_to_hex(*hash),
            project_root.display().to_string(),
            unix_seconds_now(),
        ],
    )?;
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct DependentRefSelection {
    ref_id: String,
    caller_file: String,
}

fn ref_ids_depending_on(
    tx: &Transaction<'_>,
    project_root: &Path,
    rel_path: &str,
) -> Result<Vec<DependentRefSelection>> {
    let mut stmt = tx.prepare(
        "SELECT DISTINCT r.ref_id, r.kind, r.caller_file, r.module_path, r.target_file
         FROM refs r
         WHERE r.caller_file IN (
             SELECT file_path FROM file_dependencies WHERE dep_file = ?1
         )
            OR r.target_file = ?1
         ORDER BY r.ref_id",
    )?;
    let rows = stmt.query_map(params![rel_path], |row| {
        Ok(RefDependencyRow {
            ref_id: row.get(0)?,
            kind: row.get(1)?,
            caller_file: row.get(2)?,
            module_path: row.get(3)?,
            target_file: row.get(4)?,
        })
    })?;
    let mut ids = Vec::new();
    for row in rows {
        let row = row?;
        if ref_dependency_row_depends_on(project_root, &row, rel_path) {
            ids.push(DependentRefSelection {
                ref_id: row.ref_id,
                caller_file: row.caller_file,
            });
        }
    }
    Ok(ids)
}

fn record_dependent_refs(
    selected_ref_ids: &mut BTreeSet<String>,
    selected_refs_by_caller: &mut BTreeMap<String, BTreeSet<String>>,
    dependent_refs: Vec<DependentRefSelection>,
) {
    for dependent_ref in dependent_refs {
        let DependentRefSelection {
            ref_id,
            caller_file,
        } = dependent_ref;
        selected_ref_ids.insert(ref_id.clone());
        selected_refs_by_caller
            .entry(caller_file)
            .or_default()
            .insert(ref_id);
    }
}

#[cfg(test)]
fn refs_by_caller_for_ref_ids(
    tx: &Transaction<'_>,
    ref_ids: &BTreeSet<String>,
) -> Result<BTreeMap<String, BTreeSet<String>>> {
    let mut by_caller: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    let mut stmt = tx.prepare("SELECT caller_file FROM refs WHERE ref_id = ?1")?;
    for ref_id in ref_ids {
        if let Some(caller) = stmt
            .query_row(params![ref_id], |row| row.get::<_, String>(0))
            .optional()?
        {
            by_caller.entry(caller).or_default().insert(ref_id.clone());
        }
    }
    Ok(by_caller)
}

fn delete_file_rows(tx: &Transaction<'_>, rel_path: &str) -> Result<()> {
    tx.execute(
        "DELETE FROM file_dependencies WHERE file_path = ?1",
        params![rel_path],
    )?;
    delete_refs_for_caller(tx, rel_path)?;
    tx.execute(
        "DELETE FROM dispatch_hints WHERE file = ?1",
        params![rel_path],
    )?;
    tx.execute("DELETE FROM nodes WHERE file_path = ?1", params![rel_path])?;
    tx.execute("DELETE FROM files WHERE path = ?1", params![rel_path])?;
    Ok(())
}

fn delete_refs_for_caller(tx: &Transaction<'_>, rel_path: &str) -> Result<()> {
    let mut stmt = tx.prepare("SELECT ref_id FROM refs WHERE caller_file = ?1")?;
    let rows = stmt.query_map(params![rel_path], |row| row.get::<_, String>(0))?;
    let mut ids = BTreeSet::new();
    for row in rows {
        ids.insert(row?);
    }
    delete_ref_ids(tx, &ids)
}

fn delete_ref_ids(tx: &Transaction<'_>, ref_ids: &BTreeSet<String>) -> Result<()> {
    for ref_id in ref_ids {
        tx.execute("DELETE FROM edges WHERE ref_id = ?1", params![ref_id])?;
        tx.execute("DELETE FROM refs WHERE ref_id = ?1", params![ref_id])?;
    }
    Ok(())
}

fn edge_snapshot_with_conn(conn: &Connection) -> Result<BTreeSet<StoredEdge>> {
    let mut stmt = conn.prepare(
        "SELECT source.file_path, source.scoped_name, edges.target_file,
                edges.target_symbol, edges.kind, edges.line
         FROM edges
         JOIN nodes AS source ON source.id = edges.source_node
         ORDER BY source.file_path, source.scoped_name, edges.target_file,
                  edges.target_symbol, edges.kind, edges.line",
    )?;
    let rows = stmt.query_map([], |row| {
        Ok(StoredEdge {
            source_file: row.get(0)?,
            source_symbol: row.get(1)?,
            target_file: row.get(2)?,
            target_symbol: row.get(3)?,
            kind: row.get(4)?,
            line: row.get::<_, i64>(5)? as u32,
        })
    })?;
    let mut edges = BTreeSet::new();
    for row in rows {
        edges.insert(row?);
    }
    Ok(edges)
}

fn module_target_from_dependencies(
    project_root: &Path,
    dependencies: &BTreeSet<String>,
) -> Option<String> {
    dependencies.iter().find_map(|dep| {
        let path = project_root.join(dep);
        if path.is_file() {
            Some(relative_path(project_root, &canonicalize_path(&path)))
        } else {
            None
        }
    })
}

fn reexport_index_from_raw(raw_ref: &RawRef, target_file: Option<String>) -> ReexportIndex {
    let mut named = HashMap::new();
    if let Some(full_ref) = &raw_ref.full_ref {
        named = parse_reexport_names(full_ref);
    }
    ReexportIndex {
        target_file,
        named,
        wildcard: raw_ref.wildcard,
    }
}

fn parse_reexport_names(statement: &str) -> HashMap<String, String> {
    let mut names = HashMap::new();
    let Some(open) = statement.find('{') else {
        return names;
    };
    let Some(close) = statement[open + 1..]
        .find('}')
        .map(|offset| open + 1 + offset)
    else {
        return names;
    };
    for spec in statement[open + 1..close].split(',') {
        let spec = spec.trim();
        if spec.is_empty() {
            continue;
        }
        if let Some((source, local)) = spec.split_once(" as ") {
            names.insert(local.trim().to_string(), source.trim().to_string());
        } else {
            names.insert(spec.to_string(), spec.to_string());
        }
    }
    names
}

#[derive(Debug)]
struct RefDependencyRow {
    ref_id: String,
    kind: String,
    caller_file: String,
    module_path: Option<String>,
    target_file: Option<String>,
}

fn ref_dependency_row_depends_on(
    project_root: &Path,
    row: &RefDependencyRow,
    rel_path: &str,
) -> bool {
    if row.target_file.as_deref() == Some(rel_path) {
        return true;
    }

    match row.kind.as_str() {
        "call" => true,
        "import" | "reexport" => row
            .module_path
            .as_deref()
            .map(|module_path| {
                module_dependencies_for_ref(project_root, &row.caller_file, module_path)
                    .contains(rel_path)
            })
            .unwrap_or(false),
        "export_alias" => false,
        _ => false,
    }
}

fn module_dependencies_for_ref(
    project_root: &Path,
    caller_file: &str,
    module_path: &str,
) -> BTreeSet<String> {
    module_dependencies(project_root, &project_root.join(caller_file), module_path)
}

fn import_dependencies(
    project_root: &Path,
    abs_path: &Path,
    imports: &[ImportStatement],
) -> BTreeSet<String> {
    let mut deps = BTreeSet::new();
    for import in imports {
        deps.extend(module_dependencies(
            project_root,
            abs_path,
            &import.module_path,
        ));
    }
    deps
}

fn module_dependencies(
    project_root: &Path,
    abs_path: &Path,
    module_path: &str,
) -> BTreeSet<String> {
    let mut deps = rust_module_dependencies(project_root, abs_path, module_path);
    let caller_dir = abs_path.parent().unwrap_or(project_root);
    if let Some(resolved) = callgraph::resolve_module_path(caller_dir, module_path) {
        deps.insert(relative_path(project_root, &resolved));
    }
    if module_path.starts_with('.') {
        let base = caller_dir.join(module_path);
        for candidate in relative_module_candidates(&base) {
            deps.insert(relative_path(project_root, &candidate));
        }
    }
    deps
}

fn rust_module_dependencies(
    project_root: &Path,
    abs_path: &Path,
    module_path: &str,
) -> BTreeSet<String> {
    let mut deps = BTreeSet::new();
    let rel_path = relative_path(project_root, &canonicalize_path(abs_path));
    let Some(path_segments) = rust_module_dependency_segments(&rel_path, module_path) else {
        return deps;
    };
    let src_prefix = rust_src_prefix(&rel_path);
    rust_push_module_dependency_candidate(project_root, &mut deps, &src_prefix, &path_segments);
    if !path_segments.is_empty() {
        rust_push_module_dependency_candidate(
            project_root,
            &mut deps,
            &src_prefix,
            &path_segments[..path_segments.len() - 1],
        );
    }
    deps
}

fn rust_module_dependency_segments(rel_path: &str, module_path: &str) -> Option<Vec<String>> {
    let path = rust_module_path_without_alias_or_use_list(module_path);
    let segments = path
        .split("::")
        .map(str::trim)
        .filter(|segment| !segment.is_empty())
        .collect::<Vec<_>>();
    if segments.is_empty() || matches!(segments[0], "std" | "core" | "alloc") {
        return None;
    }
    rust_resolve_segments(rel_path, &segments)
}

fn rust_module_path_without_alias_or_use_list(module_path: &str) -> &str {
    let path = module_path
        .trim()
        .trim_end_matches(';')
        .split_once(" as ")
        .map(|(left, _)| left.trim())
        .unwrap_or_else(|| module_path.trim().trim_end_matches(';'));
    path.find("::{").map(|brace| &path[..brace]).unwrap_or(path)
}

fn rust_push_module_dependency_candidate(
    project_root: &Path,
    deps: &mut BTreeSet<String>,
    src_prefix: &str,
    segments: &[String],
) {
    let candidates = if segments.is_empty() {
        vec![
            format!("{src_prefix}/lib.rs"),
            format!("{src_prefix}/main.rs"),
        ]
    } else {
        vec![
            format!("{}/{}.rs", src_prefix, segments.join("/")),
            format!("{}/{}/mod.rs", src_prefix, segments.join("/")),
        ]
    };
    for candidate in candidates {
        if project_root.join(&candidate).is_file() {
            deps.insert(candidate);
        }
    }
}

fn relative_module_candidates(base: &Path) -> Vec<PathBuf> {
    let mut candidates = Vec::new();
    if base.extension().is_some() {
        candidates.push(base.to_path_buf());
        return candidates;
    }
    for ext in JS_TS_EXTENSIONS {
        candidates.push(base.with_extension(ext));
    }
    for ext in JS_TS_EXTENSIONS {
        candidates.push(base.join(format!("index.{ext}")));
    }
    candidates
}

fn import_local_names(import: &ImportStatement) -> Vec<String> {
    let mut names = Vec::new();
    if let Some(default) = &import.default_import {
        names.push(default.clone());
    }
    if let Some(namespace) = &import.namespace_import {
        names.push(namespace.clone());
    }
    for name in &import.names {
        names.push(crate::imports::specifier_local_name(name).to_string());
    }
    names
}

fn import_requested_names(import: &ImportStatement) -> Vec<String> {
    import
        .names
        .iter()
        .map(|name| crate::imports::specifier_imported_name(name).to_string())
        .collect()
}

fn import_is_wildcard(import: &ImportStatement) -> bool {
    import.namespace_import.is_some() || import.raw_text.contains('*')
}

fn namespace_alias(full_ref: &str) -> Option<String> {
    full_ref
        .split_once('.')
        .map(|(namespace, _)| namespace.to_string())
}

fn import_kind_label(kind: ImportKind) -> &'static str {
    match kind {
        ImportKind::Value => "value",
        ImportKind::Type => "type",
        ImportKind::SideEffect => "side_effect",
    }
}

fn symbol_kind_label(kind: &SymbolKind) -> &'static str {
    match kind {
        SymbolKind::Function => "function",
        SymbolKind::Class => "class",
        SymbolKind::Method => "method",
        SymbolKind::Struct => "struct",
        SymbolKind::Interface => "interface",
        SymbolKind::Enum => "enum",
        SymbolKind::TypeAlias => "type_alias",
        SymbolKind::Variable => "variable",
        SymbolKind::Heading => "heading",
        SymbolKind::FileSummary => "file_summary",
    }
}

fn is_type_like(kind: &SymbolKind) -> bool {
    matches!(
        kind,
        SymbolKind::Class
            | SymbolKind::Struct
            | SymbolKind::Interface
            | SymbolKind::Enum
            | SymbolKind::TypeAlias
    )
}

fn lang_label(lang: LangId) -> &'static str {
    match lang {
        LangId::TypeScript => "typescript",
        LangId::Tsx => "tsx",
        LangId::JavaScript => "javascript",
        LangId::Python => "python",
        LangId::Rust => "rust",
        LangId::Go => "go",
        LangId::C => "c",
        LangId::Cpp => "cpp",
        LangId::Zig => "zig",
        LangId::CSharp => "csharp",
        LangId::Bash => "bash",
        LangId::Html => "html",
        LangId::Markdown => "markdown",
        LangId::Solidity => "solidity",
        LangId::Scss => "scss",
        LangId::Vue => "vue",
        LangId::Json => "json",
        LangId::Scala => "scala",
        LangId::Java => "java",
        LangId::Ruby => "ruby",
        LangId::Kotlin => "kotlin",
        LangId::Swift => "swift",
        LangId::Php => "php",
        LangId::Lua => "lua",
        LangId::Perl => "perl",
        LangId::Yaml => "yaml",
        LangId::Pascal => "pascal",
        LangId::R => "r",
        LangId::Groovy => "groovy",
        LangId::ObjC => "objc",
    }
}

fn lang_from_label(label: &str) -> Option<LangId> {
    match label {
        "typescript" => Some(LangId::TypeScript),
        "tsx" => Some(LangId::Tsx),
        "javascript" => Some(LangId::JavaScript),
        "python" => Some(LangId::Python),
        "rust" => Some(LangId::Rust),
        "go" => Some(LangId::Go),
        "c" => Some(LangId::C),
        "cpp" => Some(LangId::Cpp),
        "zig" => Some(LangId::Zig),
        "csharp" => Some(LangId::CSharp),
        "bash" => Some(LangId::Bash),
        "html" => Some(LangId::Html),
        "markdown" => Some(LangId::Markdown),
        "solidity" => Some(LangId::Solidity),
        "scss" => Some(LangId::Scss),
        "vue" => Some(LangId::Vue),
        "json" => Some(LangId::Json),
        "scala" => Some(LangId::Scala),
        "java" => Some(LangId::Java),
        "ruby" => Some(LangId::Ruby),
        "kotlin" => Some(LangId::Kotlin),
        "swift" => Some(LangId::Swift),
        "php" => Some(LangId::Php),
        "lua" => Some(LangId::Lua),
        "perl" => Some(LangId::Perl),
        "yaml" => Some(LangId::Yaml),
        "pascal" => Some(LangId::Pascal),
        "r" => Some(LangId::R),
        "groovy" => Some(LangId::Groovy),
        "objc" => Some(LangId::ObjC),
        _ => None,
    }
}

fn normalize_file_list(project_root: &Path, files: &[PathBuf]) -> Result<Vec<PathBuf>> {
    let mut normalized = if files.is_empty() {
        callgraph::walk_project_files(project_root).collect::<Vec<_>>()
    } else {
        files
            .iter()
            .map(|path| normalize_file_path(project_root, path))
            .collect::<Result<Vec<_>>>()?
    };
    normalized.sort();
    normalized.dedup();
    Ok(normalized)
}

fn normalize_file_path(project_root: &Path, path: &Path) -> Result<PathBuf> {
    let full_path = if path.is_relative() {
        project_root.join(path)
    } else {
        path.to_path_buf()
    };
    Ok(canonicalize_path(&full_path))
}

fn canonicalize_path(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

fn relative_path(project_root: &Path, path: &Path) -> String {
    if let Ok(stripped) = path.strip_prefix(project_root) {
        return stripped.to_string_lossy().replace('\\', "/");
    }
    let canon_root = canonicalize_path(project_root);
    let canon_path = canonicalize_path(path);
    if let Ok(stripped) = canon_path.strip_prefix(&canon_root) {
        return stripped.to_string_lossy().replace('\\', "/");
    }
    canon_path.to_string_lossy().replace('\\', "/")
}

fn unqualified_name(scoped: &str) -> &str {
    if scoped == TOP_LEVEL_SYMBOL {
        return scoped;
    }
    scoped
        .rsplit("::")
        .next()
        .unwrap_or(scoped)
        .rsplit('.')
        .next()
        .unwrap_or(scoped)
        .rsplit('#')
        .next()
        .unwrap_or(scoped)
}

fn ref_id(parts: &[&str]) -> String {
    let joined = parts.join("\0");
    hash_to_hex(blake3::hash(joined.as_bytes()))
}

fn hash_to_hex(hash: blake3::Hash) -> String {
    hash.to_hex().to_string()
}

fn hash_from_hex(value: &str) -> Option<blake3::Hash> {
    let bytes = hex_to_bytes(value)?;
    Some(blake3::Hash::from_bytes(bytes))
}

fn hex_to_bytes(value: &str) -> Option<[u8; 32]> {
    if value.len() != 64 {
        return None;
    }
    let mut bytes = [0u8; 32];
    for (index, slot) in bytes.iter_mut().enumerate() {
        let start = index * 2;
        let end = start + 2;
        *slot = u8::from_str_radix(&value[start..end], 16).ok()?;
    }
    Some(bytes)
}

#[derive(Debug, Clone)]
struct LineIndex {
    newline_offsets: Vec<usize>,
    source_len: usize,
}

impl LineIndex {
    fn new(source: &str) -> Self {
        Self {
            newline_offsets: source
                .bytes()
                .enumerate()
                .filter_map(|(offset, byte)| (byte == b'\n').then_some(offset))
                .collect(),
            source_len: source.len(),
        }
    }

    fn byte_to_line(&self, byte_offset: usize) -> u32 {
        let byte_offset = byte_offset.min(self.source_len);
        self.newline_offsets
            .partition_point(|offset| *offset < byte_offset) as u32
            + 1
    }
}

fn empty_to_none(value: String) -> Option<String> {
    if value.is_empty() {
        None
    } else {
        Some(value)
    }
}

fn bool_int(value: bool) -> i64 {
    if value {
        1
    } else {
        0
    }
}

fn system_time_to_ns(time: SystemTime) -> i64 {
    time.duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos()
        .min(i64::MAX as u128) as i64
}

fn ns_to_system_time(value: i64) -> SystemTime {
    UNIX_EPOCH + Duration::from_nanos(value.max(0) as u64)
}

fn unix_millis_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(u128::from(u64::MAX)) as u64
}

fn unix_seconds_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

/// Serializes every test that drives the process-wide refresh worker
/// (enqueue/flush swap the shared worker slot; a concurrent flush can shut a
/// worker down between another test's enqueue and its flush, deferring the
/// batch and zeroing that test's seam counts).
#[cfg(test)]
pub(crate) static REFRESH_WORKER_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(test)]
mod refresh_worker_tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    fn ready_store_fixture() -> (tempfile::TempDir, PathBuf, PathBuf, PathBuf) {
        let temp = tempdir().unwrap();
        let root = temp.path().join("root");
        fs::create_dir_all(&root).unwrap();
        let artifact_key = crate::search_index::artifact_cache_key(&root);
        crate::root_cache::configure_artifact_access(&root, &artifact_key, false);
        let callgraph_dir = temp
            .path()
            .join("storage")
            .join("callgraph")
            .join(artifact_key);
        let source = root.join("main.rs");
        fs::write(&source, "fn entry() { old_leaf(); }\nfn old_leaf() {}\n").unwrap();
        let (store, _) = CallGraphStore::cold_build_with_lease(
            callgraph_dir.clone(),
            root.clone(),
            std::slice::from_ref(&source),
        )
        .unwrap();
        drop(store);
        (temp, root, callgraph_dir, source)
    }

    fn pending_paths() -> PendingCallGraphStorePaths {
        Arc::new(parking_lot::Mutex::new(BTreeSet::new()))
    }

    fn wait_for_refresh_calls(root: &Path, expected: usize) {
        let deadline = Instant::now() + Duration::from_secs(12);
        while callgraph_refresh_worker_test_counts(root).0 < expected {
            assert!(
                Instant::now() < deadline,
                "timed out waiting for {expected} callgraph refresh worker call(s)"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    fn wait_for_refresh_worker_idle() {
        let deadline = Instant::now() + Duration::from_secs(12);
        loop {
            let worker = CALLGRAPH_REFRESH_WORKER
                .get_or_init(|| Mutex::new(None))
                .lock()
                .expect("callgraph refresh worker mutex poisoned")
                .clone();
            let idle = worker.is_none_or(|worker| {
                let queue = worker
                    .shared
                    .queue
                    .lock()
                    .expect("callgraph refresh queue mutex poisoned");
                queue.active.is_none() && queue.order.is_empty()
            });
            if idle {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for callgraph refresh worker to become idle"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    fn workspace_refresh_fixture() -> (tempfile::TempDir, PathBuf, PathBuf, PathBuf) {
        let temp = tempdir().unwrap();
        let root = temp.path().join("workspace");
        fs::create_dir_all(root.join("app/src")).unwrap();
        let artifact_key = crate::search_index::artifact_cache_key(&root);
        crate::root_cache::configure_artifact_access(&root, &artifact_key, false);
        let callgraph_dir = temp
            .path()
            .join("storage")
            .join("callgraph")
            .join(artifact_key);
        fs::write(
            root.join("Cargo.toml"),
            "[workspace]\nmembers = [\"app\"]\nresolver = \"2\"\n",
        )
        .unwrap();
        fs::write(
            root.join("app/Cargo.toml"),
            "[package]\nname = \"app\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        )
        .unwrap();
        let caller = root.join("app/src/lib.rs");
        fs::write(&caller, "pub fn run() { added_crate::target(); }\n").unwrap();
        let (store, _) = CallGraphStore::cold_build_with_lease(
            callgraph_dir.clone(),
            root.clone(),
            std::slice::from_ref(&caller),
        )
        .unwrap();
        drop(store);
        (temp, root, callgraph_dir, caller)
    }

    #[test]
    fn refresh_worker_reuses_workspace_prefix_cache_for_one_root() {
        let _guard = REFRESH_WORKER_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let _ = flush_callgraph_store_refreshes_with_budget(Duration::from_secs(30));
        let (_temp, root, callgraph_dir, caller) = workspace_refresh_fixture();
        reset_workspace_crate_prefix_build_count(&root);
        set_callgraph_refresh_worker_test_seam(root.clone(), Duration::ZERO, false);

        for revision in ["first", "second"] {
            fs::write(
                &caller,
                format!("pub fn run() {{ added_crate::target(); }}\n// {revision}\n"),
            )
            .unwrap();
            enqueue_callgraph_store_refresh(
                callgraph_dir.clone(),
                root.clone(),
                vec![caller.clone()],
                pending_paths(),
            );
            wait_for_refresh_worker_idle();
        }

        assert_eq!(workspace_crate_prefix_build_count(&root), 1);
        assert!(flush_callgraph_store_refreshes_with_budget(
            Duration::from_secs(5)
        ));
        clear_callgraph_refresh_worker_test_seam(&root);
    }

    #[test]
    fn manifest_event_rebuilds_workspace_prefix_cache_and_resolves_new_crate() {
        let _guard = REFRESH_WORKER_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let _ = flush_callgraph_store_refreshes_with_budget(Duration::from_secs(30));
        let (_temp, root, callgraph_dir, caller) = workspace_refresh_fixture();
        reset_workspace_crate_prefix_build_count(&root);
        set_callgraph_refresh_worker_test_seam(root.clone(), Duration::ZERO, false);

        fs::write(
            &caller,
            "pub fn run() { added_crate::target(); }\n// prime missing-crate map\n",
        )
        .unwrap();
        enqueue_callgraph_store_refresh(
            callgraph_dir.clone(),
            root.clone(),
            vec![caller.clone()],
            pending_paths(),
        );
        wait_for_refresh_worker_idle();
        assert_eq!(workspace_crate_prefix_build_count(&root), 1);

        let added_manifest = root.join("added/Cargo.toml");
        let added_source = root.join("added/src/lib.rs");
        fs::create_dir_all(added_source.parent().unwrap()).unwrap();
        fs::write(
            root.join("Cargo.toml"),
            "[workspace]\nmembers = [\"app\", \"added\"]\nresolver = \"2\"\n",
        )
        .unwrap();
        fs::write(
            &added_manifest,
            "[package]\nname = \"added-crate\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        )
        .unwrap();
        fs::write(&added_source, "pub fn target() {}\n").unwrap();
        fs::write(
            &caller,
            "pub fn run() { added_crate::target(); }\n// resolve added crate\n",
        )
        .unwrap();

        enqueue_callgraph_store_refresh(
            callgraph_dir.clone(),
            root.clone(),
            vec![
                root.join("Cargo.toml"),
                added_manifest,
                added_source,
                caller,
            ],
            pending_paths(),
        );
        assert!(flush_callgraph_store_refreshes_with_budget(
            Duration::from_secs(12)
        ));

        // This is the negative control for a permanently-static cache: without
        // manifest invalidation the build count stays at one and the call remains
        // unresolved because `added_crate` was absent when the map was primed.
        assert_eq!(workspace_crate_prefix_build_count(&root), 2);
        let store = CallGraphStore::open_readonly(callgraph_dir, root.clone())
            .unwrap()
            .expect("refreshed workspace store");
        let tree = store
            .call_tree(Path::new("app/src/lib.rs"), "run", 1)
            .unwrap();
        assert_eq!(tree.children.len(), 1);
        assert_eq!(tree.children[0].file, "added/src/lib.rs");
        assert_eq!(tree.children[0].name, "target");
        assert!(tree.children[0].resolved);
        clear_callgraph_refresh_worker_test_seam(&root);
    }

    fn linked_worktree_fixture() -> (tempfile::TempDir, PathBuf, PathBuf, String, PathBuf) {
        let temp = tempdir().unwrap();
        let main = temp.path().join("main");
        let worktree = temp.path().join("worktree");
        fs::create_dir_all(&main).unwrap();
        let mut git = std::process::Command::new("git");
        assert!(
            crate::test_env::apply_hermetic_git_env(git.arg("init").arg(&main))
                .status()
                .unwrap()
                .success()
        );
        fs::write(main.join("lib.rs"), "pub fn marker() {}\n").unwrap();
        for args in [
            vec![
                "-C",
                main.to_str().unwrap(),
                "config",
                "user.email",
                "test@example.com",
            ],
            vec![
                "-C",
                main.to_str().unwrap(),
                "config",
                "user.name",
                "AFT Test",
            ],
            vec!["-C", main.to_str().unwrap(), "add", "lib.rs"],
            vec!["-C", main.to_str().unwrap(), "commit", "-m", "fixture"],
        ] {
            let mut command = std::process::Command::new("git");
            assert!(crate::test_env::apply_hermetic_git_env(command.args(args))
                .status()
                .unwrap()
                .success());
        }
        let mut add_worktree = std::process::Command::new("git");
        assert!(crate::test_env::apply_hermetic_git_env(
            add_worktree
                .arg("-C")
                .arg(&main)
                .args(["worktree", "add", "--detach"])
                .arg(&worktree),
        )
        .status()
        .unwrap()
        .success());
        let main = fs::canonicalize(main).unwrap();
        let worktree = fs::canonicalize(worktree).unwrap();
        let project_key = crate::search_index::artifact_cache_key(&main);
        assert_eq!(
            crate::search_index::artifact_cache_key(&worktree),
            project_key
        );
        let callgraph_dir = temp.path().join("callgraph").join(&project_key);
        (temp, main, worktree, project_key, callgraph_dir)
    }

    #[test]
    fn linked_worktree_never_acquires_writer_or_publishes_any_build_path() {
        let _git_env = crate::test_env::hermetic_git_env_guard();
        let (_temp, _main, root, project_key, callgraph_dir) = linked_worktree_fixture();
        crate::root_cache::configure_artifact_access(&root, &project_key, true);
        crate::root_cache::reset_writer_lease_acquisition_counts_for_test();
        let publications = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let publications_for_observer = Arc::clone(&publications);
        set_cold_build_swap_observer(Some(Arc::new(move |_, _| {
            publications_for_observer.fetch_add(1, AtomicOrdering::SeqCst);
        })));
        let source = root.join("lib.rs");

        let open_error = CallGraphStore::open(callgraph_dir.clone(), root.clone())
            .expect_err("borrow-only writable open must remain unavailable");
        assert!(matches!(open_error, CallGraphStoreError::Unavailable(_)));
        assert!(
            CallGraphStore::open_ready_repairing(callgraph_dir.clone(), root.clone())
                .unwrap()
                .is_none()
        );
        assert!(
            CallGraphStore::open_ready_no_rebuild(callgraph_dir.clone(), root.clone())
                .unwrap()
                .is_none()
        );
        assert!(matches!(
            CallGraphStore::cold_build_with_lease(
                callgraph_dir.clone(),
                root.clone(),
                std::slice::from_ref(&source),
            ),
            Err(CallGraphStoreError::Unavailable(_))
        ));
        assert!(matches!(
            CallGraphStore::ensure_built_with_lease(
                callgraph_dir.clone(),
                root.clone(),
                std::slice::from_ref(&source),
            ),
            Err(CallGraphStoreError::Unavailable(_))
        ));
        let force_error = CallGraphStore::force_cold_build_with_lease_chunked(
            callgraph_dir.clone(),
            root.clone(),
            &[source],
            1,
        )
        .expect_err("borrow-only forced rebuild must remain unsatisfied");
        set_cold_build_swap_observer(None);

        assert!(matches!(force_error, CallGraphStoreError::Unavailable(_)));
        assert_eq!(
            crate::root_cache::writer_lease_acquisition_count_for_test(
                crate::root_cache::RootCacheDomain::Callgraph,
                &project_key,
                &root,
            ),
            0
        );
        assert_eq!(publications.load(AtomicOrdering::SeqCst), 0);
        assert!(!pointer_path(&callgraph_dir, &project_key).exists());
    }

    #[test]
    fn owner_and_linked_worktree_alternation_rebuilds_storm_generation_once() {
        let _git_env = crate::test_env::hermetic_git_env_guard();
        let (_temp, owner, worktree, project_key, callgraph_dir) = linked_worktree_fixture();
        crate::root_cache::configure_artifact_access(&owner, &project_key, false);
        crate::root_cache::configure_artifact_access(&worktree, &project_key, true);
        let source = owner.join("lib.rs");
        let (store, _) = CallGraphStore::cold_build_with_lease(
            callgraph_dir.clone(),
            owner.clone(),
            std::slice::from_ref(&source),
        )
        .unwrap();
        let sqlite_path = store.sqlite_path().to_path_buf();
        drop(store);

        let conn = Connection::open(&sqlite_path).unwrap();
        conn.execute(
            "UPDATE backend_file_state SET workspace_root = ?1",
            [worktree.display().to_string()],
        )
        .unwrap();
        drop(conn);

        let publications = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let publications_for_observer = Arc::clone(&publications);
        set_cold_build_swap_observer(Some(Arc::new(move |_, _| {
            publications_for_observer.fetch_add(1, AtomicOrdering::SeqCst);
        })));
        crate::root_cache::reset_writer_lease_acquisition_counts_for_test();

        let repaired = CallGraphStore::open_ready_repairing(callgraph_dir.clone(), owner.clone())
            .unwrap()
            .expect("owner should purge the storm-era worktree root");
        drop(repaired);
        for _ in 0..3 {
            let borrower = CallGraphStore::open_readonly(callgraph_dir.clone(), worktree.clone())
                .unwrap()
                .expect("linked worktree should borrow the owner generation");
            drop(borrower);
            assert!(
                CallGraphStore::open_ready_repairing(callgraph_dir.clone(), worktree.clone())
                    .unwrap()
                    .is_none()
            );
            let owner_store =
                CallGraphStore::open_ready_repairing(callgraph_dir.clone(), owner.clone())
                    .unwrap()
                    .expect("owner generation should remain ready");
            drop(owner_store);
        }
        set_cold_build_swap_observer(None);

        assert_eq!(
            publications.load(AtomicOrdering::SeqCst),
            1,
            "the owner performs one expected post-storm purge and alternation stays read-only"
        );
        assert_eq!(
            crate::root_cache::writer_lease_acquisition_count_for_test(
                crate::root_cache::RootCacheDomain::Callgraph,
                &project_key,
                &worktree,
            ),
            0
        );
    }

    #[test]
    fn rebuild_cooldown_records_only_successful_publication_per_cache_key() {
        let temp = tempdir().unwrap();
        let root = temp.path().join("owner");
        let other_root = temp.path().join("other");
        fs::create_dir_all(&root).unwrap();
        fs::create_dir_all(&other_root).unwrap();
        let source = root.join("lib.rs");
        fs::write(&source, "pub fn marker() {}\n").unwrap();
        let project_key = crate::search_index::artifact_cache_key(&root);
        let callgraph_dir = temp.path().join("callgraph").join(&project_key);
        crate::root_cache::configure_artifact_access(&root, &project_key, false);
        let cooldown_key = rebuild_cooldown_key(&callgraph_dir, &project_key);
        rebuild_cooldown_records()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&cooldown_key);
        let epoch = crate::root_cache::ArtifactPublishEpoch::default();
        let stale_epoch = epoch.current();
        epoch.next();

        let failed = with_publish_epoch(epoch, stale_epoch, || {
            CallGraphStore::cold_build_with_lease(
                callgraph_dir.clone(),
                root.clone(),
                std::slice::from_ref(&source),
            )
        });
        assert!(matches!(failed, Err(CallGraphStoreError::Superseded)));
        assert!(
            rebuild_cooldown_denial(&callgraph_dir, &project_key, &other_root, Instant::now(),)
                .is_none()
        );

        let (store, _) = CallGraphStore::cold_build_with_lease(
            callgraph_dir.clone(),
            root.clone(),
            std::slice::from_ref(&source),
        )
        .unwrap();
        drop(store);
        assert!(
            rebuild_cooldown_denial(&callgraph_dir, &project_key, &other_root, Instant::now(),)
                .is_none()
        );

        record_successful_rebuild(&callgraph_dir, &project_key, &other_root, Instant::now());
        assert!(
            rebuild_cooldown_denial(&callgraph_dir, &project_key, &root, Instant::now(),).is_some()
        );
    }

    #[test]
    fn fenced_refresh_with_stale_lifecycle_generation_defers_paths_without_commit() {
        let _guard = REFRESH_WORKER_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let _ = flush_callgraph_store_refreshes_with_budget(Duration::from_secs(30));
        let (_temp, root, callgraph_dir, source) = ready_store_fixture();
        let pending = pending_paths();
        set_callgraph_refresh_worker_test_seam(root.clone(), Duration::ZERO, false);

        let lifecycle = SubcLifecycleAdmission::default();
        let generation = Arc::new(std::sync::atomic::AtomicU64::new(7));
        let publish_epoch = crate::root_cache::ArtifactPublishEpoch::default();
        let ticket = CallgraphRefreshTicket::new(
            lifecycle,
            Arc::clone(&generation),
            7,
            publish_epoch.clone(),
            publish_epoch.current(),
        );
        // Supersede before the worker runs: the batch must defer, not commit.
        generation.store(8, std::sync::atomic::Ordering::SeqCst);
        let installed = CallGraphStore::open_readonly(callgraph_dir.clone(), root.clone())
            .unwrap()
            .expect("ready store snapshot");
        let refresh_state = CallgraphRefreshState::new(
            Arc::new(std::sync::RwLock::new(Some(Arc::new(installed)))),
            Arc::new(AtomicBool::new(true)),
        );

        enqueue_callgraph_store_refresh_fenced_with_state(
            callgraph_dir,
            root.clone(),
            vec![source.clone()],
            Arc::clone(&pending),
            refresh_state,
            ticket,
        );
        assert!(flush_callgraph_store_refreshes_with_budget(
            Duration::from_secs(5)
        ));
        assert_eq!(
            callgraph_refresh_worker_test_counts(&root).0,
            0,
            "superseded batch must not reach refresh_files or self-replay"
        );
        assert!(
            pending.lock().contains(&source),
            "superseded batch must defer its paths to the pending sink"
        );
        clear_callgraph_refresh_worker_test_seam(&root);
    }

    #[test]
    fn superseded_open_failure_defers_without_self_replay() {
        let _guard = REFRESH_WORKER_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let _ = flush_callgraph_store_refreshes_with_budget(Duration::from_secs(30));
        let (_temp, root, callgraph_dir, source) = ready_store_fixture();
        let pending = pending_paths();
        let installed = Arc::new(
            CallGraphStore::open_readonly(callgraph_dir.clone(), root.clone())
                .unwrap()
                .expect("ready store snapshot"),
        );
        let refresh_state = CallgraphRefreshState::new(
            Arc::new(std::sync::RwLock::new(Some(Arc::clone(&installed)))),
            Arc::new(AtomicBool::new(true)),
        );
        assert!(!installed.is_legacy_fallback());
        assert!(installed.is_current());
        fs::write(&source, "fn entry() { new_leaf(); }\nfn new_leaf() {}\n").unwrap();
        set_callgraph_refresh_worker_test_seam(root.clone(), Duration::ZERO, false);
        set_callgraph_refresh_worker_test_open_failure(root.clone(), true);
        let (held_rx, release_tx) = install_callgraph_refresh_worker_test_gate(root.clone());

        let lifecycle = SubcLifecycleAdmission::default();
        let generation = Arc::new(std::sync::atomic::AtomicU64::new(7));
        let publish_epoch = crate::root_cache::ArtifactPublishEpoch::default();
        let ticket = CallgraphRefreshTicket::new(
            lifecycle,
            Arc::clone(&generation),
            7,
            publish_epoch.clone(),
            publish_epoch.current(),
        );
        enqueue_callgraph_store_refresh_fenced_with_state(
            callgraph_dir,
            root.clone(),
            vec![source.clone()],
            Arc::clone(&pending),
            refresh_state,
            ticket,
        );
        held_rx
            .recv_timeout(Duration::from_secs(12))
            .expect("refresh worker must hold after injected open failure");

        // Mark the refresh request obsolete after the injected open failure,
        // then unblock the worker before its deferred retry can run.
        generation.store(8, std::sync::atomic::Ordering::SeqCst);
        set_callgraph_refresh_worker_test_open_failure(root.clone(), false);
        release_tx
            .send(())
            .expect("release superseded refresh worker");
        wait_for_refresh_worker_idle();

        assert_eq!(
            callgraph_refresh_worker_test_counts(&root).0,
            1,
            "superseded open-failure batch must not self-replay"
        );
        assert_eq!(
            callgraph_refresh_worker_test_worker_calls(&root),
            1,
            "superseded open-failure batch must not create another worker call"
        );
        assert!(
            pending.lock().contains(&source),
            "superseded open-failure paths must remain in the pending sink"
        );
        let tree = installed
            .call_tree(Path::new("main.rs"), "entry", 1)
            .unwrap();
        assert_eq!(
            tree.children[0].name, "old_leaf",
            "superseded open-failure batch must not converge the store"
        );
        clear_callgraph_refresh_worker_test_seam(&root);
    }

    #[test]
    fn fenced_refresh_with_advanced_publish_epoch_defers_paths_without_commit() {
        let _guard = REFRESH_WORKER_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let _ = flush_callgraph_store_refreshes_with_budget(Duration::from_secs(30));
        let (_temp, root, callgraph_dir, source) = ready_store_fixture();
        let pending = pending_paths();
        set_callgraph_refresh_worker_test_seam(root.clone(), Duration::ZERO, false);

        let lifecycle = SubcLifecycleAdmission::default();
        let generation = Arc::new(std::sync::atomic::AtomicU64::new(3));
        let publish_epoch = crate::root_cache::ArtifactPublishEpoch::default();
        let expected_epoch = publish_epoch.current();
        let ticket = CallgraphRefreshTicket::new(
            lifecycle,
            generation,
            3,
            publish_epoch.clone(),
            expected_epoch,
        );
        // A cold build published a replacement generation after enqueue.
        publish_epoch.next();

        enqueue_callgraph_store_refresh_fenced(
            callgraph_dir,
            root.clone(),
            vec![source.clone()],
            Arc::clone(&pending),
            ticket,
        );
        assert!(flush_callgraph_store_refreshes_with_budget(
            Duration::from_secs(5)
        ));
        assert_eq!(
            callgraph_refresh_worker_test_counts(&root).0,
            0,
            "epoch-superseded batch must not reach refresh_files"
        );
        assert!(
            pending.lock().contains(&source),
            "epoch-superseded batch must defer its paths to the pending sink"
        );
        clear_callgraph_refresh_worker_test_seam(&root);
    }

    #[test]
    fn fenced_refresh_with_current_ticket_commits_normally() {
        let _guard = REFRESH_WORKER_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let _ = flush_callgraph_store_refreshes_with_budget(Duration::from_secs(30));
        let (_temp, root, callgraph_dir, source) = ready_store_fixture();
        let pending = pending_paths();
        set_callgraph_refresh_worker_test_seam(root.clone(), Duration::ZERO, false);

        fs::write(&source, "fn entry() { new_leaf(); }\nfn new_leaf() {}\n").unwrap();

        let lifecycle = SubcLifecycleAdmission::default();
        let generation = Arc::new(std::sync::atomic::AtomicU64::new(5));
        let publish_epoch = crate::root_cache::ArtifactPublishEpoch::default();
        let ticket = CallgraphRefreshTicket::new(
            lifecycle,
            generation,
            5,
            publish_epoch.clone(),
            publish_epoch.current(),
        );

        enqueue_callgraph_store_refresh_fenced(
            callgraph_dir.clone(),
            root.clone(),
            vec![source.clone()],
            Arc::clone(&pending),
            ticket,
        );
        assert!(flush_callgraph_store_refreshes_with_budget(
            Duration::from_secs(5)
        ));
        assert_eq!(
            callgraph_refresh_worker_test_counts(&root).0,
            1,
            "current ticket must run the refresh"
        );
        assert!(
            pending.lock().is_empty(),
            "committed batch must not defer paths"
        );

        let store = CallGraphStore::open_readonly(callgraph_dir, root.clone())
            .unwrap()
            .expect("published generation must remain readable");
        let tree = store.call_tree(Path::new("main.rs"), "entry", 1).unwrap();
        assert_eq!(
            tree.children[0].name, "new_leaf",
            "fenced commit must actually persist the refreshed content"
        );
        clear_callgraph_refresh_worker_test_seam(&root);
    }

    #[test]
    fn queued_batches_for_one_root_coalesce_while_worker_is_busy() {
        let _guard = REFRESH_WORKER_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // Generous pre-drain: the refresh worker is process-wide, so a prior
        // test's still-running batch (slow Windows CI) must fully settle
        // before this test enqueues, or its wait deadline absorbs the
        // leftover work. Idle workers return immediately.
        let _ = flush_callgraph_store_refreshes_with_budget(Duration::from_secs(30));
        let (_temp, root, callgraph_dir, source) = ready_store_fixture();
        let pending = pending_paths();
        set_callgraph_refresh_worker_test_seam(root.clone(), Duration::from_millis(150), false);

        enqueue_callgraph_store_refresh(
            callgraph_dir.clone(),
            root.clone(),
            vec![source.clone()],
            Arc::clone(&pending),
        );
        wait_for_refresh_calls(&root, 1);
        for _ in 0..3 {
            enqueue_callgraph_store_refresh(
                callgraph_dir.clone(),
                root.clone(),
                vec![source.clone()],
                Arc::clone(&pending),
            );
        }

        assert!(flush_callgraph_store_refreshes_with_budget(
            Duration::from_secs(2)
        ));
        assert_eq!(callgraph_refresh_worker_test_counts(&root).0, 2);
        assert!(pending.lock().is_empty());
        clear_callgraph_refresh_worker_test_seam(&root);
    }

    #[test]
    fn queued_refresh_opens_generation_published_after_enqueue() {
        let _guard = REFRESH_WORKER_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // Generous pre-drain: the refresh worker is process-wide, so a prior
        // test's still-running batch (slow Windows CI) must fully settle
        // before this test enqueues, or its wait deadline absorbs the
        // leftover work. Idle workers return immediately.
        let _ = flush_callgraph_store_refreshes_with_budget(Duration::from_secs(30));
        let (_active_temp, active_root, active_dir, active_source) = ready_store_fixture();
        let (_target_temp, target_root, target_dir, target_source) = ready_store_fixture();
        set_callgraph_refresh_worker_test_seam(active_root.clone(), Duration::ZERO, false);
        let (active_held_rx, active_release_tx) =
            install_callgraph_refresh_worker_test_gate(active_root.clone());
        set_callgraph_refresh_worker_test_seam(target_root.clone(), Duration::ZERO, false);
        enqueue_callgraph_store_refresh(
            active_dir,
            active_root.clone(),
            vec![active_source],
            pending_paths(),
        );
        active_held_rx
            .recv_timeout(Duration::from_secs(12))
            .expect("active refresh worker holds the queue");

        fs::write(
            &target_source,
            "fn entry() { build_leaf(); }\nfn build_leaf() {}\nfn worker_leaf() {}\n",
        )
        .unwrap();
        enqueue_callgraph_store_refresh(
            target_dir.clone(),
            target_root.clone(),
            vec![target_source.clone()],
            pending_paths(),
        );
        let (new_generation, _) = CallGraphStore::cold_build_with_lease(
            target_dir.clone(),
            target_root.clone(),
            std::slice::from_ref(&target_source),
        )
        .unwrap();
        fs::write(
            &target_source,
            "fn entry() { worker_leaf(); }\nfn build_leaf() {}\nfn worker_leaf() {}\n",
        )
        .unwrap();
        drop(new_generation);

        active_release_tx
            .send(())
            .expect("release active refresh worker");
        wait_for_refresh_calls(&target_root, 1);
        assert!(flush_callgraph_store_refreshes_with_budget(
            Duration::from_secs(12)
        ));
        let current = CallGraphStore::open_readonly(target_dir, target_root.clone())
            .unwrap()
            .expect("current callgraph generation");
        let tree = current.call_tree(Path::new("main.rs"), "entry", 1).unwrap();
        assert_eq!(tree.children[0].name, "worker_leaf");
        assert_eq!(callgraph_refresh_worker_test_counts(&target_root).0, 1);
        clear_callgraph_refresh_worker_test_seam(&active_root);
        clear_callgraph_refresh_worker_test_seam(&target_root);
    }

    #[test]
    fn refresh_failure_marks_files_stale() {
        let _guard = REFRESH_WORKER_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // Generous pre-drain: the refresh worker is process-wide, so a prior
        // test's still-running batch (slow Windows CI) must fully settle
        // before this test enqueues, or its wait deadline absorbs the
        // leftover work. Idle workers return immediately.
        let _ = flush_callgraph_store_refreshes_with_budget(Duration::from_secs(30));
        let (_temp, root, callgraph_dir, source) = ready_store_fixture();
        let pending = pending_paths();
        set_callgraph_refresh_worker_test_seam(root.clone(), Duration::ZERO, true);

        enqueue_callgraph_store_refresh(callgraph_dir.clone(), root.clone(), vec![source], pending);
        assert!(flush_callgraph_store_refreshes_with_budget(
            Duration::from_secs(2)
        ));

        assert_eq!(callgraph_refresh_worker_test_counts(&root), (1, 1));
        let store = CallGraphStore::open_ready(callgraph_dir, root.clone())
            .unwrap()
            .expect("ready callgraph store");
        assert_eq!(store.stale_files().unwrap(), vec!["main.rs"]);
        clear_callgraph_refresh_worker_test_seam(&root);
    }

    #[test]
    fn idle_refresh_truncates_wal() {
        let _guard = REFRESH_WORKER_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let _ = flush_callgraph_store_refreshes_with_budget(Duration::from_secs(30));
        let (_temp, root, callgraph_dir, source) = ready_store_fixture();
        let generation = read_pointer(
            &callgraph_dir,
            &crate::search_index::artifact_cache_key(&root),
        )
        .expect("fixture publishes a generation");
        let wal_path = callgraph_dir.join(format!("{generation}-wal"));
        let pending = pending_paths();
        set_callgraph_refresh_worker_test_seam(root.clone(), Duration::ZERO, false);

        fs::write(&source, "fn entry() { old_leaf(); }\nfn old_leaf() {}\n\n").unwrap();
        enqueue_callgraph_store_refresh(
            callgraph_dir.clone(),
            root.clone(),
            vec![source.clone()],
            Arc::clone(&pending),
        );
        wait_for_refresh_calls(&root, 1);
        wait_for_refresh_worker_idle();
        let checkpoint_deadline = Instant::now() + Duration::from_secs(2);
        while fs::metadata(&wal_path)
            .map(|metadata| metadata.len())
            .unwrap_or(0)
            != 0
        {
            assert!(
                Instant::now() < checkpoint_deadline,
                "idle checkpoint did not truncate WAL"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(
            fs::metadata(&wal_path)
                .map(|metadata| metadata.len())
                .unwrap_or(0),
            0,
            "idle transition truncates the refresh WAL"
        );

        clear_callgraph_refresh_worker_test_seam(&root);
    }

    #[test]
    fn bounded_shutdown_defers_unprocessed_batches() {
        let _guard = REFRESH_WORKER_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // Generous pre-drain: the refresh worker is process-wide, so a prior
        // test's still-running batch (slow Windows CI) must fully settle
        // before this test enqueues, or its wait deadline absorbs the
        // leftover work. Idle workers return immediately.
        let _ = flush_callgraph_store_refreshes_with_budget(Duration::from_secs(30));
        let (_active_temp, active_root, active_dir, active_source) = ready_store_fixture();
        let (_queued_temp, queued_root, queued_dir, queued_source) = ready_store_fixture();
        let active_pending = pending_paths();
        let queued_pending = pending_paths();
        set_callgraph_refresh_worker_test_seam(
            active_root.clone(),
            Duration::from_millis(300),
            false,
        );

        enqueue_callgraph_store_refresh(
            active_dir,
            active_root.clone(),
            vec![active_source.clone()],
            Arc::clone(&active_pending),
        );
        wait_for_refresh_calls(&active_root, 1);
        enqueue_callgraph_store_refresh(
            queued_dir,
            queued_root.clone(),
            vec![queued_source.clone()],
            Arc::clone(&queued_pending),
        );

        assert!(!flush_callgraph_store_refreshes_with_budget(
            Duration::from_millis(20)
        ));
        assert!(active_pending.lock().contains(&active_source));
        assert!(queued_pending.lock().contains(&queued_source));
        assert_eq!(callgraph_refresh_worker_test_counts(&queued_root).0, 0);
        clear_callgraph_refresh_worker_test_seam(&active_root);
    }
}

#[cfg(test)]
mod cold_build_insert_tests {
    use super::*;
    use crate::imports::ImportBlock;
    use std::cell::Cell;
    use std::fs;
    use std::path::{Path, PathBuf};
    use tempfile::tempdir;

    thread_local! {
        static CALLER_QUERY_SELECTS: Cell<usize> = const { Cell::new(0) };
        static BOUNDARY_COUNT_SELECTS: Cell<usize> = const { Cell::new(0) };
        static TOTAL_CALLER_TRAVERSAL_SELECTS: Cell<usize> = const { Cell::new(0) };
    }

    fn count_caller_traversal_selects(sql: &str) {
        let sql = sql.trim_start();
        if sql.starts_with("SELECT") || sql.starts_with("WITH requested") {
            TOTAL_CALLER_TRAVERSAL_SELECTS.with(|count| count.set(count.get() + 1));
        }
        if sql.contains("SELECT e.target_file, e.target_symbol, e.line")
            && sql.contains("e.target_file =")
        {
            CALLER_QUERY_SELECTS.with(|count| count.set(count.get() + 1));
        }
        if sql.starts_with("WITH requested") {
            BOUNDARY_COUNT_SELECTS.with(|count| count.set(count.get() + 1));
        }
    }

    #[test]
    fn nonrepairing_open_policy_leaves_moved_root_metadata_for_maintenance() {
        let dir = tempdir().unwrap();
        let previous_root = dir.path().join("previous-root");
        let current_root = dir.path().join("current-root");
        fs::create_dir_all(&previous_root).unwrap();
        fs::create_dir_all(&current_root).unwrap();
        fs::remove_dir(&previous_root).unwrap();
        let mut conn = Connection::open_in_memory().unwrap();
        initialize_schema(&conn).unwrap();
        conn.execute(
            "INSERT INTO backend_file_state(
                backend, workspace_root, file_path, content_hash, status, updated_at
             ) VALUES ('rust', ?1, 'src/main.rs', 'hash', 'ready', 1)",
            params![previous_root.display().to_string()],
        )
        .unwrap();

        let repair = reconcile_workspace_roots(&mut conn, &current_root, false).unwrap();

        assert!(matches!(repair, OpenRootRepair::NeedsRebuild { .. }));
        assert_eq!(
            stored_workspace_roots(&conn).unwrap(),
            vec![previous_root.display().to_string()]
        );
    }

    #[test]
    fn sqlite_readonly_uri_percent_encodes_windows_paths() {
        assert_eq!(
            sqlite_readonly_uri(Path::new(r"C:\Users\name with spaces\db#1.sqlite")),
            "file:///C:/Users/name%20with%20spaces/db%231.sqlite?mode=ro"
        );
    }

    #[test]
    fn legacy_migration_completion_log_has_operator_fields() {
        assert_eq!(
            legacy_migration_completion_line("abc123", "generation_copy", 176, 177),
            "migrated root-keyed callgraph store key=abc123 method=generation_copy legacy=176 migrated=177"
        );
    }

    fn write_generation_with_age(
        dir: &Path,
        project_key: &str,
        ordinal: u64,
        age: Duration,
    ) -> String {
        let generation = format!("{project_key}.g{ordinal}.1.sqlite");
        let path = dir.join(&generation);
        fs::write(&path, b"sqlite placeholder").unwrap();
        let mtime = SystemTime::now().checked_sub(age).unwrap_or(UNIX_EPOCH);
        filetime::set_file_mtime(&path, filetime::FileTime::from_system_time(mtime)).unwrap();
        generation
    }

    #[test]
    fn gc_old_generations_preserves_live_reader_until_marker_drops() {
        let dir = tempfile::tempdir().unwrap();
        let project_key = "project";
        let current = write_generation_with_age(dir.path(), project_key, 400, Duration::ZERO);
        let previous =
            write_generation_with_age(dir.path(), project_key, 300, Duration::from_secs(1));
        let pinned =
            write_generation_with_age(dir.path(), project_key, 200, Duration::from_secs(2));
        let marker = crate::root_cache::ReadMarker::create(dir.path(), &pinned).unwrap();

        gc_old_generations(dir.path(), project_key, &current);

        assert!(dir.path().join(&previous).is_file());
        assert!(dir.path().join(&pinned).is_file());

        drop(marker);
        gc_old_generations(dir.path(), project_key, &current);

        assert!(dir.path().join(&previous).is_file());
        assert!(!dir.path().join(&pinned).exists());
    }

    #[test]
    fn gc_old_generations_ignores_same_host_marker_mtime_for_live_pid() {
        let dir = tempfile::tempdir().unwrap();
        let project_key = "project";
        let current = write_generation_with_age(dir.path(), project_key, 400, Duration::ZERO);
        let _previous =
            write_generation_with_age(dir.path(), project_key, 300, Duration::from_secs(1));
        let pinned =
            write_generation_with_age(dir.path(), project_key, 200, Duration::from_secs(2));
        let marker = crate::root_cache::ReadMarker::create(dir.path(), &pinned).unwrap();
        filetime::set_file_mtime(marker.path(), filetime::FileTime::from_unix_time(0, 0)).unwrap();

        gc_old_generations(dir.path(), project_key, &current);

        assert!(dir.path().join(&pinned).is_file());
    }

    #[test]
    fn gc_old_generations_applies_retention_ttl_to_marked_old_generations() {
        let dir = tempfile::tempdir().unwrap();
        let project_key = "project";
        let expired = MARKED_GENERATION_RETENTION_TTL + Duration::from_secs(60);
        let current = write_generation_with_age(dir.path(), project_key, 400, Duration::ZERO);
        let previous = write_generation_with_age(dir.path(), project_key, 300, expired);
        let old = write_generation_with_age(
            dir.path(),
            project_key,
            200,
            expired + Duration::from_secs(60),
        );
        let _marker = crate::root_cache::ReadMarker::create(dir.path(), &old).unwrap();

        gc_old_generations(dir.path(), project_key, &current);

        assert!(dir.path().join(&current).is_file());
        assert!(dir.path().join(&previous).is_file());
        assert!(!dir.path().join(&old).exists());
    }

    fn write_build_temp_with_age(dir: &Path, name: &str, age: Duration) -> PathBuf {
        let path = dir.join(name);
        fs::write(&path, b"temp placeholder").unwrap();
        let mtime = SystemTime::now().checked_sub(age).unwrap_or(UNIX_EPOCH);
        filetime::set_file_mtime(&path, filetime::FileTime::from_system_time(mtime)).unwrap();
        path
    }

    #[test]
    fn orphan_temp_sweep_removes_aged_orphan_and_journal_but_spares_fresh() {
        let dir = tempdir().unwrap();
        // One directory holds both an aged orphan (with its journal sidecar) and a
        // fresh temporary, so this proves the sweep SELECTS by age rather than
        // deleting everything in the directory.
        let aged = "project.g100.1.sqlite.tmp.1.200";
        let aged_journal = "project.g100.1.sqlite.tmp.1.200-journal";
        let fresh = "project.g300.1.sqlite.tmp.1.400";
        let aged_age = ORPHANED_BUILD_TEMP_MIN_AGE + Duration::from_secs(60);
        write_build_temp_with_age(dir.path(), aged, aged_age);
        write_build_temp_with_age(dir.path(), aged_journal, aged_age);
        write_build_temp_with_age(dir.path(), fresh, Duration::ZERO);

        sweep_orphaned_build_temps(dir.path());

        assert!(
            !dir.path().join(aged).exists(),
            "aged orphan must be removed"
        );
        assert!(
            !dir.path().join(aged_journal).exists(),
            "aged journal sidecar must be removed"
        );
        assert!(
            dir.path().join(fresh).is_file(),
            "fresh temporary must survive"
        );
    }

    #[test]
    fn orphan_temp_sweep_reaches_legacy_store_for_root_with_no_pointer_or_build() {
        let storage = tempdir().unwrap();
        let storage_root = storage.path();
        // The production shape: a legacy per-harness store whose root no longer
        // builds there — no `.current` pointer, no running build — so the per-root
        // cleanup never fires for it. A sibling root still building in the
        // root-keyed store triggers the store-wide sweep, which must reach into the
        // legacy directory and reclaim the orphan.
        let legacy_dir = storage_root.join("opencode").join("callgraph");
        fs::create_dir_all(&legacy_dir).unwrap();
        let orphan = "deadbeef.g100.1.sqlite.tmp.1.200";
        write_build_temp_with_age(
            &legacy_dir,
            orphan,
            ORPHANED_BUILD_TEMP_MIN_AGE + Duration::from_secs(60),
        );
        assert!(
            !legacy_dir.join("deadbeef.current").exists(),
            "the dead root has no current pointer"
        );

        let root_keyed_dir = storage_root.join("callgraph").join("livekey");
        fs::create_dir_all(&root_keyed_dir).unwrap();

        sweep_orphaned_build_temps_store_wide(&root_keyed_dir);

        assert!(
            !legacy_dir.join(orphan).exists(),
            "legacy orphan must be reclaimed by the store-wide sweep"
        );
    }

    #[test]
    fn orphan_temp_sweep_negative_control_age_predicate_is_what_spares_fresh() {
        // NEGATIVE CONTROL, mutation-proved: forcing the age predicate to accept
        // everything (min_age = 0) removes the fresh temporary that the real 24h
        // threshold spares in the test above. If a mutation to the age check leaves
        // the fresh file in place here, the predicate is no longer doing the
        // selection work the fresh-survives assertion relies on.
        let dir = tempdir().unwrap();
        let fresh = "project.g300.1.sqlite.tmp.1.400";
        write_build_temp_with_age(dir.path(), fresh, Duration::ZERO);

        sweep_orphaned_build_temps_older_than(dir.path(), Duration::ZERO);

        assert!(
            !dir.path().join(fresh).exists(),
            "with the age predicate forced open, the fresh temporary is removed"
        );
    }

    #[test]
    fn orphan_temp_sweep_leaves_completed_generation_and_read_marker_alone() {
        let dir = tempdir().unwrap();
        // A completed generation (its name has no `.sqlite.tmp.`) that is old enough
        // to be swept, plus a live read marker, is generation GC's jurisdiction.
        // The orphan sweep must not intersect it.
        let generation = write_generation_with_age(
            dir.path(),
            "project",
            400,
            ORPHANED_BUILD_TEMP_MIN_AGE + Duration::from_secs(60),
        );
        let _marker = crate::root_cache::ReadMarker::create(dir.path(), &generation).unwrap();

        sweep_orphaned_build_temps(dir.path());

        assert!(
            dir.path().join(&generation).is_file(),
            "completed generation must survive the orphan sweep"
        );
        assert!(
            crate::root_cache::read_marker_dir(dir.path(), &generation).exists(),
            "read marker must survive the orphan sweep"
        );
    }

    #[test]
    fn atomic_swap_checkpoint_uses_passive_when_live_marker_exists() {
        let dir = tempfile::tempdir().unwrap();
        let project_key = "project".to_string();
        let generation = write_generation_with_age(dir.path(), &project_key, 100, Duration::ZERO);
        let sqlite_path = dir.path().join(&generation);
        fs::remove_file(&sqlite_path).unwrap();
        let conn = Connection::open(&sqlite_path).unwrap();
        let store = CallGraphStore::from_connection(
            dir.path().to_path_buf(),
            project_key,
            sqlite_path,
            dir.path().to_path_buf(),
            false,
            Some(generation.clone()),
            None,
            None,
            conn,
        );

        let marker = crate::root_cache::ReadMarker::create(dir.path(), &generation).unwrap();
        assert!(store.atomic_swap_checkpoint_sql().contains("PASSIVE"));

        drop(marker);
        assert!(store.atomic_swap_checkpoint_sql().contains("TRUNCATE"));
    }

    #[test]
    fn readiness_cache_only_skips_checks_after_a_successful_validation() {
        let dir = tempdir().expect("temp dir");
        let file = dir.path().join("main.ts");
        fs::write(&file, "export function main() {}\n").expect("write fixture");
        let store = CallGraphStore::open(
            dir.path().join(".store-readiness-cache"),
            dir.path().to_path_buf(),
        )
        .expect("open store");
        {
            let mut conn = store.conn.lock().expect("callgraph store mutex poisoned");
            conn.trace(Some(count_caller_traversal_selects));
        }

        TOTAL_CALLER_TRAVERSAL_SELECTS.with(|count| count.set(0));
        assert!(store.indexed_file_count().is_err());
        assert!(store.indexed_file_count().is_err());
        assert_eq!(TOTAL_CALLER_TRAVERSAL_SELECTS.with(Cell::get), 6);

        store
            .cold_build(std::slice::from_ref(&file))
            .expect("cold build");
        TOTAL_CALLER_TRAVERSAL_SELECTS.with(|count| count.set(0));
        assert_eq!(store.indexed_file_count().expect("first ready read"), 1);
        assert_eq!(store.indexed_file_count().expect("cached ready read"), 1);
        assert_eq!(TOTAL_CALLER_TRAVERSAL_SELECTS.with(Cell::get), 5);

        let mut conn = store.conn.lock().expect("callgraph store mutex poisoned");
        conn.trace(None);
    }

    #[test]
    fn callers_depth_boundary_batches_sqlite_counts() {
        const CALLER_COUNT: usize = 1_000;

        let dir = tempdir().expect("temp dir");
        let file = dir.path().join("main.ts");
        let mut source = String::from("export function sharedHotHelper() {}\n");
        for index in 0..CALLER_COUNT {
            source.push_str(&format!(
                "export function caller{index}() {{ sharedHotHelper(); }}\n"
            ));
        }
        fs::write(&file, source).expect("write fixture");

        let store = CallGraphStore::open(
            dir.path().join(".store-callers-query-fanout"),
            dir.path().to_path_buf(),
        )
        .expect("open store");
        store
            .cold_build(std::slice::from_ref(&file))
            .expect("cold build");

        CALLER_QUERY_SELECTS.with(|count| count.set(0));
        BOUNDARY_COUNT_SELECTS.with(|count| count.set(0));
        TOTAL_CALLER_TRAVERSAL_SELECTS.with(|count| count.set(0));
        {
            let mut conn = store.conn.lock().expect("callgraph store mutex poisoned");
            conn.trace(Some(count_caller_traversal_selects));
        }

        let started = Instant::now();
        let result = crate::commands::callgraph_store_adapter::callers_result(
            &store,
            Path::new("main.ts"),
            "sharedHotHelper",
            1,
            true,
        )
        .expect("callers result");
        let elapsed = started.elapsed();

        {
            let mut conn = store.conn.lock().expect("callgraph store mutex poisoned");
            conn.trace(None);
        }
        let caller_queries = CALLER_QUERY_SELECTS.with(Cell::get);
        let boundary_queries = BOUNDARY_COUNT_SELECTS.with(Cell::get);
        let total_selects = TOTAL_CALLER_TRAVERSAL_SELECTS.with(Cell::get);
        eprintln!(
            "SQLITE_CALLERS_AFTER callers={} caller_queries={} boundary_queries={} total_selects={} elapsed_ms={:.3}",
            result.total_callers,
            caller_queries,
            boundary_queries,
            total_selects,
            elapsed.as_secs_f64() * 1_000.0
        );

        assert_eq!(result.total_callers, CALLER_COUNT);
        assert_eq!(caller_queries, 1);
        assert_eq!(boundary_queries, 3);
        assert_eq!(total_selects, 9);
    }

    #[test]
    fn depth_boundary_counts_match_full_fetch_lengths_with_dangling_edges() {
        let dir = tempdir().expect("temp dir");
        let file = dir.path().join("main.ts");
        fs::write(
            &file,
            r#"export function topA() {
  root();
}

export function topB() {
  root();
}

export function root() {
  leaf();
  missing();
}

export function leaf() {}
"#,
        )
        .expect("write fixture");

        let store = CallGraphStore::open(
            dir.path().join(".store-depth-boundary-counts"),
            dir.path().to_path_buf(),
        )
        .expect("open store");
        store
            .cold_build(std::slice::from_ref(&file))
            .expect("cold build");

        let root = store
            .node_for(Path::new("main.ts"), "root")
            .expect("root node");
        let leaf = store
            .node_for(Path::new("main.ts"), "leaf")
            .expect("leaf node");

        let (full_forward_len, full_direct_len) = {
            let conn = store.conn.lock().expect("callgraph store mutex poisoned");
            conn.execute(
                "INSERT INTO edges (
                    edge_id, ref_id, source_node, target_node, target_file,
                    target_symbol, kind, line, provenance
                 ) VALUES (
                    'dangling-forward-boundary', 'missing-forward-ref', ?1, NULL,
                    ?2, ?3, 'call', 98, ?4
                 )",
                rusqlite::params![
                    &root.node_id,
                    &leaf.file,
                    &leaf.symbol,
                    PROVENANCE_TREESITTER
                ],
            )
            .expect("insert dangling forward edge");
            conn.execute(
                "INSERT INTO edges (
                    edge_id, ref_id, source_node, target_node, target_file,
                    target_symbol, kind, line, provenance
                 ) VALUES (
                    'dangling-direct-boundary', 'missing-direct-ref', 'missing-source-node',
                    ?1, ?2, ?3, 'call', 99, ?4
                 )",
                rusqlite::params![
                    &root.node_id,
                    &root.file,
                    &root.symbol,
                    PROVENANCE_TREESITTER
                ],
            )
            .expect("insert dangling direct-caller edge");

            let full_forward_len = forward_calls_for_node(&conn, &root)
                .expect("full forward calls")
                .len();
            let counted_forward_len =
                forward_call_count_for_node(&conn, &root).expect("counted forward calls");
            assert_eq!(
                counted_forward_len, full_forward_len,
                "forward boundary COUNT must mirror outgoing_calls_for_node + unresolved_calls_for_node"
            );

            let full_direct = direct_callers_for_tuple(&conn, &root.file, &root.symbol)
                .expect("full direct callers");
            let full_direct_len = full_direct.len();
            let counted_direct_len = direct_caller_count_for_tuple(&conn, &root.file, &root.symbol)
                .expect("counted direct callers");
            assert_eq!(
                counted_direct_len, full_direct_len,
                "direct-caller boundary COUNT must mirror direct_callers_for_tuple"
            );

            let distinct_direct_len = full_direct
                .iter()
                .map(|site| {
                    (
                        site.caller.file.clone(),
                        site.line,
                        site.target_file.clone(),
                        site.target_symbol.clone(),
                    )
                })
                .collect::<BTreeSet<_>>()
                .len();
            let batch_counts = direct_caller_counts_for_tuples(
                &conn,
                &[
                    (root.file.clone(), root.symbol.clone()),
                    (root.file.clone(), root.symbol.clone()),
                    (leaf.file.clone(), leaf.symbol.clone()),
                ],
            )
            .expect("batched direct-caller counts");
            assert_eq!(batch_counts.len(), 2);
            assert_eq!(
                batch_counts.get(&(root.file.clone(), root.symbol.clone())),
                Some(&distinct_direct_len)
            );

            (full_forward_len, full_direct_len)
        };

        assert_eq!(
            full_forward_len, 2,
            "fixture root should have one resolved and one unresolved outgoing call"
        );
        assert_eq!(
            full_direct_len, 2,
            "fixture root should have two real direct callers"
        );

        let tree = store
            .call_tree(Path::new("main.ts"), "root", 0)
            .expect("call tree");
        assert!(tree.depth_limited);
        assert_eq!(tree.children.len(), 0);
        assert_eq!(
            tree.truncated, full_forward_len,
            "call_tree depth boundary must report the full forward-call list length"
        );

        let callers = store
            .callers_of(Path::new("main.ts"), "leaf", 0)
            .expect("callers");
        assert!(callers.depth_limited);
        assert_eq!(callers.callers.len(), 1);
        assert_eq!(callers.callers[0].caller.symbol, "root");
        assert_eq!(
            callers.truncated, full_direct_len,
            "callers depth boundary must report the full direct-caller list length"
        );
    }

    #[test]
    fn source_freshness_matches_cache_collect_for_same_bytes() {
        let dir = tempdir().expect("temp dir");
        let path = dir.path().join("fixture.ts");
        let source = "export function main() { return helper(); }\n";
        fs::write(&path, source).expect("write fixture");

        let expected = cache_freshness::collect(&path).expect("collect freshness from file");
        let actual =
            collect_source_freshness(&path, source).expect("collect freshness from source");

        assert_eq!(actual, expected);
    }

    #[test]
    fn superseded_cold_build_cannot_publish_after_newer_epoch() {
        let root = tempfile::tempdir().unwrap();
        let callgraph_dir = tempfile::tempdir().unwrap();
        let source_dir = root.path().join("src");
        std::fs::create_dir_all(&source_dir).unwrap();
        let source = source_dir.join("lib.rs");
        std::fs::write(&source, "pub fn old_generation_marker() {}\n").unwrap();
        let files = vec![source.clone()];
        let epoch = crate::root_cache::ArtifactPublishEpoch::default();
        let old_epoch = epoch.next();
        let (reached_tx, reached_rx) = crossbeam_channel::bounded(1);
        let (release_tx, release_rx) = crossbeam_channel::bounded(1);
        let old_epoch_flag = epoch.clone();
        let old_dir = callgraph_dir.path().to_path_buf();
        let old_root = root.path().to_path_buf();
        let old_files = files.clone();
        let old = std::thread::spawn(move || {
            set_cold_build_before_publish_observer(Some(Arc::new(move || {
                reached_tx.send(()).unwrap();
                release_rx.recv().unwrap();
            })));
            let result = with_publish_epoch(old_epoch_flag, old_epoch, || {
                CallGraphStore::cold_build_with_lease(old_dir, old_root, &old_files)
            });
            set_cold_build_before_publish_observer(None);
            result
        });
        // Positive wait: the older build runs a real cold build (git probe +
        // SQLite schema init) before the barrier, which can exceed 5s on a
        // contended Windows CI runner. Only negative waits stay short.
        reached_rx
            .recv_timeout(Duration::from_secs(30))
            .expect("older build did not reach its publication barrier");

        std::fs::write(&source, "pub fn new_generation_marker() {}\n").unwrap();
        let new_epoch = epoch.next();
        let new_store = with_publish_epoch(epoch.clone(), new_epoch, || {
            CallGraphStore::cold_build_with_lease(
                callgraph_dir.path().to_path_buf(),
                root.path().to_path_buf(),
                &files,
            )
        })
        .expect("newer build should publish");
        drop(new_store);

        release_tx.send(()).unwrap();
        assert!(matches!(
            old.join().unwrap(),
            Err(CallGraphStoreError::Superseded)
        ));

        let current = CallGraphStore::open_readonly(
            callgraph_dir.path().to_path_buf(),
            root.path().to_path_buf(),
        )
        .unwrap()
        .expect("current callgraph generation");
        assert_eq!(
            current
                .nodes_matching("new_generation_marker")
                .unwrap()
                .len(),
            1
        );
        assert!(current
            .nodes_matching("old_generation_marker")
            .unwrap()
            .is_empty());
    }

    #[test]
    fn cold_build_prepared_bulk_insert_matches_reference_rows() {
        let dir = tempdir().expect("temp dir");
        let project_root = dir.path();
        let extract = fixture_extract(project_root);
        let resolved = fixture_resolved(&extract);

        let reference = build_reference_connection(project_root, &extract, &resolved);
        let optimized = build_optimized_connection(project_root, &extract, &resolved);

        for table in [
            "files",
            "nodes",
            "file_dependencies",
            "dispatch_hints",
            "refs",
            "edges",
        ] {
            // `files.indexed_at` is a wall-clock insert timestamp (unix_seconds_now);
            // the reference and optimized builds run sequentially and can straddle a
            // one-second tick under load, so it is legitimately allowed to differ.
            // This mirrors the existing exclusions of `backend_file_state.updated_at`
            // and the chunked-vs-unchunked sibling test. The check is for structural
            // row equivalence of the optimized bulk insert, not wall-clock equality.
            let excluded: &[&str] = if table == "files" {
                &["indexed_at"]
            } else {
                &[]
            };
            assert_eq!(
                table_rows_without(&reference, table, excluded),
                table_rows_without(&optimized, table, excluded),
                "table `{table}` rows must match apart from wall-clock columns"
            );
        }
        assert_eq!(
            backend_state_rows(&reference),
            backend_state_rows(&optimized),
            "backend freshness rows must match apart from updated_at"
        );
        assert_eq!(secondary_indexes(&reference), secondary_indexes(&optimized));
    }

    #[test]
    fn cold_build_chunked_matches_unchunked_logical_rows() {
        let dir = tempdir().expect("temp dir");
        let project_root = fs::canonicalize(dir.path()).expect("canonical temp root");
        write_chunked_equivalence_fixture(&project_root);
        let files = callgraph::walk_project_files(&project_root).collect::<Vec<_>>();
        assert!(
            files.len() > 6,
            "fixture should be large enough to split into multiple chunks"
        );

        let unchunked = CallGraphStore::open(
            project_root.join(".store-unchunked"),
            project_root.to_path_buf(),
        )
        .expect("open unchunked store");
        let unchunked_stats = unchunked
            .cold_build_chunked(&files, 0)
            .expect("unchunked cold build");

        let chunked = CallGraphStore::open(
            project_root.join(".store-chunked"),
            project_root.to_path_buf(),
        )
        .expect("open chunked store");
        let chunked_stats = chunked
            .cold_build_chunked(&files, 3)
            .expect("chunked cold build");

        assert_cold_build_stats_match_except_elapsed(&unchunked_stats, &chunked_stats);
        assert_eq!(
            unchunked.edge_snapshot().expect("unchunked edge snapshot"),
            chunked.edge_snapshot().expect("chunked edge snapshot"),
            "public edge snapshots must match"
        );

        let dispatch_edges = {
            let conn = chunked.conn.lock().expect("callgraph store mutex poisoned");
            conn.query_row(
                "SELECT COUNT(*) FROM edges WHERE provenance IN ('name_match', 'type_match')",
                [],
                |row| row.get::<_, i64>(0),
            )
            .expect("count dispatch edges")
        };
        assert!(
            dispatch_edges > 0,
            "fixture must exercise method-dispatch edge insertion"
        );

        for table in [
            "edges",
            "refs",
            "nodes",
            "file_dependencies",
            "dispatch_hints",
        ] {
            assert_eq!(
                graph_table_rows(&unchunked, table),
                graph_table_rows(&chunked, table),
                "chunked cold build must match unchunked rows for {table}"
            );
        }
        assert_eq!(
            graph_table_rows_without(&unchunked, "files", &["indexed_at"]),
            graph_table_rows_without(&chunked, "files", &["indexed_at"]),
            "files rows must match apart from indexed_at"
        );
        assert_eq!(
            graph_table_rows_without(&unchunked, "backend_file_state", &["updated_at"]),
            graph_table_rows_without(&chunked, "backend_file_state", &["updated_at"]),
            "backend freshness rows must match apart from updated_at"
        );

        let published_dir = project_root.join(".store-published");
        let (_published, _stats) = CallGraphStore::cold_build_with_lease_chunked(
            published_dir.clone(),
            project_root.to_path_buf(),
            &files,
            0,
        )
        .expect("published unchunked cold build");
        assert!(
            !CallGraphStore::needs_cold_build(&published_dir, &project_root)
                .expect("needs_cold_build after publish"),
            "published store should be ready"
        );
        drop(_published);
        let (_opened, rebuild_stats) = CallGraphStore::ensure_built_with_lease_chunked(
            published_dir,
            project_root.to_path_buf(),
            &files,
            3,
        )
        .expect("ensure with a different chunk size");
        assert!(
            rebuild_stats.is_none(),
            "changing callgraph_chunk_size must not affect store identity or force a rebuild"
        );
    }

    // Perf A/B bench (not a gate): measures cold_build wall time at a given
    // chunk size against a real repo. Driven by env so the same binary can A/B
    // chunk=0 vs chunk=N in clean isolation. Reusable for the deferred DB-spill
    // memory work. Run:
    //   AFT_PERF_REPO=/path AFT_PERF_CHUNK=0 cargo test -p agent-file-tools \
    //     --release --lib bench_cold_build_chunk -- --ignored --nocapture
    #[test]
    #[ignore]
    fn bench_cold_build_chunk() {
        let repo = std::env::var("AFT_PERF_REPO").expect("AFT_PERF_REPO");
        let chunk: usize = std::env::var("AFT_PERF_CHUNK")
            .expect("AFT_PERF_CHUNK")
            .parse()
            .expect("AFT_PERF_CHUNK must be a non-negative integer");
        let project_root = fs::canonicalize(&repo).expect("canonical repo root");
        let files = callgraph::walk_project_files(&project_root).collect::<Vec<_>>();
        let dir = tempdir().expect("temp dir");
        let store = CallGraphStore::open(dir.path().join(".store"), project_root.clone())
            .expect("open store");
        let started = Instant::now();
        let stats = store.cold_build_chunked(&files, chunk).expect("cold build");
        let ms = started.elapsed().as_millis();
        println!(
            "BENCH_COLD_BUILD chunk={chunk} files={} nodes={} refs={} edges={} ms={ms}",
            stats.files, stats.nodes, stats.refs, stats.edges
        );
    }

    #[test]
    fn persisted_workspace_reexport_selects_its_package_dependency() {
        let root = tempdir().expect("temp dir");
        let dependencies = BTreeSet::from([
            "packages/aft-bridge/src/index.ts".to_string(),
            "packages/opencode-plugin/src/types.ts".to_string(),
        ]);
        let indexed_files = dependencies.iter().cloned().collect::<HashSet<_>>();

        assert_eq!(
            stored_dependencies_for_module(
                root.path(),
                "packages/opencode-plugin/src/shared/bash-hints.ts",
                "@cortexkit/aft-bridge",
                &dependencies,
                &indexed_files,
            ),
            BTreeSet::from(["packages/aft-bridge/src/index.ts".to_string()])
        );
    }

    #[test]
    fn incremental_barrel_refresh_matches_per_ref_lookup_and_cold_rebuild() {
        let dir = tempdir().expect("temp dir");
        let project_root = dir.path();
        let files =
            write_barrel_refresh_fixture(project_root, "export { target } from \"./target\";\n");
        let index_path = project_root.join("src/index.ts");

        let store = CallGraphStore::open(
            project_root.join(".store-incremental-barrel"),
            project_root.to_path_buf(),
        )
        .expect("open incremental store");
        store.cold_build(&files).expect("initial cold build");

        {
            let mut conn = store.conn.lock().expect("callgraph store mutex poisoned");
            let tx = conn.transaction().expect("dependency transaction");
            let dependent_refs = ref_ids_depending_on(&tx, project_root, "src/index.ts")
                .expect("dependent refs for barrel");
            let selected_ref_ids = dependent_refs
                .iter()
                .map(|dependent_ref| dependent_ref.ref_id.clone())
                .collect::<BTreeSet<_>>();
            let mut threaded_ref_ids = BTreeSet::new();
            let mut threaded_by_caller = BTreeMap::new();
            record_dependent_refs(
                &mut threaded_ref_ids,
                &mut threaded_by_caller,
                dependent_refs,
            );
            let old_by_caller = refs_by_caller_for_ref_ids(&tx, &selected_ref_ids)
                .expect("old per-ref caller lookup");

            assert_eq!(threaded_ref_ids, selected_ref_ids);
            assert_eq!(threaded_by_caller, old_by_caller);
            for consumer in [
                "src/consumer_a.ts",
                "src/consumer_b.ts",
                "src/consumer_c.ts",
            ] {
                assert!(
                    threaded_by_caller.contains_key(consumer),
                    "barrel edit should select dependent refs from {consumer}"
                );
            }
        }

        fs::write(
            &index_path,
            "export { target } from \"./target\";\nexport function extra() { return 1; }\n",
        )
        .expect("edit barrel");
        let stats = store
            .refresh_files(std::slice::from_ref(&index_path))
            .expect("incremental refresh");
        assert_eq!(stats.surface_changed, vec!["src/index.ts".to_string()]);
        assert!(
            stats.dependency_selected_refs > 0,
            "barrel surface edit should select dependent refs"
        );

        let cold_store = CallGraphStore::open(
            project_root.join(".store-cold-barrel"),
            project_root.to_path_buf(),
        )
        .expect("open cold rebuild store");
        cold_store
            .cold_build(&files)
            .expect("comparison cold build");

        for table in [
            "nodes",
            "refs",
            "file_dependencies",
            "edges",
            "dispatch_hints",
        ] {
            assert_eq!(
                graph_table_rows(&store, table),
                graph_table_rows(&cold_store, table),
                "incremental refresh {table} rows must match cold rebuild"
            );
        }

        let consumer_path = project_root.join("src/consumer_a.ts");
        fs::write(
            &consumer_path,
            "import { target } from \"./index\";\nexport function consumerA() { return target(); }\nexport const refreshed = true;\n",
        )
        .expect("edit barrel consumer");
        store
            .refresh_files(std::slice::from_ref(&consumer_path))
            .expect("refresh consumer through unchanged barrel");
        cold_store
            .cold_build(&files)
            .expect("comparison cold rebuild after consumer refresh");
        for table in [
            "nodes",
            "refs",
            "file_dependencies",
            "edges",
            "dispatch_hints",
        ] {
            assert_eq!(
                graph_table_rows(&store, table),
                graph_table_rows(&cold_store, table),
                "refresh through a persisted barrel must preserve cold-build {table} rows"
            );
        }
    }

    fn build_reference_connection(
        project_root: &Path,
        extract: &FileExtract,
        resolved: &ResolvedRef,
    ) -> Connection {
        let mut conn = Connection::open_in_memory().expect("open reference db");
        configure_build_connection(&conn).expect("configure reference db");
        initialize_schema(&conn).expect("initialize reference schema");
        {
            let tx = conn.transaction().expect("reference transaction");
            clear_tables(&tx).expect("reference clear");
            insert_meta(&tx).expect("reference meta");
            insert_file_extract(&tx, project_root, extract).expect("reference file extract");
            insert_resolved_ref(&tx, resolved).expect("reference resolved ref");
            let supplemental = insert_method_dispatch_edges(&tx, project_root, None)
                .expect("reference dispatch edges");
            assert_eq!(supplemental, 0);
            tx.commit().expect("reference commit");
        }
        conn
    }

    fn build_optimized_connection(
        project_root: &Path,
        extract: &FileExtract,
        resolved: &ResolvedRef,
    ) -> Connection {
        let mut conn = Connection::open_in_memory().expect("open optimized db");
        configure_build_connection(&conn).expect("configure optimized db");
        initialize_schema(&conn).expect("initialize optimized schema");
        {
            let tx = conn.transaction().expect("optimized transaction");
            clear_tables(&tx).expect("optimized clear");
            insert_meta(&tx).expect("optimized meta");
            drop_cold_build_secondary_indexes(&tx).expect("drop secondary indexes");
            {
                let workspace_root = project_root.display().to_string();
                let mut inserts = ColdBuildInsertStatements::new(&tx).expect("prepare inserts");
                insert_file_extract_prepared(&mut inserts, &workspace_root, extract)
                    .expect("optimized file extract");
                insert_resolved_ref_prepared(&mut inserts, resolved)
                    .expect("optimized resolved ref");
            }
            create_cold_build_secondary_indexes(&tx).expect("create secondary indexes");
            let supplemental = insert_method_dispatch_edges(&tx, project_root, None)
                .expect("optimized dispatch edges");
            assert_eq!(supplemental, 0);
            tx.commit().expect("optimized commit");
        }
        conn
    }

    fn fixture_extract(_project_root: &Path) -> FileExtract {
        let rel_path = "src/main.ts".to_string();
        let target_path = "src/helper.ts".to_string();
        let node = NodeRecord {
            id: "node-main".to_string(),
            file_path: rel_path.clone(),
            name: "main".to_string(),
            scoped_name: "main".to_string(),
            kind: "function".to_string(),
            range: Range {
                start_line: 0,
                start_col: 0,
                end_line: 0,
                end_col: 32,
            },
            range_ordinal: 0,
            signature: Some("export function main()".to_string()),
            exported: true,
            is_default_export: false,
            is_type_like: false,
            is_callgraph_entry_point: true,
        };
        let mut dependencies = BTreeSet::new();
        dependencies.insert(target_path.clone());
        let raw_ref = RawRef {
            ref_id: "ref-main-helper".to_string(),
            caller_node: Some(node.id.clone()),
            caller_symbol: Some(node.scoped_name.clone()),
            caller_file: rel_path.clone(),
            kind: "call".to_string(),
            short_name: Some("helper".to_string()),
            full_ref: Some("helper".to_string()),
            module_path: None,
            import_kind: None,
            local_name: Some("helper".to_string()),
            requested_name: Some("helper".to_string()),
            namespace_alias: None,
            wildcard: false,
            line: 1,
            byte_start: 24,
            byte_end: 32,
            dependencies,
        };
        FileExtract {
            rel_path,
            freshness: FileFreshness {
                mtime: UNIX_EPOCH + Duration::from_secs(123),
                size: 40,
                content_hash: cache_freshness::hash_bytes(b"fixture source"),
            },
            lang: LangId::TypeScript,
            data: FileCallData {
                calls_by_symbol: HashMap::new(),
                exported_symbols: Vec::new(),
                symbol_metadata: HashMap::new(),
                default_export_symbol: None,
                import_block: ImportBlock::empty(),
                lang: LangId::TypeScript,
            },
            nodes: vec![node.clone()],
            raw_refs: vec![raw_ref],
            dispatch_hints: vec![DispatchHint {
                id: "dispatch-main-helper".to_string(),
                method_name: "helper".to_string(),
                caller_node: node.id,
                file: "src/main.ts".to_string(),
                line: 1,
                byte_start: 24,
                byte_end: 32,
            }],
            surface_fingerprint: "surface".to_string(),
        }
    }

    fn fixture_resolved(extract: &FileExtract) -> ResolvedRef {
        let raw = extract.raw_refs[0].clone();
        let mut dependencies = raw.dependencies.clone();
        dependencies.insert("src/helper.ts".to_string());
        ResolvedRef {
            edge: Some(EdgeRecord {
                edge_id: "edge-main-helper".to_string(),
                source_node: raw.caller_node.clone().expect("caller node"),
                target_node: Some("node-helper".to_string()),
                target_file: "src/helper.ts".to_string(),
                target_symbol: "helper".to_string(),
                kind: "call".to_string(),
                line: raw.line,
            }),
            raw,
            status: "resolved".to_string(),
            target_node: Some("node-helper".to_string()),
            target_file: Some("src/helper.ts".to_string()),
            target_symbol: Some("helper".to_string()),
            dependencies,
        }
    }

    fn write_chunked_equivalence_fixture(project_root: &Path) {
        let ts_dir = project_root.join("ts");
        fs::create_dir_all(&ts_dir).expect("create ts dir");
        fs::write(
            ts_dir.join("leaf.ts"),
            "export function leaf(value: number) {\n  return value + 1;\n}\n",
        )
        .expect("write ts leaf");
        fs::write(
            ts_dir.join("mid.ts"),
            "import { leaf } from './leaf';\n\nexport function mid(value: number) {\n  return leaf(value);\n}\n",
        )
        .expect("write ts mid");
        fs::write(
            ts_dir.join("entry.ts"),
            "import { mid } from './mid';\nimport { Worker } from './worker';\n\nexport function entry(worker: Worker) {\n  return mid(worker.run());\n}\n",
        )
        .expect("write ts entry");
        fs::write(
            ts_dir.join("worker.ts"),
            "export class Worker {\n  run() {\n    return 41;\n  }\n}\n",
        )
        .expect("write ts worker");
        for idx in 0..4 {
            fs::write(
                ts_dir.join(format!("extra_{idx}.ts")),
                format!(
                    "import {{ entry }} from './entry';\nimport {{ Worker }} from './worker';\n\nexport function extra{idx}() {{\n  return entry(new Worker());\n}}\n"
                ),
            )
            .expect("write ts extra");
        }

        let rust_dir = project_root.join("src");
        let commands_dir = rust_dir.join("commands");
        fs::create_dir_all(&commands_dir).expect("create rust commands dir");
        fs::write(
            rust_dir.join("context.rs"),
            r#"pub struct AppContext;

impl AppContext {
    pub fn callgraph_store_for_ops(&self) -> usize {
        1
    }
}
"#,
        )
        .expect("write rust context");
        fs::write(
            rust_dir.join("lib.rs"),
            "pub mod context;\npub mod commands;\n",
        )
        .expect("write rust lib");
        fs::write(
            commands_dir.join("mod.rs"),
            "pub mod callers;\npub mod impact;\npub mod trace_to;\n",
        )
        .expect("write rust commands mod");
        for name in ["callers", "impact", "trace_to"] {
            fs::write(
                commands_dir.join(format!("{name}.rs")),
                format!(
                    r#"use crate::context::AppContext;

pub fn handle_{name}(ctx: &AppContext) -> usize {{
    ctx.callgraph_store_for_ops()
}}
"#
                ),
            )
            .expect("write rust command");
        }
    }

    fn write_barrel_refresh_fixture(project_root: &Path, barrel_source: &str) -> Vec<PathBuf> {
        let src_dir = project_root.join("src");
        fs::create_dir_all(&src_dir).expect("create src dir");

        let target_path = src_dir.join("target.ts");
        fs::write(&target_path, "export function target() {\n  return 1;\n}\n")
            .expect("write target");

        let index_path = src_dir.join("index.ts");
        fs::write(&index_path, barrel_source).expect("write barrel");

        let mut files = vec![target_path, index_path];
        for (file_name, function_name) in [
            ("consumer_a.ts", "consumerA"),
            ("consumer_b.ts", "consumerB"),
            ("consumer_c.ts", "consumerC"),
        ] {
            let path = src_dir.join(file_name);
            fs::write(
                &path,
                format!(
                    "import {{ target }} from \"./index\";\n\nexport function {function_name}() {{\n  return target();\n}}\n"
                ),
            )
            .expect("write consumer");
            files.push(path);
        }
        files
    }

    fn graph_table_rows(store: &CallGraphStore, table: &str) -> Vec<String> {
        let conn = store.conn.lock().expect("callgraph store mutex poisoned");
        table_rows(&conn, table)
    }

    fn graph_table_rows_without(
        store: &CallGraphStore,
        table: &str,
        excluded_columns: &[&str],
    ) -> Vec<String> {
        let conn = store.conn.lock().expect("callgraph store mutex poisoned");
        table_rows_without(&conn, table, excluded_columns)
    }

    fn table_rows(conn: &Connection, table: &str) -> Vec<String> {
        table_rows_without(conn, table, &[])
    }

    fn table_rows_without(
        conn: &Connection,
        table: &str,
        excluded_columns: &[&str],
    ) -> Vec<String> {
        let excluded_columns = excluded_columns.iter().copied().collect::<BTreeSet<_>>();
        let columns: Vec<String> = conn
            .prepare(&format!("PRAGMA table_info({table})"))
            .expect("prepare table_info")
            .query_map([], |row| row.get::<_, String>(1))
            .expect("query table_info")
            .collect::<std::result::Result<Vec<String>, _>>()
            .expect("collect columns")
            .into_iter()
            .filter(|column| !excluded_columns.contains(column.as_str()))
            .collect();
        let sql = format!(
            "SELECT {} FROM {table} ORDER BY {}",
            columns.join(", "),
            columns.join(", ")
        );
        conn.prepare(&sql)
            .expect("prepare table rows")
            .query_map([], |row| row_to_strings(row, columns.len()))
            .expect("query table rows")
            .collect::<std::result::Result<_, _>>()
            .expect("collect table rows")
    }

    fn assert_cold_build_stats_match_except_elapsed(
        expected: &ColdBuildStats,
        actual: &ColdBuildStats,
    ) {
        assert_eq!(actual.files, expected.files, "file counts must match");
        assert_eq!(actual.nodes, expected.nodes, "node counts must match");
        assert_eq!(actual.refs, expected.refs, "ref counts must match");
        assert_eq!(actual.edges, expected.edges, "edge counts must match");
        assert_eq!(
            actual.failed_files.iter().cloned().collect::<BTreeSet<_>>(),
            expected
                .failed_files
                .iter()
                .cloned()
                .collect::<BTreeSet<_>>(),
            "failed file sets must match"
        );
    }

    fn backend_state_rows(conn: &Connection) -> Vec<String> {
        conn.prepare(
            "SELECT backend, workspace_root, file_path, content_hash, status
             FROM backend_file_state
             ORDER BY backend, workspace_root, file_path, content_hash, status",
        )
        .expect("prepare backend rows")
        .query_map([], |row| row_to_strings(row, 5))
        .expect("query backend rows")
        .collect::<std::result::Result<_, _>>()
        .expect("collect backend rows")
    }

    fn secondary_indexes(conn: &Connection) -> Vec<String> {
        let mut indexes = Vec::new();
        for table in [
            "files",
            "nodes",
            "refs",
            "file_dependencies",
            "edges",
            "dispatch_hints",
            "type_ref_names",
            "backend_file_state",
            "meta",
        ] {
            let sql = format!("PRAGMA index_list({table})");
            let mut stmt = conn.prepare(&sql).expect("prepare index list");
            let rows = stmt
                .query_map([], |row| row.get::<_, String>(1))
                .expect("query index list");
            for name in rows {
                let name = name.expect("index name");
                if name.starts_with("idx_") {
                    indexes.push(format!("{table}:{name}"));
                }
            }
        }
        indexes.sort();
        indexes
    }

    fn row_to_strings(row: &rusqlite::Row<'_>, len: usize) -> rusqlite::Result<String> {
        let mut values = Vec::with_capacity(len);
        for index in 0..len {
            let value = row.get_ref(index)?;
            values.push(match value {
                rusqlite::types::ValueRef::Null => "NULL".to_string(),
                rusqlite::types::ValueRef::Integer(value) => value.to_string(),
                rusqlite::types::ValueRef::Real(value) => value.to_string(),
                rusqlite::types::ValueRef::Text(value) => {
                    String::from_utf8_lossy(value).into_owned()
                }
                rusqlite::types::ValueRef::Blob(value) => format!("{value:?}"),
            });
        }
        Ok(values.join("\u{1f}"))
    }
}

#[cfg(test)]
mod rust_resolution_tests {
    use super::*;
    use crate::inspect::job::CallgraphSnapshot;
    use std::fs;
    use tempfile::tempdir;

    #[test]
    fn rust_function_scoped_module_alias_resolves_and_projects_live() {
        let dir = tempdir().expect("tempdir");
        let root = dir.path();
        write_rust_manifest(root, "scoped-alias-fixture");
        write_file(
            root,
            "src/lib.rs",
            r#"pub mod finalization_contract;

pub fn run_alias() {
    use crate::finalization_contract as fc;
    fc::check_mason_contract();
}
"#,
        );
        write_file(
            root,
            "src/finalization_contract.rs",
            r#"pub fn check_mason_contract() {}
fn planted_dead() {}
"#,
        );

        let (store, snapshot) = cold_build_twice(root);
        assert_direct_caller(
            &store,
            "src/finalization_contract.rs",
            "check_mason_contract",
            "src/lib.rs",
            "run_alias",
        );
        assert_projected_call(
            root,
            &snapshot,
            "src/finalization_contract.rs",
            "check_mason_contract",
        );
        assert_no_projected_call(
            root,
            &snapshot,
            "src/finalization_contract.rs",
            "planted_dead",
        );
        assert!(
            store
                .direct_callers_of(Path::new("src/finalization_contract.rs"), "planted_dead")
                .expect("planted dead callers")
                .is_empty(),
            "planted-dead guard should stay without callers"
        );
    }

    #[test]
    fn rust_inline_sibling_module_qualified_calls_resolve_scoped_targets() {
        let dir = tempdir().expect("tempdir");
        let root = dir.path();
        write_rust_manifest(root, "inline-module-fixture");
        write_file(
            root,
            "src/lib.rs",
            r#"mod work_graph { fn operations() {} }
mod manifest { fn operations() {} }
mod audit { fn operations() {} }
mod dispatch { fn operations() {} }
mod finalization { fn operations() {} }

pub fn run_inline_operations() {
    work_graph::operations();
    manifest::operations();
    audit::operations();
    dispatch::operations();
    finalization::operations();
}

fn planted_dead() {}
"#,
        );

        let (store, snapshot) = cold_build_twice(root);
        for module in [
            "work_graph",
            "manifest",
            "audit",
            "dispatch",
            "finalization",
        ] {
            assert_direct_caller(
                &store,
                "src/lib.rs",
                &format!("{module}::operations"),
                "src/lib.rs",
                "run_inline_operations",
            );
        }
        assert_projected_call(root, &snapshot, "src/lib.rs", "operations");
        assert_no_projected_call(root, &snapshot, "src/lib.rs", "planted_dead");
    }

    #[test]
    fn rust_workspace_pub_use_reexport_resolves_to_source_file() {
        let dir = tempdir().expect("tempdir");
        let root = dir.path();
        fs::write(
            root.join("Cargo.toml"),
            "[workspace]\nresolver = \"2\"\nmembers = [\"crates/but-action\", \"crates/app\"]\n",
        )
        .expect("write workspace manifest");
        write_file(
            root,
            "crates/but-action/Cargo.toml",
            r#"[package]
name = "but-action"
version = "0.1.0"
edition = "2021"
"#,
        );
        write_file(
            root,
            "crates/but-action/src/lib.rs",
            "mod action;\npub use action::{list_actions};\n",
        );
        write_file(
            root,
            "crates/but-action/src/action.rs",
            "pub fn list_actions() {}\nfn planted_dead() {}\n",
        );
        write_file(
            root,
            "crates/app/Cargo.toml",
            r#"[package]
name = "app"
version = "0.1.0"
edition = "2021"
"#,
        );
        write_file(
            root,
            "crates/app/src/lib.rs",
            "pub fn run_actions() {\n    but_action::list_actions();\n}\n",
        );

        let (store, snapshot) = cold_build_twice(root);
        assert_direct_caller(
            &store,
            "crates/but-action/src/action.rs",
            "list_actions",
            "crates/app/src/lib.rs",
            "run_actions",
        );
        assert!(
            store
                .direct_callers_of(Path::new("crates/but-action/src/lib.rs"), "list_actions")
                .expect("lib reexport callers")
                .is_empty(),
            "call should target the reexported source function, not lib.rs"
        );
        assert_projected_call(
            root,
            &snapshot,
            "crates/but-action/src/action.rs",
            "list_actions",
        );
        assert_no_projected_call(
            root,
            &snapshot,
            "crates/but-action/src/action.rs",
            "planted_dead",
        );
    }

    #[test]
    fn rust_generic_self_turbofish_method_dispatch_resolves() {
        let dir = tempdir().expect("tempdir");
        let root = dir.path();
        write_rust_manifest(root, "generic-self-fixture");
        write_file(
            root,
            "src/lib.rs",
            r#"pub struct Matcher;

impl Matcher {
    pub fn run(&self) -> bool {
        self.fuzzy_match_optimal::<usize>("needle")
    }

    fn fuzzy_match_optimal<T>(&self, _needle: &str) -> bool {
        let _ = std::marker::PhantomData::<T>;
        true
    }

    fn planted_dead(&self) {}
}

pub fn entry() -> bool {
    let matcher = Matcher;
    matcher.run()
}
"#,
        );

        let (store, snapshot) = cold_build_twice(root);
        assert_direct_caller(
            &store,
            "src/lib.rs",
            "Matcher::fuzzy_match_optimal",
            "src/lib.rs",
            "Matcher::run",
        );
        assert_projected_call(root, &snapshot, "src/lib.rs", "fuzzy_match_optimal");
        assert_no_projected_call(root, &snapshot, "src/lib.rs", "planted_dead");
    }

    #[test]
    fn rust_manifest_operations_named_import_is_not_the_missing_edge() {
        let dir = tempdir().expect("tempdir");
        let root = dir.path();
        write_rust_manifest(root, "manifest-operations-fixture");
        write_file(
            root,
            "src/main.rs",
            r#"mod dispatch;
use dispatch::{manifest_operations};

fn main() {
    manifest_operations();
}
"#,
        );
        write_file(
            root,
            "src/dispatch.rs",
            r#"mod work_graph { fn operations() {} }
mod manifest { fn operations() {} }
mod audit { fn operations() {} }
mod descriptor { fn operations() {} }
mod writer { fn operations() {} }

pub fn manifest_operations() {
    manifest::operations();
}

pub fn work_graph_operations() {
    work_graph::operations();
}

pub fn audit_operations() {
    audit::operations();
}

pub fn descriptor_operations() {
    descriptor::operations();
}

pub fn writer_operations() {
    writer::operations();
}

fn planted_dead() {}
"#,
        );

        let (store, snapshot) = cold_build_twice(root);
        assert_direct_caller(
            &store,
            "src/dispatch.rs",
            "manifest_operations",
            "src/main.rs",
            "main",
        );
        assert_direct_caller(
            &store,
            "src/dispatch.rs",
            "manifest::operations",
            "src/dispatch.rs",
            "manifest_operations",
        );
        assert_projected_call(root, &snapshot, "src/dispatch.rs", "manifest_operations");
        assert_projected_call(root, &snapshot, "src/dispatch.rs", "operations");
        assert_no_projected_call(root, &snapshot, "src/dispatch.rs", "planted_dead");
    }

    fn cold_build_twice(root: &Path) -> (CallGraphStore, CallgraphSnapshot) {
        let files = rust_files(root);
        let first = CallGraphStore::open(root.join(".store-first"), root.to_path_buf())
            .expect("open first store");
        first.cold_build(&files).expect("first cold build");
        let first_snapshot =
            project_dead_code_snapshot(first.sqlite_path()).expect("first projected snapshot");

        let second = CallGraphStore::open(root.join(".store-second"), root.to_path_buf())
            .expect("open second store");
        second.cold_build(&files).expect("second cold build");
        let second_snapshot =
            project_dead_code_snapshot(second.sqlite_path()).expect("second projected snapshot");

        assert_eq!(
            projection_rows(&first_snapshot),
            projection_rows(&second_snapshot),
            "cold-build projection should be deterministic"
        );
        (first, first_snapshot)
    }

    fn projection_rows(snapshot: &CallgraphSnapshot) -> Vec<String> {
        let mut rows = Vec::new();
        for export in &snapshot.exported_symbols {
            rows.push(format!(
                "export\t{}\t{}\t{}\t{}",
                export.file.display(),
                export.symbol,
                export.kind,
                export.line
            ));
        }
        for call in &snapshot.outbound_calls {
            rows.push(format!(
                "call\t{}\t{}\t{}\t{}\t{}",
                call.caller_file.display(),
                call.caller_symbol,
                call.target,
                call.line,
                call.provenance
            ));
        }
        for file in &snapshot.entry_points {
            rows.push(format!("entry_file\t{}", file.display()));
        }
        for (file, symbols) in &snapshot.entry_point_symbols {
            for symbol in symbols {
                rows.push(format!("entry_symbol\t{}\t{symbol}", file.display()));
            }
        }
        rows.sort();
        rows
    }

    fn assert_direct_caller(
        store: &CallGraphStore,
        target_rel: &str,
        target_symbol: &str,
        caller_rel: &str,
        caller_symbol: &str,
    ) {
        let callers = store
            .direct_callers_of(Path::new(target_rel), target_symbol)
            .unwrap_or_else(|error| {
                panic!("direct callers for {target_rel}::{target_symbol}: {error}")
            });
        assert!(
            callers.iter().any(|site| {
                site.caller.file == caller_rel && site.caller.symbol == caller_symbol
            }),
            "expected {caller_rel}::{caller_symbol} to call {target_rel}::{target_symbol}; callers: {callers:#?}"
        );
    }

    fn assert_projected_call(
        root: &Path,
        snapshot: &CallgraphSnapshot,
        target_rel: &str,
        symbol: &str,
    ) {
        let target = projected_target(root, target_rel, symbol);
        assert!(
            snapshot.outbound_calls.iter().any(|call| {
                call.target == target
                    || call.target.starts_with(&format!(
                        "{target}{}",
                        crate::inspect::job::DISPATCHED_CALLEE_SEPARATOR
                    ))
            }),
            "expected projected call to {target}; calls: {:#?}",
            snapshot.outbound_calls
        );
    }

    fn assert_no_projected_call(
        root: &Path,
        snapshot: &CallgraphSnapshot,
        target_rel: &str,
        symbol: &str,
    ) {
        let target = projected_target(root, target_rel, symbol);
        assert!(
            snapshot.outbound_calls.iter().all(|call| {
                call.target != target
                    && !call.target.starts_with(&format!(
                        "{target}{}",
                        crate::inspect::job::DISPATCHED_CALLEE_SEPARATOR
                    ))
            }),
            "did not expect projected call to {target}; calls: {:#?}",
            snapshot.outbound_calls
        );
    }

    fn projected_target(root: &Path, target_rel: &str, symbol: &str) -> String {
        // Projection targets carry the normalized (verbatim-stripped)
        // canonical form; bare fs::canonicalize diverges on Windows.
        let path = crate::inspect::job::canonicalize_normalized(&root.join(target_rel));
        format!("{}::{symbol}", path.display())
    }

    fn write_rust_manifest(root: &Path, name: &str) {
        write_file(
            root,
            "Cargo.toml",
            &format!("[package]\nname = \"{name}\"\nversion = \"0.1.0\"\nedition = \"2021\"\n"),
        );
    }

    fn write_file(root: &Path, rel_path: &str, source: &str) -> PathBuf {
        let path = root.join(rel_path);
        fs::create_dir_all(path.parent().expect("fixture parent")).expect("create fixture parent");
        fs::write(&path, source).expect("write fixture file");
        path
    }

    fn rust_files(root: &Path) -> Vec<PathBuf> {
        let mut files = Vec::new();
        collect_rust_files(root, &mut files);
        files.sort();
        files
    }

    fn collect_rust_files(dir: &Path, files: &mut Vec<PathBuf>) {
        for entry in fs::read_dir(dir).expect("read fixture dir") {
            let entry = entry.expect("read fixture entry");
            let path = entry.path();
            if path.is_dir() {
                let name = path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .unwrap_or("");
                if !name.starts_with(".store") {
                    collect_rust_files(&path, files);
                }
            } else if path.extension().and_then(|ext| ext.to_str()) == Some("rs") {
                files.push(path);
            }
        }
    }
}

#[cfg(test)]
mod build_pool_tests {
    use super::build_pool_size;

    #[test]
    fn build_pool_is_bounded_to_half_cores_capped_at_eight() {
        let size = build_pool_size();
        // Never zero, never the full core count, never above the 8 cap — this is
        // the starvation guard for the cold-build's all-cores tree-sitter pass.
        assert!(size >= 1, "pool size must be at least 1");
        assert!(size <= 8, "pool size must be capped at 8, got {size}");

        let cores = std::thread::available_parallelism()
            .map(|p| p.get())
            .unwrap_or(1);
        let expected = cores.div_ceil(2).clamp(1, 8);
        assert_eq!(size, expected, "pool size must be div_ceil(2).clamp(1,8)");
    }
}

#[cfg(test)]
mod reexport_resolution_tests {
    use super::*;

    fn barrel_index(files: Vec<(String, DbFileIndex)>) -> ProjectIndex<'static> {
        ProjectIndex {
            project_root: PathBuf::from("/fixture"),
            files: files.into_iter().collect(),
            caller_data: HashMap::new(),
            workspace_crate_prefixes: WorkspaceCratePrefixCache::default(),
        }
    }

    fn barrel_file(reexport_targets: &[&str]) -> DbFileIndex {
        DbFileIndex {
            lang: None,
            exports: HashSet::new(),
            default_export: None,
            export_aliases: HashMap::new(),
            node_by_scoped: HashMap::new(),
            node_by_bare: HashMap::new(),
            module_targets: HashMap::new(),
            reexports: reexport_targets
                .iter()
                .map(|target| ReexportIndex {
                    target_file: Some((*target).to_string()),
                    named: HashMap::new(),
                    wildcard: true,
                })
                .collect(),
        }
    }

    /// A dense wildcard re-export cycle (barrel files re-exporting each
    /// other) must resolve in O(files), not O(branching^depth). Without the
    /// resolver's memoization, resolving a MISSING symbol through this
    /// 12-file complete digraph explores ~11^16 paths and this test never
    /// finishes: the depth cap bounds path length, not path count, and one
    /// such resolution can pin a worker thread at 100% CPU indefinitely.
    #[test]
    fn missing_symbol_in_dense_wildcard_reexport_cycle_terminates() {
        let names: Vec<String> = (0..12).map(|i| format!("src/barrel{i}.ts")).collect();
        let files = names
            .iter()
            .map(|name| {
                let targets: Vec<&str> = names
                    .iter()
                    .filter(|other| *other != name)
                    .map(String::as_str)
                    .collect();
                (name.clone(), barrel_file(&targets))
            })
            .collect();
        let index = barrel_index(files);

        assert_eq!(
            resolve_exported_symbol(&index, "src/barrel0.ts", "does_not_exist", 0),
            None
        );
    }

    /// Depth-dominance counterexample: the walk first reaches `shared` down a
    /// 16-hop chain (no budget left for its leaf), then reaches it again
    /// directly at depth 1. Plain visited-set pruning would skip the second
    /// visit and lose a resolution the capped resolver finds; the
    /// depth-dominance memo revisits because the second arrival is shallower.
    #[test]
    fn shallow_revisit_after_deep_capped_visit_still_resolves() {
        let mut leaf = barrel_file(&[]);
        leaf.exports.insert("deep_symbol".to_string());
        let mut files: Vec<(String, DbFileIndex)> = Vec::new();
        // entry -> chain0 -> chain1 -> ... -> chain14 -> shared -> leaf
        // entry's SECOND reexport goes straight to shared.
        files.push((
            "src/entry.ts".to_string(),
            barrel_file(&["src/chain0.ts", "src/shared.ts"]),
        ));
        for i in 0..15 {
            let next = if i == 14 {
                "src/shared.ts".to_string()
            } else {
                format!("src/chain{}.ts", i + 1)
            };
            files.push((format!("src/chain{i}.ts"), barrel_file(&[&next])));
        }
        files.push(("src/shared.ts".to_string(), barrel_file(&["src/leaf.ts"])));
        files.push(("src/leaf.ts".to_string(), leaf));
        let index = barrel_index(files);

        assert_eq!(
            resolve_exported_symbol(&index, "src/entry.ts", "deep_symbol", 0),
            Some(("src/leaf.ts".to_string(), "deep_symbol".to_string())),
            "a shallower re-visit must not be pruned by a deeper capped visit"
        );
    }

    #[test]
    fn symbol_reachable_through_reexport_cycle_still_resolves() {
        let mut leaf = barrel_file(&[]);
        leaf.exports.insert("real_symbol".to_string());
        let index = barrel_index(vec![
            (
                "src/a.ts".to_string(),
                barrel_file(&["src/b.ts", "src/a.ts"]),
            ),
            (
                "src/b.ts".to_string(),
                barrel_file(&["src/a.ts", "src/leaf.ts"]),
            ),
            ("src/leaf.ts".to_string(), leaf),
        ]);

        assert_eq!(
            resolve_exported_symbol(&index, "src/a.ts", "real_symbol", 0),
            Some(("src/leaf.ts".to_string(), "real_symbol".to_string()))
        );
    }
}

#[cfg(test)]
mod method_dispatch_inference_tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    #[test]
    fn java_field_receiver_type_selects_declared_class_method() {
        let source = r#"class EntryPoint {
    private UserService userService;

    void handle() {
        userService.find();
    }
}

class UserService {
    void find() {}
}

class AuditService {
    void find() {}
}
"#;
        let dir = tempdir().expect("temp dir");
        let root = dir.path();
        write_fixture(root, "src/EntryPoint.java", source);
        let reference = reference(
            "java",
            "src/EntryPoint.java",
            "EntryPoint::handle",
            "userService",
            "find",
            line_of(source, "userService.find()"),
        );
        let mut cache = DispatchSourceCache::new();

        let receiver_type =
            infer_receiver_type(root, &reference, &mut cache).expect("receiver type");
        assert_eq!(receiver_type, "UserService");

        let candidates = vec![
            method_candidate("audit", "AuditService::find"),
            method_candidate("user", "UserService::find"),
        ];
        let selected = select_type_match_candidate(&reference, &candidates, &receiver_type)
            .expect("type candidate");
        assert_eq!(selected.scoped_name, "UserService::find");

        let wrong_candidates = vec![method_candidate("audit", "AuditService::find")];
        assert!(
            select_type_match_candidate(&reference, &wrong_candidates, &receiver_type).is_none()
        );
    }

    #[test]
    fn kotlin_property_and_local_value_types_are_inferred() {
        let source = r#"class Handler {
    private val auditService: AuditService = AuditService()

    fun handle() {
        auditService.find()
        val userService: UserService = UserService()
        userService.find()
        val billingService = BillingService()
        billingService.find()
    }
}

class UserService { fun find() {} }
class AuditService { fun find() {} }
class BillingService { fun find() {} }
"#;
        let dir = tempdir().expect("temp dir");
        let root = dir.path();
        write_fixture(root, "src/Handler.kt", source);
        let mut cache = DispatchSourceCache::new();

        let audit_ref = reference(
            "kotlin",
            "src/Handler.kt",
            "Handler::handle",
            "auditService",
            "find",
            line_of(source, "auditService.find()"),
        );
        assert_eq!(
            infer_receiver_type(root, &audit_ref, &mut cache).as_deref(),
            Some("AuditService")
        );

        let user_ref = reference(
            "kotlin",
            "src/Handler.kt",
            "Handler::handle",
            "userService",
            "find",
            line_of(source, "userService.find()"),
        );
        assert_eq!(
            infer_receiver_type(root, &user_ref, &mut cache).as_deref(),
            Some("UserService")
        );

        let billing_ref = reference(
            "kotlin",
            "src/Handler.kt",
            "Handler::handle",
            "billingService",
            "find",
            line_of(source, "billingService.find()"),
        );
        assert_eq!(
            infer_receiver_type(root, &billing_ref, &mut cache).as_deref(),
            Some("BillingService")
        );
    }

    #[test]
    fn cpp_declarator_and_auto_factory_receiver_types_are_inferred() {
        let source = r#"struct Foo { void run(); };
struct PointerFoo { void run(); };
struct FactoryFoo { void run(); };
FactoryFoo makeFactoryFoo();

void handle() {
    Foo foo;
    foo.run();
    PointerFoo* pointerFoo = nullptr;
    pointerFoo->run();
    auto factoryFoo = makeFactoryFoo();
    factoryFoo.run();
}
"#;
        let dir = tempdir().expect("temp dir");
        let root = dir.path();
        write_fixture(root, "src/fixture.cpp", source);
        let mut cache = DispatchSourceCache::new();

        let foo_ref = reference(
            "cpp",
            "src/fixture.cpp",
            "handle",
            "foo",
            "run",
            line_of(source, "foo.run()"),
        );
        assert_eq!(
            infer_receiver_type(root, &foo_ref, &mut cache).as_deref(),
            Some("Foo")
        );

        let pointer_ref = reference(
            "cpp",
            "src/fixture.cpp",
            "handle",
            "pointerFoo",
            "run",
            line_of(source, "pointerFoo->run()"),
        );
        assert_eq!(
            infer_receiver_type(root, &pointer_ref, &mut cache).as_deref(),
            Some("PointerFoo")
        );

        let factory_ref = reference(
            "cpp",
            "src/fixture.cpp",
            "handle",
            "factoryFoo",
            "run",
            line_of(source, "factoryFoo.run()"),
        );
        assert_eq!(
            infer_receiver_type(root, &factory_ref, &mut cache).as_deref(),
            Some("FactoryFoo")
        );
    }

    #[test]
    fn rust_direct_self_field_name_trims_separator_whitespace() {
        for receiver_expression in ["self .engine", "self. engine", "self . engine"] {
            assert_eq!(
                rust_direct_self_field_name(receiver_expression),
                Some("engine")
            );
        }
    }

    #[test]
    fn rust_direct_self_field_receiver_type_is_conservative() {
        let source = r#"struct Engine;

struct Car {
    engine: Engine,
}

impl Car {
    fn run(&self) {
        self.engine.start();
    }
}

struct NestedCar {
    engine: Engine,
}

impl NestedCar {
    fn run(&self) {
        self.inner.engine.start();
    }
}

struct WrappedCar {
    engine: Option<Engine>,
}

impl WrappedCar {
    fn run(&self) {
        self.engine.start(); // wrapped
    }
}

struct GenericCar<T> {
    engine: T,
}

impl<T> GenericCar<T> {
    fn run(&self) {
        self.engine.start(); // generic
    }
}

type EngineAlias = Engine;

struct AliasCar {
    engine: EngineAlias,
}

impl AliasCar {
    fn run(&self) {
        self.engine.start(); // alias
    }
}
"#;
        let dir = tempdir().expect("temp dir");
        let root = dir.path();
        write_fixture(root, "src/lib.rs", source);
        let mut cache = DispatchSourceCache::new();

        let mut direct = reference(
            "rust",
            "src/lib.rs",
            "Car::run",
            "engine",
            "start",
            line_of(source, "self.engine.start()"),
        );
        direct.receiver_expression = "self.engine".to_string();
        assert_eq!(
            infer_receiver_type(root, &direct, &mut cache).as_deref(),
            Some("Engine")
        );

        let mut mismatched_impl_target = direct.clone();
        mismatched_impl_target.caller_symbol = "other::Car::run".to_string();
        assert!(infer_receiver_type(root, &mismatched_impl_target, &mut cache).is_none());

        let mut nested = reference(
            "rust",
            "src/lib.rs",
            "NestedCar::run",
            "engine",
            "start",
            line_of(source, "self.inner.engine.start()"),
        );
        nested.receiver_expression = "self.inner.engine".to_string();
        assert!(infer_receiver_type(root, &nested, &mut cache).is_none());

        let mut wrapped = reference(
            "rust",
            "src/lib.rs",
            "WrappedCar::run",
            "engine",
            "start",
            line_of(source, "self.engine.start(); // wrapped"),
        );
        wrapped.receiver_expression = "self.engine".to_string();
        assert!(infer_receiver_type(root, &wrapped, &mut cache).is_none());

        let mut generic = reference(
            "rust",
            "src/lib.rs",
            "GenericCar::run",
            "engine",
            "start",
            line_of(source, "self.engine.start(); // generic"),
        );
        generic.receiver_expression = "self.engine".to_string();
        assert!(infer_receiver_type(root, &generic, &mut cache).is_none());

        let mut alias = reference(
            "rust",
            "src/lib.rs",
            "AliasCar::run",
            "engine",
            "start",
            line_of(source, "self.engine.start(); // alias"),
        );
        alias.receiver_expression = "self.engine".to_string();
        assert!(infer_receiver_type(root, &alias, &mut cache).is_none());
    }

    #[test]
    fn rust_direct_self_reference_field_receiver_is_not_inferred() {
        let source = r#"struct Engine;

struct Car {
    engine: &'static Engine,
}

impl Car {
    fn run(&self) {
        self.engine.start();
    }
}
"#;
        let dir = tempdir().expect("temp dir");
        let root = dir.path();
        write_fixture(root, "src/lib.rs", source);
        let mut cache = DispatchSourceCache::new();
        let mut reference = reference(
            "rust",
            "src/lib.rs",
            "Car::run",
            "engine",
            "start",
            line_of(source, "self.engine.start()"),
        );
        reference.receiver_expression = "self.engine".to_string();

        assert!(infer_receiver_type(root, &reference, &mut cache).is_none());
    }

    #[test]
    fn rust_trait_impl_self_field_receiver_is_not_inferred() {
        let source = r#"trait Drive {
    fn run(&self);
}

struct Engine;

struct Car {
    engine: Engine,
}

impl Drive for Car {
    fn run(&self) {
        self.engine.start();
    }
}
"#;
        let dir = tempdir().expect("temp dir");
        let root = dir.path();
        write_fixture(root, "src/lib.rs", source);
        let mut cache = DispatchSourceCache::new();
        let mut reference = reference(
            "rust",
            "src/lib.rs",
            "Car::run",
            "engine",
            "start",
            line_of(source, "self.engine.start()"),
        );
        reference.receiver_expression = "self.engine".to_string();

        assert!(infer_receiver_type(root, &reference, &mut cache).is_none());
    }

    #[test]
    fn rust_self_field_does_not_bind_struct_from_another_module() {
        let source = r#"struct Engine;

mod unrelated {
    struct Car {
        engine: Engine,
    }
}

impl Car {
    fn run(&self) {
        self.engine.start();
    }
}
"#;
        let dir = tempdir().expect("temp dir");
        let root = dir.path();
        write_fixture(root, "src/lib.rs", source);
        let mut cache = DispatchSourceCache::new();
        let mut reference = reference(
            "rust",
            "src/lib.rs",
            "Car::run",
            "engine",
            "start",
            line_of(source, "self.engine.start()"),
        );
        reference.receiver_expression = "self.engine".to_string();

        assert!(infer_receiver_type(root, &reference, &mut cache).is_none());
    }

    #[test]
    fn unknown_java_receiver_still_uses_name_match_fallback() {
        let source = r#"class EntryPoint {
    void handle() {
        service.runSpecial();
    }
}

class OnlyService {
    void runSpecial() {}
}
"#;
        let dir = tempdir().expect("temp dir");
        let root = dir.path();
        write_fixture(root, "src/EntryPoint.java", source);
        let reference = reference(
            "java",
            "src/EntryPoint.java",
            "EntryPoint::handle",
            "service",
            "runSpecial",
            line_of(source, "service.runSpecial()"),
        );
        let mut cache = DispatchSourceCache::new();

        assert!(infer_receiver_type(root, &reference, &mut cache).is_none());
        let candidates = vec![method_candidate("only", "OnlyService::runSpecial")];
        let selected = select_name_match_candidate(&reference, &candidates).expect("name match");
        assert_eq!(selected.scoped_name, "OnlyService::runSpecial");
    }

    fn reference(
        lang: &str,
        caller_file: &str,
        caller_symbol: &str,
        receiver: &str,
        method_name: &str,
        line: u32,
    ) -> NameMatchRef {
        NameMatchRef {
            ref_id: format!("{caller_file}:{line}:{receiver}:{method_name}"),
            caller_node: format!("{caller_symbol}:node"),
            caller_file: caller_file.to_string(),
            caller_symbol: caller_symbol.to_string(),
            caller_signature: None,
            receiver_expression: receiver.to_string(),
            receiver: receiver.to_string(),
            method_name: method_name.to_string(),
            colon_dispatch: false,
            line,
            lang: lang.to_string(),
        }
    }

    fn method_candidate(node_id: &str, scoped_name: &str) -> NameMatchCandidate {
        NameMatchCandidate {
            node_id: node_id.to_string(),
            file_path: "src/targets.fixture".to_string(),
            scoped_name: scoped_name.to_string(),
            kind: "method".to_string(),
            start_line: 1,
        }
    }

    fn write_fixture(root: &std::path::Path, rel_path: &str, source: &str) {
        let path = root.join(rel_path);
        fs::create_dir_all(path.parent().expect("fixture parent")).expect("create parent");
        fs::write(path, source).expect("write fixture");
    }

    fn line_of(source: &str, needle: &str) -> u32 {
        source
            .lines()
            .position(|line| line.contains(needle))
            .map(|index| index as u32 + 1)
            .unwrap_or_else(|| panic!("missing line containing {needle:?}"))
    }
}
