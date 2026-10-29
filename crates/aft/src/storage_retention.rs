//! Durable checkout retention, independent of the roots an actor has opened.
//!
//! A missing path is necessary but not sufficient for deletion: binding history
//! must also be old, and all writers, pins, readers and local SQLite handles must
//! have gone. Unknown keys receive a fresh observation grace instead of treating
//! an old payload mtime as proof that a root has not recently been bound.
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::fs::{self, ReadDir};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::{atomic::AtomicU64, Arc, Condvar, Mutex, OnceLock, Weak};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::context::SubcLifecycleAdmission;

pub(crate) const ROOT_GRACE_MS: u64 = 7 * 24 * 60 * 60 * 1000;
/// A deleted linked worktree (a worker checkout) has its caches removed on
/// the first pass once this long has passed since its last bind. It is not
/// zero so that a session that bound the checkout seconds before deletion
/// can still finish, and a checkout recreated at the same path rebinds before
/// its caches can be removed. With the 10-minute pass interval and the
/// due-first lane, the caches go on the first pass after deletion. Every
/// other check still applies: the root must be confirmed missing, its volume
/// must match, and nothing may hold a writer, pin, reader or SQLite handle.
const LINKED_WORKTREE_REBIND_MARGIN_MS: u64 = 10 * 60 * 1000;
use crate::root_cache::StoragePaths;
/// A checkout view folder (`views/<scope>`) that no family registry lists and
/// no binding history describes was never tied to a checkout this storage
/// knows about, typically an abandoned build. It is reclaimed once it has
/// been observed for this long and none of its files changed for this long.
const ORPHAN_VIEW_GRACE_MS: u64 = 60 * 60 * 1000;
/// Caches of checkouts already past their grace that one pass processes
/// before resuming the lexical walk.
const DUE_FIRST_ENTRIES: usize = 256;
const INTERVAL: Duration = Duration::from_secs(10 * 60);
// Startup can warm many checkout views at once. Keep storage-wide V1 blob
// marking (which must exclude pin admission) off that request-critical lane.
const STARTUP_GRACE: Duration = Duration::from_secs(5 * 60);
const PASS_BUDGET: Duration = Duration::from_secs(5);
const PASS_ENTRIES: usize = 64;
pub(crate) const TREE_ENTRIES: usize = 4096;
const TRASH_SCAN_HANDLES: usize = 64;
const HISTORY_ENTRIES: usize = 8192;
static REPORTED_BINDING_READ_ERRORS: OnceLock<Mutex<HashSet<PathBuf>>> = OnceLock::new();
const DOMAINS: [&str; 6] = [
    "views",
    "callgraph",
    "inspect",
    "index",
    "semantic",
    "symbols",
];

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub(crate) struct Binding {
    pub root: PathBuf,
    pub artifact_key: String,
    pub last_bound_ms: u64,
    #[serde(default)]
    volume: Option<(PathBuf, u64)>,
    #[serde(default)]
    stable_volume: Option<VolumeIdentity>,
    #[serde(default)]
    linked_worktree: bool,
}

#[derive(Clone, Debug, Default, Serialize)]
pub(crate) struct SweepReport {
    pub at_ms: u64,
    pub examined: usize,
    pub removed_roots: usize,
    pub removed_generations: usize,
    pub removed_bytes: u64,
    pub examined_blobs: usize,
    pub removed_blobs: usize,
    pub removed_blob_payload_bytes: u64,
    pub blob_reclaim_deferred: bool,
    pub examined_bindings: usize,
    pub pruned_bindings: usize,
    pub skipped_live: usize,
    pub skipped_recent: usize,
    pub skipped_mount_mismatch: usize,
    pub skipped_protected: usize,
    /// Caches kept because a fact needed to judge them could not be read.
    pub skipped_unknown: usize,
    pub unmeasured_removed_roots: usize,
    /// Binding records written before linked-worktree topology was recorded,
    /// upgraded in place while their checkout still existed.
    pub upgraded_bindings: usize,
    /// Keys whose every binding is past its grace with caches still on disk
    /// when the pass began.
    pub due_keys: usize,
    /// Vanished-checkout view folders still due after this pass. Protected
    /// folders remain in this count so a stalled backlog is visible in health.
    pub due_views_remaining: usize,
    /// Last background trash census, updated independently of the root sweep.
    pub trash: TrashReport,
    /// The share of this pass spent on keys already past their grace.
    pub due_first: DomainReport,
    /// The share of this pass spent on view folders with no registry member
    /// and no binding history.
    pub orphan_views: DomainReport,
    pub stopped_early: bool,
    pub cancelled: bool,
    pub errors: Vec<String>,
    /// View folders larger than one generation-cleanup pass examines, each
    /// with what that pass could not prove. Cleanup still shrinks them a
    /// bounded window at a time, keeping anything it cannot prove unshared.
    pub oversized_views: Vec<String>,
    /// Blob families this pass left for a later pass, each with the reason,
    /// such as `blobs/v3/<family>: reservation_busy`.
    pub skipped_families: Vec<String>,
    pub domains: BTreeMap<String, DomainReport>,
    /// Wall time by stage, including setup and blob work outside the root budget.
    pub timings_ms: BTreeMap<String, u64>,
}

#[derive(Clone, Debug, Default, Serialize)]
pub(crate) struct TrashReport {
    pub at_ms: u64,
    /// Remaining entries and logical bytes observed by a bounded, no-follow walk.
    pub entries: usize,
    pub bytes_estimate: u64,
    pub oldest_age_ms: Option<u64>,
    pub removed_entries: usize,
    /// The census could not examine all trash or measure every remaining tree.
    pub incomplete: bool,
    /// Counts kept entries, or deferred containers when admission or census failed.
    pub kept_by_reason: BTreeMap<String, usize>,
    /// Work examined by the pass's read-only durable-directory census.
    pub protection_census: crate::bash_background::persistence::IdentityCensus,
    pub protection_failure: Option<ProtectionFailure>,
    /// A failed protection decision remains visible while reaping backs off.
    pub protection_retry_at_ms: Option<u64>,
    /// The durable-storage census (directory identities no trash removal may
    /// touch), taken at most once per reaper pass and shared by every trash
    /// container that pass visits. Taking it again for each container spent
    /// the pass budget before later domains (`inspect` and after) were
    /// reached. Not part of the published report.
    #[serde(skip)]
    pub(crate) protections: Option<Result<Arc<Vec<(u64, u64)>>, Arc<io::Error>>>,
}

#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ProtectionFailure {
    Budget,
    Cancelled,
    Unavailable,
}

impl ProtectionFailure {
    fn from_error(error: &io::Error) -> Self {
        if error.kind() == io::ErrorKind::Interrupted {
            Self::Cancelled
        } else if crate::bash_background::persistence::is_repair_budget_stop(error) {
            Self::Budget
        } else {
            Self::Unavailable
        }
    }

    fn reason(self) -> &'static str {
        match self {
            Self::Budget => "protections_budget",
            Self::Cancelled => "protections_cancelled",
            Self::Unavailable => "protections_unavailable",
        }
    }
}

impl TrashReport {
    fn keep(&mut self, path: &Path, reason: &str, detail: impl std::fmt::Display) {
        *self.kept_by_reason.entry(reason.into()).or_default() += 1;
        crate::slog_warn!(
            "storage retention trash={} retained reason={reason}: {detail}",
            path.display()
        );
    }
}

#[derive(Clone, Debug, Default, Serialize)]
pub(crate) struct DomainReport {
    /// Root directory entries (or blob rows for the blobs domains) evaluated.
    pub examined: usize,
    pub skipped_recent: usize,
    pub skipped_live: usize,
    pub skipped_mount_mismatch: usize,
    pub skipped_protected: usize,
    pub skipped_unknown: usize,
    pub removed_roots: usize,
    pub removed_bytes: u64,
    pub unmeasured_removed_roots: usize,
}

impl DomainReport {
    fn from_report(report: &SweepReport) -> Self {
        Self {
            examined: report.examined,
            skipped_recent: report.skipped_recent,
            skipped_live: report.skipped_live,
            skipped_mount_mismatch: report.skipped_mount_mismatch,
            skipped_protected: report.skipped_protected,
            skipped_unknown: report.skipped_unknown,
            removed_roots: report.removed_roots,
            removed_bytes: report.removed_bytes,
            unmeasured_removed_roots: report.unmeasured_removed_roots,
        }
    }

    fn difference(&self, before: &Self) -> Self {
        Self {
            examined: self.examined - before.examined,
            skipped_recent: self.skipped_recent - before.skipped_recent,
            skipped_live: self.skipped_live - before.skipped_live,
            skipped_mount_mismatch: self.skipped_mount_mismatch - before.skipped_mount_mismatch,
            skipped_protected: self.skipped_protected - before.skipped_protected,
            skipped_unknown: self.skipped_unknown - before.skipped_unknown,
            removed_roots: self.removed_roots - before.removed_roots,
            removed_bytes: self.removed_bytes - before.removed_bytes,
            unmeasured_removed_roots: self.unmeasured_removed_roots
                - before.unmeasured_removed_roots,
        }
    }

    fn add(&mut self, other: &Self) {
        self.examined += other.examined;
        self.skipped_recent += other.skipped_recent;
        self.skipped_live += other.skipped_live;
        self.skipped_mount_mismatch += other.skipped_mount_mismatch;
        self.skipped_protected += other.skipped_protected;
        self.skipped_unknown += other.skipped_unknown;
        self.removed_roots += other.removed_roots;
        self.removed_bytes += other.removed_bytes;
        self.unmeasured_removed_roots += other.unmeasured_removed_roots;
    }
}

#[derive(Default)]
struct State {
    startup_observed_at: Option<Instant>,
    last_run: Option<Instant>,
    running: bool,
    report: Option<SweepReport>,
    /// The last key the due-first lane processed, so keys that stay protected
    /// cannot starve the ones after them.
    due_cursor: Option<String>,
    reaping: bool,
    trash: TrashReport,
    reaper_cancelled: Option<Arc<dyn Fn() -> bool + Send + Sync>>,
    protection_failures: u32,
    protection_retry: Option<Instant>,
}

impl State {
    fn record_trash(&mut self, mut report: TrashReport, now: Instant) {
        if report.protection_failure.is_some() {
            self.protection_failures = self.protection_failures.saturating_add(1);
            // Skip at least one scheduled pass after a failed census. Repeated
            // failures reach an hourly retry without spending CPU every interval.
            let delay = (INTERVAL * 2u32.saturating_pow(self.protection_failures.min(3)))
                .min(Duration::from_secs(60 * 60));
            self.protection_retry = Some(now + delay);
            report.protection_retry_at_ms =
                Some(crate::pins::now_ms().saturating_add(delay.as_millis() as u64));
        } else {
            self.protection_failures = 0;
            self.protection_retry = None;
        }
        self.trash = report.clone();
        if let Some(sweep) = &mut self.report {
            sweep.trash = report;
        }
    }

    fn reaper_ready(&self, now: Instant) -> bool {
        !self.reaping && self.protection_retry.is_none_or(|retry| now >= retry)
    }
}

struct TrashScan {
    identity: (u64, u64),
    entries: Option<ReadDir>,
    deferred: VecDeque<fs::DirEntry>,
    omitted_prefix: bool,
    touched: Instant,
}

type TrashScans = HashMap<(PathBuf, PathBuf), Option<TrashScan>>;

fn trash_scans() -> &'static Mutex<TrashScans> {
    static SCANS: OnceLock<Mutex<TrashScans>> = OnceLock::new();
    SCANS.get_or_init(Mutex::default)
}

struct TrashScanLease(Option<(PathBuf, PathBuf)>);

impl Drop for TrashScanLease {
    fn drop(&mut self) {
        if let Some(key) = self.0.take() {
            let cursor = trash_scans()
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .remove(&key);
            drop(cursor);
        }
    }
}

pub(crate) fn startup_sweeps_ready(storage: &Path) -> bool {
    startup_sweeps_ready_at(storage, Instant::now())
}

fn startup_sweeps_ready_at(storage: &Path, now: Instant) -> bool {
    let mut states = states()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let state = states.entry(storage.to_path_buf()).or_default();
    let started = *state.startup_observed_at.get_or_insert(now);
    now.saturating_duration_since(started) >= STARTUP_GRACE
}

fn states() -> &'static Mutex<HashMap<PathBuf, State>> {
    static STATES: OnceLock<Mutex<HashMap<PathBuf, State>>> = OnceLock::new();
    STATES.get_or_init(Default::default)
}

pub(crate) fn snapshot(storage: &Path) -> Option<SweepReport> {
    states().lock().ok()?.get(storage)?.report.clone()
}

#[cfg(test)]
pub(crate) type TestHook = Arc<dyn Fn(&str) + Send + Sync>;

#[cfg(test)]
pub(crate) struct TestHookGuard {
    path: PathBuf,
    previous: Option<TestHook>,
}

#[cfg(test)]
impl Drop for TestHookGuard {
    fn drop(&mut self) {
        let mut hooks = test_hooks().lock().unwrap();
        if let Some(previous) = self.previous.take() {
            hooks.insert(self.path.clone(), previous);
        } else {
            hooks.remove(&self.path);
        }
    }
}

#[cfg(test)]
pub(crate) fn observe_test_hook(path: &Path, hook: TestHook) -> TestHookGuard {
    let path = path.to_path_buf();
    let previous = test_hooks().lock().unwrap().insert(path.clone(), hook);
    TestHookGuard { path, previous }
}

#[cfg(test)]
fn test_hooks() -> &'static Mutex<HashMap<PathBuf, TestHook>> {
    static HOOKS: OnceLock<Mutex<HashMap<PathBuf, TestHook>>> = OnceLock::new();
    HOOKS.get_or_init(Default::default)
}

#[cfg(test)]
pub(crate) fn test_hook(path: &Path, step: &str) {
    let hook = test_hooks().lock().unwrap().get(path).cloned();
    if let Some(hook) = hook {
        hook(step);
    }
}

/// Publication-round hooks (`bind-pending`, `bind-protected`, ...) fire on the
/// canonical root a binding records. A hook registered under another spelling
/// of the same checkout, such as a macOS temp path below the `/var` symlink or
/// a Windows path without its `\\?\` verbatim prefix, would never run, and its
/// test would wait out a channel timeout far from the cause. Remove such a
/// hook, which drops and disconnects any channel it holds so the waiting test
/// fails at once, and panic naming both spellings.
#[cfg(test)]
fn reject_noncanonical_root_hook(spelled: &Path, canonical: &Path) {
    if spelled == canonical {
        return;
    }
    let removed = test_hooks().lock().unwrap().remove(spelled);
    if let Some(hook) = removed {
        drop(hook);
        panic!(
            "retention bind test hook registered on {} but publication hooks fire on its canonical root {}",
            spelled.display(),
            canonical.display()
        );
    }
}

#[cfg(test)]
pub(crate) fn allow_next_scheduled_pass_for_test(storage: &Path) {
    let mut states = states().lock().unwrap();
    let state = states.entry(storage.to_path_buf()).or_default();
    state.startup_observed_at = Some(Instant::now() - STARTUP_GRACE);
    assert!(
        !state.running,
        "previous scheduled pass must have completed"
    );
    state.last_run = None;
}

#[cfg(test)]
fn reaper_running_for_test(artifacts: &Path) -> bool {
    states()
        .lock()
        .unwrap()
        .get(artifacts)
        .is_some_and(|state| state.reaping)
}

#[cfg(test)]
pub(crate) fn scheduled_pass_finished_for_test(storage: &Path) -> bool {
    states()
        .lock()
        .unwrap()
        .get(storage)
        .is_some_and(|state| !state.running && state.report.is_some())
}

fn key_valid(key: &str) -> bool {
    key.len() == 16 && key.bytes().all(|byte| byte.is_ascii_hexdigit())
}

pub(crate) struct Barrier {
    _metadata: crate::fs_lock::LockGuard,
    _artifacts: crate::fs_lock::LockGuard,
}
pub(crate) fn barrier(storage: &Path) -> io::Result<crate::fs_lock::LockGuard> {
    crate::private_storage::create_dir_all(storage.join("retention"))?;
    let metadata = crate::fs_lock::try_acquire(
        &storage.join("retention/sweep.lock"),
        Duration::from_secs(5),
    )
    .map_err(|error| io::Error::other(error.to_string()))?;
    Ok(metadata)
}
pub(crate) fn cache_barrier(storage: &StoragePaths) -> io::Result<Barrier> {
    Ok(Barrier {
        _metadata: barrier(&storage.durable)?,
        _artifacts: artifact_barrier(&storage.artifacts)?,
    })
}

fn artifact_barrier(artifacts: &Path) -> io::Result<crate::fs_lock::LockGuard> {
    crate::root_cache::check_cache_access(artifacts)?;
    crate::private_storage::create_dir_all(artifacts)?;
    crate::fs_lock::try_acquire(
        &artifacts.join(".retention-sweep.lock"),
        Duration::from_secs(5),
    )
    .map_err(io::Error::other)
}

// Keep admission on sweep.lock so older binaries sharing this storage still
// serialize their final history checks with new bindings. The separate marker
// only signals a pending bind so cooperative sweepers yield; sweep.lock still
// controls admission for both old and new binaries.
pub(crate) fn bind_pending(storage: &Path) -> bool {
    let path = storage.join("retention/bind-pending.lock");
    if !path.exists() {
        return false;
    }
    // Reclaim a dead binder's marker instead of indefinitely disabling cleanup.
    crate::fs_lock::try_acquire(&path, Duration::ZERO).is_err()
}

fn yield_to_bind(storage: &Path) -> io::Result<()> {
    if storage.join("retention/bind-pending.lock").exists() {
        Err(io::Error::new(
            io::ErrorKind::Interrupted,
            "retention yielded to checkout binding",
        ))
    } else {
        Ok(())
    }
}

/// V1 has no ref-epoch handoff. Serialize pin admission with its marking pass.
/// V2 pins use the family registry's epoch/barrier protocol instead.
///
/// Pin creation takes both retention locks, through [`cache_barrier`]: the
/// durable root's `retention/sweep.lock` first, then the artifact root's
/// `.retention-sweep.lock`, the one order every path that takes both uses.
/// Older AFT releases sharing the storage root collect blobs holding only
/// `retention/sweep.lock`; a pin written without it could land after such a
/// collector read the pins and before it deleted blobs. With no separate
/// cache root both names are under the same root but are different files, so
/// nothing is locked twice.
pub(crate) fn pin_barrier(view_dir: &Path) -> io::Result<Option<Barrier>> {
    let Some(artifacts) = view_dir
        .parent()
        .filter(|parent| parent.file_name().is_some_and(|name| name == "views"))
        .and_then(Path::parent)
    else {
        return Ok(None);
    };
    // A prepared cache root resolves to the durable root that owns it; any
    // other root is its own durable root.
    let durable = crate::root_cache::identity_storage_root(artifacts).into_owned();
    cache_barrier(&StoragePaths::new(durable, artifacts.to_path_buf())).map(Some)
}

fn atomic_json(path: &Path, value: &impl Serialize) -> io::Result<()> {
    crate::backup::create_private_durable_dir(
        path.parent()
            .ok_or_else(|| io::Error::other("missing retention parent"))?,
    )?;
    crate::durability::atomic_write(
        path,
        &serde_json::to_vec(value).map_err(io::Error::other)?,
        crate::durability::AtomicWriteDurability::FileAndDirectory,
    )
}

/// Called before cache acquisition, on every configure, even a same-key rebind.
pub(crate) fn record_bind(
    storage: &StoragePaths,
    root: &Path,
    artifact_key: &str,
) -> io::Result<()> {
    // Cache-root enrollment completes before binding. Binding history and unknown
    // observations belong to the durable owner, even when artifacts live elsewhere;
    // publish both in the group round rather than reacquiring its lock per caller.
    record_bind_until(
        &storage.durable,
        root,
        artifact_key,
        Instant::now() + BIND_QUEUE_WAIT,
    )
}

// Queueing, sweep admission and durable publication have different remedies.
// Keep their wall times separate without changing lock or sync boundaries.
struct BindProfile<'a> {
    storage: &'a Path,
    root: &'a Path,
    started: Instant,
    step_started: Instant,
    step: &'static str,
    phases: Vec<(&'static str, std::time::Duration)>,
}

#[cfg(test)]
thread_local! {
    static LAST_BIND_PROFILE: std::cell::RefCell<Option<String>> = const { std::cell::RefCell::new(None) };
}

impl<'a> BindProfile<'a> {
    fn new(storage: &'a Path, root: &'a Path) -> Self {
        let started = Instant::now();
        Self {
            storage,
            root,
            started,
            step_started: started,
            step: "metadata",
            phases: Vec::new(),
        }
    }

    fn begin(&mut self, step: &'static str) {
        let now = Instant::now();
        self.phases.push((self.step, now - self.step_started));
        self.step_started = now;
        self.step = step;
    }

    fn line(&self, total: std::time::Duration) -> String {
        let phases = self
            .phases
            .iter()
            .copied()
            .chain(std::iter::once((self.step, self.step_started.elapsed())))
            .map(|(name, duration)| format!("{name}={}", duration.as_micros()))
            .collect::<Vec<_>>()
            .join(",");
        format!(
            "retention_bind_profile storage={} root={} total_ms={} phases_us=[{}]",
            self.storage.display(),
            self.root.display(),
            total.as_millis(),
            phases
        )
    }
}

impl Drop for BindProfile<'_> {
    fn drop(&mut self) {
        let total = self.started.elapsed();
        if total >= std::time::Duration::from_secs(1) {
            crate::slog_info!("{}", self.line(total));
        }
        #[cfg(test)]
        {
            let line = self.line(total);
            LAST_BIND_PROFILE.with(|slot| *slot.borrow_mut() = Some(line.clone()));
            if std::env::var_os("AFT_TEST_BIND_PROFILE").is_some() {
                eprintln!("{line}");
            }
        }
    }
}

// Budget one active publication plus this caller's publication, allowing five
// seconds for each and two seconds for collection/scheduling overhead. All
// checks use the caller's original deadline, avoiding a separate, shorter
// publication timeout.
const BIND_ROUND_ALLOWANCE: Duration = Duration::from_secs(5);
const BIND_QUEUE_WAIT: Duration = Duration::from_secs(2 * BIND_ROUND_ALLOWANCE.as_secs() + 2);
const BIND_COLLECT: Duration = Duration::from_millis(10);

#[derive(Clone, Debug)]
pub(crate) struct BindLockError {
    pub(crate) path: PathBuf,
    pub(crate) waited: Duration,
    publication: bool,
    source: Arc<crate::fs_lock::AcquireError>,
}

impl std::fmt::Display for BindLockError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.publication {
            return write!(
                f,
                "retention binding publication round for {} exceeded its deadline after {}ms",
                self.path.display(),
                self.waited.as_millis()
            );
        }
        write!(
            f,
            "retention binding waited {}ms for {}: {}",
            self.waited.as_millis(),
            self.path.display(),
            self.source
        )
    }
}

impl std::error::Error for BindLockError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(self.source.as_ref())
    }
}

#[derive(Clone, Debug)]
struct BindRoundError(Arc<io::Error>);

impl BindRoundError {
    fn into_io(self) -> io::Error {
        if let Some(wait) = self
            .0
            .get_ref()
            .and_then(|error| error.downcast_ref::<BindLockError>())
        {
            io::Error::new(self.0.kind(), wait.clone())
        } else {
            io::Error::new(self.0.kind(), self)
        }
    }
}

impl std::fmt::Display for BindRoundError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

impl std::error::Error for BindRoundError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(self.0.as_ref())
    }
}

type BindResult = Result<(), BindRoundError>;

struct PendingBind {
    binding: Binding,
    started: Instant,
    deadline: Instant,
    result: Mutex<Option<BindResult>>,
}

#[derive(Default)]
struct BindQueueState {
    active: bool,
    pending: Vec<Arc<PendingBind>>,
}

#[derive(Default)]
struct BindQueue {
    state: Mutex<BindQueueState>,
    changed: Condvar,
}

fn bind_queue(storage: &Path) -> io::Result<Arc<BindQueue>> {
    static QUEUES: OnceLock<Mutex<HashMap<PathBuf, Weak<BindQueue>>>> = OnceLock::new();
    let key = fs::canonicalize(storage)?;
    let mut queues = QUEUES.get_or_init(Default::default).lock().unwrap();
    queues.retain(|_, queue| queue.strong_count() != 0);
    if let Some(queue) = queues.get(&key).and_then(Weak::upgrade) {
        return Ok(queue);
    }
    let queue = Arc::new(BindQueue::default());
    queues.insert(key, Arc::downgrade(&queue));
    Ok(queue)
}

// Always hand leadership back after one snapshot. Otherwise a continuous stream
// of new roots could keep the leader inside record_bind after its own success.
struct BindLeader<'a> {
    queue: &'a BindQueue,
    members: Vec<Arc<PendingBind>>,
    result: BindResult,
}

impl Drop for BindLeader<'_> {
    fn drop(&mut self) {
        let mut state = self.queue.state.lock().unwrap();
        for member in &self.members {
            let mut result = member.result.lock().unwrap();
            if result.is_none() {
                *result = Some(self.result.clone());
            }
        }
        state.active = false;
        self.queue.changed.notify_all();
    }
}

fn bind_timeout(path: &Path, started: Instant) -> io::Error {
    io::Error::new(
        io::ErrorKind::TimedOut,
        BindLockError {
            path: path.to_path_buf(),
            waited: started.elapsed(),
            publication: true,
            source: Arc::new(crate::fs_lock::AcquireError::Timeout),
        },
    )
}

fn retain_live_binds(members: &mut Vec<Arc<PendingBind>>, path: &Path) -> Option<Instant> {
    let now = Instant::now();
    members.retain(|member| {
        let mut result = member.result.lock().unwrap();
        if result.is_some() {
            return false;
        }
        if member.deadline <= now {
            *result = Some(Err(BindRoundError(Arc::new(bind_timeout(
                path,
                member.started,
            )))));
            return false;
        }
        true
    });
    members.iter().map(|member| member.deadline).min()
}

fn bind_lock_error(
    path: &Path,
    started: Instant,
    source: crate::fs_lock::AcquireError,
) -> io::Error {
    let kind = match &source {
        crate::fs_lock::AcquireError::Timeout => io::ErrorKind::TimedOut,
        crate::fs_lock::AcquireError::Io(error) => error.kind(),
    };
    io::Error::new(
        kind,
        BindLockError {
            path: path.to_path_buf(),
            waited: started.elapsed(),
            publication: false,
            source: Arc::new(source),
        },
    )
}

// Every lock needed by one publication round must share this absolute deadline;
// a fresh timeout for each preceding admission lock would extend the round.
fn bind_lock_until(
    path: &Path,
    deadline: Instant,
    started: Instant,
) -> io::Result<crate::fs_lock::LockGuard> {
    crate::fs_lock::try_acquire_until(path, deadline)
        .map_err(|source| bind_lock_error(path, started, source))
}

pub(crate) fn record_bind_until(
    storage: &Path,
    root: &Path,
    artifact_key: &str,
    deadline: Instant,
) -> io::Result<()> {
    let mut profile = BindProfile::new(storage, root);
    #[cfg(test)]
    test_hook(root, "bind-waiting");
    let started = Instant::now();
    fs::create_dir_all(storage.join("retention"))?;
    let path = storage.join("retention/sweep.lock");
    let spelled_root = root;
    let root = fs::canonicalize(spelled_root)?;
    #[cfg(test)]
    reject_noncanonical_root_hook(spelled_root, &root);
    let member = Arc::new(PendingBind {
        binding: Binding {
            volume: volume_anchor(&root),
            stable_volume: Some(VolumeIdentity {
                uuid: volume_uuid(&root),
                boot_id: boot_id(),
            }),
            linked_worktree: linked_worktree_at_bind(&root),
            root,
            artifact_key: artifact_key.to_owned(),
            last_bound_ms: crate::pins::now_ms(),
        },
        started,
        deadline,
        result: Mutex::new(None),
    });
    if Instant::now() >= deadline {
        return Err(bind_timeout(&path, started));
    }
    let queue = bind_queue(storage)?;
    profile.begin("queue_wait");
    {
        let mut state = queue.state.lock().unwrap();
        state.pending.push(member.clone());
        queue.changed.notify_all();
    }
    #[cfg(test)]
    test_hook(storage, "bind-enqueued");
    let mut state = queue.state.lock().unwrap();
    loop {
        if let Some(result) = member.result.lock().unwrap().clone() {
            return result.map_err(BindRoundError::into_io);
        }
        if Instant::now() >= deadline {
            let error = BindRoundError(Arc::new(bind_timeout(&path, started)));
            *member.result.lock().unwrap() = Some(Err(error.clone()));
            state
                .pending
                .retain(|pending| !Arc::ptr_eq(pending, &member));
            queue.changed.notify_all();
            return Err(error.into_io());
        }
        if !state.active {
            profile.begin("collect_wait");
            state.active = true;
            drop(state);
            let mut leader = BindLeader {
                queue: &queue,
                members: Vec::new(),
                result: Err(BindRoundError(Arc::new(io::Error::new(
                    io::ErrorKind::Interrupted,
                    "retention binding publication leader stopped",
                )))),
            };
            let round_started = Instant::now();
            #[cfg(test)]
            test_hook(storage, "bind-round-collecting");
            state = queue.state.lock().unwrap();
            let collect_until = (round_started + BIND_COLLECT).min(deadline);
            while Instant::now() < collect_until {
                let remaining = collect_until.saturating_duration_since(Instant::now());
                state = queue.changed.wait_timeout(state, remaining).unwrap().0;
            }
            leader.members = std::mem::take(&mut state.pending);
            // A caller may expire before its thread runs again. Give it its own
            // timeout and exclude it from both publication and deadline choice.
            let round_deadline = retain_live_binds(&mut leader.members, &path);
            queue.changed.notify_all();
            drop(state);
            let Some(round_deadline) = round_deadline else {
                drop(leader);
                state = queue.state.lock().unwrap();
                continue;
            };
            // One marker covers the entire publication round, including the
            // wait for a sweeper to release admission. Followers share it rather
            // than serializing on a marker owned by an individual caller.
            profile.begin("admission_wait");
            let admission = bind_lock_until(
                &storage.join("retention/bind-pending.lock"),
                round_deadline,
                round_started,
            )
            .and_then(|pending| {
                #[cfg(test)]
                for member in &leader.members {
                    test_hook(&member.binding.root, "bind-pending");
                }
                #[cfg(test)]
                test_hook(storage, "bind-round-pending");
                bind_lock_until(&path, round_deadline, round_started)
                    .map(|barrier| (pending, barrier))
            });
            #[cfg(test)]
            if admission.is_ok() {
                test_hook(storage, "bind-round-admitted");
            }
            state = queue.state.lock().unwrap();
            profile.begin("freeze_round");
            // Include arrivals during cross-process admission, then freeze the
            // publication snapshot. Later arrivals belong to the next round.
            leader.members.append(&mut state.pending);
            // Admission can outlast a member, including the leader. Recompute
            // from live members instead of retaining an expired earlier minimum.
            let round_deadline = retain_live_binds(&mut leader.members, &path);
            queue.changed.notify_all();
            drop(state);
            leader.result = if let Some(round_deadline) = round_deadline {
                admission
                    .and_then(|(pending, barrier)| {
                        let result = publish_bind_round(
                            storage,
                            &leader.members,
                            round_started,
                            round_deadline,
                            barrier,
                            &mut profile,
                        );
                        drop(pending);
                        result
                    })
                    .map_err(|error| BindRoundError(Arc::new(error)))
            } else {
                drop(admission);
                Ok(())
            };
            drop(leader);
            state = queue.state.lock().unwrap();
        } else {
            let remaining = deadline.saturating_duration_since(Instant::now());
            state = queue.changed.wait_timeout(state, remaining).unwrap().0;
        }
    }
}

struct StagedBinding {
    temporary: PathBuf,
    destination: PathBuf,
}

impl Drop for StagedBinding {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.temporary);
    }
}

fn publish_bind_round(
    storage: &Path,
    members: &[Arc<PendingBind>],
    started: Instant,
    deadline: Instant,
    barrier: crate::fs_lock::LockGuard,
    profile: &mut BindProfile<'_>,
) -> io::Result<()> {
    profile.begin("prepare_directory");
    let lock_path = storage.join("retention/sweep.lock");
    #[cfg(test)]
    for member in members {
        test_hook(&member.binding.root, "bind-protected");
        test_hook(&member.binding.root, "bind-acquired");
    }
    #[cfg(test)]
    test_hook(storage, "bind-round-acquired");
    let check_deadline = || {
        if Instant::now() >= deadline {
            Err(bind_timeout(&lock_path, started))
        } else {
            Ok(())
        }
    };
    check_deadline()?;
    let directory = storage.join("retention/roots");
    crate::backup::create_private_durable_dir(&directory)?;
    profile.begin("stage_files");
    let mut staged = Vec::new();
    for member in members {
        check_deadline()?;
        let mut binding = member.binding.clone();
        binding.last_bound_ms = crate::pins::now_ms();
        let scope = crate::path_identity::project_scope_key(&binding.root);
        let mut random = [0_u8; 16];
        getrandom::fill(&mut random).map_err(io::Error::other)?;
        let temporary = directory.join(format!(".bind-{:032x}.tmp", u128::from_ne_bytes(random)));
        #[cfg(test)]
        crate::durability::before_temp_create(&temporary);
        let mut file = crate::private_storage::options()
            .write(true)
            .create_new(true)
            .open(&temporary)?;
        staged.push(StagedBinding {
            temporary,
            destination: directory.join(format!("{scope}.json")),
        });
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            file.set_permissions(fs::Permissions::from_mode(
                crate::private_storage::FILE_MODE,
            ))?;
        }
        file.write_all(&serde_json::to_vec(&binding).map_err(io::Error::other)?)?;
        // Do not hold one open handle per queued root: another configure may
        // need file descriptors for its identity probes while this round runs.
        drop(file);
    }
    #[cfg(test)]
    test_hook(storage, "bind-round-staged");
    // Each data file needs its own sync; syncing a directory cannot flush file
    // contents. Group the shared directory commit instead of doing it per root.
    profile.begin("sync_files");
    for binding in &staged {
        check_deadline()?;
        let file = fs::OpenOptions::new()
            .write(true)
            .open(&binding.temporary)?;
        crate::durability::sync_file(&file, &binding.destination)?;
    }
    profile.begin("replace_files");
    for binding in &staged {
        check_deadline()?;
        crate::fs_lock::rename_over(&binding.temporary, &binding.destination)?;
        crate::durability::record(
            crate::durability::EventKind::AtomicReplace,
            &binding.destination,
        );
    }
    profile.begin("sync_directory");
    crate::durability::sync_dir(&directory)?;
    #[cfg(test)]
    test_hook(storage, "bind-round-durable");

    profile.begin("clear_unknown");
    let mut cleared = false;
    let keys: HashSet<_> = members
        .iter()
        .map(|member| member.binding.artifact_key.as_str())
        .filter(|key| key_valid(key))
        .collect();
    let observations = storage.join("retention/unknown");
    for key in keys {
        check_deadline()?;
        match fs::remove_file(observations.join(format!("{key}.json"))) {
            Ok(()) => cleared = true,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
    }
    if cleared {
        crate::durability::sync_dir(&observations)?;
    }
    check_deadline()?;
    // A late failure can leave complete records that keep caches from cleanup
    // until the bindings reach ordinary expiry. No record_bind caller gets success
    // until every file and shared directory commit has succeeded.
    drop(barrier);
    #[cfg(test)]
    for member in members {
        test_hook(&member.binding.root, "bind-released");
    }
    Ok(())
}

/// Only a verified linked-worktree Git layout gets the shorter retention age.
/// A Git file alone can also describe a submodule or a separate main Git dir.
fn linked_worktree_at_bind(root: &Path) -> bool {
    let marker = root.join(".git");
    if !fs::symlink_metadata(&marker).is_ok_and(|metadata| metadata.file_type().is_file()) {
        return false;
    }
    let Some(git_dir) = fs::read_to_string(&marker).ok().and_then(|text| {
        let path = text.strip_prefix("gitdir:")?.trim();
        (!path.is_empty())
            .then(|| fs::canonicalize(root.join(path)).ok())
            .flatten()
    }) else {
        return false;
    };
    let Some(common) = fs::read_to_string(git_dir.join("commondir"))
        .ok()
        .and_then(|text| fs::canonicalize(git_dir.join(text.trim())).ok())
    else {
        return false;
    };
    let backlink = fs::read_to_string(git_dir.join("gitdir"))
        .ok()
        .and_then(|text| fs::canonicalize(git_dir.join(text.trim())).ok());
    git_dir.is_dir()
        && !common.starts_with(root)
        && git_dir.parent() == Some(common.join("worktrees").as_path())
        && backlink.as_deref() == Some(marker.as_path())
}

#[cfg(unix)]
fn device(path: &Path) -> Option<u64> {
    use std::os::unix::fs::MetadataExt;
    fs::metadata(path).ok().map(|metadata| metadata.dev())
}
#[cfg(windows)]
fn device(path: &Path) -> Option<u64> {
    use std::os::windows::ffi::OsStrExt;
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetVolumePathNameW(path: *const u16, volume: *mut u16, length: u32) -> i32;
        fn GetVolumeInformationW(
            volume: *const u16,
            name: *mut u16,
            name_length: u32,
            serial: *mut u32,
            component_length: *mut u32,
            flags: *mut u32,
            filesystem: *mut u16,
            filesystem_length: u32,
        ) -> i32;
    }
    // A deep cache parent is past the plain-path limit of GetVolumePathNameW.
    let path = crate::windows_path::win32_call_path(path)
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect::<Vec<_>>();
    let mut volume = vec![0_u16; 32768];
    let mut serial = 0;
    unsafe {
        if GetVolumePathNameW(path.as_ptr(), volume.as_mut_ptr(), volume.len() as u32) == 0 {
            return None;
        }
        if GetVolumeInformationW(
            volume.as_ptr(),
            std::ptr::null_mut(),
            0,
            &mut serial,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            0,
        ) == 0
        {
            return None;
        }
    }
    Some(u64::from(serial))
}
#[cfg(not(any(unix, windows)))]
fn device(_path: &Path) -> Option<u64> {
    None
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
struct VolumeIdentity {
    uuid: Option<String>,
    boot_id: Option<String>,
}

#[cfg(target_os = "macos")]
fn mount_point(path: &Path) -> Option<PathBuf> {
    use std::ffi::{CStr, CString};
    use std::os::unix::ffi::OsStrExt;
    let path = CString::new(path.as_os_str().as_bytes()).ok()?;
    let mut info = std::mem::MaybeUninit::<libc::statfs>::uninit();
    // statfs returns the actual mount, including the APFS Data volume behind
    // firmlinks; walking lexical parents can instead end at the system snapshot.
    let info = unsafe {
        if libc::statfs(path.as_ptr(), info.as_mut_ptr()) != 0 {
            return None;
        }
        info.assume_init()
    };
    let bytes = unsafe { CStr::from_ptr(info.f_mntonname.as_ptr()) }.to_bytes();
    Some(PathBuf::from(std::ffi::OsStr::from_bytes(bytes)))
}

#[cfg(target_os = "macos")]
fn volume_uuid(path: &Path) -> Option<String> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    let path = CString::new(path.as_os_str().as_bytes()).ok()?;
    let mut attributes = libc::attrlist {
        bitmapcount: 5,
        reserved: 0,
        commonattr: 0,
        volattr: 0x8000_0000 | 0x0004_0000, // ATTR_VOL_INFO | ATTR_VOL_UUID
        dirattr: 0,
        fileattr: 0,
        forkattr: 0,
    };
    // getattrlist packs a four-byte length followed by the 16-byte volume UUID.
    let mut buffer = [0_u8; 20];
    let result = unsafe {
        libc::getattrlist(
            path.as_ptr(),
            (&mut attributes as *mut libc::attrlist).cast(),
            buffer.as_mut_ptr().cast(),
            buffer.len(),
            0,
        )
    };
    if result != 0
        || u32::from_ne_bytes(buffer[..4].try_into().ok()?) != 20
        || buffer[4..].iter().all(|b| *b == 0)
    {
        return None;
    }
    Some(buffer[4..].iter().map(|b| format!("{b:02x}")).collect())
}

#[cfg(target_os = "linux")]
fn volume_uuid(path: &Path) -> Option<String> {
    use std::os::unix::fs::MetadataExt;
    let dev = device(path)?;
    for entry in fs::read_dir("/dev/disk/by-uuid").ok()? {
        let entry = entry.ok()?;
        if fs::metadata(entry.path()).ok()?.rdev() == dev {
            return entry.file_name().to_str().map(str::to_owned);
        }
    }
    None
}

#[cfg(windows)]
fn volume_uuid(path: &Path) -> Option<String> {
    device(path).map(|serial| format!("windows-volume-serial:{serial}"))
}

#[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
fn volume_uuid(_path: &Path) -> Option<String> {
    None
}

#[cfg(target_os = "macos")]
pub(crate) fn boot_id() -> Option<String> {
    let mut buffer = [0_u8; 128];
    let mut size = buffer.len();
    let result = unsafe {
        libc::sysctlbyname(
            c"kern.bootsessionuuid".as_ptr(),
            buffer.as_mut_ptr().cast(),
            &mut size,
            std::ptr::null_mut(),
            0,
        )
    };
    if result != 0 || size > buffer.len() {
        return None;
    }
    std::str::from_utf8(&buffer[..size])
        .ok()
        .map(|s| s.trim_end_matches('\0').to_owned())
        .filter(|s| !s.is_empty())
}

#[cfg(target_os = "linux")]
fn boot_id() -> Option<String> {
    fs::read_to_string("/proc/sys/kernel/random/boot_id")
        .ok()
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn boot_id() -> Option<String> {
    None
}

pub(crate) fn volume_anchor(root: &Path) -> Option<(PathBuf, u64)> {
    #[cfg(target_os = "macos")]
    {
        let anchor = mount_point(root)?;
        return Some((anchor.clone(), device(&anchor)?));
    }
    #[cfg(not(target_os = "macos"))]
    {
        let dev = device(root)?;
        let mut anchor = root;
        while let Some(parent) = anchor.parent() {
            if device(parent) != Some(dev) {
                break;
            }
            anchor = parent;
        }
        Some((anchor.to_path_buf(), dev))
    }
}
pub(crate) fn cache_volume_uuid(root: &Path) -> Option<String> {
    volume_uuid(root)
}

fn mount_matches(binding: &Binding) -> bool {
    #[cfg(target_os = "macos")]
    let boot_volume = mount_point(Path::new("/")).as_deref() == Some(Path::new("/"));
    #[cfg(not(target_os = "macos"))]
    let boot_volume = false;
    #[cfg(any(test, debug_assertions))]
    let uuid_at = |path: &Path| {
        let probe_path = fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
        if let Some(uuid) = test_volume_uuids()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&probe_path)
        {
            return Some(uuid.clone());
        }
        volume_uuid(path)
    };
    #[cfg(not(any(test, debug_assertions)))]
    let uuid_at = volume_uuid;
    mount_matches_with(
        binding,
        &uuid_at,
        &device,
        boot_id().as_deref(),
        boot_volume,
    )
}

#[cfg(any(test, debug_assertions))]
fn test_volume_uuids() -> &'static Mutex<HashMap<PathBuf, String>> {
    static UUIDS: OnceLock<Mutex<HashMap<PathBuf, String>>> = OnceLock::new();
    UUIDS.get_or_init(Default::default)
}

/// A scoped mounted-volume identity for debug-build integration fixtures.
/// No environment variable can install an override, and release builds have
/// neither this guard nor the override lookup.
#[cfg(debug_assertions)]
#[doc(hidden)]
#[derive(Debug)]
pub struct TestVolumeUuidGuard {
    anchor: PathBuf,
    previous: Option<String>,
}

#[cfg(debug_assertions)]
impl Drop for TestVolumeUuidGuard {
    fn drop(&mut self) {
        let mut uuids = test_volume_uuids()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(previous) = self.previous.take() {
            uuids.insert(self.anchor.clone(), previous);
        } else {
            uuids.remove(&self.anchor);
        }
    }
}

#[cfg(debug_assertions)]
pub(crate) fn test_volume_uuid_guard(
    storage: &Path,
    anchor: &Path,
    uuid: &str,
) -> io::Result<TestVolumeUuidGuard> {
    let storage = fs::canonicalize(storage)?;
    let anchor = fs::canonicalize(anchor)?;
    if !anchor.starts_with(&storage) || uuid.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "a test volume identity needs a named anchor inside its storage root",
        ));
    }
    let previous = test_volume_uuids()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(anchor.clone(), uuid.to_owned());
    Ok(TestVolumeUuidGuard { anchor, previous })
}

fn mount_matches_with(
    binding: &Binding,
    uuid_at: &dyn Fn(&Path) -> Option<String>,
    device_at: &dyn Fn(&Path) -> Option<u64>,
    current_boot: Option<&str>,
    boot_volume: bool,
) -> bool {
    let Some((anchor, dev)) = &binding.volume else {
        // A missing path cannot prove its original volume is mounted. Keep
        // records without volume evidence until a bind records that identity.
        return false;
    };
    if let Some(identity) = &binding.stable_volume {
        if let Some(uuid) = &identity.uuid {
            return uuid_at(anchor).as_ref() == Some(uuid)
                || binding
                    .root
                    .parent()
                    .is_some_and(|parent| uuid_at(parent).as_ref() == Some(uuid));
        }
        // Without a UUID, a device number is useful only within the recorded boot.
        return identity.boot_id.is_some()
            && identity.boot_id.as_deref() == current_boot
            && device_at(anchor) == Some(*dev);
    }
    if boot_volume && anchor == Path::new("/") {
        // Legacy boot-volume bindings cannot represent an unmounted external
        // disk. The running system snapshot may receive a new st_dev at reboot.
        return true;
    }
    // Legacy device numbers can be reused after reboot or media replacement.
    // Only the proven boot-volume exception above can expire a legacy record.
    false
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RootEligibility {
    Due,
    Live,
    Recent,
    MountMismatch,
}

fn binding_eligibility(binding: &Binding, now: u64) -> RootEligibility {
    binding_eligibility_with_mount(binding, now, mount_matches(binding))
}

fn binding_eligibility_with_mount(binding: &Binding, now: u64, mounted: bool) -> RootEligibility {
    if !matches!(binding.root.try_exists(), Ok(false)) {
        return RootEligibility::Live;
    }
    if !mounted {
        return RootEligibility::MountMismatch;
    }
    let grace = if binding.linked_worktree {
        LINKED_WORKTREE_REBIND_MARGIN_MS
    } else {
        ROOT_GRACE_MS
    };
    if now.saturating_sub(binding.last_bound_ms) < grace {
        RootEligibility::Recent
    } else {
        RootEligibility::Due
    }
}

fn missing_and_old(binding: &Binding, now: u64) -> bool {
    binding_eligibility(binding, now) == RootEligibility::Due
}

pub(crate) fn missing_root_due(storage: &Path, root: &Path, last_bind_ms: u64) -> bool {
    missing_root_eligibility(storage, root, last_bind_ms) == RootEligibility::Due
}

pub(crate) fn missing_root_eligibility(
    storage: &Path,
    root: &Path,
    last_bind_ms: u64,
) -> RootEligibility {
    let root = crate::root_cache::canonical_root(root);
    let scope = missing_root_scope_key(&root);
    let mut binding = read_json::<Binding>(&storage.join(format!("retention/roots/{scope}.json")))
        .unwrap_or_else(|_| Binding {
            root,
            artifact_key: String::new(),
            last_bound_ms: last_bind_ms,
            volume: None,
            stable_volume: None,
            linked_worktree: false,
        });
    binding.last_bound_ms = binding.last_bound_ms.max(last_bind_ms);
    binding_eligibility(&binding, crate::pins::now_ms())
}

// A missing checkout still needs the identity spelling used when it was bound.
// In particular, Windows fs::canonicalize returns a verbatim (\\?\) path while
// ProjectRootId uses its normalized identity spelling. The lexical fallback for
// a now-missing verbatim path otherwise hashes a different key and misses the
// durable binding. Resolve an existing ancestor through the same identity API,
// then append the missing components without trying to canonicalize them.
fn missing_root_scope_key(root: &Path) -> String {
    let mut ancestor = root;
    let mut missing = Vec::new();
    loop {
        if let Ok(identity) = crate::path_identity::ProjectRootId::from_path(ancestor) {
            let mut normalized = identity.into_path_buf();
            for component in missing.iter().rev() {
                normalized.push(component);
            }
            return crate::path_identity::project_scope_key(&normalized);
        }
        let Some(component) = ancestor.file_name() else {
            return crate::path_identity::project_scope_key(root);
        };
        missing.push(component.to_os_string());
        let Some(parent) = ancestor.parent() else {
            return crate::path_identity::project_scope_key(root);
        };
        ancestor = parent;
    }
}

struct BindingReadError {
    error: io::Error,
    truncated: bool,
    digest: Option<[u8; 32]>,
}

fn read_binding_record(path: &Path) -> Result<(serde_json::Value, Binding), BindingReadError> {
    let metadata = fs::metadata(path).map_err(|error| BindingReadError {
        error,
        truncated: false,
        digest: None,
    })?;
    if metadata.len() > 8 * 1024 * 1024 {
        return Err(BindingReadError {
            error: io::Error::other("retention metadata exceeds 8 MiB"),
            truncated: false,
            digest: None,
        });
    }
    let bytes = fs::read(path).map_err(|error| BindingReadError {
        error,
        truncated: false,
        digest: None,
    })?;
    let record =
        serde_json::from_slice::<serde_json::Value>(&bytes).map_err(|error| BindingReadError {
            truncated: error.is_eof() && !might_be_versioned_binding(&bytes),
            digest: (error.is_eof() && !might_be_versioned_binding(&bytes))
                .then(|| *blake3::hash(&bytes).as_bytes()),
            error: io::Error::other(error),
        })?;
    if let Some(version) = record.get("schema_version") {
        return Err(BindingReadError {
            error: io::Error::other(format!(
                "root binding declares an unknown schema_version {version}"
            )),
            truncated: false,
            digest: None,
        });
    }
    // Root bindings have no reader-floor entry. Preserve unknown fields rather
    // than letting an older binary erase a record with a newer shape.
    if let Some(field) = unknown_binding_field(&record) {
        return Err(BindingReadError {
            error: io::Error::other(format!("root binding contains unknown field {field:?}")),
            truncated: false,
            digest: None,
        });
    }
    let binding = serde_json::from_value(record.clone()).map_err(|error| BindingReadError {
        error: io::Error::other(error),
        truncated: false,
        digest: None,
    })?;
    clear_logged_binding_error(path);
    Ok((record, binding))
}

fn might_be_versioned_binding(bytes: &[u8]) -> bool {
    [b"schema".as_slice(), b"version".as_slice()]
        .into_iter()
        .any(|marker| bytes.windows(marker.len()).any(|window| window == marker))
}

fn unknown_binding_field(record: &serde_json::Value) -> Option<&str> {
    let fields = record.as_object()?;
    fields.keys().find_map(|field| {
        (!matches!(
            field.as_str(),
            "root"
                | "artifact_key"
                | "last_bound_ms"
                | "volume"
                | "stable_volume"
                | "linked_worktree"
        ))
        .then_some(field.as_str())
    })
}

fn report_binding_read_error_once(path: &Path, message: &str) {
    let mut reported = REPORTED_BINDING_READ_ERRORS
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if reported.len() >= HISTORY_ENTRIES {
        reported.clear();
    }
    if reported.insert(path.to_path_buf()) {
        crate::slog_warn!(
            "storage retention skipped unreadable root binding {}: {message}",
            path.display()
        );
    }
}

fn clear_logged_binding_error(path: &Path) {
    if let Some(reported) = REPORTED_BINDING_READ_ERRORS.get() {
        reported
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(path);
    }
}

fn quarantine_truncated_binding(
    storage: &Path,
    path: &Path,
    expected_digest: [u8; 32],
) -> io::Result<PathBuf> {
    let current = fs::read(path)?;
    if blake3::hash(&current).as_bytes() != &expected_digest {
        return Err(io::Error::other("root binding changed before quarantine"));
    }
    let quarantine_root = storage.join("retention/quarantine/roots");
    crate::backup::create_private_durable_dir(&quarantine_root)?;
    let scope = path
        .file_stem()
        .and_then(|name| name.to_str())
        .ok_or_else(|| io::Error::other("root binding has no UTF-8 scope name"))?;
    let destination = quarantine_root.join(format!("{scope}.truncated.json"));
    crate::durability::atomic_write(
        &destination,
        &current,
        crate::durability::AtomicWriteDurability::FileAndDirectory,
    )?;
    fs::remove_file(path)?;
    Ok(destination)
}

fn handle_binding_read_error(
    storage: &Path,
    path: &Path,
    error: BindingReadError,
    quarantine_truncated: bool,
    barrier_held: bool,
) {
    let quarantine = if error.truncated && quarantine_truncated {
        let expected_digest = error.digest.expect("truncated binding digest");
        if barrier_held {
            quarantine_truncated_binding(storage, path, expected_digest)
        } else {
            barrier(storage)
                .and_then(|_barrier| quarantine_truncated_binding(storage, path, expected_digest))
        }
    } else {
        return report_binding_read_error_once(path, &error.error.to_string());
    };
    let message = match quarantine {
        Ok(destination) => format!(
            "{}; truncated record moved to {}",
            error.error,
            destination.display()
        ),
        Err(quarantine_error) => {
            format!("{}; quarantine failed: {quarantine_error}", error.error)
        }
    };
    report_binding_read_error_once(path, &message);
}

fn unreadable_binding_sentinel(storage: &Path, scope: &str) -> Binding {
    Binding {
        root: storage.join("retention/roots"),
        artifact_key: scope.to_owned(),
        last_bound_ms: crate::pins::now_ms(),
        volume: None,
        stable_volume: None,
        linked_worktree: false,
    }
}

fn read_json<T: serde::de::DeserializeOwned>(path: &Path) -> io::Result<T> {
    if fs::metadata(path)?.len() > 8 * 1024 * 1024 {
        return Err(io::Error::other("retention metadata exceeds 8 MiB"));
    }
    serde_json::from_slice(&fs::read(path)?).map_err(io::Error::other)
}

pub(crate) fn bounded_dirs(directory: &Path, cap: usize) -> io::Result<Vec<fs::DirEntry>> {
    let entries = match fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error),
    };
    let result = entries.take(cap + 1).collect::<io::Result<Vec<_>>>()?;
    if result.len() > cap {
        return Err(io::Error::other(format!(
            "retention walk exceeds {cap} entries at {}",
            directory.display()
        )));
    }
    Ok(result)
}

#[derive(Default)]
struct RootIdentities {
    // History is reread under admission before retirement. Reuse the same
    // canonical_root output across those reads, but never across passes: a
    // checkout path can be removed or repointed between retention runs.
    by_root: HashMap<PathBuf, PathBuf>,
    #[cfg(test)]
    resolutions: usize,
}

impl RootIdentities {
    fn canonical_root(&mut self, root: &Path) -> PathBuf {
        if let Some(canonical) = self.by_root.get(root) {
            return canonical.clone();
        }
        #[cfg(test)]
        {
            self.resolutions += 1;
        }
        let canonical = crate::root_cache::canonical_root(root);
        self.by_root.insert(root.to_path_buf(), canonical.clone());
        canonical
    }
}

fn bindings(
    storage: &StoragePaths,
    roots: &mut RootIdentities,
) -> io::Result<BTreeMap<String, Vec<Binding>>> {
    bindings_with_admission(storage, false, roots)
}

/// Retention bookkeeping (root bindings, the cache-key memo, owner manifests,
/// the bind-pending lock) is durable state under `storage.durable`.
fn bindings_with_admission(
    storage: &StoragePaths,
    admission: bool,
    roots: &mut RootIdentities,
) -> io::Result<BTreeMap<String, Vec<Binding>>> {
    bindings_with_root_resolver(storage, admission, &mut |root| roots.canonical_root(root))
}

fn bindings_with_root_resolver(
    storage: &StoragePaths,
    admission: bool,
    canonical_root: &mut impl FnMut(&Path) -> PathBuf,
) -> io::Result<BTreeMap<String, Vec<Binding>>> {
    #[derive(Deserialize)]
    struct Memo {
        key: String,
        recorded_at_ms: u64,
    }
    let mut result: BTreeMap<String, Vec<Binding>> = BTreeMap::new();
    #[cfg(test)]
    test_hook(&storage.artifacts, "history-read");
    if admission {
        yield_to_bind(&storage.durable)?;
    }
    #[cfg(test)]
    if admission {
        test_hook(&storage.durable.join("retention"), "admission-history");
    }
    let mut linked_roots = HashMap::new();
    let memo = storage.durable.join("cache-keys.json");
    if memo.exists() {
        let memo: BTreeMap<String, Memo> = read_json(&memo)?;
        if memo.len() > HISTORY_ENTRIES {
            return Err(io::Error::other("retention history exceeds 8192 bindings"));
        }
        for (root, entry) in memo {
            if admission {
                yield_to_bind(&storage.durable)?;
            }
            let binding = Binding {
                root: root.into(),
                artifact_key: entry.key,
                last_bound_ms: entry.recorded_at_ms,
                volume: None,
                stable_volume: None,
                linked_worktree: false,
            };
            result
                .entry(missing_root_scope_key(&binding.root))
                .or_default()
                .push(binding.clone());
            result
                .entry(binding.artifact_key.clone())
                .or_default()
                .push(binding);
        }
    }
    let mut root_binding_scopes = HashSet::new();
    for entry in bounded_dirs(&storage.durable.join("retention/roots"), HISTORY_ENTRIES)? {
        if admission {
            yield_to_bind(&storage.durable)?;
        }
        if entry
            .path()
            .extension()
            .is_none_or(|extension| extension != "json")
        {
            continue;
        }
        let path = entry.path();
        let scope = path
            .file_stem()
            .and_then(|name| name.to_str())
            .unwrap_or_default()
            .to_owned();
        root_binding_scopes.insert(scope.clone());
        let binding = match read_binding_record(&path) {
            Ok((_, binding)) => binding,
            Err(error) => {
                handle_binding_read_error(&storage.durable, &path, error, false, false);
                // The filename still identifies this scope. Keep it out of
                // orphan cleanup until a readable binding replaces the record.
                result
                    .entry(scope.clone())
                    .or_default()
                    .push(unreadable_binding_sentinel(&storage.durable, &scope));
                continue;
            }
        };
        if binding.linked_worktree {
            linked_roots.insert(
                canonical_root(&binding.root),
                (binding.volume.clone(), binding.stable_volume.clone()),
            );
        }
        // A durable binding supersedes the memo's possibly much older timestamp.
        result.entry(scope).or_default().push(binding.clone());
        result
            .entry(binding.artifact_key.clone())
            .or_default()
            .push(binding);
    }
    let quarantine_root = storage.durable.join("retention/quarantine/roots");
    for entry in bounded_dirs(&quarantine_root, HISTORY_ENTRIES)? {
        let path = entry.path();
        if path.extension().is_none_or(|extension| extension != "json") {
            continue;
        }
        let Some(scope) = path
            .file_stem()
            .and_then(|stem| stem.to_str())
            .and_then(|stem| stem.strip_suffix(".truncated"))
        else {
            continue;
        };
        if root_binding_scopes.insert(scope.to_owned()) {
            result
                .entry(scope.to_owned())
                .or_default()
                .push(unreadable_binding_sentinel(&storage.durable, scope));
        }
    }
    for entry in bounded_dirs(&storage.durable.join("artifact-owners"), HISTORY_ENTRIES)? {
        if admission {
            yield_to_bind(&storage.durable)?;
        }
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let path = entry.path().join("owner.json");
        if !path.exists() {
            continue;
        }
        let owner: crate::artifact_owner::ArtifactOwnerManifest = read_json(&path)?;
        if owner.schema_version != crate::artifact_owner::SCHEMA_VERSION {
            return Err(io::Error::other("unknown artifact owner format"));
        }
        let binding = Binding {
            root: owner.checkout_path.into(),
            artifact_key: entry.file_name().to_string_lossy().into_owned(),
            last_bound_ms: owner.heartbeat_at_ms,
            volume: None,
            stable_volume: None,
            linked_worktree: false,
        };
        result
            .entry(owner.project_scope_key)
            .or_default()
            .push(binding.clone());
        result
            .entry(binding.artifact_key.clone())
            .or_default()
            .push(binding);
    }
    // Memo and owner records retain their own clocks and root protections.
    // When a durable bind verified linked topology, its mount identity also
    // identifies same-root legacy copies that did not record either fact.
    for records in result.values_mut() {
        for binding in records {
            if admission {
                yield_to_bind(&storage.durable)?;
            }
            // Keep yield_to_bind above the empty-map check so checkout binding
            // can still interrupt this scan, even when there is no topology to copy.
            if linked_roots.is_empty() {
                continue;
            }
            if let Some((volume, stable_volume)) = linked_roots.get(&canonical_root(&binding.root))
            {
                binding.linked_worktree = true;
                if binding.volume.is_none() {
                    binding.volume = volume.clone();
                    binding.stable_volume = stable_volume.clone();
                }
            }
        }
    }
    Ok(result)
}

fn domain_paths(storage: &StoragePaths) -> io::Result<Vec<(PathBuf, bool)>> {
    let mut result = DOMAINS
        .iter()
        .map(|domain| (storage.artifacts.join(domain), false))
        .collect::<Vec<_>>();
    for entry in bounded_dirs(&storage.durable, 128)? {
        let name = entry.file_name().to_string_lossy().into_owned();
        if entry.file_type()?.is_dir()
            && (matches!(name.as_str(), "opencode" | "pi" | "runner")
                || name.starts_with("mcp--")
                || name.starts_with("fed--"))
        {
            for domain in ["callgraph", "inspect"] {
                result.push((entry.path().join(domain), true));
            }
        }
    }
    Ok(result)
}
fn relative_domain(storage: &StoragePaths, path: &Path) -> io::Result<PathBuf> {
    path.strip_prefix(&storage.artifacts)
        .or_else(|_| path.strip_prefix(&storage.durable))
        .map(Path::to_path_buf)
        .map_err(io::Error::other)
}

/// A lexical entry name, rather than a ReadDir offset, survives process and
/// directory replacement. New names behind the cursor are visited on the next lap.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
struct TraversalCursor {
    domain: PathBuf,
    last_processed: Option<PathBuf>,
}

struct TraversalEntry {
    directory: PathBuf,
    flat: bool,
    entry: fs::DirEntry,
    after: TraversalCursor,
}

struct TraversalBatch {
    entries: Vec<TraversalEntry>,
    end: TraversalCursor,
    cursor_path: PathBuf,
}

/// Retain at most `limit` names, ordered lexically after the last processed name.
/// Directory enumeration is metadata-only; no cache payload or SQLite file is opened.
fn traversal_batch(
    storage: &StoragePaths,
    domains: &[(PathBuf, bool)],
    kind: &str,
    limit: usize,
) -> io::Result<TraversalBatch> {
    traversal_batch_filtered(storage, domains, kind, limit, &|_| true)
}

/// Retention cursors are durable state. With a separate cache root they are
/// kept per artifact-root identity, so a different cache volume starts its
/// own laps instead of resuming another volume's position.
fn cursor_root(storage: &StoragePaths) -> io::Result<PathBuf> {
    let cursor_root = storage.durable.join("retention/cursors");
    if storage.durable == storage.artifacts {
        return Ok(cursor_root);
    }
    let identity = crate::config_live::dir_identity(&storage.artifacts)
        .ok_or_else(|| io::Error::other("artifact-root identity unavailable"))?;
    Ok(cursor_root.join(format!("{}-{}", identity.0, identity.1)))
}

fn traversal_batch_filtered(
    storage: &StoragePaths,
    domains: &[(PathBuf, bool)],
    kind: &str,
    limit: usize,
    include: &dyn Fn(&fs::DirEntry) -> bool,
) -> io::Result<TraversalBatch> {
    let cursor_path = cursor_root(storage)?.join(format!("{kind}.json"));
    let mut cursor: TraversalCursor = match read_json(&cursor_path) {
        Ok(cursor) => cursor,
        Err(error) if error.kind() == io::ErrorKind::NotFound => TraversalCursor::default(),
        Err(error) => return Err(error),
    };
    let mut domain = domains
        .iter()
        .position(|(path, _)| {
            relative_domain(storage, path).ok().as_deref() == Some(cursor.domain.as_path())
        })
        .unwrap_or(0);
    if domains.is_empty() {
        return Ok(TraversalBatch {
            entries: Vec::new(),
            end: cursor,
            cursor_path,
        });
    }
    let mut result = Vec::new();
    for _ in 0..domains.len() {
        let (directory, flat) = &domains[domain];
        let relative = relative_domain(storage, &directory)?;
        if cursor.domain != relative {
            cursor = TraversalCursor {
                domain: relative.clone(),
                last_processed: None,
            };
        }
        let remaining = limit - result.len();
        let mut names = BTreeMap::new();
        match fs::read_dir(directory) {
            Ok(entries) => {
                for entry in entries {
                    let entry = entry?;
                    if entry.file_name() == ".retention-trash" || !include(&entry) {
                        continue;
                    }
                    let name = PathBuf::from(entry.file_name());
                    if cursor
                        .last_processed
                        .as_ref()
                        .is_some_and(|last| name <= *last)
                    {
                        continue;
                    }
                    names.insert(name, entry);
                    if names.len() > remaining {
                        names.pop_last();
                    }
                }
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        for (name, entry) in names {
            cursor.last_processed = Some(name);
            result.push(TraversalEntry {
                directory: directory.clone(),
                flat: *flat,
                entry,
                after: cursor.clone(),
            });
        }
        if result.len() == limit {
            break;
        }
        domain = (domain + 1) % domains.len();
        cursor = TraversalCursor {
            domain: relative_domain(storage, &domains[domain].0)?,
            last_processed: None,
        };
    }
    Ok(TraversalBatch {
        entries: result,
        end: cursor,
        cursor_path,
    })
}

fn next_batch(storage: &StoragePaths, domains: &[(PathBuf, bool)]) -> io::Result<TraversalBatch> {
    traversal_batch(storage, domains, "roots", PASS_ENTRIES)
}

/// Generation cleanup has its own durable cursor and a count budget, not the
/// root-retirement deadline. Recursive byte accounting and due-root work can
/// consume that deadline before the first view is visited. For a stable set of
/// V views, 64 per pass reaches all of them in ceil(V / 64) + 1 uninterrupted
/// passes, including a partial lap and wraparound. No payload tree is walked.
fn sweep_view_generations(
    storage: &StoragePaths,
    cancelled: &dyn Fn() -> bool,
    report: &mut SweepReport,
) -> io::Result<()> {
    let batch = traversal_batch_filtered(
        storage,
        &[
            (storage.artifacts.join("views"), false),
            (storage.artifacts.join("views/v2"), false),
        ],
        "generations",
        64,
        &|entry| {
            (key_valid(&entry.file_name().to_string_lossy()) || directory_is_v2_view(&entry.path()))
                && entry.file_type().is_ok_and(|kind| kind.is_dir())
                && entry.path().join("pointer.sqlite").is_file()
        },
    )?;
    if batch.entries.is_empty() && !batch.cursor_path.exists() {
        return Ok(());
    }
    let registries = match generation_registries(storage, &batch.entries, report) {
        Ok(registries) => {
            generation_registry_errors()
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .remove(&storage.artifacts.join("blobs/v2"));
            registries
        }
        Err(error) => {
            // An unavailable family census protects unknown V2 owners, but it
            // must not prevent direct-view cleanup or cursor/root progress.
            report_generation_registry_error(&storage.artifacts.join("blobs/v2"), &error, report);
            HashMap::new()
        }
    };
    let mut cursor = None;
    let mut completed = true;
    for item in batch.entries {
        if cancelled() || bind_pending(&storage.durable) {
            report.cancelled |= cancelled();
            report.stopped_early = true;
            completed = false;
            break;
        }
        let path = item.entry.path();
        if let Some(mut store) = crate::views::ViewStore::existing_dir(path.clone()) {
            if directory_is_v2_view(&path) {
                let Some(registry) = registries.get(&path) else {
                    report.skipped_protected += 1;
                    cursor = Some(item.after);
                    continue;
                };
                store = store.with_sweep_registry(registry.clone());
            }
            #[cfg(test)]
            test_hook(&path, "walk");
            if cancelled() || bind_pending(&storage.durable) {
                report.cancelled |= cancelled();
                report.stopped_early = true;
                completed = false;
                break;
            }
            match store.sweep_generations_with_bytes() {
                Ok(removed) => {
                    if removed.over_limit && report.oversized_views.len() < 16 {
                        report.oversized_views.push(format!(
                            "{}: over the generation directory limit; partial cleanup removed {} generation(s) and kept {} whose derived database could be shared by an unseen reference",
                            path.display(),
                            removed.removed_generations,
                            removed.kept_unproven_owners
                        ));
                    }
                    report.removed_generations += removed.removed_generations;
                    report.removed_bytes += removed.removed_bytes;
                    report
                        .domains
                        .entry("views/generations".into())
                        .or_default()
                        .removed_bytes += removed.removed_bytes;
                    if removed.removed_generations > 0 || removed.removed_bytes > 0 {
                        log::info!(
                            "views: retention removed_generations={} removed_bytes={} view={}",
                            removed.removed_generations,
                            removed.removed_bytes,
                            path.display()
                        );
                    }
                }
                Err(error) => {
                    report.skipped_protected += 1;
                    if report.errors.len() < 16 {
                        report.errors.push(format!("{}: {error}", path.display()));
                    }
                }
            }
        }
        report
            .domains
            .entry("views/generations".into())
            .or_default()
            .examined += 1;
        report.examined += 1;
        // A blocked or malformed view cannot starve the rest of the lap.
        cursor = Some(item.after);
    }
    if completed {
        cursor = Some(batch.end);
    }
    if let Some(cursor) = cursor {
        persist_cursor(storage, &batch.cursor_path, &cursor)?;
    }
    Ok(())
}

fn directory_is_v2_view(path: &Path) -> bool {
    path.parent()
        .is_some_and(|parent| parent.file_name().is_some_and(|name| name == "v2"))
}

/// Resolve owners once for the batch, not once per view. Existing registries
/// share their tracked SQLite connection with publishers and readers.
fn generation_registries(
    storage: &StoragePaths,
    entries: &[TraversalEntry],
    report: &mut SweepReport,
) -> io::Result<HashMap<PathBuf, crate::views::registry::FamilyRegistry>> {
    let wanted: HashSet<_> = entries
        .iter()
        .map(|entry| entry.entry.path())
        .filter(|path| directory_is_v2_view(path))
        .collect();
    let mut owners = HashMap::new();
    if wanted.is_empty() {
        return Ok(owners);
    }
    for entry in bounded_dirs(&storage.artifacts.join("blobs/v2"), HISTORY_ENTRIES)? {
        let registry_path = entry.path().join(crate::views::registry::REGISTRY_FILE);
        let family = (|| -> io::Result<Option<_>> {
            if !entry.file_type()?.is_dir() {
                return Ok(None);
            }
            match fs::metadata(&registry_path) {
                Ok(metadata) if metadata.is_file() => {}
                Ok(_) => return Err(io::Error::other("family registry is not a regular file")),
                Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
                Err(error) => return Err(error),
            }
            let family = entry.file_name().to_string_lossy().into_owned();
            let Some(registry) = crate::views::registry::FamilyRegistry::open_existing_read_only(
                &storage.artifacts,
                &family,
            )
            .map_err(io::Error::other)?
            else {
                return Ok(None);
            };
            registry
                .bind_durable_storage(&storage.durable)
                .map_err(io::Error::other)?;
            let members = registry
                .members_bounded(HISTORY_ENTRIES)
                .map_err(io::Error::other)?;
            Ok(Some((registry, members)))
        })();
        let (registry, members) = match family {
            Ok(family) => {
                generation_registry_errors()
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .remove(&registry_path);
                let Some(family) = family else { continue };
                family
            }
            Err(error) => {
                report_generation_registry_error(&registry_path, &error, report);
                continue;
            }
        };
        // Publish no owners from a family until its complete membership census
        // succeeded. A bad family's views remain protected, not partly swept.
        for member in members {
            if wanted.contains(&member.view_dir) {
                owners.insert(member.view_dir, registry.clone());
            }
        }
    }
    Ok(owners)
}

fn generation_registry_errors() -> &'static Mutex<HashSet<PathBuf>> {
    static ERRORS: std::sync::OnceLock<Mutex<HashSet<PathBuf>>> = std::sync::OnceLock::new();
    ERRORS.get_or_init(|| Mutex::new(HashSet::new()))
}

fn report_generation_registry_error(path: &Path, error: &io::Error, report: &mut SweepReport) {
    if report.errors.len() < 16 {
        report
            .errors
            .push(format!("{}: generation registry: {error}", path.display()));
    }
    // Emit once per failing family until a successful census resets it. Keep
    // the per-pass health error even when the warning is rate-limited.
    let first = generation_registry_errors()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(path.to_path_buf());
    if first {
        log::warn!(
            "views: generation registry skipped registry={} error={error}",
            path.display()
        );
        #[cfg(test)]
        test_hook(path, "generation-registry-error");
    }
}

fn visit_one_family(
    storage: &StoragePaths,
    domain: &str,
    kind: &str,
    cancelled: &dyn Fn() -> bool,
    mut work: impl FnMut(&fs::DirEntry) -> io::Result<()>,
) -> io::Result<()> {
    let batch = traversal_batch(storage, &[(storage.artifacts.join(domain), false)], kind, 1)?;
    if cancelled() {
        return Ok(());
    }
    let Some(item) = batch.entries.into_iter().next() else {
        return atomic_json(&batch.cursor_path, &batch.end);
    };
    let result = work(&item.entry);
    if !cancelled() {
        // Advance even when family work fails, so later families can still run.
        atomic_json(&batch.cursor_path, &item.after)?;
    }
    result
}

fn protected(cache: &Path) -> io::Result<bool> {
    for entry in bounded_dirs(&cache.join("pins"), TREE_ENTRIES)? {
        if entry
            .path()
            .extension()
            .is_some_and(|extension| extension == "json")
        {
            let pin = crate::pins::read_metadata_strict(&entry.path())
                .map_err(|error| io::Error::other(error.to_string()))?;
            if crate::pins::owner_is_live(&pin.owner) {
                return Ok(true);
            }
        }
    }
    for entry in bounded_dirs(&cache.join("readers"), TREE_ENTRIES)? {
        if !entry.file_type()?.is_dir() {
            return Ok(true);
        }
        if crate::root_cache::protected_read_marker_exists(
            cache,
            &entry.file_name().to_string_lossy(),
        ) {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Whether a due legacy cache must stay because a v3 import is reading it.
///
/// The v3 importer reads `index/<key>`, `semantic/<key>` and `symbols/<key>`
/// while it holds a claim in the family's import ledger
/// (`migration::v3_ledger`). Retiring such a set would pull the source out
/// from under that import, so a live claim protects it, and a ledger that
/// cannot be read is uncertainty and protects it too. A finished, abandoned
/// or absent import leaves the binding-age rules alone.
fn legacy_import_source_pinned(storage: &StoragePaths, cache: &Path, key: &str) -> bool {
    let domain = cache
        .parent()
        .and_then(Path::file_name)
        .and_then(|name| name.to_str());
    if !matches!(domain, Some("index" | "semantic" | "symbols")) {
        return false;
    }
    match crate::migration::v3_ledger::legacy_key_status(&storage.artifacts, key) {
        Ok(Some(status)) => status.live_claim,
        Ok(None) => false,
        Err(_) => true,
    }
}

/// One kind of legacy (pre-v3) data that stays on disk for an offline
/// rollback to the previous binary.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub enum RetainedKind {
    /// `symbols/<key>`: a parsed-symbol cache, a source of the v3 symbol import.
    SymbolCache,
    /// `views/<scope>`: a v1 view, whose generations name v1 blob rows.
    V1View,
    /// `blobs/<family>`: v1 blob stores, the copy-on-first-use donors of v3.
    V1BlobFamily,
    /// `blobs/v2/<family>`: v2 family stores, also copy-on-first-use donors.
    V2FamilyStore,
}

impl RetainedKind {
    pub const fn describe(self) -> &'static str {
        match self {
            Self::SymbolCache => "symbol cache",
            Self::V1View => "v1 view",
            Self::V1BlobFamily => "v1 blob family",
            Self::V2FamilyStore => "v2 family store",
        }
    }
}

/// One retained legacy item.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RetainedItem {
    pub kind: RetainedKind,
    pub path: PathBuf,
    pub bytes: u64,
    /// The v3 family of the same name exists, so rows of this item may have
    /// been copied into v3 (a copy never removes or changes its source).
    pub copied_into_v3: bool,
    /// Why the item is protected right now (a live import claim, live pins
    /// or readers), or `None`. A protected item is never retired.
    pub pinned: Option<String>,
}

/// Upper bound on items listed per retained kind.
const RETAINED_ITEMS_PER_KIND: usize = 10_000;

/// Lists the legacy data this build keeps but never writes: symbol caches,
/// v1 views and blob families, and v2 family stores, with whether v3 holds
/// a family of the same name and whether something protects them now. It
/// reads directory listings, sizes, ledgers and pin files only; it writes
/// nothing under the storage root.
pub fn retained_legacy_inventory(storage: &Path) -> io::Result<Vec<RetainedItem>> {
    let paths = StoragePaths::same(storage);
    let v3_family = |name: &str| storage.join("blobs/v3").join(name).is_dir();
    let mut items = Vec::new();
    for (kind, parent) in [
        (RetainedKind::SymbolCache, storage.join("symbols")),
        (RetainedKind::V1View, storage.join("views")),
        (RetainedKind::V1BlobFamily, storage.join("blobs")),
        (RetainedKind::V2FamilyStore, storage.join("blobs/v2")),
    ] {
        let entries = match fs::read_dir(&parent) {
            Ok(entries) => entries,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error),
        };
        let mut names = Vec::new();
        for entry in entries.take(RETAINED_ITEMS_PER_KIND) {
            let entry = entry?;
            if !entry.file_type()?.is_dir() {
                continue;
            }
            let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            let listed = match kind {
                RetainedKind::SymbolCache => {
                    crate::migration::per_checkout::validate_legacy_key(&name).is_ok()
                }
                RetainedKind::V1View | RetainedKind::V1BlobFamily => key_valid(&name),
                RetainedKind::V2FamilyStore => {
                    crate::blob_store::v2::validate_family(&name).is_ok()
                }
            };
            if listed {
                names.push(name);
            }
        }
        names.sort();
        for name in names {
            let path = parent.join(&name);
            let bytes = crate::migration::prune_legacy::tree_size(&path, TREE_ENTRIES)
                .map(|(bytes, _, _)| bytes)
                .unwrap_or(0);
            let pinned = match kind {
                RetainedKind::SymbolCache => legacy_import_source_pinned(&paths, &path, &name)
                    .then(|| "a v3 import is reading it".to_owned()),
                RetainedKind::V1View => match protected(&path) {
                    Ok(true) => Some("live pins or readers".to_owned()),
                    Ok(false) => None,
                    Err(error) => Some(format!("pins unreadable: {error}")),
                },
                // Their rows are protected one by one: a copier pins each
                // source row before reading it, and the v1/v2 sweeps honour
                // those pins. Nothing protects the store as a whole.
                RetainedKind::V1BlobFamily | RetainedKind::V2FamilyStore => None,
            };
            items.push(RetainedItem {
                kind,
                copied_into_v3: v3_family(&name),
                path,
                bytes,
                pinned,
            });
        }
    }
    Ok(items)
}

fn owner_has_residency_marker(
    storage: &StoragePaths,
    owner: &crate::artifact_owner::ArtifactOwnerManifest,
) -> bool {
    let directory = storage
        .durable
        .join("retention/readers")
        .join(&owner.project_scope_key);
    let Ok(entries) = bounded_dirs(&directory, TREE_ENTRIES) else {
        return false;
    };
    entries.iter().any(|entry| {
        read_json::<crate::root_cache::ReadMarkerMetadata>(&entry.path()).is_ok_and(|marker| {
            marker.pid == owner.pid
                && marker.hostname == owner.hostname
                && crate::root_cache::process_start_time_ms(marker.pid)
                    .is_some_and(|start| start <= marker.created_at_ms.saturating_add(1000))
        })
    })
}

/// Metadata only: never open a raw file descriptor on a SQLite file set.
fn tree_files(cache: &Path, deadline: Instant) -> io::Result<Vec<(PathBuf, u64)>> {
    validate_tree(cache, deadline, true)
}

#[derive(Debug)]
struct RetentionDeadline;

impl std::fmt::Display for RetentionDeadline {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("retention deadline reached")
    }
}

impl std::error::Error for RetentionDeadline {}

/// A validation walk met more than [`TREE_ENTRIES`] entries and stopped
/// before it saw the whole tree.
#[derive(Debug)]
struct TreeEntryLimit;

impl std::fmt::Display for TreeEntryLimit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("retention tree entry limit reached")
    }
}

impl std::error::Error for TreeEntryLimit {}

fn is_tree_entry_limit(error: &io::Error) -> bool {
    error
        .get_ref()
        .is_some_and(|error| error.is::<TreeEntryLimit>())
}

fn is_retention_deadline(error: &io::Error) -> bool {
    error
        .get_ref()
        .is_some_and(|error| error.is::<RetentionDeadline>())
}

/// A due root needs the filesystem safety checks, not payload sizes. Avoid a
/// stat of every file merely to report bytes that the background reaper frees.
fn validate_tree(
    cache: &Path,
    deadline: Instant,
    measure: bool,
) -> io::Result<Vec<(PathBuf, u64)>> {
    validate_tree_with_admission(cache, deadline, measure, &|| Ok(()))
}

fn validate_tree_with_admission(
    cache: &Path,
    deadline: Instant,
    measure: bool,
    admission: &dyn Fn() -> io::Result<()>,
) -> io::Result<Vec<(PathBuf, u64)>> {
    #[cfg(test)]
    test_hook(cache, "walk");
    if !fs::symlink_metadata(cache)?.is_dir() {
        return Err(io::Error::other("non-directory cache retained"));
    }
    let boundary = crate::walk_boundary::DeviceBoundary::for_root(cache)?;
    let mut pending = vec![cache.to_path_buf()];
    let mut result = Vec::new();
    let mut examined = 0;
    while let Some(directory) = pending.pop() {
        admission()?;
        if Instant::now() >= deadline {
            return Err(io::Error::new(io::ErrorKind::TimedOut, RetentionDeadline));
        }
        let mut entries = fs::read_dir(&directory)?;
        loop {
            admission()?;
            if Instant::now() >= deadline {
                return Err(io::Error::new(io::ErrorKind::TimedOut, RetentionDeadline));
            }
            let Some(entry) = entries.next() else { break };
            let entry = entry?;
            if examined >= TREE_ENTRIES {
                return Err(io::Error::other(TreeEntryLimit));
            }
            examined += 1;
            let kind = entry.file_type()?;
            if kind.is_symlink() || (!kind.is_dir() && !kind.is_file()) {
                return Err(io::Error::other("non-regular cache entry retained"));
            }
            if kind.is_dir() {
                if !boundary.should_descend(&entry.path())? {
                    return Err(io::Error::other("foreign cache filesystem retained"));
                }
                pending.push(entry.path());
            } else if measure {
                #[cfg(test)]
                test_hook(cache, "measure-file");
                result.push((entry.path(), entry.metadata()?.len()));
            }
        }
    }
    Ok(result)
}

const RETIREMENT_WITNESS: &str = crate::bash_background::persistence::REPAIR_WITNESS_LEAF;

#[derive(Serialize, Deserialize)]
struct RetirementWitness {
    version: u32,
    nonce: String,
    owner: (u64, u64),
    directory: (u64, u64),
    members: BTreeMap<String, (u64, u64)>,
}

fn retirement_members(
    directory: &crate::bash_background::persistence::PinnedDir,
) -> io::Result<BTreeMap<String, (u64, u64)>> {
    retirement_members_with_budget(
        directory,
        &mut crate::bash_background::persistence::RepairBudget::with_entries(TREE_ENTRIES + 1),
    )
}

fn retirement_members_with_budget(
    directory: &crate::bash_background::persistence::PinnedDir,
    budget: &mut crate::bash_background::persistence::RepairBudget,
) -> io::Result<BTreeMap<String, (u64, u64)>> {
    let mut members = BTreeMap::new();
    for name in directory.repair_names(&mut budget.limited(TREE_ENTRIES + 1))? {
        budget.entry()?;
        if name == RETIREMENT_WITNESS || name == ".retention-trash" {
            continue;
        }
        if members.len() >= TREE_ENTRIES {
            return Err(io::Error::other(
                "retirement member census exceeds entry limit",
            ));
        }
        let text = name
            .to_str()
            .ok_or_else(|| io::Error::other("unrepresentable retirement entry name"))?
            .to_owned();
        let entry = directory.repair_entry_for_rename(&name)?;
        members.insert(text, entry.identity()?);
    }
    Ok(members)
}

fn write_retirement_witness(
    storage: &Path,
    directory: &crate::bash_background::persistence::PinnedDir,
    nonce: &str,
    members: BTreeMap<String, (u64, u64)>,
) -> io::Result<()> {
    let witness = RetirementWitness {
        version: 1,
        nonce: nonce.to_owned(),
        owner: crate::bash_background::persistence::PinnedDir::repair_open_path(storage)?
            .directory_identity()?,
        directory: directory.directory_identity()?,
        members,
    };
    let bytes = serde_json::to_vec(&witness).map_err(io::Error::other)?;
    if bytes.len() > 2 * 1024 * 1024 {
        return Err(io::Error::other("retirement witness exceeds byte limit"));
    }
    directory.repair_write_durable(std::ffi::OsStr::new(RETIREMENT_WITNESS), &bytes)
}

fn verify_retirement_witness(
    storage: &StoragePaths,
    directory: &crate::bash_background::persistence::PinnedDir,
    nonce: &str,
    budget: &mut crate::bash_background::persistence::RepairBudget,
) -> io::Result<()> {
    use std::io::Read;
    let file = directory.repair_read_file(std::ffi::OsStr::new(RETIREMENT_WITNESS))?;
    if file.metadata()?.len() > 2 * 1024 * 1024 {
        return Err(io::Error::other("retirement witness exceeds byte limit"));
    }
    let mut bytes = Vec::new();
    file.take(2 * 1024 * 1024 + 1).read_to_end(&mut bytes)?;
    if bytes.len() > 2 * 1024 * 1024 {
        return Err(io::Error::other("retirement witness exceeds byte limit"));
    }
    let witness: RetirementWitness = serde_json::from_slice(&bytes).map_err(io::Error::other)?;
    if witness.version != 1
        || witness.nonce != nonce
        || witness.directory != directory.directory_identity()?
        || witness.owner
            != crate::bash_background::persistence::PinnedDir::repair_open_path(&storage.durable)?
                .directory_identity()?
    {
        return Err(io::Error::other("unverified retirement identity retained"));
    }
    for (name, actual) in retirement_members_with_budget(directory, budget)? {
        if witness.members.get(&name) != Some(&actual) {
            return Err(io::Error::other("unverified retirement member retained"));
        }
    }
    Ok(())
}

/// Retirement directories made by AFT releases up to v0.62.0 carry no witness:
/// those releases renamed a cache into `.retention-trash` without writing one,
/// and the reaper that ships witnesses refuses every entry without one, so
/// their disk space was never reclaimed. The current retirement writes the
/// witness before the rename, so a witness-less entry it leaves behind is a
/// source that was swapped during the rename, which can hold anything.
///
/// A witness-less entry is therefore deleted only when its contents prove it
/// is an AFT root cache (the `inspect`, `index`, `callgraph` and similar
/// per-checkout folders): every entry is a file name AFT itself creates there,
/// nothing is a symlink, every entry is owned by the current user and not
/// writable by anyone else, and no live process still holds a read marker in
/// it. Anything else is kept, and the error names what is missing.
fn verify_retirement_proof(
    storage: &StoragePaths,
    directory: &crate::bash_background::persistence::PinnedDir,
    nonce: &str,
    budget: &mut crate::bash_background::persistence::RepairBudget,
) -> io::Result<()> {
    match directory.repair_read_file(std::ffi::OsStr::new(RETIREMENT_WITNESS)) {
        Ok(_) => verify_retirement_witness(storage, directory, nonce, budget),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            verify_unwitnessed_cache_retirement(directory, budget).map_err(|error| {
                io::Error::new(
                    error.kind(),
                    format!(
                        "retirement witness {RETIREMENT_WITNESS} is missing (retired by AFT \
                         v0.62.0 or earlier, or a source swapped during retirement) and the \
                         contents do not prove an AFT cache: {error}; AFT keeps it, delete {} \
                         by hand if its contents are not needed",
                        directory.path().display()
                    ),
                )
            })
        }
        Err(error) => Err(error),
    }
}

/// Whether `name` is a file AFT creates directly inside a root cache folder.
fn unwitnessed_cache_file(name: &str) -> bool {
    if matches!(
        name,
        "writer.lease" | "cache.lock" | "semantic.lock" | "symbols.lock"
    ) {
        return true;
    }
    if let Some(key) = name.strip_suffix(".current") {
        return key_valid(key);
    }
    let database = ["-wal", "-shm", "-journal"]
        .iter()
        .find_map(|suffix| name.strip_suffix(suffix))
        .unwrap_or(name);
    unwitnessed_generation_name(database)
}

/// `<key>.sqlite` or `<key>.g<ordinal>.<pid>.sqlite`: a cache database, also
/// used as the generation label of its `readers/` folder.
fn unwitnessed_generation_name(name: &str) -> bool {
    let Some(stem) = name.strip_suffix(".sqlite") else {
        return false;
    };
    let mut parts = stem.split('.');
    let digits = |part: &str| !part.is_empty() && part.bytes().all(|byte| byte.is_ascii_digit());
    match (parts.next(), parts.next(), parts.next(), parts.next()) {
        (Some(key), None, None, None) => key_valid(key),
        (Some(key), Some(ordinal), Some(pid), None) => {
            key_valid(key) && ordinal.strip_prefix('g').is_some_and(digits) && digits(pid)
        }
        _ => false,
    }
}

fn verify_unwitnessed_cache_retirement(
    directory: &crate::bash_background::persistence::PinnedDir,
    budget: &mut crate::bash_background::persistence::RepairBudget,
) -> io::Result<()> {
    let refuse = |what: String| Err(io::Error::other(what));
    if !directory.repair_status()?.private {
        return refuse("the directory is writable by other users or not owned by this user".into());
    }
    let mut cache_files = 0;
    let mut names = directory.repair_names(budget)?;
    names.sort();
    for name in names {
        let status = directory.repair_entry_status(&name)?;
        let text = name.to_string_lossy();
        if !status.private {
            return refuse(format!(
                "{text} is writable by other users or not owned by this user"
            ));
        }
        if status.is_file && unwitnessed_cache_file(&text) {
            cache_files += 1;
            continue;
        }
        if !(status.is_dir && name == "readers") {
            return refuse(format!(
                "{text} is not an entry AFT creates in a cache folder"
            ));
        }
        let readers = directory.repair_child(&name)?;
        for generation in readers.repair_names(budget)? {
            let label = generation.to_string_lossy();
            let status = readers.repair_entry_status(&generation)?;
            if !(status.is_dir && status.private && unwitnessed_generation_name(&label)) {
                return refuse(format!(
                    "readers/{label} is not a private read-marker generation folder"
                ));
            }
            let markers = readers.repair_child(&generation)?;
            for marker in markers.repair_names(budget)? {
                let status = markers.repair_entry_status(&marker)?;
                let marker = marker.to_string_lossy();
                if !(status.is_file && status.private && marker.ends_with(".json")) {
                    return refuse(format!(
                        "readers/{label}/{marker} is not a private read marker"
                    ));
                }
            }
        }
    }
    if cache_files == 0 {
        return refuse("it holds no AFT cache files".into());
    }
    if crate::root_cache::sweep_all_read_markers_pinned(directory, budget, false)?.protected {
        return refuse("a live process still holds a read marker in it".into());
    }
    Ok(())
}

/// Keep retirement on the cache filesystem, anchored to the opened domain.
/// Mount-point caches retire their contents into their own mount-local trash.
fn retire_directory(storage: &Path, cache: &Path) -> io::Result<()> {
    let parent = cache
        .parent()
        .ok_or_else(|| io::Error::other("missing cache parent"))?;
    let parent = crate::bash_background::persistence::PinnedDir::repair_open_path(parent)?;
    let name = cache
        .file_name()
        .ok_or_else(|| io::Error::other("missing cache name"))?;
    let source = parent.repair_entry_for_rename(name)?;
    let child = source.directory()?;
    if crate::cross_volume::is_mount_point(cache)? {
        return retire_mount_point_contents(storage, cache, &child);
    }
    child.repair_check_boundary(&parent, &[])?;
    let trash_name = std::ffi::OsStr::new(".retention-trash");
    let trash = match parent.repair_create_dir(trash_name) {
        Ok(trash) => trash,
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            parent.repair_child_same_mount(trash_name)?
        }
        Err(error) => return Err(error),
    };
    let mut nonce = [0_u8; 16];
    getrandom::fill(&mut nonce).map_err(io::Error::other)?;
    let nonce = format!("{:032x}", u128::from_le_bytes(nonce));
    write_retirement_witness(storage, &child, &nonce, retirement_members(&child)?)?;
    #[cfg(test)]
    test_hook(cache, "rename-root");
    parent.repair_rename_entry_to(
        name,
        &source,
        &trash,
        std::ffi::OsStr::new(&format!("retired-{nonce}")),
    )
}

/// Mount points stay in place; only their entries move into mount-local trash.
fn retire_mount_point_contents(
    storage: &Path,
    _cache: &Path,
    cache_pin: &crate::bash_background::persistence::PinnedDir,
) -> io::Result<()> {
    use crate::bash_background::persistence::RepairBudget;
    let members = retirement_members(cache_pin)?;
    let mut nonce = [0_u8; 16];
    getrandom::fill(&mut nonce).map_err(io::Error::other)?;
    let nonce = format!("{:032x}", u128::from_le_bytes(nonce));
    atomic_json(&storage.join("retention/mount-trash-pending"), &nonce)?;
    let retired_name = format!("retired-{nonce}");
    let mut budget = RepairBudget::new();
    let retired = loop {
        budget.entry()?;
        let trash = match cache_pin.repair_create_dir(std::ffi::OsStr::new(".retention-trash")) {
            Ok(trash) => trash,
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                cache_pin.repair_child_same_mount(std::ffi::OsStr::new(".retention-trash"))?
            }
            Err(error) => return Err(error),
        };
        match trash.repair_create_dir(std::ffi::OsStr::new(&retired_name)) {
            Ok(retired) => break retired,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error),
        }
    };
    write_retirement_witness(storage, &retired, &nonce, members.clone())?;
    #[cfg(test)]
    test_hook(_cache, "rename-root");
    for (name, expected) in members {
        budget.entry()?;
        let name = std::ffi::OsStr::new(&name);
        let source = cache_pin.repair_entry_for_rename(name)?;
        if source.identity()? != expected {
            return Err(io::Error::other("mount retirement source identity changed"));
        }
        cache_pin.repair_rename_entry_to(name, &source, &retired, name)?;
    }
    match cache_pin.repair_read_file(std::ffi::OsStr::new(RETIREMENT_WITNESS)) {
        Ok(_) => cache_pin.repair_remove_file(std::ffi::OsStr::new(RETIREMENT_WITNESS))?,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    Ok(())
}

/// Retired data still counts as present until the reaper has reclaimed it.
fn emptied_mount_point(cache: &Path) -> bool {
    crate::cross_volume::is_mount_point(cache).unwrap_or(false)
        && fs::read_dir(cache).is_ok_and(|mut entries| entries.next().is_none())
}

/// Do not retire an empty mount again, or create more tombstones while its
/// previous retirement awaits reaping. The reaper scans these mounts even if
/// no binding history or live cache contents remain.
fn retired_mount_point(cache: &Path) -> bool {
    crate::cross_volume::is_mount_point(cache).unwrap_or(false)
        && fs::read_dir(cache).is_ok_and(|mut entries| {
            entries.all(|entry| entry.is_ok_and(|entry| entry.file_name() == ".retention-trash"))
        })
}

/// Returns whether any trash remains or the census was incomplete. A busy
/// mount-local snapshot also keeps the pending hint for a later reaper pass.
fn reap_trash(
    storage: &StoragePaths,
    trash: &Path,
    mount_local: bool,
    deadline: Instant,
    budget: &mut crate::bash_background::persistence::RepairBudget,
    report: &mut TrashReport,
) -> io::Result<bool> {
    reap_trash_with_snapshot_budget(
        storage,
        trash,
        mount_local,
        deadline,
        budget,
        report,
        Duration::from_millis(10),
    )
}

fn reap_trash_with_snapshot_budget(
    storage: &StoragePaths,
    trash: &Path,
    mount_local: bool,
    deadline: Instant,
    budget: &mut crate::bash_background::persistence::RepairBudget,
    report: &mut TrashReport,
    admission_budget: Duration,
) -> io::Result<bool> {
    let key = (storage.artifacts.clone(), trash.to_path_buf());
    let (mut cursor, full, expired) = {
        let mut scans = trash_scans()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // Close idle cached iterators after two retention intervals. Active
        // passes keep their reserved slots until they finish or unwind.
        let expired = scans
            .extract_if(|_, scan| {
                scan.as_ref()
                    .is_some_and(|scan| scan.touched.elapsed() >= INTERVAL * 2)
            })
            .map(|(_, scan)| scan)
            .collect::<Vec<_>>();
        let (cursor, full) = if let Some(slot) = scans.get_mut(&key) {
            let cursor = slot.take();
            let busy = cursor.is_none();
            (cursor, busy)
        } else if scans.len() < TRASH_SCAN_HANDLES {
            // A None slot reserves a handle for the active pass, so concurrent
            // reapers cannot exceed the bound before publishing their cursors.
            scans.insert(key.clone(), None);
            (None, false)
        } else {
            (None, true)
        };
        (cursor, full, expired)
    };
    drop(expired);
    if full {
        report.incomplete = true;
        report.keep(
            trash,
            "cursor_capacity",
            "waiting for an open trash scan to finish",
        );
        return Ok(true);
    }
    let mut lease = TrashScanLease(Some(key.clone()));
    let result = reap_trash_with_cursor(
        storage,
        trash,
        mount_local,
        deadline,
        budget,
        report,
        admission_budget,
        &mut cursor,
    );
    if let Some(mut cursor) = cursor.filter(|scan| {
        (scan.entries.is_some() || !scan.deferred.is_empty())
            && (scan.omitted_prefix || !scan.deferred.is_empty())
    }) {
        cursor.touched = Instant::now();
        let previous = {
            trash_scans()
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .insert(key, Some(cursor))
        };
        drop(previous);
        lease.0 = None;
    }
    result
}

fn reap_trash_with_cursor(
    storage: &StoragePaths,
    trash: &Path,
    mount_local: bool,
    deadline: Instant,
    budget: &mut crate::bash_background::persistence::RepairBudget,
    report: &mut TrashReport,
    admission_budget: Duration,
    cursor: &mut Option<TrashScan>,
) -> io::Result<bool> {
    let entries = {
        // Snapshot mount-local tombstones under admission so a concurrent
        // retirement finishes moving entries before any of them are unlinked.
        // Domain-local retirement publishes a whole tree with one rename.
        let _admission = if mount_local {
            match crate::fs_lock::try_acquire(
                &storage.durable.join("retention/sweep.lock"),
                Duration::ZERO,
            ) {
                Ok(lease) => Some(lease),
                Err(error) => {
                    report.incomplete = true;
                    report.keep(trash, "admission_busy", error);
                    return Ok(true);
                }
            }
        } else {
            None
        };
        let previous = cursor.take();
        match fs::symlink_metadata(trash) {
            Ok(metadata) if metadata.is_dir() => {
                if mount_local && crate::cross_volume::is_mount_point(trash)? {
                    return Err(io::Error::other(
                        "foreign retention trash filesystem retained",
                    ));
                }
                let identity =
                    crate::bash_background::persistence::PinnedDir::repair_open_path(trash)?
                        .directory_identity()?;
                // Keep the live iterator, not a name to skip after reopening:
                // rereading a retained prefix can itself exhaust admission.
                let scan = match previous.filter(|scan| scan.identity == identity) {
                    Some(scan) => scan,
                    None => TrashScan {
                        identity,
                        entries: Some(fs::read_dir(trash)?),
                        deferred: VecDeque::new(),
                        omitted_prefix: false,
                        touched: Instant::now(),
                    },
                };
                if scan.omitted_prefix {
                    report.incomplete = true;
                    report.keep(
                        trash,
                        "scan_resumed",
                        "earlier entries in this lap were examined by previous passes",
                    );
                }
                *cursor = Some(scan);
                let scan = cursor.as_mut().unwrap();
                let mut snapshot = Vec::new();
                // Mount-local retirement needs admission only for the snapshot.
                // Yield promptly to binders and cap this lock hold separately
                // from the much slower, admission-free unlink work.
                let admission_deadline = Instant::now() + admission_budget;
                #[cfg(test)]
                test_hook(trash, "trash-snapshot");
                loop {
                    if snapshot.len() == PASS_ENTRIES {
                        report.incomplete = true;
                        report.keep(
                            trash,
                            "snapshot_entries",
                            "trash snapshot entry limit reached",
                        );
                        break;
                    }
                    if mount_local && bind_pending(&storage.durable) {
                        report.incomplete = true;
                        report.keep(
                            trash,
                            "bind_pending",
                            "snapshot yielded to checkout binding",
                        );
                        break;
                    }
                    if mount_local && Instant::now() >= admission_deadline {
                        report.incomplete = true;
                        report.keep(
                            trash,
                            "admission_budget",
                            "mount snapshot admission deadline reached",
                        );
                        break;
                    }
                    if let Err(error) = budget.entry() {
                        report.incomplete = true;
                        report.keep(trash, "enumeration_budget", error);
                        break;
                    }
                    let next = scan
                        .deferred
                        .pop_front()
                        .map(Ok)
                        .or_else(|| scan.entries.as_mut().and_then(Iterator::next));
                    match next {
                        Some(Ok(entry)) => {
                            snapshot.push(entry);
                            #[cfg(test)]
                            test_hook(trash, "trash-snapshot-entry");
                        }
                        Some(Err(error)) => {
                            scan.entries = None;
                            report.incomplete = true;
                            report.keep(trash, "enumeration_error", error);
                            break;
                        }
                        None => {
                            scan.entries = None;
                            break;
                        }
                    }
                }
                snapshot
            }
            Ok(_) => return Err(io::Error::other("non-directory retention trash retained")),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                if mount_local {
                    // A previous pass may have unlinked the container but failed
                    // to make that deletion durable. Retry the parent barrier.
                    crate::durability::sync_dir(trash.parent().unwrap())?;
                }
                return Ok(false);
            }
            Err(error) => return Err(error),
        }
    };
    if entries.is_empty() && !mount_local {
        return Ok(report.incomplete);
    }
    let protected = report
        .protections
        .get_or_insert_with(|| {
            crate::root_cache::retention_protections_with_budget(
                storage,
                budget,
                &mut report.protection_census,
            )
            .map(Arc::new)
            .map_err(Arc::new)
        })
        .clone();
    let protected = match protected {
        Ok(protected) => protected,
        Err(error) => {
            let reason = ProtectionFailure::from_error(&error);
            report.protection_failure = Some(reason);
            report.incomplete = true;
            report.entries += entries.len();
            report.keep(trash, reason.reason(), error);
            // An incomplete census authorizes no unlink or tree walk. Retry the
            // prefetched entries with fresh protections on the next pass.
            cursor.as_mut().unwrap().deferred.extend(entries);
            return Ok(true);
        }
    };
    let parent = crate::bash_background::persistence::PinnedDir::repair_open_path(
        trash
            .parent()
            .ok_or_else(|| io::Error::other("missing trash parent"))?,
    )
    .and_then(|parent| parent.repair_child_same_mount(std::ffi::OsStr::new(".retention-trash")));
    let mut pending = report.incomplete;
    let mut retained = false;
    let mut entries = entries.into_iter();
    while entries.len() != 0 {
        if let Err(error) = budget.enter(0) {
            pending = true;
            report.incomplete = true;
            report.keep(trash, "entry_budget", error);
            break;
        }
        let entry = entries.next().unwrap();
        cursor.as_mut().unwrap().omitted_prefix = true;
        report.entries += 1;
        let path = entry.path();
        let age = entry
            .metadata()
            .ok()
            .map(|metadata| {
                if metadata.is_dir() {
                    fs::symlink_metadata(path.join(RETIREMENT_WITNESS)).unwrap_or(metadata)
                } else {
                    metadata
                }
            })
            .and_then(|meta| meta.modified().ok())
            .and_then(|modified| std::time::SystemTime::now().duration_since(modified).ok())
            .map(|age| age.as_millis().min(u64::MAX as u128) as u64);
        let name = entry.file_name();
        let name = name.to_string_lossy();
        let mut bytes = 0;
        let mut measured = false;
        let result = (|| -> Result<(), (&str, io::Error)> {
            budget.enter(0).map_err(|error| ("budget", error))?;
            // Measure only safe, same-filesystem trees. Measurements omit unsafe
            // or unexamined trees and can precede a partially successful unlink.
            let oversized =
                match validate_tree_with_admission(&path, deadline, true, &|| budget.enter(0)) {
                    Ok(files) => {
                        bytes = files.iter().map(|(_, size)| *size).sum();
                        measured = true;
                        false
                    }
                    // A tree larger than one validation walk may examine could
                    // never be validated, so it would never be reclaimed. It is
                    // removed below by a walk that applies the same rules as it
                    // goes and keeps its progress when a pass ends.
                    Err(error) if is_tree_entry_limit(&error) => true,
                    Err(error) => return Err(("tree_guard", error)),
                };
            if !name.strip_prefix("retired-").is_some_and(|nonce| {
                nonce.len() == 32 && nonce.bytes().all(|byte| byte.is_ascii_hexdigit())
            }) {
                return Err(("foreign_name", io::Error::other("unrecognized trash entry")));
            }
            if mount_local
                && crate::cross_volume::is_mount_point(&path)
                    .map_err(|error| ("mount_guard", error))?
            {
                return Err(("mount_guard", io::Error::other("foreign trash mount")));
            }
            let parent = parent
                .as_ref()
                .map_err(|error| ("open_parent", io::Error::other(error.to_string())))?;
            let child = parent
                .repair_child(entry.file_name().as_os_str())
                .map_err(|error| ("open_child", error))?;
            child
                .repair_check_boundary(parent, &[])
                .map_err(|error| ("mount_guard", error))?;
            let expected = child
                .directory_identity()
                .map_err(|error| ("identity", error))?;
            verify_retirement_proof(storage, &child, &name["retired-".len()..], budget)
                .map_err(|error| ("witness", error))?;
            #[cfg(test)]
            test_hook(&storage.artifacts, "reap-root");
            let mut warnings = Vec::new();
            let mut removed = 0;
            let result = if oversized {
                parent.repair_remove_regular_child_with_budget(
                    entry.file_name().as_os_str(),
                    expected,
                    &protected,
                    &mut removed,
                    &mut warnings,
                    budget,
                )
            } else {
                parent.repair_remove_child_with_budget(
                    entry.file_name().as_os_str(),
                    expected,
                    &protected,
                    &mut removed,
                    &mut warnings,
                    budget,
                )
            };
            for warning in warnings {
                crate::slog_warn!("{warning}");
            }
            match result {
                Ok(()) => {}
                // What this pass removed stays removed, and the witness is
                // deleted last, so the next pass verifies the rest and
                // continues instead of starting over.
                Err(error)
                    if oversized
                        && crate::bash_background::persistence::is_repair_budget_stop(&error) =>
                {
                    return Err((
                        "removal_in_progress",
                        io::Error::new(
                            error.kind(),
                            format!(
                                "oversized entry: removed {removed} entries this pass, \
                                 later passes continue: {error}"
                            ),
                        ),
                    ));
                }
                Err(error)
                    if error.get_ref().is_some_and(|inner| {
                        inner.is::<crate::bash_background::persistence::NonRegularEntryRetained>()
                    }) =>
                {
                    return Err(("tree_guard", error));
                }
                Err(error) => return Err(("unlink_guard", error)),
            }
            match parent.repair_child(entry.file_name().as_os_str()) {
                Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
                Err(error) => Err(("unlink_status", error)),
                Ok(_) => Err((
                    "unlink_pending",
                    io::Error::other("directory still present after unlink"),
                )),
            }
        })();
        if let Err((reason, error)) = result {
            pending = true;
            retained = true;
            report.incomplete |= !measured;
            report.bytes_estimate = report.bytes_estimate.saturating_add(bytes);
            if let Some(age) = age {
                report.oldest_age_ms = Some(report.oldest_age_ms.unwrap_or(0).max(age));
            }
            report.keep(&path, reason, error);
        } else {
            report.entries -= 1;
            report.removed_entries += 1;
        }
    }
    // A stopped unlink/measurement lane must retry its prefetched tail before
    // advancing the directory iterator in a later pass.
    for entry in entries.rev() {
        cursor.as_mut().unwrap().deferred.push_front(entry);
    }
    if mount_local {
        // Never recursively delete the container: a concurrent retirement may
        // have populated it since the snapshot. An empty-directory unlink is
        // safe outside admission and leaves any new tombstones for a later pass.
        match fs::remove_dir(trash) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) if error.kind() == io::ErrorKind::DirectoryNotEmpty => {
                pending = true;
                // DirectoryNotEmpty is expected when a scanned entry was
                // retained, not an incomplete census. Otherwise an unseen
                // entry arrived, so the census must be marked incomplete.
                if !retained {
                    report.incomplete = true;
                }
                report.keep(trash, "container_not_empty", error);
            }
            Err(error) => return Err(error),
        }
        // Flush the remaining container's unlinks, then the container's own
        // deletion in its mount-local parent before clearing the durable hint.
        match crate::durability::sync_dir(trash) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        crate::durability::sync_dir(trash.parent().unwrap())?;
    }
    Ok(pending)
}

fn reap_retired(storage: &StoragePaths) -> io::Result<()> {
    let cancelled = states()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(&storage.artifacts)
        .and_then(|state| state.reaper_cancelled.clone())
        .unwrap_or_else(|| Arc::new(|| false));
    let deadline = Instant::now() + Duration::from_secs(60);
    let mut budget = crate::bash_background::persistence::RepairBudget::until(deadline, cancelled);
    let mut report = TrashReport {
        at_ms: crate::pins::now_ms(),
        ..Default::default()
    };
    let result = reap_retired_with_budget(storage, deadline, &mut budget, &mut report);
    // The census belongs to this pass only; a later pass takes a fresh one.
    report.protections = None;
    if let Err(error) = &result {
        report.incomplete = true;
        if report.kept_by_reason.is_empty() {
            report.keep(&storage.artifacts, "reaper_pass", error);
        }
    }
    let report = {
        let mut states = states()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let state = states.entry(storage.artifacts.clone()).or_default();
        state.record_trash(report, Instant::now());
        state.trash.clone()
    };
    crate::slog_info!(
        "storage retention trash root={} report={}",
        storage.artifacts.display(),
        serde_json::to_string(&report).unwrap_or_default()
    );
    result
}

fn reap_retired_with_budget(
    storage: &StoragePaths,
    deadline: Instant,
    budget: &mut crate::bash_background::persistence::RepairBudget,
    report: &mut TrashReport,
) -> io::Result<()> {
    crate::root_cache::check_cache_access(&storage.artifacts)?;
    if !crate::root_cache::storage_allows_root_keyed(&storage.durable)? {
        return Err(io::Error::other("network storage retained"));
    }
    // Different daemon processes may discover the same tombstones. This lock
    // is separate from admission: slow unlinks must not block binds or pins.
    let lock = storage.durable.join("retention/reap.lock");
    let _lease = crate::fs_lock::try_acquire(&lock, Duration::ZERO).map_err(|error| {
        report.keep(&lock, "reap_lock", &error);
        io::Error::other(error.to_string())
    })?;
    let marker = storage.durable.join("retention/mount-trash-pending");
    let mount_scan = match fs::read(&marker) {
        Ok(generation) => Some(generation),
        Err(error) if error.kind() == io::ErrorKind::NotFound => None,
        Err(error) => return Err(error),
    };
    let mut mount_trash_pending = false;
    for (domain, flat) in domain_paths(storage)? {
        budget.enter(0)?;
        if flat {
            continue;
        }
        let trash = domain.join(".retention-trash");
        if let Err(error) = reap_trash(storage, &trash, false, deadline, budget, report) {
            report.incomplete = true;
            report.keep(&trash, "trash_container", &error);
            return Err(error);
        }
        if mount_scan.is_none() {
            continue;
        }
        let entries = match fs::read_dir(&domain) {
            Ok(entries) => entries,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error),
        };
        let mut entries = entries;
        loop {
            budget.entry()?;
            let Some(entry) = entries.next() else { break };
            let entry = entry?;
            let cache = entry.path();
            if !key_valid(&entry.file_name().to_string_lossy()) || !entry.file_type()?.is_dir() {
                continue;
            }
            #[cfg(test)]
            test_hook(&storage.artifacts, "mount-trash-probe");
            let mounted = match crate::cross_volume::is_mount_point(&cache) {
                Ok(mounted) => mounted,
                // A normal cache can be retired while the reaper lists mounts.
                Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                Err(error) => return Err(error),
            };
            if mounted {
                let trash = cache.join(".retention-trash");
                match reap_trash(storage, &trash, true, deadline, budget, report) {
                    Ok(pending) => mount_trash_pending |= pending,
                    Err(error) => {
                        report.incomplete = true;
                        report.keep(&trash, "trash_container", &error);
                        return Err(error);
                    }
                }
            }
        }
    }
    if !mount_trash_pending {
        if let Some(scanned) = mount_scan {
            #[cfg(test)]
            test_hook(&storage.artifacts, "mount-trash-scanned");
            let Ok(_admission) = crate::fs_lock::try_acquire(
                &storage.durable.join("retention/sweep.lock"),
                Duration::ZERO,
            ) else {
                report.keep(&marker, "marker_admission_busy", "sweep admission is busy");
                return Ok(());
            };
            // Retirements publish under the same sweep.lock held by _admission.
            // Keep a newer generation: it may describe trash created after this
            // reaper scanned that domain.
            match fs::read(&marker) {
                Ok(current) if current == scanned => {
                    fs::remove_file(&marker)?;
                    crate::durability::sync_dir(&storage.durable.join("retention"))?;
                }
                Ok(_) => {
                    report.incomplete = true;
                    report.keep(
                        &marker,
                        "marker_generation_changed",
                        "a concurrent retirement published more trash",
                    );
                }
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(error),
            }
        }
    }
    Ok(())
}

fn schedule_reaper(storage: &StoragePaths) {
    {
        let mut states = states()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let state = states.entry(storage.artifacts.clone()).or_default();
        if !state.reaper_ready(Instant::now()) {
            return;
        }
        state.reaping = true;
    }
    #[cfg(test)]
    test_hook(&storage.artifacts, "reaper-launch");
    let storage = storage.clone();
    let worker_storage = storage.clone();
    let spawned = std::thread::Builder::new()
        .name("aft-retention-reaper".into())
        .spawn(move || {
            struct Reset(PathBuf);
            impl Drop for Reset {
                fn drop(&mut self) {
                    if let Some(state) = states()
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .get_mut(&self.0)
                    {
                        state.reaping = false;
                    }
                }
            }
            let _reset = Reset(worker_storage.artifacts.clone());
            if let Err(error) = reap_retired(&worker_storage) {
                crate::slog_warn!(
                    "storage retention reaper root={} error={error}",
                    worker_storage.artifacts.display()
                );
            }
        });
    if let Err(error) = spawned {
        states()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .entry(storage.artifacts.clone())
            .or_default()
            .reaping = false;
        crate::slog_warn!(
            "storage retention reaper spawn root={} error={error}",
            storage.artifacts.display()
        );
    }
}

fn root_resident(
    storage: &StoragePaths,
    key: &str,
    history: &BTreeMap<String, Vec<Binding>>,
) -> bool {
    crate::root_cache::protected_read_marker_exists(&storage.durable.join("retention"), key)
        || history.get(key).is_some_and(|records| {
            records.iter().any(|binding| {
                crate::root_cache::protected_read_marker_exists(
                    &storage.durable.join("retention"),
                    &crate::path_identity::project_scope_key(&binding.root),
                )
            })
        })
}

/// The artifact-owner manifests under a storage folder, read once and then
/// consulted for many keys. Each manifest keeps its own read result, so a
/// key is refused on an unreadable manifest exactly when a fresh scan in the
/// same order would have hit it before finding a protecting owner.
struct OwnerManifests(
    io::Result<
        Vec<(
            std::ffi::OsString,
            io::Result<crate::artifact_owner::ArtifactOwnerManifest>,
        )>,
    >,
);

impl OwnerManifests {
    fn read(storage: &StoragePaths) -> Self {
        Self::read_with_admission(storage, false)
    }

    fn read_with_admission(storage: &StoragePaths, admission: bool) -> Self {
        #[cfg(test)]
        test_hook(&storage.artifacts, "owner-scan");
        Self(
            bounded_dirs(&storage.durable.join("artifact-owners"), HISTORY_ENTRIES).and_then(
                |owners| {
                    let mut records = Vec::new();
                    for owner in owners {
                        if admission {
                            yield_to_bind(&storage.durable)?;
                        }
                        let path = owner.path().join("owner.json");
                        if path.exists() {
                            records.push((owner.file_name(), read_json(&path)));
                        }
                    }
                    Ok(records)
                },
            ),
        )
    }

    /// Whether a live owner protects `key`. `known` says whether the key has
    /// binding history; without it, any protected owner lacking a residency
    /// marker protects the key as well.
    fn protect(&self, storage: &StoragePaths, key: &str, known: bool) -> io::Result<bool> {
        let owners = self
            .0
            .as_ref()
            .map_err(|error| io::Error::new(error.kind(), error.to_string()))?;
        for (name, metadata) in owners {
            let metadata = metadata
                .as_ref()
                .map_err(|error| io::Error::new(error.kind(), error.to_string()))?;
            if ((!known && !owner_has_residency_marker(storage, metadata))
                || name == key
                || metadata.project_scope_key == key)
                && crate::artifact_owner::protected_for_retention(metadata)
            {
                return Ok(true);
            }
        }
        Ok(false)
    }
}

/// Most due caches one admission recheck covers. Prepared caches keep their
/// leases until that recheck, so this also bounds how many caches a pass
/// holds locked at once.
const ADMISSION_BATCH: usize = 16;

/// Due caches that passed every check that runs without the retention
/// barrier, waiting with their leases held for one shared admission recheck.
///
/// The recheck needs the binding history read under the barrier, and that
/// history is every binding record and owner manifest in the storage. Reading
/// it once per batch, instead of once per cache, keeps a backlog drain from
/// rereading thousands of records for every cache it removes. The barrier is
/// still taken only for the recheck and rename, never across the walks.
#[derive(Default)]
struct DueBatch {
    items: Vec<PreparedDue>,
    /// Owner manifests for the checks made before the retention barrier
    /// (`retention/sweep.lock`, which binds and pins also take), read once
    /// per pass. The admission recheck reads them again while holding the
    /// barrier, just before each rename.
    owners: Option<OwnerManifests>,
    /// Set while the due-first lane prepares caches, so their outcomes also
    /// count toward [`SweepReport::due_first`].
    due_lane: bool,
}

struct PreparedDue {
    directory: PathBuf,
    key: String,
    leases: Vec<crate::fs_lock::LockGuard>,
    due_lane: bool,
    deadline: Instant,
}

impl DueBatch {
    fn full(&self) -> bool {
        self.items.len() >= ADMISSION_BATCH
    }

    /// Recheck and retire every prepared cache under one barrier hold. Each
    /// cache's outcome is counted for its own domain, as it would have been
    /// had it been retired when its lane met it.
    fn flush(
        &mut self,
        storage: &StoragePaths,
        roots: &mut RootIdentities,
        now: u64,
        deadline: Instant,
        clock: &dyn Fn() -> Instant,
        cancelled: &dyn Fn() -> bool,
        report: &mut SweepReport,
    ) -> io::Result<()> {
        if self.items.is_empty() {
            return Ok(());
        }
        let items = std::mem::take(&mut self.items);
        // Cancelled or expired caches stay on disk for the next pass. Skip the
        // lock and metadata rereads so root binding and cache pinning are not
        // delayed by work that can no longer be retired.
        if report.cancelled {
            return Ok(());
        }
        if clock() >= deadline {
            report.stopped_early = true;
            return Ok(());
        }
        #[cfg(test)]
        test_hook(&storage.artifacts, "due-ready");
        if bind_pending(&storage.durable) {
            for item in items {
                let before = DomainReport::from_report(report);
                report.skipped_protected += 1;
                item.attribute(storage, &before, report)?;
            }
            return Ok(());
        }
        // Do not hold bind admission while waiting for SQLite's open gate.
        // Lock acquisition below is nonblocking, avoiding an inverted wait
        // against older retirement paths that take these locks in reverse.
        let files = crate::db::file_identity::filesystem_guard();
        let phase = Instant::now();
        // Try the retention barrier without waiting. If a bind or pin holds it,
        // that caller is protecting this storage right now, so retention backs
        // off instead of spending its pass budget waiting.
        let admission = crate::fs_lock::try_acquire(
            &storage.durable.join("retention/sweep.lock"),
            Duration::ZERO,
        )
        .map_err(|error| io::Error::other(error.to_string()))
        .and_then(|barrier| {
            let artifacts = crate::fs_lock::try_acquire(
                &storage.artifacts.join(".retention-sweep.lock"),
                Duration::ZERO,
            )
            .map_err(io::Error::other)?;
            let barrier = Barrier {
                _metadata: barrier,
                _artifacts: artifacts,
            };
            #[cfg(test)]
            test_hook(&storage.artifacts, "due-admitted");
            if cancelled() {
                report.cancelled = true;
                return Ok(None);
            }
            if clock() >= deadline {
                report.stopped_early = true;
                return Ok(None);
            }
            yield_to_bind(&storage.durable)?;
            Ok(Some((
                barrier,
                bindings_with_admission(storage, true, roots)?,
                OwnerManifests::read_with_admission(storage, true),
            )))
        });
        *report
            .timings_ms
            .entry("due_admission_history".into())
            .or_default() += phase.elapsed().as_millis() as u64;
        let (barrier, current, owners) = match admission {
            Ok(Some(admission)) => admission,
            Ok(None) => return Ok(()),
            Err(error) => {
                for item in items {
                    let before = DomainReport::from_report(report);
                    item.refuse(&error, report);
                    item.attribute(storage, &before, report)?;
                }
                return Ok(());
            }
        };
        let mut retired = Vec::new();
        for item in items {
            if clock() >= deadline {
                report.stopped_early = true;
                break;
            }
            if yield_to_bind(&storage.durable).is_err() {
                break;
            }
            let before = DomainReport::from_report(report);
            let cache = item.directory.join(&item.key);
            let directory = item.directory.clone();
            let due_lane = item.due_lane;
            let key = item.key.clone();
            let deadline = item.deadline;
            if !report.cancelled {
                if let Err(error) = item.retire(storage, &current, &owners, now, cancelled, report)
                {
                    report.skipped_protected += 1;
                    if error.kind() != io::ErrorKind::Interrupted && report.errors.len() < 16 {
                        report.errors.push(format!("{}: {error}", cache.display()));
                    }
                }
            }
            if report.removed_roots > before.removed_roots {
                retired.push((key, deadline));
            }
            attribute_due(storage, &directory, due_lane, &before, report)?;
        }
        drop(barrier);
        #[cfg(test)]
        test_hook(&storage.durable, "due-released");
        drop(files);
        // These lookups can walk legacy flat cache domains. The roots are
        // already retired; a bind must not queue behind this bookkeeping.
        for (key, deadline) in retired {
            if !cache_key_present(storage, &key, deadline)? {
                clear_unknown_observation(storage, &key)?;
            }
        }
        Ok(())
    }
}

fn attribute_due(
    storage: &StoragePaths,
    directory: &Path,
    due_lane: bool,
    before: &DomainReport,
    report: &mut SweepReport,
) -> io::Result<()> {
    let delta = DomainReport::from_report(report).difference(before);
    if due_lane {
        report.due_first.add(&delta);
    }
    report
        .domains
        .entry(
            relative_domain(storage, &directory)?
                .to_string_lossy()
                .into_owned(),
        )
        .or_default()
        .add(&delta);
    Ok(())
}

impl PreparedDue {
    fn attribute(
        &self,
        storage: &StoragePaths,
        before: &DomainReport,
        report: &mut SweepReport,
    ) -> io::Result<()> {
        attribute_due(storage, &self.directory, self.due_lane, before, report)
    }

    /// The retention barrier (`retention/sweep.lock`) could not be taken: the
    /// cache stays, counted as `skipped_protected` with the error and the
    /// cache path, as a lane reports any candidate whose checks failed.
    fn refuse(&self, error: &io::Error, report: &mut SweepReport) {
        report.skipped_protected += 1;
        if error.kind() != io::ErrorKind::Interrupted && report.errors.len() < 16 {
            report.errors.push(format!(
                "{}: {error}",
                self.directory.join(&self.key).display()
            ));
        }
    }

    /// Recheck against history read while holding sweep.lock, which both old
    /// and new binaries take before recording a binding.
    fn retire(
        self,
        storage: &StoragePaths,
        current: &BTreeMap<String, Vec<Binding>>,
        owners: &OwnerManifests,
        now: u64,
        cancelled: &dyn Fn() -> bool,
        report: &mut SweepReport,
    ) -> io::Result<()> {
        let Self {
            directory,
            key,
            mut leases,
            deadline: _,
            ..
        } = self;
        let cache = directory.join(&key);
        let key = key.as_str();
        if !eligible(storage, key, current, now, ROOT_GRACE_MS, report)?
            || root_resident(storage, key, current)
            || crate::root_cache::live_scope_keys_for_storage(&storage.durable).contains(key)
            || owners.protect(storage, key, current.contains_key(key))?
            || protected(&cache)?
            || legacy_import_source_pinned(storage, &cache, key)
        {
            report.skipped_protected += 1;
            return Ok(());
        }
        if crate::db::file_identity::has_open_connections_under(&cache) {
            report.skipped_protected += 1;
            return Ok(());
        }
        if cancelled() {
            report.cancelled = true;
            return Ok(());
        }
        yield_to_bind(&storage.durable)?;
        let phase = Instant::now();
        for lease in &mut leases {
            lease.stop_heartbeat_for_removal();
        }
        retire_directory(&storage.durable, &cache)?;
        *report.timings_ms.entry("due_rename".into()).or_default() +=
            phase.elapsed().as_millis() as u64;
        report.removed_roots += 1;
        report.unmeasured_removed_roots += 1;
        if directory == storage.artifacts.join("views") {
            report.due_views_remaining = report.due_views_remaining.saturating_sub(1);
        }
        // Lease guards address the old pathname. Drop them before releasing
        // admission so they cannot touch a newly bound replacement at that path.
        drop(leases);
        Ok(())
    }
}

/// A cache whose every binding is past its grace is about to be retired whole,
/// so it skips the obsolete-generation GC and the byte-measuring tree walks
/// that other candidates get. Validate its tree once without measuring, then
/// queue it in `batch` for the admission recheck that retires it.
fn prepare_due_cache(
    storage: &StoragePaths,
    directory: &Path,
    key: &str,
    history: &BTreeMap<String, Vec<Binding>>,
    deadline: Instant,
    batch: &mut DueBatch,
    report: &mut SweepReport,
) -> io::Result<()> {
    let cache = directory.join(key);
    let mut leases = Vec::new();
    let local_writer = crate::root_cache::local_callgraph_writer(&cache);
    for name in [
        "writer.lease",
        "cache.lock",
        "symbols.lock",
        "semantic.lock",
    ] {
        if name == "writer.lease"
            && directory
                .file_name()
                .is_some_and(|name| name == "callgraph")
            && local_writer.is_some()
        {
            continue;
        }
        match crate::fs_lock::try_acquire(&cache.join(name), Duration::ZERO) {
            Ok(lease) => leases.push(lease),
            Err(_) => {
                report.skipped_protected += 1;
                return Ok(());
            }
        }
    }
    if root_resident(storage, key, history)
        || batch
            .owners
            .get_or_insert_with(|| OwnerManifests::read(storage))
            .protect(storage, key, true)?
        || protected(&cache)?
    {
        report.skipped_protected += 1;
        return Ok(());
    }
    if crate::root_cache::live_scope_keys_for_storage(&storage.durable).contains(key) {
        report.skipped_live += 1;
        return Ok(());
    }
    let phase = Instant::now();
    validate_tree(&cache, deadline, false)?;
    *report
        .timings_ms
        .entry("due_validation".into())
        .or_default() += phase.elapsed().as_millis() as u64;
    batch.items.push(PreparedDue {
        directory: directory.to_path_buf(),
        key: key.to_owned(),
        leases,
        due_lane: batch.due_lane,
        deadline,
    });
    Ok(())
}

pub(crate) fn remove_directory_if_closed(cache: &Path) -> io::Result<()> {
    remove_directory_if_closed_with(cache, |_| Ok(()))
}

pub(crate) fn remove_directory_if_closed_with(
    cache: &Path,
    before_remove: impl FnOnce(&crate::bash_background::persistence::PinnedDir) -> io::Result<()>,
) -> io::Result<()> {
    let access = crate::root_cache::pin_owned_cache_path(cache)?;
    #[cfg(test)]
    test_hook(cache, "remove-pinned");
    let (parent, name) = access.parent()?;
    let child = parent.repair_child_same_mount(&name)?;
    child.repair_check_boundary(&access.root, &[])?;
    let expected = child.directory_identity()?;
    let protected = crate::root_cache::cache_deletion_protections(cache)?;
    tree_files(cache, Instant::now() + PASS_BUDGET)?;
    let _files = crate::db::file_identity::filesystem_guard();
    // Eviction clears its pointer through this same admitted handle before
    // payload removal; a still-open payload database can then retain the tree.
    before_remove(&child)?;
    if crate::db::file_identity::has_open_connections_under(cache) {
        return Err(io::Error::other("open SQLite cache retained"));
    }
    #[cfg(test)]
    test_hook(cache, "remove-root");
    parent.repair_remove_child(&name, expected, &protected, &mut 0)
}

pub(crate) fn retention_tree_bytes(directory: &Path, deadline: Instant) -> io::Result<u64> {
    tree_files(directory, deadline).map(|files| files.iter().map(|(_, bytes)| bytes).sum())
}

fn eligible(
    storage: &StoragePaths,
    key: &str,
    history: &BTreeMap<String, Vec<Binding>>,
    now: u64,
    unknown_grace_ms: u64,
    report: &mut SweepReport,
) -> io::Result<bool> {
    if let Some(records) = history.get(key) {
        if records
            .iter()
            .any(|binding| !matches!(binding.root.try_exists(), Ok(false)))
        {
            report.skipped_live += 1;
            return Ok(false);
        }
        if records.iter().all(|binding| missing_and_old(binding, now)) {
            return Ok(true);
        }
        if records.iter().any(|binding| !mount_matches(binding)) {
            report.skipped_mount_mismatch += 1;
        } else {
            report.skipped_recent += 1;
        }
        return Ok(false);
    }
    let path = storage
        .durable
        .join("retention/unknown")
        .join(format!("{key}.json"));
    // The first observation is an on-disk clock, not scheduling state. Reload
    // it on every pass so daemon replacement cannot restart the root grace.
    let first_seen = match read_json::<u64>(&path) {
        Ok(first_seen) => first_seen,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            atomic_json(&path, &now)?;
            now
        }
        Err(error) => return Err(error),
    };
    if now.saturating_sub(first_seen) >= unknown_grace_ms {
        return Ok(true);
    }
    report.skipped_recent += 1;
    Ok(false)
}

/// The view scopes every family registry lists, read at most once per pass
/// and only when a pass meets a view folder with no binding history.
#[derive(Default)]
struct ViewMembers(Option<Result<HashSet<String>, String>>);

impl ViewMembers {
    /// `Err` when any family registry could not be read completely: a scope
    /// that registry might list is then unknown, never absent.
    fn contains(&mut self, storage: &StoragePaths, scope: &str) -> Result<bool, String> {
        self.0
            .get_or_insert_with(|| view_member_scopes(storage).map_err(|error| error.to_string()))
            .as_ref()
            .map(|scopes| scopes.contains(scope))
            .map_err(Clone::clone)
    }
}

fn view_member_scopes(storage: &StoragePaths) -> io::Result<HashSet<String>> {
    let mut scopes = HashSet::new();
    for entry in bounded_dirs(&storage.artifacts.join("blobs/v2"), TREE_ENTRIES)? {
        if !entry
            .path()
            .join(crate::views::registry::REGISTRY_FILE)
            .exists()
        {
            continue;
        }
        let family = entry.file_name().to_string_lossy().into_owned();
        let registry =
            crate::views::registry::FamilyRegistry::open_existing(&storage.artifacts, &family)
                .map_err(|error| io::Error::other(format!("family registry {family}: {error}")))?
                .ok_or_else(|| io::Error::other(format!("family registry {family} vanished")))?;
        let members = registry
            .members_bounded(HISTORY_ENTRIES + 1)
            .map_err(|error| io::Error::other(format!("family registry {family}: {error}")))?;
        if members.len() > HISTORY_ENTRIES {
            return Err(io::Error::other(format!(
                "family registry {family} lists more than {HISTORY_ENTRIES} views"
            )));
        }
        scopes.extend(members.into_iter().map(|member| member.scope));
    }
    Ok(scopes)
}

fn report_candidate_error(cache: &Path, error: &io::Error, report: &mut SweepReport) {
    // An unfinished walk has not established protection. Keep its reason and
    // mark the pass incomplete, without conflating filesystem timeouts with
    // the collector's own deadline.
    if is_retention_deadline(error) {
        report.stopped_early = true;
    } else {
        report.skipped_protected += 1;
    }
    if report.errors.len() < 16 {
        report.errors.push(format!("{}: {error}", cache.display()));
    }
}

fn candidate(
    storage: &StoragePaths,
    roots: &mut RootIdentities,
    directory: &Path,
    key: &str,
    flat: bool,
    history: &BTreeMap<String, Vec<Binding>>,
    now: u64,
    deadline: Instant,
    cancelled: &dyn Fn() -> bool,
    members: &mut ViewMembers,
    batch: &mut DueBatch,
    report: &mut SweepReport,
) -> io::Result<()> {
    let before = DomainReport::from_report(report);
    let mut orphan = false;
    let result = candidate_inner(
        storage,
        roots,
        directory,
        key,
        flat,
        history,
        now,
        deadline,
        cancelled,
        members,
        batch,
        &mut orphan,
        report,
    );
    if orphan {
        let mut delta = DomainReport::from_report(report).difference(&before);
        delta.examined += 1;
        if result
            .as_ref()
            .is_err_and(|error| !is_retention_deadline(error))
        {
            // The caller counts non-deadline errors as protected after this
            // wrapper returns. Mirror that count in the orphan subtotal.
            delta.skipped_protected += 1;
        }
        report.orphan_views.add(&delta);
    }
    result
}

fn candidate_inner(
    storage: &StoragePaths,
    roots: &mut RootIdentities,
    directory: &Path,
    key: &str,
    flat: bool,
    history: &BTreeMap<String, Vec<Binding>>,
    now: u64,
    deadline: Instant,
    cancelled: &dyn Fn() -> bool,
    members: &mut ViewMembers,
    batch: &mut DueBatch,
    orphan: &mut bool,
    report: &mut SweepReport,
) -> io::Result<()> {
    let cache = if flat {
        directory.to_path_buf()
    } else {
        directory.join(key)
    };
    if !flat && retired_mount_point(&cache) {
        // Only the mount point and possibly its queued trash are left.
        return Ok(());
    }
    if !flat
        && history
            .get(key)
            .is_some_and(|records| records.iter().all(|binding| missing_and_old(binding, now)))
    {
        return prepare_due_cache(storage, directory, key, history, deadline, batch, report);
    }
    let flat_pin = flat
        .then(|| crate::bash_background::persistence::PinnedDir::repair_open_path(&cache))
        .transpose()?;
    let mut leases = Vec::new();
    let local_writer = crate::root_cache::local_callgraph_writer(&cache);
    for name in [
        "writer.lease",
        "cache.lock",
        "symbols.lock",
        "semantic.lock",
    ] {
        if name == "writer.lease"
            && directory
                .file_name()
                .is_some_and(|domain| domain == "callgraph")
            && local_writer.is_some()
        {
            continue;
        }
        match crate::fs_lock::try_acquire(&cache.join(name), Duration::ZERO) {
            Ok(lease) => leases.push(lease),
            Err(_) => {
                report.skipped_protected += 1;
                return Ok(());
            }
        }
    }
    let before = tree_files(&cache, deadline)?;
    if directory
        .file_name()
        .is_some_and(|name| name == "callgraph")
    {
        let pointer = cache.join(format!("{key}.current"));
        if let Ok(current) = fs::read_to_string(pointer) {
            report.removed_generations +=
                crate::callgraph_store::gc_old_generations(&cache, key, current.trim());
        }
    }
    let after = tree_files(&cache, deadline)?;
    let old_bytes: u64 = before.iter().map(|(_, bytes)| bytes).sum();
    let new_bytes: u64 = after.iter().map(|(_, bytes)| bytes).sum();
    report.removed_bytes += old_bytes.saturating_sub(new_bytes);
    // Residency protects the root, not unheld obsolete generations. An owner
    // can be alive while using a different plane in the same repository family.
    if crate::root_cache::protected_read_marker_exists(&storage.durable.join("retention"), key)
        || history.get(key).is_some_and(|records| {
            records.iter().any(|binding| {
                crate::root_cache::protected_read_marker_exists(
                    &storage.durable.join("retention"),
                    &crate::path_identity::project_scope_key(&binding.root),
                )
            })
        })
    {
        report.skipped_protected += 1;
        return Ok(());
    }
    if crate::root_cache::live_scope_keys_for_storage(&storage.durable).contains(key) {
        report.skipped_live += 1;
        return Ok(());
    }
    for owner in bounded_dirs(&storage.durable.join("artifact-owners"), HISTORY_ENTRIES)? {
        let path = owner.path().join("owner.json");
        if !path.exists() {
            continue;
        }
        let metadata: crate::artifact_owner::ArtifactOwnerManifest = read_json(&path)?;
        if ((!history.contains_key(key) && !owner_has_residency_marker(storage, &metadata))
            || owner.file_name() == key
            || metadata.project_scope_key == key)
            && crate::artifact_owner::protected_for_retention(&metadata)
        {
            report.skipped_protected += 1;
            return Ok(());
        }
    }
    // A checkout view folder with neither binding history nor a registry
    // member is an orphan: it gets a short observation grace instead of the
    // week reserved for keys whose checkout might still come back. A folder a
    // registry lists keeps the week; an unreadable registry keeps everything.
    let mut unknown_grace_ms = ROOT_GRACE_MS;
    if !flat
        && directory.file_name().is_some_and(|name| name == "views")
        && !history.contains_key(key)
    {
        match members.contains(storage, key) {
            Ok(true) => {}
            Ok(false) => {
                *orphan = true;
                unknown_grace_ms = ORPHAN_VIEW_GRACE_MS;
            }
            Err(error) => {
                *orphan = true;
                report.skipped_unknown += 1;
                if report.errors.len() < 16 {
                    report
                        .errors
                        .push(format!("{}: orphan check: {error}", cache.display()));
                }
                return Ok(());
            }
        }
    }
    if !eligible(storage, key, history, now, unknown_grace_ms, report)? {
        return Ok(());
    }
    if !history.contains_key(key)
        && before.iter().any(|(path, _)| {
            path.file_name().is_none_or(|name| {
                !matches!(
                    name.to_str(),
                    Some("writer.lease" | "cache.lock" | "semantic.lock" | "symbols.lock")
                )
            }) && fs::metadata(path)
                .and_then(|metadata| metadata.modified())
                // A file whose modification time cannot be read counts as
                // recent for an orphan, so the orphan is kept: the file may be
                // changing right now. For any other key this check runs only
                // after the week-long observation grace has passed, and an
                // undatable file (usually one removed since the walk) is
                // treated as old, as it was before the orphan rule existed.
                .map_or(*orphan, |mtime| {
                    std::time::SystemTime::now()
                        .duration_since(mtime)
                        .unwrap_or_default()
                        < Duration::from_millis(unknown_grace_ms)
                })
        })
    {
        report.skipped_recent += 1;
        return Ok(());
    }
    // Walks run without either admission or SQLite's process-wide open gate.
    // Both old and new binders use sweep.lock. Reread history under it, but
    // never wait for SQLite's open gate while holding binding admission.
    // Recheck protection and binding history under admission immediately before
    // deletion, since a configure or query may have arrived during the walks.
    // A registration binds its root under this barrier before it creates a
    // view folder, so an orphan that gained history here is no longer one.
    // With a separate cache root the cache-side lock is taken too, as in
    // `cache_barrier`, without waiting.
    #[cfg(test)]
    test_hook(&storage.artifacts, "candidate-ready");
    yield_to_bind(&storage.durable)?;
    let files_guard = crate::db::file_identity::filesystem_guard();
    let admission = crate::fs_lock::try_acquire(
        &storage.durable.join("retention/sweep.lock"),
        Duration::ZERO,
    )
    .map_err(|error| io::Error::other(error.to_string()))
    .and_then(|metadata| {
        let artifacts = crate::fs_lock::try_acquire(
            &storage.artifacts.join(".retention-sweep.lock"),
            Duration::ZERO,
        )
        .map_err(io::Error::other)?;
        Ok(Barrier {
            _metadata: metadata,
            _artifacts: artifacts,
        })
    })?;
    let current_history = bindings_with_admission(storage, true, roots)?;
    if !eligible(
        storage,
        key,
        &current_history,
        now,
        unknown_grace_ms,
        report,
    )? || crate::root_cache::live_scope_keys_for_storage(&storage.durable).contains(key)
        || protected(&cache)?
        || legacy_import_source_pinned(storage, &cache, key)
    {
        report.skipped_protected += 1;
        return Ok(());
    }
    if crate::db::file_identity::has_open_connections_under(&cache) {
        report.skipped_protected += 1;
        return Ok(());
    }
    if cancelled() {
        report.cancelled = true;
        return Ok(());
    }
    yield_to_bind(&storage.durable)?;
    if flat {
        // Legacy domains contain several roots. Delete only the key's file set,
        // never the directory or another key's files/coordination records.
        #[cfg(test)]
        test_hook(&cache, "flat-unlink");
        for (path, bytes) in after {
            if path.parent() == Some(cache.as_path())
                && path
                    .file_name()
                    .is_some_and(|name| name.to_string_lossy().starts_with(&format!("{key}.")))
            {
                flat_pin
                    .as_ref()
                    .unwrap()
                    .repair_remove_file(path.file_name().unwrap())?;
                report.removed_bytes += bytes;
            }
        }
    } else {
        for lease in &mut leases {
            lease.stop_heartbeat_for_removal();
        }
        retire_directory(&storage.durable, &cache)?;
        report.removed_bytes += before
            .iter()
            .map(|(_, bytes)| bytes)
            .sum::<u64>()
            .min(new_bytes);
    }
    report.removed_roots += 1;
    drop(leases);
    drop(admission);
    drop(files_guard);
    // The observation is shared by every plane for this key. Keep its original
    // grace until the last cache is gone, rather than restarting it per plane.
    if !cache_key_present(storage, key, deadline)? {
        clear_unknown_observation(storage, key)?;
    }
    Ok(())
}

pub(crate) fn run_pass(storage: &StoragePaths, cancelled: &dyn Fn() -> bool) -> SweepReport {
    run_pass_with_clock(storage, cancelled, &Instant::now)
}

/// Keys whose every binding is missing and past its grace, in key order.
fn due_keys(history: &BTreeMap<String, Vec<Binding>>, now: u64) -> Vec<String> {
    history
        .iter()
        .filter(|(key, records)| {
            key_valid(key)
                // Checking that every root is gone takes one stat per record,
                // so it runs first. missing_and_old may also probe the volume
                // the root was on, and runs only for keys that pass it.
                && records
                    .iter()
                    .all(|binding| matches!(binding.root.try_exists(), Ok(false)))
                && records.iter().all(|binding| missing_and_old(binding, now))
        })
        .map(|(key, _)| key.clone())
        .collect()
}

/// Runs the full candidate checks on the checkout caches of keys that are
/// already due, starting after the key the previous pass ended on.
fn due_first(
    storage: &StoragePaths,
    roots: &mut RootIdentities,
    domains: &[(PathBuf, bool)],
    history: &BTreeMap<String, Vec<Binding>>,
    deadline: Instant,
    clock: &dyn Fn() -> Instant,
    cancelled: &dyn Fn() -> bool,
    members: &mut ViewMembers,
    batch: &mut DueBatch,
    report: &mut SweepReport,
) -> io::Result<()> {
    let mut caches = Vec::new();
    let phase = Instant::now();
    let keys = due_keys(history, report.at_ms);
    report
        .timings_ms
        .insert("due_keys".into(), phase.elapsed().as_millis() as u64);
    for key in keys {
        let mut present = false;
        for (directory, flat) in domains {
            // A flat domain keeps the files of many keys side by side in one
            // directory, with no folder per key, so this per-key folder check
            // does not apply. The lexical walk handles those file sets.
            if *flat {
                continue;
            }
            if fs::symlink_metadata(directory.join(&key)).is_ok_and(|metadata| metadata.is_dir())
                && !retired_mount_point(&directory.join(&key))
            {
                present = true;
                if directory == &storage.artifacts.join("views") {
                    report.due_views_remaining += 1;
                }
                caches.push((key.clone(), directory.clone()));
            }
        }
        report.due_keys += usize::from(present);
    }
    let cursor = states()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(storage.artifacts.as_path())
        .and_then(|state| state.due_cursor.clone());
    // Large per-checkout views dominate disk usage. Process them before other
    // planes, retaining a rotating key order within each priority class.
    caches.sort_by_key(|(key, directory)| {
        (
            directory != &storage.artifacts.join("views"),
            cursor.as_ref().is_some_and(|last| key <= last),
            key.clone(),
        )
    });
    let mut last = None;
    batch.due_lane = true;
    for (key, directory) in caches.into_iter().take(DUE_FIRST_ENTRIES) {
        if cancelled() {
            report.cancelled = true;
            break;
        }
        if clock() >= deadline {
            report.stopped_early = true;
            break;
        }
        let before = DomainReport::from_report(report);
        report.examined += 1;
        if let Err(error) = candidate(
            storage,
            roots,
            &directory,
            &key,
            false,
            history,
            report.at_ms,
            deadline,
            cancelled,
            members,
            batch,
            report,
        ) {
            report_candidate_error(&directory.join(&key), &error, report);
        }
        let delta = DomainReport::from_report(report).difference(&before);
        report.due_first.add(&delta);
        report
            .domains
            .entry(
                relative_domain(storage, &directory)?
                    .to_string_lossy()
                    .into_owned(),
            )
            .or_default()
            .add(&delta);
        last = Some(key);
        if batch.full() {
            batch.flush(
                storage,
                roots,
                report.at_ms,
                deadline,
                clock,
                cancelled,
                report,
            )?;
        }
    }
    let flushed = batch.flush(
        storage,
        roots,
        report.at_ms,
        deadline,
        clock,
        cancelled,
        report,
    );
    batch.due_lane = false;
    flushed?;
    if let Some(last) = last {
        states()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .entry(storage.artifacts.clone())
            .or_default()
            .due_cursor = Some(last);
    }
    Ok(())
}

fn run_pass_with_clock(
    storage: &StoragePaths,
    cancelled: &dyn Fn() -> bool,
    clock: &dyn Fn() -> Instant,
) -> SweepReport {
    run_pass_with_budget(storage, cancelled, clock, PASS_BUDGET)
}

fn persist_cursor(storage: &StoragePaths, path: &Path, cursor: &TraversalCursor) -> io::Result<()> {
    #[cfg(test)]
    test_hook(&storage.artifacts, "cursor-commit");
    #[cfg(not(test))]
    let _ = storage;
    atomic_json(path, cursor)
}

/// Unbound views have their own cursor and reserved budget. A long due queue
/// or a lexical batch full of bound views must not postpone orphan observation.
fn orphan_first(
    storage: &StoragePaths,
    roots: &mut RootIdentities,
    history: &BTreeMap<String, Vec<Binding>>,
    deadline: Instant,
    clock: &dyn Fn() -> Instant,
    cancelled: &dyn Fn() -> bool,
    members: &mut ViewMembers,
    due: &mut DueBatch,
    report: &mut SweepReport,
) -> io::Result<HashSet<PathBuf>> {
    let batch = traversal_batch_filtered(
        storage,
        &[(storage.artifacts.join("views"), false)],
        "orphans",
        PASS_ENTRIES,
        &|entry| {
            let key = entry.file_name().to_string_lossy().into_owned();
            key_valid(&key) && !history.contains_key(&key)
        },
    )?;
    let mut visited = HashSet::new();
    let mut last = None;
    let mut completed = true;
    for item in batch.entries {
        if cancelled() {
            report.cancelled = true;
            completed = false;
            break;
        }
        if clock() >= deadline {
            report.stopped_early = true;
            completed = false;
            break;
        }
        let before = DomainReport::from_report(report);
        report.examined += 1;
        let key = item.entry.file_name().to_string_lossy().into_owned();
        if item.entry.file_type()?.is_dir() {
            if let Err(error) = candidate(
                storage,
                roots,
                &item.directory,
                &key,
                false,
                history,
                report.at_ms,
                deadline,
                cancelled,
                members,
                due,
                report,
            ) {
                report_candidate_error(&item.entry.path(), &error, report);
            }
        }
        let delta = DomainReport::from_report(report).difference(&before);
        report
            .domains
            .entry("views".into())
            .or_default()
            .add(&delta);
        visited.insert(item.entry.path());
        last = Some(item.after);
    }
    // Views this lane visits have no binding history, so none is due; flush
    // anyway so nothing prepared here can outlive the lane.
    due.flush(
        storage,
        roots,
        report.at_ms,
        deadline,
        clock,
        cancelled,
        report,
    )?;
    if completed {
        last = Some(batch.end);
    }
    if let Some(cursor) = last {
        persist_cursor(storage, &batch.cursor_path, &cursor)?;
    }
    Ok(visited)
}

/// One pass with an explicit time budget. Production always passes
/// PASS_BUDGET; tests that check the entry bound pass a budget no runner can
/// exhaust, so a slow machine cannot stop the pass by wall clock first.
#[cfg(test)]
pub(crate) fn run_pass_with_budget_for_test(
    storage: &StoragePaths,
    budget: Duration,
) -> SweepReport {
    run_pass_with_budget(storage, &|| false, &Instant::now, budget)
}

fn run_pass_with_budget(
    storage: &StoragePaths,
    cancelled: &dyn Fn() -> bool,
    clock: &dyn Fn() -> Instant,
    budget: Duration,
) -> SweepReport {
    run_pass_with_root_identities(
        storage,
        cancelled,
        clock,
        budget,
        &mut RootIdentities::default(),
    )
}

fn run_pass_with_root_identities(
    storage: &StoragePaths,
    cancelled: &dyn Fn() -> bool,
    clock: &dyn Fn() -> Instant,
    budget: Duration,
    roots: &mut RootIdentities,
) -> SweepReport {
    let started = Instant::now();
    let mut report = SweepReport {
        at_ms: crate::pins::now_ms(),
        ..Default::default()
    };
    let result = (|| -> io::Result<()> {
        if bind_pending(&storage.durable) {
            report.stopped_early = true;
            return Ok(());
        }
        crate::root_cache::check_cache_access(&storage.artifacts)?;
        if !crate::root_cache::storage_allows_root_keyed(&storage.durable)? {
            return Err(io::Error::other("network storage retained"));
        }
        // Residency probes distinguish an absent readers directory from an
        // unavailable cache root. Establish the durable metadata root first.
        crate::private_storage::create_dir_all(storage.durable.join("retention"))?;
        let phase = Instant::now();
        sweep_view_generations(storage, cancelled, &mut report)?;
        report
            .timings_ms
            .insert("generations".into(), phase.elapsed().as_millis() as u64);
        if report.cancelled || bind_pending(&storage.durable) {
            return Ok(());
        }
        prune_bindings(storage, cancelled, &mut report)?;
        let history = bindings(storage, roots)?;
        let domains = domain_paths(storage)?;
        for (directory, _) in &domains {
            report
                .domains
                .entry(
                    relative_domain(storage, &directory)?
                        .to_string_lossy()
                        .into_owned(),
                )
                .or_default();
        }
        let deadline = Instant::now() + budget;
        report
            .timings_ms
            .insert("setup".into(), started.elapsed().as_millis() as u64);
        let phase = Instant::now();
        let mut members = ViewMembers::default();
        let mut due = DueBatch::default();
        // Keys already past their grace go first, with at most half the
        // budget, so their reclaim does not wait for the lexical walk to come
        // around. That walk visits every cache in every domain and takes days
        // at the rate new checkouts arrive.
        due_first(
            storage,
            roots,
            &domains,
            &history,
            deadline - budget / 2,
            clock,
            cancelled,
            &mut members,
            &mut due,
            &mut report,
        )?;
        report
            .timings_ms
            .insert("due_first".into(), phase.elapsed().as_millis() as u64);
        if report.cancelled {
            return Ok(());
        }
        let phase = Instant::now();
        let orphan_visited = orphan_first(
            storage,
            roots,
            &history,
            deadline - budget / 4,
            clock,
            cancelled,
            &mut members,
            &mut due,
            &mut report,
        )?;
        report
            .timings_ms
            .insert("orphan_views".into(), phase.elapsed().as_millis() as u64);
        let phase = Instant::now();
        let batch = next_batch(storage, &domains)?;
        report.stopped_early |= batch.entries.len() == PASS_ENTRIES;
        let mut completed = true;
        let mut last_processed = None;
        for item in batch.entries {
            let TraversalEntry {
                directory,
                flat,
                entry,
                after,
            } = item;
            if cancelled() {
                report.cancelled = true;
                completed = false;
                break;
            }
            if clock() >= deadline {
                report.stopped_early = true;
                completed = false;
                break;
            }
            if orphan_visited.contains(&entry.path()) {
                last_processed = Some(after);
                continue;
            }
            let before = DomainReport::from_report(&report);
            report.examined += 1;
            let name = entry.file_name().to_string_lossy().into_owned();
            let key = if flat {
                name.split('.').next().unwrap_or_default()
            } else {
                &name
            };
            if key_valid(key)
                && (flat || entry.file_type()?.is_dir())
                && (!flat || name.ends_with(".current") || name == format!("{key}.sqlite"))
            {
                if let Err(error) = candidate(
                    storage,
                    roots,
                    &directory,
                    key,
                    flat,
                    &history,
                    report.at_ms,
                    deadline,
                    cancelled,
                    &mut members,
                    &mut due,
                    &mut report,
                ) {
                    report_candidate_error(&entry.path(), &error, &mut report);
                }
            }
            let delta = DomainReport::from_report(&report).difference(&before);
            report
                .domains
                .entry(after.domain.to_string_lossy().into_owned())
                .or_default()
                .add(&delta);
            last_processed = Some(after);
            if due.full() {
                due.flush(
                    storage,
                    roots,
                    report.at_ms,
                    deadline,
                    clock,
                    cancelled,
                    &mut report,
                )?;
            }
        }
        due.flush(
            storage,
            roots,
            report.at_ms,
            deadline,
            clock,
            cancelled,
            &mut report,
        )?;
        if completed {
            last_processed = Some(batch.end);
        }
        // One durable commit per batch is enough: a crash can repeat checked
        // entries, but cannot skip the unexamined tail of a prefetched batch.
        if let Some(cursor) = last_processed {
            persist_cursor(storage, &batch.cursor_path, &cursor)?;
        }
        report
            .timings_ms
            .insert("roots".into(), phase.elapsed().as_millis() as u64);
        Ok(())
    })();
    if let Err(error) = result {
        if error.kind() != io::ErrorKind::Interrupted {
            report.errors.push(error.to_string());
        }
    }
    // Trash is already detached from live caches. Its independent deadline and
    // lock must not depend on how much of the root sweep's budget remains.
    if !report.cancelled && !cancelled() {
        schedule_reaper(storage);
    }
    // Blob stores have their own mark-before-sweep barrier. Release the root
    // barrier before entering it; otherwise pin admission would deadlock.
    if !report.cancelled && !cancelled() && !bind_pending(&storage.durable) {
        let phase = Instant::now();
        if let Err(error) = collect_one_family(storage, cancelled, &mut report) {
            report.errors.push(error.to_string());
        }
        report
            .timings_ms
            .insert("blobs_v1".into(), phase.elapsed().as_millis() as u64);
        let phase = Instant::now();
        if let Err(error) = collect_one_v2_family(storage, cancelled, &mut report) {
            report.errors.push(error.to_string());
        }
        report
            .timings_ms
            .insert("blobs_v2".into(), phase.elapsed().as_millis() as u64);
        let phase = Instant::now();
        if let Err(error) = collect_one_v3_family(storage, cancelled, &mut report, V3Bounds::PASS) {
            report.errors.push(error.to_string());
        }
        report
            .timings_ms
            .insert("blobs_v3".into(), phase.elapsed().as_millis() as u64);
    }
    report
        .timings_ms
        .insert("total".into(), started.elapsed().as_millis() as u64);
    report.cancelled |= cancelled();
    let mut states = states()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let state = states.entry(storage.artifacts.clone()).or_default();
    report.trash = state.trash.clone();
    if report.cancelled {
        state.last_run = None;
    }
    state.report = Some(report.clone());
    report
}

fn collect_one_v2_family(
    storage: &StoragePaths,
    cancelled: &dyn Fn() -> bool,
    report: &mut SweepReport,
) -> io::Result<()> {
    visit_one_family(storage, "blobs/v2", "families-v2", cancelled, |entry| {
        let family = entry.file_name().to_string_lossy().into_owned();
        if !key_valid(&family) || !entry.file_type()?.is_dir() || cancelled() {
            return Ok(());
        }
        let registry =
            crate::views::registry::FamilyRegistry::open_existing(&storage.artifacts, &family)
                .map_err(|error| io::Error::other(error.to_string()))?;
        let Some(registry) = registry else {
            return Ok(());
        };
        registry
            .bind_durable_storage(&storage.durable)
            .map_err(io::Error::other)?;
        let deadline = Instant::now() + PASS_BUDGET;
        // Generation cleanup uses the independent view cursor above; starting
        // at the first registry member on every family visit starves its tail.
        if cancelled() {
            return Ok(());
        }
        let swept = crate::gc::family::sweep_family_scheduled(
            &registry,
            crate::gc::family::SweepBounds {
                max_rows_per_store: 4096,
                max_entries_per_dir: 4096,
                deadline: Some(deadline),
            },
            false,
        )
        .map_err(|error| io::Error::other(error.to_string()))?;
        report.removed_roots += swept.deregistered.len();
        report.examined += swept.examined_roots;
        report.skipped_live += swept.skipped_live;
        report.skipped_recent += swept.skipped_recent;
        report.skipped_mount_mismatch += swept.skipped_mount_mismatch;
        report.skipped_protected += swept.skipped_protected;
        report.removed_bytes += swept.removed_root_bytes;
        report.unmeasured_removed_roots += swept.unmeasured_removed_roots;
        report
            .domains
            .entry("views/v2".to_owned())
            .or_default()
            .add(&DomainReport {
                examined: swept.examined_roots,
                skipped_live: swept.skipped_live,
                skipped_recent: swept.skipped_recent,
                skipped_mount_mismatch: swept.skipped_mount_mismatch,
                skipped_protected: swept.skipped_protected,
                skipped_unknown: 0,
                removed_roots: swept.deregistered.len(),
                removed_bytes: swept.removed_root_bytes,
                unmeasured_removed_roots: swept.unmeasured_removed_roots,
            });
        report
            .domains
            .entry("blobs/v2".to_owned())
            .or_default()
            .add(&DomainReport {
                examined: swept.examined_blobs,
                removed_bytes: swept.deleted_bytes,
                ..Default::default()
            });
        report.removed_blobs += swept.deleted_blobs;
        report.examined_blobs += swept.examined_blobs;
        report.removed_blob_payload_bytes += swept.deleted_bytes;
        report.blob_reclaim_deferred |= swept.reclaim_deferred;
        report.stopped_early |= swept.stopped_early;
        Ok(())
    })
}

/// One v3 member row as the retention collector reads it.
struct V3Member {
    scope: String,
    kind: String,
    root: Option<Vec<u8>>,
    view_dir: String,
    last_bind_ms: u64,
    state: String,
}

/// Outcome of one barrier step for a missing-root v3 member, with the same
/// meaning as the v2 family sweep's deregistration steps.
enum V3Step {
    /// The member row is gone.
    Gone,
    /// The root exists again (or is not eligible); the count was reset.
    Present,
    /// Protected or not yet counted to the limit; the member stays.
    Retained,
    /// The count reached the limit with this recorded value.
    Due { recorded: i64 },
    /// The recheck passed and `evicting` is committed.
    Evicting,
}

/// Protection of a v3 member: a live session row, a live pin or a protected
/// read marker. Rows, pins and markers whose owner is dead are removed on the
/// way, so they never keep a member. `Err` is uncertainty (an unreadable pin,
/// or a `pins/` or `readers/` listing longer than the bound) and keeps the
/// member without resetting its count, as the v2 family sweep does.
pub(crate) fn v3_member_protected(
    tx: &rusqlite::Transaction<'_>,
    scope: &str,
    view_dir: &Path,
) -> Result<bool, String> {
    let sessions: Vec<(String, crate::pins::PinOwner)> = tx
        .prepare("SELECT session_id,owner_pid,owner_start FROM view_sessions WHERE scope=?1")
        .and_then(|mut statement| {
            statement
                .query_map([scope], |row| {
                    Ok((
                        row.get(0)?,
                        crate::pins::PinOwner {
                            pid: row.get(1)?,
                            start_time: row.get(2)?,
                        },
                    ))
                })?
                .collect()
        })
        .map_err(|error| error.to_string())?;
    let mut live = false;
    for (session, owner) in sessions {
        if crate::pins::owner_is_live(&owner) {
            live = true;
        } else {
            tx.execute("DELETE FROM view_sessions WHERE session_id=?1", [&session])
                .map_err(|error| error.to_string())?;
        }
    }
    if live {
        return Ok(true);
    }
    for entry in bounded_dirs(&view_dir.join("pins"), TREE_ENTRIES).map_err(|e| e.to_string())? {
        let path = entry.path();
        if path.extension().and_then(|extension| extension.to_str()) != Some("json") {
            continue;
        }
        let metadata = crate::pins::read_metadata_strict(&path)
            .map_err(|error| format!("malformed pin {}: {error}", path.display()))?;
        let (metadata_path, keys_path) = crate::pins::pin_paths(view_dir, &metadata.generation);
        if metadata_path != path {
            return Err(format!("pin {} names another generation", path.display()));
        }
        if crate::pins::owner_is_live(&metadata.owner) {
            return Ok(true);
        }
        let _ = fs::remove_file(&metadata_path);
        let _ = fs::remove_file(&keys_path);
    }
    for entry in bounded_dirs(&view_dir.join("readers"), TREE_ENTRIES).map_err(|e| e.to_string())? {
        if !entry.file_type().map_err(|e| e.to_string())?.is_dir() {
            continue;
        }
        let generation = entry
            .file_name()
            .to_str()
            .map(str::to_owned)
            .ok_or_else(|| "non-UTF-8 reader directory".to_string())?;
        if crate::root_cache::sweep_read_markers(view_dir, &generation).protected {
            return Ok(true);
        }
    }
    Ok(false)
}

fn v3_root_path(root: &[u8]) -> Option<PathBuf> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        Some(PathBuf::from(std::ffi::OsStr::from_bytes(root)))
    }
    #[cfg(not(unix))]
    {
        std::str::from_utf8(root).ok().map(PathBuf::from)
    }
}

/// True when the root is certainly gone. An unreadable parent is not "gone".
fn v3_root_missing(root: &Path) -> bool {
    matches!(fs::symlink_metadata(root), Err(error) if error.kind() == io::ErrorKind::NotFound)
}

fn v3_member(tx: &rusqlite::Transaction<'_>, scope: &str) -> rusqlite::Result<Option<V3Member>> {
    use rusqlite::OptionalExtension;
    tx.query_row(
        "SELECT scope,kind,root,view_dir,last_bind_ms,state FROM members WHERE scope=?1",
        [scope],
        v3_member_row,
    )
    .optional()
}

fn v3_member_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<V3Member> {
    Ok(V3Member {
        scope: row.get(0)?,
        kind: row.get(1)?,
        root: row.get(2)?,
        view_dir: row.get(3)?,
        last_bind_ms: row.get(4)?,
        state: row.get(5)?,
    })
}

fn v3_missing_sweeps(tx: &rusqlite::Transaction<'_>, scope: &str) -> rusqlite::Result<Option<i64>> {
    use rusqlite::OptionalExtension;
    tx.query_row(
        "SELECT missing_sweeps FROM members WHERE scope=?1",
        [scope],
        |row| row.get(0),
    )
    .optional()
}

fn v3_reset_missing_sweeps(tx: &rusqlite::Transaction<'_>, scope: &str) -> rusqlite::Result<()> {
    tx.execute(
        "UPDATE members SET missing_sweeps=0 WHERE scope=?1",
        [scope],
    )
    .map(|_| ())
}

/// The first barrier step: resets or advances the member's count of
/// consecutive missing-root passes, or reports that removal is due.
fn v3_count_missing_pass(
    tx: &rusqlite::Transaction<'_>,
    scope: &str,
    view_dir: &Path,
    missing: bool,
) -> rusqlite::Result<V3Step> {
    let Some(recorded) = v3_missing_sweeps(tx, scope)? else {
        return Ok(V3Step::Gone);
    };
    if !missing {
        if recorded != 0 {
            v3_reset_missing_sweeps(tx, scope)?;
        }
        return Ok(V3Step::Present);
    }
    match v3_member_protected(tx, scope, view_dir) {
        Ok(false) => {}
        Ok(true) => {
            v3_reset_missing_sweeps(tx, scope)?;
            return Ok(V3Step::Retained);
        }
        Err(_) => return Ok(V3Step::Retained),
    }
    let count = recorded.max(0) as u32 + 1;
    if count < crate::views::registry::MISSING_ROOT_SWEEPS {
        tx.execute(
            "UPDATE members SET missing_sweeps=?2 WHERE scope=?1",
            rusqlite::params![scope, i64::from(count)],
        )?;
        return Ok(V3Step::Retained);
    }
    Ok(V3Step::Due { recorded })
}

/// The second barrier step, taken under the storage-root reservation lock
/// just before deletion: re-reads the row and rechecks its kind, root, count
/// and every protection, then commits `evicting`. A pin or session that
/// appeared since the first step keeps the member.
fn v3_recheck_and_mark_evicting(
    tx: &rusqlite::Transaction<'_>,
    // The durable root, where retention keeps root bindings.
    storage: &Path,
    scope: &str,
    view_dir: &Path,
    recorded: i64,
) -> rusqlite::Result<V3Step> {
    let Some(current) = v3_member(tx, scope)? else {
        return Ok(V3Step::Gone);
    };
    if v3_missing_sweeps(tx, scope)? != Some(recorded) {
        return Ok(V3Step::Retained);
    }
    let due = current.kind == "view"
        && current
            .root
            .as_deref()
            .and_then(v3_root_path)
            .is_some_and(|root| {
                v3_root_missing(&root) && missing_root_due(storage, &root, current.last_bind_ms)
            });
    if !due {
        v3_reset_missing_sweeps(tx, scope)?;
        return Ok(V3Step::Present);
    }
    match v3_member_protected(tx, scope, view_dir) {
        Ok(false) => {}
        Ok(true) => {
            v3_reset_missing_sweeps(tx, scope)?;
            return Ok(V3Step::Retained);
        }
        Err(_) => return Ok(V3Step::Retained),
    }
    tx.execute(
        "UPDATE members SET state=?2 WHERE scope=?1",
        rusqlite::params![scope, crate::views::registry::STATE_EVICTING],
    )?;
    Ok(V3Step::Evicting)
}

/// Removes the view directory of a member whose `evicting` state is
/// committed, deletes its row, then takes a covering measurement so committed
/// usage falls by the bytes actually freed. A crash anywhere in here leaves
/// the `evicting` row, and the next pass calls this again to finish.
///
/// Once `evicting` is committed the eviction is always finished, even if the
/// member's root has come back since. That is safe only because of the
/// contract every v3 binder must follow: a binder that finds its member in
/// `evicting` never uses that view directory. It treats the member as absent
/// and publishes a fresh one, or waits for the eviction to finish.
fn v3_finish_eviction(
    storage: &Path,
    registry: &mut crate::blob_store::v3::RegistryWriter<'_>,
    scope: &str,
    view_dir: &Path,
    deadline: Instant,
) -> io::Result<crate::blob_store::v3::Measurement> {
    #[cfg(test)]
    test_hook(storage, "v3-evicting-committed");
    #[cfg(not(test))]
    let _ = storage;
    crate::views::registry::remove_view_dir(view_dir)?;
    registry
        .with_barrier(|tx| {
            let removed = tx.execute(
                "DELETE FROM members WHERE scope=?1 AND state=?2",
                rusqlite::params![scope, crate::views::registry::STATE_EVICTING],
            )?;
            // A deregistered worker's referrer and strict-rebuild rows go in
            // the same transaction as its member row, after its directory.
            if removed > 0 && crate::views::base_refs::has_base_refs(tx)? {
                tx.execute("DELETE FROM base_refs WHERE worker_scope=?1", [scope])?;
                tx.execute("DELETE FROM strict_rebuild WHERE worker_scope=?1", [scope])?;
            }
            Ok(())
        })
        .map_err(|error| io::Error::other(error.to_string()))?;
    // Under the pass deadline: the reservation lock held here is the one
    // v3 publishers need, so a measurement that runs out of time is left
    // owed rather than finished late.
    registry
        .covering_measurement_until(deadline)
        .map_err(|error| io::Error::other(error.to_string()))
}

/// The v3 retention collector. Each pass visits one family under
/// `blobs/v3/` and applies the v2 missing-root rule to its `kind = view`
/// members: a member is removed with its view directory once its root has
/// been missing, unprotected and past the recency and mount guards for
/// `MISSING_ROOT_SWEEPS` consecutive passes. `kind = base` members and
/// members with no root are never removed. Plane rows stay for the v3 plane
/// sweep.
///
/// It only opens registries that already exist. A storage root with no
/// `blobs/v3/` lists nothing, and a family directory without `members.sqlite`
/// is skipped before the storage-root reservation lock is taken, because
/// taking that lock creates `blobs/v3/reservations.lock`. Views-off and
/// views-on users who never published v3 data therefore get no v3 file.
/// Per-visit bounds of the v3 collector. Production uses [`V3Bounds::PASS`];
/// tests shrink them so a bound can be reached with a handful of members.
#[derive(Clone, Copy, Debug)]
struct V3Bounds {
    /// Time for the lock wait, the member work and the measurements together.
    budget: Duration,
    /// Members examined in one visit, as `collect_one_v2_family` caps them.
    members: usize,
}

impl V3Bounds {
    const PASS: Self = Self {
        budget: PASS_BUDGET,
        members: HISTORY_ENTRIES,
    };
}

/// Where the next visit of one v3 family resumes: the scope of the last
/// member a visit examined. Members are taken in scope order after it,
/// wrapping to the start, so a full window of live or protected members
/// cannot keep later members from ever being examined.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
struct V3MemberCursor {
    last_scope: Option<String>,
}

fn v3_member_cursor_path(storage: &StoragePaths, family: &str) -> io::Result<PathBuf> {
    Ok(cursor_root(storage)?
        .join("v3-members")
        .join(format!("{family}.json")))
}

/// v3 family stores live where v2 ones do: under the artifact root (the
/// `storage.cache_dir` volume when one is set; `blobs` is a cache domain).
/// Retention bindings, which decide whether a missing root is due, and the
/// collector's cursors are durable state under the durable root.
fn collect_one_v3_family(
    storage: &StoragePaths,
    cancelled: &dyn Fn() -> bool,
    report: &mut SweepReport,
    bounds: V3Bounds,
) -> io::Result<()> {
    // Only family directories take the pass's one visit, so the lock file
    // beside them never uses up a pass.
    let next = || {
        traversal_batch_filtered(
            storage,
            &[(storage.artifacts.join("blobs/v3"), false)],
            "families-v3",
            1,
            &|entry| {
                key_valid(&entry.file_name().to_string_lossy())
                    && entry.file_type().is_ok_and(|kind| kind.is_dir())
            },
        )
    };
    let mut batch = next()?;
    if batch.entries.is_empty() && batch.cursor_path.try_exists()? {
        // The end of a lap: save the reset cursor and start the next lap in
        // this same pass, so a lone family is visited on every pass rather
        // than every other one. With no saved cursor there is nothing to
        // reset, so a storage root that never had v3 data gets no cursor file.
        atomic_json(&batch.cursor_path, &batch.end)?;
        batch = next()?;
    }
    if cancelled() {
        return Ok(());
    }
    let Some(item) = batch.entries.into_iter().next() else {
        return Ok(());
    };
    let family = item.entry.file_name().to_string_lossy().into_owned();
    let result = collect_v3_family(storage, &family, cancelled, report, bounds);
    // A family skipped because its reservation lock was busy keeps the lap
    // position, so the next pass tries it first.
    if !cancelled() && !matches!(result, Ok(V3Visit::LockBusy)) {
        // Advance even when family work fails, so later families can still run.
        atomic_json(&batch.cursor_path, &item.after)?;
    }
    result.map(|_| ())
}

/// How one family visit of the v3 collector ended.
enum V3Visit {
    /// The family was examined, or had no registry to examine.
    Done,
    /// Another process held the storage-root reservation lock past the
    /// pass's budget, so the family was not examined.
    LockBusy,
}

fn reservation_lock_busy(error: &crate::blob_store::v3::Error) -> bool {
    matches!(
        error,
        crate::blob_store::v3::Error::Io(error)
            if error
                .get_ref()
                .and_then(|source| {
                    source.downcast_ref::<crate::blob_store::v3::ReservationLockError>()
                })
                .is_some_and(crate::blob_store::v3::ReservationLockError::is_timeout)
    )
}

fn collect_v3_family(
    paths: &StoragePaths,
    family: &str,
    cancelled: &dyn Fn() -> bool,
    report: &mut SweepReport,
    bounds: V3Bounds,
) -> io::Result<V3Visit> {
    use crate::blob_store::v3::{Measurement, RegistryWriter, ReservationLock};
    let v3 = |error: crate::blob_store::v3::Error| io::Error::other(error.to_string());
    // Family data: the artifact root. Root bindings: the durable root.
    let storage = paths.artifacts.as_path();
    let durable = paths.durable.as_path();
    let registry_path = storage.join("blobs/v3").join(family).join("members.sqlite");
    if !registry_path.try_exists()? {
        return Ok(V3Visit::Done);
    }
    // View-directory removal refuses a cache whose durable owner is unknown;
    // declare it, as the v2 collector does when it binds its registry.
    crate::root_cache::register_durable_owner(paths)?;
    let deadline = Instant::now() + bounds.budget;
    // Lock first, then the registry barrier, the order every v3 writer uses.
    // The wait for the lock comes out of the same budget as the member work:
    // a publisher holding it must not stall the retention lane, which also
    // gates binds. The lock is taken only once a registry is known to exist,
    // because taking it creates `blobs/v3/` and the lock file.
    let lock = match ReservationLock::acquire_until(storage, deadline) {
        Ok(lock) => lock,
        Err(error) if reservation_lock_busy(&error) => {
            report
                .skipped_families
                .push(format!("blobs/v3/{family}: reservation_busy"));
            return Ok(V3Visit::LockBusy);
        }
        Err(error) => return Err(v3(error)),
    };
    // Opening the registry takes a covering measurement and releases the
    // reservations of dead owners. Like every measurement here it stops at
    // the pass deadline; one that runs out records nothing and is owed (see
    // `Measurement::Owed`), and the visit ends so the lock is released.
    let (mut registry, opened) =
        RegistryWriter::open_existing_until(storage, family, &lock, deadline).map_err(v3)?;
    if opened == Measurement::Owed {
        report
            .skipped_families
            .push(format!("blobs/v3/{family}: measurement_owed"));
        return Ok(V3Visit::Done);
    }
    let mut owed = false;
    let cursor_path = v3_member_cursor_path(paths, family)?;
    // An unreadable cursor only costs the resume position.
    let resume = read_json::<V3MemberCursor>(&cursor_path)
        .ok()
        .and_then(|cursor| cursor.last_scope);
    let limit = bounds.members as i64;
    let members: Vec<V3Member> = registry
        .with_barrier(|tx| {
            // The window starts after the last member examined. When it
            // reaches the end of the scope order first, it continues from the
            // start, up to and including that member.
            let mut rows = tx
                .prepare(
                    "SELECT scope,kind,root,view_dir,last_bind_ms,state FROM members WHERE scope > ?1 ORDER BY scope LIMIT ?2",
                )?
                .query_map(
                    rusqlite::params![resume.as_deref().unwrap_or_default(), limit],
                    v3_member_row,
                )?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            if let Some(resume) = resume.as_deref() {
                let room = limit - rows.len() as i64;
                if room > 0 {
                    rows.extend(
                        tx.prepare(
                            "SELECT scope,kind,root,view_dir,last_bind_ms,state FROM members WHERE scope <= ?1 ORDER BY scope LIMIT ?2",
                        )?
                        .query_map(rusqlite::params![resume, room], v3_member_row)?
                        .collect::<rusqlite::Result<Vec<_>>>()?,
                    );
                }
            }
            Ok(rows)
        })
        .map_err(v3)?;
    let mut domain = DomainReport::default();
    let mut last_examined = None;
    for member in members {
        if cancelled() || Instant::now() >= deadline {
            report.stopped_early = true;
            break;
        }
        // Noted before the member's work. The resume position is saved only
        // when the visit's loop ends, so a visit that stops on an error
        // resumes from its previous position.
        last_examined = Some(member.scope.clone());
        if member.kind != "view" {
            continue;
        }
        domain.examined += 1;
        // Delete only the directory this family and scope own. A row whose
        // stored path points anywhere else is kept and reported.
        let view_dir = crate::blob_store::v3::view_dir(storage, family, &member.scope);
        let Some(view_dir) = view_dir
            .ok()
            .filter(|expected| Path::new(&member.view_dir) == expected)
        else {
            domain.skipped_unknown += 1;
            if report.errors.len() < 16 {
                report.errors.push(format!(
                    "v3 member {} in family {family} names an unexpected view directory",
                    member.scope
                ));
            }
            continue;
        };
        if member.state == crate::views::registry::STATE_EVICTING {
            let bytes = retention_tree_bytes(&view_dir, deadline).ok();
            let measured =
                v3_finish_eviction(storage, &mut registry, &member.scope, &view_dir, deadline)?;
            record_v3_removal(&mut domain, bytes);
            owed |= measured == Measurement::Owed;
            continue;
        }
        let Some(root) = member.root.as_deref().and_then(v3_root_path) else {
            domain.skipped_live += 1;
            continue;
        };
        let eligibility = if v3_root_missing(&root) {
            missing_root_eligibility(durable, &root, member.last_bind_ms)
        } else {
            RootEligibility::Live
        };
        match eligibility {
            RootEligibility::Live => domain.skipped_live += 1,
            RootEligibility::Recent => domain.skipped_recent += 1,
            RootEligibility::MountMismatch => domain.skipped_mount_mismatch += 1,
            RootEligibility::Due => {}
        }
        let missing = eligibility == RootEligibility::Due;
        let step = registry
            .with_barrier(|tx| {
                Ok(v3_count_missing_pass(
                    tx,
                    &member.scope,
                    &view_dir,
                    missing,
                )?)
            })
            .map_err(v3)?;
        let step = match step {
            V3Step::Due { recorded } => {
                #[cfg(test)]
                test_hook(storage, "v3-before-recheck");
                registry
                    .with_barrier(|tx| {
                        Ok(v3_recheck_and_mark_evicting(
                            tx,
                            durable,
                            &member.scope,
                            &view_dir,
                            recorded,
                        )?)
                    })
                    .map_err(v3)?
            }
            other => other,
        };
        match step {
            V3Step::Evicting => {
                let bytes = retention_tree_bytes(&view_dir, deadline).ok();
                let measured =
                    v3_finish_eviction(storage, &mut registry, &member.scope, &view_dir, deadline)?;
                record_v3_removal(&mut domain, bytes);
                owed |= measured == Measurement::Owed;
            }
            V3Step::Retained if missing => domain.skipped_protected += 1,
            V3Step::Retained | V3Step::Present | V3Step::Gone | V3Step::Due { .. } => {}
        }
    }
    if let Some(last_scope) = last_examined {
        atomic_json(
            &cursor_path,
            &V3MemberCursor {
                last_scope: Some(last_scope),
            },
        )?;
    }
    // The base-reference sweep: referrer reconciliation, the two-sweep base
    // reclaim and the plane-row sweep. It needs the registry's base-reference
    // tables and runs under the reservation lock this visit already holds.
    if !cancelled()
        && Instant::now() < deadline
        && registry.registry_version().map_err(v3)?
            >= crate::blob_store::v3::REGISTRY_BASE_REFS_VERSION
    {
        let plane_cursor = cursor_root(paths)?
            .join("v3-plane-rows")
            .join(format!("{family}.json"));
        let bounds = crate::views::base_sweep::SweepBounds {
            deadline: Some(deadline),
            ..Default::default()
        };
        match crate::views::base_sweep::sweep_family_locked(
            storage,
            family,
            &lock,
            None,
            bounds,
            Some(&plane_cursor),
        ) {
            Ok(swept) => {
                report.removed_roots += swept.reclaimed_bases.len();
                report.removed_blobs += swept.plane_rows_deleted as usize;
                report.examined_blobs += swept.plane_rows_examined as usize;
                report.stopped_early |= !swept.plane_rows_complete && swept.skipped.is_none();
            }
            Err(error) => {
                if report.errors.len() < 16 {
                    report
                        .errors
                        .push(format!("v3 base sweep of family {family}: {error}"));
                }
            }
        }
    }
    // Every collector pass ends with a covering measurement, so committed
    // usage also reflects files removed by anything else since the last one.
    // It is skipped once one is already owed: the deadline has passed.
    if !owed {
        owed = registry.covering_measurement_until(deadline).map_err(v3)? == Measurement::Owed;
    }
    if owed {
        report
            .skipped_families
            .push(format!("blobs/v3/{family}: measurement_owed"));
    }
    report.examined += domain.examined;
    report.skipped_live += domain.skipped_live;
    report.skipped_recent += domain.skipped_recent;
    report.skipped_mount_mismatch += domain.skipped_mount_mismatch;
    report.skipped_protected += domain.skipped_protected;
    report.skipped_unknown += domain.skipped_unknown;
    report.removed_roots += domain.removed_roots;
    report.removed_bytes += domain.removed_bytes;
    report.unmeasured_removed_roots += domain.unmeasured_removed_roots;
    report
        .domains
        .entry("views/v3".to_owned())
        .or_default()
        .add(&domain);
    Ok(V3Visit::Done)
}

fn record_v3_removal(domain: &mut DomainReport, bytes: Option<u64>) {
    domain.removed_roots += 1;
    match bytes {
        Some(bytes) => domain.removed_bytes += bytes,
        None => domain.unmeasured_removed_roots += 1,
    }
}

#[cfg(test)]
mod storage_retention_tests {
    #[test]
    fn retention_protection_failure_backs_off_and_stays_in_health() {
        let now = Instant::now();
        let mut state = State {
            report: Some(SweepReport::default()),
            ..Default::default()
        };
        for attempt in 1..=5 {
            state.record_trash(
                TrashReport {
                    incomplete: true,
                    protection_failure: Some(ProtectionFailure::Budget),
                    ..Default::default()
                },
                now,
            );
            assert!(!state.reaper_ready(now + INTERVAL));
            let retry = state.protection_retry.unwrap();
            assert!(retry <= now + Duration::from_secs(60 * 60));
            assert!(state.reaper_ready(retry));
            assert_eq!(state.protection_failures, attempt);
            let health = serde_json::to_value(state.report.as_ref().unwrap()).unwrap();
            assert_eq!(health["trash"]["incomplete"], true);
            assert_eq!(health["trash"]["protection_failure"], "budget");
            assert!(health["trash"]["protection_retry_at_ms"].as_u64().is_some());
        }
        state.record_trash(TrashReport::default(), now);
        assert!(state.reaper_ready(now));
        assert_eq!(state.protection_failures, 0);
        assert!(state.trash.protection_retry_at_ms.is_none());
    }

    #[test]
    fn retention_protection_large_task_store_fixture() {
        let stress = std::env::var("AFT_STRESS").as_deref() == Ok("1");
        let tasks = if stress { 30_000 } else { 300 };
        let sessions = if stress { 1_372 } else { 14 };
        let files = tasks * 11 + tasks * 2 / 3;
        let temp = tempfile::tempdir().unwrap();
        let storage = StoragePaths::same(temp.path());
        fs::create_dir_all(temp.path().join("retention/bindings")).unwrap();
        for root in 0..200 {
            fs::write(
                temp.path()
                    .join(format!("retention/bindings/{root:016x}.json")),
                b"{}",
            )
            .unwrap();
        }
        for task in 0..tasks {
            let directory = temp
                .path()
                .join(format!("tasks/session-{:04}/{task:032x}", task % sessions));
            fs::create_dir_all(directory.join("control")).unwrap();
            fs::create_dir_all(directory.join("io")).unwrap();
            for name in crate::bash_background::persistence::readable_task_file_names(
                crate::bash_background::persistence::TaskLayout::Directory,
            ) {
                fs::write(directory.join(name), b"finished task artifact").unwrap();
            }
            if task < tasks * 2 / 3 {
                fs::write(directory.join("control/environment.bin"), b"environment").unwrap();
            }
        }
        for entry in 0..50 {
            let domain = if entry % 2 == 0 { "index" } else { "inspect" };
            let source = temp.path().join(format!("{domain}/{entry:016x}"));
            fs::create_dir_all(&source).unwrap();
            fs::write(source.join("payload"), b"retired cache").unwrap();
            retire_directory(temp.path(), &source).unwrap();
        }
        // Even the default fixture exceeds a small census allowance. An
        // incomplete protection set must authorize no deletion, then a fresh
        // pass must be able to reclaim the same prefetched trash entries.
        let mut cutoff = TrashReport::default();
        assert!(reap_trash_with_snapshot_budget(
            &storage,
            &temp.path().join("index/.retention-trash"),
            false,
            Instant::now() + Duration::from_secs(60),
            &mut crate::bash_background::persistence::RepairBudget::with_entries(500),
            &mut cutoff,
            NO_TIME_BOUND,
        )
        .unwrap());
        assert_eq!(cutoff.protection_failure, Some(ProtectionFailure::Budget));
        assert!(cutoff.incomplete);
        assert_eq!(cutoff.removed_entries, 0);
        let mut state = State::default();
        let failed_at = Instant::now();
        state.record_trash(cutoff, failed_at);
        assert!(!state.reaper_ready(failed_at + INTERVAL));
        assert!(state.reaper_ready(state.protection_retry.unwrap()));
        for domain in ["index", "inspect"] {
            let entries = fs::read_dir(temp.path().join(domain).join(".retention-trash"))
                .unwrap()
                .collect::<Result<Vec<_>, _>>()
                .unwrap();
            assert_eq!(entries.len(), 25);
            for entry in entries {
                assert_eq!(
                    fs::read(entry.path().join("payload")).unwrap(),
                    b"retired cache"
                );
            }
        }
        let deadline = Instant::now() + Duration::from_secs(60);
        let mut budget = crate::bash_background::persistence::RepairBudget::new();
        let mut report = TrashReport::default();
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let observed = calls.clone();
        let _hook = observe_test_hook(
            temp.path(),
            Arc::new(move |step| {
                if step == "protection-census" {
                    observed.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                }
            }),
        );
        let started = Instant::now();
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        let cpu_started = thread_cpu_seconds();
        for domain in ["index", "inspect"] {
            reap_trash_with_snapshot_budget(
                &storage,
                &temp.path().join(domain).join(".retention-trash"),
                false,
                deadline,
                &mut budget,
                &mut report,
                NO_TIME_BOUND,
            )
            .unwrap();
        }
        eprintln!(
            "retention fixture: roots=200 finished_tasks={tasks} sessions={sessions} files={files} trash=50 elapsed_ms={:.3} report={}",
            started.elapsed().as_secs_f64() * 1_000.0,
            serde_json::to_string(&report).unwrap(),
        );
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        eprintln!(
            "retention fixture: thread_cpu_ms={:.3}",
            (thread_cpu_seconds() - cpu_started) * 1_000.0
        );
        assert_eq!(report.removed_entries, 50, "{report:?}");
        assert!(!report.incomplete, "{report:?}");
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert!(report.protection_census.directories >= tasks * 3);
        state.record_trash(report, Instant::now());
        assert!(state.reaper_ready(Instant::now()));
        assert!(state.protection_retry.is_none());
        assert!(state.trash.protection_retry_at_ms.is_none());
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    fn thread_cpu_seconds() -> f64 {
        let mut time = libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        assert_eq!(
            unsafe { libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, &mut time) },
            0
        );
        time.tv_sec as f64 + time.tv_nsec as f64 / 1_000_000_000.0
    }

    #[test]
    fn retention_protection_cancelled_defers_without_deleting() {
        use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
        let (temp, entries) = followup_trash_fixture(3);
        let cancelled = Arc::new(AtomicBool::new(false));
        let signal = cancelled.clone();
        let calls = Arc::new(AtomicUsize::new(0));
        let observed = calls.clone();
        let _hook = observe_test_hook(
            temp.path(),
            Arc::new(move |step| {
                if step == "protection-census" {
                    observed.fetch_add(1, Ordering::SeqCst);
                    signal.store(true, Ordering::SeqCst);
                }
            }),
        );
        let deadline = Instant::now() + Duration::from_secs(60);
        let mut budget = crate::bash_background::persistence::RepairBudget::until(
            deadline,
            Arc::new(move || cancelled.load(Ordering::SeqCst)),
        );
        let mut report = TrashReport::default();
        assert!(reap_trash_with_snapshot_budget(
            &StoragePaths::same(temp.path()),
            entries[0].parent().unwrap(),
            false,
            deadline,
            &mut budget,
            &mut report,
            NO_TIME_BOUND,
        )
        .unwrap());
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            report.protection_failure,
            Some(ProtectionFailure::Cancelled)
        );
        assert_eq!(report.kept_by_reason.get("protections_cancelled"), Some(&1));
        assert_eq!(report.removed_entries, 0);
        assert!(report.incomplete);
        for entry in &entries {
            assert!(entry.join("payload").exists());
        }
    }

    #[test]
    fn retention_protection_entry_budget_defers_without_deleting() {
        let (temp, entries) = followup_trash_fixture(3);
        // Three entries plus iterator exhaustion consume the snapshot budget;
        // no allowance remains for a complete protection census.
        let mut budget = crate::bash_background::persistence::RepairBudget::with_entries(4);
        let mut report = TrashReport::default();
        assert!(reap_trash_with_snapshot_budget(
            &StoragePaths::same(temp.path()),
            entries[0].parent().unwrap(),
            false,
            Instant::now() + Duration::from_secs(60),
            &mut budget,
            &mut report,
            NO_TIME_BOUND,
        )
        .unwrap());
        assert_eq!(report.protection_failure, Some(ProtectionFailure::Budget));
        assert_eq!(report.kept_by_reason.get("protections_budget"), Some(&1));
        assert_eq!(report.removed_entries, 0);
        assert!(report.incomplete);
        for entry in &entries {
            assert!(entry.join("payload").exists());
        }
        let storage = StoragePaths::same(temp.path());
        states()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .entry(storage.artifacts.clone())
            .or_default()
            .record_trash(report, Instant::now());
        let launches = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let observed = launches.clone();
        let _hook = observe_test_hook(
            temp.path(),
            Arc::new(move |step| {
                if step == "reaper-launch" {
                    observed.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                }
            }),
        );
        schedule_reaper(&storage);
        schedule_reaper(&storage);
        assert_eq!(launches.load(std::sync::atomic::Ordering::SeqCst), 0);
    }

    #[test]
    fn retention_protection_unavailable_is_cached_across_containers() {
        let (temp, mut entries) = followup_trash_fixture(2);
        let source = temp.path().join("inspect/0123456789abcdef");
        fs::create_dir_all(&source).unwrap();
        fs::write(source.join("payload"), b"retired inspect").unwrap();
        retire_directory(temp.path(), &source).unwrap();
        entries.extend(
            fs::read_dir(temp.path().join("inspect/.retention-trash"))
                .unwrap()
                .map(|entry| entry.unwrap().path()),
        );
        let mut deep = temp.path().join("durable");
        for _ in 0..66 {
            deep.push("d");
        }
        fs::create_dir_all(deep).unwrap();
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let observed = calls.clone();
        let _hook = observe_test_hook(
            temp.path(),
            Arc::new(move |step| {
                if step == "protection-census" {
                    observed.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                }
            }),
        );
        let mut report = TrashReport::default();
        let mut budget = crate::bash_background::persistence::RepairBudget::new();
        for domain in ["views", "inspect"] {
            assert!(reap_trash_with_snapshot_budget(
                &StoragePaths::same(temp.path()),
                &temp.path().join(domain).join(".retention-trash"),
                false,
                Instant::now() + Duration::from_secs(60),
                &mut budget,
                &mut report,
                NO_TIME_BOUND,
            )
            .unwrap());
        }
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert_eq!(
            report.protection_failure,
            Some(ProtectionFailure::Unavailable)
        );
        assert_eq!(
            report.kept_by_reason.get("protections_unavailable"),
            Some(&2)
        );
        assert!(report.protection_census.directories > 0);
        assert_eq!(report.removed_entries, 0);
        assert!(report.incomplete);
        for entry in entries {
            assert!(entry.join("payload").exists());
        }
    }

    #[test]
    fn retention_protection_keeps_a_nested_live_task_alias() {
        assert_nested_task_alias_protected(false);
    }

    #[test]
    fn retention_protection_keeps_a_nested_finished_task_alias() {
        assert_nested_task_alias_protected(true);
    }

    fn assert_nested_task_alias_protected(finished: bool) {
        use crate::bash_background::persistence::{PersistedTask, PinnedDir, REPAIR_IDENTITIES};
        use crate::bash_background::BgTaskStatus;
        let temp = tempfile::tempdir().unwrap();
        let task = temp.path().join("tasks/session").join("a".repeat(32));
        fs::create_dir_all(task.join("control")).unwrap();
        fs::create_dir_all(task.join("io")).unwrap();
        let mut metadata = PersistedTask::starting(
            "a".repeat(32),
            "session".into(),
            "echo output".into(),
            temp.path().into(),
            None,
            None,
            false,
            false,
        );
        metadata.status = if finished {
            BgTaskStatus::Completed
        } else {
            BgTaskStatus::Running
        };
        if finished {
            metadata.finished_at = Some(crate::pins::now_ms());
            metadata.exit_code = Some(0);
        }
        let metadata = serde_json::to_vec(&metadata).unwrap();
        fs::write(task.join("control/metadata.json"), &metadata).unwrap();
        fs::write(task.join("io/stdout"), b"task output").unwrap();
        let source = temp.path().join("views/0123456789abcdef");
        fs::create_dir_all(source.join("nested")).unwrap();
        fs::write(source.join("nested/output"), b"task output").unwrap();
        retire_directory(temp.path(), &source).unwrap();
        let trash = temp.path().join("views/.retention-trash");
        let entry = fs::read_dir(&trash)
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        // The identity seam is keyed by path, and the census may see the
        // alias in its plain or canonical form (macOS /private/var, Windows
        // verbatim prefixes), so the injected identity covers both.
        let aliases = [
            entry.join("nested"),
            fs::canonicalize(entry.join("nested")).unwrap(),
        ];
        let task_id = PinnedDir::open(&task.join("io"))
            .unwrap()
            .directory_identity()
            .unwrap();
        REPAIR_IDENTITIES.with(|ids| {
            for alias in &aliases {
                ids.borrow_mut().insert(alias.clone(), task_id);
            }
        });
        let mut report = TrashReport::default();
        let result = reap_trash_with_snapshot_budget(
            &StoragePaths::same(temp.path()),
            &trash,
            false,
            Instant::now() + Duration::from_secs(60),
            &mut crate::bash_background::persistence::RepairBudget::new(),
            &mut report,
            NO_TIME_BOUND,
        );
        REPAIR_IDENTITIES.with(|ids| {
            for alias in &aliases {
                ids.borrow_mut().remove(alias);
            }
        });
        assert!(result.unwrap());
        assert_eq!(report.removed_entries, 0, "{report:?}");
        assert_eq!(
            report.kept_by_reason.get("unlink_guard"),
            Some(&1),
            "{report:?}"
        );
        assert_eq!(
            fs::read(entry.join("nested/output")).unwrap(),
            b"task output"
        );
        assert_eq!(
            fs::read(task.join("control/metadata.json")).unwrap(),
            metadata
        );
        assert_eq!(fs::read(task.join("io/stdout")).unwrap(), b"task output");
    }

    fn followup_trash_fixture(count: usize) -> (tempfile::TempDir, Vec<PathBuf>) {
        let temp = tempfile::tempdir().unwrap();
        fs::create_dir_all(temp.path().join("retention")).unwrap();
        for key in 0..count {
            let view = temp.path().join(format!("views/{key:016x}"));
            fs::create_dir_all(&view).unwrap();
            fs::write(view.join("payload"), b"retired view bytes").unwrap();
            retire_directory(temp.path(), &view).unwrap();
        }
        let entries = fs::read_dir(temp.path().join("views/.retention-trash"))
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect();
        (temp, entries)
    }

    #[test]
    fn retention_trash_followup_retained_prefix_does_not_starve_verified_tail() {
        let (temp, entries) = followup_trash_fixture(9);
        // Use the filesystem's actual enumeration order, not a name-order
        // assumption: only the last entry in that order is reclaimable.
        let tail = entries.last().unwrap();
        for entry in &entries[..entries.len() - 1] {
            fs::remove_file(entry.join(RETIREMENT_WITNESS)).unwrap();
        }
        let trash = tail.parent().unwrap();
        let _hook = observe_test_hook(
            trash,
            Arc::new(|step| {
                if step == "trash-snapshot-entry" {
                    std::thread::sleep(Duration::from_millis(12));
                }
            }),
        );
        let mut examined = 0;
        for _ in 0..entries.len() * 2 {
            let deadline = Instant::now() + Duration::from_secs(60);
            let mut budget = crate::bash_background::persistence::RepairBudget::new();
            let mut report = TrashReport::default();
            assert!(reap_trash(
                &StoragePaths::same(temp.path()),
                trash,
                true,
                deadline,
                &mut budget,
                &mut report
            )
            .unwrap());
            let window = report.entries + report.removed_entries;
            assert!(
                window <= 1,
                "injected delay must bound the snapshot: {report:?}"
            );
            examined += window;
            if !tail.exists() {
                break;
            }
        }
        assert!(
            !tail.exists(),
            "verified tail was starved by retained prefixes"
        );
        assert!(
            examined >= entries.len(),
            "each pass must report its examined window"
        );
        for entry in &entries[..entries.len() - 1] {
            assert!(
                entry.join("payload").exists(),
                "unverified trash must remain"
            );
        }
    }

    #[test]
    fn retention_trash_followup_fully_measured_leftovers_are_not_incomplete() {
        let (temp, entries) = followup_trash_fixture(1);
        fs::remove_file(entries[0].join(RETIREMENT_WITNESS)).unwrap();
        let deadline = Instant::now() + Duration::from_secs(60);
        let mut budget = crate::bash_background::persistence::RepairBudget::new();
        let mut report = TrashReport::default();
        assert!(reap_trash_with_snapshot_budget(
            &StoragePaths::same(temp.path()),
            entries[0].parent().unwrap(),
            true,
            deadline,
            &mut budget,
            &mut report,
            NO_TIME_BOUND
        )
        .unwrap());
        assert_eq!(report.entries, 1, "{report:?}");
        assert_eq!(
            report.bytes_estimate,
            b"retired view bytes".len() as u64,
            "{report:?}"
        );
        assert_eq!(report.kept_by_reason.get("witness"), Some(&1), "{report:?}");
        assert!(
            !report.incomplete,
            "known retained trash is not an incomplete census: {report:?}"
        );
        assert!(entries[0].join("payload").exists());
    }

    #[test]
    fn retention_trash_followup_budget_stop_is_incomplete() {
        let (temp, entries) = followup_trash_fixture(1);
        let deadline = Instant::now();
        let mut budget =
            crate::bash_background::persistence::RepairBudget::until(deadline, Arc::new(|| false));
        let mut report = TrashReport::default();
        assert!(reap_trash_with_snapshot_budget(
            &StoragePaths::same(temp.path()),
            entries[0].parent().unwrap(),
            true,
            deadline,
            &mut budget,
            &mut report,
            NO_TIME_BOUND
        )
        .unwrap());
        assert!(
            report.incomplete,
            "a stopped iterator must leave an incomplete census"
        );
        assert_eq!(report.entries + report.removed_entries, 0);
        assert_eq!(report.kept_by_reason.get("enumeration_budget"), Some(&1));
        assert!(entries[0].join("payload").exists());
    }

    #[test]
    fn retention_trash_cursor_retries_prefetched_unprocessed_entries() {
        use std::sync::atomic::{AtomicBool, Ordering};
        let (temp, entries) = followup_trash_fixture(3);
        let trash = entries[0].parent().unwrap();
        let cancelled = Arc::new(AtomicBool::new(false));
        let signal = cancelled.clone();
        let hook = observe_test_hook(
            temp.path(),
            Arc::new(move |step| {
                if step == "reap-root" {
                    signal.store(true, Ordering::SeqCst);
                }
            }),
        );
        let deadline = Instant::now() + Duration::from_secs(60);
        let mut budget = crate::bash_background::persistence::RepairBudget::until(
            deadline,
            Arc::new(move || cancelled.load(Ordering::SeqCst)),
        );
        let mut report = TrashReport::default();
        assert!(reap_trash_with_snapshot_budget(
            &StoragePaths::same(temp.path()),
            trash,
            true,
            deadline,
            &mut budget,
            &mut report,
            NO_TIME_BOUND
        )
        .unwrap());
        assert!(report.incomplete);
        assert_eq!(report.entries, 1);
        drop(hook);
        for expected_removed in [2, 1] {
            let mut budget = crate::bash_background::persistence::RepairBudget::new();
            let mut report = TrashReport::default();
            reap_trash_with_snapshot_budget(
                &StoragePaths::same(temp.path()),
                trash,
                true,
                Instant::now() + Duration::from_secs(60),
                &mut budget,
                &mut report,
                NO_TIME_BOUND,
            )
            .unwrap();
            assert_eq!(report.removed_entries, expected_removed, "{report:?}");
        }
        assert!(
            !trash.exists(),
            "the prefetched tail and cancelled entry must all be retried"
        );
    }

    #[cfg(unix)]
    #[test]
    fn retention_trash_cursor_resets_when_container_identity_changes() {
        let (temp, entries) = followup_trash_fixture(3);
        for entry in &entries {
            fs::remove_file(entry.join(RETIREMENT_WITNESS)).unwrap();
        }
        let trash = entries[0].parent().unwrap();
        let hook = observe_test_hook(
            trash,
            Arc::new(|step| {
                if step == "trash-snapshot-entry" {
                    std::thread::sleep(Duration::from_millis(12));
                }
            }),
        );
        let mut budget = crate::bash_background::persistence::RepairBudget::new();
        reap_trash(
            &StoragePaths::same(temp.path()),
            trash,
            true,
            Instant::now() + Duration::from_secs(60),
            &mut budget,
            &mut TrashReport::default(),
        )
        .unwrap();
        drop(hook);
        let old = temp.path().join("old-trash");
        fs::rename(trash, &old).unwrap();
        let fresh = temp.path().join("views/ffffffffffffffff");
        fs::create_dir(&fresh).unwrap();
        fs::write(fresh.join("payload"), b"fresh verified bytes").unwrap();
        retire_directory(temp.path(), &fresh).unwrap();
        let mut budget = crate::bash_background::persistence::RepairBudget::new();
        let mut report = TrashReport::default();
        reap_trash_with_snapshot_budget(
            &StoragePaths::same(temp.path()),
            trash,
            true,
            Instant::now() + Duration::from_secs(60),
            &mut budget,
            &mut report,
            NO_TIME_BOUND,
        )
        .unwrap();
        assert_eq!(
            report.removed_entries, 1,
            "replacement must start a new scan: {report:?}"
        );
        for entry in &entries {
            assert!(old
                .join(entry.file_name().unwrap())
                .join("payload")
                .exists());
        }
    }

    #[test]
    fn retention_trash_fully_measured_empty_unverified_directory_is_not_incomplete() {
        let (temp, entries) = followup_trash_fixture(1);
        fs::remove_file(entries[0].join("payload")).unwrap();
        fs::remove_file(entries[0].join(RETIREMENT_WITNESS)).unwrap();
        let mut budget = crate::bash_background::persistence::RepairBudget::new();
        let mut report = TrashReport::default();
        assert!(reap_trash_with_snapshot_budget(
            &StoragePaths::same(temp.path()),
            entries[0].parent().unwrap(),
            true,
            Instant::now() + Duration::from_secs(60),
            &mut budget,
            &mut report,
            NO_TIME_BOUND
        )
        .unwrap());
        assert!(
            !report.incomplete,
            "zero measured bytes do not mean measurement failed: {report:?}"
        );
        assert_eq!(report.bytes_estimate, 0);
        assert!(entries[0].exists());
    }

    fn retired_view_fixture() -> (tempfile::TempDir, PathBuf) {
        let temp = tempfile::tempdir().unwrap();
        fs::create_dir_all(temp.path().join("retention")).unwrap();
        let view = temp.path().join("views/0123456789abcdef");
        fs::create_dir_all(&view).unwrap();
        for name in [
            "derived-abc.sqlite",
            "derived-abc.sqlite-shm",
            "derived-abc.sqlite-wal",
            "derived-abc.ref",
            "cache.lock",
        ] {
            fs::write(view.join(name), b"retired payload").unwrap();
        }
        fs::OpenOptions::new()
            .write(true)
            .open(view.join("derived-abc.sqlite"))
            .unwrap()
            .set_len(1_500_000_000)
            .unwrap();
        retire_directory(temp.path(), &view).unwrap();
        let retired = fs::read_dir(temp.path().join("views/.retention-trash"))
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        (temp, retired)
    }

    #[test]
    fn retention_trash_real_view_shape_is_reaped() {
        let (temp, retired) = retired_view_fixture();
        reap_retired(temp.path()).unwrap();
        assert!(
            !retired.exists(),
            "a verified 1.5 GB view must be reclaimed"
        );
    }

    #[test]
    fn retention_trash_expired_sweep_still_reaps_verified_view() {
        let (temp, retired) = retired_view_fixture();
        fs::create_dir(temp.path().join("views/ffffffffffffffff")).unwrap();
        let report = run_pass_with_budget(
            temp.path(),
            &|| false,
            &|| Instant::now() + NO_TIME_BOUND * 2,
            NO_TIME_BOUND,
        );
        assert!(report.stopped_early, "{report:?}");
        let until = Instant::now() + Duration::from_secs(10);
        while reaper_running_for_test(temp.path()) && Instant::now() < until {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(
            !retired.exists(),
            "expired sweep must still launch trash reclaim"
        );
        assert_eq!(trash_report(temp.path()).removed_entries, 1);
    }

    fn trash_report(storage: &Path) -> TrashReport {
        states()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(storage)
            .unwrap()
            .trash
            .clone()
    }

    fn assert_trash_kept_once(storage: &Path, entry: &Path, reason: &'static str) -> TrashReport {
        let (result, logs) = crate::logging::capture_log_lines(|| reap_retired(storage));
        if let Err(error) = result {
            assert_eq!(error.kind(), io::ErrorKind::Interrupted, "{error}");
        }
        let report = trash_report(storage);
        assert!(entry.exists(), "guard {reason} removed the entry");
        assert_eq!(report.entries, 1, "{report:?}");
        assert_eq!(report.removed_entries, 0, "{report:?}");
        assert_eq!(report.kept_by_reason.get(reason), Some(&1), "{report:?}");
        assert_eq!(
            logs.iter()
                .filter(|line| warning_names_entry(line, entry, reason))
                .count(),
            1,
            "one retention warning per kept entry: {logs:?}"
        );
        report
    }

    fn warning_names_entry(line: &str, entry: &Path, reason: &str) -> bool {
        let Some(rest) = line.strip_prefix("WARN storage retention trash=") else {
            return false;
        };
        let Some((logged_path, _)) = rest.split_once(&format!(" retained reason={reason}:")) else {
            return false;
        };
        // Windows TEMP may use an 8.3 alias or different separators. Compare
        // resolved identities of the path spellings, not the displayed bytes.
        match (fs::canonicalize(logged_path), fs::canonicalize(entry)) {
            (Ok(logged), Ok(expected)) => logged == expected,
            _ => false,
        }
    }

    #[test]
    fn retention_trash_warning_matches_equivalent_path_spellings_only() {
        let (temp, entries) = followup_trash_fixture(2);
        let entry = &entries[0];
        let alias = entry
            .parent()
            .unwrap()
            .join(".")
            .join(entry.file_name().unwrap());
        let line = format!(
            "WARN storage retention trash={} retained reason=witness: missing",
            alias.display()
        );
        assert!(warning_names_entry(&line, entry, "witness"));
        assert!(!warning_names_entry(&line, &entries[1], "witness"));
        assert!(!warning_names_entry(&line, entry, "foreign_name"));
        drop(temp);
    }

    #[test]
    fn retention_trash_guard_missing_witness_is_logged_and_counted() {
        let (temp, retired) = retired_view_fixture();
        fs::remove_file(retired.join(RETIREMENT_WITNESS)).unwrap();
        let report = assert_trash_kept_once(temp.path(), &retired, "witness");
        assert!(report.bytes_estimate >= 1_500_000_000, "{report:?}");
        assert!(report.oldest_age_ms.is_some(), "{report:?}");
    }

    #[test]
    fn retention_trash_guard_foreign_owner_is_logged_and_counted() {
        let (temp, retired) = retired_view_fixture();
        let path = retired.join(RETIREMENT_WITNESS);
        let mut witness: RetirementWitness = read_json(&path).unwrap();
        witness.owner = (0, 0);
        fs::write(path, serde_json::to_vec(&witness).unwrap()).unwrap();
        assert_trash_kept_once(temp.path(), &retired, "witness");
    }

    fn change_witness(entry: &Path, change: impl FnOnce(&mut RetirementWitness)) {
        let path = entry.join(RETIREMENT_WITNESS);
        let mut witness: RetirementWitness = read_json(&path).unwrap();
        change(&mut witness);
        fs::write(path, serde_json::to_vec(&witness).unwrap()).unwrap();
    }

    #[test]
    fn retention_trash_guard_witness_nonce() {
        let (temp, retired) = retired_view_fixture();
        change_witness(&retired, |witness| witness.nonce = "foreign".into());
        assert_trash_kept_once(temp.path(), &retired, "witness");
    }

    #[test]
    fn retention_trash_guard_witness_directory_identity() {
        let (temp, retired) = retired_view_fixture();
        change_witness(&retired, |witness| witness.directory = (0, 0));
        assert_trash_kept_once(temp.path(), &retired, "witness");
    }

    #[test]
    fn retention_trash_guard_witness_version() {
        let (temp, retired) = retired_view_fixture();
        change_witness(&retired, |witness| witness.version = 2);
        assert_trash_kept_once(temp.path(), &retired, "witness");
    }

    #[test]
    fn retention_trash_guard_changed_member_is_logged_and_counted() {
        let (temp, retired) = retired_view_fixture();
        fs::write(retired.join("unwitnessed.sqlite"), b"foreign").unwrap();
        assert_trash_kept_once(temp.path(), &retired, "witness");
    }

    #[test]
    fn retention_trash_guard_foreign_name_is_logged_and_counted() {
        let (temp, retired) = retired_view_fixture();
        let foreign = retired.with_file_name("foreign-view");
        fs::rename(retired, &foreign).unwrap();
        assert_trash_kept_once(temp.path(), &foreign, "foreign_name");
    }

    #[test]
    fn retention_trash_guard_unverified_empty_directory_is_kept() {
        let (temp, retired) = retired_view_fixture();
        fs::remove_dir_all(&retired).unwrap();
        fs::create_dir(&retired).unwrap();
        assert_trash_kept_once(temp.path(), &retired, "witness");
    }

    const UNWITNESSED_GENERATION: &str = "0123456789abcdef.g1761300000000000000.4242.sqlite";

    /// An `inspect` cache folder retired by AFT v0.62.0 or earlier, which
    /// wrote no retirement witness: the files a real one holds, and a read
    /// marker whose process (on another host) stopped heartbeating long ago.
    fn unwitnessed_inspect_retirement() -> (tempfile::TempDir, PathBuf) {
        let temp = tempfile::tempdir().unwrap();
        fs::create_dir_all(temp.path().join("retention")).unwrap();
        let retired = temp.path().join(format!(
            "inspect/.retention-trash/retired-{}",
            "0123456789abcdef".repeat(2)
        ));
        let generation = retired.join("readers").join(UNWITNESSED_GENERATION);
        fs::create_dir_all(&generation).unwrap();
        for name in [
            "writer.lease".to_owned(),
            "cache.lock".into(),
            "semantic.lock".into(),
            "symbols.lock".into(),
            "0123456789abcdef.current".into(),
            UNWITNESSED_GENERATION.into(),
            format!("{UNWITNESSED_GENERATION}-wal"),
        ] {
            fs::write(retired.join(name), b"inspect cache").unwrap();
        }
        let marker = generation.join("4242.other-host.1761300000000.0.json");
        fs::write(
            &marker,
            serde_json::to_vec(&crate::root_cache::ReadMarkerMetadata {
                pid: 4242,
                hostname: "other-host".into(),
                created_at_ms: 1_761_300_000_000,
                nonce: None,
            })
            .unwrap(),
        )
        .unwrap();
        filetime::set_file_mtime(
            &marker,
            filetime::FileTime::from_unix_time(1_000_000_000, 0),
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&retired, fs::Permissions::from_mode(0o700)).unwrap();
        }
        (temp, retired)
    }

    #[test]
    fn retention_trash_reaps_unwitnessed_inspect_cache_retirement() {
        let (temp, retired) = unwitnessed_inspect_retirement();
        super::reap_retired(&StoragePaths::same(temp.path())).unwrap();
        assert!(
            !retired.exists(),
            "a witness-less retirement holding only AFT cache files was kept"
        );
        assert_eq!(trash_report(temp.path()).removed_entries, 1);
    }

    #[test]
    fn retention_trash_keeps_unwitnessed_retirements_it_cannot_prove() {
        type Change = Box<dyn Fn(&Path, &Path)>;
        #[cfg_attr(not(unix), allow(unused_mut))]
        let mut cases: Vec<(&str, &str, Change)> = vec![
            (
                "foreign file",
                "witness",
                Box::new(|retired, _| fs::write(retired.join("notes.txt"), b"user data").unwrap()),
            ),
            (
                "live reader",
                "witness",
                // A marker from another host that heartbeated just now.
                Box::new(|retired, _| {
                    let marker = retired
                        .join("readers")
                        .join(UNWITNESSED_GENERATION)
                        .join("4242.other-host.1761300000000.0.json");
                    filetime::set_file_mtime(marker, filetime::FileTime::now()).unwrap();
                }),
            ),
        ];
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            cases.push((
                "symlink",
                "tree_guard",
                Box::new(|retired, outside| {
                    std::os::unix::fs::symlink(outside, retired.join("cache.lock-link")).unwrap()
                }),
            ));
            cases.push((
                "group-writable file",
                "witness",
                Box::new(|retired, _| {
                    fs::set_permissions(
                        retired.join("cache.lock"),
                        fs::Permissions::from_mode(0o664),
                    )
                    .unwrap()
                }),
            ));
        }
        for (case, reason, change) in cases {
            let (temp, retired) = unwitnessed_inspect_retirement();
            let outside = temp.path().join("outside");
            fs::write(&outside, b"outside").unwrap();
            change(&retired, &outside);
            assert_trash_kept_once(temp.path(), &retired, reason);
            assert!(
                retired.join(UNWITNESSED_GENERATION).exists(),
                "{case}: cache payload was removed"
            );
            assert_eq!(fs::read(&outside).unwrap(), b"outside", "{case}");
        }
    }

    #[test]
    fn retention_trash_unwitnessed_reason_names_the_unrecognized_entry() {
        let (temp, retired) = unwitnessed_inspect_retirement();
        fs::write(retired.join("notes.txt"), b"user data").unwrap();
        let directory = crate::bash_background::persistence::PinnedDir::open(&retired).unwrap();
        let error = verify_retirement_proof(
            &StoragePaths::same(temp.path()),
            &directory,
            &"0123456789abcdef".repeat(2),
            &mut crate::bash_background::persistence::RepairBudget::new(),
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains(".aft-retirement.json is missing"), "{error}");
        assert!(
            error.contains("notes.txt is not an entry AFT creates in a cache folder"),
            "{error}"
        );
        assert!(error.contains("delete "), "{error}");
    }

    /// A retired cache with more entries than one validation walk examines
    /// (`TREE_ENTRIES`), split over subfolders the way real caches are.
    fn oversized_retirement(temp: &Path) -> PathBuf {
        fs::create_dir_all(temp.join("retention")).unwrap();
        let cache = temp.join("index/0123456789abcdef");
        for folder in 0..8 {
            let folder = cache.join(format!("shard-{folder}"));
            fs::create_dir_all(&folder).unwrap();
            for file in 0..TREE_ENTRIES / 6 {
                fs::write(folder.join(format!("blob-{file}")), b"blob").unwrap();
            }
        }
        retire_directory(temp, &cache).unwrap();
        fs::read_dir(temp.join("index/.retention-trash"))
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path()
    }

    fn reap_index_trash(
        temp: &Path,
        budget: &mut crate::bash_background::persistence::RepairBudget,
    ) -> TrashReport {
        let mut report = TrashReport::default();
        reap_trash(
            &StoragePaths::same(temp),
            &temp.join("index/.retention-trash"),
            false,
            Instant::now() + Duration::from_secs(60),
            budget,
            &mut report,
        )
        .unwrap();
        report
    }

    #[test]
    fn retention_trash_removes_an_oversized_retirement_across_passes() {
        let temp = tempfile::tempdir().unwrap();
        let retired = oversized_retirement(temp.path());
        let count = |path: &Path| walkdir_count(path);
        let before = count(&retired);
        assert!(
            before > TREE_ENTRIES,
            "fixture must exceed one walk: {before}"
        );
        // A pass whose entry budget runs out part-way through the removal.
        let mut budget =
            crate::bash_background::persistence::RepairBudget::with_entries(TREE_ENTRIES / 2);
        let report = reap_index_trash(temp.path(), &mut budget);
        assert_eq!(
            report.kept_by_reason.get("removal_in_progress"),
            Some(&1),
            "{report:?}"
        );
        let after_first = count(&retired);
        assert!(after_first < before, "the first pass removed nothing");
        assert!(
            retired.join(RETIREMENT_WITNESS).is_file(),
            "the stopped removal lost the proof the next pass needs"
        );
        // The next pass continues from what is left, it does not start over.
        let report = reap_index_trash(
            temp.path(),
            &mut crate::bash_background::persistence::RepairBudget::new(),
        );
        assert_eq!(report.removed_entries, 1, "{report:?}");
        assert!(!retired.exists());
    }

    #[cfg(unix)]
    #[test]
    fn retention_trash_oversized_removal_stops_at_a_symlink() {
        let temp = tempfile::tempdir().unwrap();
        let retired = oversized_retirement(temp.path());
        let before = walkdir_count(&retired);
        let outside = temp.path().join("outside");
        fs::write(&outside, b"outside").unwrap();
        // Plant the link after the validation walk and right before removal,
        // so the incremental removal (not the validation walk, whose reach
        // depends on directory order) is what meets it.
        let link = retired.join("shard-7/zz-link");
        let planted = (outside.clone(), link.clone());
        let _hook = observe_test_hook(
            temp.path(),
            Arc::new(move |step| {
                if step == "reap-root" && fs::symlink_metadata(&planted.1).is_err() {
                    std::os::unix::fs::symlink(&planted.0, &planted.1).unwrap();
                }
            }),
        );
        let (report, logs) = crate::logging::capture_log_lines(|| {
            reap_index_trash(
                temp.path(),
                &mut crate::bash_background::persistence::RepairBudget::new(),
            )
        });
        assert_eq!(
            report.kept_by_reason.get("tree_guard"),
            Some(&1),
            "{report:?}"
        );
        assert!(
            logs.iter()
                .any(|line| line.contains("retained reason=tree_guard")
                    && line.contains("non-regular entry retained")
                    && line.contains("zz-link")),
            "the incremental removal did not report the link: {logs:?}"
        );
        assert!(
            walkdir_count(&retired) < before,
            "the incremental removal never ran"
        );
        assert!(fs::symlink_metadata(&link).is_ok());
        assert!(retired.join(RETIREMENT_WITNESS).is_file());
        assert_eq!(fs::read(&outside).unwrap(), b"outside");
    }

    fn walkdir_count(path: &Path) -> usize {
        let mut count = 0;
        let mut pending = vec![path.to_path_buf()];
        while let Some(directory) = pending.pop() {
            for entry in fs::read_dir(directory).unwrap() {
                let entry = entry.unwrap();
                count += 1;
                if entry.file_type().unwrap().is_dir() {
                    pending.push(entry.path());
                }
            }
        }
        count
    }

    /// Every trash container shares one durable-storage census per pass. A
    /// census per container spent the pass budget before later domains, so
    /// their entries were kept with "repair traversal limit exceeded".
    #[test]
    fn retention_trash_census_is_taken_once_per_pass() {
        let temp = tempfile::tempdir().unwrap();
        fs::create_dir_all(temp.path().join("retention")).unwrap();
        // Durable state the census walks: every name costs one budget entry.
        let durable = temp.path().join("durable-state");
        fs::create_dir(&durable).unwrap();
        for file in 0..1500 {
            fs::write(durable.join(format!("record-{file}")), b"r").unwrap();
        }
        let mut retired = Vec::new();
        for domain in ["views", "inspect"] {
            let cache = temp.path().join(domain).join("0123456789abcdef");
            fs::create_dir_all(&cache).unwrap();
            fs::write(cache.join("payload"), b"retired").unwrap();
            retire_directory(temp.path(), &cache).unwrap();
            retired.push(temp.path().join(domain).join(".retention-trash"));
        }
        // Room for one census of the durable state, not one per container.
        let mut budget = crate::bash_background::persistence::RepairBudget::with_entries(2500);
        let mut report = TrashReport::default();
        reap_retired_with_budget(
            &StoragePaths::same(temp.path()),
            Instant::now() + Duration::from_secs(60),
            &mut budget,
            &mut report,
        )
        .unwrap();
        assert_eq!(report.removed_entries, 2, "{report:?}");
        for trash in retired {
            assert_eq!(
                fs::read_dir(&trash).unwrap().count(),
                0,
                "{}",
                trash.display()
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn retention_trash_guard_symlink_is_logged_and_counted() {
        let (temp, retired) = retired_view_fixture();
        let canary = temp.path().join("canary");
        fs::write(&canary, b"must survive").unwrap();
        std::os::unix::fs::symlink(&canary, retired.join("symlink")).unwrap();
        assert_trash_kept_once(temp.path(), &retired, "tree_guard");
        assert_eq!(fs::read(canary).unwrap(), b"must survive");
    }

    #[cfg(unix)]
    #[test]
    fn retention_trash_guard_protected_directory_is_logged_and_counted() {
        let (temp, retired) = retired_view_fixture();
        std::os::unix::fs::symlink(&retired, temp.path().join("durable-alias")).unwrap();
        assert_trash_kept_once(temp.path(), &retired, "unlink_guard");
        assert!(retired.join("derived-abc.sqlite").exists());
    }

    #[test]
    fn retention_trash_guard_foreign_filesystem_is_logged_and_counted() {
        let (temp, retired) = retired_view_fixture();
        let nested = retired.join("foreign-mount");
        fs::create_dir(&nested).unwrap();
        fs::write(nested.join("canary"), b"foreign").unwrap();
        // Windows raw metadata has no baseline device id; inject both sides
        // so the portable boundary comparison is exercised on every platform.
        let _baseline =
            crate::walk_boundary::simulated_volumes::simulate(temp.path(), u64::MAX - 20);
        let _mount = crate::walk_boundary::simulated_volumes::simulate(&nested, u64::MAX - 21);
        assert_trash_kept_once(temp.path(), &retired, "tree_guard");
        assert!(nested.join("canary").exists());
    }

    #[test]
    fn retention_trash_guard_reap_lock_is_reported_without_waiting() {
        let (temp, retired) = retired_view_fixture();
        let lock = temp.path().join("retention/reap.lock");
        let _lease = crate::fs_lock::try_acquire(&lock, Duration::ZERO).unwrap();
        assert!(reap_retired(temp.path()).is_err());
        let report = trash_report(temp.path());
        assert_eq!(report.kept_by_reason.get("reap_lock"), Some(&1));
        assert!(report.incomplete);
        assert!(retired.exists());
    }

    #[test]
    fn retention_trash_guard_deadline_and_cancellation_bound_the_iterator() {
        let (temp, retired) = retired_view_fixture();
        for cancelled in [false, true] {
            let deadline = if cancelled {
                Instant::now() + Duration::from_secs(60)
            } else {
                Instant::now()
            };
            let mut budget = crate::bash_background::persistence::RepairBudget::until(
                deadline,
                Arc::new(move || cancelled),
            );
            let mut report = TrashReport::default();
            assert!(reap_trash(
                &StoragePaths::same(temp.path()),
                retired.parent().unwrap(),
                false,
                deadline,
                &mut budget,
                &mut report
            )
            .unwrap());
            assert_eq!(
                report.kept_by_reason.get("enumeration_budget"),
                Some(&1),
                "{report:?}"
            );
            assert!(report.incomplete);
            assert!(retired.join("derived-abc.sqlite").exists());
        }
    }

    #[test]
    fn retention_trash_guard_cancellation_before_unlink() {
        use std::sync::atomic::{AtomicBool, Ordering};
        let (temp, retired) = retired_view_fixture();
        let cancelled = Arc::new(AtomicBool::new(false));
        let observed = cancelled.clone();
        states()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .entry(temp.path().to_path_buf())
            .or_default()
            .reaper_cancelled = Some(Arc::new(move || observed.load(Ordering::SeqCst)));
        let _hook = observe_test_hook(
            temp.path(),
            Arc::new(move |step| {
                if step == "reap-root" {
                    cancelled.store(true, Ordering::SeqCst);
                }
            }),
        );
        assert_trash_kept_once(temp.path(), &retired, "unlink_guard");
        assert!(retired.join("derived-abc.sqlite").exists());
    }

    #[test]
    fn retention_trash_guard_mount_admission_is_logged_and_counted() {
        let (temp, retired) = retired_view_fixture();
        let _admission = barrier(temp.path()).unwrap();
        let deadline = Instant::now() + Duration::from_secs(60);
        let mut budget = crate::bash_background::persistence::RepairBudget::new();
        let mut report = TrashReport::default();
        let trash = retired.parent().unwrap();
        let (result, logs) = crate::logging::capture_log_lines(|| {
            reap_trash(
                &StoragePaths::same(temp.path()),
                trash,
                true,
                deadline,
                &mut budget,
                &mut report,
            )
        });
        assert!(result.unwrap());
        assert_eq!(report.kept_by_reason.get("admission_busy"), Some(&1));
        assert_eq!(
            logs.iter()
                .filter(|line| line.contains("retained reason=admission_busy:"))
                .count(),
            1
        );
        assert!(report.incomplete);
        assert!(retired.exists());
    }

    #[test]
    fn retention_trash_guard_mount_root_is_logged_and_counted() {
        let (temp, retired) = retired_view_fixture();
        let _baseline =
            crate::walk_boundary::simulated_volumes::simulate(temp.path(), u64::MAX - 23);
        let _mount = crate::walk_boundary::simulated_volumes::simulate(&retired, u64::MAX - 22);
        let deadline = Instant::now() + Duration::from_secs(60);
        let mut budget = crate::bash_background::persistence::RepairBudget::new();
        let mut report = TrashReport::default();
        assert!(reap_trash(
            &StoragePaths::same(temp.path()),
            retired.parent().unwrap(),
            true,
            deadline,
            &mut budget,
            &mut report
        )
        .unwrap());
        assert_eq!(
            report.kept_by_reason.get("mount_guard"),
            Some(&1),
            "{report:?}"
        );
        assert!(retired.join("derived-abc.sqlite").exists());
    }

    #[cfg(windows)]
    #[test]
    fn retention_trash_windows_pinned_removal_refuses_volume_and_mount_mismatch() {
        use crate::bash_background::persistence::{PinnedDir, REPAIR_IDENTITIES, REPAIR_MOUNTS};
        let (_temp, retired) = retired_view_fixture();
        let parent = PinnedDir::repair_open_path(retired.parent().unwrap()).unwrap();
        let name = retired.file_name().unwrap();
        let child = parent.repair_child(name).unwrap();
        let expected = child.directory_identity().unwrap();
        let child_path = child.path().to_path_buf();
        drop(child);
        for foreign_volume in [true, false] {
            if foreign_volume {
                REPAIR_IDENTITIES.with(|ids| {
                    ids.borrow_mut()
                        .insert(child_path.clone(), (expected.0 ^ 1, expected.1))
                });
            } else {
                REPAIR_MOUNTS.with(|ids| ids.borrow_mut().insert(child_path.clone(), u64::MAX));
            }
            let mut removed = 0;
            let result = parent.repair_remove_child(name, expected, &[], &mut removed);
            REPAIR_IDENTITIES.with(|ids| ids.borrow_mut().remove(&child_path));
            REPAIR_MOUNTS.with(|ids| ids.borrow_mut().remove(&child_path));
            let error = result.expect_err("pinned deletion must refuse a foreign handle boundary");
            assert!(
                error.to_string().contains("mounted or protected directory"),
                "{error}"
            );
            assert_eq!(removed, 0);
            assert!(retired.join("derived-abc.sqlite").exists());
            assert!(retired.join(RETIREMENT_WITNESS).exists());
        }
    }

    #[cfg(windows)]
    #[test]
    fn retention_trash_windows_pinned_removal_refuses_junction() {
        use crate::bash_background::persistence::PinnedDir;
        let temp = tempfile::tempdir().unwrap();
        let parent_path = temp.path().join("parent");
        let target = temp.path().join("foreign-target");
        fs::create_dir(&parent_path).unwrap();
        fs::create_dir(&target).unwrap();
        fs::write(target.join("canary"), b"foreign junction bytes").unwrap();
        let junction = parent_path.join("junction");
        let output = std::process::Command::new("cmd.exe")
            .args(["/D", "/C", "mklink", "/J"])
            .arg(&junction)
            .arg(&target)
            .output()
            .unwrap();
        assert!(output.status.success(), "mklink /J failed: {output:?}");
        let parent = PinnedDir::repair_open_path(&parent_path).unwrap();
        let mut removed = 0;
        let error = parent
            .repair_remove_child(junction.file_name().unwrap(), (0, 0), &[], &mut removed)
            .expect_err("pinned removal must reject the junction before its target");
        assert_eq!(error.kind(), io::ErrorKind::InvalidData, "{error}");
        assert!(error.to_string().contains("reparse point"), "{error}");
        assert_eq!(removed, 0);
        assert_eq!(
            fs::read(target.join("canary")).unwrap(),
            b"foreign junction bytes"
        );
        fs::remove_dir(junction).unwrap();
    }

    #[test]
    fn retention_trash_guard_mount_snapshot_yields_to_bind() {
        let (temp, retired) = retired_view_fixture();
        let pending = temp.path().join("retention/bind-pending.lock");
        let _binder = crate::fs_lock::try_acquire(&pending, Duration::ZERO).unwrap();
        let deadline = Instant::now() + Duration::from_secs(60);
        let mut budget = crate::bash_background::persistence::RepairBudget::new();
        let mut report = TrashReport::default();
        let (result, logs) = crate::logging::capture_log_lines(|| {
            reap_trash(
                &StoragePaths::same(temp.path()),
                retired.parent().unwrap(),
                true,
                deadline,
                &mut budget,
                &mut report,
            )
        });
        assert!(result.unwrap());
        assert_eq!(
            report.kept_by_reason.get("bind_pending"),
            Some(&1),
            "{report:?}"
        );
        assert_eq!(
            logs.iter()
                .filter(|line| line.contains("retained reason=bind_pending:"))
                .count(),
            1
        );
        assert!(report.incomplete);
        assert!(retired.exists());
    }

    #[test]
    fn retention_trash_guard_mount_snapshot_admission_deadline() {
        let (temp, retired) = retired_view_fixture();
        let trash = retired.parent().unwrap();
        let _hook = observe_test_hook(
            trash,
            Arc::new(|step| {
                if step == "trash-snapshot" {
                    std::thread::sleep(Duration::from_millis(20));
                }
            }),
        );
        let deadline = Instant::now() + Duration::from_secs(60);
        let mut budget = crate::bash_background::persistence::RepairBudget::new();
        let mut report = TrashReport::default();
        let (result, logs) = crate::logging::capture_log_lines(|| {
            reap_trash(
                &StoragePaths::same(temp.path()),
                trash,
                true,
                deadline,
                &mut budget,
                &mut report,
            )
        });
        assert!(result.unwrap());
        assert_eq!(
            report.kept_by_reason.get("admission_budget"),
            Some(&1),
            "{report:?}"
        );
        assert_eq!(
            logs.iter()
                .filter(|line| line.contains("retained reason=admission_budget:"))
                .count(),
            1
        );
        assert!(report.incomplete);
        assert!(retired.exists());
    }

    #[test]
    fn bind_profile_separates_waits_from_publication() {
        let root = tempfile::tempdir().unwrap();
        let storage = tempfile::tempdir().unwrap();
        super::record_bind(
            &super::StoragePaths::same(storage.path()),
            root.path(),
            "0123456789abcdef",
        )
        .unwrap();
        let line = super::LAST_BIND_PROFILE.with(|slot| slot.borrow_mut().take().unwrap());
        assert!(line.starts_with("retention_bind_profile storage="));
        assert!(line.contains("phases_us=[metadata="));
        for phase in [
            "queue_wait",
            "admission_wait",
            "sync_files",
            "sync_directory",
        ] {
            assert!(line.contains(&format!(",{phase}=")), "{line}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn reaper_retains_a_source_swapped_in_the_final_unix_rename_window() {
        use super::*;
        let temp = tempfile::tempdir().unwrap();
        fs::create_dir(temp.path().join("retention")).unwrap();
        let cache = temp.path().join("index/key");
        fs::create_dir_all(&cache).unwrap();
        fs::write(cache.join("own"), "own").unwrap();
        let foreign = temp.path().join("foreign");
        fs::create_dir(&foreign).unwrap();
        fs::write(foreign.join("precious"), "precious").unwrap();
        let source = cache.clone();
        let parked = temp.path().join("parked");
        crate::bash_background::persistence::BEFORE_REPAIR_RENAME.with(|hook| {
            *hook.borrow_mut() = Some(Box::new(move || {
                fs::rename(&source, &parked).unwrap();
                fs::rename(&foreign, &source).unwrap();
            }))
        });
        let error = retire_directory(temp.path(), &cache)
            .expect_err("post-rename identity must reject the substituted source");
        assert!(
            error.to_string().contains("identity changed during rename"),
            "{error}"
        );
        let retired = fs::read_dir(temp.path().join("index/.retention-trash"))
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        super::reap_retired(&StoragePaths::same(temp.path())).unwrap();
        assert_eq!(
            fs::read_to_string(retired.join("precious")).unwrap(),
            "precious",
            "unverified retirement was deleted after recovery"
        );
    }

    #[test]
    fn reaper_restarts_after_partial_removal_with_the_witness_still_present() {
        use super::*;
        let temp = tempfile::tempdir().unwrap();
        fs::create_dir(temp.path().join("retention")).unwrap();
        let cache = temp.path().join("index/key");
        fs::create_dir_all(&cache).unwrap();
        fs::write(cache.join("000-first"), "own").unwrap();
        fs::write(cache.join("zzz-last"), "own").unwrap();
        retire_directory(temp.path(), &cache).unwrap();
        let retired = fs::read_dir(temp.path().join("index/.retention-trash"))
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        crate::bash_background::persistence::with_repair_failure_after_for_test(1, || {
            super::reap_retired(&StoragePaths::same(temp.path()))
        })
        .unwrap();
        assert!(!retired.join("000-first").exists());
        assert!(
            retired.join(RETIREMENT_WITNESS).is_file(),
            "interrupted unlink lost its recovery proof"
        );
        super::reap_retired(&StoragePaths::same(temp.path())).unwrap();
        assert!(!retired.exists());
    }

    #[test]
    fn reaper_keeps_retired_directories_its_witness_does_not_vouch_for() {
        use super::*;
        let temp = tempfile::tempdir().unwrap();
        fs::create_dir(temp.path().join("retention")).unwrap();
        // A directory carrying a tombstone name but no retirement proof, as a
        // substituted source or a hand-made folder would after a crash.
        let trash = temp.path().join("index/.retention-trash");
        let forged = trash.join(format!("retired-{}", "a".repeat(32)));
        fs::create_dir_all(&forged).unwrap();
        fs::write(forged.join("precious"), "precious").unwrap();
        // A genuine retirement whose contents changed after it was recorded.
        let cache = temp.path().join("index/key");
        fs::create_dir_all(&cache).unwrap();
        fs::write(cache.join("own"), "own").unwrap();
        retire_directory(temp.path(), &cache).unwrap();
        let altered = fs::read_dir(&trash)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .find(|path| path != &forged)
            .unwrap();
        fs::write(altered.join("added-later"), "precious").unwrap();
        super::reap_retired(&StoragePaths::same(temp.path())).unwrap();
        assert_eq!(
            fs::read_to_string(forged.join("precious")).unwrap(),
            "precious",
            "a tombstone without a witness was deleted"
        );
        assert_eq!(
            fs::read_to_string(altered.join("added-later")).unwrap(),
            "precious",
            "a member the witness never recorded was deleted"
        );
    }
    #[test]
    fn residency_listing_failure_keeps_unknown_keys_protected() {
        use super::*;
        let temp = tempfile::tempdir().unwrap();
        let paths = StoragePaths::same(temp.path());
        let directory = temp.path().join("retention/readers/owner-scope");
        fs::create_dir_all(directory.parent().unwrap()).unwrap();
        fs::write(&directory, "not a directory").unwrap();
        let owner = crate::artifact_owner::ArtifactOwnerManifest {
            schema_version: 1,
            project_scope_key: "owner-scope".into(),
            checkout_path: "missing-checkout".into(),
            git_common_dir: None,
            pid: 0,
            hostname: "different-host".into(),
            created_at_ms: 0,
            heartbeat_at_ms: 0,
        };
        assert!(!owner_has_residency_marker(&paths, &owner));
        let owners = OwnerManifests(Ok(vec![("owner-family".into(), Ok(owner))]));
        assert!(
            owners.protect(&paths, "unknown-key", false).unwrap(),
            "an unreadable residency census must broaden protection, not authorize deletion"
        );
    }
    #[test]
    fn directory_removal_carries_the_admitted_root_identity_into_its_pin() {
        use super::*;
        let temp = tempfile::tempdir().unwrap();
        let durable = temp.path().join("storage");
        let parent = temp.path().join("cache-volume");
        fs::create_dir(&parent).unwrap();
        let mut config = crate::config::Config {
            storage_dir: Some(durable),
            cache_dir: Some(parent),
            ..Default::default()
        };
        assert!(crate::root_cache::prepare_cache_root_for_test(&mut config).is_none());
        let root = crate::root_cache::cache_root(&config);
        let cache = root.join("views/key");
        fs::create_dir_all(&cache).unwrap();
        fs::write(cache.join("own"), "own").unwrap();
        let foreign = temp.path().join("foreign-root");
        let precious = foreign.join("views/key/precious");
        fs::create_dir_all(precious.parent().unwrap()).unwrap();
        fs::write(&precious, "precious").unwrap();
        let parked = temp.path().join("parked-root");
        let a = root.clone();
        let b = foreign.clone();
        let p = parked.clone();
        // Substitute a real directory before removal starts. If removal accepts
        // that replacement, restore the admitted pathname immediately afterward
        // so later path checks cannot hide that removal pinned the wrong directory.
        let restored = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let done = restored.clone();
        let _hook = observe_test_hook(
            &cache,
            Arc::new(move |step| {
                if step == "remove-pinned" {
                    fs::rename(&a, &b).unwrap();
                    fs::rename(&p, &a).unwrap();
                    done.store(true, std::sync::atomic::Ordering::SeqCst);
                }
            }),
        );
        // Windows will not rename a folder while a file inside it is open, and
        // the cache lease keeps the root's in-use lock open. Release it so the
        // substitution below can be staged; the removal must still refuse it.
        #[cfg(windows)]
        {
            config.cache_lease = None;
        }
        fs::rename(&root, &parked).unwrap();
        fs::rename(&foreign, &root).unwrap();
        let result = remove_directory_if_closed(&cache);
        if !restored.load(std::sync::atomic::Ordering::SeqCst) {
            fs::rename(&root, &foreign).unwrap();
            fs::rename(&parked, &root).unwrap();
        }
        let error = result.expect_err("removal ran on a substituted root");
        assert!(
            error.to_string().contains("identity changed while pinning"),
            "{error}"
        );
        assert_eq!(fs::read_to_string(precious).unwrap(), "precious");
        assert_eq!(fs::read_to_string(cache.join("own")).unwrap(), "own");
    }

    #[test]
    fn retirement_refuses_a_source_replaced_after_validation() {
        use super::*;
        let temp = tempfile::tempdir().unwrap();
        let cache = temp.path().join("index/key");
        fs::create_dir_all(&cache).unwrap();
        fs::write(cache.join("own"), "own").unwrap();
        let foreign = temp.path().join("foreign");
        fs::create_dir(&foreign).unwrap();
        fs::write(foreign.join("precious"), "precious").unwrap();
        let source = cache.clone();
        let parked = temp.path().join("parked");
        let _hook = observe_test_hook(
            &cache,
            Arc::new(move |step| {
                if step == "rename-root" {
                    fs::rename(&source, &parked).unwrap();
                    fs::rename(&foreign, &source).unwrap();
                }
            }),
        );
        let result = retire_directory(temp.path(), &cache);
        assert!(
            result.is_err(),
            "a different directory was retired under the validated name"
        );
        assert_eq!(
            fs::read_to_string(cache.join("precious")).unwrap(),
            "precious"
        );
    }

    #[cfg(unix)]
    #[test]
    fn retention_flat_unlink_stays_on_pinned_domain_after_ancestor_swap() {
        use super::*;
        let (temp, root, key) = fixture();
        fs::remove_dir(&root).unwrap();
        let domain = temp.path().join("opencode/inspect");
        fs::create_dir_all(&domain).unwrap();
        let name = format!("{key}.payload");
        fs::write(domain.join(&name), "own").unwrap();
        let outside = temp.path().join("foreign");
        fs::create_dir(&outside).unwrap();
        fs::write(outside.join(&name), "precious").unwrap();
        let parked = temp.path().join("parked");
        let source = domain.clone();
        let target = outside.clone();
        let parked_hook = parked.clone();
        let _hook = observe_test_hook(
            &domain,
            Arc::new(move |step| {
                if step == "flat-unlink" {
                    fs::rename(&source, &parked_hook).unwrap();
                    std::os::unix::fs::symlink(&target, &source).unwrap();
                }
            }),
        );
        let paths = StoragePaths::same(temp.path());
        let mut report = SweepReport::default();
        let result = super::candidate(
            &paths,
            &mut RootIdentities::default(),
            &domain,
            &key,
            true,
            &super::bindings(&paths, &mut RootIdentities::default()).unwrap(),
            crate::pins::now_ms(),
            Instant::now() + PASS_BUDGET,
            &|| false,
            &mut ViewMembers::default(),
            &mut DueBatch::default(),
            &mut report,
        );
        assert_eq!(fs::read_to_string(outside.join(&name)).unwrap(), "precious");
        if let Err(error) = result {
            assert!(
                error.to_string().contains("non-directory cache retained"),
                "{error}"
            );
        }
        assert!(
            !parked.join(name).exists(),
            "the original pinned file must be removed"
        );
    }

    #[cfg(unix)]
    #[test]
    fn retention_retirement_pins_domain_and_mount_local_trash() {
        use super::*;
        for mounted in [false, true] {
            let temp = tempfile::tempdir().unwrap();
            let domain = temp.path().join("index");
            let cache = domain.join("key");
            fs::create_dir_all(&cache).unwrap();
            fs::write(cache.join("payload"), "own").unwrap();
            let outside = temp.path().join("foreign");
            fs::create_dir_all(outside.join("key/.retention-trash")).unwrap();
            fs::write(outside.join("key/payload"), "precious").unwrap();
            fs::create_dir_all(outside.join(".retention-trash")).unwrap();
            let _mounted = mounted
                .then(|| crate::walk_boundary::simulated_volumes::simulate(&cache, u64::MAX - 33));
            let parked = temp.path().join("parked");
            let source = domain.clone();
            let target = outside.clone();
            let _hook = observe_test_hook(
                &cache,
                Arc::new(move |step| {
                    if step == "rename-root" {
                        fs::rename(&source, &parked).unwrap();
                        std::os::unix::fs::symlink(&target, &source).unwrap();
                    }
                }),
            );
            retire_directory(temp.path(), &cache).unwrap();
            assert_eq!(
                fs::read_to_string(outside.join("key/payload")).unwrap(),
                "precious",
                "retirement followed a swapped ancestor (mounted={mounted})"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn retention_fd_reap_keeps_outside_canary_after_ancestor_swap() {
        use super::*;
        let temp = tempfile::tempdir().unwrap();
        let storage = StoragePaths::same(temp.path());
        fs::create_dir_all(temp.path().join("retention")).unwrap();
        let name = "retired-0123456789abcdef0123456789abcdef";
        let own = temp.path().join("views/.retention-trash").join(name);
        fs::create_dir_all(&own).unwrap();
        fs::write(own.join("payload"), "own").unwrap();
        let own_pin =
            crate::bash_background::persistence::PinnedDir::repair_open_path(&own).unwrap();
        write_retirement_witness(
            temp.path(),
            &own_pin,
            "0123456789abcdef0123456789abcdef",
            retirement_members(&own_pin).unwrap(),
        )
        .unwrap();
        let outside = temp.path().join("outside");
        let precious = outside.join(".retention-trash").join(name).join("payload");
        fs::create_dir_all(precious.parent().unwrap()).unwrap();
        fs::write(&precious, "outside").unwrap();
        let source = temp.path().join("views");
        let parked = temp.path().join("parked");
        let _hook = observe_test_hook(
            temp.path(),
            Arc::new(move |step| {
                if step == "reap-root" {
                    fs::rename(&source, &parked).unwrap();
                    std::os::unix::fs::symlink(&outside, &source).unwrap();
                }
            }),
        );
        reap_retired(&storage).unwrap();
        assert!(
            precious.is_file(),
            "the reaper must unlink only through pinned handles"
        );
        assert!(!temp
            .path()
            .join("parked/.retention-trash")
            .join(name)
            .exists());
    }

    #[cfg(unix)]
    #[test]
    fn retention_fd_view_removal_keeps_outside_canary_after_ancestor_swap() {
        use super::*;
        let temp = tempfile::tempdir().unwrap();
        let mut config = crate::config::Config {
            storage_dir: Some(temp.path().to_path_buf()),
            ..Default::default()
        };
        assert!(crate::root_cache::prepare_cache_root_for_test(&mut config).is_none());
        let parent = temp.path().join("views");
        let cache = parent.join("key");
        fs::create_dir_all(&cache).unwrap();
        fs::write(cache.join("payload"), "own").unwrap();
        let outside = temp.path().join("outside");
        let precious = outside.join("key/payload");
        fs::create_dir_all(precious.parent().unwrap()).unwrap();
        fs::write(&precious, "outside").unwrap();
        let parked = temp.path().join("parked");
        let _hook = observe_test_hook(
            &cache,
            Arc::new(move |step| {
                if step == "remove-root" {
                    fs::rename(&parent, &parked).unwrap();
                    std::os::unix::fs::symlink(&outside, &parent).unwrap();
                }
            }),
        );
        remove_directory_if_closed(&cache).unwrap();
        assert!(precious.is_file());
        assert!(!temp.path().join("parked/key").exists());
    }

    #[test]
    fn retention_barrier_cannot_recreate_unavailable_configured_volume() {
        use super::*;
        let temp = tempfile::tempdir().unwrap();
        let parent = temp.path().join("volume");
        fs::create_dir(&parent).unwrap();
        let mut config = crate::config::Config {
            cache_dir: Some(parent.clone()),
            storage_dir: Some(temp.path().join("storage")),
            ..Default::default()
        };
        assert!(crate::root_cache::prepare_cache_root_for_test(&mut config).is_none());
        let paths = StoragePaths::for_config(&config);
        // Windows will not move a folder while a file below it is open, and the
        // cache lease keeps the root's in-use lock open. Release it so the
        // volume can be taken away for this test.
        #[cfg(windows)]
        {
            config.cache_lease = None;
        }
        fs::rename(&parent, temp.path().join("unmounted")).unwrap();
        assert!(cache_barrier(&paths).is_err());
        assert!(
            !parent.exists(),
            "retention admission must not recreate an absent volume"
        );
    }

    #[test]
    fn retention_legacy_domains_are_enumerated_on_the_durable_root() {
        use super::*;
        let temp = tempfile::tempdir().unwrap();
        let storage = temp.path().join("storage");
        let artifacts = temp.path().join("artifacts");
        fs::create_dir_all(storage.join("opencode/callgraph")).unwrap();
        fs::create_dir_all(artifacts.join("other-tool/inspect")).unwrap();
        let paths = StoragePaths::new(storage.clone(), artifacts.clone());
        let domains = domain_paths(&paths).unwrap();
        assert!(domains.contains(&(storage.join("opencode/callgraph"), true)));
        assert!(domains.contains(&(artifacts.join("index"), false)));
        assert_eq!(
            relative_domain(&paths, &storage.join("opencode/callgraph")).unwrap(),
            PathBuf::from("opencode/callgraph")
        );
    }
    // Existing fixtures intentionally put both roots in one temporary directory.
    // Keep that explicit at this test boundary; production always passes both.
    macro_rules! fixture_roots {
        ($name:ident($($arg:ident: $ty:ty),* $(,)?) -> $result:ty) => {
            fn $name(storage: &Path, $($arg: $ty),*) -> $result {
                super::$name(&StoragePaths::same(storage), $($arg),*)
            }
        };
    }
    fixture_roots!(run_pass(cancelled: &dyn Fn() -> bool) -> SweepReport);
    fixture_roots!(
        run_pass_with_clock(
            cancelled: &dyn Fn() -> bool,
            clock: &dyn Fn() -> Instant,
        ) -> SweepReport
    );
    fixture_roots!(
        run_pass_with_budget(
            cancelled: &dyn Fn() -> bool,
            clock: &dyn Fn() -> Instant,
            budget: Duration,
        ) -> SweepReport
    );
    fixture_roots!(
        visit_one_family(
            domain: &str,
            kind: &str,
            cancelled: &dyn Fn() -> bool,
            work: impl FnMut(&fs::DirEntry) -> io::Result<()>,
        ) -> io::Result<()>
    );
    fn bindings(storage: &Path) -> io::Result<BTreeMap<String, Vec<Binding>>> {
        super::bindings(&StoragePaths::same(storage), &mut RootIdentities::default())
    }

    fn root_identity_pass_fixture(linked: bool) -> (tempfile::TempDir, StoragePaths) {
        let temp = tempfile::tempdir().unwrap();
        let storage = StoragePaths::same(&temp.path().join("storage"));
        for index in 0..20 {
            fs::create_dir_all(temp.path().join(format!("checkout-{index}"))).unwrap();
        }
        for index in 0..1000 {
            let binding = Binding {
                root: temp.path().join(format!("checkout-{}", index % 20)),
                artifact_key: format!("{index:016x}"),
                last_bound_ms: crate::pins::now_ms(),
                volume: None,
                stable_volume: None,
                linked_worktree: linked,
            };
            atomic_json(
                &storage
                    .durable
                    .join(format!("retention/roots/{index:016x}.json")),
                &binding,
            )
            .unwrap();
        }
        // candidate_inner rereads history under sweep.lock before deleting an
        // unbound cache whose observation grace has expired.
        let key = "fffffffffffffffe";
        let cache = storage.artifacts.join("symbols").join(key);
        fs::create_dir_all(&cache).unwrap();
        atomic_json(
            &storage
                .durable
                .join(format!("retention/unknown/{key}.json")),
            &0u64,
        )
        .unwrap();
        (temp, storage)
    }

    fn assert_pass_root_resolutions(linked: bool, expected: usize) {
        let (_temp, storage) = root_identity_pass_fixture(linked);
        let reads = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let observed = reads.clone();
        let _hook = observe_test_hook(
            &storage.artifacts,
            Arc::new(move |step| {
                if step == "history-read" {
                    observed.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                }
            }),
        );
        let mut roots = RootIdentities::default();
        let now = Instant::now();
        let report = super::run_pass_with_root_identities(
            &storage,
            &|| false,
            &|| now,
            Duration::from_secs(3600),
            &mut roots,
        );
        assert!(report.errors.is_empty(), "{report:?}");
        assert!(
            reads.load(std::sync::atomic::Ordering::SeqCst) >= 2,
            "{report:?}"
        );
        assert_eq!(report.removed_roots, 1, "{report:?}");
        assert_eq!(roots.resolutions, expected);
    }

    #[test]
    fn storage_retention_skips_root_resolution_without_linked_roots() {
        assert_pass_root_resolutions(false, 0);
    }

    #[test]
    fn storage_retention_resolves_distinct_roots_once_per_pass() {
        assert_pass_root_resolutions(true, 20);
    }

    #[test]
    fn storage_retention_root_memo_preserves_merged_bindings_and_admission() {
        let temp = tempfile::tempdir().unwrap();
        let storage = StoragePaths::same(&temp.path().join("storage"));
        let linked = temp.path().join("linked-worktree");
        let live = temp.path().join("main-checkout");
        let missing = temp.path().join("removed-worktree");
        fs::create_dir(&linked).unwrap();
        fs::create_dir(&live).unwrap();
        let file = temp.path().join("not-a-directory");
        fs::write(&file, b"file").unwrap();
        let unresolved = file.join("checkout");
        assert!(fs::canonicalize(&unresolved).is_err());
        let now = crate::pins::now_ms();
        let mut memo = BTreeMap::new();
        for (index, root) in [&linked, &live, &missing, &unresolved]
            .into_iter()
            .enumerate()
        {
            memo.insert(
                root.clone(),
                serde_json::json!({
                    "key": format!("{:016x}", index + 100), "recorded_at_ms": 0,
                }),
            );
            let binding = Binding {
                root: root.clone(),
                artifact_key: format!("{index:016x}"),
                last_bound_ms: if root == &live { now } else { 0 },
                volume: Some((temp.path().to_path_buf(), 7)),
                stable_volume: None,
                linked_worktree: root != &live,
            };
            atomic_json(
                &storage
                    .durable
                    .join(format!("retention/roots/{index:016x}.json")),
                &binding,
            )
            .unwrap();
        }
        atomic_json(&storage.durable.join("cache-keys.json"), &memo).unwrap();
        let owner_key = "fffffffffffffffc";
        crate::artifact_owner::write_synthetic_manifest_for_test(
            &storage.durable,
            &linked,
            owner_key,
            &crate::path_identity::project_scope_key(&linked),
            std::process::id(),
            0,
        );

        // The reference reader resolves every binding independently, as it did
        // before the pass-local memo, including nearest-ancestor fallbacks.
        let expected =
            bindings_with_root_resolver(&storage, false, &mut crate::root_cache::canonical_root)
                .unwrap();
        let mut roots = RootIdentities::default();
        for admission in [false, true, true] {
            let actual = bindings_with_admission(&storage, admission, &mut roots).unwrap();
            assert_eq!(actual, expected);
            for key in expected.keys() {
                let mut old_report = SweepReport::default();
                let mut new_report = SweepReport::default();
                assert_eq!(
                    eligible(&storage, key, &actual, now, ROOT_GRACE_MS, &mut new_report).unwrap(),
                    eligible(
                        &storage,
                        key,
                        &expected,
                        now,
                        ROOT_GRACE_MS,
                        &mut old_report
                    )
                    .unwrap(),
                    "{key}",
                );
                assert_eq!(new_report.skipped_live, old_report.skipped_live);
                assert_eq!(new_report.skipped_recent, old_report.skipped_recent);
                assert_eq!(
                    new_report.skipped_mount_mismatch,
                    old_report.skipped_mount_mismatch
                );
            }
        }
        assert_eq!(roots.resolutions, 4);
        let inherited = &expected[owner_key][0];
        assert!(inherited.linked_worktree);
        assert_eq!(inherited.volume, Some((temp.path().to_path_buf(), 7)));
        let unresolved_records = &expected[&format!("{:016x}", 103)];
        assert!(unresolved_records
            .iter()
            .all(|binding| binding.linked_worktree));
        assert_eq!(
            roots.canonical_root(&unresolved),
            crate::root_cache::canonical_root(&unresolved)
        );

        fs::write(
            storage.durable.join("retention/bind-pending.lock"),
            b"pending",
        )
        .unwrap();
        assert_eq!(
            bindings_with_admission(&storage, true, &mut roots)
                .unwrap_err()
                .kind(),
            io::ErrorKind::Interrupted,
        );
        assert_eq!(
            bindings_with_root_resolver(&storage, true, &mut crate::root_cache::canonical_root)
                .unwrap_err()
                .kind(),
            io::ErrorKind::Interrupted,
        );
    }
    fixture_roots!(record_bind(root: &Path, artifact_key: &str) -> io::Result<()>);
    fixture_roots!(reap_retired() -> io::Result<()>);
    fn schedule(
        storage: PathBuf,
        admission: SubcLifecycleAdmission,
        generation: Arc<AtomicU64>,
        expected: u64,
    ) {
        super::schedule(
            StoragePaths::same(&storage),
            admission,
            generation,
            expected,
        );
    }
    #[test]
    fn cache_root_retirement_trash_is_on_the_cache_volume() {
        let temp = tempfile::tempdir().unwrap();
        let storage = temp.path().join("aft");
        let cache = temp.path().join("cache");
        fs::create_dir(&cache).unwrap();
        let mut config = crate::config::Config {
            storage_dir: Some(storage.clone()),
            cache_dir: Some(cache),
            ..Default::default()
        };
        assert!(crate::root_cache::prepare_cache_root_for_test(&mut config).is_none());
        let cache = crate::root_cache::cache_root(&config);
        let retired = cache.join("symbols/retired");
        fs::create_dir_all(&retired).unwrap();
        fs::write(retired.join("payload"), "cache").unwrap();
        retire_directory(&storage, &retired).unwrap();
        assert!(!retired.exists());
        assert!(cache.join("symbols/.retention-trash").is_dir());
        assert!(!storage.join("symbols").exists());
    }

    #[test]
    fn cache_root_prunes_linked_binding_using_durable_family_owner() {
        let temp = tempfile::tempdir().unwrap();
        let durable = temp.path().join("storage");
        let parent = temp.path().join("cache-volume");
        fs::create_dir(&parent).unwrap();
        let mut config = crate::config::Config {
            storage_dir: Some(durable.clone()),
            cache_dir: Some(parent),
            ..Default::default()
        };
        assert!(crate::root_cache::prepare_cache_root_for_test(&mut config).is_none());
        let paths = StoragePaths::for_config(&config);
        let checkout = temp.path().join("retired-worktree");
        let main = temp.path().join("main-checkout");
        fs::create_dir(&checkout).unwrap();
        fs::create_dir(&main).unwrap();
        let family = "0123456789abcdef";
        super::record_bind(&paths, &checkout, family).unwrap();
        let scope = crate::path_identity::project_scope_key(&checkout);
        let record = durable.join(format!("retention/roots/{scope}.json"));
        let mut binding: Binding = read_json(&record).unwrap();
        binding.linked_worktree = true;
        binding.last_bound_ms = 0;
        atomic_json(&record, &binding).unwrap();
        fs::remove_dir(&checkout).unwrap();
        crate::artifact_owner::write_synthetic_manifest_for_test(
            &durable,
            &main,
            family,
            &crate::path_identity::project_scope_key(&main),
            std::process::id(),
            crate::pins::now_ms(),
        );
        let family_cache = paths.artifacts.join("index").join(family);
        fs::create_dir_all(&family_cache).unwrap();
        let mut report = SweepReport {
            at_ms: crate::pins::now_ms(),
            ..Default::default()
        };
        super::prune_bindings(&paths, &|| false, &mut report).unwrap();
        assert_eq!(report.pruned_bindings, 1);
        assert!(
            !record.exists(),
            "retired checkout binding must not be kept by a family owned by the live main checkout"
        );
        assert!(family_cache.is_dir());
        assert!(durable
            .join("artifact-owners")
            .join(family)
            .join("owner.json")
            .is_file());
        assert!(!paths.artifacts.join("artifact-owners").exists());
    }

    #[test]
    fn cache_root_retention_cursors_are_isolated_by_artifact_identity() {
        let temp = tempfile::tempdir().unwrap();
        let durable = temp.path().join("storage");
        let first = StoragePaths::new(durable.clone(), temp.path().join("first"));
        let second = StoragePaths::new(durable, temp.path().join("second"));
        for paths in [&first, &second] {
            for key in ["0000000000000001", "0000000000000002"] {
                fs::create_dir_all(paths.artifacts.join("index").join(key)).unwrap();
            }
        }
        let batch = super::traversal_batch(
            &first,
            &[(first.artifacts.join("index"), false)],
            "roots",
            1,
        )
        .unwrap();
        assert_eq!(batch.entries[0].entry.file_name(), "0000000000000001");
        super::persist_cursor(&first, &batch.cursor_path, &batch.entries[0].after).unwrap();
        let next = super::traversal_batch(
            &second,
            &[(second.artifacts.join("index"), false)],
            "roots",
            1,
        )
        .unwrap();
        assert_ne!(batch.cursor_path, next.cursor_path);
        assert_eq!(
            next.entries[0].entry.file_name(),
            "0000000000000001",
            "another root's cursor must not skip this root's first entry"
        );
    }
    use super::*;

    fn expired_pending_bind(root: &Path) -> Arc<PendingBind> {
        let started = Instant::now() - Duration::from_secs(2);
        Arc::new(PendingBind {
            binding: Binding {
                root: fs::canonicalize(root).unwrap(),
                artifact_key: "0123456789abcdef".into(),
                last_bound_ms: crate::pins::now_ms(),
                volume: None,
                stable_volume: None,
                linked_worktree: false,
            },
            started,
            deadline: started + Duration::from_secs(1),
            result: Mutex::new(None),
        })
    }

    fn assert_publication_timeout(error: &io::Error, storage: &Path) {
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        let wait = error
            .get_ref()
            .unwrap()
            .downcast_ref::<BindLockError>()
            .unwrap();
        assert_eq!(wait.path, storage.join("retention/sweep.lock"));
        assert!(wait.publication);
    }

    fn assert_fresh_binding_only(storage: &Path, fresh: &Path, expired: &[&Path]) {
        let fresh = fs::canonicalize(fresh).unwrap();
        let key = crate::path_identity::project_scope_key(&fresh);
        let binding: Binding =
            read_json(&storage.join(format!("retention/roots/{key}.json"))).unwrap();
        assert_eq!(binding.root, fresh);
        assert_eq!(binding.artifact_key, "fedcba9876543210");
        for root in expired {
            let key = crate::path_identity::project_scope_key(&fs::canonicalize(root).unwrap());
            assert!(!storage.join(format!("retention/roots/{key}.json")).exists());
        }
    }

    #[test]
    fn record_bind_timed_out_waiter_leaves_pending_queue() {
        use std::sync::mpsc;
        let temp = tempfile::tempdir().unwrap();
        let expired_root = temp.path().join("expired");
        let fresh_root = temp.path().join("fresh");
        fs::create_dir(&expired_root).unwrap();
        fs::create_dir(&fresh_root).unwrap();
        let queue = bind_queue(temp.path()).unwrap();
        // Keep the caller behind an active round without holding sweep.lock.
        queue.state.lock().unwrap().active = true;
        let (enqueued, arrival) = mpsc::channel();
        let _hook = {
            let queue = queue.clone();
            observe_test_hook(
                temp.path(),
                Arc::new(move |step| {
                    if step == "bind-enqueued" {
                        enqueued
                            .send(queue.state.lock().unwrap().pending.last().unwrap().clone())
                            .unwrap();
                    }
                }),
            )
        };
        std::thread::scope(|scope| {
            let expired = scope.spawn(|| {
                record_bind_until(
                    temp.path(),
                    &expired_root,
                    "0123456789abcdef",
                    Instant::now() + Duration::from_millis(200),
                )
            });
            let member = arrival.recv_timeout(Duration::from_secs(5)).unwrap();
            let error = expired.join().unwrap().unwrap_err();
            assert_publication_timeout(&error, temp.path());
            let removed_before_fresh_bind = {
                let mut state = queue.state.lock().unwrap();
                let removed = state.pending.is_empty();
                state.active = false;
                removed
            };
            record_bind(temp.path(), &fresh_root, "fedcba9876543210")
                .expect("an expired waiter poisoned a fresh binding");
            assert!(removed_before_fresh_bind, "timed-out caller stayed pending");
            let result = member.result.lock().unwrap().clone().unwrap();
            assert_publication_timeout(&result.unwrap_err().into_io(), temp.path());
        });
        assert_fresh_binding_only(temp.path(), &fresh_root, &[&expired_root]);
    }

    #[test]
    fn record_bind_expired_unscheduled_waiter_does_not_poison_round() {
        let temp = tempfile::tempdir().unwrap();
        let expired_root = temp.path().join("expired");
        let fresh_root = temp.path().join("fresh");
        fs::create_dir(&expired_root).unwrap();
        fs::create_dir(&fresh_root).unwrap();
        let queue = bind_queue(temp.path()).unwrap();
        let expired = expired_pending_bind(&expired_root);
        queue.state.lock().unwrap().pending.push(expired.clone());
        record_bind(temp.path(), &fresh_root, "fedcba9876543210")
            .expect("an unscheduled expired waiter poisoned the round");
        let error = expired
            .result
            .lock()
            .unwrap()
            .clone()
            .unwrap()
            .unwrap_err()
            .into_io();
        assert_publication_timeout(&error, temp.path());
        let wait = error
            .get_ref()
            .unwrap()
            .downcast_ref::<BindLockError>()
            .unwrap();
        assert!(
            wait.waited >= Duration::from_secs(1),
            "timeout used the leader's clock"
        );
        assert_fresh_binding_only(temp.path(), &fresh_root, &[&expired_root]);
    }

    #[test]
    fn record_bind_expired_admission_members_do_not_poison_fresh_arrivals() {
        use std::sync::mpsc;
        let temp = tempfile::tempdir().unwrap();
        let leader_root = temp.path().join("leader");
        let expired_root = temp.path().join("expired");
        let fresh_root = temp.path().join("fresh");
        for root in [&leader_root, &expired_root, &fresh_root] {
            fs::create_dir(root).unwrap();
        }
        let queue = bind_queue(temp.path()).unwrap();
        let expired = expired_pending_bind(&expired_root);
        let deadline = Instant::now() + Duration::from_secs(2);
        let (admitted, admission) = mpsc::channel();
        let (enqueued, arrival) = mpsc::channel();
        let (resume, resumed) = mpsc::channel();
        let resumed = Mutex::new(resumed);
        let _hook = observe_test_hook(
            temp.path(),
            Arc::new(move |step| match step {
                "bind-round-admitted" => {
                    admitted.send(()).unwrap();
                    resumed
                        .lock()
                        .unwrap()
                        .recv_timeout(Duration::from_secs(5))
                        .unwrap();
                }
                "bind-enqueued" => {
                    enqueued.send(()).unwrap();
                }
                _ => {}
            }),
        );
        std::thread::scope(|scope| {
            let leader = scope.spawn(|| {
                record_bind_until(temp.path(), &leader_root, "0123456789abcdef", deadline)
            });
            admission.recv_timeout(Duration::from_secs(5)).unwrap();
            arrival.recv_timeout(Duration::from_secs(5)).unwrap();
            let fresh = scope.spawn(|| record_bind(temp.path(), &fresh_root, "fedcba9876543210"));
            arrival.recv_timeout(Duration::from_secs(5)).unwrap();
            queue.state.lock().unwrap().pending.push(expired.clone());
            // Admission has finished and the fresh caller is queued. Expire
            // only the old leader before freezing the publication snapshot.
            let timer = Condvar::new();
            let clock = Mutex::new(());
            drop(
                timer
                    .wait_timeout_while(
                        clock.lock().unwrap(),
                        deadline.saturating_duration_since(Instant::now()),
                        |_| Instant::now() < deadline,
                    )
                    .unwrap(),
            );
            assert!(Instant::now() >= deadline);
            resume.send(()).unwrap();
            let leader_error = leader.join().unwrap().unwrap_err();
            fresh
                .join()
                .unwrap()
                .expect("expired admission members poisoned a fresh arrival");
            assert_publication_timeout(&leader_error, temp.path());
        });
        let error = expired
            .result
            .lock()
            .unwrap()
            .clone()
            .unwrap()
            .unwrap_err()
            .into_io();
        assert_publication_timeout(&error, temp.path());
        assert!(
            error
                .get_ref()
                .unwrap()
                .downcast_ref::<BindLockError>()
                .unwrap()
                .waited
                >= Duration::from_secs(1)
        );
        assert_fresh_binding_only(temp.path(), &fresh_root, &[&leader_root, &expired_root]);
    }

    #[test]
    fn record_bind_parallel_queue_survives_slow_publish() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::{Arc, Barrier, Mutex};
        const BINDS: usize = 64;
        for delay in [Duration::ZERO, Duration::from_millis(100)] {
            let temp = tempfile::tempdir().unwrap();
            let ready = Arc::new(Barrier::new(BINDS));
            let measurements = Arc::new(Mutex::new(Vec::new()));
            let completion_waits = Arc::new(Mutex::new(Vec::new()));
            let acquisitions = Arc::new(AtomicUsize::new(0));
            let rounds = Arc::new(AtomicUsize::new(0));
            let round_count = rounds.clone();
            let _round_hook = observe_test_hook(
                temp.path(),
                Arc::new(move |step| {
                    if step == "bind-round-acquired" {
                        round_count.fetch_add(1, Ordering::SeqCst);
                        // Model one slow protected section, not one delay per root.
                        std::thread::sleep(delay);
                    }
                }),
            );
            let mut hooks = Vec::new();
            let mut roots = Vec::new();
            for index in 0..BINDS {
                let root = temp.path().join(format!("checkout-{index}"));
                fs::create_dir(&root).unwrap();
                let root = fs::canonicalize(root).unwrap();
                let phases = Arc::new(Mutex::new((Instant::now(), Instant::now())));
                let measurements = measurements.clone();
                hooks.push(observe_test_hook(
                    &root,
                    Arc::new(move |step| match step {
                        "bind-waiting" => phases.lock().unwrap().0 = Instant::now(),
                        "bind-acquired" => {
                            phases.lock().unwrap().1 = Instant::now();
                        }
                        "bind-released" => {
                            let (waiting, acquired) = *phases.lock().unwrap();
                            measurements
                                .lock()
                                .unwrap()
                                .push((acquired.duration_since(waiting), acquired.elapsed()));
                        }
                        _ => {}
                    }),
                ));
                roots.push(root);
            }
            let wall_started = Instant::now();
            let results = std::thread::scope(|scope| {
                let workers: Vec<_> = roots
                    .iter()
                    .map(|root| {
                        let ready = &ready;
                        let acquisitions = &acquisitions;
                        let completion_waits = &completion_waits;
                        let storage = temp.path();
                        scope.spawn(move || {
                            ready.wait();
                            crate::fs_lock::io_ledger::take();
                            let started = Instant::now();
                            let result = record_bind(
                                storage,
                                root,
                                &crate::path_identity::project_scope_key(root),
                            );
                            completion_waits.lock().unwrap().push(started.elapsed());
                            acquisitions.fetch_add(
                                crate::fs_lock::io_ledger::take().acquisitions as usize,
                                Ordering::SeqCst,
                            );
                            result
                        })
                    })
                    .collect();
                workers
                    .into_iter()
                    .map(|worker| worker.join().unwrap())
                    .collect::<Vec<_>>()
            });
            let mut measurements = measurements.lock().unwrap().clone();
            measurements.sort_by_key(|(wait, _)| *wait);
            eprintln!(
                "record_bind queue: binds={BINDS} delay={delay:?} rounds={} lock_acquisitions={} wall={:?} published={} wait[min/p50/max]={:?}/{:?}/{:?} max_binder_wait={:?} hold[max]={:?} errors={:?}",
                rounds.load(Ordering::SeqCst),
                acquisitions.load(Ordering::SeqCst),
                wall_started.elapsed(),
                measurements.len(),
                measurements.first().map(|m| m.0),
                measurements.get(measurements.len() / 2).map(|m| m.0),
                measurements.last().map(|m| m.0),
                completion_waits.lock().unwrap().iter().copied().max(),
                measurements.iter().map(|m| m.1).max(),
                results.iter().filter_map(|r| r.as_ref().err()).collect::<Vec<_>>()
            );
            assert!(results.iter().all(Result::is_ok), "queued binds failed");
            assert_eq!(measurements.len(), BINDS);
            for root in roots {
                let scope = crate::path_identity::project_scope_key(&root);
                let binding: Binding =
                    read_json(&temp.path().join(format!("retention/roots/{scope}.json"))).unwrap();
                assert_eq!(binding.root, root);
                assert_eq!(binding.artifact_key, scope);
            }
        }
    }

    #[test]
    fn record_bind_group_commit_coalesces_sixty_four_callers() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        const BINDS: usize = 64;
        #[cfg(target_os = "linux")]
        {
            const CHILD: &str = "AFT_BIND_LOW_FD_CHILD";
            if std::env::var_os(CHILD).is_some() {
                let mut limit = libc::rlimit {
                    rlim_cur: 0,
                    rlim_max: 0,
                };
                assert_eq!(
                    unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut limit) },
                    0
                );
                limit.rlim_cur = 64.min(limit.rlim_max);
                assert_eq!(unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &limit) }, 0);
            } else {
                let output = std::process::Command::new(std::env::current_exe().unwrap())
                    .args(["--exact", "storage_retention::storage_retention_tests::record_bind_group_commit_coalesces_sixty_four_callers", "--nocapture"])
                    .env(CHILD, "1").output().unwrap();
                assert!(
                    output.status.success(),
                    "64 binders exceeded the file-handle budget: {} {}",
                    String::from_utf8_lossy(&output.stdout),
                    String::from_utf8_lossy(&output.stderr)
                );
            }
        }
        let temp = tempfile::tempdir().unwrap();
        let directory = temp.path().join("retention/roots");
        crate::backup::create_private_durable_dir(&directory).unwrap();
        let roots: Vec<_> = (0..BINDS)
            .map(|index| {
                let root = temp.path().join(format!("checkout-{index}"));
                fs::create_dir(&root).unwrap();
                fs::canonicalize(root).unwrap()
            })
            .collect();
        let arrivals = Arc::new((Mutex::new(0_usize), Condvar::new()));
        let acquisitions = Arc::new(AtomicUsize::new(0));
        let directory_syncs = Arc::new(AtomicUsize::new(0));
        let file_syncs = Arc::new(AtomicUsize::new(0));
        let _hook = {
            let arrivals = arrivals.clone();
            let directory_syncs = directory_syncs.clone();
            let file_syncs = file_syncs.clone();
            let directory = directory.clone();
            observe_test_hook(
                temp.path(),
                Arc::new(move |step| match step {
                    "bind-enqueued" => {
                        *arrivals.0.lock().unwrap() += 1;
                        arrivals.1.notify_all();
                    }
                    "bind-round-collecting" => {
                        let count = arrivals.0.lock().unwrap();
                        let (count, timeout) = arrivals
                            .1
                            .wait_timeout_while(count, Duration::from_secs(20), |count| {
                                *count < BINDS
                            })
                            .unwrap();
                        assert!(!timeout.timed_out(), "only {count} callers enqueued");
                        crate::durability::take();
                    }
                    "bind-round-staged" => {
                        assert_eq!(
                            fs::read_dir(&directory)
                                .unwrap()
                                .filter(|entry| {
                                    entry
                                        .as_ref()
                                        .unwrap()
                                        .path()
                                        .extension()
                                        .is_some_and(|ext| ext == "json")
                                })
                                .count(),
                            0,
                            "a final record appeared before staging completed"
                        );
                    }
                    "bind-round-durable" => {
                        let events = crate::durability::take();
                        assert_eq!(
                            events
                                .iter()
                                .filter(|(kind, path)| {
                                    *kind == crate::durability::EventKind::FileSync
                                        && path.parent() == Some(directory.as_path())
                                })
                                .count(),
                            BINDS,
                            "round lost durable file syncs"
                        );
                        let last_file_sync = events
                            .iter()
                            .rposition(|(kind, path)| {
                                *kind == crate::durability::EventKind::FileSync
                                    && path.parent() == Some(directory.as_path())
                            })
                            .unwrap();
                        let first_replace = events
                            .iter()
                            .position(|(kind, path)| {
                                *kind == crate::durability::EventKind::AtomicReplace
                                    && path.parent() == Some(directory.as_path())
                            })
                            .unwrap();
                        assert!(
                            last_file_sync < first_replace,
                            "a rename preceded the round's file syncs"
                        );
                        #[cfg(unix)]
                        {
                            let last_replace = events
                                .iter()
                                .rposition(|(kind, path)| {
                                    *kind == crate::durability::EventKind::AtomicReplace
                                        && path.parent() == Some(directory.as_path())
                                })
                                .unwrap();
                            let directory_sync = events
                                .iter()
                                .position(|(kind, path)| {
                                    *kind == crate::durability::EventKind::DirectorySync
                                        && path == &directory
                                })
                                .unwrap();
                            assert!(last_replace < directory_sync);
                        }
                        for (kind, path) in events {
                            if kind == crate::durability::EventKind::DirectorySync
                                && path == directory
                            {
                                directory_syncs.fetch_add(1, Ordering::SeqCst);
                            }
                            if kind == crate::durability::EventKind::FileSync
                                && path.parent() == Some(directory.as_path())
                            {
                                file_syncs.fetch_add(1, Ordering::SeqCst);
                            }
                        }
                    }
                    _ => {}
                }),
            )
        };
        let ready = std::sync::Barrier::new(BINDS);
        std::thread::scope(|scope| {
            let workers: Vec<_> = roots
                .iter()
                .map(|root| {
                    let ready = &ready;
                    let acquisitions = &acquisitions;
                    let storage = temp.path();
                    scope.spawn(move || {
                        ready.wait();
                        crate::fs_lock::io_ledger::take();
                        let result = record_bind(
                            storage,
                            root,
                            &crate::path_identity::project_scope_key(root),
                        );
                        acquisitions.fetch_add(
                            crate::fs_lock::io_ledger::take().acquisitions as usize,
                            Ordering::SeqCst,
                        );
                        result
                    })
                })
                .collect();
            for worker in workers {
                worker.join().unwrap().unwrap();
            }
        });
        // The group acquires one cooperative pending marker in addition to
        // its single legacy-compatible sweep lock, never either lock per root.
        assert_eq!(
            acquisitions.load(Ordering::SeqCst),
            2,
            "binds were not coalesced"
        );
        assert_eq!(file_syncs.load(Ordering::SeqCst), BINDS);
        #[cfg(unix)]
        assert_eq!(
            directory_syncs.load(Ordering::SeqCst),
            1,
            "directory synced per binding"
        );
        for root in roots {
            let key = crate::path_identity::project_scope_key(&root);
            let path = directory.join(format!("{key}.json"));
            let binding: Binding = read_json(&path).unwrap();
            assert_eq!(binding.root, root);
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                assert_eq!(
                    fs::metadata(path).unwrap().permissions().mode() & 0o777,
                    0o600
                );
            }
        }
    }

    #[test]
    fn record_bind_cache_dir_concurrent_binds_take_one_retention_lock_per_round() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        const BINDS: usize = 16;
        let temp = tempfile::tempdir().unwrap();
        let parent = temp.path().join("cache-volume");
        fs::create_dir(&parent).unwrap();
        let _volume = crate::walk_boundary::simulated_volumes::simulate(&parent, u64::MAX - 17);
        let mut config = crate::config::Config {
            storage_dir: Some(temp.path().join("durable")),
            cache_dir: Some(parent),
            ..Default::default()
        };
        assert!(crate::root_cache::prepare_cache_root(&mut config).is_none());
        let paths = StoragePaths::for_config(&config);
        assert_ne!(paths.durable, paths.artifacts);
        let directory = paths.durable.join("retention/roots");
        let observations = paths.durable.join("retention/unknown");
        fs::create_dir_all(&observations).unwrap();
        let roots: Vec<_> = (0..BINDS)
            .map(|index| {
                let root = temp.path().join(format!("checkout-{index}"));
                fs::create_dir(&root).unwrap();
                let root = fs::canonicalize(root).unwrap();
                let key = crate::path_identity::project_scope_key(&root);
                fs::write(observations.join(format!("{key}.json")), b"{}").unwrap();
                (root, key)
            })
            .collect();
        let arrivals = Arc::new((Mutex::new(0_usize), Condvar::new()));
        let rounds = Arc::new(AtomicUsize::new(0));
        let acquisitions = AtomicUsize::new(0);
        let _hook = {
            let arrivals = arrivals.clone();
            let rounds = rounds.clone();
            observe_test_hook(
                &paths.durable,
                Arc::new(move |step| match step {
                    "bind-enqueued" => {
                        *arrivals.0.lock().unwrap() += 1;
                        arrivals.1.notify_all();
                    }
                    "bind-round-collecting" => {
                        let count = arrivals.0.lock().unwrap();
                        let (count, timeout) = arrivals
                            .1
                            .wait_timeout_while(count, Duration::from_secs(20), |count| {
                                *count < BINDS
                            })
                            .unwrap();
                        assert!(!timeout.timed_out(), "only {count} callers enqueued");
                    }
                    "bind-round-acquired" => {
                        rounds.fetch_add(1, Ordering::SeqCst);
                    }
                    _ => {}
                }),
            )
        };
        std::thread::scope(|scope| {
            let workers: Vec<_> = roots
                .iter()
                .map(|(root, key)| {
                    let paths = &paths;
                    let acquisitions = &acquisitions;
                    scope.spawn(move || {
                        crate::fs_lock::io_ledger::take();
                        let result = super::record_bind(paths, root, key);
                        acquisitions.fetch_add(
                            crate::fs_lock::io_ledger::take().acquisitions as usize,
                            Ordering::SeqCst,
                        );
                        result
                    })
                })
                .collect();
            for worker in workers {
                worker.join().unwrap().unwrap();
            }
        });
        assert_eq!(
            rounds.load(Ordering::SeqCst),
            1,
            "binds did not share a round"
        );
        // Each bind round takes one bind-pending marker and one durable
        // `retention/sweep.lock`, however many callers it carries. With the
        // cache root on its own volume, binds must take no further retention
        // lock of their own.
        assert_eq!(
            acquisitions.load(Ordering::SeqCst),
            2 * rounds.load(Ordering::SeqCst),
            "split-cache binds acquired retention locks outside the group round"
        );
        for (root, key) in roots {
            let binding: Binding = read_json(&directory.join(format!("{key}.json"))).unwrap();
            assert_eq!(binding.root, root);
            assert_eq!(binding.artifact_key, key);
            assert!(!observations.join(format!("{key}.json")).exists());
        }
        assert!(!paths.artifacts.join("retention").exists());
    }

    #[test]
    fn record_bind_failed_group_retry_is_idempotent_and_extra_records_expire() {
        use std::sync::atomic::{AtomicBool, Ordering};
        let temp = tempfile::tempdir().unwrap();
        let directory = temp.path().join("retention/roots");
        let roots: Vec<_> = (0..2)
            .map(|index| {
                let root = temp.path().join(format!("checkout-{index}"));
                fs::create_dir(&root).unwrap();
                let root = fs::canonicalize(root).unwrap();
                cache(
                    temp.path(),
                    "views",
                    &crate::path_identity::project_scope_key(&root),
                );
                root
            })
            .collect();
        let arrivals = Arc::new((Mutex::new(0), Condvar::new()));
        let inject = Arc::new(AtomicBool::new(true));
        let _hook = {
            let arrivals = arrivals.clone();
            let directory = directory.clone();
            let inject = inject.clone();
            observe_test_hook(
                temp.path(),
                Arc::new(move |step| match step {
                    "bind-enqueued" => {
                        *arrivals.0.lock().unwrap() += 1;
                        arrivals.1.notify_all();
                    }
                    "bind-round-collecting" if inject.load(Ordering::SeqCst) => {
                        let (count, timeout) = arrivals
                            .1
                            .wait_timeout_while(
                                arrivals.0.lock().unwrap(),
                                Duration::from_secs(10),
                                |count| *count < 2,
                            )
                            .unwrap();
                        assert!(!timeout.timed_out(), "only {count} binds enqueued");
                    }
                    "bind-round-staged" if inject.swap(false, Ordering::SeqCst) => {
                        crate::durability::FAIL_DIRECTORY_SYNC.with(|slot| {
                            *slot.borrow_mut() = Some(directory.clone());
                        });
                    }
                    _ => {}
                }),
            )
        };
        let errors = std::thread::scope(|scope| {
            let workers: Vec<_> = roots
                .iter()
                .map(|root| {
                    let storage = temp.path();
                    scope.spawn(move || {
                        record_bind(
                            storage,
                            root,
                            &crate::path_identity::project_scope_key(root),
                        )
                    })
                })
                .collect();
            workers
                .into_iter()
                .map(|worker| worker.join().unwrap().unwrap_err())
                .collect::<Vec<_>>()
        });
        assert_eq!(errors[0].kind(), errors[1].kind());
        assert_eq!(errors[0].to_string(), errors[1].to_string());
        assert!(errors[0]
            .to_string()
            .contains("injected directory sync failure"));
        assert_eq!(fs::read_dir(&directory).unwrap().count(), roots.len());
        // Derive keys while the checkouts exist. After removal the key falls
        // back to a lexical form, which on Windows differs from the verbatim
        // canonical path the binding was recorded under.
        let keys: Vec<String> = roots
            .iter()
            .map(|root| crate::path_identity::project_scope_key(root))
            .collect();
        for (index, (root, key)) in roots.iter().zip(&keys).enumerate() {
            let path = directory.join(format!("{key}.json"));
            let before: Binding = read_json(&path).unwrap();
            if index == 0 {
                record_bind(temp.path(), root, key).unwrap();
            }
            let after: Binding = read_json(&path).unwrap();
            assert_eq!(before.root, after.root);
            assert_eq!(before.artifact_key, after.artifact_key);
            assert!(after.last_bound_ms >= before.last_bound_ms);
            fs::remove_dir(root).unwrap();
        }
        assert_eq!(fs::read_dir(&directory).unwrap().count(), roots.len());
        assert_eq!(run_pass_entry_bound(temp.path()).removed_roots, 0);
        for key in &keys {
            let path = directory.join(format!("{key}.json"));
            let mut binding: Binding = read_json(&path).unwrap();
            binding.last_bound_ms = 0;
            atomic_json(&path, &binding).unwrap();
        }
        let report = run_pass_entry_bound(temp.path());
        assert_eq!(report.removed_roots, 2, "{report:?}");
        run_pass_entry_bound(temp.path());
        assert_eq!(fs::read_dir(&directory).unwrap().count(), 0);
    }

    #[test]
    fn record_bind_arriving_during_publication_uses_next_round() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::mpsc;
        let temp = tempfile::tempdir().unwrap();
        let first = temp.path().join("first");
        let second = temp.path().join("second");
        fs::create_dir(&first).unwrap();
        fs::create_dir(&second).unwrap();
        let key = crate::path_identity::project_scope_key(&fs::canonicalize(&second).unwrap());
        let second_record = temp.path().join(format!("retention/roots/{key}.json"));
        let (admitted, admission) = mpsc::channel();
        let (enqueued, arrival) = mpsc::channel();
        let (resume, resumed) = mpsc::channel();
        let resumed = Mutex::new(resumed);
        let rounds = Arc::new(AtomicUsize::new(0));
        let _hook = {
            let rounds = rounds.clone();
            observe_test_hook(
                temp.path(),
                Arc::new(move |step| match step {
                    "bind-round-acquired" => {
                        if rounds.fetch_add(1, Ordering::SeqCst) == 0 {
                            admitted.send(()).unwrap();
                            resumed
                                .lock()
                                .unwrap()
                                .recv_timeout(Duration::from_secs(10))
                                .unwrap();
                        }
                    }
                    "bind-enqueued" => {
                        enqueued.send(()).unwrap();
                    }
                    "bind-round-durable" if rounds.load(Ordering::SeqCst) == 1 => {
                        assert!(
                            !second_record.exists(),
                            "late bind entered a publishing snapshot"
                        );
                    }
                    _ => {}
                }),
            )
        };
        std::thread::scope(|scope| {
            let one = scope.spawn(|| record_bind(temp.path(), &first, "0123456789abcdef"));
            admission.recv_timeout(Duration::from_secs(10)).unwrap();
            arrival.recv_timeout(Duration::from_secs(10)).unwrap();
            let two = scope.spawn(|| record_bind(temp.path(), &second, "fedcba9876543210"));
            arrival.recv_timeout(Duration::from_secs(10)).unwrap();
            resume.send(()).unwrap();
            one.join().unwrap().unwrap();
            two.join().unwrap().unwrap();
        });
        assert_eq!(rounds.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn record_bind_lock_chain_preserves_one_deadline() {
        let temp = tempfile::tempdir().unwrap();
        fs::create_dir_all(temp.path().join("retention")).unwrap();
        let pending_path = temp.path().join("retention/bind-pending.lock");
        let sweep_path = temp.path().join("retention/sweep.lock");
        let _sweep = crate::fs_lock::try_acquire(&sweep_path, Duration::ZERO).unwrap();
        let clock = crate::fs_lock::retry_clock::Guard::install(Duration::from_millis(40));
        let started = crate::fs_lock::acquisition_now();
        let deadline = started + Duration::from_millis(100);
        let _pending = bind_lock_until(&pending_path, deadline, started).unwrap();
        let error = match bind_lock_until(&sweep_path, deadline, started) {
            Ok(_) => panic!("admitted through an occupied sweep lock"),
            Err(error) => error,
        };
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        let wait = error
            .get_ref()
            .unwrap()
            .downcast_ref::<BindLockError>()
            .unwrap();
        assert_eq!(wait.path, sweep_path);
        let (elapsed, sleeps, _) = clock.observations();
        assert_eq!(
            elapsed,
            Duration::from_millis(100),
            "lock chain renewed its budget"
        );
        assert_eq!(sleeps, vec![Duration::from_millis(60)]);
    }

    #[test]
    fn record_bind_waits_for_sweep_admission() {
        use std::sync::{mpsc, Arc, Mutex};
        let (temp, _, _, _) = due_cache_fixture();
        let root = temp.path().join("incoming-checkout");
        fs::create_dir(&root).unwrap();
        let root = fs::canonicalize(root).unwrap();
        let (admitted, admission) = mpsc::channel();
        let completed = admitted.clone();
        let (waiting, waiter) = mpsc::channel();
        let waiter = Mutex::new(waiter);
        let held = Arc::new(Mutex::new(None::<Instant>));
        let held_hook = held.clone();
        let _sweep_hook = observe_test_hook(
            temp.path(),
            Arc::new(move |step| match step {
                "due-admitted" => {
                    *held_hook.lock().unwrap() = Some(Instant::now());
                    admitted.send(Some(())).unwrap();
                    waiter
                        .lock()
                        .unwrap()
                        .recv_timeout(Duration::from_secs(60))
                        .unwrap();
                }
                "due-released" => {
                    eprintln!(
                        "sweep admission holder duration: {:?}",
                        held_hook.lock().unwrap().unwrap().elapsed()
                    );
                }
                _ => {}
            }),
        );
        let bind_started = Mutex::new(Instant::now());
        let _bind_hook = observe_test_hook(
            &root,
            Arc::new(move |step| match step {
                "bind-waiting" => {
                    *bind_started.lock().unwrap() = Instant::now();
                    waiting.send(()).unwrap();
                }
                "bind-acquired" => eprintln!(
                    "bind waiting on sweep admission (including scheduling): {:?}",
                    bind_started.lock().unwrap().elapsed()
                ),
                _ => {}
            }),
        );
        std::thread::scope(|scope| {
            let sweep = scope.spawn(|| {
                let report = run_pass_entry_bound(temp.path());
                eprintln!("sweep contention report: {report:?}");
                let _ = completed.send(None);
                report
            });
            assert_eq!(
                admission.recv_timeout(Duration::from_secs(60)).unwrap(),
                Some(())
            );
            record_bind(temp.path(), &root, "fedcba9876543210").unwrap();
            let report = sweep.join().unwrap();
            assert_eq!(report.removed_roots, 1, "{report:?}");
            eprintln!("sweep phases: {:?}", report.timings_ms);
        });
        let scope = crate::path_identity::project_scope_key(&root);
        assert!(temp
            .path()
            .join(format!("retention/roots/{scope}.json"))
            .is_file());
    }

    #[test]
    fn legacy_path_spelling_never_proves_volume_continuity() {
        let temp = tempfile::tempdir().unwrap();
        let binding = Binding {
            root: temp.path().join("cortexkit/alfonso/worktrees/absent"),
            artifact_key: "0123456789abcdef".into(),
            last_bound_ms: 0,
            volume: None,
            stable_volume: None,
            linked_worktree: false,
        };
        let mounted = mount_matches_with(&binding, &|_| None, &|_| Some(42), Some("boot"), false);
        assert_eq!(
            binding_eligibility_with_mount(&binding, ROOT_GRACE_MS + 1, mounted),
            RootEligibility::MountMismatch
        );
    }

    #[test]
    fn legacy_nonboot_anchor_or_device_without_recorded_identity_is_retained() {
        let temp = tempfile::tempdir().unwrap();
        for (anchor, boot_volume) in [
            ("/", false),
            ("/Volumes/external", false),
            ("/Volumes/external", true),
        ] {
            let binding = Binding {
                root: temp.path().join("absent"),
                artifact_key: "0123456789abcdef".into(),
                last_bound_ms: 0,
                volume: Some((PathBuf::from(anchor), 42)),
                stable_volume: None,
                linked_worktree: false,
            };
            let mounted = mount_matches_with(
                &binding,
                &|_| None,
                &|_| Some(42),
                Some("boot"),
                boot_volume,
            );
            assert_eq!(
                binding_eligibility_with_mount(&binding, ROOT_GRACE_MS + 1, mounted),
                RootEligibility::MountMismatch,
                "anchor={anchor}, boot_volume={boot_volume}"
            );
        }
    }

    #[test]
    fn legacy_boot_anchor_proves_continuity_despite_a_changed_device_number() {
        let temp = tempfile::tempdir().unwrap();
        let binding = Binding {
            root: temp.path().join("absent"),
            artifact_key: "0123456789abcdef".into(),
            last_bound_ms: 0,
            volume: Some((PathBuf::from("/"), 42)),
            stable_volume: None,
            linked_worktree: false,
        };
        let mounted =
            mount_matches_with(&binding, &|_| None, &|_| Some(99), Some("new-boot"), true);
        assert_eq!(
            binding_eligibility_with_mount(&binding, ROOT_GRACE_MS + 1, mounted),
            RootEligibility::Due
        );
    }

    #[test]
    fn absent_root_git_uncertainty_is_not_volume_evidence() {
        let (temp, root, scope) = fixture();
        let _uuid = uuid_backed_binding(temp.path(), &scope);
        fs::remove_dir(&root).unwrap();
        assert!(matches!(
            crate::git_probe::toplevel(&root),
            Err(crate::git_probe::ProbeError::Failed {
                kind: crate::git_probe::FailureKind::Other,
                ..
            })
        ));
        let mut binding: Binding =
            read_json(&temp.path().join(format!("retention/roots/{scope}.json"))).unwrap();
        assert_eq!(
            binding_eligibility(&binding, ROOT_GRACE_MS + 1),
            RootEligibility::Due
        );
        binding.volume = None;
        binding.stable_volume = None;
        assert_eq!(
            binding_eligibility(&binding, ROOT_GRACE_MS + 1),
            RootEligibility::MountMismatch
        );
    }

    #[test]
    fn retention_observation_clears_only_after_last_cache_plane() {
        let temp = tempfile::tempdir().unwrap();
        let key = "0123456789abcdef";
        for domain in ["symbols", "semantic"] {
            let path = cache(temp.path(), domain, key);
            filetime::set_file_mtime(
                path.join("payload.bin"),
                filetime::FileTime::from_unix_time(0, 0),
            )
            .unwrap();
        }
        atomic_json(
            &temp.path().join(format!("retention/unknown/{key}.json")),
            &0_u64,
        )
        .unwrap();
        assert_eq!(
            run_pass_entry_bound(temp.path()).removed_roots,
            2,
            "removing one plane reset the shared grace for the other plane"
        );
        assert!(!temp
            .path()
            .join(format!("retention/unknown/{key}.json"))
            .exists());
    }

    #[cfg(unix)]
    #[test]
    fn retention_private_permissions_survive_restrictive_umask() {
        use std::os::unix::fs::PermissionsExt;
        const CHILD_PATH: &str = "AFT_RETENTION_PRIVATE_UMASK_PATH";
        if let Some(path) = std::env::var_os(CHILD_PATH) {
            let path = PathBuf::from(path);
            unsafe {
                libc::umask(0o777);
            }
            atomic_json(&path, &serde_json::json!({"bound": true})).unwrap();
            assert_eq!(
                fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
            assert_eq!(
                read_json::<serde_json::Value>(&path).unwrap()["bound"],
                true
            );
            return;
        }
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("private.json");
        let output = std::process::Command::new(std::env::current_exe().unwrap()).args(["--exact", "storage_retention::storage_retention_tests::retention_private_permissions_survive_restrictive_umask"]).env(CHILD_PATH, &path).output().unwrap();
        assert!(
            output.status.success(),
            "restrictive-umask writer failed: {} {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(
            fs::metadata(path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    #[test]
    fn retention_worker_panic_releases_running_state() {
        let temp = tempfile::tempdir().unwrap();
        let storage = temp.path().to_path_buf();
        states()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(
                storage.clone(),
                State {
                    running: true,
                    last_run: Some(Instant::now()),
                    ..State::default()
                },
            );
        assert!(
            std::panic::catch_unwind(|| scheduled_run(&storage, || panic!(
                "injected retention panic"
            )))
            .is_err()
        );
        let states = states()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert!(!states[&storage].running, "panic stranded the running flag");
        assert!(
            states[&storage].last_run.is_none(),
            "failed pass must permit rescheduling"
        );
    }

    #[test]
    fn retention_record_is_private_and_durable() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("binding.json");
        crate::durability::take();
        atomic_json(&path, &serde_json::json!({"bound": true})).unwrap();
        let events = crate::durability::take();
        assert!(events
            .iter()
            .any(|(kind, _)| *kind == crate::durability::EventKind::FileSync));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
            assert_eq!(
                events.last(),
                Some(&(
                    crate::durability::EventKind::DirectorySync,
                    dir.path().to_path_buf()
                ))
            );
        }
    }

    /// A budget no test runner exhausts. Tests that count entries per pass
    /// check the entry bound, not the runner's speed; the time bound has its
    /// own tests with an injected clock or a zero budget.
    const NO_TIME_BOUND: Duration = Duration::from_secs(24 * 60 * 60);

    fn run_pass_entry_bound(storage: &Path) -> SweepReport {
        run_pass_with_budget(storage, &|| false, &Instant::now, NO_TIME_BOUND)
    }

    fn fixture() -> (tempfile::TempDir, PathBuf, String) {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("checkout");
        fs::create_dir_all(&root).unwrap();
        let root = fs::canonicalize(root).unwrap();
        let scope = crate::path_identity::project_scope_key(&root);
        record_bind(temp.path(), &root, &scope).unwrap();
        let record = temp.path().join(format!("retention/roots/{scope}.json"));
        let mut binding: Binding = read_json(&record).unwrap();
        binding.last_bound_ms = 0;
        atomic_json(&record, &binding).unwrap();
        (temp, root, scope)
    }

    fn cache(storage: &Path, domain: &str, key: &str) -> PathBuf {
        let path = storage.join(domain).join(key);
        fs::create_dir_all(&path).unwrap();
        fs::write(path.join("payload.bin"), b"rebuildable").unwrap();
        path
    }

    struct TestVolumeUuidGuard(PathBuf);

    impl Drop for TestVolumeUuidGuard {
        fn drop(&mut self) {
            test_volume_uuids()
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .remove(&self.0);
        }
    }

    // Give the surviving parent a UUID even on runners without filesystem UUID
    // discovery. Disconnection fixtures must invalidate this identity as well
    // as their device number, or they still describe the mounted parent volume.
    fn uuid_backed_binding(storage: &Path, scope: &str) -> TestVolumeUuidGuard {
        let record = storage.join(format!("retention/roots/{scope}.json"));
        let mut binding: Binding = read_json(&record).unwrap();
        let parent = binding.root.parent().unwrap().to_path_buf();
        assert!(parent.is_dir());
        let uuid = format!("fixture-volume-{scope}");
        test_volume_uuids()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(parent.clone(), uuid.clone());
        let guard = TestVolumeUuidGuard(parent);
        binding.stable_volume = Some(VolumeIdentity {
            uuid: Some(uuid),
            boot_id: boot_id(),
        });
        atomic_json(&record, &binding).unwrap();
        assert!(
            mount_matches(&binding),
            "fixture parent must expose its recorded UUID"
        );
        guard
    }

    fn disconnect_uuid_backed_binding(storage: &Path, scope: &str, anchor: &Path) -> Binding {
        let record = storage.join(format!("retention/roots/{scope}.json"));
        let mut binding: Binding = read_json(&record).unwrap();
        binding.volume = Some((anchor.to_path_buf(), 42));
        // The removed volume's UUID is absent at both the unreachable anchor
        // and the surviving parent, which belongs to the fixture's mounted volume.
        binding.stable_volume.as_mut().unwrap().uuid = Some(format!("disconnected-volume-{scope}"));
        atomic_json(&record, &binding).unwrap();
        binding
    }

    #[test]
    fn traversal_resumes_after_restart() {
        let temp = tempfile::tempdir().unwrap();
        for i in 0..130 {
            cache(temp.path(), "inspect", &format!("{i:016x}"));
        }
        let first = run_pass_entry_bound(temp.path());
        assert_eq!(first.examined, PASS_ENTRIES, "{first:?}");
        states().lock().unwrap().remove(temp.path());
        let second = run_pass_entry_bound(temp.path());
        assert_eq!(second.examined, PASS_ENTRIES, "{second:?}");
        assert_eq!(
            fs::read_dir(temp.path().join("retention/unknown"))
                .unwrap()
                .count(),
            128
        );
    }

    #[test]
    fn traversal_preserves_unprocessed_batch_tail() {
        let temp = tempfile::tempdir().unwrap();
        for i in 0..10 {
            cache(temp.path(), "inspect", &format!("{i:016x}"));
        }
        let calls = std::sync::atomic::AtomicUsize::new(0);
        let now = Instant::now();
        let first = run_pass_with_clock(temp.path(), &|| false, &|| {
            if calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0 {
                now
            } else {
                now + PASS_BUDGET * 2
            }
        });
        assert_eq!(first.examined, 1, "{first:?}");
        let second = run_pass_entry_bound(temp.path());
        assert_eq!(second.examined, 9, "{second:?}");
        let cursor: TraversalCursor =
            read_json(&temp.path().join("retention/cursors/roots.json")).unwrap();
        assert!(
            cursor.last_processed.is_none(),
            "completed lap did not reset"
        );
    }

    #[test]
    fn family_traversal_resumes_after_restart_for_both_layouts() {
        let temp = tempfile::tempdir().unwrap();
        for (domain, kind) in [("blobs", "families-v1"), ("blobs/v2", "families-v2")] {
            for i in 0..3 {
                fs::create_dir_all(temp.path().join(domain).join(format!("{i:016x}"))).unwrap();
            }
            let mut visited = Vec::new();
            for _ in 0..2 {
                visit_one_family(temp.path(), domain, kind, &|| false, |entry| {
                    visited.push(entry.file_name());
                    Ok(())
                })
                .unwrap();
                states().lock().unwrap().remove(temp.path());
            }
            assert_eq!(
                visited,
                ["0000000000000000", "0000000000000001"].map(std::ffi::OsString::from)
            );
        }
    }

    #[test]
    fn stable_volume_change_with_parent_continuity_is_eligible() {
        let (temp, root, scope) = fixture();
        let mut binding: Binding =
            read_json(&temp.path().join(format!("retention/roots/{scope}.json"))).unwrap();
        binding.volume = Some((temp.path().join("old-anchor"), 10));
        binding.stable_volume = Some(VolumeIdentity {
            uuid: Some("original-volume".to_owned()),
            boot_id: Some("old-boot".to_owned()),
        });
        fs::remove_dir(&root).unwrap();
        let mounted = mount_matches_with(
            &binding,
            &|path| {
                Some(
                    if Some(path) == root.parent() {
                        "original-volume"
                    } else {
                        "replacement-volume"
                    }
                    .to_owned(),
                )
            },
            &|_| Some(11),
            Some("new-boot"),
            false,
        );
        assert_eq!(
            binding_eligibility_with_mount(&binding, ROOT_GRACE_MS, mounted),
            RootEligibility::Due
        );
    }

    #[test]
    fn stable_volume_replacement_is_blocked_even_if_device_is_reused() {
        let (temp, root, scope) = fixture();
        let mut binding: Binding =
            read_json(&temp.path().join(format!("retention/roots/{scope}.json"))).unwrap();
        binding.volume = Some((temp.path().to_path_buf(), 10));
        binding.stable_volume = Some(VolumeIdentity {
            uuid: Some("original-volume".to_owned()),
            boot_id: Some("old-boot".to_owned()),
        });
        fs::remove_dir(&root).unwrap();
        let mounted = mount_matches_with(
            &binding,
            &|_| Some("replacement-volume".to_owned()),
            &|_| Some(10),
            Some("new-boot"),
            false,
        );
        assert_eq!(
            binding_eligibility_with_mount(&binding, ROOT_GRACE_MS, mounted),
            RootEligibility::MountMismatch
        );
    }

    #[test]
    fn device_fallback_requires_same_boot_and_device() {
        let (temp, _root, scope) = fixture();
        let mut binding: Binding =
            read_json(&temp.path().join(format!("retention/roots/{scope}.json"))).unwrap();
        binding.volume = Some((temp.path().to_path_buf(), 10));
        binding.stable_volume = Some(VolumeIdentity {
            uuid: None,
            boot_id: Some("same-boot".to_owned()),
        });
        assert!(mount_matches_with(
            &binding,
            &|_| None,
            &|_| Some(10),
            Some("same-boot"),
            false
        ));
        assert!(!mount_matches_with(
            &binding,
            &|_| None,
            &|_| Some(10),
            Some("new-boot"),
            false
        ));
        assert!(!mount_matches_with(
            &binding,
            &|_| None,
            &|_| Some(11),
            Some("same-boot"),
            false
        ));
    }

    #[test]
    fn sweep_reports_examined_and_skip_reasons_per_domain() {
        let temp = tempfile::tempdir().unwrap();
        for (i, live, mismatch) in [(0, false, false), (1, true, false), (2, false, true)] {
            let root = temp.path().join(format!("checkout-{i}"));
            fs::create_dir_all(&root).unwrap();
            let scope = crate::path_identity::project_scope_key(&root);
            record_bind(temp.path(), &root, &scope).unwrap();
            let record = temp.path().join(format!("retention/roots/{scope}.json"));
            let mut binding: Binding = read_json(&record).unwrap();
            binding.last_bound_ms = 0;
            if mismatch {
                binding.volume = Some((temp.path().join("Volumes/X"), u64::MAX));
                binding.stable_volume = None;
            }
            atomic_json(&record, &binding).unwrap();
            cache(temp.path(), "inspect", &scope);
            if !live {
                fs::remove_dir(root).unwrap();
            }
        }
        cache(temp.path(), "inspect", "0000000000000000");
        let report = run_pass_entry_bound(temp.path());
        let domain = &report.domains["inspect"];
        assert_eq!(
            (
                domain.examined,
                domain.skipped_live,
                domain.skipped_recent,
                domain.skipped_mount_mismatch,
                domain.removed_roots
            ),
            (4, 1, 1, 1, 1),
            "{report:?}"
        );
        assert_eq!(domain.removed_bytes, 0);
        assert_eq!(domain.unmeasured_removed_roots, 1);
        let log = serde_json::to_value(&report).unwrap();
        assert_eq!(log["domains"]["inspect"]["skipped_mount_mismatch"], 1);
    }

    #[test]
    fn unproven_mount_change_is_blocked_and_counted() {
        let (temp, root, scope) = fixture();
        let record = temp.path().join(format!("retention/roots/{scope}.json"));
        let mut binding: Binding = read_json(&record).unwrap();
        binding.volume = Some((temp.path().join("Volumes/X"), u64::MAX));
        binding.stable_volume = None;
        atomic_json(&record, &binding).unwrap();
        fs::remove_dir(root).unwrap();
        cache(temp.path(), "inspect", &scope);
        let report = run_pass(temp.path(), &|| false);
        assert_eq!(report.skipped_mount_mismatch, 1, "{report:?}");
        assert_eq!(report.domains["inspect"].skipped_mount_mismatch, 1);
        assert_eq!(report.skipped_recent, 0, "{report:?}");
        assert_eq!(report.removed_roots, 0);
        assert!(temp.path().join("inspect").join(scope).exists());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_legacy_boot_device_change_is_eligible() {
        let (temp, root, scope) = fixture();
        let record = temp.path().join(format!("retention/roots/{scope}.json"));
        let mut binding: Binding = read_json(&record).unwrap();
        binding.volume = Some((PathBuf::from("/"), device(Path::new("/")).unwrap() + 1));
        binding.stable_volume = None;
        atomic_json(&record, &binding).unwrap();
        fs::remove_dir(root).unwrap();
        cache(temp.path(), "inspect", &scope);
        let report = run_pass(temp.path(), &|| false);
        assert_eq!(report.removed_roots, 1, "{report:?}");
        assert!(
            missing_root_due(temp.path(), &binding.root, 0),
            "{report:?}"
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_volume_identity_follows_data_volume_firmlinks() {
        let temp = tempfile::tempdir().unwrap();
        let anchor = volume_anchor(temp.path()).unwrap().0;
        assert_eq!(anchor, Path::new("/System/Volumes/Data"));
        let uuid = volume_uuid(temp.path()).expect("APFS UUID unavailable");
        assert_eq!(volume_uuid(&anchor), Some(uuid.clone()));
        assert_eq!(volume_anchor(Path::new("/Users")).unwrap().0, anchor);
        assert_eq!(volume_uuid(Path::new("/Users")), Some(uuid));
        assert!(boot_id().is_some());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_stable_uuid_survives_device_change() {
        let (temp, root, scope) = fixture();
        let record = temp.path().join(format!("retention/roots/{scope}.json"));
        let mut binding: Binding = read_json(&record).unwrap();
        assert!(binding.stable_volume.as_ref().unwrap().uuid.is_some());
        binding.volume.as_mut().unwrap().1 += 1;
        atomic_json(&record, &binding).unwrap();
        fs::remove_dir(root).unwrap();
        cache(temp.path(), "inspect", &scope);
        let report = run_pass(temp.path(), &|| false);
        assert_eq!(report.removed_roots, 1, "{report:?}");
        assert_eq!(report.skipped_mount_mismatch, 0);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_unproven_external_device_change_is_blocked() {
        unproven_mount_change_is_blocked_and_counted();
    }

    #[test]
    fn missing_root_retention_finds_the_identity_recorded_while_present() {
        let (temp, root, scope) = fixture();
        fs::remove_dir(&root).unwrap();
        let missing = crate::root_cache::canonical_root(&root);
        assert_eq!(missing_root_scope_key(&missing), scope);
        assert!(
            missing_root_due(temp.path(), &root, 0),
            "old missing root lost its durable binding: root={root:?} missing={missing:?} scope={scope}"
        );
    }

    #[test]
    fn startup_grace_does_not_admit_retention_or_blob_sweeps() {
        let temp = tempfile::tempdir().unwrap();
        let now = Instant::now();
        assert!(!startup_sweeps_ready_at(temp.path(), now));
        assert!(!startup_sweeps_ready_at(
            temp.path(),
            now + STARTUP_GRACE - Duration::from_nanos(1)
        ));
        let ctx = crate::context::AppContext::new(
            Box::new(crate::parser::TreeSitterProvider::new()),
            crate::config::Config {
                storage_dir: Some(temp.path().to_path_buf()),
                ..Default::default()
            },
        );
        schedule(
            temp.path().to_path_buf(),
            ctx.subc_lifecycle_admission(),
            ctx.configure_generation_flag(),
            ctx.configure_generation(),
        );
        // Scheduling records admission synchronously, before spawning a worker.
        // Counting that record checks the actual scheduling lane without racing
        // the background worker or sleeping through the startup grace.
        let admitted = states()
            .lock()
            .unwrap()
            .get(temp.path())
            .unwrap()
            .last_run
            .iter()
            .count();
        assert_eq!(admitted, 0, "retention started during startup warm-up");
        assert!(startup_sweeps_ready_at(temp.path(), now + STARTUP_GRACE));
    }

    // Gate the maintenance worker at a named phase, without making the request
    // thread depend on scheduler speed or an artificial sleep.
    fn paused_pass(
        storage: &Path,
        cache: &Path,
        phase: &'static str,
    ) -> (
        std::thread::JoinHandle<SweepReport>,
        std::sync::mpsc::Sender<()>,
    ) {
        paused_pass_with_budget(storage, cache, phase, PASS_BUDGET)
    }

    fn paused_pass_with_budget(
        storage: &Path,
        cache: &Path,
        phase: &'static str,
        budget: Duration,
    ) -> (
        std::thread::JoinHandle<SweepReport>,
        std::sync::mpsc::Sender<()>,
    ) {
        let (entered_tx, entered_rx) = std::sync::mpsc::sync_channel(1);
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let release_rx = Mutex::new(release_rx);
        let once = std::sync::atomic::AtomicBool::new(false);
        test_hooks().lock().unwrap().insert(
            cache.to_path_buf(),
            Arc::new(move |step| {
                if step == phase && !once.swap(true, std::sync::atomic::Ordering::SeqCst) {
                    entered_tx.send(()).unwrap();
                    release_rx
                        .lock()
                        .unwrap()
                        .recv_timeout(Duration::from_secs(20))
                        .unwrap();
                }
            }),
        );
        let storage = storage.to_path_buf();
        let cache = cache.to_path_buf();
        let worker = std::thread::spawn(move || {
            let report = run_pass_with_budget(&storage, &|| false, &Instant::now, budget);
            test_hooks().lock().unwrap().remove(&cache);
            report
        });
        entered_rx.recv_timeout(Duration::from_secs(10)).unwrap();
        (worker, release_tx)
    }

    #[test]
    fn retention_walk_allows_view_load_without_lock_waits() {
        let (temp, _root, scope) = fixture();
        let view = crate::views::ViewStore::open(temp.path(), &scope).unwrap();
        fs::write(view.derived_path("old").unwrap(), b"generation").unwrap();
        let (worker, release) = paused_pass(temp.path(), view.view_dir(), "walk");
        let waits = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let observed = waits.clone();
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let loader = std::thread::spawn(move || {
            let _observer = crate::fs_lock::observe_retry_sleeps_for_test(observed);
            let result = crate::pins::QueryPin::acquire(view.view_dir(), "old")
                .map_err(|error| error.to_string())
                .and_then(|pin| {
                    view.current_generation_read_only()
                        .map(|generation| (pin, generation))
                        .map_err(|error| error.to_string())
                });
            done_tx.send(result).unwrap();
        });
        let loaded = done_rx.recv_timeout(Duration::from_secs(6));
        release.send(()).unwrap();
        let report = worker.join().unwrap();
        loader.join().unwrap();
        let (pin, current) = loaded
            .expect("view load blocked by retention walk")
            .unwrap();
        assert_eq!(waits.load(std::sync::atomic::Ordering::SeqCst), 0);
        assert_eq!(current, None);
        assert_eq!(report.removed_generations, 0, "{report:?}");
        drop(pin);
    }

    #[test]
    fn retention_rechecks_a_generation_pinned_during_the_walk() {
        let (temp, _root, scope) = fixture();
        let view = crate::views::ViewStore::open(temp.path(), &scope).unwrap();
        let old = view.derived_path("old").unwrap();
        fs::write(&old, b"generation").unwrap();
        let (worker, release) = paused_pass(temp.path(), view.view_dir(), "walk");
        let pin = crate::pins::QueryPin::acquire(view.view_dir(), "old");
        release.send(()).unwrap();
        let report = worker.join().unwrap();
        let pin = pin.unwrap();
        assert_eq!(report.removed_generations, 0, "{report:?}");
        assert!(old.is_file());
        drop(pin);
        assert_eq!(run_pass(temp.path(), &|| false).removed_generations, 1);
    }

    #[test]
    fn retention_generation_delete_excludes_pin_admission() {
        let (temp, _root, scope) = fixture();
        let view = crate::views::ViewStore::open(temp.path(), &scope).unwrap();
        let old = view.derived_path("old").unwrap();
        fs::write(&old, b"generation").unwrap();
        let (worker, release) = paused_pass(temp.path(), view.view_dir(), "generation-delete");
        // Pin admission acquires this exact lock. A nonwaiting attempt must be
        // refused while the deletion is paused after its protection recheck.
        let admission =
            crate::fs_lock::try_acquire(&temp.path().join(".retention-sweep.lock"), Duration::ZERO);
        release.send(()).unwrap();
        let report = worker.join().unwrap();
        assert!(matches!(
            admission,
            Err(crate::fs_lock::AcquireError::Timeout)
        ));
        assert_eq!(report.removed_generations, 1, "{report:?}");
        assert!(!old.exists());
        assert!(crate::pins::QueryPin::acquire(view.view_dir(), "next").is_ok());
    }

    /// An older AFT release on the same storage collects blobs holding only
    /// the durable `retention/sweep.lock`. A new assembly pin must wait for
    /// that collector, with and without a separate cache root.
    #[test]
    fn assembly_pin_waits_for_an_old_collector_holding_only_the_durable_lock() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        for split in [false, true] {
            let temp = tempfile::tempdir().unwrap();
            let durable = temp.path().join("storage");
            fs::create_dir_all(durable.join("retention")).unwrap();
            let mut prepared = None;
            let artifacts = if split {
                let parent = temp.path().join("cache-volume");
                fs::create_dir(&parent).unwrap();
                let mut config = crate::config::Config {
                    storage_dir: Some(durable.clone()),
                    cache_dir: Some(parent),
                    ..Default::default()
                };
                assert!(crate::root_cache::prepare_cache_root_for_test(&mut config).is_none());
                let artifacts = crate::root_cache::cache_root(&config);
                prepared = Some(config);
                artifacts
            } else {
                durable.clone()
            };
            let view_dir = artifacts.join("views").join("scope");
            crate::private_storage::create_dir_all(&view_dir).unwrap();
            let metadata = view_dir.join("pins/generation.json");

            // The old collector takes only the durable lock.
            let old_collector = crate::fs_lock::try_acquire(
                &durable.join("retention/sweep.lock"),
                Duration::from_secs(5),
            )
            .unwrap();
            let waits = Arc::new(AtomicUsize::new(0));
            let observer = waits.clone();
            let pin_dir = view_dir.clone();
            let writer = std::thread::spawn(move || {
                let _observe = crate::fs_lock::observe_retry_sleeps_for_test(observer);
                crate::pins::AssemblyPin::create(&pin_dir, "family", "scope", "generation", &[])
            });
            // Rendezvous: the writer records a retry on the busy lock before
            // it can write anything.
            let deadline = Instant::now() + Duration::from_secs(20);
            while waits.load(Ordering::SeqCst) == 0 {
                assert!(
                    !writer.is_finished(),
                    "split={split}: pin written while an old collector held retention/sweep.lock"
                );
                assert!(Instant::now() < deadline, "split={split}: pin never waited");
                std::thread::yield_now();
            }
            assert!(
                !metadata.exists(),
                "split={split}: pin visible during collection"
            );
            drop(old_collector);
            let pin = writer
                .join()
                .unwrap()
                .expect("pin after the old collector released its lock");
            assert!(metadata.is_file(), "split={split}");
            drop(pin);
            drop(prepared);
        }
    }

    /// With no separate cache root both retention locks live under one root.
    /// Pin creation takes each once, never itself twice, and releases both.
    #[test]
    fn assembly_pin_on_a_shared_root_takes_each_retention_lock_once() {
        let temp = tempfile::tempdir().unwrap();
        let view_dir = temp.path().join("views").join("scope");
        crate::private_storage::create_dir_all(&view_dir).unwrap();
        let pin = crate::pins::AssemblyPin::create(&view_dir, "family", "scope", "g1", &[])
            .expect("pin on a shared root");
        drop(pin);
        let sweep = temp.path().join("retention/sweep.lock");
        let artifact = temp.path().join(".retention-sweep.lock");
        drop(crate::fs_lock::try_acquire(&sweep, Duration::ZERO).expect("durable lock released"));
        drop(crate::fs_lock::try_acquire(&artifact, Duration::ZERO).expect("cache lock released"));
        let barrier = pin_barrier(&view_dir)
            .unwrap()
            .expect("a views directory is pinned");
        assert!(matches!(
            crate::fs_lock::try_acquire(&sweep, Duration::ZERO),
            Err(crate::fs_lock::AcquireError::Timeout)
        ));
        drop(barrier);
    }

    #[test]
    fn storage_retention_collects_generation_from_a_bound_root() {
        let (temp, root, scope) = fixture();
        let view = crate::views::ViewStore::open(temp.path(), &scope).unwrap();
        let old = view.derived_path("old").unwrap();
        fs::write(&old, b"generation").unwrap();
        crate::root_cache::register_live_scope(temp.path(), &root);
        let report = run_pass(temp.path(), &|| false);
        crate::root_cache::unregister_live_scope(temp.path(), &root);
        assert_eq!(report.removed_generations, 1, "{report:?}");
        assert!(!old.exists());
        assert!(view.view_dir().exists());
    }

    #[test]
    fn storage_retention_generation_lane_visits_every_view_despite_expired_root_budget() {
        let temp = tempfile::tempdir().unwrap();
        assert_generation_lane_bound(&StoragePaths::same(temp.path()));
    }

    #[test]
    fn storage_retention_generation_lane_reaches_nested_v2_views() {
        let storage = tempfile::tempdir().unwrap();
        let paths = StoragePaths::same(storage.path());
        let root = storage.path().join("checkout");
        fs::create_dir(&root).unwrap();
        let registry =
            crate::views::registry::FamilyRegistry::open(storage.path(), "0123456789abcdef")
                .unwrap();
        let mut registrations = Vec::new();
        let mut views = Vec::new();
        for index in 0..145 {
            let scope = if index == 144 {
                "named-scope".to_owned()
            } else {
                format!("{index:016x}")
            };
            let registration = registry
                .register_view_with_storage(storage.path(), &scope, &root)
                .unwrap();
            let view = registration.view_store().unwrap();
            fs::write(view.derived_path("old").unwrap(), b"obsolete").unwrap();
            registrations.push(registration);
            views.push(view);
        }
        let direct = crate::views::ViewStore::open(storage.path(), "ffffffffffffffff").unwrap();
        fs::write(direct.derived_path("old").unwrap(), b"obsolete").unwrap();
        views.push(direct);
        atomic_json(
            &super::cursor_root(&paths).unwrap().join("generations.json"),
            &TraversalCursor {
                domain: "views/v2".into(),
                last_processed: Some("0000000000000048".into()),
            },
        )
        .unwrap();
        let mut removed = 0;
        let mut bytes = 0;
        for _ in 0..4 {
            let report = super::run_pass_with_budget(
                &paths,
                &|| false,
                &|| Instant::now() + Duration::from_secs(60),
                Duration::ZERO,
            );
            assert!(report.errors.is_empty(), "{report:?}");
            removed += report.removed_generations;
            bytes += report.removed_bytes;
        }
        assert_eq!(removed, views.len(), "nested V2 views were not all swept");
        assert_eq!(bytes, 8 * views.len() as u64);
        assert!(views
            .iter()
            .all(|view| !view.derived_path("old").unwrap().exists()));
    }

    #[test]
    fn storage_retention_generation_lane_isolates_corrupt_family_and_advances() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let storage = tempfile::tempdir().unwrap();
        let paths = StoragePaths::same(storage.path());
        let root = storage.path().join("checkout");
        fs::create_dir(&root).unwrap();
        let bad_registry =
            crate::views::registry::FamilyRegistry::open(storage.path(), "0000000000000001")
                .unwrap();
        let good_registry =
            crate::views::registry::FamilyRegistry::open(storage.path(), "0000000000000002")
                .unwrap();
        let bad_registration = bad_registry
            .register_view_with_storage(storage.path(), "0000000000000001", &root)
            .unwrap();
        let good_registration = good_registry
            .register_view_with_storage(storage.path(), "0000000000000002", &root)
            .unwrap();
        let bad_view = bad_registration.view_store().unwrap();
        let good_view = good_registration.view_store().unwrap();
        let bad_old = bad_view.derived_path("old").unwrap();
        fs::write(&bad_old, b"must be retained").unwrap();
        // Corrupt the schema through its tracked connection, never by opening
        // the live SQLite file through a second, raw descriptor.
        bad_registry
            .with_barrier(|tx| {
                tx.execute_batch("DROP TABLE members")?;
                Ok(())
            })
            .unwrap();
        let warnings = Arc::new(AtomicUsize::new(0));
        let observed = warnings.clone();
        let _hook = observe_test_hook(
            bad_registry.path(),
            Arc::new(move |step| {
                if step == "generation-registry-error" {
                    observed.fetch_add(1, Ordering::SeqCst);
                }
            }),
        );
        let cursor_path = super::cursor_root(&paths).unwrap().join("generations.json");
        atomic_json(
            &cursor_path,
            &TraversalCursor {
                domain: "views/v2".into(),
                last_processed: Some("0000000000000000".into()),
            },
        )
        .unwrap();
        for pass in 0..2 {
            let good_old = good_view.derived_path(&format!("old-{pass}")).unwrap();
            fs::write(&good_old, b"reclaim this").unwrap();
            let report = super::run_pass_with_budget(
                &paths,
                &|| false,
                &|| Instant::now() + Duration::from_secs(60),
                Duration::ZERO,
            );
            assert!(
                !good_old.exists(),
                "healthy family was starved by a corrupt neighbor: {report:?}"
            );
            assert!(bad_old.exists(), "unchecked family must remain protected");
            let cursor: TraversalCursor = read_json(&cursor_path).unwrap();
            assert!(
                cursor.last_processed.is_none(),
                "the completed generation lap was not committed: {cursor:?}"
            );
            assert!(
                report.timings_ms.contains_key("roots"),
                "generation lookup failure skipped root-retirement work: {report:?}"
            );
        }
        assert_eq!(
            warnings.load(Ordering::SeqCst),
            1,
            "a persistent bad family must not log on every pass"
        );
    }

    #[test]
    fn generation_registry_census_preserves_closed_wal_bytes() {
        let storage = tempfile::tempdir().unwrap();
        let paths = StoragePaths::same(storage.path());
        let registry =
            crate::views::registry::FamilyRegistry::open(storage.path(), "0000000000000001")
                .unwrap();
        let registration = registry
            .register_view_with_storage(
                storage.path(),
                "scope",
                &storage.path().join("absent-root"),
            )
            .unwrap();
        drop(registration.view_store().unwrap());
        drop(registration);
        registry
            .with_barrier(|tx| {
                tx.set_db_config(
                    rusqlite::config::DbConfig::SQLITE_DBCONFIG_NO_CKPT_ON_CLOSE,
                    true,
                )?;
                tx.execute("UPDATE registry_meta SET gc_epoch = gc_epoch + 1", [])?;
                Ok(())
            })
            .unwrap();
        let database = registry.path().to_path_buf();
        drop(registry);
        assert_eq!(crate::db::file_identity::open_connections(&database), 0);
        let wal = PathBuf::from(format!("{}-wal", database.display()));
        let before_database = fs::read(&database).unwrap();
        let before_wal = fs::read(&wal).unwrap();
        assert!(
            !before_wal.is_empty(),
            "fixture must retain committed WAL frames"
        );
        let batch = super::traversal_batch(
            &paths,
            &[(storage.path().join("views/v2"), false)],
            "census-audit",
            64,
        )
        .unwrap();
        let mut report = SweepReport::default();
        let owners = generation_registries(&paths, &batch.entries, &mut report).unwrap();
        assert_eq!(owners.len(), 1);
        drop(owners);
        assert!(report.errors.is_empty(), "{report:?}");
        assert_eq!(crate::db::file_identity::open_connections(&database), 0);
        assert_eq!(
            fs::read(&database).unwrap(),
            before_database,
            "ownership census checkpointed the registry"
        );
        assert_eq!(
            fs::read(&wal).unwrap(),
            before_wal,
            "ownership census removed or changed the WAL"
        );
    }

    #[test]
    fn storage_retention_generation_lane_reports_unlinked_bytes() {
        let storage = tempfile::tempdir().unwrap();
        let view = crate::views::ViewStore::open(storage.path(), "0123456789abcdef").unwrap();
        for (name, bytes) in [
            ("derived-old.sqlite", 17),
            ("derived-old.sqlite-wal", 5),
            ("derived-old.sqlite-shm", 3),
            ("manifest-old.json", 11),
            ("trigram-old.bin", 13),
        ] {
            fs::write(view.view_dir().join(name), vec![b'x'; bytes]).unwrap();
        }
        fs::write(view.view_dir().join("unrelated"), vec![b'x'; 101]).unwrap();
        let report = run_pass_with_budget(
            storage.path(),
            &|| false,
            &|| Instant::now() + Duration::from_secs(60),
            Duration::ZERO,
        );
        assert!(report.errors.is_empty(), "{report:?}");
        assert_eq!(report.removed_generations, 1);
        assert_eq!(
            report.removed_bytes, 49,
            "must count only successful generation-owned unlinks"
        );
        assert_eq!(report.domains["views/generations"].removed_bytes, 49);
        assert!(view.view_dir().join("unrelated").exists());
    }

    #[test]
    fn storage_retention_generation_lane_uses_cache_views_and_durable_cursor() {
        let temp = tempfile::tempdir().unwrap();
        let durable = temp.path().join("storage");
        let cache = temp.path().join("cache-volume");
        fs::create_dir(&cache).unwrap();
        let mut config = crate::config::Config {
            storage_dir: Some(durable.clone()),
            cache_dir: Some(cache),
            ..Default::default()
        };
        assert!(crate::root_cache::prepare_cache_root_for_test(&mut config).is_none());
        let paths = StoragePaths::for_config(&config);
        assert_ne!(paths.artifacts, durable);
        assert_generation_lane_bound(&paths);
        assert!(super::cursor_root(&paths)
            .unwrap()
            .join("generations.json")
            .is_file());
        assert!(!paths.artifacts.join("retention/cursors").exists());
        assert!(!durable.join("views").exists());
    }

    fn assert_generation_lane_bound(paths: &StoragePaths) {
        let views: Vec<_> = (0..145)
            .map(|index| {
                let view =
                    crate::views::ViewStore::open(&paths.artifacts, &format!("{index:016x}"))
                        .unwrap();
                fs::write(view.derived_path("old").unwrap(), b"obsolete").unwrap();
                view
            })
            .collect();
        // Start halfway through a lap, as a daemon resuming its durable cursor
        // would. Three batches plus one wraparound must reach every view.
        atomic_json(
            &super::cursor_root(paths).unwrap().join("generations.json"),
            &TraversalCursor {
                domain: "views".into(),
                last_processed: Some("0000000000000048".into()),
            },
        )
        .unwrap();
        let mut removed = 0;
        for _ in 0..4 {
            let report = super::run_pass_with_budget(
                paths,
                &|| false,
                &|| Instant::now() + Duration::from_secs(60),
                Duration::ZERO,
            );
            assert!(report.errors.is_empty(), "{report:?}");
            removed += report.removed_generations;
        }
        let unvisited: Vec<_> = views
            .iter()
            .filter(|view| view.derived_path("old").unwrap().exists())
            .map(|view| view.view_dir())
            .collect();
        assert!(unvisited.is_empty(), "unvisited views: {unvisited:?}");
        assert_eq!(removed, views.len());
    }

    #[test]
    fn storage_retention_reclaims_old_missing_roots_in_every_domain() {
        let (temp, root, scope) = fixture();
        let paths = DOMAINS
            .iter()
            .map(|domain| cache(temp.path(), domain, &scope))
            .collect::<Vec<_>>();
        fs::remove_dir(root).unwrap();
        let report = run_pass_entry_bound(temp.path());
        assert_eq!(report.removed_roots, 6, "{report:?}");
        assert_eq!(report.removed_bytes, 0);
        assert_eq!(report.unmeasured_removed_roots, 6);
        assert!(paths.iter().all(|path| !path.exists()));
        assert_eq!(snapshot(temp.path()).unwrap().removed_roots, 6);
    }

    #[test]
    fn storage_retention_keeps_a_live_root_even_with_old_binding() {
        let (temp, _root, scope) = fixture();
        let path = cache(temp.path(), "semantic", &scope);
        assert_eq!(run_pass(temp.path(), &|| false).removed_roots, 0);
        assert!(path.exists());
    }

    #[test]
    fn storage_retention_keeps_a_recently_bound_missing_root() {
        let (temp, root, scope) = fixture();
        record_bind(temp.path(), &root, &scope).unwrap();
        fs::remove_dir(root).unwrap();
        let path = cache(temp.path(), "symbols", &scope);
        assert_eq!(run_pass(temp.path(), &|| false).removed_roots, 0);
        assert!(path.exists());
    }

    fn deleted_checkout_fixture(
        linked: bool,
        minutes: u64,
        legacy: bool,
        history_copies: bool,
    ) -> (tempfile::TempDir, PathBuf, String, PathBuf) {
        let (temp, root, scope) = bound_checkout_fixture(linked, minutes, legacy, history_copies);
        let path = cache(temp.path(), "views", &scope);
        fs::remove_dir_all(&root).unwrap();
        (temp, root, scope, path)
    }

    /// A checkout last bound `minutes` ago.
    fn bound_checkout_fixture(
        linked: bool,
        minutes: u64,
        legacy: bool,
        history_copies: bool,
    ) -> (tempfile::TempDir, PathBuf, String) {
        let temp = tempfile::tempdir().unwrap();
        let main = temp.path().join("main");
        fs::create_dir(&main).unwrap();
        let git = |args: &[&str]| {
            let mut command = std::process::Command::new("git");
            crate::test_env::apply_hermetic_git_env(command.current_dir(&main));
            let output = command.args(args).output().unwrap();
            assert!(output.status.success(), "git {args:?}: {output:?}");
        };
        git(&["init", "--quiet"]);
        git(&[
            "-c",
            "user.name=Retention Tests",
            "-c",
            "user.email=retention@example.invalid",
            "commit",
            "--quiet",
            "--allow-empty",
            "-m",
            "initial",
        ]);
        let root = if linked {
            let worktree = temp.path().join("linked-checkout");
            git(&[
                "worktree",
                "add",
                "--quiet",
                "--detach",
                worktree.to_str().unwrap(),
            ]);
            worktree
        } else {
            main
        };
        let root = fs::canonicalize(root).unwrap();
        let scope = crate::path_identity::project_scope_key(&root);
        record_bind(temp.path(), &root, &scope).unwrap();
        let record = temp.path().join(format!("retention/roots/{scope}.json"));
        let mut binding: serde_json::Value = read_json(&record).unwrap();
        let last_bound = crate::pins::now_ms() - minutes * 60 * 1000;
        binding["last_bound_ms"] = last_bound.into();
        if legacy {
            binding.as_object_mut().unwrap().remove("linked_worktree");
        }
        atomic_json(&record, &binding).unwrap();
        if history_copies {
            // An alias of the surviving parent exercises missing-root identity
            // lookup on Unix, just as verbatim paths do on Windows.
            #[cfg(unix)]
            let memo_root = {
                let alias = temp.path().join("memo-parent");
                std::os::unix::fs::symlink(temp.path(), &alias).unwrap();
                alias.join(root.file_name().unwrap())
            };
            #[cfg(not(unix))]
            let memo_root = root.clone();
            atomic_json(
                &temp.path().join("cache-keys.json"),
                &serde_json::json!({(memo_root.to_string_lossy().as_ref()): {
                    "key": scope, "recorded_at_ms": last_bound - 1000
                }}),
            )
            .unwrap();
            // Use a real exited PID: Windows tasklist rejects u32::MAX, which
            // correctly leaves that synthetic owner's liveness unknown.
            let mut command = std::process::Command::new("git");
            crate::test_env::apply_hermetic_git_env(&mut command);
            let mut child = command
                .arg("--version")
                .stdout(std::process::Stdio::null())
                .spawn()
                .unwrap();
            let owner_pid = child.id();
            assert!(child.wait().unwrap().success());
            crate::artifact_owner::write_synthetic_manifest_for_test(
                temp.path(),
                &root,
                &scope,
                &scope,
                owner_pid,
                last_bound + 1000,
            );
        }
        (temp, root, scope)
    }

    #[test]
    fn storage_retention_linked_worktree_past_margin_with_history_copies_is_reclaimed() {
        let (temp, root, scope, path) = deleted_checkout_fixture(true, 11, false, true);
        let history = bindings(temp.path()).unwrap();
        // Each source contributes a scope and an artifact-key copy. Keep all
        // their timestamps, even though they describe the same checkout.
        assert_eq!(
            history[&scope].len(),
            6,
            "scope={scope}, history={history:?}"
        );
        let original: Binding =
            read_json(&temp.path().join(format!("retention/roots/{scope}.json"))).unwrap();
        let mut clocks: Vec<_> = history[&scope]
            .iter()
            .map(|binding| binding.last_bound_ms)
            .collect();
        clocks.sort_unstable();
        assert_eq!(
            clocks,
            vec![
                original.last_bound_ms - 1000,
                original.last_bound_ms - 1000,
                original.last_bound_ms,
                original.last_bound_ms,
                original.last_bound_ms + 1000,
                original.last_bound_ms + 1000,
            ]
        );
        let report = run_pass(temp.path(), &|| false);
        assert_eq!(report.removed_roots, 1, "{report:?}");
        assert!(!path.exists());
        assert!(missing_root_due(temp.path(), &root, 0));
        let record: serde_json::Value =
            read_json(&temp.path().join(format!("retention/roots/{scope}.json"))).unwrap();
        assert_eq!(record["linked_worktree"], true);
    }

    #[test]
    fn storage_retention_linked_worktree_inside_margin_is_kept() {
        let (temp, root, _scope, path) = deleted_checkout_fixture(true, 9, false, false);
        assert_eq!(run_pass(temp.path(), &|| false).removed_roots, 0);
        assert!(path.exists());
        assert!(!missing_root_due(temp.path(), &root, 0));
    }

    #[test]
    fn storage_retention_present_linked_worktree_past_margin_is_kept() {
        let (temp, root, scope) = bound_checkout_fixture(true, 11, false, true);
        let path = cache(temp.path(), "views", &scope);
        assert_eq!(run_pass(temp.path(), &|| false).removed_roots, 0);
        assert!(path.exists());
        assert!(!missing_root_due(temp.path(), &root, 0));
    }

    #[test]
    fn storage_retention_linked_worktree_recent_owner_clock_still_keeps_root() {
        let (temp, _root, scope, path) = deleted_checkout_fixture(true, 11, false, true);
        let owner = temp
            .path()
            .join(format!("artifact-owners/{scope}/owner.json"));
        let mut metadata: serde_json::Value = read_json(&owner).unwrap();
        metadata["heartbeat_at_ms"] = (crate::pins::now_ms() - 9 * 60 * 1000).into();
        atomic_json(&owner, &metadata).unwrap();
        assert_eq!(run_pass(temp.path(), &|| false).removed_roots, 0);
        assert!(path.exists());
    }

    #[test]
    fn storage_retention_main_checkout_25h_is_kept() {
        let (temp, _root, _scope, path) = deleted_checkout_fixture(false, 25 * 60, false, false);
        assert_eq!(run_pass(temp.path(), &|| false).removed_roots, 0);
        assert!(path.exists());
    }

    #[test]
    fn storage_retention_ambiguous_git_file_does_not_record_linked_topology() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("checkout");
        fs::create_dir(&root).unwrap();
        let separate_git = temp.path().join("separate-git");
        fs::create_dir(&separate_git).unwrap();
        for target in [separate_git, temp.path().join("missing-git")] {
            fs::write(root.join(".git"), format!("gitdir: {}\n", target.display())).unwrap();
            record_bind(temp.path(), &root, "0123456789abcdef").unwrap();
            let scope = crate::path_identity::project_scope_key(&root);
            let binding: Binding =
                read_json(&temp.path().join(format!("retention/roots/{scope}.json"))).unwrap();
            assert!(!binding.linked_worktree);
        }
    }

    #[test]
    fn storage_retention_linked_worktree_unflagged_history_copies_keep_seven_days() {
        let (temp, _root, scope, path) = deleted_checkout_fixture(true, 25 * 60, true, true);
        let history = bindings(temp.path()).unwrap();
        assert_eq!(
            history[&scope].len(),
            6,
            "scope={scope}, history={history:?}"
        );
        assert_eq!(run_pass(temp.path(), &|| false).removed_roots, 0);
        assert!(path.exists());
    }

    #[test]
    fn storage_retention_legacy_binding_uses_seven_days() {
        let (temp, _root, scope, path) = deleted_checkout_fixture(true, 25 * 60, true, false);
        assert_eq!(run_pass(temp.path(), &|| false).removed_roots, 0);
        assert!(path.exists());
        let record = temp.path().join(format!("retention/roots/{scope}.json"));
        let mut binding: serde_json::Value = read_json(&record).unwrap();
        binding["last_bound_ms"] = (crate::pins::now_ms() - 8 * 24 * 60 * 60 * 1000).into();
        atomic_json(&record, &binding).unwrap();
        assert_eq!(run_pass(temp.path(), &|| false).removed_roots, 1);
        assert!(!path.exists());
    }

    #[test]
    fn storage_retention_linked_worktree_past_margin_keeps_live_reader_and_mount_checks() {
        let (temp, _root, scope, path) = deleted_checkout_fixture(true, 11, false, true);
        let _uuid = uuid_backed_binding(temp.path(), &scope);
        let marker = crate::root_cache::ReadMarker::create(&path, "query").unwrap();
        assert_eq!(run_pass(temp.path(), &|| false).removed_roots, 0);
        assert!(path.exists());
        drop(marker);
        let binding =
            disconnect_uuid_backed_binding(temp.path(), &scope, &temp.path().join("unmounted"));
        assert!(!mount_matches(&binding));
        let report = run_pass(temp.path(), &|| false);
        assert_eq!(report.removed_roots, 0, "{report:?}");
        assert_eq!(report.skipped_mount_mismatch, 1, "{report:?}");
        assert!(path.exists());
    }

    #[test]
    fn storage_retention_keeps_a_disconnected_volume() {
        let (temp, root, scope) = fixture();
        let _uuid = uuid_backed_binding(temp.path(), &scope);
        let binding = disconnect_uuid_backed_binding(
            temp.path(),
            &scope,
            &temp.path().join("unmounted-volume"),
        );
        fs::remove_dir(root).unwrap();
        assert!(!mount_matches(&binding));
        let path = cache(temp.path(), "index", &scope);
        let report = run_pass(temp.path(), &|| false);
        assert_eq!(report.removed_roots, 0, "{report:?}");
        assert_eq!(report.skipped_mount_mismatch, 1, "{report:?}");
        assert!(path.exists());
    }

    #[test]
    fn storage_retention_keeps_pins_and_open_file_sets() {
        let (temp, root, scope) = fixture();
        fs::remove_dir(root).unwrap();
        let path = cache(temp.path(), "inspect", &scope);
        let marker = crate::root_cache::ReadMarker::create(&path, "query").unwrap();
        assert_eq!(run_pass(temp.path(), &|| false).removed_roots, 0);
        drop(marker);
        let db = path.join("open.sqlite");
        let connection =
            crate::db::file_identity::IdentityConnection::open(&db, "retention fixture").unwrap();
        assert_eq!(run_pass(temp.path(), &|| false).removed_roots, 0);
        drop(connection);
        assert_eq!(run_pass(temp.path(), &|| false).removed_roots, 1);
        assert!(!path.exists());
    }

    fn due_cache_fixture() -> (tempfile::TempDir, PathBuf, String, PathBuf) {
        let (temp, root, scope) = fixture();
        fs::remove_dir(&root).unwrap();
        let path = cache(temp.path(), "views", &scope);
        (temp, root, scope, path)
    }

    #[test]
    fn retention_due_root_keeps_query_pin() {
        let (temp, _, _, path) = due_cache_fixture();
        let _pin = crate::pins::QueryPin::acquire(&path, "old").unwrap();
        let report = run_pass(temp.path(), &|| false);
        assert_eq!(report.removed_roots, 0, "{report:?}");
        assert!(path.exists());
        assert!(report.skipped_protected > 0);
    }

    #[test]
    fn retention_due_legacy_cache_keeps_a_live_v3_import_source() {
        let _serial = crate::blob_store::v3::serial_for_test();
        let (temp, root, scope) = fixture();
        fs::remove_dir(&root).unwrap();
        let path = cache(temp.path(), "index", &scope);
        // The v3 importer of this key's family holds a live claim: it may be
        // reading `index/<scope>` right now.
        let held = crate::migration::v3_ledger::hold_claim_for_test(temp.path(), &scope, &scope);
        let report = run_pass(temp.path(), &|| false);
        assert_eq!(report.removed_roots, 0, "{report:?}");
        assert!(path.join("payload.bin").exists());
        assert!(report.skipped_protected > 0, "{report:?}");
        // Once the import is gone the binding-age rule applies again.
        drop(held);
        let report = run_pass(temp.path(), &|| false);
        assert_eq!(report.removed_roots, 1, "{report:?}");
        assert!(!path.exists());
    }

    #[test]
    fn retained_legacy_inventory_lists_every_kind_without_writing() {
        let temp = tempfile::tempdir().unwrap();
        let storage = temp.path();
        for (path, bytes) in [
            ("symbols/0123456789abcdef/symbols.bin", &b"symbols"[..]),
            ("views/0123456789abcdef/manifest.json", b"view"),
            ("views/v2/scope/manifest.json", b"v2 view"),
            ("blobs/fedcba9876543210/semantic.sqlite", b"v1 blobs"),
            ("blobs/v2/family/semantic.sqlite", b"v2 blobs"),
            ("blobs/v3/family/members.sqlite", b"v3 registry"),
        ] {
            let path = storage.join(path);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, bytes).unwrap();
        }
        let pins = storage.join("views/0123456789abcdef/pins");
        fs::create_dir_all(&pins).unwrap();
        let _pin =
            crate::pins::QueryPin::acquire(&storage.join("views/0123456789abcdef"), "g").unwrap();
        let snapshot = crate::migration::v3_import::tests::snapshot_tree;
        let before = snapshot(storage);
        let items = retained_legacy_inventory(storage).unwrap();
        let summary = items
            .iter()
            .map(|item| {
                (
                    item.kind,
                    item.path.strip_prefix(storage).unwrap().to_path_buf(),
                    item.copied_into_v3,
                    item.pinned.is_some(),
                )
            })
            .collect::<Vec<_>>();
        assert_eq!(
            summary,
            vec![
                (
                    RetainedKind::SymbolCache,
                    PathBuf::from("symbols/0123456789abcdef"),
                    false,
                    false
                ),
                (
                    RetainedKind::V1View,
                    PathBuf::from("views/0123456789abcdef"),
                    false,
                    true
                ),
                (
                    RetainedKind::V1BlobFamily,
                    PathBuf::from("blobs/fedcba9876543210"),
                    false,
                    false
                ),
                (
                    RetainedKind::V2FamilyStore,
                    PathBuf::from("blobs/v2/family"),
                    true,
                    false
                ),
            ]
        );
        assert!(items.iter().all(|item| item.bytes > 0));
        assert_eq!(snapshot(storage), before);
    }

    #[test]
    fn retention_due_root_keeps_writer_lock() {
        let (temp, _, _, path) = due_cache_fixture();
        let _writer =
            crate::fs_lock::try_acquire(&path.join("writer.lease"), Duration::ZERO).unwrap();
        let report = run_pass(temp.path(), &|| false);
        assert_eq!(report.removed_roots, 0, "{report:?}");
        assert!(path.exists());
        assert!(report.skipped_protected > 0);
    }

    #[test]
    fn retention_due_root_keeps_unknown_pin_state() {
        let (temp, _, _, path) = due_cache_fixture();
        fs::create_dir_all(path.join("pins")).unwrap();
        fs::write(path.join("pins/broken.json"), b"not pin metadata").unwrap();
        let report = run_pass(temp.path(), &|| false);
        assert_eq!(report.removed_roots, 0, "{report:?}");
        assert!(path.exists());
        assert!(!report.errors.is_empty());
    }

    #[test]
    fn retention_due_root_rechecks_rebind_before_rename() {
        let (temp, root, scope, path) = due_cache_fixture();
        let (worker, release) = paused_pass(temp.path(), &path, "walk");
        fs::create_dir(&root).unwrap();
        record_bind(temp.path(), &root, &scope).unwrap();
        release.send(()).unwrap();
        let report = worker.join().unwrap();
        assert_eq!(report.removed_roots, 0, "{report:?}");
        assert!(path.exists());
    }

    #[test]
    fn retention_bind_bypasses_busy_sweep_without_losing_protection() {
        let (temp, _, _, _) = due_cache_fixture();
        let root = temp.path().join("new-checkout");
        fs::create_dir(&root).unwrap();
        let root = fs::canonicalize(root).unwrap();
        let scope = crate::path_identity::project_scope_key(&root);
        let path = cache(temp.path(), "views", &scope);
        assert_bind_bypasses_sweep(&temp, root, scope, path, "cache-presence", 1);
    }

    #[test]
    fn retention_bind_rechecks_recent_clock_before_due_retirement() {
        let (temp, root, scope, path) = due_cache_fixture();
        assert_bind_bypasses_sweep(&temp, root, scope, path, "due-ready", 0);
    }

    #[test]
    fn retention_bind_invalidates_orphan_sweep_history() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("checkout");
        fs::create_dir(&root).unwrap();
        let root = fs::canonicalize(root).unwrap();
        let scope = crate::path_identity::project_scope_key(&root);
        let path = cache(temp.path(), "views", &scope);
        age_orphan(temp.path(), &path, &scope);
        assert_bind_bypasses_sweep(&temp, root, scope, path, "candidate-ready", 0);
    }

    fn assert_bind_bypasses_sweep(
        temp: &tempfile::TempDir,
        root: PathBuf,
        scope: String,
        path: PathBuf,
        phase: &'static str,
        retired_before_bind: usize,
    ) {
        let (sweep, release) =
            paused_pass_with_budget(temp.path(), temp.path(), phase, NO_TIME_BOUND);
        fs::create_dir_all(&root).unwrap();
        let storage = temp.path().to_path_buf();
        let bind_root = root.clone();
        let (bound_tx, bound_rx) = std::sync::mpsc::channel();
        let bind = std::thread::spawn(move || {
            let retries = Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let _observer = crate::fs_lock::observe_retry_sleeps_for_test(retries.clone());
            let started = Instant::now();
            // The bind uses a different key from the checkout's view cache, so
            // clearing the bound key's unknown-observation record cannot keep
            // this view safe by accidentally resetting its observation grace.
            let result = record_bind(&storage, &bind_root, "fedcba9876543210");
            bound_tx
                .send((
                    started.elapsed(),
                    result,
                    retries.load(std::sync::atomic::Ordering::SeqCst),
                ))
                .unwrap();
        });
        let acknowledged = bound_rx.recv_timeout(Duration::from_secs(10));
        let before_release = matches!(&acknowledged, Ok((_, Ok(()), _)));
        if before_release {
            let binding: Binding =
                read_json(&temp.path().join(format!("retention/roots/{scope}.json"))).unwrap();
            assert!(binding.last_bound_ms > 0);
            // Remove the checkout again so only its updated retention record,
            // not its continued existence, can protect the cached artifacts.
            fs::remove_dir(&root).unwrap();
        }
        release.send(()).unwrap();
        let report = sweep.join().unwrap();
        let (elapsed, result, retries) = acknowledged
            .unwrap_or_else(|_| bound_rx.recv_timeout(Duration::from_secs(10)).unwrap());
        bind.join().unwrap();
        eprintln!(
            "retention bind with {phase} paused: {}ms; acknowledged_before_release={before_release}; retry_sleeps={retries}; removed_roots={}",
            elapsed.as_millis(), report.removed_roots
        );
        assert!(
            before_release,
            "bind waited for the busy sweep: {elapsed:?}; {result:?}"
        );
        result.unwrap();
        assert_eq!(retries, 0, "bind retried the busy sweep's lock");
        assert_eq!(report.removed_roots, retired_before_bind, "{report:?}");
        assert!(path.join("payload.bin").exists());
    }

    #[test]
    fn retention_cleanup_yields_to_thirty_two_grouped_binds() {
        use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
        use std::sync::mpsc;

        const BINDS: usize = 32;
        let (temp, _, _, cache) = due_cache_fixture();
        let storage = temp.path();
        let roots: Vec<_> = (0..BINDS)
            .map(|index| {
                let root = storage.join(format!("incoming-{index}"));
                fs::create_dir(&root).unwrap();
                fs::canonicalize(root).unwrap()
            })
            .collect();
        let arrivals = Arc::new((Mutex::new(0_usize), Condvar::new()));
        let rounds = Arc::new(AtomicUsize::new(0));
        let acquisitions = AtomicUsize::new(0);
        let reads = Arc::new(AtomicUsize::new(0));
        let (admitted_tx, admitted_rx) = mpsc::channel();
        let (resume_sweep_tx, resume_sweep_rx) = mpsc::channel();
        let resume_sweep_rx = Mutex::new(resume_sweep_rx);
        let (pending_tx, pending_rx) = mpsc::channel();
        let (resume_bind_tx, resume_bind_rx) = mpsc::channel();
        let resume_bind_rx = Mutex::new(resume_bind_rx);
        let _hook = {
            let admitted_once = AtomicBool::new(false);
            let arrivals = arrivals.clone();
            let rounds = rounds.clone();
            observe_test_hook(
                storage,
                Arc::new(move |step| match step {
                    "due-admitted" if !admitted_once.swap(true, Ordering::SeqCst) => {
                        admitted_tx.send(()).unwrap();
                        resume_sweep_rx
                            .lock()
                            .unwrap()
                            .recv_timeout(Duration::from_secs(20))
                            .unwrap();
                    }
                    "bind-enqueued" => {
                        *arrivals.0.lock().unwrap() += 1;
                        arrivals.1.notify_all();
                    }
                    "bind-round-collecting" => {
                        let count = arrivals.0.lock().unwrap();
                        let (count, timeout) = arrivals
                            .1
                            .wait_timeout_while(count, Duration::from_secs(20), |count| {
                                *count < BINDS
                            })
                            .unwrap();
                        assert!(!timeout.timed_out(), "only {count} binds enqueued");
                    }
                    "bind-round-pending" => {
                        pending_tx.send(()).unwrap();
                        resume_bind_rx
                            .lock()
                            .unwrap()
                            .recv_timeout(Duration::from_secs(20))
                            .unwrap();
                    }
                    "bind-round-admitted" => {
                        rounds.fetch_add(1, Ordering::SeqCst);
                    }
                    _ => {}
                }),
            )
        };
        let _history = {
            let reads = reads.clone();
            observe_test_hook(
                &storage.join("retention"),
                Arc::new(move |step| {
                    if step == "admission-history" {
                        reads.fetch_add(1, Ordering::SeqCst);
                    }
                }),
            )
        };

        // Pause cleanup after admission, then keep all binders waiting before
        // their admission until the entire cleanup pass has had a chance to yield.
        let (pending_before, pending_after, sweep_released, report) = std::thread::scope(|scope| {
            let sweep = scope
                .spawn(|| run_pass_with_budget(storage, &|| false, &Instant::now, NO_TIME_BOUND));
            admitted_rx.recv_timeout(Duration::from_secs(20)).unwrap();
            let workers: Vec<_> = roots
                .iter()
                .map(|root| {
                    let acquisitions = &acquisitions;
                    scope.spawn(move || {
                        crate::fs_lock::io_ledger::take();
                        let result = record_bind(
                            storage,
                            root,
                            &crate::path_identity::project_scope_key(root),
                        );
                        acquisitions.fetch_add(
                            crate::fs_lock::io_ledger::take().acquisitions as usize,
                            Ordering::SeqCst,
                        );
                        result
                    })
                })
                .collect();
            pending_rx.recv_timeout(Duration::from_secs(20)).unwrap();
            let pending_before = bind_pending(storage);
            resume_sweep_tx.send(()).unwrap();
            let report = sweep.join().unwrap();
            let pending_after = bind_pending(storage);
            let sweep_released = !storage.join("retention/sweep.lock").exists();
            resume_bind_tx.send(()).unwrap();
            for worker in workers {
                worker.join().unwrap().unwrap();
            }
            (pending_before, pending_after, sweep_released, report)
        });

        assert!(
            pending_before && pending_after,
            "round lost its pending marker"
        );
        assert!(
            sweep_released,
            "cleanup kept admission while binds were pending"
        );
        assert_eq!(
            reads.load(Ordering::SeqCst),
            0,
            "cleanup reread history: {report:?}"
        );
        assert_eq!(report.removed_roots, 0, "cleanup did not yield: {report:?}");
        assert!(cache.join("payload.bin").exists());
        assert_eq!(rounds.load(Ordering::SeqCst), 1);
        // Count actual filesystem acquisitions on binder threads, not just the
        // round hook: one pending marker and one legacy-compatible sweep lock.
        assert_eq!(acquisitions.load(Ordering::SeqCst), 2);
        assert!(
            !bind_pending(storage),
            "completed round stranded its marker"
        );
        for root in roots {
            let key = crate::path_identity::project_scope_key(&root);
            let binding: Binding =
                read_json(&storage.join(format!("retention/roots/{key}.json"))).unwrap();
            assert_eq!(binding.root, root);
        }
    }

    #[test]
    fn retention_bind_interrupts_admitted_history_reread() {
        let (temp, _, _, _) = due_cache_fixture();
        let root = temp.path().join("new-checkout");
        fs::create_dir(&root).unwrap();
        // A publication round fires its per-root hooks with the canonical root
        // it records. Temp paths are not canonical on macOS (/var is a symlink
        // to /private/var) or Windows (canonical form is \\?\ verbatim), so a
        // hook keyed by the spelled path would never run there.
        let root = fs::canonicalize(root).unwrap();
        let (sweep, release) =
            paused_pass_with_budget(temp.path(), temp.path(), "due-admitted", NO_TIME_BOUND);
        let reads = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let count = reads.clone();
        let _history = observe_test_hook(
            &temp.path().join("retention"),
            Arc::new(move |step| {
                if step == "admission-history" {
                    count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                }
            }),
        );
        let (pending_tx, pending_rx) = std::sync::mpsc::channel();
        let (finish_bind_tx, finish_bind_rx) = std::sync::mpsc::channel();
        let finish_bind_rx = Mutex::new(finish_bind_rx);
        let _pending = observe_test_hook(
            &root,
            Arc::new(move |step| {
                if step == "bind-pending" {
                    pending_tx.send(()).unwrap();
                    // Keep the bind pending for the whole observed pass. Once
                    // it finishes, the lexical lane may legitimately retry the
                    // cache, and that later reread does not indicate a failed yield.
                    finish_bind_rx
                        .lock()
                        .unwrap()
                        .recv_timeout(Duration::from_secs(20))
                        .unwrap();
                }
            }),
        );
        let storage = temp.path().to_path_buf();
        let bind = std::thread::spawn(move || record_bind(&storage, &root, "fedcba9876543210"));
        pending_rx.recv_timeout(Duration::from_secs(10)).unwrap();
        let pending_before_sweep = bind_pending(temp.path());
        release.send(()).unwrap();
        let report = sweep.join().unwrap();
        let pending_after_sweep = bind_pending(temp.path());
        finish_bind_tx.send(()).unwrap();
        bind.join().unwrap().unwrap();
        assert!(pending_before_sweep && pending_after_sweep);
        assert_eq!(
            reads.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "an admitted sweep must yield before rereading history for a waiting bind: {report:?}"
        );
    }

    #[test]
    fn retention_bind_still_excludes_legacy_sweepers() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("checkout");
        fs::create_dir(&root).unwrap();
        // Per-root publication hooks are keyed by the canonical root; see
        // retention_bind_interrupts_admitted_history_reread.
        let root = fs::canonicalize(root).unwrap();
        let (entered_tx, entered_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let release_rx = Mutex::new(release_rx);
        let _hook = observe_test_hook(
            &root,
            Arc::new(move |step| {
                if step == "bind-protected" {
                    entered_tx.send(()).unwrap();
                    release_rx
                        .lock()
                        .unwrap()
                        .recv_timeout(Duration::from_secs(20))
                        .unwrap();
                }
            }),
        );
        let storage = temp.path().to_path_buf();
        let bind = std::thread::spawn(move || record_bind(&storage, &root, "fedcba9876543210"));
        entered_rx.recv_timeout(Duration::from_secs(10)).unwrap();
        // Older binaries know only this lock. They must remain excluded while
        // the new binder publishes its durable registration.
        let legacy =
            crate::fs_lock::try_acquire(&temp.path().join("retention/sweep.lock"), Duration::ZERO);
        release_tx.send(()).unwrap();
        bind.join().unwrap().unwrap();
        assert!(
            legacy.is_err(),
            "new binding bypassed the legacy retention lock"
        );
    }

    #[test]
    fn retention_bind_hook_on_noncanonical_root_fails_loudly() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("checkout");
        fs::create_dir(&root).unwrap();
        // A `..` component makes this spelling differ from the canonical root
        // on every platform, as /var and \\?\ spellings do on macOS and Windows.
        let spelled = root.join("..").join("checkout");
        let (entered_tx, entered_rx) = std::sync::mpsc::channel::<()>();
        let _hook = observe_test_hook(
            &spelled,
            Arc::new(move |step| {
                if step == "bind-protected" {
                    let _ = entered_tx.send(());
                }
            }),
        );
        let storage = temp.path().to_path_buf();
        let bind = std::thread::spawn(move || record_bind(&storage, &spelled, "fedcba9876543210"));
        let panic = bind
            .join()
            .expect_err("a hook keyed by a non-canonical root must fail the bind");
        let message = panic.downcast_ref::<String>().cloned().unwrap_or_default();
        assert!(
            message.contains("publication hooks fire on its canonical root"),
            "{message}"
        );
        // The hook was dropped, so a test waiting on its channel fails at once
        // instead of waiting out its timeout.
        assert_eq!(
            entered_rx.try_recv(),
            Err(std::sync::mpsc::TryRecvError::Disconnected)
        );
    }

    #[test]
    fn retention_retires_a_mount_point_cache_by_emptying_it_on_its_own_volume() {
        // The reaper runs on its own thread and reports why it deferred work
        // only through warnings. Send them to this test's captured output so
        // a failure explains itself.
        let _ = env_logger::builder()
            .is_test(true)
            .filter_module("aft::storage_retention", log::LevelFilter::Warn)
            .filter_module("aft::cache_location", log::LevelFilter::Warn)
            .try_init();
        let (temp, _, _, path) = due_cache_fixture();
        let steps = Arc::new(Mutex::new(Vec::<String>::new()));
        let observed = steps.clone();
        let _hook = observe_test_hook(
            temp.path(),
            Arc::new(move |step| observed.lock().unwrap().push(step.to_owned())),
        );
        let seen = |step: &str| steps.lock().unwrap().iter().any(|seen| seen == step);
        fs::create_dir_all(path.join("nested")).unwrap();
        fs::write(path.join("nested/derived.bin"), b"rebuildable").unwrap();
        // The domain folder and the cache folder below it report different
        // devices, as when a separate volume is mounted at the cache folder.
        // Renaming the cache into `views/.retention-trash` would then fail
        // with EXDEV.
        let _views = crate::walk_boundary::simulated_volumes::simulate(
            &temp.path().join("views"),
            u64::MAX - 10,
        );
        let _mount = crate::walk_boundary::simulated_volumes::simulate(&path, u64::MAX - 11);

        // A pass past its time budget skips scheduling the reaper, and a
        // loaded runner can exhaust the budget; this test is about where the
        // retired entries go, not about speed.
        let report = run_pass_with_budget(temp.path(), &|| false, &Instant::now, NO_TIME_BOUND);
        assert_eq!(report.removed_roots, 1, "{report:?}");
        assert!(report.errors.is_empty(), "{report:?}");
        assert!(
            seen("reaper-launch"),
            "the pass did not schedule a reaper: {:?}",
            steps.lock().unwrap()
        );
        // Hang guard, not a speed bound: wait for the scheduled reaper to
        // finish however slow the runner is, and fail only if it never does.
        let hang_guard = Instant::now() + Duration::from_secs(120);
        while reaper_running_for_test(temp.path()) {
            assert!(
                Instant::now() < hang_guard,
                "hang guard: the reaper scheduled for {} was still running after 120 s; steps: {:?}",
                temp.path().display(),
                steps.lock().unwrap()
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        // That reaper may leave the mount-local trash for a later reaper, by
        // design: when sweep.lock is busy, or after a transient error it
        // logs. Run later reaper passes, as the next retention pass would.
        let mount_emptied = || fs::read_dir(&path).unwrap().next().is_none();
        for _ in 0..3 {
            if mount_emptied() {
                break;
            }
            if let Err(error) = reap_retired(temp.path()) {
                panic!(
                    "a later reaper pass failed: {error}; steps: {:?}",
                    steps.lock().unwrap()
                );
            }
        }
        assert!(
            seen("reap-root") && mount_emptied(),
            "the mount-local trash was never reaped; steps: {:?}; pending marker present: {}; left in the mount: {:?}",
            steps.lock().unwrap(),
            temp.path().join("retention/mount-trash-pending").exists(),
            fs::read_dir(&path)
                .unwrap()
                .map(|entry| entry.unwrap().path())
                .collect::<Vec<_>>()
        );
        // Every reaper has returned; take its lock anyway so the checks below
        // cannot overlap a straggling unlink.
        let reaped = crate::fs_lock::try_acquire(
            &temp.path().join("retention/reap.lock"),
            Duration::from_secs(10),
        )
        .unwrap();
        assert!(path.is_dir(), "the mount point itself must stay");
        assert_eq!(
            fs::read_dir(&path).unwrap().count(),
            0,
            "every entry of the mounted cache is retired"
        );
        assert!(
            !temp.path().join("views/.retention-trash").exists()
                || fs::read_dir(temp.path().join("views/.retention-trash"))
                    .unwrap()
                    .count()
                    == 0,
            "nothing crossed into the domain folder's trash"
        );
        drop(reaped);

        // The emptied mount point counts as gone, so the next pass does not
        // retire it again.
        let report = run_pass(temp.path(), &|| false);
        assert_eq!(report.removed_roots, 0, "{report:?}");
    }

    #[test]
    fn retention_reaper_skips_mount_probes_without_pending_marker() {
        let (temp, _, scope, _) = due_cache_fixture();
        cache(temp.path(), "callgraph", &scope);
        let ordinary = temp
            .path()
            .join(format!("views/.retention-trash/retired-{:032x}", 1));
        fs::create_dir_all(&ordinary).unwrap();
        fs::write(ordinary.join("payload.bin"), b"retired").unwrap();
        let ordinary_pin =
            crate::bash_background::persistence::PinnedDir::repair_open_path(&ordinary).unwrap();
        write_retirement_witness(
            temp.path(),
            &ordinary_pin,
            &format!("{:032x}", 1),
            retirement_members(&ordinary_pin).unwrap(),
        )
        .unwrap();
        let probes = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let observed = probes.clone();
        let _hook = observe_test_hook(
            temp.path(),
            Arc::new(move |step| {
                if step == "mount-trash-probe" {
                    observed.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                }
            }),
        );
        reap_retired(temp.path()).unwrap();
        assert_eq!(
            probes.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "no marker means no cache-folder mount probes"
        );
        assert!(!ordinary.exists(), "domain-local trash still gets reaped");

        let marker = temp.path().join("retention/mount-trash-pending");
        {
            let _admission = barrier(temp.path()).unwrap();
            atomic_json(&marker, &"scan").unwrap();
        }
        reap_retired(temp.path()).unwrap();
        assert_eq!(probes.load(std::sync::atomic::Ordering::SeqCst), 2);
        assert!(!marker.exists(), "a clean scan clears the marker");
        reap_retired(temp.path()).unwrap();
        assert_eq!(probes.load(std::sync::atomic::Ordering::SeqCst), 2);
    }

    fn stopped_pass_keeps_mount_trash(cancel: bool) {
        use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

        let (temp, _, _, path) = due_cache_fixture();
        let _views = crate::walk_boundary::simulated_volumes::simulate(
            &temp.path().join("views"),
            u64::MAX - 10,
        );
        let _mount = crate::walk_boundary::simulated_volumes::simulate(&path, u64::MAX - 11);
        {
            let _admission = barrier(temp.path()).unwrap();
            retire_directory(temp.path(), &path).unwrap();
        }
        let trash = path.join(".retention-trash");
        let retired = fs::read_dir(&trash)
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        let marker = temp.path().join("retention/mount-trash-pending");
        let generation = fs::read(&marker).unwrap();
        let stopped = Arc::new(AtomicBool::new(false));
        let launches = Arc::new(AtomicUsize::new(0));
        let hook = {
            let stopped = stopped.clone();
            let launches = launches.clone();
            observe_test_hook(
                temp.path(),
                Arc::new(move |step| match step {
                    // Stop after due-first has returned, so the root lane must
                    // refuse reaper launch instead of relying on its early return.
                    "cursor-commit" => stopped.store(true, Ordering::SeqCst),
                    "reaper-launch" => {
                        launches.fetch_add(1, Ordering::SeqCst);
                    }
                    _ => {}
                }),
            )
        };
        // Keep a regressed launch from unlinking the fixture asynchronously.
        let reaper =
            crate::fs_lock::try_acquire(&temp.path().join("retention/reap.lock"), Duration::ZERO)
                .unwrap();
        let started = Instant::now();
        let clock = || {
            started
                + if !cancel && stopped.load(Ordering::SeqCst) {
                    NO_TIME_BOUND * 2
                } else {
                    Duration::ZERO
                }
        };
        let report = run_pass_with_budget(
            temp.path(),
            &|| cancel && stopped.load(Ordering::SeqCst),
            &clock,
            NO_TIME_BOUND,
        );
        assert!(stopped.load(Ordering::SeqCst), "stop hook was not reached");
        assert!(report.errors.is_empty(), "{report:?}");
        assert_eq!(report.cancelled, cancel, "{report:?}");
        assert!(cancel || report.stopped_early, "{report:?}");
        assert_eq!(
            launches.load(Ordering::SeqCst),
            usize::from(!cancel),
            "only cancellation, not root-budget expiry, prevents reaper launch"
        );
        assert_eq!(fs::read(&marker).unwrap(), generation);
        assert!(retired.join("payload.bin").exists());
        let until = Instant::now() + Duration::from_secs(10);
        while reaper_running_for_test(temp.path()) && Instant::now() < until {
            std::thread::sleep(Duration::from_millis(10));
        }
        drop(hook);
        drop(reaper);
        reap_retired(temp.path()).unwrap();
        assert!(!trash.exists(), "a later reaper can reclaim the trash");
        assert!(!marker.exists(), "a later durable reap clears the marker");
    }

    #[test]
    fn retention_cancelled_pass_does_not_launch_mount_trash_reaper() {
        stopped_pass_keeps_mount_trash(true);
    }

    #[test]
    fn retention_expired_pass_launches_mount_trash_reaper() {
        stopped_pass_keeps_mount_trash(false);
    }

    #[test]
    fn retention_reaper_resumes_interrupted_mount_point_retirement() {
        let (temp, _, _, path) = due_cache_fixture();
        let _views = crate::walk_boundary::simulated_volumes::simulate(
            &temp.path().join("views"),
            u64::MAX - 10,
        );
        let _mount = crate::walk_boundary::simulated_volumes::simulate(&path, u64::MAX - 11);
        let trash = path.join(".retention-trash");
        let marker = temp.path().join("retention/mount-trash-pending");
        let before_moves = path.clone();
        let pending = marker.clone();
        let _hook = observe_test_hook(
            &path,
            Arc::new(move |step| {
                if step == "rename-root" {
                    assert!(
                        pending.is_file(),
                        "publish the marker before moving entries"
                    );
                    assert!(before_moves.join("payload.bin").exists());
                }
            }),
        );
        {
            let _admission = barrier(temp.path()).unwrap();
            // Model a daemon exit after all moves but before the reaper runs.
            retire_directory(temp.path(), &path).unwrap();
        }
        let retired = fs::read_dir(&trash)
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        assert!(retired.join("payload.bin").exists());
        assert!(marker.exists());
        assert!(!emptied_mount_point(&path), "retired data is still on disk");

        reap_retired(temp.path()).unwrap();
        assert!(
            !trash.exists(),
            "a later reaper pass must reclaim local trash"
        );
        assert!(!marker.exists(), "a clean reap clears the marker");
        assert!(path.is_dir(), "the mount point itself must stay");
        assert!(emptied_mount_point(&path));

        let unknown = trash.join("unrecognized");
        let foreign = trash.join(format!("retired-{:032x}", 2));
        fs::create_dir_all(&unknown).unwrap();
        fs::create_dir_all(&foreign).unwrap();
        fs::write(unknown.join("important"), b"not retired").unwrap();
        fs::write(foreign.join("important"), b"another volume").unwrap();
        let _foreign = crate::walk_boundary::simulated_volumes::simulate(&foreign, u64::MAX - 12);
        {
            let _admission = barrier(temp.path()).unwrap();
            atomic_json(&marker, &"protected").unwrap();
        }
        reap_retired(temp.path()).unwrap();
        assert!(unknown.join("important").exists());
        assert!(foreign.join("important").exists());
        assert!(!emptied_mount_point(&path));
        assert!(marker.exists(), "unreaped tombstones keep the marker");
    }

    fn mount_trash_sync_failure_keeps_marker(keep_container: bool) {
        let (temp, _, _, path) = due_cache_fixture();
        let _views = crate::walk_boundary::simulated_volumes::simulate(
            &temp.path().join("views"),
            u64::MAX - 10,
        );
        let _mount = crate::walk_boundary::simulated_volumes::simulate(&path, u64::MAX - 11);
        {
            let _admission = barrier(temp.path()).unwrap();
            retire_directory(temp.path(), &path).unwrap();
        }
        let trash = path.join(".retention-trash");
        if keep_container {
            fs::write(trash.join("unknown"), b"keep unrecognized entries").unwrap();
        }
        let sync_path = if keep_container { &trash } else { &path };
        let marker = temp.path().join("retention/mount-trash-pending");
        let generation = fs::read(&marker).unwrap();
        crate::durability::FAIL_DIRECTORY_SYNC.with(|slot| {
            *slot.borrow_mut() = Some(sync_path.to_path_buf());
        });
        let result = reap_retired(temp.path());
        crate::durability::FAIL_DIRECTORY_SYNC.with(|slot| slot.borrow_mut().take());
        assert!(
            result.is_err(),
            "mount-local sync failure must defer reaping"
        );
        assert_eq!(fs::read(&marker).unwrap(), generation);
        assert_eq!(trash.exists(), keep_container);

        crate::durability::take();
        reap_retired(temp.path()).unwrap();
        assert_eq!(
            marker.exists(),
            keep_container,
            "unrecognized trash must remain discoverable after a durable reap"
        );
        #[cfg(unix)]
        {
            let events = crate::durability::take();
            assert!(events.contains(&(crate::durability::EventKind::DirectorySync, path)));
            if keep_container {
                assert!(
                    events.contains(&(crate::durability::EventKind::DirectorySync, trash.clone()))
                );
            }
        }
        if keep_container {
            fs::remove_file(trash.join("unknown")).unwrap();
            reap_retired(temp.path()).unwrap();
            assert!(
                !marker.exists(),
                "only a clean durable reap clears the marker"
            );
        }
    }

    #[test]
    fn retention_mount_trash_parent_sync_failure_keeps_marker() {
        mount_trash_sync_failure_keeps_marker(false);
    }

    #[test]
    fn retention_mount_trash_container_sync_failure_keeps_marker() {
        mount_trash_sync_failure_keeps_marker(true);
    }

    #[test]
    fn retention_mount_trash_marker_survives_concurrent_retirement() {
        let (temp, _, _, path) = due_cache_fixture();
        let _views = crate::walk_boundary::simulated_volumes::simulate(
            &temp.path().join("views"),
            u64::MAX - 10,
        );
        let _mount = crate::walk_boundary::simulated_volumes::simulate(&path, u64::MAX - 11);
        {
            let _admission = barrier(temp.path()).unwrap();
            retire_directory(temp.path(), &path).unwrap();
        }
        let marker = temp.path().join("retention/mount-trash-pending");
        let previous = fs::read(&marker).unwrap();
        let (scanned_tx, scanned_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let release_rx = Mutex::new(release_rx);
        let hook = observe_test_hook(
            temp.path(),
            Arc::new(move |step| {
                if step == "mount-trash-scanned" {
                    scanned_tx.send(()).unwrap();
                    release_rx
                        .lock()
                        .unwrap()
                        .recv_timeout(Duration::from_secs(20))
                        .unwrap();
                }
            }),
        );
        let storage = temp.path().to_path_buf();
        let worker = std::thread::spawn(move || reap_retired(&storage));
        scanned_rx.recv_timeout(Duration::from_secs(10)).unwrap();
        {
            let _admission = barrier(temp.path()).unwrap();
            fs::write(path.join("new-payload.bin"), b"retired after the scan").unwrap();
            retire_directory(temp.path(), &path).unwrap();
        }
        let current = fs::read(&marker).unwrap();
        release_tx.send(()).unwrap();
        worker.join().unwrap().unwrap();
        drop(hook);
        assert_ne!(
            previous, current,
            "each retirement publishes a new generation"
        );
        assert_eq!(
            fs::read(&marker).expect("a concurrent retirement's marker must survive"),
            current,
            "a concurrent retirement's marker must survive"
        );
        let trash = path.join(".retention-trash");
        let retired = fs::read_dir(&trash)
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        assert!(retired.join("new-payload.bin").exists());
        reap_retired(temp.path()).unwrap();
        assert!(!trash.exists());
        assert!(!marker.exists());
    }

    #[test]
    fn retention_mount_point_unlink_runs_outside_sweep_lock() {
        let (temp, _, scope, path) = due_cache_fixture();
        let _views = crate::walk_boundary::simulated_volumes::simulate(
            &temp.path().join("views"),
            u64::MAX - 10,
        );
        let _mount = crate::walk_boundary::simulated_volumes::simulate(&path, u64::MAX - 11);
        let sweep_lock = temp.path().join("retention/sweep.lock");
        let rename_lock = sweep_lock.clone();
        let renamed = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let renamed_hook = renamed.clone();
        let _rename_hook = observe_test_hook(
            &path,
            Arc::new(move |step| {
                if step == "rename-root" {
                    assert!(crate::fs_lock::try_acquire(&rename_lock, Duration::ZERO).is_err());
                    renamed_hook.store(true, std::sync::atomic::Ordering::SeqCst);
                }
            }),
        );
        let reaped = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let reaped_hook = reaped.clone();
        let _reap_hook = observe_test_hook(
            temp.path(),
            Arc::new(move |step| {
                if step == "reap-root" {
                    let _admission =
                        crate::fs_lock::try_acquire(&sweep_lock, Duration::ZERO).unwrap();
                    reaped_hook.store(true, std::sync::atomic::Ordering::SeqCst);
                }
            }),
        );
        let mut report = SweepReport::default();
        let mut due = DueBatch::default();
        let deadline = Instant::now() + PASS_BUDGET;
        prepare_due_cache(
            &StoragePaths::same(temp.path()),
            &temp.path().join("views"),
            &scope,
            &bindings(temp.path()).unwrap(),
            deadline,
            &mut due,
            &mut report,
        )
        .unwrap();
        due.flush(
            &StoragePaths::same(temp.path()),
            &mut RootIdentities::default(),
            crate::pins::now_ms(),
            deadline,
            &Instant::now,
            &|| false,
            &mut report,
        )
        .unwrap();
        assert_eq!(report.removed_roots, 1, "{report:?}");
        assert!(renamed.load(std::sync::atomic::Ordering::SeqCst));
        let trash = path.join(".retention-trash");
        assert!(
            trash.is_dir(),
            "mount-local trash was deleted during locked retirement"
        );
        let retired = fs::read_dir(&trash)
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        assert!(retired.join("payload.bin").exists());

        reap_retired(temp.path()).unwrap();
        assert!(reaped.load(std::sync::atomic::Ordering::SeqCst));
        assert!(!trash.exists());
    }

    #[test]
    fn retention_reaper_resumes_retired_roots_without_a_pass() {
        let (temp, _, _, path) = due_cache_fixture();
        retire_directory(temp.path(), &path).unwrap();
        let trash = temp.path().join("views/.retention-trash");
        assert_eq!(fs::read_dir(&trash).unwrap().count(), 1);
        assert!(!path.exists());
        // No in-memory queue is needed to discover a rename left by an exit.
        reap_retired(temp.path()).unwrap();
        assert_eq!(fs::read_dir(&trash).unwrap().count(), 0);
    }

    #[test]
    fn retention_due_rename_does_not_wait_for_background_unlink() {
        let (temp, _, _, path) = due_cache_fixture();
        let (entered_tx, entered_rx) = std::sync::mpsc::sync_channel(1);
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let release_rx = Mutex::new(release_rx);
        let _hook = observe_test_hook(
            temp.path(),
            Arc::new(move |step| {
                if step == "reap-root" {
                    entered_tx.send(()).unwrap();
                    release_rx
                        .lock()
                        .unwrap()
                        .recv_timeout(Duration::from_secs(20))
                        .unwrap();
                }
            }),
        );
        let started = Instant::now();
        let report = run_pass(temp.path(), &|| false);
        let elapsed = started.elapsed();
        entered_rx.recv_timeout(Duration::from_secs(10)).unwrap();
        let retired = fs::read_dir(temp.path().join("views/.retention-trash"))
            .unwrap()
            .count();
        release_tx.send(()).unwrap();
        assert_eq!(report.removed_roots, 1, "{report:?}");
        assert_eq!(report.unmeasured_removed_roots, 1);
        assert!(!path.exists());
        assert_eq!(retired, 1, "pass unlinked the root synchronously");
        assert!(
            elapsed < PASS_BUDGET,
            "background unlink consumed the pass budget"
        );
    }

    #[cfg(unix)]
    #[test]
    fn retention_due_root_keeps_symlink_and_reaper_keeps_unknown_trash() {
        let (temp, _, _, path) = due_cache_fixture();
        let outside = temp.path().join("outside");
        fs::create_dir(&outside).unwrap();
        fs::write(outside.join("important"), b"not a cache").unwrap();
        std::os::unix::fs::symlink(&outside, path.join("link")).unwrap();
        // This fixture invokes the reaper itself below; avoid racing an
        // independently scheduled pass for the same reap lock.
        states()
            .lock()
            .unwrap()
            .entry(temp.path().to_path_buf())
            .or_default()
            .reaping = true;
        let report = run_pass(temp.path(), &|| false);
        assert_eq!(report.removed_roots, 0, "{report:?}");
        assert!(path.exists());
        let error = retire_directory(temp.path(), &path)
            .expect_err("retirement must refuse a symlinked member");
        assert!(
            matches!(error.raw_os_error(), Some(libc::ELOOP | libc::ENOTDIR)),
            "{error}"
        );
        let retired = temp
            .path()
            .join("views/.retention-trash/retired-0123456789abcdef0123456789abcdef");
        fs::create_dir_all(retired.parent().unwrap()).unwrap();
        fs::rename(&path, &retired).unwrap();
        reap_retired(temp.path()).unwrap();
        assert_eq!(
            fs::read_dir(temp.path().join("views/.retention-trash"))
                .unwrap()
                .count(),
            1
        );
        assert!(outside.join("important").exists());
    }

    #[test]
    fn storage_retention_unknown_keys_receive_an_observation_grace() {
        let temp = tempfile::tempdir().unwrap();
        let key = "0123456789abcdef";
        let path = cache(temp.path(), "symbols", key);
        filetime::set_file_mtime(
            path.join("payload.bin"),
            filetime::FileTime::from_unix_time(0, 0),
        )
        .unwrap();
        assert_eq!(run_pass(temp.path(), &|| false).removed_roots, 0);
        atomic_json(
            &temp.path().join(format!("retention/unknown/{key}.json")),
            &0_u64,
        )
        .unwrap();
        assert_eq!(run_pass(temp.path(), &|| false).removed_roots, 1);
        assert!(!path.exists());
        assert!(
            !temp
                .path()
                .join(format!("retention/unknown/{key}.json"))
                .exists(),
            "deleted cache retained its old observation grace"
        );
    }

    #[test]
    fn storage_retention_observation_restart_child() {
        let Some(storage) = std::env::var_os("AFT_RETENTION_RESTART_STORAGE") else {
            return;
        };
        let storage = StoragePaths::same(&PathBuf::from(storage));
        // This fixture calls the candidate lane directly rather than run_pass,
        // so it supplies the same existing metadata root as production setup.
        fs::create_dir_all(storage.durable.join("retention")).unwrap();
        let now: u64 = std::env::var("AFT_RETENTION_RESTART_NOW")
            .unwrap()
            .parse()
            .unwrap();
        let key = "0123456789abcdef";
        let mut report = SweepReport::default();
        let mut due = DueBatch::default();
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut roots = RootIdentities::default();
        candidate(
            &storage,
            &mut roots,
            &storage.artifacts.join("symbols"),
            key,
            false,
            &BTreeMap::new(),
            now,
            deadline,
            &|| false,
            &mut ViewMembers::default(),
            &mut due,
            &mut report,
        )
        .unwrap();
        due.flush(
            &storage,
            &mut roots,
            now,
            deadline,
            &Instant::now,
            &|| false,
            &mut report,
        )
        .unwrap();
        atomic_json(
            &storage.durable.join(format!("restart-result-{now}.json")),
            &report,
        )
        .unwrap();
    }

    #[test]
    fn storage_retention_unknown_observation_survives_process_restart() {
        let temp = tempfile::tempdir().unwrap();
        let key = "0123456789abcdef";
        // A symbols cache, not a checkout view folder: view folders without
        // history follow the hour-long orphan rule, while every other domain
        // keeps the week-long observation grace this test exercises.
        let view = cache(temp.path(), "symbols", key);
        // An old payload alone must not authorize deletion on first sight.
        // The synthetic clock advances a week without editing the observation
        // file or waiting a week; every observation uses a fresh OS process.
        filetime::set_file_mtime(
            view.join("payload.bin"),
            filetime::FileTime::from_unix_time(0, 0),
        )
        .unwrap();
        let first_seen = 1000_u64;
        let observation = temp.path().join(format!("retention/unknown/{key}.json"));
        for (now, should_remove) in [
            (first_seen, false),
            (first_seen + ROOT_GRACE_MS - 1, false),
            (first_seen + ROOT_GRACE_MS, true),
        ] {
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "storage_retention::storage_retention_tests::storage_retention_observation_restart_child",
                ])
                .env("AFT_RETENTION_RESTART_STORAGE", temp.path())
                .env("AFT_RETENTION_RESTART_NOW", now.to_string())
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "retention child failed: stdout={} stderr={}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            if should_remove {
                assert!(
                    !observation.exists(),
                    "removed cache kept its old observation"
                );
            } else {
                assert_eq!(
                    read_json::<u64>(&observation).unwrap(),
                    first_seen,
                    "a restarted pass replaced the first observation"
                );
            }
            let result: serde_json::Value =
                read_json(&temp.path().join(format!("restart-result-{now}.json"))).unwrap();
            assert_eq!(result["removed_roots"], usize::from(should_remove));
            assert_eq!(
                view.exists(),
                !should_remove,
                "a fresh process did not honor the original observation grace at {now}"
            );
        }
    }

    #[test]
    fn storage_retention_cancellation_leaves_the_root_untouched() {
        let (temp, root, scope) = fixture();
        fs::remove_dir(root).unwrap();
        let path = cache(temp.path(), "symbols", &scope);
        let report = run_pass(temp.path(), &|| true);
        assert!(report.cancelled);
        assert_eq!(report.removed_roots, 0);
        assert!(path.exists());
    }

    #[test]
    fn storage_retention_bounds_enumeration_and_resumes() {
        let temp = tempfile::tempdir().unwrap();
        for number in 0..80 {
            cache(temp.path(), "symbols", &format!("{number:016x}"));
        }
        let first = run_pass_entry_bound(temp.path());
        let second = run_pass_entry_bound(temp.path());
        assert_eq!(first.examined, PASS_ENTRIES);
        assert_eq!(second.examined, 16);
        assert!(first.stopped_early);
    }

    #[test]
    fn storage_retention_time_budget_stops_the_walk_before_the_entry_bound() {
        let temp = tempfile::tempdir().unwrap();
        for number in 0..80 {
            cache(temp.path(), "symbols", &format!("{number:016x}"));
        }
        // An exhausted budget stops the pass before its first entry, however
        // many entries remain, and leaves the cursor where it was.
        let stopped = run_pass_with_budget(temp.path(), &|| false, &Instant::now, Duration::ZERO);
        assert_eq!(stopped.examined, 0, "{stopped:?}");
        assert!(stopped.stopped_early, "{stopped:?}");
        let resumed = run_pass_entry_bound(temp.path());
        assert_eq!(resumed.examined, PASS_ENTRIES, "{resumed:?}");
    }

    /// Makes an orphan view look observed, and unchanged, for over an hour.
    fn age_orphan(storage: &Path, view: &Path, key: &str) {
        let two_hours_ago = (crate::pins::now_ms() / 1000) as i64 - 2 * 60 * 60;
        filetime::set_file_mtime(
            view.join("payload.bin"),
            filetime::FileTime::from_unix_time(two_hours_ago, 0),
        )
        .unwrap();
        atomic_json(
            &storage.join(format!("retention/unknown/{key}.json")),
            &(crate::pins::now_ms() - ORPHAN_VIEW_GRACE_MS - 60_000),
        )
        .unwrap();
    }

    #[test]
    fn storage_retention_orphan_view_is_reclaimed_after_its_short_grace() {
        let temp = tempfile::tempdir().unwrap();
        let key = "0123456789abcdef";
        let view = cache(temp.path(), "views", key);
        // This tests the observation grace, not how fast the runner can walk
        // the view. Keep the production pass deadline out of both sweeps.
        // Expected: the first pass keeps the folder. A folder seen for the
        // first time may belong to a build in progress, so it is counted as
        // recent and its observation clock starts.
        let first = run_pass_entry_bound(temp.path());
        assert_eq!(first.removed_roots, 0, "{first:?}");
        assert_eq!(first.orphan_views.skipped_recent, 1, "{first:?}");
        assert!(view.exists());
        // Expected: once the folder has been observed, and left unchanged, for
        // over an hour, the next pass removes it and reports its bytes.
        age_orphan(temp.path(), &view, key);
        let second = run_pass_entry_bound(temp.path());
        assert_eq!(second.removed_roots, 1, "{second:?}");
        assert_eq!(second.orphan_views.removed_roots, 1, "{second:?}");
        assert!(second.orphan_views.removed_bytes > 0, "{second:?}");
        assert!(!view.exists());
    }

    #[test]
    fn storage_retention_orphan_walk_deadline_is_incomplete_not_protected() {
        let temp = tempfile::tempdir().unwrap();
        let key = "0123456789abcdef";
        let view = cache(temp.path(), "views", key);
        let clock = Instant::now();
        let delay = observe_test_hook(
            &view,
            Arc::new(|step| {
                if step == "walk" {
                    std::thread::sleep(Duration::from_millis(20));
                }
            }),
        );
        // Hold the lane clock before its deadline so only the real-time tree
        // walk expires, even if the runner stalls before entering the lane.
        let stopped =
            run_pass_with_budget(temp.path(), &|| false, &|| clock, Duration::from_millis(10));
        drop(delay);
        assert_eq!(stopped.examined, 1, "{stopped:?}");
        assert!(stopped.stopped_early, "{stopped:?}");
        assert_eq!(stopped.skipped_protected, 0, "{stopped:?}");
        assert_eq!(stopped.orphan_views.skipped_protected, 0, "{stopped:?}");
        assert_eq!(stopped.domains["views"].skipped_protected, 0, "{stopped:?}");
        assert_eq!(stopped.removed_roots, 0, "{stopped:?}");
        assert_eq!(stopped.errors.len(), 1, "{stopped:?}");
        assert!(stopped.errors[0].contains("retention deadline reached"));
        assert!(view.exists());

        let observed = run_pass_entry_bound(temp.path());
        assert_eq!(observed.orphan_views.skipped_recent, 1, "{observed:?}");
        age_orphan(temp.path(), &view, key);
        let reclaimed = run_pass_entry_bound(temp.path());
        assert_eq!(reclaimed.orphan_views.removed_roots, 1, "{reclaimed:?}");
        assert!(!view.exists());
    }

    #[test]
    fn storage_retention_candidate_io_timeout_is_not_a_pass_deadline() {
        let mut report = SweepReport::default();
        let error = io::Error::new(io::ErrorKind::TimedOut, "filesystem probe timed out");
        report_candidate_error(Path::new("cache"), &error, &mut report);
        assert_eq!(report.skipped_protected, 1, "{report:?}");
        assert!(!report.stopped_early, "{report:?}");
        assert_eq!(report.errors, ["cache: filesystem probe timed out"]);
    }

    #[test]
    fn storage_retention_orphan_view_registration_in_progress_is_kept() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("checkout");
        fs::create_dir_all(&root).unwrap();
        let root = fs::canonicalize(root).unwrap();
        let scope = crate::path_identity::project_scope_key(&root);
        let view = cache(temp.path(), "views", &scope);
        age_orphan(temp.path(), &view, &scope);
        // The pass loads binding history when it starts, finds none for this
        // folder, and so treats it as an orphan. While the pass is paused in
        // its walk, a registration binds the checkout root. The binding is
        // recorded under the family's artifact key (the key shared by every
        // checkout of one repository), as configure and register_view do.
        // Expected: the recheck under the barrier sees the new binding and
        // keeps the folder.
        let (worker, release) = paused_pass(temp.path(), &view, "walk");
        record_bind(temp.path(), &root, "fedcba9876543210").unwrap();
        release.send(()).unwrap();
        let report = worker.join().unwrap();
        assert_eq!(report.removed_roots, 0, "{report:?}");
        assert!(view.exists());
    }

    #[test]
    fn storage_retention_unreadable_registry_keeps_every_orphan_view() {
        let temp = tempfile::tempdir().unwrap();
        let key = "0123456789abcdef";
        let view = cache(temp.path(), "views", key);
        age_orphan(temp.path(), &view, key);
        let family = temp.path().join("blobs/v2/fedcba9876543210");
        fs::create_dir_all(&family).unwrap();
        fs::write(
            family.join(crate::views::registry::REGISTRY_FILE),
            b"not a registry database",
        )
        .unwrap();
        // Expected: a registry that cannot be read might list this folder, so
        // the folder is kept and counted as unknown, never as an orphan.
        let report = run_pass(temp.path(), &|| false);
        assert_eq!(report.removed_roots, 0, "{report:?}");
        assert_eq!(report.skipped_unknown, 1, "{report:?}");
        assert_eq!(report.orphan_views.skipped_unknown, 1, "{report:?}");
        assert!(view.exists());
    }

    #[test]
    fn storage_retention_registered_view_without_history_keeps_the_week() {
        let temp = tempfile::tempdir().unwrap();
        let scope = "0123456789abcdef";
        let registry =
            crate::views::registry::FamilyRegistry::open(temp.path(), "fedcba9876543210").unwrap();
        // Registering with a root that does not exist records no binding, so
        // the registry is the only record of this scope.
        drop(
            registry
                .register_view_with_storage(registry.storage(), scope, &temp.path().join("missing"))
                .unwrap(),
        );
        let view = cache(temp.path(), "views", scope);
        age_orphan(temp.path(), &view, scope);
        // Expected: a folder that a registry lists is never an orphan. It keeps
        // the week-long grace, so an hour-old folder is kept and the orphan
        // counts stay at zero.
        let report = run_pass(temp.path(), &|| false);
        assert_eq!(report.removed_roots, 0, "{report:?}");
        assert_eq!(report.orphan_views.examined, 0, "{report:?}");
        assert!(view.exists());
    }

    #[test]
    fn storage_retention_due_caches_do_not_wait_for_the_lexical_walk() {
        let (temp, root, scope) = fixture();
        fs::remove_dir(root).unwrap();
        // The due key is the checkout's scope: its root is deleted and its
        // binding is past the grace. A lexical batch is the set of
        // PASS_ENTRIES cache names one pass takes in name order across the
        // domains. Create more fresh view folders than one batch holds; their
        // names sort before the due key's symbols cache, so the batch never
        // reaches that cache.
        for number in 0..PASS_ENTRIES + 8 {
            cache(temp.path(), "views", &format!("{number:016x}"));
        }
        let path = cache(temp.path(), "symbols", &scope);
        // Expected: the due-first lane removes the due key's cache in this
        // same pass, without waiting for the batch to reach it.
        let report = run_pass_entry_bound(temp.path());
        assert_eq!(report.due_keys, 1, "{report:?}");
        assert_eq!(report.due_first.removed_roots, 1, "{report:?}");
        assert_eq!(report.removed_roots, 1, "{report:?}");
        assert!(!path.exists());
    }

    #[test]
    fn storage_retention_vanished_view_backlog_drains_in_two_passes() {
        const ROOTS: usize = 600;
        let (temp, root, scope) = fixture();
        let template: Binding =
            read_json(&temp.path().join(format!("retention/roots/{scope}.json"))).unwrap();
        fs::remove_dir(root).unwrap();
        for number in 0..ROOTS {
            let key = format!("{number:016x}");
            let mut binding = template.clone();
            binding.root = temp.path().join(format!("deleted-{number}"));
            binding.artifact_key = key.clone();
            atomic_json(
                &temp.path().join(format!("retention/roots/{key}.json")),
                &binding,
            )
            .unwrap();
            cache(temp.path(), "views", &key);
            cache(temp.path(), "callgraph", &key);
        }
        let remaining = || {
            (0..ROOTS)
                .filter(|number| temp.path().join(format!("views/{number:016x}")).exists())
                .count()
        };
        let mut passes = 0;
        drain_due_views(|| {
            let report = run_pass_entry_bound(temp.path());
            passes += 1;
            assert_eq!(report.due_views_remaining, remaining(), "{report:?}");
            assert!(report.due_first.examined <= DUE_FIRST_ENTRIES, "{report:?}");
            if passes == 1 {
                assert_eq!(report.due_first.removed_roots, 256, "{report:?}");
                assert_eq!(
                    remaining(),
                    ROOTS - 256 - PASS_ENTRIES,
                    "views must precede other domains"
                );
            }
            assert!(passes <= 2, "vanished views stalled: {report:?}");
            report
        });
        assert_eq!(passes, 2);
        assert_eq!(remaining(), 0);
    }

    #[test]
    fn storage_retention_cancelled_final_flush_skips_admission_and_rereads() {
        use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

        let (temp, root, scope) = fixture();
        fs::remove_dir(root).unwrap();
        let view = cache(temp.path(), "views", &scope);
        let callgraph = cache(temp.path(), "callgraph", &scope);
        let cancelled = Arc::new(AtomicBool::new(false));
        let history_reads = Arc::new(AtomicUsize::new(0));
        let owner_scans = Arc::new(AtomicUsize::new(0));
        let admissions = Arc::new(AtomicUsize::new(0));
        let walk_hook = {
            let cancelled = cancelled.clone();
            observe_test_hook(
                &view,
                Arc::new(move |step| {
                    if step == "walk" {
                        // Signal cancellation while validating the view. Preparation
                        // still queues it before the loop checks the next candidate.
                        cancelled.store(true, Ordering::SeqCst);
                    }
                }),
            )
        };
        let hook = {
            let cancelled = cancelled.clone();
            let history_reads = history_reads.clone();
            let owner_scans = owner_scans.clone();
            let admissions = admissions.clone();
            observe_test_hook(
                temp.path(),
                Arc::new(move |step| {
                    // Setup reads precede cancellation; only count final-flush work.
                    if cancelled.load(Ordering::SeqCst) {
                        match step {
                            "history-read" => {
                                history_reads.fetch_add(1, Ordering::SeqCst);
                            }
                            "owner-scan" => {
                                owner_scans.fetch_add(1, Ordering::SeqCst);
                            }
                            "due-admitted" => {
                                admissions.fetch_add(1, Ordering::SeqCst);
                            }
                            _ => {}
                        }
                    }
                }),
            )
        };
        let report = run_pass_with_budget(
            temp.path(),
            &|| cancelled.load(Ordering::SeqCst),
            &Instant::now,
            NO_TIME_BOUND,
        );
        assert!(report.cancelled, "{report:?}");
        assert!(report.errors.is_empty(), "{report:?}");
        assert_eq!(report.examined, 1, "cancel after queuing only the view");
        assert_eq!(report.removed_roots, 0, "{report:?}");
        assert_eq!(report.skipped_protected, 0, "{report:?}");
        assert_eq!(report.due_views_remaining, 1, "{report:?}");
        assert!(view.exists() && callgraph.exists());
        assert_eq!(
            (
                history_reads.load(Ordering::SeqCst),
                owner_scans.load(Ordering::SeqCst),
                admissions.load(Ordering::SeqCst),
            ),
            (0, 0, 0),
            "cancelled final flush must not reread history, scan owners, or take sweep.lock"
        );
        drop(hook);
        drop(walk_hook);
        let next = run_pass_entry_bound(temp.path());
        assert!(!next.cancelled, "{next:?}");
        assert!(next.errors.is_empty(), "{next:?}");
        assert_eq!(next.removed_roots, 2, "{next:?}");
        assert_eq!(next.due_views_remaining, 0, "{next:?}");
        assert!(!view.exists() && !callgraph.exists());
    }

    #[test]
    fn storage_retention_due_flush_rechecks_cancellation_under_barrier() {
        use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

        let (temp, root, scope) = fixture();
        fs::remove_dir(root).unwrap();
        let view = cache(temp.path(), "views", &scope);
        let cancelled = Arc::new(AtomicBool::new(false));
        let history_reads = Arc::new(AtomicUsize::new(0));
        let owner_scans = Arc::new(AtomicUsize::new(0));
        let admissions = Arc::new(AtomicUsize::new(0));
        let hook = {
            let cancelled = cancelled.clone();
            let history_reads = history_reads.clone();
            let owner_scans = owner_scans.clone();
            let admissions = admissions.clone();
            let storage = temp.path().to_path_buf();
            observe_test_hook(
                temp.path(),
                Arc::new(move |step| match step {
                    "due-admitted" => {
                        assert!(storage.join("retention/sweep.lock").exists());
                        admissions.fetch_add(1, Ordering::SeqCst);
                        cancelled.store(true, Ordering::SeqCst);
                    }
                    "history-read" if cancelled.load(Ordering::SeqCst) => {
                        history_reads.fetch_add(1, Ordering::SeqCst);
                    }
                    "owner-scan" if cancelled.load(Ordering::SeqCst) => {
                        owner_scans.fetch_add(1, Ordering::SeqCst);
                    }
                    _ => {}
                }),
            )
        };
        let report = run_pass_with_budget(
            temp.path(),
            &|| cancelled.load(Ordering::SeqCst),
            &Instant::now,
            NO_TIME_BOUND,
        );
        assert!(report.cancelled, "{report:?}");
        assert!(report.errors.is_empty(), "{report:?}");
        assert_eq!(report.removed_roots, 0, "{report:?}");
        assert_eq!(report.skipped_protected, 0, "{report:?}");
        assert_eq!(report.due_views_remaining, 1, "{report:?}");
        assert!(view.exists());
        assert_eq!(admissions.load(Ordering::SeqCst), 1);
        assert_eq!(
            (
                history_reads.load(Ordering::SeqCst),
                owner_scans.load(Ordering::SeqCst),
            ),
            (0, 0),
            "cancellation at admission must skip history and owner rereads"
        );
        drop(hook);
        let next = run_pass_entry_bound(temp.path());
        assert!(!next.cancelled, "{next:?}");
        assert!(next.errors.is_empty(), "{next:?}");
        assert_eq!(next.removed_roots, 1, "{next:?}");
        assert!(!view.exists());
    }

    #[test]
    fn storage_retention_budget_expired_final_flush_skips_admission_and_rereads() {
        use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

        let (temp, root, scope) = fixture();
        fs::remove_dir(root).unwrap();
        let view = cache(temp.path(), "views", &scope);
        let callgraph = cache(temp.path(), "callgraph", &scope);
        let expired = Arc::new(AtomicBool::new(false));
        let history_reads = Arc::new(AtomicUsize::new(0));
        let owner_scans = Arc::new(AtomicUsize::new(0));
        let admissions = Arc::new(AtomicUsize::new(0));
        let walk_hook = {
            let expired = expired.clone();
            observe_test_hook(
                &view,
                Arc::new(move |step| {
                    if step == "walk" {
                        // The injected clock expires while validation still queues
                        // the view, before the loop checks the next candidate.
                        expired.store(true, Ordering::SeqCst);
                    }
                }),
            )
        };
        let hook = {
            let expired = expired.clone();
            let history_reads = history_reads.clone();
            let owner_scans = owner_scans.clone();
            let admissions = admissions.clone();
            observe_test_hook(
                temp.path(),
                Arc::new(move |step| {
                    // Ignore setup reads; only final-flush work is past budget.
                    if expired.load(Ordering::SeqCst) {
                        match step {
                            "history-read" => {
                                history_reads.fetch_add(1, Ordering::SeqCst);
                            }
                            "owner-scan" => {
                                owner_scans.fetch_add(1, Ordering::SeqCst);
                            }
                            "due-admitted" => {
                                admissions.fetch_add(1, Ordering::SeqCst);
                            }
                            _ => {}
                        }
                    }
                }),
            )
        };
        let started = Instant::now();
        let now = || {
            started
                + if expired.load(Ordering::SeqCst) {
                    NO_TIME_BOUND * 2
                } else {
                    Duration::ZERO
                }
        };
        let report = run_pass_with_budget(temp.path(), &|| false, &now, NO_TIME_BOUND);
        assert!(report.stopped_early && !report.cancelled, "{report:?}");
        assert!(report.errors.is_empty(), "{report:?}");
        assert_eq!(report.examined, 1, "expire after queuing only the view");
        assert_eq!(report.removed_roots, 0, "{report:?}");
        assert_eq!(report.unmeasured_removed_roots, 0, "{report:?}");
        assert_eq!(report.skipped_protected, 0, "{report:?}");
        assert_eq!(report.due_views_remaining, 1, "{report:?}");
        assert!(view.exists() && callgraph.exists());
        assert_eq!(
            (
                history_reads.load(Ordering::SeqCst),
                owner_scans.load(Ordering::SeqCst),
                admissions.load(Ordering::SeqCst),
            ),
            (0, 0, 0),
            "expired final flush must not reread history, scan owners, or take sweep.lock"
        );
        drop(hook);
        drop(walk_hook);
        let next = run_pass_entry_bound(temp.path());
        assert!(!next.cancelled, "{next:?}");
        assert!(next.errors.is_empty(), "{next:?}");
        assert_eq!(next.removed_roots, 2, "{next:?}");
        assert_eq!(next.due_views_remaining, 0, "{next:?}");
        assert!(!view.exists() && !callgraph.exists());
    }

    #[test]
    fn storage_retention_budget_due_flush_rechecks_expiry_under_barrier() {
        use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

        let (temp, root, scope) = fixture();
        fs::remove_dir(root).unwrap();
        let view = cache(temp.path(), "views", &scope);
        let expired = Arc::new(AtomicBool::new(false));
        let history_reads = Arc::new(AtomicUsize::new(0));
        let owner_scans = Arc::new(AtomicUsize::new(0));
        let admissions = Arc::new(AtomicUsize::new(0));
        let hook = {
            let expired = expired.clone();
            let history_reads = history_reads.clone();
            let owner_scans = owner_scans.clone();
            let admissions = admissions.clone();
            let storage = temp.path().to_path_buf();
            observe_test_hook(
                temp.path(),
                Arc::new(move |step| match step {
                    "due-admitted" => {
                        assert!(storage.join("retention/sweep.lock").exists());
                        admissions.fetch_add(1, Ordering::SeqCst);
                        expired.store(true, Ordering::SeqCst);
                    }
                    "history-read" if expired.load(Ordering::SeqCst) => {
                        history_reads.fetch_add(1, Ordering::SeqCst);
                    }
                    "owner-scan" if expired.load(Ordering::SeqCst) => {
                        owner_scans.fetch_add(1, Ordering::SeqCst);
                    }
                    _ => {}
                }),
            )
        };
        let started = Instant::now();
        let now = || {
            started
                + if expired.load(Ordering::SeqCst) {
                    NO_TIME_BOUND * 2
                } else {
                    Duration::ZERO
                }
        };
        let report = run_pass_with_budget(temp.path(), &|| false, &now, NO_TIME_BOUND);
        assert!(report.stopped_early && !report.cancelled, "{report:?}");
        assert!(report.errors.is_empty(), "{report:?}");
        assert_eq!(report.removed_roots, 0, "{report:?}");
        assert_eq!(report.unmeasured_removed_roots, 0, "{report:?}");
        assert_eq!(report.skipped_protected, 0, "{report:?}");
        assert_eq!(report.due_views_remaining, 1, "{report:?}");
        assert!(view.exists());
        assert_eq!(admissions.load(Ordering::SeqCst), 1);
        assert_eq!(
            (
                history_reads.load(Ordering::SeqCst),
                owner_scans.load(Ordering::SeqCst),
            ),
            (0, 0),
            "expiry at admission must skip history and owner rereads"
        );
        drop(hook);
        let next = run_pass_entry_bound(temp.path());
        assert!(next.errors.is_empty(), "{next:?}");
        assert_eq!(next.removed_roots, 1, "{next:?}");
        assert!(!view.exists());
    }

    #[test]
    fn storage_retention_budget_due_flush_stops_retiring_between_items() {
        use std::sync::atomic::{AtomicBool, Ordering};

        let (temp, root, scope) = fixture();
        fs::remove_dir(root).unwrap();
        let view = cache(temp.path(), "views", &scope);
        let callgraph = cache(temp.path(), "callgraph", &scope);
        let expired = Arc::new(AtomicBool::new(false));
        let hook = {
            let expired = expired.clone();
            observe_test_hook(
                &view,
                Arc::new(move |step| {
                    if step == "rename-root" {
                        // Let the first retirement finish, but exhaust the budget
                        // before the next queued cache can be retired.
                        expired.store(true, Ordering::SeqCst);
                    }
                }),
            )
        };
        let started = Instant::now();
        let now = || {
            started
                + if expired.load(Ordering::SeqCst) {
                    NO_TIME_BOUND * 2
                } else {
                    Duration::ZERO
                }
        };
        let report = run_pass_with_budget(temp.path(), &|| false, &now, NO_TIME_BOUND);
        assert!(
            expired.load(Ordering::SeqCst),
            "first retirement was not reached"
        );
        assert!(report.errors.is_empty(), "{report:?}");
        assert_eq!(report.removed_roots, 1, "{report:?}");
        assert!(report.stopped_early && !report.cancelled, "{report:?}");
        assert_eq!(report.unmeasured_removed_roots, 1, "{report:?}");
        assert_eq!(report.due_first.removed_roots, 1, "{report:?}");
        assert_eq!(report.domains["views"].removed_roots, 1, "{report:?}");
        assert_eq!(report.domains["callgraph"].removed_roots, 0, "{report:?}");
        assert_eq!(report.skipped_protected, 0, "{report:?}");
        assert_eq!(report.due_views_remaining, 0, "{report:?}");
        assert!(!view.exists() && callgraph.exists());
        drop(hook);
        let next = run_pass_entry_bound(temp.path());
        assert!(next.errors.is_empty(), "{next:?}");
        assert_eq!(next.removed_roots, 1, "{next:?}");
        assert!(!callgraph.exists());
    }

    /// A drain burst over a large backlog. Every removal rechecks binding
    /// history under the retention barrier, and that history is every binding
    /// record in the storage. The recheck must not reread all of it for each
    /// removed cache: even hundreds of records make repeated file reads costly,
    /// and a burst runs pass after pass.
    #[test]
    fn storage_retention_due_drain_rereads_history_per_batch_not_per_cache() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        // More than one pass and several admission batches still distinguish
        // per-batch history reads from a reread for every removed cache.
        const DUE: usize = 96;
        const LIVE: usize = 128;
        let (temp, root, scope) = fixture();
        let template: Binding =
            read_json(&temp.path().join(format!("retention/roots/{scope}.json"))).unwrap();
        fs::remove_dir(root).unwrap();
        // Checkouts live outside the storage folder, as they do in production.
        let checkouts = tempfile::tempdir().unwrap();
        for number in 0..DUE + LIVE {
            let key = format!("{number:016x}");
            let mut binding = template.clone();
            binding.artifact_key = key.clone();
            if number < DUE {
                binding.root = checkouts.path().join(format!("deleted-{number}"));
                cache(temp.path(), "views", &key);
                cache(temp.path(), "callgraph", &key);
            } else {
                // Bindings whose checkout still exists: every admission
                // recheck reads them, and retention must never remove them.
                let live = checkouts.path().join(format!("live-{number}"));
                fs::create_dir(&live).unwrap();
                binding.root = live;
            }
            atomic_json(
                &temp.path().join(format!("retention/roots/{key}.json")),
                &binding,
            )
            .unwrap();
        }
        let history_reads = Arc::new(AtomicUsize::new(0));
        let owner_scans = Arc::new(AtomicUsize::new(0));
        let _hook = {
            let history_reads = history_reads.clone();
            let owner_scans = owner_scans.clone();
            observe_test_hook(
                temp.path(),
                Arc::new(move |step| match step {
                    "history-read" => {
                        history_reads.fetch_add(1, Ordering::Relaxed);
                    }
                    "owner-scan" => {
                        owner_scans.fetch_add(1, Ordering::Relaxed);
                    }
                    _ => {}
                }),
            )
        };
        let started = Instant::now();
        let mut passes = Vec::new();
        drain_due_views(|| {
            let reads_before = history_reads.load(Ordering::Relaxed);
            let scans_before = owner_scans.load(Ordering::Relaxed);
            let report = run_pass_entry_bound(temp.path());
            let reads = history_reads.load(Ordering::Relaxed) - reads_before;
            let scans = owner_scans.load(Ordering::Relaxed) - scans_before;
            eprintln!(
                "retention drain pass={} removed_roots={} examined={} due_first_examined={} due_views_remaining={} history_reads={reads} owner_scans={scans} errors={:?} timings_ms={:?}",
                passes.len() + 1,
                report.removed_roots,
                report.examined,
                report.due_first.examined,
                report.due_views_remaining,
                report.errors,
                report.timings_ms
            );
            passes.push((
                report.removed_roots,
                reads,
                scans,
                report.due_views_remaining,
            ));
            report
        });
        let removed: usize = passes.iter().map(|(removed, ..)| removed).sum();
        eprintln!(
            "retention drain due={DUE} live={LIVE} passes={} removed={removed} elapsed_ms={}",
            passes.len(),
            started.elapsed().as_millis()
        );
        assert_eq!(
            passes.last().map(|(.., remaining)| *remaining),
            Some(0),
            "the burst left due views: {passes:?}"
        );
        assert!(removed >= DUE, "the burst removed too little: {passes:?}");
        for (removed, reads, scans, _) in &passes {
            // One read at pass setup, and one per admission batch of removals
            // (each lane flushes its own partial batch, hence the slack).
            let batches = removed.div_ceil(ADMISSION_BATCH) + 2;
            assert!(
                *reads <= 1 + batches,
                "history reread per removal: removed={removed} reads={reads}"
            );
            assert!(
                *scans <= 1 + batches,
                "owners rescanned per removal: removed={removed} scans={scans}"
            );
        }
    }

    #[test]
    fn storage_retention_backlog_burst_stops_without_progress_or_on_cancellation() {
        for cancelled in [false, true] {
            let mut passes = 0;
            drain_due_views(|| {
                passes += 1;
                SweepReport {
                    due_views_remaining: 3,
                    cancelled,
                    ..Default::default()
                }
            });
            assert_eq!(passes, 1);
        }
    }

    #[test]
    fn storage_retention_large_due_backlog_drains_within_budget() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        const ROOTS: usize = 256;
        const FILES: usize = 64;
        const MIN_REMOVED: usize = 32;
        let (temp, root, scope) = fixture();
        let template: Binding =
            read_json(&temp.path().join(format!("retention/roots/{scope}.json"))).unwrap();
        fs::remove_dir(root).unwrap();
        let measurements = Arc::new(AtomicUsize::new(0));
        let walks = Arc::new(AtomicUsize::new(0));
        let retirements = Arc::new(AtomicUsize::new(0));
        let cursor_commits = Arc::new(AtomicUsize::new(0));
        let mut hooks = Vec::new();
        for number in 0..ROOTS {
            let key = format!("{number:016x}");
            let mut binding = template.clone();
            binding.root = temp.path().join(format!("deleted-{number}"));
            binding.artifact_key = key.clone();
            atomic_json(
                &temp.path().join(format!("retention/roots/{key}.json")),
                &binding,
            )
            .unwrap();
            let path = cache(temp.path(), "views", &key);
            for file in 0..FILES {
                // File count exercises traversal cost; small payloads also work
                // on filesystems that allocate the full length of an extended file.
                fs::write(path.join(format!("payload-{file}.bin")), b"rebuildable").unwrap();
            }
            let measurements = measurements.clone();
            let walks = walks.clone();
            let retirements = retirements.clone();
            hooks.push(observe_test_hook(
                &path,
                Arc::new(move |step| {
                    if step == "measure-file" {
                        measurements.fetch_add(1, Ordering::Relaxed);
                    } else if step == "walk" {
                        walks.fetch_add(1, Ordering::Relaxed);
                    } else if step == "rename-root" {
                        retirements.fetch_add(1, Ordering::Relaxed);
                    }
                }),
            ));
        }
        let commits = cursor_commits.clone();
        hooks.push(observe_test_hook(
            temp.path(),
            Arc::new(move |step| {
                if step == "cursor-commit" {
                    commits.fetch_add(1, Ordering::Relaxed);
                }
            }),
        ));
        let orphan_key = "ffffffffffffffff";
        let orphan = cache(temp.path(), "views", orphan_key);
        age_orphan(temp.path(), &orphan, orphan_key);
        let run_bounded_pass = || {
            let clock_start = Instant::now();
            let retired_before = retirements.load(Ordering::Relaxed);
            let clock = || {
                if retirements.load(Ordering::Relaxed) - retired_before < MIN_REMOVED {
                    clock_start
                } else {
                    // Expire the due lane after MIN_REMOVED retirements, not
                    // clock probes: admission and inter-item checks also read it.
                    // Stay before the orphan lane's three-quarter cutoff and
                    // the full-budget cutoff of the lexical traversal.
                    clock_start + NO_TIME_BOUND * 5 / 8
                }
            };
            // Inner safety walks still use wall time. Their generous guard is
            // independent of the logical clock that bounds the candidate count.
            run_pass_with_budget(temp.path(), &|| false, &clock, NO_TIME_BOUND)
        };
        let report = run_bounded_pass();
        let remaining = (0..ROOTS)
            .filter(|number| temp.path().join(format!("views/{number:016x}")).exists())
            .count();
        eprintln!("retention fixture roots={ROOTS} files_per_root={FILES} logical_due_quota={MIN_REMOVED} measured_files={} walks={} cursor_commits={} due_removed={} remaining={} orphan_examined={} errors={:?}", measurements.load(Ordering::Relaxed), walks.load(Ordering::Relaxed), cursor_commits.load(Ordering::Relaxed), report.due_first.removed_roots, remaining, report.orphan_views.examined, report.errors);
        assert_eq!(
            report.due_first.removed_roots, MIN_REMOVED,
            "due backlog did not drain: {report:?}"
        );
        assert_eq!(
            remaining,
            ROOTS - MIN_REMOVED - PASS_ENTRIES,
            "backlog did not shrink"
        );
        assert_eq!(
            report.orphan_views.examined, 1,
            "orphan lane starved: {report:?}"
        );
        assert_eq!(
            measurements.load(Ordering::Relaxed),
            0,
            "due roots were byte-measured"
        );
        assert_eq!(
            cursor_commits.load(Ordering::Relaxed),
            2,
            "cursor was synced per entry"
        );
        let second = run_bounded_pass();
        let second_remaining = (0..ROOTS)
            .filter(|number| temp.path().join(format!("views/{number:016x}")).exists())
            .count();
        eprintln!(
            "retention second pass due_removed={} remaining={second_remaining}",
            second.due_first.removed_roots
        );
        assert_eq!(
            second.due_first.removed_roots, MIN_REMOVED,
            "second pass stalled: {second:?}"
        );
        assert_eq!(
            second_remaining,
            remaining - MIN_REMOVED - PASS_ENTRIES,
            "second pass did not shrink backlog"
        );
        assert_eq!(
            measurements.load(Ordering::Relaxed),
            0,
            "second pass byte-measured due roots"
        );
        assert_eq!(
            cursor_commits.load(Ordering::Relaxed),
            4,
            "second pass did not commit each batch once"
        );
    }

    #[test]
    fn storage_retention_legacy_binding_of_a_present_worktree_is_upgraded() {
        let (temp, root, scope) = bound_checkout_fixture(true, 11, true, false);
        let record_path = temp.path().join(format!("retention/roots/{scope}.json"));
        let legacy: serde_json::Value = read_json(&record_path).unwrap();
        let report = run_pass(temp.path(), &|| false);
        assert_eq!(report.upgraded_bindings, 1, "{report:?}");
        let record: serde_json::Value = read_json(&record_path).unwrap();
        assert_eq!(record["linked_worktree"], true);
        assert_eq!(record["last_bound_ms"], legacy["last_bound_ms"]);
        let path = cache(temp.path(), "views", &scope);
        fs::remove_dir_all(&root).unwrap();
        let report = run_pass(temp.path(), &|| false);
        assert_eq!(report.removed_roots, 1, "{report:?}");
        assert!(!path.exists());
    }

    #[test]
    fn storage_retention_v2_recent_missing_root_survives_repeated_sweeps() {
        let (temp, root, scope) = fixture();
        let registry = crate::views::registry::FamilyRegistry::open(temp.path(), &scope).unwrap();
        let view = registry
            .register_view_with_storage(registry.storage(), &scope, &root)
            .unwrap();
        let path = view.view_dir().to_path_buf();
        drop(view);
        fs::remove_dir(root).unwrap();
        for _ in 0..3 {
            crate::gc::family::sweep_family(
                &registry,
                None,
                crate::gc::family::FamilySweepPolicy { byte_budget: 0 },
                None,
            )
            .unwrap();
        }
        assert!(registry.member(&scope).unwrap().is_some());
        assert!(path.exists());
    }

    #[test]
    fn v2_mount_mismatch_is_counted_and_keeps_member() {
        let (temp, root, scope) = fixture();
        let registry = crate::views::registry::FamilyRegistry::open(temp.path(), &scope).unwrap();
        let view = registry
            .register_view_with_storage(registry.storage(), &scope, &root)
            .unwrap();
        let path = view.view_dir().to_path_buf();
        drop(view);
        registry
            .with_barrier(|tx| {
                tx.execute("UPDATE members SET last_bind_ms = 0", [])?;
                Ok(())
            })
            .unwrap();
        let record = temp.path().join(format!("retention/roots/{scope}.json"));
        let mut binding: Binding = read_json(&record).unwrap();
        binding.last_bound_ms = 0;
        binding.volume = Some((temp.path().join("Volumes/X"), u64::MAX));
        binding.stable_volume = None;
        atomic_json(&record, &binding).unwrap();
        fs::remove_dir(root).unwrap();
        let report = crate::gc::family::sweep_family(
            &registry,
            None,
            crate::gc::family::FamilySweepPolicy { byte_budget: 0 },
            None,
        )
        .unwrap();
        assert_eq!(report.examined_roots, 1);
        assert_eq!(report.skipped_mount_mismatch, 1, "{report:?}");
        assert_eq!(report.skipped_recent, 0);
        assert!(registry.member(&scope).unwrap().is_some());
        assert!(path.exists());
    }

    #[test]
    fn prune_bindings_isolates_truncated_and_unknown_version_records() {
        let storage = tempfile::tempdir().unwrap();
        let roots = storage.path().join("retention/roots");
        fs::create_dir_all(&roots).unwrap();

        let truncated_scope = "truncated-scope";
        let truncated_path = roots.join(format!("{truncated_scope}.json"));
        let truncated_bytes = br#"{"root":"/unfinished"#;
        fs::write(&truncated_path, truncated_bytes).unwrap();

        let future_scope = "future-scope";
        let future_path = roots.join(format!("{future_scope}.json"));
        let future_bytes = serde_json::to_vec(&serde_json::json!({
            "schema_version": 999,
            "root": storage.path().join("future-missing-root"),
            "artifact_key": "future-key",
            "last_bound_ms": 0,
        }))
        .unwrap();
        fs::write(&future_path, &future_bytes).unwrap();

        let eligible_scope = "eligible-scope";
        let eligible_path = roots.join(format!("{eligible_scope}.json"));
        let anchor = storage.path().join("anchor");
        fs::create_dir(&anchor).unwrap();
        let _volume =
            test_volume_uuid_guard(storage.path(), &anchor, "retention-test-volume").unwrap();
        atomic_json(
            &eligible_path,
            &Binding {
                root: storage.path().join("missing-root"),
                artifact_key: "x".into(),
                last_bound_ms: 0,
                volume: Some((anchor, 0)),
                stable_volume: Some(VolumeIdentity {
                    uuid: Some("retention-test-volume".into()),
                    boot_id: None,
                }),
                linked_worktree: false,
            },
        )
        .unwrap();

        let mut report = SweepReport {
            at_ms: u64::MAX,
            ..Default::default()
        };
        let (result, lines) = crate::logging::capture_log_lines(|| {
            prune_bindings(&StoragePaths::same(storage.path()), &|| false, &mut report)
        });

        assert!(
            result.is_ok(),
            "one bad binding aborted pruning: {result:?}"
        );
        assert_eq!(report.examined_bindings, 3);
        assert_eq!(report.skipped_unknown, 2);
        assert_eq!(report.pruned_bindings, 1);
        assert!(
            !eligible_path.exists(),
            "a later valid binding was not pruned"
        );
        assert_eq!(fs::read(&future_path).unwrap(), future_bytes);
        let quarantine = bounded_dirs(
            &storage.path().join("retention/quarantine/roots"),
            HISTORY_ENTRIES,
        )
        .unwrap();
        assert_eq!(quarantine.len(), 1, "truncated binding was not quarantined");
        let quarantined_path = quarantine[0].path();
        assert_eq!(fs::read(&quarantined_path).unwrap(), truncated_bytes);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(&quarantined_path)
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o077,
                0,
                "truncated binding quarantine file is not owner-only"
            );
        }
        assert_eq!(
            lines
                .iter()
                .filter(|line| line.contains("unreadable root binding"))
                .count(),
            2,
            "each unknown binding should be logged once: {lines:?}"
        );

        let history = bindings(storage.path()).unwrap();
        assert!(history
            .get(truncated_scope)
            .is_some_and(|records| { records.iter().any(|binding| binding.root == roots) }));
        assert!(history
            .get(future_scope)
            .is_some_and(|records| { records.iter().any(|binding| binding.root == roots) }));
    }

    #[test]
    fn truncated_binding_quarantine_stays_bounded_across_repair_cycles() {
        let storage = tempfile::tempdir().unwrap();
        let roots = storage.path().join("retention/roots");
        fs::create_dir_all(&roots).unwrap();
        let broken_scope = "repaired-scope";
        let broken_path = roots.join(format!("{broken_scope}.json"));
        let live_root = storage.path().join("live-root");
        let anchor = storage.path().join("volume-anchor");
        fs::create_dir(&live_root).unwrap();
        fs::create_dir(&anchor).unwrap();
        let _volume =
            test_volume_uuid_guard(storage.path(), &anchor, "retention-cycle-volume").unwrap();

        for cycle in 0..24 {
            fs::write(&broken_path, br#"{"root":"/unfinished"#).unwrap();
            let eligible_path = roots.join(format!("eligible-{cycle}.json"));
            atomic_json(
                &eligible_path,
                &Binding {
                    root: storage.path().join(format!("missing-{cycle}")),
                    artifact_key: "x".into(),
                    last_bound_ms: 0,
                    volume: Some((anchor.clone(), 0)),
                    stable_volume: Some(VolumeIdentity {
                        uuid: Some("retention-cycle-volume".into()),
                        boot_id: None,
                    }),
                    linked_worktree: false,
                },
            )
            .unwrap();
            let mut report = SweepReport {
                at_ms: u64::MAX,
                ..Default::default()
            };
            prune_bindings(&StoragePaths::same(storage.path()), &|| false, &mut report).unwrap();
            assert!(!eligible_path.exists(), "cleanup stalled in cycle {cycle}");
            assert_eq!(report.skipped_unknown, 1);

            let quarantine = bounded_dirs(
                &storage.path().join("retention/quarantine/roots"),
                HISTORY_ENTRIES,
            )
            .unwrap();
            assert_eq!(
                quarantine.len(),
                1,
                "quarantine history grew during repair cycle {cycle}: {:?}",
                quarantine
                    .iter()
                    .map(|entry| entry.file_name())
                    .collect::<Vec<_>>()
            );

            atomic_json(
                &broken_path,
                &Binding {
                    root: live_root.clone(),
                    artifact_key: "x".into(),
                    last_bound_ms: crate::pins::now_ms(),
                    volume: Some((anchor.clone(), 0)),
                    stable_volume: Some(VolumeIdentity {
                        uuid: Some("retention-cycle-volume".into()),
                        boot_id: None,
                    }),
                    linked_worktree: false,
                },
            )
            .unwrap();
            let history = bindings(storage.path()).unwrap();
            assert!(history.get(broken_scope).is_some_and(|records| {
                records.iter().any(|binding| binding.root == live_root)
            }));
        }
    }
}

fn collect_one_family(
    storage: &StoragePaths,
    cancelled: &dyn Fn() -> bool,
    report: &mut SweepReport,
) -> io::Result<()> {
    visit_one_family(storage, "blobs", "families-v1", cancelled, |entry| {
        let family = entry.file_name().to_string_lossy().into_owned();
        if !key_valid(&family) || !entry.file_type()?.is_dir() || cancelled() {
            return Ok(());
        }
        let swept = crate::gc::sweep_bounded(
            crate::gc::SweepRequest {
                durable_storage: &storage.durable,
                storage: &storage.artifacts,
                family: &family,
                view_dir: &storage.artifacts.join("views/retention-no-view"),
                byte_budget: 0,
                now_ms: report.at_ms,
                references: Default::default(),
            },
            cancelled,
        )
        .map_err(|error| io::Error::other(error.to_string()))?;
        report.examined_blobs += swept.examined_blobs;
        report.removed_blobs += swept.deleted_blobs;
        report.removed_blob_payload_bytes += swept.deleted_bytes;
        report.blob_reclaim_deferred |= swept.reclaim_deferred;
        report
            .domains
            .entry("blobs".to_owned())
            .or_default()
            .add(&DomainReport {
                examined: swept.examined_blobs,
                removed_bytes: swept.deleted_bytes,
                ..Default::default()
            });
        Ok(())
    })
}

// Forget history only after its checkout-private caches and registry member
// have gone. A shared family still in use retains its own live owner's record.
fn family_owner_checkout_live(storage: &StoragePaths, key: &str) -> bool {
    if !key_valid(key) {
        return false;
    }
    let path = storage
        .durable
        .join("artifact-owners")
        .join(key)
        .join("owner.json");
    read_json::<crate::artifact_owner::ArtifactOwnerManifest>(&path)
        .is_ok_and(|owner| Path::new(&owner.checkout_path).exists())
}

fn prune_bindings(
    storage: &StoragePaths,
    cancelled: &dyn Fn() -> bool,
    report: &mut SweepReport,
) -> io::Result<()> {
    static RECORDS: OnceLock<Mutex<HashMap<PathBuf, ReadDir>>> = OnceLock::new();
    let mut records = RECORDS
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if !records.contains_key(&storage.artifacts) {
        match fs::read_dir(storage.durable.join("retention/roots")) {
            Ok(entries) => {
                records.insert(storage.artifacts.clone(), entries);
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error),
        }
    }
    let batch = records
        .get_mut(&storage.artifacts)
        .map(|entries| entries.by_ref().take(32).collect::<io::Result<Vec<_>>>())
        .transpose()?
        .unwrap_or_default();
    if batch.len() < 32 {
        records.remove(&storage.artifacts);
    }
    drop(records);
    for entry in batch {
        if cancelled() {
            break;
        }
        report.examined_bindings += 1;
        if entry
            .path()
            .extension()
            .is_none_or(|extension| extension != "json")
        {
            continue;
        }
        let path = entry.path();
        let (record, binding) = match read_binding_record(&path) {
            Ok(record) => record,
            Err(error) => {
                let message = error.error.to_string();
                handle_binding_read_error(&storage.durable, &path, error, true, false);
                report.skipped_unknown += 1;
                if report.errors.len() < 16 {
                    report
                        .errors
                        .push(format!("{}: binding skipped: {message}", path.display()));
                }
                continue;
            }
        };
        if record.get("linked_worktree").is_none() && matches!(binding.root.try_exists(), Ok(true))
        {
            report.upgraded_bindings += usize::from(upgrade_legacy_binding(storage, &path)?);
            continue;
        }
        if !missing_and_old(&binding, report.at_ms) {
            continue;
        }
        let scope = crate::path_identity::project_scope_key(&binding.root);
        if DOMAINS
            .iter()
            .any(|domain| storage.artifacts.join(domain).join(&scope).exists())
            || storage.artifacts.join("views/v2").join(&scope).exists()
        {
            continue;
        }
        if crate::root_cache::protected_read_marker_exists(
            &storage.durable.join("retention"),
            &scope,
        ) {
            continue;
        }
        if key_valid(&binding.artifact_key) {
            let family_live = family_owner_checkout_live(storage, &binding.artifact_key);
            if !family_live
                && DOMAINS.iter().any(|domain| {
                    storage
                        .artifacts
                        .join(domain)
                        .join(&binding.artifact_key)
                        .exists()
                })
            {
                continue;
            }
            if let Some(registry) = crate::views::registry::FamilyRegistry::open_existing(
                &storage.artifacts,
                &binding.artifact_key,
            )
            .map_err(|error| io::Error::other(error.to_string()))?
            {
                if registry
                    .members_bounded(HISTORY_ENTRIES)
                    .map_err(|error| io::Error::other(error.to_string()))?
                    .iter()
                    .any(|member| {
                        member.root.as_ref().is_some_and(|root| {
                            crate::root_cache::canonical_root(root) == binding.root
                        })
                    })
                {
                    continue;
                }
            }
        }
        let mut legacy_present = false;
        for (directory, flat) in domain_paths(storage)? {
            if !flat {
                continue;
            }
            if bounded_dirs(&directory, TREE_ENTRIES)?.iter().any(|entry| {
                let name = entry.file_name().to_string_lossy().into_owned();
                name.starts_with(&format!("{scope}."))
                    || (key_valid(&binding.artifact_key)
                        && name.starts_with(&format!("{}.", binding.artifact_key)))
            }) {
                legacy_present = true;
                break;
            }
        }
        if legacy_present {
            continue;
        }
        let _barrier = cache_barrier(storage)?;
        // A rebind can replace the record while the cache inventory is being
        // read. Never erase that newer observation or its root's history.
        let family_live = family_owner_checkout_live(storage, &binding.artifact_key);
        let current_binding = match read_binding_record(&path) {
            Ok((_, current_binding)) => current_binding,
            Err(error) => {
                let message = error.error.to_string();
                handle_binding_read_error(&storage.durable, &path, error, true, true);
                report.skipped_unknown += 1;
                if report.errors.len() < 16 {
                    report.errors.push(format!(
                        "{}: binding changed unreadably: {message}",
                        path.display()
                    ));
                }
                continue;
            }
        };
        if current_binding != binding
            || DOMAINS.iter().any(|domain| {
                storage.artifacts.join(domain).join(&scope).exists()
                    || (!family_live
                        && storage
                            .artifacts
                            .join(domain)
                            .join(&binding.artifact_key)
                            .exists())
            })
            || storage.artifacts.join("views/v2").join(&scope).exists()
            || crate::root_cache::protected_read_marker_exists(
                &storage.durable.join("retention"),
                &scope,
            )
        {
            continue;
        }
        fs::remove_file(entry.path())?;
        report.pruned_bindings += 1;
    }
    Ok(())
}

/// A record written before linked-worktree topology was recorded falls back
/// to the week-long grace, because the topology can no longer be proven once
/// the checkout is gone. While the checkout still exists, record the verified
/// topology in place. The bind clock and volume identity stay as recorded.
fn upgrade_legacy_binding(storage: &StoragePaths, path: &Path) -> io::Result<bool> {
    let _barrier = barrier(&storage.durable)?;
    let record: serde_json::Value = read_json(path)?;
    if record.get("linked_worktree").is_some() {
        return Ok(false);
    }
    let mut binding: Binding = serde_json::from_value(record).map_err(io::Error::other)?;
    if !matches!(binding.root.try_exists(), Ok(true)) {
        return Ok(false);
    }
    binding.linked_worktree = linked_worktree_at_bind(&binding.root);
    atomic_json(path, &binding)?;
    Ok(true)
}

pub(crate) fn schedule(
    storage: StoragePaths,
    admission: SubcLifecycleAdmission,
    generation: Arc<AtomicU64>,
    expected: u64,
) {
    if crate::root_cache::check_cache_access(&storage.artifacts).is_err() {
        return;
    }
    if !admission.is_current(&generation, expected) {
        return;
    }
    if !startup_sweeps_ready(&storage.artifacts) {
        return;
    }
    {
        let mut states = states()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let state = states.entry(storage.artifacts.clone()).or_default();
        if state.running || state.last_run.is_some_and(|last| last.elapsed() < INTERVAL) {
            return;
        }
        state.running = true;
        state.last_run = Some(Instant::now());
        let admission = admission.clone();
        let generation = generation.clone();
        state.reaper_cancelled = Some(Arc::new(move || {
            !admission.is_current(&generation, expected)
        }));
    }
    let key = storage.clone();
    if let Err(error) = std::thread::Builder::new()
        .name("aft-storage-retention".into())
        .spawn(move || {
            scheduled_run(&storage.artifacts, || {
                drain_due_views(|| {
                    let report =
                        run_pass(&storage, &|| !admission.is_current(&generation, expected));
                    crate::slog_info!(
                        "storage retention root={} report={}",
                        storage.artifacts.display(),
                        serde_json::to_string(&report).unwrap_or_default()
                    );
                    report
                });
            });
        })
    {
        drop(RunningReset {
            storage: &key.artifacts,
            completed: false,
        });
        crate::slog_warn!("storage retention worker could not start: {error}");
    }
}

/// Each pass keeps its own entry and time bounds. A shrinking vanished-view
/// backlog does not wait ten minutes between passes; protected or unreadable
/// views stop the burst rather than spinning on an uncollectable directory.
fn drain_due_views(mut pass: impl FnMut() -> SweepReport) {
    loop {
        let report = pass();
        if report.cancelled
            || report.due_views_remaining == 0
            || report
                .domains
                .get("views")
                .is_none_or(|views| views.unmeasured_removed_roots == 0)
        {
            break;
        }
    }
}

fn scheduled_run(storage: &Path, run: impl FnOnce()) {
    let mut reset = RunningReset {
        storage,
        completed: false,
    };
    run();
    reset.completed = true;
}

struct RunningReset<'a> {
    storage: &'a Path,
    completed: bool,
}

impl Drop for RunningReset<'_> {
    fn drop(&mut self) {
        let mut states = states()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(state) = states.get_mut(self.storage) {
            state.running = false;
            if !self.completed {
                state.last_run = None;
            }
        }
    }
}

fn clear_unknown_observation(storage: &StoragePaths, key: &str) -> io::Result<()> {
    if !key_valid(key) {
        return Ok(());
    }
    let path = storage
        .durable
        .join("retention/unknown")
        .join(format!("{key}.json"));
    match fs::remove_file(&path) {
        Ok(()) => crate::durability::sync_dir(path.parent().unwrap()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

fn cache_key_present(storage: &StoragePaths, key: &str, deadline: Instant) -> io::Result<bool> {
    #[cfg(test)]
    test_hook(&storage.artifacts, "cache-presence");
    for (directory, flat) in domain_paths(storage)? {
        if !flat {
            let cache = directory.join(key);
            if cache.try_exists()? && !emptied_mount_point(&cache) {
                return Ok(true);
            }
        } else if directory.try_exists()? {
            let prefix = format!("{key}.");
            for (path, _) in tree_files(&directory, deadline)? {
                if path.parent() == Some(directory.as_path())
                    && path
                        .file_name()
                        .is_some_and(|name| name.to_string_lossy().starts_with(&prefix))
                {
                    return Ok(true);
                }
            }
        }
    }
    Ok(false)
}

#[cfg(test)]
mod v3_collector_tests {
    use super::*;
    use crate::blob_store::v2::{FamilyKey, FamilyPlane};
    use crate::blob_store::v3::{self, RegistryWriter, ReservationLock};
    use std::sync::atomic::{AtomicBool, Ordering};

    const FAMILY: &str = "0123456789abcdef";

    fn plane_key() -> FamilyKey {
        FamilyKey::new(FamilyPlane::Semantic, [9; 32])
    }

    /// Creates the v3 registry in a child process. Publication reads a
    /// process-global free-space override that the `blob_store::v3` tests
    /// change under their own lock; a child keeps these tests out of that.
    fn seed(storage: &Path, members: &[(&str, &str, Option<&Path>)]) {
        seed_with_durable(storage, None, members);
    }

    /// Seed a family under `storage`. With `durable`, `storage` is a prepared
    /// `storage.cache_dir` cache root owned by that durable root, which the
    /// child prepares too before it writes there.
    fn seed_with_durable(
        storage: &Path,
        durable: Option<&Path>,
        members: &[(&str, &str, Option<&Path>)],
    ) {
        let spec = members
            .iter()
            .map(|(scope, kind, root)| {
                format!(
                    "{scope}\t{kind}\t{}",
                    root.map(|root| root.to_string_lossy().into_owned())
                        .unwrap_or_default()
                )
            })
            .collect::<Vec<_>>()
            .join("\n");
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "storage_retention::v3_collector_tests::seed_child",
                "--nocapture",
                "--test-threads=1",
            ])
            .env("AFT_V3_COLLECTOR_STORAGE", storage)
            .env(
                "AFT_V3_COLLECTOR_DURABLE",
                durable.map(Path::as_os_str).unwrap_or_default(),
            )
            .env("AFT_V3_COLLECTOR_MEMBERS", spec)
            .env("RUSTC_WRAPPER", "")
            .env("CARGO_BUILD_RUSTC_WRAPPER", "")
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
        assert!(
            storage
                .join("blobs/v3")
                .join(FAMILY)
                .join("members.sqlite")
                .exists(),
            "{}",
            String::from_utf8_lossy(&output.stdout)
        );
    }

    #[test]
    fn seed_child() {
        let Some(storage) = std::env::var_os("AFT_V3_COLLECTOR_STORAGE") else {
            return;
        };
        let storage = PathBuf::from(storage);
        let members = std::env::var("AFT_V3_COLLECTOR_MEMBERS").unwrap();
        // A cache root is admitted for writes only once this process has
        // prepared it, as the AFT process that publishes v3 data would have.
        let _prepared = std::env::var_os("AFT_V3_COLLECTOR_DURABLE")
            .filter(|durable| !durable.is_empty())
            .map(|durable| {
                let mut config = crate::config::Config {
                    storage_dir: Some(PathBuf::from(durable)),
                    cache_dir: storage.parent().map(Path::to_path_buf),
                    ..Default::default()
                };
                let warning = crate::root_cache::prepare_cache_root_for_test(&mut config);
                assert!(warning.is_none(), "{warning:?}");
                config
            });
        v3::set_free_space_for_test(u64::MAX, u64::MAX / 2);
        v3::publish(&storage, FAMILY, 1 << 20, None, |publication| {
            for line in members.lines() {
                let mut fields = line.split('\t');
                let scope = fields.next().unwrap();
                let kind = fields.next().unwrap();
                let root = fields.next().filter(|root| !root.is_empty()).map(Path::new);
                publication.registry.register_member(scope, kind, root)?;
            }
            publication.put(&plane_key(), b"v3 plane payload", 1)
        })
        .unwrap();
    }

    fn with_registry<T>(
        storage: &Path,
        body: impl FnOnce(&rusqlite::Transaction<'_>) -> v3::Result<T>,
    ) -> T {
        let lock = ReservationLock::acquire(storage).unwrap();
        let mut registry = RegistryWriter::open_existing(storage, FAMILY, &lock).unwrap();
        registry.with_barrier(body).unwrap()
    }

    fn member_scopes(storage: &Path) -> Vec<String> {
        with_registry(storage, |tx| {
            Ok(tx
                .prepare("SELECT scope FROM members ORDER BY scope")?
                .query_map([], |row| row.get(0))?
                .collect::<rusqlite::Result<Vec<String>>>()?)
        })
    }

    fn missing_sweeps(storage: &Path, scope: &str) -> i64 {
        with_registry(storage, |tx| {
            Ok(tx.query_row(
                "SELECT missing_sweeps FROM members WHERE scope=?1",
                [scope],
                |row| row.get(0),
            )?)
        })
    }

    fn view_dir(storage: &Path, scope: &str) -> PathBuf {
        v3::view_dir(storage, FAMILY, scope).unwrap()
    }

    /// A checkout root with a durable binding old enough to be past its
    /// grace and with the volume evidence the mount guard needs.
    fn bound_root(temp: &Path, storage: &Path, name: &str) -> PathBuf {
        let root = temp.join("roots").join(name);
        fs::create_dir_all(&root).unwrap();
        let root = fs::canonicalize(root).unwrap();
        let scope = crate::path_identity::project_scope_key(&root);
        record_bind(&StoragePaths::same(storage), &root, &scope).unwrap();
        let record = storage.join(format!("retention/roots/{scope}.json"));
        let mut binding: Binding = read_json(&record).unwrap();
        binding.last_bound_ms = 0;
        atomic_json(&record, &binding).unwrap();
        root
    }

    fn pass(storage: &Path) -> SweepReport {
        let mut report = SweepReport::default();
        collect_one_v3_family(
            &StoragePaths::same(storage),
            &|| false,
            &mut report,
            V3Bounds::PASS,
        )
        .unwrap();
        assert!(report.errors.is_empty(), "{report:?}");
        report
    }

    fn dead_pid() -> u32 {
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .arg("--list")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let pid = child.id();
        child.wait().unwrap();
        pid
    }

    fn write_pin(view_dir: &Path, generation: &str, owner: crate::pins::PinOwner) -> PathBuf {
        let (metadata_path, keys_path) = crate::pins::pin_paths(view_dir, generation);
        fs::create_dir_all(metadata_path.parent().unwrap()).unwrap();
        let metadata = crate::pins::PinMetadata {
            family: FAMILY.to_owned(),
            view: view_dir.file_name().unwrap().to_string_lossy().into_owned(),
            generation: generation.to_owned(),
            owner,
            created_at: crate::pins::now_ms(),
            renewed_at: crate::pins::now_ms(),
        };
        fs::write(&metadata_path, serde_json::to_vec(&metadata).unwrap()).unwrap();
        fs::write(&keys_path, b"").unwrap();
        metadata_path
    }

    fn live_owner() -> crate::pins::PinOwner {
        let pid = std::process::id();
        crate::pins::PinOwner {
            pid,
            start_time: crate::root_cache::process_start_time_ms(pid).unwrap(),
        }
    }

    /// A read marker that names a process that has exited.
    fn dead_read_marker(view_dir: &Path, generation: &str) -> PathBuf {
        let marker = crate::root_cache::ReadMarker::create(view_dir, generation).unwrap();
        let path = marker.path().to_path_buf();
        let mut metadata = marker.metadata().clone();
        std::mem::forget(marker);
        metadata.pid = dead_pid();
        fs::write(&path, serde_json::to_vec(&metadata).unwrap()).unwrap();
        path
    }

    fn fill_view_dir(view_dir: &Path) {
        fs::create_dir_all(view_dir).unwrap();
        fs::write(view_dir.join("data.bin"), vec![7u8; 64 * 1024]).unwrap();
    }

    #[test]
    fn v3_collector_applies_the_missing_root_rule_to_view_members_only() {
        let temp = tempfile::tempdir().unwrap();
        let temp = fs::canonicalize(temp.path()).unwrap();
        let storage = temp.join("storage");
        fs::create_dir_all(&storage).unwrap();
        // Scopes are visited in this (sorted) order; the late-pin member
        // comes first so the test hook below pins it before anything else.
        let names = [
            "a-late-pin",
            "base",
            "dead-owner",
            "eligible",
            "live-reader",
            "live-session",
            "one-pass",
            "reappeared",
            "recent",
        ];
        let roots = names
            .iter()
            .map(|name| (*name, bound_root(&temp, &storage, name)))
            .collect::<HashMap<_, _>>();
        let mut members = names
            .iter()
            .map(|name| {
                let kind = if *name == "base" { "base" } else { "view" };
                (*name, kind, Some(roots[name].as_path()))
            })
            .collect::<Vec<_>>();
        members.push(("no-root", "view", None));
        seed(&storage, &members);
        // Every member was bound long ago except "recent".
        with_registry(&storage, |tx| {
            tx.execute(
                "UPDATE members SET last_bind_ms=0 WHERE scope!='recent'",
                [],
            )?;
            Ok(())
        });
        for (name, _, _) in &members {
            fill_view_dir(&view_dir(&storage, name));
        }
        let owner = live_owner();
        let dead = dead_pid();
        with_registry(&storage, |tx| {
            tx.execute(
                "INSERT INTO view_sessions VALUES('live',?1,?2,?3,0),('dead','dead-owner',?4,?3,0)",
                rusqlite::params!["live-session", owner.pid, owner.start_time, dead],
            )?;
            Ok(())
        });
        let _live_marker =
            crate::root_cache::ReadMarker::create(&view_dir(&storage, "live-reader"), "g1")
                .unwrap();
        let dead_pin = write_pin(
            &view_dir(&storage, "dead-owner"),
            "g1",
            crate::pins::PinOwner {
                pid: dead,
                start_time: 1,
            },
        );
        let dead_marker = dead_read_marker(&view_dir(&storage, "dead-owner"), "g1");
        let late_pin_dir = view_dir(&storage, "a-late-pin");
        let pinned = Arc::new(AtomicBool::new(false));
        let hook_pinned = Arc::clone(&pinned);
        let _hook = observe_test_hook(
            &storage,
            Arc::new(move |step| {
                if step == "v3-before-recheck" && !hook_pinned.swap(true, Ordering::SeqCst) {
                    write_pin(&late_pin_dir, "late", live_owner());
                }
            }),
        );
        for name in names.iter().filter(|name| **name != "one-pass") {
            fs::remove_dir(&roots[name]).unwrap();
        }

        assert_eq!(crate::views::registry::MISSING_ROOT_SWEEPS, 2);
        let first = pass(&storage);
        assert_eq!(first.removed_roots, 0, "{first:?}");
        assert_eq!(member_scopes(&storage).len(), members.len());
        // Dead-owner protection is reclaimed and does not hold the count back.
        assert_eq!(missing_sweeps(&storage, "dead-owner"), 1);
        assert!(!dead_pin.exists());
        assert!(!dead_marker.exists());
        assert_eq!(
            with_registry(&storage, |tx| Ok(tx.query_row(
                "SELECT COUNT(*) FROM view_sessions WHERE scope='dead-owner'",
                [],
                |row| row.get::<_, i64>(0)
            )?)),
            0
        );
        assert_eq!(missing_sweeps(&storage, "reappeared"), 1);
        assert_eq!(missing_sweeps(&storage, "live-session"), 0);
        assert_eq!(missing_sweeps(&storage, "live-reader"), 0);
        assert_eq!(missing_sweeps(&storage, "recent"), 0);
        assert!(!pinned.load(Ordering::SeqCst));

        fs::create_dir_all(&roots["reappeared"]).unwrap();
        fs::remove_dir(&roots["one-pass"]).unwrap();
        let second = pass(&storage);
        assert!(pinned.load(Ordering::SeqCst), "the recheck hook never ran");
        assert_eq!(second.removed_roots, 2, "{second:?}");
        assert_eq!(second.removed_bytes, 2 * 64 * 1024, "{second:?}");
        assert_eq!(
            member_scopes(&storage),
            [
                "a-late-pin",
                "base",
                "live-reader",
                "live-session",
                "no-root",
                "one-pass",
                "reappeared",
                "recent"
            ]
        );
        for removed in ["dead-owner", "eligible"] {
            assert!(!view_dir(&storage, removed).exists(), "{removed}");
        }
        for kept in member_scopes(&storage) {
            assert!(
                view_dir(&storage, &kept).join("data.bin").exists(),
                "{kept}"
            );
        }
        assert_eq!(missing_sweeps(&storage, "a-late-pin"), 0);
        assert_eq!(missing_sweeps(&storage, "reappeared"), 0);
        assert_eq!(missing_sweeps(&storage, "one-pass"), 1);
        assert_eq!(missing_sweeps(&storage, "base"), 0);
        // Plane rows are not the collector's to delete.
        assert_eq!(
            v3::PlaneReader::open_existing(&storage, FAMILY, FamilyPlane::Semantic)
                .unwrap()
                .get(&plane_key())
                .unwrap()
                .as_deref(),
            Some(&b"v3 plane payload"[..])
        );
    }

    /// With `storage.cache_dir` set, v3 family stores are under the cache
    /// root and root bindings under the durable root. The collector must
    /// read each where it lives, and keep its cursors durable.
    #[test]
    fn v3_collector_reads_families_on_the_cache_root_and_bindings_on_the_durable_root() {
        let temp = tempfile::tempdir().unwrap();
        let temp = fs::canonicalize(temp.path()).unwrap();
        let durable = temp.join("storage");
        let cache_parent = temp.join("cache-volume");
        fs::create_dir_all(&cache_parent).unwrap();
        let mut config = crate::config::Config {
            storage_dir: Some(durable.clone()),
            cache_dir: Some(cache_parent),
            ..Default::default()
        };
        assert!(crate::root_cache::prepare_cache_root_for_test(&mut config).is_none());
        let cache = crate::root_cache::cache_root(&config);
        let root = bound_root(&temp, &durable, "eligible");
        seed_with_durable(
            &cache,
            Some(&durable),
            &[("eligible", "view", Some(root.as_path()))],
        );
        with_registry(&cache, |tx| {
            tx.execute("UPDATE members SET last_bind_ms=0", [])?;
            Ok(())
        });
        fill_view_dir(&view_dir(&cache, "eligible"));
        fs::remove_dir(&root).unwrap();
        let paths = StoragePaths::new(durable.clone(), cache.clone());

        assert_eq!(crate::views::registry::MISSING_ROOT_SWEEPS, 2);
        for _ in 0..2 {
            let mut report = SweepReport::default();
            collect_one_v3_family(&paths, &|| false, &mut report, V3Bounds::PASS).unwrap();
            assert!(report.errors.is_empty(), "{report:?}");
        }
        assert!(member_scopes(&cache).is_empty(), "the due member was kept");
        assert!(!view_dir(&cache, "eligible").exists());
        assert!(
            !durable.join("blobs").exists(),
            "the collector created v3 data on the durable root"
        );
        assert!(
            !cache.join("retention").exists(),
            "the collector wrote its cursors on the cache root"
        );
        let cursors = cursor_root(&paths).unwrap();
        assert!(cursors.starts_with(durable.join("retention/cursors")));
        assert!(cursors.join("families-v3.json").exists());
        assert!(cursors
            .join("v3-members")
            .join(format!("{FAMILY}.json"))
            .exists());
    }

    #[test]
    fn v3_eviction_interrupted_after_its_evicting_commit_is_finished_next_pass() {
        let temp = tempfile::tempdir().unwrap();
        let temp = fs::canonicalize(temp.path()).unwrap();
        let storage = temp.join("storage");
        fs::create_dir_all(&storage).unwrap();
        let root = bound_root(&temp, &storage, "crash");
        seed(&storage, &[("crash", "view", Some(&root))]);
        with_registry(&storage, |tx| {
            tx.execute("UPDATE members SET last_bind_ms=0", [])?;
            Ok(())
        });
        fill_view_dir(&view_dir(&storage, "crash"));
        fs::remove_dir(&root).unwrap();
        pass(&storage);
        {
            let _hook = observe_test_hook(
                &storage,
                Arc::new(|step| {
                    if step == "v3-evicting-committed" {
                        panic!("simulated crash after the evicting commit");
                    }
                }),
            );
            let crashed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let mut report = SweepReport::default();
                let _ = collect_one_v3_family(
                    &StoragePaths::same(&storage),
                    &|| false,
                    &mut report,
                    V3Bounds::PASS,
                );
            }));
            assert!(crashed.is_err());
        }
        let state: String = with_registry(&storage, |tx| {
            Ok(
                tx.query_row("SELECT state FROM members WHERE scope='crash'", [], |row| {
                    row.get(0)
                })?,
            )
        });
        assert_eq!(state, crate::views::registry::STATE_EVICTING);
        assert!(view_dir(&storage, "crash").join("data.bin").exists());
        // Once `evicting` is committed the eviction is finished regardless:
        // a root that reappears now does not bring the member back.
        fs::create_dir_all(&root).unwrap();
        let finished = pass(&storage);
        assert_eq!(finished.removed_roots, 1, "{finished:?}");
        assert!(member_scopes(&storage).is_empty());
        assert!(!view_dir(&storage, "crash").exists());
    }

    /// A window of protected members cannot starve the members after it: each
    /// visit resumes after the last member the previous one examined, and
    /// wraps to the start at the end of the scope order.
    #[test]
    fn v3_collector_resumes_after_the_last_examined_member() {
        let temp = tempfile::tempdir().unwrap();
        let temp = fs::canonicalize(temp.path()).unwrap();
        let storage = temp.join("storage");
        fs::create_dir_all(&storage).unwrap();
        let names = ["p1", "p2", "p3", "z-target"];
        let roots = names
            .iter()
            .map(|name| (*name, bound_root(&temp, &storage, name)))
            .collect::<HashMap<_, _>>();
        let members = names
            .iter()
            .map(|name| (*name, "view", Some(roots[name].as_path())))
            .collect::<Vec<_>>();
        seed(&storage, &members);
        let owner = live_owner();
        with_registry(&storage, |tx| {
            tx.execute("UPDATE members SET last_bind_ms=0", [])?;
            for (index, scope) in ["p1", "p2", "p3"].into_iter().enumerate() {
                tx.execute(
                    "INSERT INTO view_sessions VALUES(?1,?2,?3,?4,0)",
                    rusqlite::params![format!("live-{index}"), scope, owner.pid, owner.start_time],
                )?;
            }
            Ok(())
        });
        for name in names {
            fill_view_dir(&view_dir(&storage, name));
            fs::remove_dir(&roots[name]).unwrap();
        }
        // Two members per visit: the protected members alone fill a window.
        let bounds = V3Bounds {
            members: 2,
            ..V3Bounds::PASS
        };
        let mut passes = 0;
        while member_scopes(&storage).contains(&"z-target".to_owned()) {
            passes += 1;
            assert!(passes <= 4, "z-target was never collected");
            let mut report = SweepReport::default();
            collect_one_v3_family(
                &StoragePaths::same(&storage),
                &|| false,
                &mut report,
                bounds,
            )
            .unwrap();
            assert!(report.errors.is_empty(), "{report:?}");
        }
        assert_eq!(member_scopes(&storage), ["p1", "p2", "p3"]);
        assert!(!view_dir(&storage, "z-target").exists());
    }

    /// A covering measurement that runs out of pass budget records nothing,
    /// reports the measurement as owed and releases the reservation lock; the
    /// next pass takes the complete measurement.
    #[cfg(unix)]
    #[test]
    fn v3_measurement_past_the_deadline_records_nothing_and_is_owed() {
        let temp = tempfile::tempdir().unwrap();
        let temp = fs::canonicalize(temp.path()).unwrap();
        let storage = temp.join("storage");
        fs::create_dir_all(&storage).unwrap();
        seed(&storage, &[("member", "view", None)]);
        fill_view_dir(&view_dir(&storage, "member"));
        let measured = || {
            v3::LedgerReader::open_existing(&storage, FAMILY)
                .unwrap()
                .accounting()
                .unwrap()
                .measured_bytes
        };
        // Bring the record up to date, then grow the family so any complete
        // walk would record a different total.
        pass(&storage);
        let recorded = measured();
        let family = storage.join("blobs/v3").join(FAMILY);
        fs::write(family.join("growth.bin"), vec![5u8; 256 * 1024]).unwrap();

        // Every entry of the walk now takes 100 ms; the family has more
        // entries than a 300 ms budget allows.
        v3::set_walk_delay_for_test(Duration::from_millis(100));
        let budget = Duration::from_millis(300);
        let started = Instant::now();
        let mut report = SweepReport::default();
        let result = collect_one_v3_family(
            &StoragePaths::same(&storage),
            &|| false,
            &mut report,
            V3Bounds {
                budget,
                ..V3Bounds::PASS
            },
        );
        let elapsed = started.elapsed();
        v3::set_walk_delay_for_test(Duration::ZERO);
        result.unwrap();
        assert!(elapsed < budget + Duration::from_secs(2), "{elapsed:?}");
        assert_eq!(
            report.skipped_families,
            [format!("blobs/v3/{FAMILY}: measurement_owed")]
        );
        assert_eq!(measured(), recorded, "a partial walk was recorded");
        // The reservation lock is free again at once.
        drop(ReservationLock::acquire_until(&storage, Instant::now()).unwrap());

        let next = pass(&storage);
        assert!(next.skipped_families.is_empty(), "{next:?}");
        assert_eq!(measured(), inventory(&family));
        assert!(measured() > recorded);
    }

    /// Another process's hold on the reservation lock costs the pass at most
    /// its budget: the family is reported as skipped, the lap position stays,
    /// and the next pass examines that same family first once the lock is free.
    #[test]
    fn v3_collector_skips_a_family_whose_reservation_lock_is_busy() {
        let temp = tempfile::tempdir().unwrap();
        let temp = fs::canonicalize(temp.path()).unwrap();
        let storage = temp.join("storage");
        fs::create_dir_all(&storage).unwrap();
        let root = bound_root(&temp, &storage, "busy");
        seed(&storage, &[("busy", "view", Some(&root))]);
        with_registry(&storage, |tx| {
            tx.execute("UPDATE members SET last_bind_ms=0", [])?;
            Ok(())
        });
        fs::remove_dir(&root).unwrap();
        // A second family sorts after the seeded one, so a skip that wrongly
        // advanced the lap would make the next pass visit this one instead.
        const LATER: &str = "fedcba9876543210";
        fs::create_dir_all(storage.join("blobs/v3").join(LATER)).unwrap();

        // The lock is not reentrant, so holding it here makes the
        // collector's own acquisition wait exactly as another process's would.
        let held = ReservationLock::acquire(&storage).unwrap();
        let budget = Duration::from_millis(300);
        let (sender, receiver) = std::sync::mpsc::channel();
        let pass_storage = storage.clone();
        std::thread::spawn(move || {
            let started = Instant::now();
            let mut report = SweepReport::default();
            let bounds = V3Bounds {
                budget,
                ..V3Bounds::PASS
            };
            let result = collect_one_v3_family(
                &StoragePaths::same(&pass_storage),
                &|| false,
                &mut report,
                bounds,
            )
            .map_err(|error| error.to_string());
            let _ = sender.send((result, report, started.elapsed()));
        });
        let (result, report, elapsed) = receiver
            .recv_timeout(budget + Duration::from_secs(10))
            .expect("the pass waited for the reservation lock past its budget");
        result.unwrap();
        assert!(elapsed < budget + Duration::from_secs(5), "{elapsed:?}");
        assert_eq!(
            report.skipped_families,
            [format!("blobs/v3/{FAMILY}: reservation_busy")]
        );
        assert!(report.errors.is_empty(), "{report:?}");
        assert!(!report.domains.contains_key("views/v3"), "{report:?}");
        drop(held);
        assert_eq!(missing_sweeps(&storage, "busy"), 0);

        let next = pass(&storage);
        assert!(next.skipped_families.is_empty(), "{next:?}");
        assert_eq!(next.domains["views/v3"].examined, 1, "{next:?}");
        assert_eq!(missing_sweeps(&storage, "busy"), 1);
    }

    /// The st_blocks size of every file below `path`, computed here rather
    /// than through the accounting code under test.
    #[cfg(unix)]
    fn inventory(path: &Path) -> u64 {
        use std::os::unix::fs::MetadataExt;
        fs::read_dir(path)
            .unwrap()
            .map(|entry| {
                let path = entry.unwrap().path();
                let metadata = fs::symlink_metadata(&path).unwrap();
                if metadata.is_dir() {
                    inventory(&path)
                } else {
                    metadata.blocks() * 512
                }
            })
            .sum()
    }

    #[cfg(unix)]
    #[test]
    fn collecting_a_view_member_lowers_committed_usage_by_its_allocated_bytes() {
        let temp = tempfile::tempdir().unwrap();
        let temp = fs::canonicalize(temp.path()).unwrap();
        let storage = temp.join("storage");
        fs::create_dir_all(&storage).unwrap();
        let root = bound_root(&temp, &storage, "accounted");
        seed(&storage, &[("accounted", "view", Some(&root))]);
        with_registry(&storage, |tx| {
            tx.execute("UPDATE members SET last_bind_ms=0", [])?;
            Ok(())
        });
        let member = view_dir(&storage, "accounted");
        fs::create_dir_all(member.join("pins")).unwrap();
        fs::create_dir_all(member.join("readers")).unwrap();
        for (index, size) in [300 * 1024, 4096, 17].into_iter().enumerate() {
            fs::write(member.join(format!("file-{index}")), vec![1u8; size]).unwrap();
        }
        fs::remove_dir(&root).unwrap();
        // The first pass only counts the missing root.
        pass(&storage);
        let family = storage.join("blobs/v3").join(FAMILY);
        let usage = || {
            v3::LedgerReader::open_existing(&storage, FAMILY)
                .unwrap()
                .accounting()
                .unwrap()
                .usage()
        };
        let before = usage();
        assert_eq!(before, inventory(&family));
        let member_bytes = inventory(&member);
        assert!(member_bytes >= 300 * 1024);
        let collected = pass(&storage);
        assert_eq!(collected.removed_roots, 1, "{collected:?}");
        assert!(!member.exists());
        let after = usage();
        assert_eq!(after, inventory(&family));
        assert_eq!(before - after, member_bytes);
    }

    /// `reader-floor.json` as the `aft` binary built from tag v0.60.0 writes
    /// it on a fresh storage root: one configure, captured byte for byte from
    /// a build of commit 988f4f3e52b9.
    const V0_60_0_FLOOR: &str = "{\n  \"floor_schema\": 1,\n  \"stores\": {\n    \"aft.db\": 18,\n    \"artifact_owner\": 1,\n    \"backup_meta\": 5,\n    \"bash_task\": 7,\n    \"callgraph\": 1,\n    \"search\": 4,\n    \"semantic\": 7,\n    \"semantic_segment\": 1,\n    \"symbols\": 3,\n    \"symbols_extraction\": 4\n  }\n}\n";

    fn walkdir_for_test(path: &Path) -> Vec<PathBuf> {
        let mut out = Vec::new();
        for entry in fs::read_dir(path).into_iter().flatten().flatten() {
            out.push(entry.path());
            if entry.file_type().is_ok_and(|kind| kind.is_dir()) {
                out.extend(walkdir_for_test(&entry.path()));
            }
        }
        out
    }

    /// An OpenAI-compatible embedding endpoint on loopback that answers
    /// every input with the same vector, so the semantic runtime loads with
    /// no network. Returns its base URL; the listener lives for the process.
    fn fixed_vector_embedder() -> String {
        use std::io::Read;
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let base_url = format!("http://{}", listener.local_addr().unwrap());
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { break };
                std::thread::spawn(move || {
                    let mut bytes = Vec::new();
                    let mut buf = [0; 8192];
                    let (start, length) = loop {
                        let Ok(n) = stream.read(&mut buf) else { return };
                        if n == 0 {
                            return;
                        }
                        bytes.extend_from_slice(&buf[..n]);
                        if let Some(end) = bytes.windows(4).position(|s| s == b"\r\n\r\n") {
                            let length: usize = String::from_utf8_lossy(&bytes[..end])
                                .lines()
                                .find_map(|line| {
                                    let (name, value) = line.split_once(':')?;
                                    name.eq_ignore_ascii_case("content-length")
                                        .then(|| value.trim().parse().unwrap_or(0))
                                })
                                .unwrap_or(0);
                            if bytes.len() >= end + 4 + length {
                                break (end + 4, length);
                            }
                        }
                    };
                    let body: serde_json::Value =
                        serde_json::from_slice(&bytes[start..start + length]).unwrap_or_default();
                    let inputs = body["input"].as_array().map_or(1, Vec::len);
                    let data = (0..inputs)
                        .map(|index| {
                            serde_json::json!({"index": index, "embedding": [0.25, 0.5, 0.75, 1.0]})
                        })
                        .collect::<Vec<_>>();
                    let body = serde_json::json!({ "data": data }).to_string();
                    let _ = write!(
                        stream,
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    );
                });
            }
        });
        base_url
    }

    struct NoProcesses;
    impl crate::migration::prune_legacy::ProcessCensus for NoProcesses {
        fn take(&self) -> Result<Vec<crate::migration::prune_legacy::CensusFinding>, String> {
            Ok(Vec::new())
        }
    }

    #[test]
    fn v3_first_registration_and_lock_raise_floor_before_namespace() {
        use crate::persisted_format::PersistedStore;

        for entry in ["registration", "lock", "deadline-lock"] {
            let temp = tempfile::tempdir().unwrap();
            let storage = fs::canonicalize(temp.path()).unwrap();
            fs::write(storage.join("reader-floor.json"), V0_60_0_FLOOR).unwrap();
            let boundaries = Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let observed = Arc::clone(&boundaries);
            let root = storage.clone();
            let _hook = observe_test_hook(
                &storage,
                Arc::new(move |step| {
                    if step == "v3-floor-raising" {
                        crate::durability::take();
                    } else if step.starts_with("v3-floor-raised:")
                        && observed.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0
                    {
                        assert!(
                            !root.join("blobs/v3").exists(),
                            "{entry}: namespace created before the first durable floor boundary"
                        );
                        let floor = crate::reader_floor::read(&root).unwrap().unwrap();
                        for domain in [
                            PersistedStore::ViewsV3Registry,
                            PersistedStore::ViewsV3Store,
                        ] {
                            assert!(floor.get(domain).is_some_and(|version| version >= 1));
                        }
                        #[cfg(unix)]
                        assert!(crate::durability::take().contains(&(
                            crate::durability::EventKind::DirectorySync,
                            root.clone(),
                        )));
                    }
                }),
            );
            // Registration must be safe even when no semantic or callgraph
            // publisher has won the race to raise the floor first.
            match entry {
                "registration" => {
                    crate::views::base_refs::with_base_barrier(&storage, FAMILY, |_| Ok(()))
                        .unwrap();
                }
                "lock" => drop(ReservationLock::acquire(&storage).unwrap()),
                "deadline-lock" => {
                    drop(ReservationLock::acquire_until(&storage, Instant::now()).unwrap());
                }
                _ => unreachable!(),
            }
            assert!(boundaries.load(std::sync::atomic::Ordering::SeqCst) > 0);
            assert!(storage.join("blobs/v3").is_dir());
        }
    }

    #[cfg(unix)]
    #[test]
    fn v3_first_lock_sync_failure_leaves_no_namespace_and_retry_must_sync() {
        for timed in [false, true] {
            let temp = tempfile::tempdir().unwrap();
            let storage = fs::canonicalize(temp.path()).unwrap();
            fs::write(storage.join("reader-floor.json"), V0_60_0_FLOOR).unwrap();
            let acquire = || {
                if timed {
                    ReservationLock::acquire_until(&storage, Instant::now())
                } else {
                    ReservationLock::acquire(&storage)
                }
            };
            // The first failure leaves a replaced but unsynced floor. A retry
            // must not mistake its covering versions for a durability proof.
            for _ in 0..2 {
                crate::durability::FAIL_DIRECTORY_SYNC.with(|slot| {
                    *slot.borrow_mut() = Some(storage.clone());
                });
                assert!(
                    acquire().is_err(),
                    "floor sync failure must refuse the lock"
                );
                assert!(!storage.join("blobs/v3").exists());
                assert!(crate::durability::FAIL_DIRECTORY_SYNC.with(|slot| slot.borrow().is_none()));
            }
            crate::durability::take();
            let _lock = acquire().unwrap();
            assert!(crate::durability::take()
                .contains(&(crate::durability::EventKind::DirectorySync, storage.clone(),)));
            assert!(storage.join("blobs/v3/reservations.lock").is_file());
        }
    }

    /// Views-off configure must not promote a v0.60.0 root to v3. Views-on
    /// configure may promote it, but both v3 floor domains must be durable before
    /// its first v3 publication. Maintenance must preserve that boundary.
    #[test]
    fn no_production_path_writes_v3_over_a_v0_60_0_floor() {
        use crate::migration::prune_legacy::{prune_legacy, PruneOptions};
        use crate::persisted_format::PersistedStore;
        let _git_env = crate::test_env::hermetic_git_env_guard();
        let temp = tempfile::tempdir().unwrap();
        let temp = fs::canonicalize(temp.path()).unwrap();
        let storage = temp.join("storage");
        let project = temp.join("project");
        fs::create_dir_all(&storage).unwrap();
        fs::create_dir_all(project.join("src")).unwrap();
        fs::write(project.join("src/lib.rs"), "pub fn marker() {}\n").unwrap();
        for args in [
            &["init", "--quiet"][..],
            &["add", "."],
            &[
                "-c",
                "user.name=Retention Tests",
                "-c",
                "user.email=retention@example.invalid",
                "commit",
                "--quiet",
                "-m",
                "initial",
            ],
        ] {
            let mut command = std::process::Command::new("git");
            crate::test_env::apply_hermetic_git_env(command.current_dir(&project));
            let output = command.args(args).output().unwrap();
            assert!(output.status.success(), "git {args:?}: {output:?}");
        }
        fs::write(storage.join("reader-floor.json"), V0_60_0_FLOOR).unwrap();
        let stores = |text: &str| -> BTreeMap<String, u64> {
            serde_json::from_value(
                serde_json::from_str::<serde_json::Value>(text).unwrap()["stores"].take(),
            )
            .unwrap()
        };
        let before = stores(V0_60_0_FLOOR);
        let check_baseline = |after: &BTreeMap<String, u64>| {
            for (name, version) in after {
                let store = PersistedStore::from_name(name)
                    .unwrap_or_else(|| panic!("unknown store {name} in the floor: {after:?}"));
                assert!(
                    before.get(name) == Some(version) || u64::from(store.written()) == *version,
                    "{name} at {version} is neither v0.60.0's value nor this build's: {after:?}"
                );
            }
            for name in before.keys() {
                assert!(after.contains_key(name), "{name} left the floor: {after:?}");
            }
        };
        let publications = Arc::new(Mutex::new(Vec::new()));
        let publication_calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let calls = Arc::clone(&publication_calls);
        let observed = Arc::clone(&publications);
        let observed_storage = storage.clone();
        let _hook = observe_test_hook(
            &storage,
            Arc::new(move |step| {
                if step == "v3-floor-raising" {
                    calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    // Isolate syncs performed by this floor raise, not earlier
                    // configure work on the same worker thread.
                    crate::durability::take();
                    return;
                }
                let Some(versions) = step.strip_prefix("v3-floor-raised:") else {
                    return;
                };
                let (registry, store) = versions.split_once(':').unwrap();
                observed.lock().unwrap().push((
                    crate::reader_floor::read(&observed_storage)
                        .unwrap()
                        .unwrap(),
                    observed_storage.join("blobs/v3").exists(),
                    crate::durability::take(),
                    (
                        registry.parse::<u64>().unwrap(),
                        store.parse::<u64>().unwrap(),
                    ),
                    crate::file_identity::metadata(&crate::reader_floor::floor_path(
                        &observed_storage,
                    ))
                    .unwrap()
                    .1,
                ));
            }),
        );
        let embedder = fixed_vector_embedder();
        for views in [false, true] {
            let ctx = crate::context::AppContext::new(
                Box::new(crate::parser::TreeSitterProvider::new()),
                crate::config::Config {
                    storage_dir: Some(storage.clone()),
                    ..crate::config::Config::default()
                },
            );
            let request: crate::protocol::RawRequest = serde_json::from_value(serde_json::json!({
                "id": "cfg",
                "command": "configure",
                "project_root": project,
                "storage_dir": storage,
                "harness": "opencode",
                "config": [{
                    "tier": "user",
                    "source": "/u/aft.jsonc",
                    "doc": serde_json::json!({
                        "views": { "enabled": views },
                        "indexes": { "trigram": false, "semantic": true, "callgraph": true },
                        "semantic": {
                            "backend": "openai_compatible",
                            "model": "fixed-vector",
                            "base_url": embedder,
                            "timeout_ms": 5_000
                        }
                    }).to_string(),
                }],
            }))
            .unwrap();
            let response = crate::commands::configure::handle_configure(&request, &ctx);
            assert!(response.success, "{}", response.data);
            crate::commands::configure::drain_deferred_configure_maintenance(&ctx);
            if views {
                // The checkout's semantic runtime owns the v3 family registry;
                // wait for it to load as the binary's drain loop would.
                let deadline = Instant::now() + Duration::from_secs(120);
                let registry = loop {
                    crate::runtime_drain::drain_semantic_index_events(&ctx);
                    if let Some(registry) = ctx.checkout_view_registry() {
                        break registry;
                    }
                    assert!(
                        Instant::now() < deadline,
                        "the views-on runtime never loaded: {:#?}",
                        walkdir_for_test(&storage)
                    );
                    std::thread::sleep(Duration::from_millis(50));
                };
                crate::views::eviction::enforce(&registry, Default::default()).unwrap();
            } else {
                assert!(!storage.join("blobs/v3").exists());
                assert!(publications.lock().unwrap().is_empty());
                assert_eq!(
                    publication_calls.load(std::sync::atomic::Ordering::SeqCst),
                    0
                );
                let after = stores(&fs::read_to_string(storage.join("reader-floor.json")).unwrap());
                check_baseline(&after);
                for v3 in [
                    PersistedStore::ViewsV3Registry,
                    PersistedStore::ViewsV3Store,
                ] {
                    assert_eq!(after.get(v3.name()), before.get(v3.name()));
                    assert!(!after.contains_key(v3.name()), "views-off promoted {v3:?}");
                }
            }
        }
        for _ in 0..2 {
            let report = run_pass_with_budget(
                &StoragePaths::same(&storage),
                &|| false,
                &Instant::now,
                PASS_BUDGET,
            );
            assert!(report.timings_ms.contains_key("blobs_v3"), "{report:?}");
        }
        for yes in [false, true] {
            let mut out = Vec::new();
            prune_legacy(
                &storage,
                &PruneOptions {
                    yes,
                    now: std::time::SystemTime::now(),
                },
                &NoProcesses,
                None,
                &mut out,
            )
            .unwrap();
            assert!(!out.is_empty());
        }
        assert!(storage.join("blobs/v3").exists(), "views-on never ran");
        // Non-v3 stores may advance to this build's baseline, but publication
        // must never invent a store key or remove an older reader requirement.
        let floor = fs::read_to_string(storage.join("reader-floor.json")).unwrap();
        let after = stores(&floor);
        check_baseline(&after);
        let publications = publications.lock().unwrap();
        let (_, v3_existed, _, _, _) = publications.first().expect("v3 publication observed");
        assert!(
            !v3_existed,
            "v3 data existed before the first floor boundary"
        );
        #[cfg(unix)]
        let mut durable_floor = None;
        for (published_floor, _, syncs, (registry, store), identity) in publications.iter() {
            for (domain, required) in [
                (PersistedStore::ViewsV3Registry, registry),
                (PersistedStore::ViewsV3Store, store),
            ] {
                assert!(
                    published_floor
                        .get(domain)
                        .is_some_and(|version| version >= *required),
                    "{domain:?} must cover {required} before publication: {published_floor:?}"
                );
            }
            #[cfg(unix)]
            {
                let proof = (*identity, published_floor.clone());
                // The first boundary, and every replaced floor entry, must
                // perform a real sync. Later boundaries may reuse that exact
                // entry's observed successful sync, never just its versions.
                if syncs.contains(&(crate::durability::EventKind::DirectorySync, storage.clone())) {
                    durable_floor = Some(proof.clone());
                }
                assert_eq!(
                    durable_floor.as_ref(),
                    Some(&proof),
                    "the current reader floor must be durable before publication: {syncs:?}"
                );
            }
            #[cfg(not(unix))]
            let _ = (syncs, identity);
        }
        for required_registry in [
            crate::blob_store::v3::REGISTRY_BASE_REFS_VERSION,
            crate::blob_store::v3::REGISTRY_VERIFICATION_VERSION,
        ] {
            assert!(
                publications
                    .iter()
                    .any(|(_, _, _, (registry, _), _)| *registry == u64::from(required_registry)),
                "the production path must exercise registry schema {required_registry}"
            );
        }
        for v3 in [
            PersistedStore::ViewsV3Registry,
            PersistedStore::ViewsV3Store,
        ] {
            assert_eq!(after.get(v3.name()), Some(&u64::from(v3.written())));
        }
    }

    #[test]
    fn v3_collector_never_creates_a_registry() {
        let temp = tempfile::tempdir().unwrap();
        let storage = temp.path();
        // A storage root without blobs/v3 gets nothing at all.
        pass(storage);
        assert!(!storage.join("blobs/v3").exists());
        assert!(!storage.join("retention/cursors/families-v3.json").exists());
        // A family directory without a registry (here, only a member tree)
        // is skipped before the reservation lock is taken.
        let member = view_dir(storage, "orphan");
        fill_view_dir(&member);
        pass(storage);
        pass(storage);
        assert!(!storage
            .join("blobs/v3")
            .join(FAMILY)
            .join("members.sqlite")
            .exists());
        assert!(!storage.join("blobs/v3/reservations.lock").exists());
        assert!(member.join("data.bin").exists());
    }
}

#[cfg(test)]
mod v3_base_reference_collector_tests {
    use super::*;
    use crate::views::base_refs::tests::{
        bind, plane_has, publish_test_base, published, put_rows, refs, worker_delta, FAMILY,
    };
    use crate::views::base_refs::with_base_barrier;

    fn pass(storage: &Path) -> SweepReport {
        let mut report = SweepReport::default();
        collect_one_v3_family(
            &StoragePaths::same(storage),
            &|| false,
            &mut report,
            V3Bounds::PASS,
        )
        .unwrap();
        assert!(report.errors.is_empty(), "{report:?}");
        report
    }

    fn registered(storage: &Path, scope: &str) -> bool {
        crate::blob_store::v3::LedgerReader::open_existing(storage, FAMILY)
            .unwrap()
            .member(scope)
            .unwrap()
            .is_some()
    }

    #[test]
    fn the_collector_keeps_a_referenced_base_and_reclaims_it_two_passes_after_its_worker_goes() {
        let _serial = crate::blob_store::v3::serial_for_test();
        let storage = tempfile::tempdir().unwrap();
        let (identity, generation) = publish_test_base(storage.path(), &[("src/a.rs", b"alpha")]);
        put_rows(storage.path(), &[b"new"]);
        let (_, composed) = published(
            bind(
                storage.path(),
                "worker-a",
                &identity,
                &generation,
                worker_delta(&identity, &generation, b"new"),
                None,
            )
            .unwrap(),
        );
        drop(composed);
        for _ in 0..3 {
            pass(storage.path());
            assert!(registered(storage.path(), &identity.scope));
            assert!(plane_has(storage.path(), b"alpha"));
        }
        // The worker is deregistered: its rows go with its member row.
        with_base_barrier(storage.path(), FAMILY, |tx| {
            tx.execute(
                "UPDATE members SET state='evicting' WHERE scope='worker-a'",
                [],
            )?;
            Ok(())
        })
        .unwrap();
        pass(storage.path());
        assert!(!registered(storage.path(), "worker-a"));
        assert!(refs(storage.path()).is_empty());
        // That same pass counted the base once; the next one reclaims it.
        assert!(registered(storage.path(), &identity.scope));
        pass(storage.path());
        assert!(!registered(storage.path(), &identity.scope));
        pass(storage.path());
        assert!(!plane_has(storage.path(), b"alpha"));
        assert!(!storage.path().join("views/v3").exists());
    }
}
