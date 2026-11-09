use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock, Weak};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crossbeam_channel::{after, bounded, select_biased, Receiver, Sender};
use serde::Deserialize;
use serde_json::{json, Value};

use super::cache::{
    InspectCache, InspectCacheError, InspectCacheRead, InspectDbTimings, Tier2ContributionUpdates,
    WRITER_RETRY_BACKOFF,
};
use super::dispatch::{default_worker, InspectDispatcher, InspectWorker};
use super::freshness::{verify_contribution_file, ContributionFreshness};
use super::job::{
    is_test_tree_file, CallgraphSnapshot, ExcludedTestTally, FileContribution, InspectCategory,
    InspectJob, InspectResult, InspectScanSuccess, InspectSnapshot, JobKey, JobOutcome, JobScope,
    PendingWaitCause,
};
use super::oxc_engine::LivenessVerdict;
use super::oxc_engine::{
    analyze_file_facts, analyze_files_with_cache, normalize_input_path, AnalyzeOptions,
    DynamicImportFact, ExportFact, FileFacts, FileId, ImportFact, OxcEngineResult, OxcFactsCache,
    ReExportFact, FACTS_FORMAT_VERSION, OXC_PROVENANCE,
};
use crate::cache_freshness::{self, FileFreshness, FreshnessVerdict};
use crate::callgraph_store::{
    CallGraphStoreError, ProjectionCostEstimates, ProjectionKind, ProjectionVerdict,
    MAX_DELTA_BYTES,
};
use crate::cold_build_limiter;

const DEFAULT_SOFT_DEADLINE: Duration = Duration::from_secs(1);

fn inspect_category_is_off(
    config: &crate::config::InspectConfig,
    category: InspectCategory,
) -> bool {
    // Retired categories still take their existing unsupported-category path.
    category.is_active() && !config.category_enabled(category)
}

/// How long the dead-code graph a request builds stays in memory without
/// another dead-code request. Dead code and unused exports read it; it holds
/// hundreds of MB on a large repository, and nothing else uses it, so an
/// agent that stopped asking for dead code does not keep paying for it. The
/// next request builds it again.
pub(crate) const DEAD_CODE_GRAPH_IDLE_RELEASE: Duration = Duration::from_secs(10 * 60);

/// What [`InspectManager::release_idle_dead_code_graph`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DeadCodeGraphRelease {
    /// No dead-code request is outstanding: nothing to release.
    NotHeld,
    /// The graph was released.
    Released,
    /// The graph stays; check again after this long.
    KeepFor(Duration),
}

type WaiterTx = Sender<JobOutcome>;
type Tier2PermitSlot = Arc<Mutex<Option<cold_build_limiter::ColdBuildPermit>>>;

#[derive(Clone)]
struct Waiter {
    tx: WaiterTx,
}

#[derive(Default)]
struct InFlightRegistration {
    // Workers retain this token after publication so late cleanup cannot match
    // a replacement registered under the same key.
    identity: Arc<()>,
    waiters: Vec<Waiter>,
}

impl InFlightRegistration {
    fn new(waiters: Vec<Waiter>) -> Self {
        Self {
            identity: Arc::new(()),
            waiters,
        }
    }
}

struct CachedContributionFreshness {
    file_path: PathBuf,
    freshness: FileFreshness,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct InspectCacheIdentity {
    sqlite_path: PathBuf,
    project_root: PathBuf,
}

#[cfg(test)]
#[derive(Debug)]
enum CacheWriterWatchObservation {
    Subscribed,
    Failed(String),
    Activity { release: bool, detail: String },
}

type CacheOpenGates = Mutex<HashMap<InspectCacheIdentity, Weak<Mutex<()>>>>;

/// A published generation names an immutable cold build; the durable revision
/// distinguishes the cheap in-place refreshes of that generation.
#[derive(Debug, Clone, PartialEq, Eq)]
struct CallgraphProjectionIdentity {
    project_root: PathBuf,
    generation: Option<String>,
    /// Only legacy stores lack a generation label, so their concrete database
    /// path keeps fallback stores distinct without weakening generation checks.
    legacy_sqlite_path: Option<PathBuf>,
    write_revision: u64,
}

#[derive(Debug)]
struct CachedCallgraphProjection {
    identity: CallgraphProjectionIdentity,
    snapshot: Arc<CallgraphSnapshot>,
    estimated_bytes: u64,
    costs: ProjectionCostEstimates,
}

// Projection snapshots are useful only as whole-root splice bases. Bound their
// aggregate residency rather than multiplying a per-root cap by every manager.

type ProjectionSlot = Mutex<Option<CachedCallgraphProjection>>;

struct ProjectionFleetEntry {
    slot: Weak<ProjectionSlot>,
    bytes: u64,
}

#[derive(Default)]
struct ProjectionFleet {
    entries: HashMap<PathBuf, ProjectionFleetEntry>,
    bytes: u64,
    drops: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct DeadCodeSnapshotCensus {
    pub roots: usize,
    pub bytes: u64,
    pub drops: u64,
}

impl ProjectionFleet {
    fn forget(&mut self, root: &Path) {
        if let Some(entry) = self.entries.remove(root) {
            self.bytes = self.bytes.saturating_sub(entry.bytes);
        }
    }

    fn census(&mut self) -> DeadCodeSnapshotCensus {
        let stale = self
            .entries
            .iter()
            .filter(|(_, entry)| entry.slot.upgrade().is_none())
            .map(|(root, _)| root.clone())
            .collect::<Vec<_>>();
        for root in stale {
            self.forget(&root);
        }
        DeadCodeSnapshotCensus {
            roots: self.entries.len(),
            bytes: self.bytes,
            drops: self.drops,
        }
    }
}

fn projection_fleet() -> &'static Mutex<ProjectionFleet> {
    static FLEET: OnceLock<Mutex<ProjectionFleet>> = OnceLock::new();
    FLEET.get_or_init(|| Mutex::new(ProjectionFleet::default()))
}

pub(crate) fn dead_code_snapshot_census() -> DeadCodeSnapshotCensus {
    projection_fleet()
        .lock()
        .map(|mut fleet| fleet.census())
        .unwrap_or(DeadCodeSnapshotCensus {
            roots: 0,
            bytes: 0,
            drops: 0,
        })
}

#[derive(Debug, Clone)]
struct Tier2ReuseOptions {
    inventory: Option<Arc<super::inventory::InspectInventory>>,
    force_rescan_paths: BTreeSet<PathBuf>,
    allow_callgraph_cold_build: bool,
    require_callgraph_snapshot: bool,
    interactive: bool,
    // Direct result readers consume per-file facts; waiter completion only needs the aggregate.
    return_contributions: bool,
}

impl Tier2ReuseOptions {
    fn has_force_paths(&self) -> bool {
        !self.force_rescan_paths.is_empty()
    }
}

impl Default for Tier2ReuseOptions {
    fn default() -> Self {
        Self {
            inventory: super::inventory::inherited(),
            force_rescan_paths: BTreeSet::new(),
            allow_callgraph_cold_build: true,
            require_callgraph_snapshot: false,
            interactive: false,
            return_contributions: false,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum InspectBuilderState {
    Building,
    WaitingForCacheWriterLease,
    QueuedBehindColdBuilds,
    GatedBySemanticSeed,
    #[cfg(test)]
    Suspended,
    BuildDenied,
    Absent,
}

impl InspectBuilderState {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Building => "building",
            Self::WaitingForCacheWriterLease => "waiting_for_inspect_cache_writer_lease",
            Self::QueuedBehindColdBuilds => "queued_behind_cold_builds",
            Self::GatedBySemanticSeed => "gated_by_semantic_seed",
            #[cfg(test)]
            Self::Suspended => "suspended",
            Self::BuildDenied => "build_denied (borrow-only)",
            Self::Absent => "absent",
        }
    }
}

/// One admission into the inspect builder registry. Health's `tier2` field and
/// inspect refusals both read this map so a published aggregate cannot report
/// ready while a rebuild is still registered.
///
/// Failed attempts keep their history here after the in-flight state is
/// cleared. A fast-failing rebuild that restarts on every probe would otherwise
/// look like a brand-new warm-up (`building` at age 0) even though the same
/// terminal keeps repeating.
struct BuilderStateEntry {
    state: Option<InspectBuilderState>,
    started_at: Instant,
    started_unix: u64,
    first_attempt_unix: u64,
    attempt_count: u64,
    last_failure: Option<String>,
    suspension: Option<crate::build_breaker::BuildSuspension>,
    progress: String,
}

impl BuilderStateEntry {
    fn new(state: InspectBuilderState) -> Self {
        let now = unix_now_secs();
        Self {
            state: Some(state),
            started_at: Instant::now(),
            started_unix: now,
            first_attempt_unix: now,
            attempt_count: 0,
            last_failure: None,
            suspension: None,
            progress: "checking cached analysis".into(),
        }
    }

    fn is_in_flight(&self) -> bool {
        self.state.is_some_and(|state| {
            matches!(
                state,
                InspectBuilderState::Building
                    | InspectBuilderState::QueuedBehindColdBuilds
                    | InspectBuilderState::GatedBySemanticSeed
            )
        })
    }

    fn begin_attempt(&mut self, state: InspectBuilderState) {
        self.state = Some(state);
        self.suspension = None;
        self.started_at = Instant::now();
        self.started_unix = unix_now_secs();
        if self.attempt_count == 0 && self.last_failure.is_none() {
            self.first_attempt_unix = self.started_unix;
        }
    }

    /// Record a failed attempt.
    fn record_failure(&mut self, terminal: String) {
        self.state = None;
        self.suspension = None;
        self.attempt_count = self.attempt_count.saturating_add(1);
        self.last_failure = Some(terminal);
    }

    fn record_cache_writer_wait(&mut self) {
        self.state = Some(InspectBuilderState::WaitingForCacheWriterLease);
        self.suspension = None;
        self.attempt_count = self.attempt_count.saturating_add(1);
        self.last_failure = None;
    }

    /// Record a pass that found the published call graph view's database
    /// locked. The lock says nothing about the inputs, so it is not a failure.
    fn record_view_busy_wait(&mut self) {
        self.state = None;
        self.suspension = None;
        self.last_failure = None;
    }

    fn detail_at(&self, now_ms: u64) -> String {
        if let Some(suspension) = self.suspension.as_ref() {
            return format!(
                "suspended domain={} deaths={} age_s={} reason={}",
                suspension.domain.as_str(),
                suspension.death_count,
                suspension.age_seconds_at(now_ms),
                suspension.reason,
            );
        }
        if let Some(terminal) = self.last_failure.as_deref() {
            return format!(
                "last attempt failed: {terminal} (attempt {}, first at {})",
                self.attempt_count, self.first_attempt_unix
            );
        }
        match self.state {
            Some(InspectBuilderState::Building) => format!(
                "building since {} (age_s={})",
                self.started_unix,
                self.started_at.elapsed().as_secs()
            ),
            Some(other) => other.as_str().to_string(),
            None => InspectBuilderState::Absent.as_str().to_string(),
        }
    }
}

fn unix_millis_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(u128::from(u64::MAX)) as u64
}

fn unix_now_secs() -> u64 {
    unix_millis_now() / 1_000
}

fn run_tier2_pass_with_deadline<T>(
    project_root: &Path,
    category: InspectCategory,
    timeout: Duration,
    permit_slot: Option<Tier2PermitSlot>,
    action: impl FnOnce() -> T,
) -> (T, bool) {
    let cancellation = crate::executor::JobCancellation::new().with_root(project_root);
    let parent_cancellation = crate::executor::current_job_cancellation();
    let deadline_cancellation = cancellation.clone();
    let root = project_root.to_path_buf();
    let timed_out = Arc::new(AtomicBool::new(false));
    let timer_timed_out = Arc::clone(&timed_out);
    let (done_tx, done_rx) = bounded::<()>(1);
    let timer = std::thread::spawn(move || {
        let deadline = Instant::now() + timeout;
        loop {
            if parent_cancellation
                .as_ref()
                .is_some_and(|token| token.cancel_requested_before_commit())
            {
                deadline_cancellation.request_cancel();
                return;
            }
            let now = Instant::now();
            if now >= deadline {
                timer_timed_out.store(true, Ordering::Release);
                deadline_cancellation.request_cancel();
                if let Some(slot) = permit_slot.as_ref() {
                    slot.lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .take();
                }
                crate::slog_warn!(
                    "tier2 pass timeout: root={} category={} timeout_ms={} slot released, pass still running",
                    root.display(),
                    category,
                    timeout.as_millis()
                );
                return;
            }
            let slice = deadline
                .saturating_duration_since(now)
                .min(Duration::from_millis(100));
            match done_rx.recv_timeout(slice) {
                Ok(()) | Err(crossbeam_channel::RecvTimeoutError::Disconnected) => return,
                Err(crossbeam_channel::RecvTimeoutError::Timeout) => {}
            }
        }
    });
    let _cancellation = crate::executor::install_job_cancellation(cancellation);
    let result = action();
    let _ = done_tx.send(());
    let _ = timer.join();
    (result, timed_out.load(Ordering::Acquire))
}

enum BuilderAttemptTerminal {
    Succeeded,
    CacheWriterLeaseWait,
    /// The published call graph view's database was locked.
    ViewBusyWait,
    Failed(String),
    Inconclusive,
}

fn builder_attempt_terminal(outcome: &JobOutcome) -> BuilderAttemptTerminal {
    match outcome {
        JobOutcome::Fresh { payload }
            if payload["status"] == "pending" && payload["code"] == "callgraph_building" =>
        {
            BuilderAttemptTerminal::ViewBusyWait
        }
        JobOutcome::Fresh { payload } if payload["status"] == "pending" => {
            BuilderAttemptTerminal::Inconclusive
        }
        JobOutcome::Fresh { payload } if callgraph_unavailable_payload(payload) => {
            BuilderAttemptTerminal::Failed("callgraph_unavailable".to_string())
        }
        JobOutcome::Fresh { .. } => BuilderAttemptTerminal::Succeeded,
        JobOutcome::Failed { message } if message.contains("writer_lease_timeout:") => {
            BuilderAttemptTerminal::CacheWriterLeaseWait
        }
        JobOutcome::Failed { message } => {
            BuilderAttemptTerminal::Failed(builder_failure_terminal(message))
        }
        JobOutcome::Stale { .. } | JobOutcome::Pending { .. } => {
            BuilderAttemptTerminal::Inconclusive
        }
    }
}

fn callgraph_unavailable_payload(payload: &Value) -> bool {
    payload.get("callgraph_available").and_then(Value::as_bool) == Some(false)
        || payload
            .get("notes")
            .and_then(Value::as_array)
            .is_some_and(|notes| {
                notes
                    .iter()
                    .any(|note| note.as_str() == Some("callgraph_unavailable"))
            })
}

fn builder_failure_terminal(message: &str) -> String {
    if message.contains("callgraph_unavailable") {
        "callgraph_unavailable".to_string()
    } else {
        message
            .lines()
            .next()
            .unwrap_or("failed")
            .chars()
            .take(64)
            .collect()
    }
}

fn wake_cache_writer_if_available(
    identity: &InspectCacheIdentity,
    waits: &Mutex<HashMap<InspectCacheIdentity, (Instant, String)>>,
    caches: &Mutex<HashMap<InspectCacheIdentity, Arc<InspectCache>>>,
) {
    let Some(dir) = identity.sqlite_path.parent() else {
        return;
    };
    if !matches!(
        crate::fs_lock::lock_availability(&crate::root_cache::writer_lease_path(dir)),
        Ok(crate::fs_lock::LockAvailability::Available | crate::fs_lock::LockAvailability::Stale)
    ) {
        return;
    }
    if let Ok(mut waits) = waits.lock() {
        waits.remove(identity);
    }
    if let Ok(mut caches) = caches.lock() {
        // A contended reader's short open cooldown must not hide the release.
        // Outstanding readers retain their Arc and keep serving the snapshot.
        if caches
            .get(identity)
            .is_some_and(|cache| cache.waiting_for_writer())
        {
            caches.remove(identity);
        }
    }
}

#[derive(Debug)]
enum Tier2RunError {
    Failed(String),
    CallgraphBusy,
}

impl From<String> for Tier2RunError {
    fn from(message: String) -> Self {
        Self::Failed(message)
    }
}

impl From<CallGraphStoreError> for Tier2RunError {
    fn from(error: CallGraphStoreError) -> Self {
        if error.is_transient_lock_contention() || matches!(error, CallGraphStoreError::Building) {
            Self::CallgraphBusy
        } else {
            Self::Failed(error.to_string())
        }
    }
}

fn callgraph_pending_payload() -> Value {
    json!({"status": "pending", "reason": "callgraph_building", "complete": false})
}

/// One wait counted in a counter for as long as it lasts.
struct CountedWait<'a>(&'a AtomicUsize);

impl<'a> CountedWait<'a> {
    fn start(counter: &'a AtomicUsize) -> Self {
        counter.fetch_add(1, Ordering::SeqCst);
        Self(counter)
    }
}

impl Drop for CountedWait<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

fn callgraph_pending_result(job: &InspectJob, duration: Duration) -> InspectResult {
    InspectResult::success(
        job,
        InspectScanSuccess {
            scanned_files: Vec::new(),
            contributions: Vec::new(),
            aggregate: callgraph_pending_payload(),
        },
        duration,
    )
}

fn dead_code_store_read_now() -> Instant {
    #[cfg(test)]
    if let Some(now) = guard_tests::callgraph_read_retry_now_for_test() {
        return now;
    }
    Instant::now()
}

fn retry_dead_code_store_read<T>(
    mut read: impl FnMut() -> Result<T, CallGraphStoreError>,
) -> Result<T, CallGraphStoreError> {
    // Even a published WAL generation can briefly reject a read-only open while
    // another handle closes and cleans up its WAL. Readiness probes fail fast;
    // give contention the same short grace period as serving-reader queries.
    let deadline = dead_code_store_read_now() + Duration::from_millis(100);
    loop {
        #[cfg(test)]
        let attempt_started = Instant::now();
        let result = read();
        #[cfg(test)]
        if let Err(error) = &result {
            guard_tests::observe_callgraph_read_error_for_test(
                error,
                attempt_started.elapsed(),
                deadline.saturating_duration_since(Instant::now()),
            );
        }
        match result {
            Err(error)
                if error.is_transient_lock_contention()
                    && dead_code_store_read_now() < deadline =>
            {
                #[cfg(test)]
                guard_tests::notify_callgraph_read_retry_for_test();
                std::thread::sleep(Duration::from_millis(5));
            }
            result => return result,
        }
    }
}

/// Completes the builder registration and sends `Failed` to leftover waiters
/// if a reuse worker returns, panics, or is cancelled without going through the
/// completion router. Admission and exit must be paired; a leftover registration
/// reads as `building` with no running job.
struct Tier2FlightExitGuard<'a> {
    manager: &'a InspectManager,
    key: JobKey,
    identity: Option<Arc<()>>,
}

impl Drop for Tier2FlightExitGuard<'_> {
    fn drop(&mut self) {
        if let Some(identity) = &self.identity {
            self.manager.finish_tier2_flight_if_current(
                &self.key,
                Some(identity),
                JobOutcome::Failed {
                    message: "tier2 reuse worker exited without publishing a result".to_string(),
                },
            );
        }
    }
}

fn cached_tier2_aggregate_usable(
    category: InspectCategory,
    options: &Tier2ReuseOptions,
    aggregate: &Value,
) -> bool {
    if aggregate["status"] == "pending" {
        return false;
    }
    if category == InspectCategory::DeadCode
        && options.allow_callgraph_cold_build
        && aggregate
            .get("callgraph_available")
            .and_then(Value::as_bool)
            == Some(false)
    {
        return false;
    }
    true
}

pub struct InspectManager {
    file_progress: Arc<HashMap<InspectCategory, crate::run_tool_call::progress::FileProgress>>,
    root_work_cancellation: Arc<Mutex<crate::executor::JobCancellation>>,
    dispatcher: InspectDispatcher,
    result_rx: Receiver<InspectResult>,
    #[allow(dead_code)]
    pool: Arc<rayon::ThreadPool>,
    in_flight: Mutex<HashMap<JobKey, InFlightRegistration>>,
    in_flight_changed: Condvar,
    caches: Arc<Mutex<HashMap<InspectCacheIdentity, Arc<InspectCache>>>>,
    cache_writer_waits: Arc<Mutex<HashMap<InspectCacheIdentity, (Instant, String)>>>,
    cache_open_gates: Arc<CacheOpenGates>,
    cache_writer_watches:
        Mutex<HashMap<InspectCacheIdentity, crate::callgraph_store::ReadinessWatcher>>,
    #[cfg(test)]
    cache_writer_watch_observer: Mutex<Option<Sender<CacheWriterWatchObservation>>>,
    /// One root-scoped dead-code graph projection. It is cleared with the
    /// manager's other idle artifacts rather than on a separate timer.
    callgraph_projection: Arc<ProjectionSlot>,
    oxc_facts_cache: Mutex<OxcFactsCache>,
    soft_deadline: Duration,
    #[cfg(test)]
    metrics_wait_for_test: Arc<Mutex<Option<Arc<MetricsWaitControlForTest>>>>,
    next_job_id: AtomicU64,
    heavy_root_work_allowed: Arc<AtomicBool>,
    semantic_cold_seed_active: Arc<AtomicBool>,
    cold_build_limiter: Mutex<Arc<cold_build_limiter::ColdBuildLimiter>>,
    interactive_tier2_permits: Mutex<HashMap<PathBuf, cold_build_limiter::WeakColdBuildPermit>>,
    #[cfg(test)]
    interactive_tier2_acquisitions: AtomicU64,
    /// Inspect refusals (`builder_state=...`) and health's `tier2` field both
    /// read this registry. The waiter map (`in_flight`) fans out completions;
    /// both surfaces treat a category as busy when it has an entry here, and
    /// fall back to the waiter map if the registry is empty.
    builder_states: Arc<Mutex<HashMap<JobKey, BuilderStateEntry>>>,
    completed_build_durations: Mutex<HashMap<InspectCategory, Duration>>,
    automatic_tier2_refresh_allowed: AtomicBool,
    automatic_tier2_skip_logged: AtomicBool,
    /// Monotonic count of Tier-2 completions delivered via the reuse path,
    /// which bypass `result_rx`/`drain_completions`.
    reuse_completions: AtomicU64,
    /// Test observability for distinguishing queued reuse work from a worker that
    /// has actually begun executing it.
    reuse_starts: AtomicU64,
    source_scan_files: AtomicU64,
    /// The data-layer checkouts dead code reads its call graph from while
    /// they are ready; empty unless the process runs the data layer.
    data_layer_dead_code: crate::data_layer::tools::inspect::DeadCodeSources,
    /// When an `aft_inspect` that read the dead-code graph last finished;
    /// `None` once the graph was released, or before any such request.
    last_dead_code_request: Mutex<Option<Instant>>,
    /// Set while a thread is waiting to release the dead-code graph.
    dead_code_release_armed: AtomicBool,
    /// How many dead-code passes are waiting for their call graph right now:
    /// for it to be published, or to catch up with recorded edits. A pass
    /// that is building but not waiting is working out dead code itself.
    dead_code_callgraph_waits: AtomicUsize,
}

#[cfg(test)]
struct MetricsWaitControlForTest {
    non_expiring: bool,
    worker_held: AtomicBool,
    release_tx: Sender<()>,
    release_rx: Receiver<()>,
}

#[cfg(test)]
pub(crate) struct MetricsWaitForTest(Arc<MetricsWaitControlForTest>);

#[cfg(test)]
impl MetricsWaitForTest {
    pub(crate) fn worker_was_held(&self) -> bool {
        self.0.worker_held.load(Ordering::SeqCst)
    }
}

#[cfg(test)]
impl Drop for MetricsWaitForTest {
    fn drop(&mut self) {
        // A failed assertion must not leave the metrics worker parked forever.
        let _ = self.0.release_tx.try_send(());
    }
}

impl InspectManager {
    pub fn new() -> Self {
        Self::with_heavy_root_work_gate(Arc::new(AtomicBool::new(true)))
    }

    pub fn with_heavy_root_work_gate(heavy_root_work_allowed: Arc<AtomicBool>) -> Self {
        Self::with_root_work_gates(heavy_root_work_allowed, Arc::new(AtomicBool::new(false)))
    }

    pub fn with_root_work_gates(
        heavy_root_work_allowed: Arc<AtomicBool>,
        semantic_cold_seed_active: Arc<AtomicBool>,
    ) -> Self {
        Self::with_worker_and_gates(
            default_worker(),
            DEFAULT_SOFT_DEADLINE,
            heavy_root_work_allowed,
            semantic_cold_seed_active,
        )
    }

    #[doc(hidden)]
    pub fn with_worker(worker: InspectWorker, soft_deadline: Duration) -> Self {
        Self::with_worker_and_gate(worker, soft_deadline, Arc::new(AtomicBool::new(true)))
    }

    #[doc(hidden)]
    pub fn with_worker_and_gate(
        worker: InspectWorker,
        soft_deadline: Duration,
        heavy_root_work_allowed: Arc<AtomicBool>,
    ) -> Self {
        Self::with_worker_and_gates(
            worker,
            soft_deadline,
            heavy_root_work_allowed,
            Arc::new(AtomicBool::new(false)),
        )
    }

    fn with_worker_and_gates(
        worker: InspectWorker,
        soft_deadline: Duration,
        heavy_root_work_allowed: Arc<AtomicBool>,
        semantic_cold_seed_active: Arc<AtomicBool>,
    ) -> Self {
        let root_work_cancellation = Arc::new(Mutex::new(crate::executor::JobCancellation::new()));
        let worker_cancellation = Arc::clone(&root_work_cancellation);
        #[cfg(test)]
        let metrics_wait_for_test = Arc::new(Mutex::new(None::<Arc<MetricsWaitControlForTest>>));
        #[cfg(test)]
        let metrics_worker_control = metrics_wait_for_test.clone();
        let caches = Arc::new(Mutex::new(HashMap::new()));
        let cache_writer_waits = Arc::new(Mutex::new(HashMap::new()));
        let cache_open_gates = Arc::new(Mutex::new(HashMap::new()));
        let worker_caches = Arc::clone(&caches);
        let worker_writer_waits = Arc::clone(&cache_writer_waits);
        let worker_open_gates = Arc::clone(&cache_open_gates);
        let file_progress: Arc<HashMap<_, _>> = Arc::new(
            InspectCategory::active()
                .iter()
                .map(|category| {
                    (
                        *category,
                        crate::run_tool_call::progress::FileProgress::default(),
                    )
                })
                .collect(),
        );
        let worker_file_progress = file_progress.clone();
        let guarded_worker: InspectWorker = Arc::new(move |job| {
            let _files = worker_file_progress
                .get(&job.category)
                .map(|progress| progress.enter());
            #[cfg(test)]
            if job.category == InspectCategory::Metrics {
                let control = metrics_worker_control.lock().unwrap().clone();
                if let Some(control) = control {
                    if !control.worker_held.swap(true, Ordering::SeqCst) {
                        let _ = control.release_rx.recv();
                    }
                }
            }
            let token = worker_cancellation
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone();
            let token = token.with_root(&job.project_root);
            let _cancel = crate::executor::install_job_cancellation(token.clone());
            if token.cancel_requested_before_commit() || !job.project_root.is_dir() {
                return InspectResult::failed(
                    &job,
                    "inspect root is unbound or missing",
                    Duration::ZERO,
                );
            }
            let mut result = worker(job);
            // Publication may need a writer lease. Finish it on the inspect
            // worker before notifying the request/reply loop of completion.
            if let Err(message) = Self::store_completion(
                &worker_caches,
                &worker_writer_waits,
                &worker_open_gates,
                &result,
            ) {
                result.outcome = Err(message);
            }
            result
        });
        let dispatcher = InspectDispatcher::new(guarded_worker);
        Self {
            file_progress,
            root_work_cancellation,
            result_rx: dispatcher.result_rx.clone(),
            pool: Arc::clone(&dispatcher.pool),
            dispatcher,
            in_flight: Mutex::new(HashMap::new()),
            in_flight_changed: Condvar::new(),
            caches,
            cache_writer_waits,
            cache_open_gates,
            cache_writer_watches: Mutex::new(HashMap::new()),
            #[cfg(test)]
            cache_writer_watch_observer: Mutex::new(None),
            callgraph_projection: Arc::new(Mutex::new(None)),
            oxc_facts_cache: Mutex::new(OxcFactsCache::new()),
            soft_deadline,
            #[cfg(test)]
            metrics_wait_for_test,
            next_job_id: AtomicU64::new(1),
            heavy_root_work_allowed,
            semantic_cold_seed_active,
            cold_build_limiter: Mutex::new(cold_build_limiter::global_limiter()),
            interactive_tier2_permits: Mutex::new(HashMap::new()),
            data_layer_dead_code: crate::data_layer::tools::inspect::DeadCodeSources::default(),
            last_dead_code_request: Mutex::new(None),
            dead_code_release_armed: AtomicBool::new(false),
            dead_code_callgraph_waits: AtomicUsize::new(0),
            #[cfg(test)]
            interactive_tier2_acquisitions: AtomicU64::new(0),
            builder_states: Arc::new(Mutex::new(HashMap::new())),
            completed_build_durations: Mutex::new(HashMap::new()),
            automatic_tier2_refresh_allowed: AtomicBool::new(true),
            automatic_tier2_skip_logged: AtomicBool::new(false),
            reuse_completions: AtomicU64::new(0),
            reuse_starts: AtomicU64::new(0),
            source_scan_files: AtomicU64::new(0),
        }
    }

    #[cfg(test)]
    pub(crate) fn non_expiring_metrics_wait_for_test(&self) -> MetricsWaitForTest {
        let (release_tx, release_rx) = bounded(1);
        let control = Arc::new(MetricsWaitControlForTest {
            non_expiring: true,
            worker_held: AtomicBool::new(false),
            release_tx,
            release_rx,
        });
        let previous = self
            .metrics_wait_for_test
            .lock()
            .unwrap()
            .replace(control.clone());
        assert!(
            previous.is_none(),
            "metrics wait control already installed for manager"
        );
        MetricsWaitForTest(control)
    }

    fn heavy_root_work_allowed(&self) -> bool {
        self.heavy_root_work_allowed.load(Ordering::SeqCst)
            && !self
                .root_work_cancellation
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .cancel_requested_before_commit()
    }

    pub(crate) fn cancel_root_work(&self) {
        self.root_work_cancellation
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .request_cancel();
    }

    pub(crate) fn set_root_lifecycle(&self, lifecycle: crate::context::SubcLifecycleAdmission) {
        let mut token = self
            .root_work_cancellation
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *token = token.clone().with_lifecycle(lifecycle);
    }

    pub(crate) fn resume_root_work(&self) {
        let mut token = self
            .root_work_cancellation
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if token.cancel_requested_before_commit() {
            *token = token.fresh();
        }
    }

    pub(crate) fn set_cold_build_limiter(
        &self,
        limiter: Arc<cold_build_limiter::ColdBuildLimiter>,
    ) {
        *self
            .cold_build_limiter
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = limiter;
    }

    fn cold_build_limiter(&self) -> Arc<cold_build_limiter::ColdBuildLimiter> {
        Arc::clone(
            &self
                .cold_build_limiter
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        )
    }

    fn acquire_interactive_tier2_permit_slot(&self, job: &InspectJob) -> Option<Tier2PermitSlot> {
        let mut permits = self
            .interactive_tier2_permits
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        permits.retain(|_, permit| permit.upgrade().is_some());
        if let Some(permit) = permits
            .get(&job.project_root)
            .and_then(cold_build_limiter::WeakColdBuildPermit::upgrade)
        {
            return Some(Arc::new(Mutex::new(Some(permit))));
        }

        // All Tier-2 categories in one blocking inspect start concurrently. Keep
        // the root lookup and first acquisition atomic so they clone one permit
        // instead of each consuming process-wide capacity.
        #[cfg(test)]
        self.interactive_tier2_acquisitions
            .fetch_add(1, Ordering::SeqCst);
        let request = cold_build_limiter::ColdBuildAdmissionRequest::for_root(
            job.project_root.display().to_string(),
            format!("inspect:{}:{}", job.project_root.display(), job.job_id),
            cold_build_limiter::ColdBuildAdmissionClass::InspectTriggered,
        );
        let permit = cold_build_limiter::acquire_blocking_while_cancellable_with_limiter(
            &self.cold_build_limiter(),
            "explicit inspect Tier-2 run",
            request,
            || self.heavy_root_work_allowed(),
            || {
                crate::executor::current_job_cancellation()
                    .is_some_and(|token| token.cancel_requested_before_commit())
            },
        )?;
        permits.insert(job.project_root.clone(), permit.downgrade());
        Some(Arc::new(Mutex::new(Some(permit))))
    }

    fn set_builder_state(&self, key: &JobKey, state: InspectBuilderState) {
        if let Ok(mut states) = self.builder_states.lock() {
            if let Some(entry) = states.get_mut(key) {
                entry.begin_attempt(state);
            } else {
                states.insert(key.clone(), BuilderStateEntry::new(state));
            }
        }
    }

    fn clear_builder_state(&self, key: &JobKey) {
        if let Ok(mut states) = self.builder_states.lock() {
            states.remove(key);
        }
    }

    fn record_flight_start(&self, key: &JobKey) {
        self.set_builder_state(key, InspectBuilderState::Building);
    }

    fn record_builder_attempt_outcome(&self, key: &JobKey, outcome: &JobOutcome) {
        let Ok(mut states) = self.builder_states.lock() else {
            return;
        };
        if states
            .get(key)
            .is_some_and(|entry| entry.suspension.is_some())
        {
            return;
        }
        match builder_attempt_terminal(outcome) {
            BuilderAttemptTerminal::Succeeded => {
                states.remove(key);
            }
            BuilderAttemptTerminal::CacheWriterLeaseWait => {
                states
                    .entry(key.clone())
                    .or_insert_with(|| BuilderStateEntry::new(InspectBuilderState::Building))
                    .record_cache_writer_wait();
            }
            BuilderAttemptTerminal::ViewBusyWait => {
                states
                    .entry(key.clone())
                    .or_insert_with(|| BuilderStateEntry::new(InspectBuilderState::Building))
                    .record_view_busy_wait();
            }
            BuilderAttemptTerminal::Failed(terminal) => {
                if let Some(entry) = states.get_mut(key) {
                    entry.record_failure(terminal);
                } else {
                    let mut entry = BuilderStateEntry::new(InspectBuilderState::Building);
                    entry.record_failure(terminal);
                    states.insert(key.clone(), entry);
                }
            }
            BuilderAttemptTerminal::Inconclusive => {
                if let Some(entry) = states.get_mut(key) {
                    entry.state = None;
                    if entry.last_failure.is_none() && entry.attempt_count == 0 {
                        states.remove(key);
                    }
                }
            }
        }
        drop(states);
        if matches!(
            builder_attempt_terminal(outcome),
            BuilderAttemptTerminal::CacheWriterLeaseWait
        ) {
            // Close the race where the holder released before the failed
            // attempt registered its resource wait.
            if let Ok(watches) = self.cache_writer_watches.lock() {
                for identity in watches.keys() {
                    self.wake_cache_writer_if_available(identity);
                }
            }
        }
    }

    fn tier2_flight_exit_guard(&self, key: JobKey) -> Tier2FlightExitGuard<'_> {
        let identity = self
            .in_flight
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&key)
            .map(|registration| Arc::clone(&registration.identity));
        Tier2FlightExitGuard {
            manager: self,
            key,
            identity,
        }
    }

    /// Publish a reuse outcome to the currently registered waiters. Exit guards
    /// also match the registration identity before publishing their failure.
    fn finish_tier2_flight(&self, key: &JobKey, outcome: JobOutcome) {
        self.finish_tier2_flight_if_current(key, None, outcome);
    }

    fn finish_tier2_flight_if_current(
        &self,
        key: &JobKey,
        identity: Option<&Arc<()>>,
        outcome: JobOutcome,
    ) {
        let Some(waiters) = self.take_waiters_after(key, identity, || {
            self.record_builder_attempt_outcome(key, &outcome);
            self.reuse_completions.fetch_add(1, Ordering::SeqCst);
        }) else {
            return;
        };
        Self::deliver_waiters(waiters, outcome);
    }

    fn take_waiters(&self, key: &JobKey, identity: Option<&Arc<()>>) -> Option<Vec<Waiter>> {
        self.take_waiters_after(key, identity, || {})
    }

    fn take_waiters_after(
        &self,
        key: &JobKey,
        identity: Option<&Arc<()>>,
        publish: impl FnOnce(),
    ) -> Option<Vec<Waiter>> {
        let mut in_flight = self
            .in_flight
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // Publication removes the key before the worker exits. Match its retained
        // token under the removal lock so late cleanup cannot consume a new flight.
        if let Some(identity) = identity {
            if !in_flight
                .get(key)
                .is_some_and(|registration| Arc::ptr_eq(&registration.identity, identity))
            {
                return None;
            }
        }
        if !in_flight.contains_key(key) {
            return None;
        }
        // Idle must imply that the terminal state (including the retry pause)
        // is visible. Otherwise an automatic refresh can rerun a failed build
        // between flight removal and publication of its failure.
        publish();
        let waiters = in_flight
            .remove(key)
            .map(|registration| registration.waiters);
        drop(in_flight);
        if waiters.is_some() {
            self.in_flight_changed.notify_all();
        }
        waiters
    }

    fn deliver_waiters(waiters: Vec<Waiter>, outcome: JobOutcome) {
        for waiter in waiters {
            let _ = waiter.tx.send(outcome.clone());
        }
    }

    pub(crate) fn tier2_builder_state(&self, category: InspectCategory) -> InspectBuilderState {
        let key = JobKey::for_project_category(category);
        if let Ok(states) = self.builder_states.lock() {
            if let Some(entry) = states.get(&key) {
                if let Some(state) = entry.state {
                    return state;
                }
            }
        }
        if self
            .in_flight
            .lock()
            .map(|in_flight| in_flight.contains_key(&key))
            .unwrap_or(false)
        {
            InspectBuilderState::Building
        } else {
            InspectBuilderState::Absent
        }
    }

    pub(crate) fn tier2_builder_state_detail(&self, category: InspectCategory) -> String {
        self.tier2_builder_state_detail_at(category, unix_millis_now())
    }

    pub(crate) fn dead_code_build_is_pending(&self) -> bool {
        self.builder_states.lock().ok().is_some_and(|states| {
            states
                .get(&JobKey::for_project_category(InspectCategory::DeadCode))
                .is_some_and(|entry| {
                    matches!(
                        entry.state,
                        Some(
                            InspectBuilderState::Building
                                | InspectBuilderState::QueuedBehindColdBuilds
                        )
                    )
                })
        })
    }

    /// What a dead-code pass still building waits for, as the pending
    /// reason inspect reports: `callgraph_building` while it waits for its
    /// call graph, `dead_code_building` while it works out dead code itself
    /// (or waits for a build slot to). `None` when no pass is building.
    pub(crate) fn dead_code_pending_reason(&self) -> Option<&'static str> {
        if !self.dead_code_build_is_pending() {
            return None;
        }
        Some(
            if self.dead_code_callgraph_waits.load(Ordering::SeqCst) > 0 {
                "callgraph_building"
            } else {
                "dead_code_building"
            },
        )
    }

    /// Marks a dead-code pass as waiting for its call graph, or no longer.
    #[cfg(test)]
    pub(crate) fn set_dead_code_waiting_for_callgraph_for_test(&self, waiting: bool) {
        if waiting {
            self.dead_code_callgraph_waits
                .fetch_add(1, Ordering::SeqCst);
        } else {
            self.dead_code_callgraph_waits
                .fetch_sub(1, Ordering::SeqCst);
        }
    }

    fn record_build_progress(&self, key: &JobKey, progress: impl Into<String>) {
        if let Ok(mut states) = self.builder_states.lock() {
            if let Some(entry) = states.get_mut(key) {
                entry.progress = progress.into();
            }
        }
    }

    pub(crate) fn work_progress(&self, category: InspectCategory) -> Option<(String, u64)> {
        self.builder_states
            .try_lock()
            .ok()?
            .get(&JobKey::for_project_category(category))
            .map(|entry| {
                (
                    entry.progress.clone(),
                    self.file_progress
                        .get(&category)
                        .map_or(0, |progress| progress.completed()),
                )
            })
    }

    /// Keep unfinished analysis separate from current findings. Cached rows are
    /// shown only inside a labelled stale result, never as a verified count.
    pub(crate) fn tier2_incomplete_payload(
        &self,
        snapshot: &InspectSnapshot,
        category: InspectCategory,
        scope: &JobScope,
        kind: &str,
        mut reason: String,
    ) -> Value {
        let key = JobKey::for_project_category(category);
        let mut payload = serde_json::json!({"unavailable": true, "complete": false});
        if let Ok(states) = self.builder_states.lock() {
            if let Some(entry) = states.get(&key).filter(|entry| entry.is_in_flight()) {
                let estimate = self
                    .completed_build_durations
                    .lock()
                    .ok()
                    .and_then(|durations| durations.get(&category).copied())
                    .map(|duration| {
                        duration
                            .saturating_sub(entry.started_at.elapsed())
                            .as_millis() as u64
                    });
                let estimate_text = estimate.map_or_else(
                    || "estimate unavailable (no completed build in this session)".to_string(),
                    |ms| format!("estimated remaining {ms}ms (based on the last completed build; not a deadline)"),
                );
                reason = format!(
                    "{}; progress: {}; {estimate_text}",
                    if kind == "view_pending" {
                        reason
                    } else {
                        entry.detail_at(unix_millis_now())
                    },
                    entry.progress
                );
                payload["building"] = serde_json::json!({
                    "state": entry.state.unwrap().as_str(), "started_at": entry.started_unix,
                    "elapsed_ms": entry.started_at.elapsed().as_millis() as u64,
                    "progress": entry.progress, "estimated_remaining_ms": estimate,
                    "estimate_basis": "last completed build in this session; unavailable on cold start"
                });
            }
        }
        // This cache belongs to the checkout, not its artifact owner. Do not
        // substitute a sibling checkout's aggregate when it is absent.
        if let Ok(Some(cache)) =
            InspectCache::open_readonly(snapshot.inspect_dir.clone(), snapshot.project_root.clone())
        {
            if let Ok(Some(cached)) = cache.latest_aggregate_any_hash(category) {
                if cached.get("callgraph_available").and_then(Value::as_bool) != Some(false) {
                    let generated_at = cache.last_full_run(category).ok().flatten();
                    let age =
                        generated_at.map(|at| unix_now_secs().saturating_sub(at.max(0) as u64));
                    let cached = filter_outcome_for_scope_with_contributions(
                        &self.data_layer_dead_code,
                        None,
                        JobOutcome::Fresh { payload: cached },
                        snapshot,
                        category,
                        &cache,
                        scope,
                    )
                    .payload()
                    .cloned()
                    .unwrap_or(Value::Null);
                    payload["last_complete"] = serde_json::json!({
                        "stale": true, "generated_at": generated_at, "age_s": age, "payload": cached
                    });
                    reason.push_str(&format!(
                        "; last complete result is stale (age_s={})",
                        age.map_or_else(|| "unknown".into(), |age| age.to_string())
                    ));
                }
            }
        }
        payload["gaps"] = serde_json::json!([{"kind": kind, "reason": reason}]);
        payload
    }

    pub(crate) fn tier2_builder_state_detail_at(
        &self,
        category: InspectCategory,
        now_ms: u64,
    ) -> String {
        let key = JobKey::for_project_category(category);
        if let Ok(states) = self.builder_states.lock() {
            if let Some(entry) = states.get(&key) {
                return entry.detail_at(now_ms);
            }
        }
        self.tier2_builder_state(category).as_str().to_string()
    }

    fn builder_state_detail_for_job(&self, job: &InspectJob) -> String {
        if job_is_borrow_only(job) {
            InspectBuilderState::BuildDenied.as_str().to_string()
        } else {
            self.tier2_builder_state_detail(job.category)
        }
    }

    /// Whether any Tier-2 category is registered in the builder registry.
    /// Health uses this instead of published status-bar completeness so the
    /// two surfaces cannot disagree about a live rebuild.
    pub(crate) fn try_tier2_builder_busy(&self) -> Option<bool> {
        let states = self.builder_states.try_lock().ok()?;
        if states
            .iter()
            .any(|(key, entry)| key.category.is_tier2() && entry.is_in_flight())
        {
            return Some(true);
        }
        drop(states);
        self.try_tier2_any_in_flight()
    }

    /// Whether the root has a published call graph a dead_code snapshot can be
    /// built from. Edits pending on a published graph don't count against
    /// it: the snapshot brings them in.
    pub(crate) fn callgraph_ready_for_snapshot(&self, snapshot: &InspectSnapshot) -> bool {
        snapshot.config.indexes.callgraph
            && !self
                .data_layer_dead_code
                .callgraph_busy(&snapshot.project_root)
    }

    pub fn set_automatic_tier2_refresh_allowed(&self, allowed: bool) {
        self.automatic_tier2_refresh_allowed
            .store(allowed, Ordering::SeqCst);
        self.automatic_tier2_skip_logged
            .store(false, Ordering::SeqCst);
    }

    pub fn automatic_tier2_refresh_enabled(&self) -> bool {
        self.automatic_tier2_refresh_allowed.load(Ordering::SeqCst)
    }

    pub fn automatic_tier2_refresh_allowed(&self) -> bool {
        let allowed = self.automatic_tier2_refresh_enabled();
        if !allowed
            && !self
                .automatic_tier2_skip_logged
                .swap(true, Ordering::SeqCst)
        {
            crate::slog_debug!("automatic TODO refresh skipped for linked worktree root");
        }
        allowed
    }

    #[doc(hidden)]
    pub fn inspect_pool_for_test(&self) -> Arc<rayon::ThreadPool> {
        Arc::clone(&self.pool)
    }

    fn category_needs_heavy_root_work(category: InspectCategory) -> bool {
        category != InspectCategory::Diagnostics
    }

    fn heavy_root_work_block_message(category: InspectCategory) -> String {
        format!(
            "inspect category '{category}' is unavailable because heavy project-wide work is disabled for this root"
        )
    }

    pub fn submit_category(
        &self,
        snapshot: InspectSnapshot,
        category: InspectCategory,
        caller_scope: JobScope,
    ) -> JobOutcome {
        self.submit_category_with_callgraph(snapshot, category, caller_scope, None)
    }

    /// Wait for a category until the caller's absolute deadline instead of the
    /// manager's short soft deadline. Blocking inspect uses this path because a
    /// cold scan can sit behind parse-heavy Tier-2 work in the shared pool.
    #[doc(hidden)]
    pub fn submit_category_until(
        &self,
        snapshot: InspectSnapshot,
        category: InspectCategory,
        caller_scope: JobScope,
        deadline: Instant,
    ) -> JobOutcome {
        self.submit_category_with_callgraph_until(snapshot, category, caller_scope, None, deadline)
    }

    pub fn submit_category_with_callgraph(
        &self,
        snapshot: InspectSnapshot,
        category: InspectCategory,
        caller_scope: JobScope,
        callgraph_snapshot: Option<Arc<CallgraphSnapshot>>,
    ) -> JobOutcome {
        self.submit_category_with_callgraph_until(
            snapshot,
            category,
            caller_scope,
            callgraph_snapshot,
            Instant::now() + self.soft_deadline,
        )
    }

    fn submit_category_with_callgraph_until(
        &self,
        snapshot: InspectSnapshot,
        category: InspectCategory,
        caller_scope: JobScope,
        callgraph_snapshot: Option<Arc<CallgraphSnapshot>>,
        deadline: Instant,
    ) -> JobOutcome {
        let wait_started = Instant::now();
        if inspect_category_is_off(&snapshot.config.inspect, category) {
            return JobOutcome::off();
        }
        let wait_budget = deadline.saturating_duration_since(wait_started);
        if !category.is_active() {
            return JobOutcome::Failed {
                message: format!("inspect category '{category}' is disabled in v0.33"),
            };
        }
        if Self::category_needs_heavy_root_work(category) && !self.heavy_root_work_allowed() {
            return JobOutcome::Failed {
                message: Self::heavy_root_work_block_message(category),
            };
        }

        let cache = match self.cache_for_snapshot(&snapshot) {
            Ok(cache) => cache,
            Err(message) => return JobOutcome::Failed { message },
        };
        let key = JobKey::for_category_scope(category, &caller_scope);
        let (waiter_tx, waiter_rx) = bounded(1);

        let wait_snapshot = snapshot.clone();
        match self.enqueue_with_waiter(
            snapshot,
            category,
            caller_scope.clone(),
            key.clone(),
            waiter_tx,
            callgraph_snapshot,
        ) {
            Ok(()) => self.wait_for_outcome(
                key,
                caller_scope,
                cache,
                waiter_rx,
                wait_snapshot,
                deadline,
                wait_started,
                wait_budget,
            ),
            Err(message) => JobOutcome::Failed { message },
        }
    }

    pub fn submit_background(
        &self,
        snapshot: InspectSnapshot,
        category: InspectCategory,
        caller_scope: JobScope,
    ) -> Result<JobKey, String> {
        self.submit_background_with_callgraph(snapshot, category, caller_scope, None)
    }

    pub fn submit_background_with_callgraph(
        &self,
        snapshot: InspectSnapshot,
        category: InspectCategory,
        caller_scope: JobScope,
        callgraph_snapshot: Option<Arc<CallgraphSnapshot>>,
    ) -> Result<JobKey, String> {
        if inspect_category_is_off(&snapshot.config.inspect, category) {
            return Err(format!("inspect.categories.{} is off", category.as_str()));
        }
        if !category.is_active() {
            return Err(format!(
                "inspect category '{category}' is disabled in v0.33"
            ));
        }
        if Self::category_needs_heavy_root_work(category) && !self.heavy_root_work_allowed() {
            return Err(Self::heavy_root_work_block_message(category));
        }
        let key = JobKey::for_category_scope(category, &caller_scope);
        self.enqueue_without_waiter(
            snapshot,
            category,
            caller_scope,
            key.clone(),
            callgraph_snapshot,
        )?;
        Ok(key)
    }

    pub fn tier2_any_in_flight(&self) -> bool {
        self.in_flight
            .lock()
            .map(|in_flight| in_flight.keys().any(|key| key.category.is_tier2()))
            .unwrap_or(false)
    }

    pub(crate) fn try_tier2_any_in_flight(&self) -> Option<bool> {
        self.in_flight
            .try_lock()
            .ok()
            .map(|in_flight| in_flight.keys().any(|key| key.category.is_tier2()))
    }

    /// Hold the in-flight job map for the duration of `while_held`, so a test
    /// can prove a caller does not wait for it.
    #[cfg(test)]
    pub(crate) fn hold_in_flight_lock_for_test(&self, while_held: impl FnOnce()) {
        let _held = self
            .in_flight
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        while_held();
    }

    #[cfg(test)]
    pub(crate) fn set_tier2_in_flight_for_test(&self, category: InspectCategory, in_flight: bool) {
        let key = JobKey::for_project_category(category);
        let mut jobs = self
            .in_flight
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if in_flight {
            jobs.entry(key.clone()).or_default();
            drop(jobs);
            self.record_flight_start(&key);
        } else {
            jobs.remove(&key);
            drop(jobs);
            self.clear_builder_state(&key);
        }
    }

    #[cfg(test)]
    pub(crate) fn record_tier2_attempt_outcome_for_test(
        &self,
        category: InspectCategory,
        outcome: JobOutcome,
    ) {
        let key = JobKey::for_project_category(category);
        {
            let mut jobs = self
                .in_flight
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            jobs.entry(key.clone()).or_default();
        }
        self.record_flight_start(&key);
        self.finish_tier2_flight(&key, outcome);
    }

    /// Release per-project inspect caches so their SQLite readers and writer
    /// leases do not remain open after a root has gone idle. Callers must check
    /// [`Self::tier2_any_in_flight`] first so a running scan never loses its
    /// cache while it is being used.
    pub fn evict_idle_caches(&self) {
        // Drop watches before taking the maps their callbacks may access.
        if let Ok(mut watches) = self.cache_writer_watches.lock() {
            watches.clear();
        }
        if let Ok(mut caches) = self.caches.lock() {
            caches.clear();
        }
        if let Ok(mut waits) = self.cache_writer_waits.lock() {
            waits.clear();
        }
        if let Ok(mut gates) = self.cache_open_gates.lock() {
            gates.clear();
        }
        self.clear_callgraph_projection();
        if let Ok(mut facts) = self.oxc_facts_cache.lock() {
            *facts = OxcFactsCache::new();
        }
        // A new corpus after idle eviction must not inherit the previous
        // root's failed-attempt history.
        if let Ok(mut states) = self.builder_states.lock() {
            states.retain(|_, entry| entry.is_in_flight());
        }
    }

    /// Estimate inspect's resident aggregate maps without waiting on active
    /// scans. SQLite allocations are measured process-wide; OXC fact payload
    /// bytes remain an explicit gap.
    pub fn estimated_memory(&self) -> crate::memory::MemoryEstimate {
        let caches = match self.caches.try_lock() {
            Ok(caches) => caches.values().cloned().collect::<Vec<_>>(),
            Err(_) => return crate::memory::MemoryEstimate::busy(),
        };
        let facts_entries = match self.oxc_facts_cache.try_lock() {
            Ok(facts) => facts.len(),
            Err(_) => return crate::memory::MemoryEstimate::busy(),
        };
        let mut bytes = 0u64;
        let mut memory_aggregates = 0u64;
        for cache in &caches {
            let estimate = cache.estimated_memory();
            let Some(cache_bytes) = estimate.estimated_bytes else {
                return crate::memory::MemoryEstimate::busy();
            };
            bytes = bytes.saturating_add(cache_bytes);
            memory_aggregates = memory_aggregates.saturating_add(
                estimate
                    .counts
                    .get("memory_aggregates")
                    .copied()
                    .unwrap_or(0),
            );
        }
        crate::memory::MemoryEstimate::partial(bytes)
            .count("open_generation_handles", caches.len())
            .count("oxc_fact_entries", facts_entries)
            .count_u64("memory_aggregates", memory_aggregates)
            .gap("oxc_fact_bytes")
    }

    pub(crate) fn dead_code_snapshot_census() -> DeadCodeSnapshotCensus {
        dead_code_snapshot_census()
    }

    /// Estimate the resident full-graph projection used by dead-code scans.
    /// The slot belongs to this root's manager and is dropped on idle eviction.
    pub fn callgraph_projection_estimated_memory(&self) -> crate::memory::MemoryEstimate {
        let projection = match self.callgraph_projection.try_lock() {
            Ok(projection) => projection,
            Err(_) => return crate::memory::MemoryEstimate::busy(),
        };
        let bytes = projection
            .as_ref()
            .map(|projection| projection.estimated_bytes)
            .unwrap_or(0);
        crate::memory::MemoryEstimate::estimated(bytes)
            .count(
                "callgraph_projection_snapshots",
                usize::from(projection.is_some()),
            )
            .count_u64("callgraph_projection_snapshot_bytes", bytes)
    }

    /// The full-graph projection this root's manager holds, if any, so a
    /// memory census that reaches the same snapshot elsewhere (the data
    /// layer's dead-code memo) leaves it to the projection's figure. `None`
    /// when there is none, or when its slot is held: that figure then counts
    /// nothing.
    pub(crate) fn try_callgraph_projection_snapshot(&self) -> Option<Arc<CallgraphSnapshot>> {
        let projection = self.callgraph_projection.try_lock().ok()?;
        projection
            .as_ref()
            .map(|projection| Arc::clone(&projection.snapshot))
    }

    fn observe_callgraph_projection_cost(
        &self,
        project_root: &Path,
        verdict: ProjectionVerdict,
        elapsed: Duration,
    ) {
        if let Ok(mut projection) = self.callgraph_projection.lock() {
            if let Some(cached) = projection
                .as_mut()
                .filter(|cached| cached.identity.project_root == project_root)
            {
                cached.costs.observe(verdict, elapsed);
            }
        }
    }

    fn clear_callgraph_projection(&self) {
        let root = self
            .callgraph_projection
            .lock()
            .ok()
            .and_then(|mut cached| cached.take())
            .map(|cached| cached.identity.project_root);
        if let Some(root) = root {
            if let Ok(mut fleet) = projection_fleet().lock() {
                fleet.forget(&root);
            }
        }
    }

    #[cfg(test)]
    fn build_tier2_callgraph_snapshot_with_refresh(
        &self,
        job: &InspectJob,
        allow_cold_build: bool,
        build_if_missing: bool,
        refresh_paths: &[PathBuf],
    ) -> Option<Arc<CallgraphSnapshot>> {
        self.build_tier2_callgraph_snapshot_with_refresh_and_verdict(
            job,
            allow_cold_build,
            build_if_missing,
            refresh_paths,
        )
        .expect("test snapshot readiness")
        .map(|(snapshot, _, _)| snapshot)
    }

    /// Lets dead code at `project_root` read its call graph from the data
    /// layer's `checkout` whenever that graph is ready, in place of today's
    /// database (see `data_layer::tools::inspect`).
    pub(crate) fn read_dead_code_from_data_layer(
        &self,
        project_root: &Path,
        checkout: &Arc<crate::data_layer::runtime::DataCheckout>,
    ) {
        self.data_layer_dead_code.bind(project_root, checkout);
    }

    /// Note that an `aft_inspect` which read the dead-code graph finished at
    /// `now`, and make sure a thread will release the graph once it has gone
    /// [`DEAD_CODE_GRAPH_IDLE_RELEASE`] without another such request.
    pub(crate) fn note_dead_code_request(self: &Arc<Self>, now: Instant) {
        self.record_dead_code_request(now);
        if self.dead_code_release_armed.swap(true, Ordering::SeqCst) {
            return;
        }
        let manager = Arc::downgrade(self);
        let spawned = std::thread::Builder::new()
            .name("aft-dead-code-release".into())
            .spawn(move || loop {
                // Hold the manager only while checking, so dropping the
                // manager ends the thread at its next wake-up.
                let Some(current) = manager.upgrade() else {
                    return;
                };
                match current.release_idle_dead_code_graph(Instant::now()) {
                    DeadCodeGraphRelease::KeepFor(wait) => {
                        drop(current);
                        std::thread::sleep(wait);
                    }
                    DeadCodeGraphRelease::Released | DeadCodeGraphRelease::NotHeld => {
                        current
                            .dead_code_release_armed
                            .store(false, Ordering::SeqCst);
                        // A request noted while this thread was deciding found
                        // it armed and started none; keep watching for it.
                        let pending = current
                            .last_dead_code_request
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .is_some();
                        if !pending || current.dead_code_release_armed.swap(true, Ordering::SeqCst)
                        {
                            return;
                        }
                    }
                }
            });
        if spawned.is_err() {
            self.dead_code_release_armed.store(false, Ordering::SeqCst);
        }
    }

    /// Record a dead-code request at `now` without arming the release thread.
    pub(crate) fn record_dead_code_request(&self, now: Instant) {
        *self
            .last_dead_code_request
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(now);
    }

    /// Release the dead-code graph once [`DEAD_CODE_GRAPH_IDLE_RELEASE`] has
    /// passed since the last dead-code request, as of `now`: the call-graph
    /// projection, the Oxc facts and the dead-code snapshot each bound
    /// checkout's view keeps. A dead-code or unused-exports run still in
    /// flight keeps the graph until it finishes.
    pub(crate) fn release_idle_dead_code_graph(&self, now: Instant) -> DeadCodeGraphRelease {
        let mut last = self
            .last_dead_code_request
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(requested_at) = *last else {
            return DeadCodeGraphRelease::NotHeld;
        };
        let idle = now.saturating_duration_since(requested_at);
        if idle < DEAD_CODE_GRAPH_IDLE_RELEASE {
            return DeadCodeGraphRelease::KeepFor(DEAD_CODE_GRAPH_IDLE_RELEASE - idle);
        }
        let reading_graph = self.in_flight.lock().is_ok_and(|in_flight| {
            in_flight.keys().any(|key| {
                matches!(
                    key.category,
                    InspectCategory::DeadCode | InspectCategory::UnusedExports
                )
            })
        });
        if reading_graph {
            return DeadCodeGraphRelease::KeepFor(Duration::from_secs(60));
        }
        *last = None;
        drop(last);
        self.clear_callgraph_projection();
        if let Ok(mut facts) = self.oxc_facts_cache.lock() {
            *facts = OxcFactsCache::new();
        }
        let snapshots = self.data_layer_dead_code.release_dead_code_snapshots();
        crate::slog_info!(
            "dead-code graph released after {} idle minutes: checkout snapshots={snapshots}",
            DEAD_CODE_GRAPH_IDLE_RELEASE.as_secs() / 60
        );
        // Return what the graph held to the system rather than leaving it as
        // free allocator space.
        crate::memory::spawn_allocator_slack_relief_if_due(Instant::now());
        DeadCodeGraphRelease::Released
    }

    /// Build the dead-code snapshot and report how it was produced, so the
    /// `perf tier2 phases` line can name the projection verdict instead of
    /// leaving the operator to infer it from timings.
    fn build_tier2_callgraph_snapshot_with_refresh_and_verdict(
        &self,
        job: &InspectJob,
        allow_cold_build: bool,
        build_if_missing: bool,
        refresh_paths: &[PathBuf],
    ) -> Result<Option<(Arc<CallgraphSnapshot>, ProjectionVerdict, Duration)>, CallGraphStoreError>
    {
        // While this waits, a dead-code pass that inspect stops waiting for
        // is reported as waiting for the call graph, not as working out dead
        // code (see `dead_code_pending_reason`).
        let _waiting = (job.category == InspectCategory::DeadCode)
            .then(|| CountedWait::start(&self.dead_code_callgraph_waits));
        build_tier2_callgraph_snapshot_with_refresh_inner(
            job,
            allow_cold_build,
            build_if_missing,
            refresh_paths,
            Some(self),
        )
    }

    /// Whether completed scan results are waiting in the channel. Used by the
    /// maintenance scheduler to skip enqueueing a completion drain with no work.
    pub fn has_pending_completions(&self) -> bool {
        !self.result_rx.is_empty()
    }

    pub fn drain_completions(&self) -> usize {
        let mut drained = 0usize;
        while let Ok(result) = self.result_rx.try_recv() {
            self.route_completion(result);
            drained += 1;
        }
        drained
    }

    pub fn discard_completions(&self) -> usize {
        let mut discarded = 0usize;
        while let Ok(result) = self.result_rx.try_recv() {
            let outcome = JobOutcome::Failed {
                message: "inspect job cancelled because its project root was unbound".to_string(),
            };
            self.record_builder_attempt_outcome(&result.key, &outcome);
            if let Some(waiters) = self.take_waiters(&result.key, None) {
                Self::deliver_waiters(waiters, outcome);
            }
            discarded += 1;
        }
        discarded
    }

    pub fn cache_for_snapshot(
        &self,
        snapshot: &InspectSnapshot,
    ) -> Result<Arc<InspectCache>, String> {
        self.cache_for_paths(snapshot.inspect_dir.clone(), snapshot.project_root.clone())
    }

    /// Latest project-wide TODO count, or `None` when no project-scoped todos
    /// scan has completed yet. Todos is a Tier-1 category whose result lives in
    /// the cache's in-memory aggregates rather than the Tier-2 table.
    pub fn latest_project_todos_count(
        &self,
        inspect_dir: PathBuf,
        project_root: PathBuf,
    ) -> Option<usize> {
        let key = JobKey::for_category_scope(
            InspectCategory::Todos,
            &JobScope::for_project(project_root.clone()),
        );
        let cache = self.resident_cache_for_paths(inspect_dir, project_root)?;
        cache
            .get_aggregated(&key)
            .ok()
            .flatten()?
            .get("count")
            .and_then(serde_json::Value::as_u64)
            .map(|count| count as usize)
    }

    fn cache_identity(
        inspect_dir: PathBuf,
        project_root: PathBuf,
    ) -> (PathBuf, InspectCacheIdentity) {
        let project_key = crate::path_identity::project_scope_key(&project_root);
        let inspect_dir = if inspect_dir
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name == project_key)
        {
            inspect_dir
        } else {
            inspect_dir.join(&project_key)
        };
        let identity = InspectCacheIdentity {
            sqlite_path: inspect_dir.join(format!("{project_key}.current")),
            project_root: project_root.clone(),
        };
        (inspect_dir, identity)
    }

    fn resident_cache_for_paths(
        &self,
        inspect_dir: PathBuf,
        project_root: PathBuf,
    ) -> Option<Arc<InspectCache>> {
        let (_, identity) = Self::cache_identity(inspect_dir, project_root);
        // Status is an observation, not cache initialization. A cold or busy
        // manager leaves the counts unknown until a scan supplies an aggregate.
        self.caches.try_lock().ok()?.get(&identity).cloned()
    }

    pub fn cache_for_paths(
        &self,
        inspect_dir: PathBuf,
        project_root: PathBuf,
    ) -> Result<Arc<InspectCache>, String> {
        let result = Self::open_cache_for_paths(
            &self.caches,
            &self.cache_writer_waits,
            &self.cache_open_gates,
            inspect_dir.clone(),
            project_root.clone(),
        );
        if result.as_ref().map_or_else(
            |message| message.contains("writer_lease_timeout:"),
            |cache| cache.waiting_for_writer(),
        ) {
            let (dir, identity) = Self::cache_identity(inspect_dir, project_root);
            self.watch_cache_writer(&dir, identity);
        }
        result
    }

    fn wake_cache_writer_if_available(&self, identity: &InspectCacheIdentity) {
        wake_cache_writer_if_available(identity, &self.cache_writer_waits, &self.caches);
    }

    fn watch_cache_writer(&self, dir: &Path, identity: InspectCacheIdentity) {
        let Ok(mut watches) = self.cache_writer_watches.lock() else {
            return;
        };
        if watches.contains_key(&identity) {
            return;
        }
        let Ok(dir) = std::fs::canonicalize(dir) else {
            return;
        };
        let lease = crate::root_cache::writer_lease_path(&dir);
        let waits = Arc::clone(&self.cache_writer_waits);
        let caches = Arc::clone(&self.caches);
        let watched_identity = identity.clone();
        #[cfg(test)]
        let observer = self.cache_writer_watch_observer.lock().unwrap().clone();
        #[cfg(test)]
        let event_observer = observer.clone();
        let watcher = crate::callgraph_store::watch_readiness_directory(&dir, move |event| {
            if let Ok(event) = event {
                let release = crate::callgraph_store::refresh_lease_release_event(&event, &lease);
                if release {
                    wake_cache_writer_if_available(&watched_identity, &waits, &caches);
                }
                #[cfg(test)]
                if let Some(observer) = &event_observer {
                    let _ = observer.send(CacheWriterWatchObservation::Activity {
                        release,
                        detail: format!(
                            "event={event:?} lease={} exists={}",
                            lease.display(),
                            lease.exists()
                        ),
                    });
                }
            }
        });
        match watcher {
            Ok(watcher) => {
                watches.insert(identity, watcher);
                #[cfg(test)]
                if let Some(observer) = observer {
                    let _ = observer.send(CacheWriterWatchObservation::Subscribed);
                }
            }
            Err(error) => {
                #[cfg(test)]
                if let Some(observer) = observer {
                    let _ = observer.send(CacheWriterWatchObservation::Failed(error.to_string()));
                }
                crate::slog_debug!(
                    "inspect lease readiness watch unavailable: {}; using retry timer",
                    error
                );
            }
        }
    }

    fn open_cache_for_paths(
        cache_map: &Mutex<HashMap<InspectCacheIdentity, Arc<InspectCache>>>,
        writer_waits: &Mutex<HashMap<InspectCacheIdentity, (Instant, String)>>,
        open_gates: &CacheOpenGates,
        inspect_dir: PathBuf,
        project_root: PathBuf,
    ) -> Result<Arc<InspectCache>, String> {
        let (inspect_dir, identity) = Self::cache_identity(inspect_dir, project_root.clone());
        let gate = {
            let mut gates = open_gates
                .lock()
                .map_err(|_| "inspect manager open gate map lock poisoned".to_string())?;
            gates.retain(|_, gate| gate.strong_count() > 0);
            if let Some(gate) = gates.get(&identity).and_then(Weak::upgrade) {
                gate
            } else {
                let gate = Arc::new(Mutex::new(()));
                gates.insert(identity.clone(), Arc::downgrade(&gate));
                gate
            }
        };
        // Serialize initialization of one scope's SQLite generation, not other
        // scopes or status readers of the resident map.
        let _opening = gate
            .lock()
            .map_err(|_| "inspect cache open gate poisoned".to_string())?;
        if let Some(cache) = cache_map
            .lock()
            .map_err(|_| "inspect manager cache map lock poisoned".to_string())?
            .get(&identity)
            .filter(|cache| !cache.writer_retry_due())
            .cloned()
        {
            return Ok(cache);
        }
        {
            let mut waits = writer_waits
                .lock()
                .map_err(|_| "inspect manager writer wait map lock poisoned".to_string())?;
            waits.retain(|_, (retry_at, _)| Instant::now() < *retry_at);
            if let Some((_, message)) = waits.get(&identity) {
                return Err(message.clone());
            }
        }
        // Opening can wait on another process. Never keep the resident map
        // locked while doing filesystem or SQLite work for a cold scope.
        let cache = match InspectCache::open(inspect_dir, project_root) {
            Ok(cache) => Arc::new(cache),
            Err(error) => {
                let message = format!("failed to open inspect cache: {error}");
                if matches!(error, InspectCacheError::WriterLeaseTimeout) {
                    writer_waits
                        .lock()
                        .map_err(|_| "inspect manager writer wait map lock poisoned".to_string())?
                        .insert(
                            identity,
                            (Instant::now() + WRITER_RETRY_BACKOFF, message.clone()),
                        );
                }
                return Err(message);
            }
        };
        let mut caches = cache_map
            .lock()
            .map_err(|_| "inspect manager cache map lock poisoned".to_string())?;
        if let Some(existing) = caches
            .get(&identity)
            .filter(|cache| !cache.writer_retry_due())
        {
            return Ok(Arc::clone(existing));
        }
        caches.insert(identity, Arc::clone(&cache));
        Ok(cache)
    }

    fn oxc_result_for_scan(
        &self,
        job: &InspectJob,
        files: &[PathBuf],
        force_reparse_files: &[PathBuf],
    ) -> Result<Option<OxcEngineResult>, String> {
        self.source_scan_files
            .fetch_add(files.len() as u64, Ordering::Relaxed);
        if !category_uses_oxc(job.category) {
            return Ok(None);
        }
        if job.category == InspectCategory::DeadCode && job.callgraph_snapshot.is_none() {
            return Ok(None);
        }

        let public_api_entries =
            crate::inspect::entry_points::resolve_entry_points(&job.project_root);
        let entry_points = if job.category == InspectCategory::DeadCode {
            job.callgraph_snapshot
                .as_ref()
                .map(|snapshot| snapshot.entry_points.iter().cloned().collect::<Vec<_>>())
                .unwrap_or_default()
        } else {
            Vec::new()
        };
        let options = AnalyzeOptions {
            entry_points,
            public_api_files: public_api_entries.public_api_files(),
            executable_root_exports: public_api_entries.executable_root_exports(),
            force_reparse_files: force_reparse_files.to_vec(),
            entry_reachability: job.category == InspectCategory::DeadCode,
        };

        let mut cache = self
            .oxc_facts_cache
            .lock()
            .map_err(|_| "inspect oxc facts cache lock poisoned".to_string())?;
        let _files = self
            .file_progress
            .get(&job.category)
            .map(|progress| progress.enter());
        analyze_files_with_cache(&job.project_root, files, options, &mut cache)
            .map(Some)
            .map_err(|message| format!("oxc analyze failed: {message}"))
    }

    fn scan_with_file_progress(
        &self,
        job: &InspectJob,
        oxc: Option<&OxcEngineResult>,
    ) -> InspectResult {
        let _files = self
            .file_progress
            .get(&job.category)
            .map(|progress| progress.enter());
        run_tier2_scan(job, oxc)
    }

    /// [`Self::scan_with_file_progress`] for a scan whose files' facts are
    /// stored and rolled up later with every other file's: dead code then
    /// skips working out an aggregate of these files alone, which nothing
    /// reads.
    fn scan_facts_with_file_progress(
        &self,
        job: &InspectJob,
        oxc: Option<&OxcEngineResult>,
    ) -> InspectResult {
        if job.category != InspectCategory::DeadCode {
            return self.scan_with_file_progress(job, oxc);
        }
        let _files = self
            .file_progress
            .get(&job.category)
            .map(|progress| progress.enter());
        super::scanners::dead_code::run_dead_code_facts_scan_with_oxc(job, oxc)
    }

    pub fn tier2_run_with_reuse(
        &self,
        snapshot: InspectSnapshot,
        category: InspectCategory,
        caller_scope: JobScope,
        callgraph_snapshot: Option<Arc<CallgraphSnapshot>>,
    ) -> JobOutcome {
        if inspect_category_is_off(&snapshot.config.inspect, category) {
            return JobOutcome::off();
        }
        if let Err(outcome) = validate_tier2_read_category(category) {
            return outcome;
        }
        if !self.heavy_root_work_allowed() {
            return JobOutcome::Failed {
                message: Self::heavy_root_work_block_message(category),
            };
        }
        let cache = match self.cache_for_snapshot(&snapshot) {
            Ok(cache) => cache,
            Err(message) => return JobOutcome::Failed { message },
        };
        let job = self.tier2_reuse_job(snapshot.clone(), category, callgraph_snapshot);
        let key = job.key.clone();
        let (waiter_tx, waiter_rx) = bounded(1);
        let claimed = match self.register_tier2_reuse_waiter(&key, waiter_tx) {
            Ok(claimed) => claimed,
            Err(message) => return JobOutcome::Failed { message },
        };

        if claimed {
            let _flight = self.tier2_flight_exit_guard(key.clone());
            let result = self.tier2_run_with_reuse_job_result_catching(
                job,
                Tier2ReuseOptions::default(),
                None,
            );
            self.route_tier2_reuse_completion(result);
        }

        match waiter_rx.recv() {
            Ok(outcome) => filter_outcome_for_scope_with_contributions(
                &self.data_layer_dead_code,
                Some(self),
                outcome,
                &snapshot,
                category,
                cache.as_ref(),
                &caller_scope,
            ),
            Err(_) => JobOutcome::Failed {
                message: "inspect Tier-2 waiter dropped without a terminal outcome".to_string(),
            },
        }
    }

    /// Run a Tier-2 category to a terminal outcome for an explicit inspect.
    ///
    /// The blocking inspect path must not turn an unfinished reuse job into a
    /// partial response. A caller either receives the completed aggregate or a
    /// failure from the worker; it never receives a timeout-shaped `Pending`.
    pub fn tier2_run_with_reuse_blocking(
        self: &Arc<Self>,
        snapshot: InspectSnapshot,
        category: InspectCategory,
        caller_scope: JobScope,
    ) -> JobOutcome {
        self.tier2_run_with_reuse_blocking_once(snapshot, category, caller_scope, false)
    }

    /// Run a Tier-2 category for a blocking request that requires fresh results.
    /// Unlike compatibility callers, this retries a temporarily unavailable
    /// callgraph instead of accepting that incomplete scan as the final result.
    pub fn tier2_run_with_reuse_blocking_fresh(
        self: &Arc<Self>,
        snapshot: InspectSnapshot,
        category: InspectCategory,
        caller_scope: JobScope,
    ) -> JobOutcome {
        let first = self.tier2_run_with_reuse_blocking_once(
            snapshot.clone(),
            category,
            caller_scope.clone(),
            category == InspectCategory::DeadCode,
        );
        if category == InspectCategory::DeadCode
            && first.payload().is_some_and(|payload| {
                payload.get("callgraph_available").and_then(Value::as_bool) == Some(false)
            })
        {
            // A blocking caller can attach to a background scan that started
            // before the callgraph was ready. Retry once under the blocking
            // policy so that transient result cannot become the terminal payload.
            return self.tier2_run_with_reuse_blocking_once(snapshot, category, caller_scope, true);
        }
        first
    }

    fn tier2_run_with_reuse_blocking_once(
        self: &Arc<Self>,
        snapshot: InspectSnapshot,
        category: InspectCategory,
        caller_scope: JobScope,
        require_callgraph_snapshot: bool,
    ) -> JobOutcome {
        if inspect_category_is_off(&snapshot.config.inspect, category) {
            return JobOutcome::off();
        }
        if let Err(outcome) = validate_tier2_read_category(category) {
            return outcome;
        }
        if !self.heavy_root_work_allowed() {
            return JobOutcome::Failed {
                message: Self::heavy_root_work_block_message(category),
            };
        }
        let cache = match self.cache_for_snapshot(&snapshot) {
            Ok(cache) => cache,
            Err(message) => return JobOutcome::Failed { message },
        };

        let job = self.tier2_reuse_job(snapshot.clone(), category, None);
        let key = job.key.clone();
        let (waiter_tx, waiter_rx) = bounded(1);
        let claimed = match self.register_tier2_reuse_waiter(&key, waiter_tx) {
            Ok(claimed) => claimed,
            Err(message) => return JobOutcome::Failed { message },
        };
        if claimed {
            self.spawn_tier2_reuse_job(
                job,
                Tier2ReuseOptions {
                    require_callgraph_snapshot,
                    interactive: true,
                    ..Tier2ReuseOptions::default()
                },
            );
        }

        self.wait_for_tier2_reuse(&key, &caller_scope, cache.as_ref(), waiter_rx, &snapshot)
    }

    fn register_tier2_reuse_waiter(
        &self,
        key: &JobKey,
        waiter_tx: WaiterTx,
    ) -> Result<bool, String> {
        let mut in_flight = self
            .in_flight
            .lock()
            .map_err(|_| "inspect in-flight map lock poisoned".to_string())?;
        if let Some(registration) = in_flight.get_mut(key) {
            registration.waiters.push(Waiter { tx: waiter_tx });
            self.in_flight_changed.notify_all();
            return Ok(false);
        }

        in_flight.insert(
            key.clone(),
            InFlightRegistration::new(vec![Waiter { tx: waiter_tx }]),
        );
        drop(in_flight);
        self.record_flight_start(key);
        Ok(true)
    }

    fn spawn_tier2_reuse_job(self: &Arc<Self>, job: InspectJob, options: Tier2ReuseOptions) {
        // Rebinds retain the persisted contribution cache. Let quick reuse prove
        // that cache before joining the cold-build queue, so an unchanged root can
        // answer immediately even while unrelated background builds own the slots.
        self.record_flight_start(&job.key);
        let manager = Arc::clone(self);
        let pool = Arc::clone(&self.pool);
        let cancellation = crate::executor::current_job_cancellation();
        pool.spawn_fifo(move || {
            let _cancellation = cancellation.map(crate::executor::install_job_cancellation);
            let _flight = manager.tier2_flight_exit_guard(job.key.clone());
            let result = manager.tier2_run_with_reuse_job_result_catching(job, options, None);
            manager.route_tier2_reuse_completion(result);
        });
    }

    fn wait_for_tier2_reuse(
        &self,
        key: &JobKey,
        caller_scope: &JobScope,
        cache: &(impl InspectCacheRead + ?Sized),
        waiter_rx: Receiver<JobOutcome>,
        snapshot: &InspectSnapshot,
    ) -> JobOutcome {
        match waiter_rx.recv() {
            // Scoped completions need the shared scan's revision-checked graph
            // and contribution-checked rollup, not a full projection per waiter.
            Ok(outcome) => filter_outcome_for_scope_with_contributions(
                &self.data_layer_dead_code,
                Some(self),
                outcome,
                snapshot,
                key.category,
                cache,
                caller_scope,
            ),
            Err(_) => JobOutcome::Failed {
                message: "inspect Tier-2 worker disconnected before completion".to_string(),
            },
        }
    }

    /// Read-only Tier 2 aggregate lookup for `aft_inspect`. Does NOT run any
    /// scanner — returns the latest cached aggregate if present and verifies
    /// its contribution freshness so warm cache hits are reported as fresh.
    /// This is the non-blocking variant intended for the synchronous `inspect`
    /// command path; Tier 2 scans run only from an explicit `aft_inspect`
    /// through the blocking variants.
    pub fn tier2_read_cached(
        &self,
        snapshot: InspectSnapshot,
        category: InspectCategory,
        caller_scope: JobScope,
    ) -> JobOutcome {
        if inspect_category_is_off(&snapshot.config.inspect, category) {
            return JobOutcome::off();
        }
        if let Err(outcome) = validate_tier2_read_category(category) {
            return outcome;
        }
        if !self.heavy_root_work_allowed() {
            return JobOutcome::Failed {
                message: Self::heavy_root_work_block_message(category),
            };
        }
        let cache = match self.cache_for_snapshot(&snapshot) {
            Ok(cache) => cache,
            Err(message) => return JobOutcome::Failed { message },
        };
        self.tier2_read_cached_from_cache(&snapshot, category, &caller_scope, cache.as_ref())
    }

    pub fn tier2_read_cached_readonly(
        &self,
        snapshot: InspectSnapshot,
        category: InspectCategory,
        caller_scope: JobScope,
    ) -> JobOutcome {
        if inspect_category_is_off(&snapshot.config.inspect, category) {
            return JobOutcome::off();
        }
        if let Err(outcome) = validate_tier2_read_category(category) {
            return outcome;
        }
        if !self.heavy_root_work_allowed() {
            return JobOutcome::Failed {
                message: Self::heavy_root_work_block_message(category),
            };
        }
        let key = JobKey::for_project_category(category);
        let in_flight = self
            .in_flight
            .lock()
            .map(|guard| guard.contains_key(&key))
            .unwrap_or(false);
        let cache = match InspectCache::open_readonly(
            snapshot.inspect_dir.clone(),
            snapshot.project_root.clone(),
        ) {
            Ok(Some(cache)) => cache,
            Ok(None) => return JobOutcome::pending(in_flight),
            Err(error) => {
                return JobOutcome::Failed {
                    message: error.to_string(),
                }
            }
        };
        self.tier2_read_cached_from_cache(&snapshot, category, &caller_scope, &cache)
    }

    fn tier2_read_cached_from_cache(
        &self,
        snapshot: &InspectSnapshot,
        category: InspectCategory,
        caller_scope: &JobScope,
        cache: &(impl InspectCacheRead + ?Sized),
    ) -> JobOutcome {
        let key = JobKey::for_project_category(category);
        let in_flight = self
            .in_flight
            .lock()
            .map(|guard| guard.contains_key(&key))
            .unwrap_or(false);
        match cache.get_aggregated_for_config(&key, snapshot.config.as_ref()) {
            Ok(Some(payload)) => {
                match self.tier2_cached_aggregate_is_fresh(snapshot, category, cache) {
                    Ok(true) => filter_outcome_for_scope_with_contributions(
                        &self.data_layer_dead_code,
                        // Only verified source inventories may reuse a rollup.
                        // Stale/last-complete reads still reconstruct it below.
                        Some(self),
                        JobOutcome::Fresh { payload },
                        snapshot,
                        category,
                        cache,
                        caller_scope,
                    ),
                    Ok(false) => filter_outcome_for_scope_with_contributions(
                        &self.data_layer_dead_code,
                        None,
                        JobOutcome::Stale {
                            cached: Some(payload),
                            in_flight,
                        },
                        snapshot,
                        category,
                        cache,
                        caller_scope,
                    ),
                    Err(message) => JobOutcome::Failed { message },
                }
            }
            Ok(None) => match cache.latest_aggregate_any_hash(category) {
                Ok(Some(payload)) => filter_outcome_for_scope_with_contributions(
                    &self.data_layer_dead_code,
                    None,
                    JobOutcome::Stale {
                        cached: Some(payload),
                        in_flight,
                    },
                    snapshot,
                    category,
                    cache,
                    caller_scope,
                ),
                Ok(None) => JobOutcome::pending(in_flight),
                Err(error) => JobOutcome::Failed {
                    message: error.to_string(),
                },
            },
            Err(error) => JobOutcome::Failed {
                message: error.to_string(),
            },
        }
    }

    fn tier2_cached_aggregate_is_fresh(
        &self,
        snapshot: &InspectSnapshot,
        category: InspectCategory,
        cache: &(impl InspectCacheRead + ?Sized),
    ) -> Result<bool, String> {
        let cached_records = load_contribution_freshness(cache, category)?;
        let cached_relative = cached_records
            .iter()
            .map(freshness_record_relative_key)
            .collect::<BTreeSet<_>>();

        // The project walk is part of every identity check, including a negative
        // verdict. It detects additions and removals that per-record metadata
        // cannot observe, and gives all callers the same gitignore-aware file set.
        let project_scope = JobScope::for_project(snapshot.project_root.clone());
        let project_files = scope_files(&snapshot.project_root, &project_scope);
        let current_by_relative = current_project_files(&snapshot.project_root, &project_files);

        for record in &cached_records {
            #[cfg(test)]
            FRESHNESS_RECORD_VISITS.with(|visits| visits.set(visits.get() + 1));
            let absolute = if record.file_path.is_absolute() {
                record.file_path.clone()
            } else {
                snapshot.project_root.join(&record.file_path)
            };
            match verify_contribution_file(&absolute, &record.freshness) {
                ContributionFreshness::Fresh { .. } => {}
                ContributionFreshness::Stale | ContributionFreshness::Deleted => {
                    return Ok(false);
                }
            }
        }

        Ok(current_by_relative.len() == cached_relative.len()
            && current_by_relative
                .keys()
                .all(|relative| cached_relative.contains(relative)))
    }

    #[doc(hidden)]
    pub fn tier2_run_with_reuse_result(
        &self,
        snapshot: InspectSnapshot,
        category: InspectCategory,
        callgraph_snapshot: Option<Arc<CallgraphSnapshot>>,
    ) -> InspectResult {
        let job = self.tier2_reuse_job(snapshot, category, callgraph_snapshot);
        self.tier2_run_with_reuse_job_result(job)
    }

    fn tier2_run_with_reuse_job_result(&self, job: InspectJob) -> InspectResult {
        self.tier2_run_with_reuse_job_result_with_options(
            job,
            Tier2ReuseOptions {
                return_contributions: true,
                ..Tier2ReuseOptions::default()
            },
            None,
        )
    }

    fn tier2_run_with_reuse_job_result_catching(
        &self,
        job: InspectJob,
        options: Tier2ReuseOptions,
        permit_slot: Option<Tier2PermitSlot>,
    ) -> InspectResult {
        let started = Instant::now();
        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            self.tier2_run_with_reuse_job_result_with_options(job.clone(), options, permit_slot)
        })) {
            Ok(result) => result,
            Err(_) => InspectResult::failed(
                &job,
                "tier2 reuse worker panicked before completion",
                started.elapsed(),
            ),
        }
    }

    fn tier2_run_with_reuse_job_result_with_options(
        &self,
        mut job: InspectJob,
        mut options: Tier2ReuseOptions,
        permit_slot: Option<Tier2PermitSlot>,
    ) -> InspectResult {
        let started = Instant::now();
        if inspect_category_is_off(&job.config.inspect, job.category) {
            return InspectResult::success(
                &job,
                InspectScanSuccess {
                    scanned_files: Vec::new(),
                    contributions: Vec::new(),
                    aggregate: serde_json::json!({"off":true,"complete":true}),
                },
                Duration::ZERO,
            );
        }
        self.reuse_starts.fetch_add(1, Ordering::SeqCst);
        panic_tier2_reuse_for_debug(&job);
        if !job.category.is_active() {
            let result = InspectResult::failed(
                &job,
                format!("inspect category '{}' is disabled in v0.33", job.category),
                started.elapsed(),
            );
            log_tier2_benchmark_category_end(&result);
            return result;
        }
        if !job.category.is_tier2() {
            let result = InspectResult::failed(
                &job,
                format!(
                    "inspect category '{}' is not a Tier 2 category",
                    job.category
                ),
                started.elapsed(),
            );
            log_tier2_benchmark_category_end(&result);
            return result;
        }

        if !job.inspect_writer {
            let result = InspectResult::failed(
                &job,
                "inspect writer capability is unavailable for this read-only cache path",
                started.elapsed(),
            );
            log_tier2_benchmark_category_end(&result);
            return result;
        }

        // A root whose first call graph isn't published yet has nothing to
        // analyse; wait for that build as any exact reader does (the inspect
        // call's own deadline may still answer pending first, naming the call
        // graph), and answer pending only if the graph is still missing.
        if job.category == InspectCategory::DeadCode
            && job.config.indexes.callgraph
            && self.data_layer_dead_code.callgraph_busy(&job.project_root)
        {
            let still_missing = {
                let _waiting = CountedWait::start(&self.dead_code_callgraph_waits);
                self.data_layer_dead_code
                    .wait_for_first_callgraph(&job.project_root)
            };
            if still_missing {
                return callgraph_pending_result(&job, started.elapsed());
            }
        }

        let project_scope = JobScope::for_project(job.project_root.clone());
        self.record_build_progress(&job.key, "enumerating checkout source files");
        // A blocking inspect already has the initial freshness census. Carry it
        // across the worker hop; background jobs collect their own census once.
        let inventory = options
            .inventory
            .take()
            .filter(|inventory| inventory.matches(&job.project_root))
            .unwrap_or_else(|| {
                // The bound index-store checkout already lists the folder's
                // files; walk it only when no checkout can stand in.
                Arc::new(
                    self.data_layer_dead_code
                        .inventory(&job.project_root)
                        .unwrap_or_else(|| {
                            super::inventory::InspectInventory::collect(&job.project_root, || {
                                !crate::executor::current_job_cancelled()
                            })
                        }),
                )
            });
        let _inventory = inventory.install();
        if crate::executor::current_job_cancelled() {
            return InspectResult::failed(
                &job,
                "tier2 pass cancelled before incremental scan",
                started.elapsed(),
            );
        }
        job.scope_files = scope_files(&job.project_root, &project_scope);
        self.record_build_progress(
            &job.key,
            format!("checking cache for {} source files", job.scope_files.len()),
        );
        log_tier2_benchmark_category_start(&job);
        let cache = match self.cache_for_paths(job.inspect_dir.clone(), job.project_root.clone()) {
            Ok(cache) => cache,
            Err(message) => {
                let result = InspectResult::failed(&job, message, started.elapsed());
                log_tier2_benchmark_category_end(&result);
                return result;
            }
        };
        delay_tier2_reuse_for_debug(&job);
        if options.has_force_paths() {
            if let Ok(cached) = load_contribution_freshness(cache.as_ref(), job.category) {
                let (remaining, downgraded) = downgrade_unchanged_forced_paths_with_freshness(
                    &job.project_root,
                    &cached,
                    options.force_rescan_paths.iter().cloned().collect(),
                );
                options.force_rescan_paths = remaining.into_iter().collect();
                if downgraded > 0 {
                    crate::slog_info!(
                        "inspect: {} forced paths downgraded to cached (content unchanged)",
                        downgraded
                    );
                }
            }
        }
        if !options.has_force_paths() {
            if let Ok(Some(success)) =
                self.tier2_quick_reuse_success(&job, cache.as_ref(), &options)
            {
                let result = InspectResult::success(&job, success, started.elapsed());
                crate::slog_debug!(
                    "perf tier2 category={} reuse=hit ms={}",
                    job.category,
                    started.elapsed().as_millis()
                );
                log_tier2_benchmark_category_end(&result);
                return result;
            }
        }

        // Automatic scans use the background seed gate to serialize their work.
        // A blocking inspect that proves it needs real work joins the interactive
        // class instead: it never preempts an in-flight build, but it takes a
        // released slot before another maintenance build can extend the wait.
        let interactive_permit_slot = if options.interactive {
            let queued_state = if self.semantic_cold_seed_active.load(Ordering::SeqCst) {
                InspectBuilderState::GatedBySemanticSeed
            } else {
                InspectBuilderState::QueuedBehindColdBuilds
            };
            self.set_builder_state(&job.key, queued_state);
            let Some(permit_slot) = self.acquire_interactive_tier2_permit_slot(&job) else {
                let result = InspectResult::failed(
                    &job,
                    "explicit inspect Tier-2 cold-build admission was cancelled",
                    started.elapsed(),
                );
                log_tier2_benchmark_category_end(&result);
                return result;
            };
            self.set_builder_state(&job.key, InspectBuilderState::Building);
            Some(permit_slot)
        } else {
            None
        };

        let permit_slot = permit_slot.or(interactive_permit_slot);
        let timeout = Duration::from_millis(job.config.inspect.tier2_pass_timeout_ms);
        let (scan_result, timed_out) = run_tier2_pass_with_deadline(
            &job.project_root,
            job.category,
            timeout,
            permit_slot,
            || self.tier2_run_with_reuse_job(&job, &cache, &options),
        );
        let result = if timed_out {
            InspectResult::failed(
                &job,
                format!(
                    "tier2 pass timed out after {}ms and was cancelled",
                    timeout.as_millis()
                ),
                started.elapsed(),
            )
        } else {
            match scan_result {
                Ok(success) => InspectResult::success(&job, success, started.elapsed()),
                Err(Tier2RunError::Failed(message)) => {
                    InspectResult::failed(&job, message, started.elapsed())
                }
                Err(Tier2RunError::CallgraphBusy) => {
                    callgraph_pending_result(&job, started.elapsed())
                }
            }
        };
        // Always-on perf line: a full (reuse=miss) scan is the expensive path —
        // for dead_code it includes store snapshot projection plus the scanner.
        // ms here lets us attribute background CPU bursts to a specific category from the log.
        crate::slog_info!(
            "perf tier2 category={} reuse=miss ms={}",
            job.category,
            started.elapsed().as_millis()
        );
        log_tier2_benchmark_category_end(&result);
        result
    }

    fn tier2_reuse_job(
        &self,
        snapshot: InspectSnapshot,
        category: InspectCategory,
        callgraph_snapshot: Option<Arc<CallgraphSnapshot>>,
    ) -> InspectJob {
        InspectJob {
            job_id: self.next_job_id.fetch_add(1, Ordering::Relaxed),
            key: JobKey::for_project_category(category),
            category,
            scope_files: Vec::new(),
            project_root: snapshot.project_root,
            inspect_dir: snapshot.inspect_dir,
            config: snapshot.config,
            symbol_cache: snapshot.symbol_cache,
            inspect_writer: snapshot.inspect_writer,
            callgraph_writer: snapshot.callgraph_writer,
            callgraph_snapshot,
        }
    }

    /// Scans the expected dead-code files that have no stored facts, with the
    /// call graph already attached to `aggregate_job`, and stores the result.
    /// Returns the new contribution-set hash when anything was stored.
    ///
    /// A file that still yields no contribution after this scan is left out of
    /// the rollup (logged) rather than blocking it: the scanner produces a
    /// contribution for every file it is given, so this only covers a file
    /// that vanished or was renamed between the walk and the scan.
    /// Dead code for `job`'s scope files over `snapshot`, which carries the
    /// data layer's call graph: read from the dead-code graph kept for the
    /// graph's publication, or worked out from the files' stored facts. The
    /// per-file facts the inspect cache holds are not read.
    fn data_layer_dead_code_aggregate(
        &self,
        job: &InspectJob,
        snapshot: &CallgraphSnapshot,
    ) -> Result<Value, String> {
        let started = Instant::now();
        let public_api_files =
            super::scanners::dead_code::collect_public_api_files(&job.project_root);
        let roles = super::entry_points::resolve_project_roles(&job.project_root);
        let mut timings = Vec::new();
        let oxc = |files: &[PathBuf]| self.oxc_result_for_scan(job, files, &[]);
        let mut aggregate = super::scanners::dead_code::section::aggregate_from_view(
            job,
            snapshot,
            &oxc,
            &public_api_files,
            &roles,
            &mut timings,
            super::scanners::dead_code::section::Use::Kept,
            None,
        )?;
        super::job::attach_dead_code_language_coverage(&mut aggregate, &job.scope_files);
        crate::slog_info!(
            "perf tier2_dead_code_rollup: root={} files={} ms={} phases={}",
            job.project_root.display(),
            job.scope_files.len(),
            started.elapsed().as_millis(),
            timings
                .iter()
                .map(|(label, value)| format!("{label}={value:.0}"))
                .collect::<Vec<_>>()
                .join(",")
        );
        Ok(aggregate)
    }

    fn fill_missing_dead_code_facts(
        &self,
        aggregate_job: &InspectJob,
        cache: &InspectCache,
        expected_files: &BTreeMap<String, PathBuf>,
        contributions: &[FileContribution],
    ) -> Result<Option<String>, String> {
        let missing = dead_code_files_without_facts(
            &aggregate_job.project_root,
            expected_files,
            contributions,
        );
        if missing.is_empty() {
            return Ok(None);
        }
        let mut fill_job = aggregate_job.clone();
        fill_job.job_id = self.next_job_id.fetch_add(1, Ordering::Relaxed);
        fill_job.scope_files = missing.clone();
        let oxc_result = self.oxc_result_for_scan(&fill_job, &fill_job.scope_files, &missing)?;
        let filled = self
            .scan_facts_with_file_progress(&fill_job, oxc_result.as_ref())
            .outcome
            .map_err(|message| {
                format!("dead_code scan of files without facts failed: {message}")
            })?;
        let still_missing = missing.len().saturating_sub(filled.contributions.len());
        if still_missing > 0 {
            crate::slog_warn!(
                "tier2 dead_code: {still_missing} of {} files produced no dead-code facts after a rescan; rolling up without them",
                missing.len()
            );
        }
        if filled.contributions.is_empty() {
            return Ok(None);
        }
        let (hash, _timings) = cache
            .apply_contribution_updates_for_config(
                aggregate_job.category,
                Tier2ContributionUpdates {
                    upserts: filled.contributions,
                    ..Tier2ContributionUpdates::default()
                },
                aggregate_job.config.as_ref(),
            )
            .map_err(|error| error.to_string())?;
        Ok(Some(hash))
    }

    fn tier2_quick_reuse_success(
        &self,
        job: &InspectJob,
        cache: &InspectCache,
        options: &Tier2ReuseOptions,
    ) -> Result<Option<InspectScanSuccess>, String> {
        let cached_records = load_contribution_freshness(cache, job.category)?;
        let current_by_relative = current_project_files(&job.project_root, &job.scope_files);
        if cached_records.len() != current_by_relative.len() {
            return Ok(None);
        }
        for record in &cached_records {
            let relative = freshness_record_relative_key(record);
            let Some(current_file) = current_by_relative.get(&relative) else {
                return Ok(None);
            };
            match cache_freshness::metadata_matches(current_file, &record.freshness) {
                Ok(true) => {}
                Ok(false) => return Ok(None),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
                Err(error) => {
                    return Err(format!(
                        "failed to stat {} for tier2 quick reuse: {error}",
                        current_file.display()
                    ));
                }
            }
        }

        let contribution_set_hash = cache
            .contribution_set_hash_for_config(job.category, job.config.as_ref())
            .map_err(|error| error.to_string())?;
        let Some(aggregate) = cache
            .load_aggregate_if_hash_matches(job.category, &contribution_set_hash)
            .map_err(|error| error.to_string())?
        else {
            return Ok(None);
        };
        if !cached_tier2_aggregate_usable(job.category, options, &aggregate) {
            return Ok(None);
        }

        cache
            .touch_tier2_last_full_run(job.category)
            .map_err(|error| error.to_string())?;
        Ok(Some(InspectScanSuccess {
            scanned_files: Vec::new(),
            contributions: Vec::new(),
            aggregate,
        }))
    }

    #[allow(clippy::too_many_lines)]
    fn tier2_run_with_reuse_job(
        &self,
        job: &InspectJob,
        cache: &InspectCache,
        options: &Tier2ReuseOptions,
    ) -> Result<InspectScanSuccess, Tier2RunError> {
        let mut phases = Tier2PhaseTimings::default();
        self.record_build_progress(
            &job.key,
            format!(
                "verifying freshness of {} source files",
                job.scope_files.len()
            ),
        );
        phases.projection_skip_reason = Some(if job.category != InspectCategory::DeadCode {
            "not_required"
        } else if job.callgraph_snapshot.is_some() {
            "provided_snapshot"
        } else {
            "aggregate_reused"
        });
        let phase_started = Instant::now();
        let cached_records = load_contribution_freshness(cache, job.category)?;
        let current_by_relative = current_project_files(&job.project_root, &job.scope_files);
        // Every file in the walked scope (the same `.aftignore`-aware walk
        // that defines `scope_files`) gets a dead-code contribution whenever it
        // is scanned with a call graph, including empty, unparseable,
        // generated and unsupported-language files.
        let expected_dead_code_files = if job.category == InspectCategory::DeadCode {
            current_by_relative.clone()
        } else {
            BTreeMap::new()
        };
        let cached_relative = cached_records
            .iter()
            .map(freshness_record_relative_key)
            .collect::<BTreeSet<_>>();
        let force_relative = forced_relative_paths(job, &options.force_rescan_paths);
        let cold_cache = cached_relative.is_empty();
        #[cfg(debug_assertions)]
        let debug_cold_cache = cold_cache;

        let mut updates = Tier2ContributionUpdates::default();
        let mut scan_by_relative = BTreeMap::<String, PathBuf>::new();
        let require_callgraph_refresh =
            if job.category == InspectCategory::DeadCode && options.require_callgraph_snapshot {
                !cache
                    .get_aggregated_for_config(&job.key, job.config.as_ref())
                    .map_err(|error| error.to_string())?
                    .is_some_and(|aggregate| {
                        aggregate
                            .get("callgraph_available")
                            .and_then(Value::as_bool)
                            == Some(true)
                    })
            } else {
                false
            };
        let mut callgraph_refresh_paths = options
            .force_rescan_paths
            .iter()
            .filter(|path| callgraph_store_indexes_path(path))
            .cloned()
            .collect::<BTreeSet<_>>();
        if require_callgraph_refresh {
            callgraph_refresh_paths.extend(
                current_by_relative
                    .values()
                    .filter(|path| callgraph_store_indexes_path(path))
                    .cloned(),
            );
        }
        let mut aggregate_job = job.clone();

        for record in cached_records {
            if crate::executor::current_job_cancelled() {
                return Err("tier2 pass cancelled during freshness scan"
                    .to_string()
                    .into());
            }
            let relative = freshness_record_relative_key(&record);
            let relative_path = PathBuf::from(&relative);
            let Some(current_file) = current_by_relative.get(&relative) else {
                updates.deletes.push(relative_path);
                insert_callgraph_refresh_path(
                    &mut callgraph_refresh_paths,
                    job.project_root.join(&relative),
                );
                continue;
            };

            if force_relative.contains(&relative) {
                updates.deletes.push(relative_path);
                scan_by_relative.insert(relative, current_file.clone());
                insert_callgraph_refresh_path(&mut callgraph_refresh_paths, current_file.clone());
                continue;
            }

            let absolute = job.project_root.join(&record.file_path);
            match verify_contribution_file(&absolute, &record.freshness) {
                ContributionFreshness::Fresh {
                    metadata_changed,
                    freshness,
                } => {
                    if metadata_changed {
                        updates.metadata_updates.push((relative_path, freshness));
                    }
                }
                ContributionFreshness::Stale => {
                    updates.deletes.push(relative_path);
                    scan_by_relative.insert(relative, current_file.clone());
                    insert_callgraph_refresh_path(
                        &mut callgraph_refresh_paths,
                        current_file.clone(),
                    );
                }
                ContributionFreshness::Deleted => {
                    updates.deletes.push(relative_path);
                    insert_callgraph_refresh_path(
                        &mut callgraph_refresh_paths,
                        job.project_root.join(&record.file_path),
                    );
                }
            }
        }

        for (relative, file) in &current_by_relative {
            if !cached_relative.contains(relative) {
                scan_by_relative.insert(relative.clone(), file.clone());
                if !cold_cache {
                    insert_callgraph_refresh_path(&mut callgraph_refresh_paths, file.clone());
                }
            }
        }
        phases.freshness = phase_started.elapsed();

        let mut scan_files = scan_by_relative.into_values().collect::<Vec<_>>();
        let force_reparse_files = scan_files.clone();
        let callgraph_refresh_files = callgraph_refresh_paths.into_iter().collect::<Vec<_>>();
        let dead_code_callgraph_refresh =
            job.category == InspectCategory::DeadCode && !callgraph_refresh_files.is_empty();
        if crate::executor::current_job_cancelled() {
            return Err("tier2 pass cancelled before incremental scan"
                .to_string()
                .into());
        }
        if !scan_files.is_empty() {
            let mut scan_job = job.clone();
            scan_job.job_id = self.next_job_id.fetch_add(1, Ordering::Relaxed);
            scan_job.scope_files = scan_files.clone();
            if scan_job.category == InspectCategory::DeadCode
                && scan_job.callgraph_snapshot.is_none()
            {
                self.record_build_progress(
                    &job.key,
                    "waiting for or projecting the checkout call graph",
                );
                let snapshot_started = Instant::now();
                match self.build_tier2_callgraph_snapshot_with_refresh_and_verdict(
                    &scan_job,
                    options.allow_callgraph_cold_build,
                    options.require_callgraph_snapshot,
                    &callgraph_refresh_files,
                )? {
                    Some((snapshot, verdict, projection_cost)) => {
                        scan_job.callgraph_snapshot = Some(snapshot);
                        phases.projection = Some(verdict);
                        phases.projection_cost += projection_cost;
                    }
                    None => phases.projection_skip_reason = Some("no_callgraph"),
                }
                phases.snapshot += snapshot_started.elapsed();
            }
            aggregate_job.callgraph_snapshot = scan_job.callgraph_snapshot.clone();
            #[cfg(debug_assertions)]
            cold_cache_test_delay(debug_cold_cache, std::thread::sleep);
            let scan_started = Instant::now();
            self.record_build_progress(
                &job.key,
                format!(
                    "scanning {} changed files ({} source files total)",
                    scan_files.len(),
                    job.scope_files.len()
                ),
            );
            let oxc_result =
                self.oxc_result_for_scan(&scan_job, &scan_job.scope_files, &force_reparse_files)?;
            let scan_result = self.scan_facts_with_file_progress(&scan_job, oxc_result.as_ref());
            phases.scan += scan_started.elapsed();
            phases.scanned_files += scan_files.len();
            let scan_success = scan_result.outcome.map_err(|message| {
                format!("{} incremental scan failed: {message}", job.category)
            })?;
            updates.upserts.extend(scan_success.contributions);
        }

        let has_updates = !updates.upserts.is_empty()
            || !updates.deletes.is_empty()
            || !updates.metadata_updates.is_empty();
        if !has_updates && !dead_code_callgraph_refresh {
            if let Some(aggregate) = cache
                .get_aggregated_for_config(&job.key, job.config.as_ref())
                .map_err(|error| error.to_string())?
            {
                if cached_tier2_aggregate_usable(job.category, options, &aggregate) {
                    cache
                        .touch_tier2_last_full_run(job.category)
                        .map_err(|error| error.to_string())?;
                    phases.log(job.category, &job.project_root);
                    return Ok(InspectScanSuccess {
                        scanned_files: scan_files,
                        contributions: Vec::new(),
                        aggregate,
                    });
                }
            }
        }

        let db_started = Instant::now();
        self.record_build_progress(&job.key, "assembling and storing analysis results");
        let mut contribution_set_hash = if has_updates {
            let (hash, db_timings) = cache
                .apply_contribution_updates_for_config(job.category, updates, job.config.as_ref())
                .map_err(|error| error.to_string())?;
            phases.add_db_timings(db_timings);
            hash
        } else {
            cache
                .contribution_set_hash_for_config(job.category, job.config.as_ref())
                .map_err(|error| error.to_string())?
        };
        phases.db = db_started.elapsed();

        if !dead_code_callgraph_refresh {
            if let Some(aggregate) = cache
                .load_aggregate_if_hash_matches(job.category, &contribution_set_hash)
                .map_err(|error| error.to_string())?
            {
                if cached_tier2_aggregate_usable(job.category, options, &aggregate) {
                    cache
                        .touch_tier2_last_full_run(job.category)
                        .map_err(|error| error.to_string())?;
                    phases.log(job.category, &job.project_root);
                    return Ok(InspectScanSuccess {
                        scanned_files: scan_files,
                        contributions: if options.return_contributions {
                            load_contributions(cache, job)?
                        } else {
                            Vec::new()
                        },
                        aggregate,
                    });
                }
            }
        }

        if crate::executor::current_job_cancelled() {
            return Err("tier2 pass cancelled before projection refresh"
                .to_string()
                .into());
        }
        let refresh_dead_code_facts = if job.category == InspectCategory::DeadCode {
            dead_code_contributions_need_fact_refresh(cache, job)?
        } else {
            false
        };
        let refresh_unused_exports_facts = if job.category == InspectCategory::UnusedExports {
            unused_exports_contributions_need_fact_refresh(cache, job)?
        } else {
            false
        };
        let refresh_duplicates_facts = if job.category == InspectCategory::Duplicates {
            duplicates_contributions_need_fact_refresh(cache, job)?
        } else {
            false
        };
        if refresh_dead_code_facts || refresh_unused_exports_facts || refresh_duplicates_facts {
            // Raw-facts contributions can be rolled up after manifest/resolver
            // edits without re-reading source. Only legacy verdict-bearing or
            // facts-version-mismatched caches need a one-time full refresh before
            // verdicts/roots can be recomputed globally.
            let full_scan_files = current_by_relative.into_values().collect::<Vec<_>>();
            if !full_scan_files.is_empty() {
                let mut rescan_job = job.clone();
                rescan_job.job_id = self.next_job_id.fetch_add(1, Ordering::Relaxed);
                rescan_job.scope_files = full_scan_files.clone();
                if rescan_job.category == InspectCategory::DeadCode
                    && rescan_job.callgraph_snapshot.is_none()
                {
                    let snapshot_started = Instant::now();
                    match self.build_tier2_callgraph_snapshot_with_refresh_and_verdict(
                        &rescan_job,
                        options.allow_callgraph_cold_build,
                        options.require_callgraph_snapshot,
                        &callgraph_refresh_files,
                    )? {
                        Some((snapshot, verdict, projection_cost)) => {
                            rescan_job.callgraph_snapshot = Some(snapshot);
                            phases.projection = Some(verdict);
                            phases.projection_cost += projection_cost;
                        }
                        None => phases.projection_skip_reason = Some("no_callgraph"),
                    }
                    phases.snapshot += snapshot_started.elapsed();
                }
                let scan_started = Instant::now();
                let oxc_result = self.oxc_result_for_scan(
                    &rescan_job,
                    &rescan_job.scope_files,
                    &force_reparse_files,
                )?;
                let scan_result =
                    self.scan_facts_with_file_progress(&rescan_job, oxc_result.as_ref());
                phases.scan += scan_started.elapsed();
                phases.scanned_files += full_scan_files.len();
                let scan_success = scan_result.outcome.map_err(|message| {
                    format!(
                        "{} full rescan after entry-point cache miss failed: {message}",
                        job.category
                    )
                })?;
                let rescan_updates = Tier2ContributionUpdates {
                    upserts: scan_success.contributions,
                    ..Tier2ContributionUpdates::default()
                };
                let db_started = Instant::now();
                let (hash, db_timings) = cache
                    .apply_contribution_updates_for_config(
                        job.category,
                        rescan_updates,
                        job.config.as_ref(),
                    )
                    .map_err(|error| error.to_string())?;
                contribution_set_hash = hash;
                phases.add_db_timings(db_timings);
                phases.db += db_started.elapsed();
                aggregate_job.callgraph_snapshot = rescan_job.callgraph_snapshot.clone();
                scan_files = full_scan_files;

                if !dead_code_callgraph_refresh {
                    if let Some(aggregate) = cache
                        .load_aggregate_if_hash_matches(job.category, &contribution_set_hash)
                        .map_err(|error| error.to_string())?
                    {
                        if cached_tier2_aggregate_usable(job.category, options, &aggregate) {
                            cache
                                .touch_tier2_last_full_run(job.category)
                                .map_err(|error| error.to_string())?;
                            phases.log(job.category, &job.project_root);
                            return Ok(InspectScanSuccess {
                                scanned_files: scan_files,
                                contributions: if options.return_contributions {
                                    load_contributions(cache, job)?
                                } else {
                                    Vec::new()
                                },
                                aggregate,
                            });
                        }
                    }
                }
            }
        }

        if crate::executor::current_job_cancelled() {
            return Err("tier2 pass cancelled before aggregate projection"
                .to_string()
                .into());
        }
        if aggregate_job.category == InspectCategory::DeadCode
            && aggregate_job.callgraph_snapshot.is_none()
        {
            let snapshot_started = Instant::now();
            match self.build_tier2_callgraph_snapshot_with_refresh_and_verdict(
                &aggregate_job,
                options.allow_callgraph_cold_build,
                options.require_callgraph_snapshot,
                &callgraph_refresh_files,
            )? {
                Some((snapshot, verdict, projection_cost)) => {
                    aggregate_job.callgraph_snapshot = Some(snapshot);
                    phases.projection = Some(verdict);
                    phases.projection_cost += projection_cost;
                }
                None => phases.projection_skip_reason = Some("no_callgraph"),
            }
            phases.snapshot += snapshot_started.elapsed();
        }
        if aggregate_job.category == InspectCategory::DeadCode
            && aggregate_job.callgraph_snapshot.is_none()
        {
            if options.require_callgraph_snapshot {
                if let Some(reason) = callgraph_path_identity_gap(job)? {
                    return Ok(InspectScanSuccess {
                        scanned_files: scan_files,
                        contributions: Vec::new(),
                        aggregate: crate::inspect::scanners::dead_code::callgraph_unavailable_aggregate_with_reason(
                            job.scope_files.len(),
                            Some(&reason),
                        ),
                    });
                }
            }
            // A borrow-only root (a worktree reading the main checkout's
            // stores) cannot build the call graph dead code needs, so retrying
            // cannot help. Return an unavailable result that names this cause
            // and carries no count, so it is not mistaken for zero dead code.
            if job.config.indexes.callgraph && job_is_borrow_only(job) {
                return Ok(InspectScanSuccess {
                    scanned_files: scan_files,
                    contributions: Vec::new(),
                    aggregate:
                        crate::inspect::scanners::dead_code::borrow_only_unavailable_aggregate(
                            job.scope_files.len(),
                        ),
                });
            }
            if options.require_callgraph_snapshot {
                return Err(format!(
                    "tier2 dead_code aggregate did not complete; builder_state={}",
                    self.builder_state_detail_for_job(job)
                )
                .into());
            }
        }
        let rollup_started = Instant::now();
        if crate::executor::current_job_cancelled() {
            return Err("tier2 pass cancelled before rollup".to_string().into());
        }
        // Dead code over the data layer's call graph is worked out from the
        // graph and the facts its content records store; the per-file facts
        // the inspect cache holds only keep track of which files changed.
        let data_layer_dead_code = aggregate_job.category == InspectCategory::DeadCode
            && aggregate_job
                .callgraph_snapshot
                .as_ref()
                .is_some_and(|snapshot| snapshot.data_layer.is_some());
        let mut contributions = if data_layer_dead_code && !options.return_contributions {
            Vec::new()
        } else {
            load_contributions(cache, &aggregate_job)?
        };
        if aggregate_job.category == InspectCategory::DeadCode
            && aggregate_job.callgraph_snapshot.is_some()
            && !data_layer_dead_code
        {
            // Files scanned while no call graph was ready produce no dead-code
            // facts. If the graph became ready before this rollup, rolling up
            // the partial set would report those files' findings as absent (an
            // empty set reads as zero dead code). Scan the missing files now,
            // with the graph this rollup uses, so the result is complete and a
            // pass can never stay incomplete.
            let filled = self.fill_missing_dead_code_facts(
                &aggregate_job,
                cache,
                &expected_dead_code_files,
                &contributions,
            )?;
            if let Some(hash) = filled {
                contribution_set_hash = hash;
                contributions = load_contributions(cache, &aggregate_job)?;
            }
        }
        let aggregate = if data_layer_dead_code {
            let snapshot = aggregate_job
                .callgraph_snapshot
                .as_deref()
                .expect("checked dead-code snapshot");
            self.data_layer_dead_code_aggregate(&aggregate_job, snapshot)?
        } else if aggregate_job.category == InspectCategory::DeadCode
            && aggregate_job.callgraph_snapshot.is_some()
        {
            let snapshot = aggregate_job
                .callgraph_snapshot
                .as_deref()
                .expect("checked dead-code snapshot");
            let public_api_files =
                super::scanners::dead_code::collect_public_api_files(&job.project_root);
            let roles = super::entry_points::resolve_project_roles(&job.project_root);
            // Every rollup works out liveness from the whole graph: on the
            // integer graph that costs little more than reusing an earlier
            // rollup would.
            let mut aggregate =
                super::scanners::dead_code::aggregate_dead_code_contributions_with_snapshot(
                    &job.project_root,
                    snapshot,
                    &contributions,
                    &public_api_files,
                    &roles,
                    None,
                );
            super::job::attach_dead_code_language_coverage(&mut aggregate, &job.scope_files);
            aggregate
        } else {
            roll_up_tier2_contributions(&aggregate_job, &contributions)
        };
        if crate::executor::current_job_cancelled() {
            return Err("tier2 pass cancelled after rollup".to_string().into());
        }
        cache
            .store_tier2_aggregate(job.key.clone(), &contribution_set_hash, aggregate.clone())
            .map_err(|error| error.to_string())?;
        phases.rollup = rollup_started.elapsed();
        // The cost estimates predict today's store projections; a snapshot
        // from the data layer costs something else and is left out of them.
        if let Some(verdict) = phases
            .projection
            .filter(|verdict| *verdict != DATA_LAYER_SNAPSHOT)
        {
            self.observe_callgraph_projection_cost(
                &job.project_root,
                verdict,
                projection_estimator_duration(&phases),
            );
        }
        phases.log(job.category, &job.project_root);

        Ok(InspectScanSuccess {
            scanned_files: scan_files,
            // Facts are already persisted. Move them only to direct readers, not aggregate-only completion.
            contributions: if options.return_contributions {
                contributions
            } else {
                Vec::new()
            },
            aggregate,
        })
    }

    fn enqueue_with_waiter(
        &self,
        snapshot: InspectSnapshot,
        category: InspectCategory,
        caller_scope: JobScope,
        key: JobKey,
        waiter_tx: WaiterTx,
        callgraph_snapshot: Option<Arc<CallgraphSnapshot>>,
    ) -> Result<(), String> {
        let mut in_flight = self
            .in_flight
            .lock()
            .map_err(|_| "inspect in-flight map lock poisoned".to_string())?;
        if let Some(registration) = in_flight.get_mut(&key) {
            registration.waiters.push(Waiter { tx: waiter_tx });
            return Ok(());
        }

        in_flight.insert(
            key.clone(),
            InFlightRegistration::new(vec![Waiter { tx: waiter_tx }]),
        );
        drop(in_flight);
        self.record_flight_start(&key);

        if let Err(message) = self.enqueue_new_job(
            snapshot,
            category,
            caller_scope,
            key.clone(),
            callgraph_snapshot,
        ) {
            let outcome = JobOutcome::Failed {
                message: message.clone(),
            };
            if let Some(waiters) = self.take_waiters(&key, None) {
                Self::deliver_waiters(waiters, outcome);
            }
            self.clear_builder_state(&key);
            return Ok(());
        }
        Ok(())
    }

    fn enqueue_without_waiter(
        &self,
        snapshot: InspectSnapshot,
        category: InspectCategory,
        caller_scope: JobScope,
        key: JobKey,
        callgraph_snapshot: Option<Arc<CallgraphSnapshot>>,
    ) -> Result<(), String> {
        let mut in_flight = self
            .in_flight
            .lock()
            .map_err(|_| "inspect in-flight map lock poisoned".to_string())?;
        if in_flight.contains_key(&key) {
            return Ok(());
        }
        in_flight.insert(key.clone(), InFlightRegistration::new(Vec::new()));
        drop(in_flight);
        self.record_flight_start(&key);

        if let Err(message) = self.enqueue_new_job(
            snapshot,
            category,
            caller_scope,
            key.clone(),
            callgraph_snapshot,
        ) {
            if let Ok(mut in_flight) = self.in_flight.lock() {
                in_flight.remove(&key);
            }
            self.clear_builder_state(&key);
            return Err(message);
        }
        Ok(())
    }

    fn enqueue_new_job(
        &self,
        snapshot: InspectSnapshot,
        category: InspectCategory,
        caller_scope: JobScope,
        key: JobKey,
        callgraph_snapshot: Option<Arc<CallgraphSnapshot>>,
    ) -> Result<(), String> {
        let scan_scope = if category.is_tier2() {
            JobScope::for_project(snapshot.project_root.clone())
        } else {
            caller_scope
        };
        let scope_files = scope_files(&snapshot.project_root, &scan_scope);
        let job = InspectJob {
            job_id: self.next_job_id.fetch_add(1, Ordering::Relaxed),
            key,
            category,
            scope_files,
            project_root: snapshot.project_root,
            inspect_dir: snapshot.inspect_dir,
            config: snapshot.config,
            symbol_cache: snapshot.symbol_cache,
            inspect_writer: snapshot.inspect_writer,
            callgraph_writer: snapshot.callgraph_writer,
            callgraph_snapshot,
        };
        self.dispatcher.submit(job);
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn wait_for_outcome(
        &self,
        key: JobKey,
        caller_scope: JobScope,
        cache: Arc<InspectCache>,
        waiter_rx: Receiver<JobOutcome>,
        snapshot: InspectSnapshot,
        deadline: Instant,
        wait_started: Instant,
        wait_budget: Duration,
    ) -> JobOutcome {
        let timeout = after(deadline.saturating_duration_since(Instant::now()));
        #[cfg(test)]
        let timeout = match self.metrics_wait_for_test.lock().unwrap().clone() {
            Some(control) if key.category == InspectCategory::Metrics => {
                if control.non_expiring {
                    // Install a clock that cannot expire before releasing the
                    // real metrics worker; scheduling latency is not freshness.
                    let timeout = crossbeam_channel::never();
                    let _ = control.release_tx.try_send(());
                    timeout
                } else {
                    // The expired-clock control keeps metrics pending, forcing
                    // the old deadline/result ordering without a wall sleep.
                    after(Duration::ZERO)
                }
            }
            _ => timeout,
        };
        let result_rx = self.result_rx.clone();
        loop {
            // Route a worker result before an equally ready deadline notification.
            // Returning Pending while a terminal result is already available makes
            // the response depend on thread scheduling rather than job completion.
            select_biased! {
                recv(waiter_rx) -> outcome => {
                    return match outcome {
                        Ok(outcome) => filter_outcome_for_scope_with_contributions(
                            &self.data_layer_dead_code,
                            None,
                            outcome,
                            &snapshot,
                            key.category,
                            cache.as_ref(),
                            &caller_scope,
                        ),
                        Err(_) => self.timeout_outcome(
                            &key,
                            &caller_scope,
                            &cache,
                            &snapshot,
                            PendingWaitCause::WaiterDropped,
                            wait_started,
                            wait_budget,
                        ),
                    };
                }
                recv(result_rx) -> result => {
                    match result {
                        Ok(result) => self.route_completion(result),
                        Err(_) => return self.timeout_outcome(
                            &key,
                            &caller_scope,
                            &cache,
                            &snapshot,
                            PendingWaitCause::ResultChannelDisconnected,
                            wait_started,
                            wait_budget,
                        ),
                    }
                }
                recv(timeout) -> _ => {
                    return self.timeout_outcome(
                        &key,
                        &caller_scope,
                        &cache,
                        &snapshot,
                        PendingWaitCause::DeadlineElapsed,
                        wait_started,
                        wait_budget,
                    );
                }
            }
        }
    }

    fn timeout_outcome(
        &self,
        key: &JobKey,
        caller_scope: &JobScope,
        cache: &(impl InspectCacheRead + ?Sized),
        snapshot: &InspectSnapshot,
        cause: PendingWaitCause,
        wait_started: Instant,
        wait_budget: Duration,
    ) -> JobOutcome {
        match cache.get_aggregated_for_config(key, snapshot.config.as_ref()) {
            Ok(Some(cached)) => filter_outcome_for_scope_with_contributions(
                &self.data_layer_dead_code,
                None,
                JobOutcome::Stale {
                    cached: Some(cached),
                    in_flight: true,
                },
                snapshot,
                key.category,
                cache,
                caller_scope,
            ),
            Ok(None) => JobOutcome::pending_wait(true, cause, wait_started.elapsed(), wait_budget),
            Err(error) => JobOutcome::Failed {
                message: error.to_string(),
            },
        }
    }

    fn route_completion(&self, result: InspectResult) {
        let key = result.key.clone();
        let outcome = self.completion_outcome(result);
        self.record_builder_attempt_outcome(&key, &outcome);
        if let Some(waiters) = self.take_waiters(&key, None) {
            Self::deliver_waiters(waiters, outcome);
        }
    }

    fn route_tier2_reuse_completion(&self, result: InspectResult) {
        if result.outcome.as_ref().is_ok_and(|success| {
            !success.scanned_files.is_empty()
                && success
                    .aggregate
                    .get("callgraph_available")
                    .and_then(Value::as_bool)
                    != Some(false)
        }) {
            if let Ok(mut durations) = self.completed_build_durations.lock() {
                durations.insert(result.category, result.duration);
            }
        }
        let outcome = match result.outcome {
            Ok(success) => JobOutcome::Fresh {
                payload: success.aggregate,
            },
            Err(message) => JobOutcome::Failed { message },
        };
        // Publish completion before waking waiters so a direct-reuse caller sees all
        // completion side effects when its result channel becomes ready. The same
        // finish path runs from the exit guard if this router is skipped.
        self.finish_tier2_flight(&result.key, outcome);
        // The counter also signals the main-thread drain that a background
        // (watcher-driven) Tier-2 scan finished. This path bypasses
        // `result_rx`/`drain_completions`, so without this signal the bar's
        // counts and `~` marker would only update on a manual `aft_inspect`.
    }

    /// Snapshot the cumulative count of reuse-path Tier-2 completions.
    pub fn reuse_completion_count(&self) -> u64 {
        self.reuse_completions.load(Ordering::SeqCst)
    }

    #[doc(hidden)]
    pub fn reuse_start_count_for_test(&self) -> u64 {
        self.reuse_starts.load(Ordering::SeqCst)
    }

    #[doc(hidden)]
    pub fn source_scan_files_for_test(&self) -> u64 {
        self.source_scan_files.load(Ordering::Relaxed)
    }

    fn completion_outcome(&self, result: InspectResult) -> JobOutcome {
        match result.outcome {
            Ok(success) => JobOutcome::Fresh {
                payload: success.aggregate,
            },
            Err(message) => JobOutcome::Failed { message },
        }
    }

    fn store_completion(
        caches: &Mutex<HashMap<InspectCacheIdentity, Arc<InspectCache>>>,
        writer_waits: &Mutex<HashMap<InspectCacheIdentity, (Instant, String)>>,
        open_gates: &CacheOpenGates,
        result: &InspectResult,
    ) -> Result<(), String> {
        let Ok(success) = &result.outcome else {
            return Ok(());
        };
        // Pending observations are renderable gaps, not reusable scan results.
        if success.aggregate["status"] == "pending" {
            return Ok(());
        }
        let cache = Self::open_cache_for_paths(
            caches,
            writer_waits,
            open_gates,
            result.inspect_dir.clone(),
            result.project_root.clone(),
        )?;
        if result.category.is_tier2() {
            cache.store_tier2_result_for_config(
                result.key.clone(),
                &success.scanned_files,
                &success.contributions,
                success.aggregate.clone(),
                result.config.as_ref(),
            )
        } else {
            cache.store_aggregated(result.key.clone(), success.aggregate.clone())
        }
        .map_err(|error| error.to_string())
    }
}

impl Default for InspectManager {
    fn default() -> Self {
        Self::new()
    }
}

fn validate_tier2_read_category(category: InspectCategory) -> Result<(), JobOutcome> {
    if !category.is_active() {
        return Err(JobOutcome::Failed {
            message: format!("inspect category '{category}' is disabled in v0.33"),
        });
    }
    if !category.is_tier2() {
        return Err(JobOutcome::Failed {
            message: format!("inspect category '{category}' is not a Tier 2 category"),
        });
    }
    Ok(())
}

/// Phase-level wall-time attribution for one Tier-2 reuse=miss pass.
///
/// Exists to self-attribute pathological scans (e.g. a normally-100ms
/// unused_exports pass once took 677s under heavy machine load) without
/// needing a lucky live `sample`. Logged as ONE info line per pass, only when
/// real work happened (freshness/scan/snapshot/rollup/db), so quiet reuse passes stay silent.
#[derive(Default)]
struct Tier2PhaseTimings {
    /// Freshness verification of cached contributions (file stat + hash reads).
    freshness: Duration,
    /// Callgraph refresh, store open, and snapshot projection (dead_code only).
    snapshot: Duration,
    /// The projection operation alone, excluding store refresh and open work.
    projection_cost: Duration,
    /// Scanner compute over files needing (re)scan.
    scan: Duration,
    /// SQLite contribution upserts/deletes, including connection lock wait.
    db: Duration,
    /// Time waiting for the shared SQLite connection mutex.
    db_lock: Duration,
    /// Time spent in contribution update transactions after acquiring the mutex.
    db_txn: Duration,
    /// Aggregate roll-up + store.
    rollup: Duration,
    scanned_files: usize,
    /// How the dead-code snapshot was produced (dead_code only).
    projection: Option<ProjectionVerdict>,
    /// Why a path completed without running the callgraph projection.
    projection_skip_reason: Option<&'static str>,
}

const TIER2_WORK_LOG_THRESHOLD: Duration = Duration::from_millis(50);

#[cfg(debug_assertions)]
// Timing fixtures are built with Cargo's test-timing-hooks feature to request
// this artificial delay. Ordinary debug builds do not enable that feature.
fn cold_cache_test_delay(cold: bool, sleep: impl FnOnce(Duration)) {
    if cold && cfg!(feature = "test-timing-hooks") {
        sleep(Duration::from_millis(10));
    }
}

fn projection_estimator_duration(phases: &Tier2PhaseTimings) -> Duration {
    // Projection journal files are the denominator used by the cost model.
    // Refresh and rollup have different work sets, so folding either into this
    // sample makes a cheap splice look expensive for unrelated work.
    phases.projection_cost
}

impl Tier2PhaseTimings {
    fn add_db_timings(&mut self, timings: InspectDbTimings) {
        self.db_lock += timings.lock_wait;
        self.db_txn += timings.transaction;
    }

    fn worked(&self) -> Duration {
        self.freshness + self.scan + self.snapshot + self.rollup + self.db
    }

    fn log(&self, category: InspectCategory, project_root: &Path) {
        let worked = self.worked();
        if !worked.is_zero() {
            crate::logging::note_tier2_scan(
                category.to_string(),
                worked.as_millis().min(u128::from(u64::MAX)) as u64,
            );
        }
        if worked < TIER2_WORK_LOG_THRESHOLD {
            return;
        }
        let key = crate::search_index::artifact_cache_key_for_build(project_root);
        crate::slog_info!("{}", self.render(category, project_root, &key));
    }

    fn render(&self, category: InspectCategory, project_root: &Path, key: &str) -> String {
        let projection = self
            .projection
            .map(render_projection_suffix)
            .unwrap_or_else(|| {
                render_no_projection_suffix(
                    self.projection_skip_reason
                        .unwrap_or_else(|| default_projection_skip_reason(category)),
                )
            });
        format!(
            "perf tier2 phases category={} freshness={}ms snapshot={}ms projection_ms={} scan={}ms({} files) db={}ms(lock={},txn={}) rollup_ms={}{} root={} key={}",
            category,
            self.freshness.as_millis(),
            self.snapshot.as_millis(),
            self.projection_cost.as_millis(),
            self.scan.as_millis(),
            self.scanned_files,
            self.db.as_millis(),
            self.db_lock.as_millis(),
            self.db_txn.as_millis(),
            self.rollup.as_millis(),
            projection,
            crate::logging::normalize_index_root(project_root),
            key
        )
    }
}

fn default_projection_skip_reason(category: InspectCategory) -> &'static str {
    if category == InspectCategory::DeadCode {
        "no_callgraph"
    } else {
        "not_required"
    }
}

/// Projection fields in a `perf tier2 phases` line use this grammar:
/// `projection=<spliced|full|reused|none> reason=<cold|journal_gap|splice_costlier|no_callgraph|aggregate_reused|provided_snapshot|not_required> journal_bytes=<bytes> changed_files=<count>`.
/// Successful splice/reuse verdicts omit `reason`; every `none` verdict names why
/// no callgraph projection ran.
fn render_no_projection_suffix(reason: &'static str) -> String {
    format!(" projection=none reason={reason} journal_bytes=0 changed_files=0")
}

/// Render the `projection=...` suffix of the `perf tier2 phases` line for one
/// dead-code snapshot verdict. `reason` is omitted for spliced and reused
/// projections.
fn render_projection_suffix(verdict: ProjectionVerdict) -> String {
    let kind = match verdict.kind {
        #[cfg(test)]
        ProjectionKind::Spliced => "spliced",
        ProjectionKind::Full => "full",
        #[cfg(test)]
        ProjectionKind::Reused => "reused",
    };
    let reason = match verdict.reason {
        Some("journal_oversize") => format!(" reason=journal_oversize:{MAX_DELTA_BYTES}"),
        Some(reason) => format!(" reason={reason}"),
        None => String::new(),
    };
    format!(
        " projection={kind}{reason} journal_bytes={} changed_files={}",
        verdict.journal_bytes, verdict.changed_files
    )
}

#[cfg(test)]
thread_local! {
    static SCOPE_FILE_WALKS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

pub(crate) fn scope_files(project_root: &Path, scope: &JobScope) -> Vec<PathBuf> {
    let in_scope = |inventory: &super::inventory::InspectInventory| {
        inventory
            .sources
            .iter()
            .filter(|path| scope.contains(path))
            .cloned()
            .collect::<Vec<_>>()
    };
    if let Some(inventory) = super::inventory::current(project_root) {
        return in_scope(&inventory);
    }
    // No caller installed a census on this thread (background jobs such as
    // the status bar's TODO refresh never do): the root's bound index-store
    // checkout lists the same files without touching the disk.
    let miss = match crate::data_layer::tools::inspect::bound_inventory(project_root) {
        Ok(inventory) => return in_scope(&inventory),
        Err(miss) => miss,
    };
    #[cfg(test)]
    let _profile = super::walk_profile::Walk::start("scope_files");
    #[cfg(test)]
    SCOPE_FILE_WALKS.with(|walks| walks.set(walks.get() + 1));
    let started = Instant::now();
    let mut files = crate::callgraph::walk_project_files(project_root)
        .filter(|path| scope.contains(path))
        .collect::<Vec<_>>();
    files.sort();
    crate::slog_info!(
        "inspect scope files walked: reason={} files={} ms={} root={}",
        miss.as_str(),
        files.len(),
        started.elapsed().as_millis(),
        crate::logging::normalize_index_root(project_root)
    );
    files
}

fn forced_relative_paths(job: &InspectJob, paths: &BTreeSet<PathBuf>) -> BTreeSet<String> {
    let mut keys = BTreeSet::new();
    for path in paths {
        let absolute = if path.is_absolute() {
            path.clone()
        } else {
            job.project_root.join(path)
        };
        keys.insert(relative_cache_key(&job.project_root, &absolute));
        // Normalized, not bare-canonical: the project root is verbatim-stripped,
        // so a verbatim canonical path would fail strip_prefix and produce an
        // absolute key no cached contribution matches (the forced rescan then
        // silently misses).
        keys.insert(relative_cache_key(
            &job.project_root,
            &crate::inspect::job::canonicalize_normalized(&absolute),
        ));
    }
    keys
}

fn downgrade_unchanged_forced_paths_with_freshness(
    project_root: &Path,
    cached: &[CachedContributionFreshness],
    paths: Vec<PathBuf>,
) -> (Vec<PathBuf>, usize) {
    let cached = cached
        .iter()
        .map(|record| (freshness_record_relative_key(record), record.freshness))
        .collect::<BTreeMap<_, _>>();
    let mut remaining = Vec::with_capacity(paths.len());
    let mut downgraded = 0;

    for path in paths {
        let absolute = if path.is_absolute() {
            path.clone()
        } else {
            project_root.join(&path)
        };
        let direct_key = relative_cache_key(project_root, &absolute);
        // Same normalized form as forced_relative_paths; see the comment there.
        let canonical_key = Some(relative_cache_key(
            project_root,
            &crate::inspect::job::canonicalize_normalized(&absolute),
        ));
        let freshness = cached
            .get(&direct_key)
            .or_else(|| canonical_key.as_ref().and_then(|key| cached.get(key)));
        let content_unchanged = freshness.is_some_and(|freshness| {
            matches!(
                cache_freshness::verify_file_strict(&absolute, freshness),
                FreshnessVerdict::HotFresh | FreshnessVerdict::ContentFresh { .. }
            )
        });
        if content_unchanged {
            downgraded += 1;
        } else {
            remaining.push(path);
        }
    }

    (remaining, downgraded)
}

fn panic_tier2_reuse_for_debug(job: &InspectJob) {
    #[cfg(not(debug_assertions))]
    let _ = job;
    #[cfg(debug_assertions)]
    {
        if !env_project_root_matches("AFT_TEST_TIER2_REUSE_PANIC_ROOT", &job.project_root) {
            return;
        }
        let should_panic = std::env::var("AFT_TEST_TIER2_REUSE_PANIC_CATEGORY")
            .ok()
            .is_some_and(|category| category == job.category.as_str());
        if should_panic {
            panic!("forced tier2 reuse panic for {}", job.category);
        }
    }
}

fn delay_tier2_reuse_for_debug(job: &InspectJob) {
    #[cfg(not(debug_assertions))]
    let _ = job;
    #[cfg(debug_assertions)]
    {
        let project_root = &job.project_root;
        if std::env::var_os("AFT_TEST_TIER2_REUSE_GATE_ROOT").is_some()
            && env_project_root_matches("AFT_TEST_TIER2_REUSE_GATE_ROOT", project_root)
            && std::env::var("AFT_TEST_TIER2_REUSE_GATE_CATEGORY")
                .ok()
                .is_none_or(|category| category == job.category.as_str())
        {
            let ready = std::env::var_os("AFT_TEST_TIER2_REUSE_GATE_READY").map(PathBuf::from);
            let release = std::env::var_os("AFT_TEST_TIER2_REUSE_GATE_RELEASE").map(PathBuf::from);
            if let (Some(ready), Some(release)) = (ready, release) {
                let _ = std::fs::write(&ready, b"ready");
                // The release file controls correctness ordering. This deadline
                // only prevents a broken fixture from wedging the test process.
                let hang_deadline = Instant::now() + Duration::from_secs(30);
                while !release.exists() {
                    assert!(
                        Instant::now() < hang_deadline,
                        "timed out waiting for Tier-2 reuse gate release"
                    );
                    std::thread::sleep(Duration::from_millis(10));
                }
                return;
            }
        }

        if !env_project_root_matches("AFT_TEST_TIER2_REUSE_DELAY_ROOT", project_root) {
            return;
        }
        if let Some(delay_ms) = std::env::var("AFT_TEST_TIER2_REUSE_DELAY_MS")
            .ok()
            .and_then(|raw| raw.parse::<u64>().ok())
        {
            std::thread::sleep(Duration::from_millis(delay_ms));
        }
    }
}

#[cfg(debug_assertions)]
fn env_project_root_matches(var: &str, project_root: &Path) -> bool {
    let Some(raw) = std::env::var_os(var) else {
        return true;
    };
    let expected = PathBuf::from(raw);
    let expected = std::fs::canonicalize(&expected).unwrap_or(expected);
    let actual = std::fs::canonicalize(project_root).unwrap_or_else(|_| project_root.to_path_buf());
    expected == actual
}

fn current_project_files(project_root: &Path, files: &[PathBuf]) -> BTreeMap<String, PathBuf> {
    files
        .iter()
        .map(|file| (relative_cache_key(project_root, file), file.clone()))
        .collect()
}

fn insert_callgraph_refresh_path(paths: &mut BTreeSet<PathBuf>, path: PathBuf) {
    if callgraph_store_indexes_path(&path) {
        paths.insert(path);
    }
}

fn callgraph_store_indexes_path(path: &Path) -> bool {
    crate::parser::detect_language(path).is_some()
}

fn tier2_benchmark_logging_enabled() -> bool {
    std::env::var_os("AFT_SETTLE_BENCH_LOG").is_some()
}

thread_local! {
    static TIER2_INDEX_SCOPE: RefCell<Option<crate::logging::IndexBuildScope>> =
        const { RefCell::new(None) };
}

fn log_tier2_benchmark_category_start(job: &InspectJob) {
    let key = crate::search_index::artifact_cache_key_for_build(&job.project_root);
    let scope = crate::logging::IndexBuildScope::new(
        crate::logging::IndexPlane::Tier2,
        &job.project_root,
        key,
    );
    TIER2_INDEX_SCOPE.with(|slot| *slot.borrow_mut() = Some(scope));
    if !tier2_benchmark_logging_enabled() {
        return;
    }
    crate::slog_info!(
        "settle bench: tier2_category_start category={} job_id={} files={}",
        job.category.as_str(),
        job.job_id,
        job.scope_files.len()
    );
}

fn tier2_pass_did_real_work(result: &InspectResult) -> bool {
    match &result.outcome {
        Ok(success) => {
            !success.scanned_files.is_empty() || result.duration >= TIER2_WORK_LOG_THRESHOLD
        }
        Err(_) => false,
    }
}

fn log_tier2_benchmark_category_end(result: &InspectResult) {
    let scope = TIER2_INDEX_SCOPE.with(|slot| slot.borrow_mut().take());
    if let Some(scope) = scope {
        match &result.outcome {
            Ok(success) if tier2_pass_did_real_work(result) => {
                crate::logging::log_index_event(
                    crate::logging::IndexEvent::from_scope(
                        crate::logging::IndexEventKind::BuildStarted,
                        &scope,
                    )
                    .field("category", result.category.as_str())
                    .field("files", success.scanned_files.len()),
                );
                crate::logging::log_index_event(
                    crate::logging::IndexEvent::from_scope(
                        crate::logging::IndexEventKind::BuildReady,
                        &scope,
                    )
                    .field("category", result.category.as_str())
                    .field("elapsed_ms", result.duration.as_millis())
                    .field("files", success.scanned_files.len())
                    .field("contributions", success.contributions.len()),
                );
            }
            Ok(_) => {}
            Err(message) => {
                crate::logging::log_index_event(
                    crate::logging::IndexEvent::from_scope(
                        crate::logging::IndexEventKind::BuildFailed,
                        &scope,
                    )
                    .field("category", result.category.as_str())
                    .field("elapsed_ms", result.duration.as_millis())
                    .field("reason", message),
                );
            }
        }
    }
    if !tier2_benchmark_logging_enabled() {
        return;
    }
    match &result.outcome {
        Ok(success) => {
            let count = success
                .aggregate
                .get("count")
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(0);
            crate::slog_info!(
                "settle bench: tier2_category_end category={} job_id={} status=success total_ms={} scanned_files={} contributions={} count={}",
                result.category.as_str(),
                result.job_id,
                result.duration.as_millis(),
                success.scanned_files.len(),
                success.contributions.len(),
                count
            );
        }
        Err(message) => {
            crate::slog_info!(
                "settle bench: tier2_category_end category={} job_id={} status=failed total_ms={} error={}",
                result.category.as_str(),
                result.job_id,
                result.duration.as_millis(),
                message.replace('\n', " ")
            );
        }
    }
}

#[cfg(test)]
fn build_tier2_callgraph_snapshot(
    job: &InspectJob,
    allow_cold_build: bool,
) -> Option<Arc<CallgraphSnapshot>> {
    build_tier2_callgraph_snapshot_with_refresh_inner(job, allow_cold_build, false, &[], None)
        .expect("test snapshot readiness")
        .map(|(snapshot, _, _)| snapshot)
}

#[cfg(test)]
thread_local! {
    static LEGACY_VIEW_REFRESHES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

fn callgraph_path_identity_gap(job: &InspectJob) -> Result<Option<String>, CallGraphStoreError> {
    // Today's call-graph store is retired, so it records no path identity.
    let _ = job;
    Ok(None)
}

/// The verdict a dead-code snapshot from the data layer reports: a whole
/// snapshot, not derived from an earlier one, so the rollup recomputes every
/// verdict rather than patching a previous rollup.
const DATA_LAYER_SNAPSHOT: ProjectionVerdict = ProjectionVerdict {
    kind: ProjectionKind::Full,
    reason: Some("data_layer"),
    journal_bytes: 0,
    changed_files: 0,
};

fn build_tier2_callgraph_snapshot_with_refresh_inner(
    job: &InspectJob,
    _allow_cold_build: bool,
    _build_if_missing: bool,
    _refresh_paths: &[PathBuf],
    projection_cache: Option<&InspectManager>,
) -> Result<Option<(Arc<CallgraphSnapshot>, ProjectionVerdict, Duration)>, CallGraphStoreError> {
    let started = Instant::now();
    if !job.config.indexes.callgraph {
        crate::slog_info!(
            "tier2 dead_code: callgraph store disabled; reporting callgraph_unavailable"
        );
        return Ok(None);
    }
    // The root's checkout in the data layer answers. While its call graph
    // builds or catches up with edits, dead code reports it as building and
    // retries; it never answers from another store.
    let Some(manager) = projection_cache else {
        return Err(CallGraphStoreError::Building);
    };
    match manager.data_layer_dead_code.snapshot(&job.project_root) {
        crate::data_layer::tools::inspect::DeadCodeSnapshot::Ready(snapshot) => {
            Ok(Some((snapshot, DATA_LAYER_SNAPSHOT, started.elapsed())))
        }
        crate::data_layer::tools::inspect::DeadCodeSnapshot::Building => {
            Err(CallGraphStoreError::Building)
        }
        crate::data_layer::tools::inspect::DeadCodeSnapshot::Failed(reason) => {
            Err(CallGraphStoreError::Unavailable(reason))
        }
    }
}

#[cfg(test)]
fn canonicalize_for_snapshot(path: &Path) -> PathBuf {
    // Mirrors the projection's normalizer: snapshot paths are
    // verbatim-stripped, so test expectations must be too.
    crate::inspect::job::canonicalize_normalized(path)
}

fn load_contribution_freshness(
    cache: &(impl InspectCacheRead + ?Sized),
    category: InspectCategory,
) -> Result<Vec<CachedContributionFreshness>, String> {
    cache
        .contribution_freshness(category)
        .map_err(|error| error.to_string())
        .map(|records| {
            records
                .into_iter()
                .map(|(file_path, freshness)| CachedContributionFreshness {
                    file_path,
                    freshness,
                })
                .collect()
        })
}

fn freshness_record_relative_key(record: &CachedContributionFreshness) -> String {
    record.file_path.to_string_lossy().to_string()
}

fn relative_cache_key(project_root: &Path, path: &Path) -> String {
    path.strip_prefix(project_root)
        .unwrap_or(path)
        .to_string_lossy()
        .to_string()
}

/// Whether the job runs in a borrow-only root: one that may not write the
/// inspect store or build the call-graph store (a worktree that reads the main
/// checkout's stores). Builder state reports this as `build_denied`.
fn job_is_borrow_only(job: &InspectJob) -> bool {
    !job.inspect_writer || !job.callgraph_writer
}

/// The expected files (keyed like freshness records) that have no dead-code
/// facts among `contributions`. Coverage is keyed by each contribution's file
/// path relative to the root, the same key the inspect store uses, so a
/// payload `file` spelled differently (canonicalized) cannot read as missing.
fn dead_code_files_without_facts(
    project_root: &Path,
    expected_files: &BTreeMap<String, PathBuf>,
    contributions: &[FileContribution],
) -> Vec<PathBuf> {
    let covered = contributions
        .iter()
        .map(|contribution| relative_cache_key(project_root, &contribution.file_path))
        .collect::<BTreeSet<_>>();
    expected_files
        .iter()
        .filter(|(relative, _)| !covered.contains(*relative))
        .map(|(_, path)| path.clone())
        .collect()
}

fn load_contributions(
    cache: &(impl InspectCacheRead + ?Sized),
    job: &InspectJob,
) -> Result<Vec<FileContribution>, String> {
    cache
        .load_tier2_contributions(job.category)
        .map_err(|error| error.to_string())
        .map(|records| {
            records
                .into_iter()
                .map(|record| contribution_from_record(&job.project_root, record))
                .collect()
        })
}

fn dead_code_contributions_need_fact_refresh(
    cache: &InspectCache,
    job: &InspectJob,
) -> Result<bool, String> {
    cache
        .contributions_need_fact_refresh(job.category)
        .map_err(|error| error.to_string())
}

pub(super) fn dead_code_contribution_needs_fact_refresh(contribution: &FileContribution) -> bool {
    let Ok(parsed) = DeadCodeRefreshContribution::deserialize(&contribution.contribution) else {
        return true;
    };

    if parsed.facts_format_version
        != Some(super::scanners::dead_code::DEAD_CODE_FACTS_FORMAT_VERSION)
    {
        return true;
    }

    matches!(
        parsed.oxc_facts,
        Some(facts) if facts.format_version != FACTS_FORMAT_VERSION
    )
}

fn unused_exports_contributions_need_fact_refresh(
    cache: &InspectCache,
    job: &InspectJob,
) -> Result<bool, String> {
    cache
        .contributions_need_fact_refresh(job.category)
        .map_err(|error| error.to_string())
}

fn duplicates_contributions_need_fact_refresh(
    cache: &InspectCache,
    job: &InspectJob,
) -> Result<bool, String> {
    cache
        .contributions_need_fact_refresh(job.category)
        .map_err(|error| error.to_string())
}

pub(super) fn unused_exports_contribution_needs_fact_refresh(
    contribution: &FileContribution,
) -> bool {
    let top_level_oxc = contribution
        .contribution
        .get("provenance")
        .and_then(Value::as_str)
        == Some(OXC_PROVENANCE);
    let Ok(parsed) = UnusedExportsContribution::deserialize(&contribution.contribution) else {
        return false;
    };
    let uses_oxc =
        top_level_oxc || parsed.oxc_facts.is_some() || parsed.exports.iter().any(export_uses_oxc);
    if !uses_oxc {
        return false;
    }

    !matches!(
        parsed.oxc_facts,
        Some(facts) if facts.format_version == FACTS_FORMAT_VERSION
    )
}

fn contribution_from_record(
    project_root: &Path,
    record: super::cache::ContributionRecord,
) -> FileContribution {
    FileContribution::new(
        record.category,
        project_root.join(record.file_path),
        record.freshness,
        record.contribution,
    )
    .with_type_ref_names(record.type_ref_names)
}

fn run_tier2_scan(job: &InspectJob, oxc_result: Option<&OxcEngineResult>) -> InspectResult {
    use super::scanners;

    let mut result = match job.category {
        InspectCategory::DeadCode => {
            scanners::dead_code::run_dead_code_scan_with_oxc(job, oxc_result)
        }
        InspectCategory::UnusedExports => {
            scanners::unused_exports::run_unused_exports_scan_with_oxc(job, oxc_result)
        }
        InspectCategory::Duplicates => scanners::duplicates::run_duplicates_scan(job),
        InspectCategory::Cycles => scanners::cycles::run_cycles_scan_with_oxc(job, oxc_result),
        InspectCategory::Complexity => scanners::complexity::run_complexity_scan(job),
        other => InspectResult::failed(
            job,
            format!("inspect category '{other}' is not an active Tier 2 scanner"),
            Duration::from_secs(0),
        ),
    };
    if job.category == InspectCategory::DeadCode {
        if let Ok(success) = &mut result.outcome {
            super::job::attach_dead_code_language_coverage(
                &mut success.aggregate,
                &job.scope_files,
            );
        }
    }
    result
}

fn roll_up_tier2_contributions(job: &InspectJob, contributions: &[FileContribution]) -> Value {
    roll_up_tier2_contributions_with_limit(job, contributions, None)
}

fn roll_up_tier2_contributions_with_limit(
    job: &InspectJob,
    contributions: &[FileContribution],
    drill_down_limit: Option<usize>,
) -> Value {
    let mut payload = match job.category {
        InspectCategory::DeadCode => {
            roll_up_dead_code_contributions(job, contributions, drill_down_limit)
        }
        InspectCategory::UnusedExports => {
            roll_up_unused_exports_contributions(job, contributions, drill_down_limit)
        }
        InspectCategory::Duplicates => {
            roll_up_duplicate_contributions(job, contributions, drill_down_limit)
        }
        InspectCategory::Cycles => {
            roll_up_cycle_contributions(job, contributions, drill_down_limit)
        }
        InspectCategory::Complexity => {
            roll_up_complexity_contributions(job, contributions, drill_down_limit)
        }
        _ => json!({
            "count": 0,
            "items": [],
            "scanned_files": contributions.len(),
        }),
    };
    if job.category == InspectCategory::DeadCode {
        super::job::attach_dead_code_language_coverage(&mut payload, &job.scope_files);
    }
    payload
}

fn scoped_tier2_payload_from_contributions(
    manager: Option<&InspectManager>,
    snapshot: &InspectSnapshot,
    category: InspectCategory,
    cache: &(impl InspectCacheRead + ?Sized),
    project_payload: Value,
    scope: &JobScope,
) -> Result<Value, String> {
    // A pending pass scanned no current graph. A later unlock cannot make its
    // cached source contributions into a completed scoped analysis.
    if scope.is_project_wide() || project_payload["status"] == "pending" {
        return Ok(project_payload);
    }

    let project_scope = JobScope::for_project(snapshot.project_root.clone());
    let rollup_job = match scoped_tier2_rollup_job(manager, snapshot, category, &project_scope) {
        Ok(job) => job,
        Err(error) if error.is_transient_lock_contention() => {
            return Ok(callgraph_pending_payload())
        }
        Err(error) => return Err(error.to_string()),
    };
    if let (Some(manager), Some(graph)) = (manager, rollup_job.callgraph_snapshot.as_deref()) {
        if graph.data_layer.is_some() {
            // Global liveness includes callers outside the scope, so the
            // whole project is worked out before the findings are filtered.
            let payload = manager.data_layer_dead_code_aggregate(&rollup_job, graph)?;
            return Ok(filter_payload_for_scope(payload, scope));
        }
    }
    let contributions = load_contributions(cache, &rollup_job)?;
    if let (Some(_), Some(graph)) = (manager, rollup_job.callgraph_snapshot.as_deref()) {
        let public_api_files =
            super::scanners::dead_code::collect_public_api_files(&snapshot.project_root);
        let roles = super::entry_points::resolve_project_roles(&snapshot.project_root);
        // Global liveness includes callers outside the scope, so the whole
        // project is rolled up before the findings are filtered.
        let mut payload =
            super::scanners::dead_code::aggregate_dead_code_contributions_with_snapshot(
                &snapshot.project_root,
                graph,
                &contributions,
                &public_api_files,
                &roles,
                None,
            );
        super::job::attach_dead_code_language_coverage(&mut payload, &rollup_job.scope_files);
        return Ok(filter_payload_for_scope(payload, scope));
    }
    let full_payload = roll_up_tier2_contributions_with_limit(&rollup_job, &contributions, None);
    let scoped_payload = filter_payload_for_scope(full_payload, scope);
    Ok(scoped_payload)
}

fn scoped_tier2_rollup_job(
    manager: Option<&InspectManager>,
    snapshot: &InspectSnapshot,
    category: InspectCategory,
    scope: &JobScope,
) -> Result<InspectJob, CallGraphStoreError> {
    let mut job = InspectJob {
        job_id: 0,
        key: JobKey::for_project_category(category),
        category,
        // Only dead code needs a file inventory here. Other categories use it
        // solely for language coverage, which the scoped filter removes.
        scope_files: if category == InspectCategory::DeadCode {
            scope_files(&snapshot.project_root, scope)
        } else {
            Vec::new()
        },
        project_root: snapshot.project_root.clone(),
        inspect_dir: snapshot.inspect_dir.clone(),
        config: Arc::clone(&snapshot.config),
        symbol_cache: Arc::clone(&snapshot.symbol_cache),
        inspect_writer: snapshot.inspect_writer,
        callgraph_writer: snapshot.callgraph_writer,
        callgraph_snapshot: None,
    };

    if category == InspectCategory::DeadCode {
        // Scoped read-path rollups recompute dead-code liveness from cached
        // contributions. Use a real ready store snapshot when one exists; if no
        // snapshot is available, leave it absent so the rollup reports degraded
        // callgraph_unavailable instead of treating an empty graph as truth.
        job.callgraph_snapshot = retry_dead_code_store_read(|| {
            build_tier2_callgraph_snapshot_with_refresh_inner(&job, false, false, &[], manager)
        })?
        .map(|(snapshot, _, _)| snapshot);
    }

    Ok(job)
}

fn roll_up_dead_code_contributions(
    job: &InspectJob,
    contributions: &[FileContribution],
    drill_down_limit: Option<usize>,
) -> Value {
    let Some(snapshot) = job.callgraph_snapshot.as_deref() else {
        return super::scanners::dead_code::callgraph_unavailable_aggregate_for_job(job);
    };

    let public_api_files = super::scanners::dead_code::collect_public_api_files(&job.project_root);
    let roles = super::entry_points::resolve_project_roles(&job.project_root);
    super::scanners::dead_code::aggregate_dead_code_contributions_with_snapshot(
        &job.project_root,
        snapshot,
        contributions,
        &public_api_files,
        &roles,
        drill_down_limit,
    )
}

fn roll_up_unused_exports_contributions(
    job: &InspectJob,
    contributions: &[FileContribution],
    drill_down_limit: Option<usize>,
) -> Value {
    let parsed = contributions
        .iter()
        .filter_map(|contribution| {
            serde_json::from_value::<UnusedExportsContribution>(contribution.contribution.clone())
                .ok()
        })
        .collect::<Vec<_>>();

    if parsed.iter().any(|scan| scan.oxc_facts.is_some()) {
        return roll_up_unused_exports_oxc_contributions(job, &parsed, drill_down_limit);
    }

    let (public_api_files, package_warnings) = unused_public_api_entries(&job.project_root);
    let mut imported_by: BTreeMap<(String, String), BTreeSet<String>> = BTreeMap::new();
    let mut uncertain_by: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for scan in &parsed {
        for import in &scan.imports {
            let Some(resolved_file) = &import.resolved_file else {
                continue;
            };
            for name in &import.named {
                if name == "*" {
                    uncertain_by
                        .entry(resolved_file.clone())
                        .or_default()
                        .insert(scan.file.clone());
                } else {
                    imported_by
                        .entry((resolved_file.clone(), name.clone()))
                        .or_default()
                        .insert(scan.file.clone());
                }
            }
        }
    }

    let mut count = 0usize;
    let mut items = Vec::new();
    let mut generated_count = 0usize;
    let mut generated_items = Vec::new();
    let test_only_count = 0usize;
    let test_only_items = Vec::new();
    let mut uncertain_count = 0usize;
    let mut uncertain_items = Vec::new();
    let mut excluded = ExcludedTestTally::default();
    let has_source_gaps = parsed.iter().any(|scan| {
        scan.skipped_files
            .iter()
            .any(|skip| skip["reason"] == super::scanners::source_text::GRAPH_TOO_LARGE)
    });
    for scan in &parsed {
        if has_source_gaps || scan.excluded_from_graph {
            continue;
        }
        if public_api_files.contains(&scan.file) {
            continue;
        }
        // Mirror the fresh-scan path: test files and fixtures are analyzed for
        // the product exports they keep live, but their own exports are
        // withheld from the product count and tallied as excluded.
        let test_tree = is_test_tree_file(&scan.file);
        let generated_file = super::generated::is_generated_file_with_cached_hint(
            &job.project_root,
            &scan.file,
            scan.generated,
        );

        for export in &scan.exports {
            if export_uses_oxc(export) {
                match export.verdict.unwrap_or(LivenessVerdict::Unused) {
                    LivenessVerdict::Used => continue,
                    LivenessVerdict::Uncertain if test_tree => continue,
                    LivenessVerdict::Uncertain => {
                        uncertain_count += 1;
                        if drill_down_limit.is_none_or(|limit| uncertain_items.len() < limit) {
                            uncertain_items.push(json!({
                                "file": scan.file,
                                "symbol": export.symbol,
                                "kind": export.kind,
                                "line": export.line,
                                "reason": export.reason.as_deref().unwrap_or("oxc_uncertain"),
                                "provenance": export.provenance.as_deref().unwrap_or(OXC_PROVENANCE),
                            }));
                        }
                        continue;
                    }
                    LivenessVerdict::Unused => {}
                }
            } else {
                let imported = imported_by
                    .get(&(scan.file.clone(), export.symbol.clone()))
                    .map(|files| !files.is_empty())
                    .unwrap_or(false);
                let uncertain = uncertain_by
                    .get(&scan.file)
                    .map(|files| !files.is_empty())
                    .unwrap_or(false);

                if imported {
                    continue;
                }
                if uncertain && !test_tree {
                    uncertain_count += 1;
                    if drill_down_limit.is_none_or(|limit| uncertain_items.len() < limit) {
                        uncertain_items.push(json!({
                            "file": scan.file,
                            "symbol": export.symbol,
                            "kind": export.kind,
                            "line": export.line,
                            "reason": "wildcard_import",
                        }));
                    }
                    continue;
                }
            }

            let mut item = json!({
                "file": scan.file,
                "symbol": export.symbol,
                "kind": export.kind,
                "line": export.line,
            });
            if test_tree {
                excluded.record(&scan.file);
                continue;
            }
            if let Some(provenance) = &export.provenance {
                item["provenance"] = json!(provenance);
            }
            if generated_file {
                item["generated"] = json!(true);
                generated_count += 1;
                generated_items.push(item);
            } else {
                count += 1;
                items.push(item);
            }
        }
    }

    let roles = super::entry_points::resolve_project_roles(&job.project_root);
    let items = super::entry_points::rank_and_truncate_items(items, &roles, drill_down_limit);
    let generated_items =
        super::entry_points::rank_and_truncate_items(generated_items, &roles, drill_down_limit);
    let top = super::entry_points::top_preview_symbols(&items);
    let generated_top = generated_items
        .iter()
        .take(super::entry_points::TOP_PREVIEW_ITEMS)
        .cloned()
        .collect::<Vec<_>>();
    let mut all_items = items;
    all_items.extend(generated_items.iter().cloned());
    if let Some(limit) = drill_down_limit {
        all_items.truncate(limit);
    }
    let test_only_items =
        super::entry_points::rank_and_truncate_items(test_only_items, &roles, drill_down_limit);
    let test_only_top = test_only_items
        .iter()
        .take(super::entry_points::TOP_PREVIEW_ITEMS)
        .cloned()
        .collect::<Vec<_>>();

    let (parse_errors, skipped_files) = unused_exports_honesty_fields(&parsed);
    let mut aggregate = json!({
        "count": count,
        "generated_count": generated_count,
        "total_count": count + test_only_count + generated_count,
        "items": all_items,
        "top": top,
        "generated_items": generated_items,
        "generated_top": generated_top,
        "test_only_count": test_only_count,
        "test_only_items": test_only_items,
        "test_only_top": test_only_top,
        "scanned_files": parsed.len(),
        "languages_skipped": skipped_languages(&job.scope_files, LanguageSkipMode::UnusedExports),
        "uncertain_count": uncertain_count,
        "uncertain_items": uncertain_items,
        "complete": parse_errors.is_empty() && skipped_files.is_empty(),
    });
    excluded.write_into(&mut aggregate);
    if !parse_errors.is_empty() {
        aggregate["parse_errors"] = Value::Array(parse_errors);
    }
    if !skipped_files.is_empty() {
        aggregate["skipped_files_count"] = json!(skipped_files.len());
        aggregate["skipped_files"] = Value::Array(skipped_files);
    }
    if !package_warnings.is_empty() {
        aggregate["note"] = Value::String(package_warnings.join("; "));
    }
    aggregate
}

fn roll_up_unused_exports_oxc_contributions(
    job: &InspectJob,
    parsed: &[UnusedExportsContribution],
    drill_down_limit: Option<usize>,
) -> Value {
    let (public_api_files, package_warnings) = unused_public_api_entries(&job.project_root);
    let facts = parsed
        .iter()
        .filter_map(|scan| {
            if scan.excluded_from_graph {
                return None;
            }
            let oxc_facts = scan.oxc_facts.as_ref()?;
            let path = job.project_root.join(&scan.file);
            Some(FileFacts {
                file_id: FileId(0),
                path: normalize_input_path(&job.project_root, &path),
                content_hash: oxc_facts.content_hash.clone(),
                exports: oxc_facts.exports.clone(),
                imports: oxc_facts.imports.clone(),
                re_exports: oxc_facts.re_exports.clone(),
                dynamic_imports: oxc_facts.dynamic_imports.clone(),
                same_file_value_references: oxc_facts.same_file_value_references.clone(),
                used_import_bindings: oxc_facts.used_import_bindings.clone(),
                type_referenced_import_bindings: oxc_facts.type_referenced_import_bindings.clone(),
                value_referenced_import_bindings: oxc_facts
                    .value_referenced_import_bindings
                    .clone(),
                parse_error: oxc_facts.parse_error.clone(),
            })
        })
        .collect::<Vec<_>>();
    let generated_by_file = parsed
        .iter()
        .map(|scan| {
            (
                scan.file.clone(),
                !scan.excluded_from_graph
                    && super::generated::is_generated_file_with_cached_hint(
                        &job.project_root,
                        &scan.file,
                        scan.generated,
                    ),
            )
        })
        .collect::<BTreeMap<_, _>>();
    let entry_point_set = crate::inspect::entry_points::resolve_entry_points(&job.project_root);
    let oxc_result = analyze_file_facts(
        &job.project_root,
        facts,
        AnalyzeOptions {
            entry_points: Vec::new(),
            public_api_files: entry_point_set.public_api_files(),
            executable_root_exports: entry_point_set.executable_root_exports(),
            force_reparse_files: Vec::new(),
            entry_reachability: false,
        },
        Vec::new(),
    );
    let roles = super::entry_points::resolve_project_roles(&job.project_root);

    let mut count = 0usize;
    let mut items = Vec::new();
    let mut generated_count = 0usize;
    let mut generated_items = Vec::new();
    let mut test_only_count = 0usize;
    let mut test_only_items = Vec::new();
    let mut uncertain_count = 0usize;
    let mut uncertain_items = Vec::new();
    let mut excluded = ExcludedTestTally::default();
    let has_source_gaps = parsed.iter().any(|scan| {
        scan.skipped_files
            .iter()
            .any(|skip| skip["reason"] == super::scanners::source_text::GRAPH_TOO_LARGE)
    });
    for file in &oxc_result.files {
        if has_source_gaps || public_api_files.contains(&file.relative_file) {
            continue;
        }
        let test_tree = is_test_tree_file(&file.relative_file);
        let generated_file = generated_by_file
            .get(&file.relative_file)
            .copied()
            .unwrap_or_else(|| {
                super::generated::is_generated_file(
                    &job.project_root,
                    Path::new(&file.relative_file),
                )
            });

        for export in &file.exports {
            match export.verdict {
                LivenessVerdict::Used => {
                    if !test_tree && !export.test_only_reference_files.is_empty() {
                        let mut item = json!({
                            "file": file.relative_file,
                            "symbol": export.symbol,
                            "kind": export.kind,
                            "line": export.line,
                            "provenance": export.provenance,
                            "used_by": export.test_only_reference_files,
                        });
                        add_oxc_reexport_contexts(&mut item, &export.also_reexported);
                        if generated_file {
                            item["generated"] = json!(true);
                            generated_count += 1;
                            generated_items.push(item);
                        } else {
                            test_only_count += 1;
                            test_only_items.push(item);
                        }
                    } else if export.only_same_file_references {
                        // Mirror the fresh scan: an export referenced only
                        // inside its own file is an unused export.
                        if test_tree {
                            excluded.record(&file.relative_file);
                            continue;
                        }
                        let mut item = json!({
                            "file": file.relative_file,
                            "symbol": export.symbol,
                            "kind": export.kind,
                            "line": export.line,
                            "provenance": export.provenance,
                            "reason": super::scanners::unused_exports::ONLY_SAME_FILE_REFERENCES_REASON,
                        });
                        add_oxc_reexport_contexts(&mut item, &export.also_reexported);
                        if generated_file {
                            item["generated"] = json!(true);
                            generated_count += 1;
                            generated_items.push(item);
                        } else {
                            count += 1;
                            items.push(item);
                        }
                    }
                }
                LivenessVerdict::Uncertain if test_tree => {}
                LivenessVerdict::Uncertain => {
                    uncertain_count += 1;
                    if drill_down_limit.is_none_or(|limit| uncertain_items.len() < limit) {
                        let mut item = json!({
                            "file": file.relative_file,
                            "symbol": export.symbol,
                            "kind": export.kind,
                            "line": export.line,
                            "reason": export.reason,
                            "provenance": export.provenance,
                        });
                        add_oxc_reexport_contexts(&mut item, &export.also_reexported);
                        uncertain_items.push(item);
                    }
                }
                LivenessVerdict::Unused => {
                    if !test_tree && !export.test_only_reference_files.is_empty() {
                        let mut item = json!({
                            "file": file.relative_file,
                            "symbol": export.symbol,
                            "kind": export.kind,
                            "line": export.line,
                            "provenance": export.provenance,
                            "used_by": export.test_only_reference_files,
                        });
                        add_oxc_reexport_contexts(&mut item, &export.also_reexported);
                        if generated_file {
                            item["generated"] = json!(true);
                            generated_count += 1;
                            generated_items.push(item);
                        } else {
                            test_only_count += 1;
                            test_only_items.push(item);
                        }
                        continue;
                    }
                    if export.has_references {
                        continue;
                    }
                    if test_tree {
                        excluded.record(&file.relative_file);
                        continue;
                    }
                    let mut item = json!({
                        "file": file.relative_file,
                        "symbol": export.symbol,
                        "kind": export.kind,
                        "line": export.line,
                        "provenance": export.provenance,
                    });
                    add_oxc_reexport_contexts(&mut item, &export.also_reexported);
                    if generated_file {
                        item["generated"] = json!(true);
                        generated_count += 1;
                        generated_items.push(item);
                    } else {
                        count += 1;
                        items.push(item);
                    }
                }
            }
        }
    }

    let items = super::entry_points::rank_and_truncate_items(items, &roles, drill_down_limit);
    let generated_items =
        super::entry_points::rank_and_truncate_items(generated_items, &roles, drill_down_limit);
    let top = super::entry_points::top_preview_symbols(&items);
    let generated_top = generated_items
        .iter()
        .take(super::entry_points::TOP_PREVIEW_ITEMS)
        .cloned()
        .collect::<Vec<_>>();
    let mut all_items = items;
    all_items.extend(generated_items.iter().cloned());
    if let Some(limit) = drill_down_limit {
        all_items.truncate(limit);
    }
    let test_only_items =
        super::entry_points::rank_and_truncate_items(test_only_items, &roles, drill_down_limit);
    let test_only_top = test_only_items
        .iter()
        .take(super::entry_points::TOP_PREVIEW_ITEMS)
        .cloned()
        .collect::<Vec<_>>();
    let (mut parse_errors, skipped_files) = unused_exports_honesty_fields(parsed);
    for scan in parsed {
        if let Some(oxc_facts) = &scan.oxc_facts {
            if oxc_facts.format_version != FACTS_FORMAT_VERSION {
                parse_errors.push(json!({
                    "file": scan.file,
                    "message": format!(
                        "unsupported oxc facts format {}; expected {}",
                        oxc_facts.format_version, FACTS_FORMAT_VERSION
                    ),
                }));
            }
        }
    }

    let mut aggregate = json!({
        "count": count,
        "generated_count": generated_count,
        "total_count": count + test_only_count + generated_count,
        "items": all_items,
        "top": top,
        "generated_items": generated_items,
        "generated_top": generated_top,
        "test_only_count": test_only_count,
        "test_only_items": test_only_items,
        "test_only_top": test_only_top,
        "scanned_files": parsed.len(),
        "languages_skipped": skipped_languages(&job.scope_files, LanguageSkipMode::UnusedExports),
        "uncertain_count": uncertain_count,
        "uncertain_items": uncertain_items,
        "complete": parse_errors.is_empty() && skipped_files.is_empty(),
    });
    excluded.write_into(&mut aggregate);
    if !parse_errors.is_empty() {
        aggregate["parse_errors"] = Value::Array(parse_errors);
    }
    if !skipped_files.is_empty() {
        aggregate["skipped_files_count"] = json!(skipped_files.len());
        aggregate["skipped_files"] = Value::Array(skipped_files);
    }
    if !package_warnings.is_empty() {
        aggregate["note"] = Value::String(package_warnings.join("; "));
    }
    aggregate
}

fn add_oxc_reexport_contexts(
    item: &mut Value,
    contexts: &[crate::inspect::oxc_engine::OxcReExportContext],
) {
    if !contexts.is_empty() {
        item["also_reexported"] = json!(contexts);
    }
}

fn unused_exports_honesty_fields(parsed: &[UnusedExportsContribution]) -> (Vec<Value>, Vec<Value>) {
    let mut parse_error_keys = BTreeSet::new();
    let mut parse_errors = Vec::new();
    let mut skipped_file_keys = BTreeSet::new();
    let mut skipped_files = Vec::new();
    for contribution in parsed {
        for value in &contribution.parse_errors {
            let key = value.to_string();
            if parse_error_keys.insert(key) {
                parse_errors.push(value.clone());
            }
        }
        for value in &contribution.skipped_files {
            let key = value.to_string();
            if skipped_file_keys.insert(key) {
                skipped_files.push(value.clone());
            }
        }
    }
    (parse_errors, skipped_files)
}

fn roll_up_duplicate_contributions(
    job: &InspectJob,
    contributions: &[FileContribution],
    drill_down_limit: Option<usize>,
) -> Value {
    super::scanners::duplicates::aggregate_duplicate_contributions_with_limit(
        contributions,
        skipped_languages(&job.scope_files, LanguageSkipMode::Duplicates),
        drill_down_limit,
        &job.config.inspect.duplicates.expected_mirrors,
    )
}

fn roll_up_cycle_contributions(
    job: &InspectJob,
    contributions: &[FileContribution],
    drill_down_limit: Option<usize>,
) -> Value {
    super::scanners::cycles::aggregate_cycle_contributions_with_limit(
        &job.project_root,
        contributions,
        skipped_languages(&job.scope_files, LanguageSkipMode::Cycles),
        drill_down_limit,
    )
}

fn roll_up_complexity_contributions(
    job: &InspectJob,
    contributions: &[FileContribution],
    drill_down_limit: Option<usize>,
) -> Value {
    super::scanners::complexity::aggregate_complexity_contributions_with_limit(
        &job.project_root,
        contributions,
        drill_down_limit,
    )
}

#[cfg(test)]
const MAX_DRILL_DOWN_ITEMS: usize = 100;

#[derive(Debug, Clone, Deserialize)]
struct ExportContribution {
    symbol: String,
    kind: String,
    line: u32,
    #[serde(default)]
    verdict: Option<LivenessVerdict>,
    #[serde(default)]
    reason: Option<String>,
    #[serde(default)]
    provenance: Option<String>,
}

fn export_uses_oxc(export: &ExportContribution) -> bool {
    export.verdict.is_some() || export.provenance.as_deref() == Some(OXC_PROVENANCE)
}

#[derive(Debug, Clone, Deserialize)]
struct DeadCodeRefreshContribution {
    #[serde(default)]
    facts_format_version: Option<u32>,
    #[serde(default)]
    oxc_facts: Option<OxcFactsContribution>,
}

#[derive(Debug, Clone, Deserialize)]
struct UnusedExportsContribution {
    #[serde(default)]
    excluded_from_graph: bool,
    file: String,
    #[serde(default)]
    generated: Option<bool>,
    exports: Vec<ExportContribution>,
    #[serde(default)]
    imports: Vec<ImportContribution>,
    #[serde(default)]
    oxc_facts: Option<OxcFactsContribution>,
    #[serde(default)]
    parse_errors: Vec<Value>,
    #[serde(default)]
    skipped_files: Vec<Value>,
}

#[derive(Debug, Clone, Deserialize)]
struct ImportContribution {
    resolved_file: Option<String>,
    named: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
struct OxcFactsContribution {
    format_version: u32,
    content_hash: String,
    exports: Vec<ExportFact>,
    imports: Vec<ImportFact>,
    re_exports: Vec<ReExportFact>,
    dynamic_imports: Vec<DynamicImportFact>,
    same_file_value_references: BTreeSet<String>,
    used_import_bindings: BTreeSet<String>,
    type_referenced_import_bindings: BTreeSet<String>,
    value_referenced_import_bindings: BTreeSet<String>,
    #[serde(default)]
    parse_error: Option<String>,
}

#[derive(Debug, Clone, Copy)]
enum LanguageSkipMode {
    Duplicates,
    Cycles,
    UnusedExports,
}

fn category_uses_oxc(category: InspectCategory) -> bool {
    matches!(
        category,
        InspectCategory::DeadCode | InspectCategory::UnusedExports | InspectCategory::Cycles
    )
}

fn skipped_languages(files: &[PathBuf], mode: LanguageSkipMode) -> Vec<String> {
    files
        .iter()
        .filter_map(|file| skipped_language(file, mode))
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

fn skipped_language(file: &Path, mode: LanguageSkipMode) -> Option<String> {
    let Some(language) = crate::parser::detect_language(file) else {
        return match mode {
            LanguageSkipMode::Duplicates => Some("unknown".to_string()),
            LanguageSkipMode::Cycles => Some("unknown".to_string()),
            LanguageSkipMode::UnusedExports => None,
        };
    };

    let skipped = match mode {
        LanguageSkipMode::Duplicates => !duplicates_supports_language(language),
        LanguageSkipMode::Cycles => !is_js_ts_language(language),
        LanguageSkipMode::UnusedExports => !is_js_ts_language(language),
    };
    skipped.then(|| language_name(language).to_string())
}

fn duplicates_supports_language(language: crate::parser::LangId) -> bool {
    !matches!(
        language,
        crate::parser::LangId::Bash
            | crate::parser::LangId::Html
            | crate::parser::LangId::Json
            | crate::parser::LangId::Scala
            | crate::parser::LangId::Solidity
            | crate::parser::LangId::Scss
            | crate::parser::LangId::Vue
            | crate::parser::LangId::Markdown
            | crate::parser::LangId::Java
            | crate::parser::LangId::Ruby
            | crate::parser::LangId::Kotlin
            | crate::parser::LangId::Swift
            | crate::parser::LangId::Php
            | crate::parser::LangId::Lua
            | crate::parser::LangId::Perl
            | crate::parser::LangId::Pascal
            | crate::parser::LangId::R
            | crate::parser::LangId::Groovy
            | crate::parser::LangId::ObjC
            | crate::parser::LangId::Toml
    )
}

fn is_js_ts_language(language: crate::parser::LangId) -> bool {
    matches!(
        language,
        crate::parser::LangId::TypeScript
            | crate::parser::LangId::Tsx
            | crate::parser::LangId::JavaScript
    )
}

fn language_name(language: crate::parser::LangId) -> &'static str {
    match language {
        crate::parser::LangId::TypeScript => "typescript",
        crate::parser::LangId::Tsx => "tsx",
        crate::parser::LangId::JavaScript => "javascript",
        crate::parser::LangId::Python => "python",
        crate::parser::LangId::Rust => "rust",
        crate::parser::LangId::Go => "go",
        crate::parser::LangId::C => "c",
        crate::parser::LangId::Cpp => "cpp",
        crate::parser::LangId::Cuda => "cuda",
        crate::parser::LangId::Metal => "metal",
        crate::parser::LangId::Zig => "zig",
        crate::parser::LangId::CSharp => "csharp",
        crate::parser::LangId::Bash => "bash",
        crate::parser::LangId::Html => "html",
        crate::parser::LangId::Markdown => "markdown",
        crate::parser::LangId::Yaml => "yaml",
        crate::parser::LangId::Solidity => "solidity",
        crate::parser::LangId::Scss => "scss",
        crate::parser::LangId::Vue => "vue",
        crate::parser::LangId::Json => "json",
        crate::parser::LangId::Scala => "scala",
        crate::parser::LangId::Java => "java",
        crate::parser::LangId::Ruby => "ruby",
        crate::parser::LangId::Kotlin => "kotlin",
        crate::parser::LangId::Swift => "swift",
        crate::parser::LangId::Php => "php",
        crate::parser::LangId::Lua => "lua",
        crate::parser::LangId::Perl => "perl",
        crate::parser::LangId::Pascal => "pascal",
        crate::parser::LangId::R => "r",
        crate::parser::LangId::Groovy => "groovy",
        crate::parser::LangId::ObjC => "objc",
        crate::parser::LangId::Toml => "toml",
    }
}

fn unused_public_api_entries(project_root: &Path) -> (BTreeSet<String>, Vec<String>) {
    let entry_points = crate::inspect::entry_points::resolve_entry_points(project_root);
    (
        entry_points.public_api_files_relative(project_root),
        entry_points.warnings().to_vec(),
    )
}

fn filter_outcome_for_scope_with_contributions(
    dead_code_sources: &crate::data_layer::tools::inspect::DeadCodeSources,
    manager: Option<&InspectManager>,
    outcome: JobOutcome,
    snapshot: &InspectSnapshot,
    category: InspectCategory,
    cache: &(impl InspectCacheRead + ?Sized),
    scope: &JobScope,
) -> JobOutcome {
    // Fresh source files do not establish graph liveness before the root has a
    // call graph, so no answer, fresh or cached, is served until it has one.
    if category == InspectCategory::DeadCode
        && snapshot.config.indexes.callgraph
        && dead_code_sources.callgraph_busy(&snapshot.project_root)
    {
        return JobOutcome::Fresh {
            payload: callgraph_pending_payload(),
        };
    }
    if !category.is_tier2() || scope.is_project_wide() {
        return filter_outcome_for_scope(outcome, scope);
    }

    match outcome {
        JobOutcome::Fresh { payload } => {
            match scoped_tier2_payload_from_contributions(
                manager, snapshot, category, cache, payload, scope,
            ) {
                Ok(payload) => JobOutcome::Fresh { payload },
                Err(message) => JobOutcome::Failed { message },
            }
        }
        JobOutcome::Stale { cached, in_flight } => match cached {
            Some(payload) => {
                match scoped_tier2_payload_from_contributions(
                    manager, snapshot, category, cache, payload, scope,
                ) {
                    Ok(payload) => JobOutcome::Stale {
                        cached: Some(payload),
                        in_flight,
                    },
                    Err(message) => JobOutcome::Failed { message },
                }
            }
            None => JobOutcome::Stale {
                cached: None,
                in_flight,
            },
        },
        JobOutcome::Pending { in_flight, wait } => JobOutcome::Pending { in_flight, wait },
        JobOutcome::Failed { message } => JobOutcome::Failed { message },
    }
}

fn filter_outcome_for_scope(outcome: JobOutcome, scope: &JobScope) -> JobOutcome {
    match outcome {
        JobOutcome::Fresh { payload } => JobOutcome::Fresh {
            payload: filter_payload_for_scope(payload, scope),
        },
        JobOutcome::Stale { cached, in_flight } => JobOutcome::Stale {
            cached: cached.map(|payload| filter_payload_for_scope(payload, scope)),
            in_flight,
        },
        JobOutcome::Pending { in_flight, wait } => JobOutcome::Pending { in_flight, wait },
        JobOutcome::Failed { message } => JobOutcome::Failed { message },
    }
}

#[cfg(test)]
pub(crate) fn filter_payload_for_scope_for_test(payload: Value, scope: &JobScope) -> Value {
    filter_payload_for_scope(payload, scope)
}

fn filter_payload_for_scope(mut payload: serde_json::Value, scope: &JobScope) -> serde_json::Value {
    if scope.is_project_wide() {
        return payload;
    }

    // Scoped Tier 2 callers pass an uncapped rollup into this filter and cap
    // drill-down only afterwards, so the recomputed count below remains the
    // true in-scope total rather than the size of a capped sample.
    let duplicates = payload.get("duplicated_lines").is_some();
    if duplicates {
        // A duplicate group may cross the boundary. Keep its scoped occurrences
        // even if only one remains, and label the count as groups touching scope.
        for key in ["items", "groups", "generated_items"] {
            if let Some(values) = payload.get_mut(key).and_then(Value::as_array_mut) {
                values.retain_mut(|value| {
                    let Some(files) = value.get_mut("files").and_then(Value::as_array_mut) else {
                        return false;
                    };
                    files.retain(|file| {
                        file.as_str().is_some_and(|file| {
                            scope.contains_display_path(display_file_from_occurrence(file))
                        })
                    });
                    let first = files.first().and_then(Value::as_str).map(str::to_string);
                    if let Some(first) = first {
                        let lines = value["files"]
                            .as_array()
                            .into_iter()
                            .flatten()
                            .filter_map(Value::as_str)
                            .filter_map(parse_duplicate_occurrence)
                            .map(|(_, start, end)| end.saturating_sub(start).saturating_add(1))
                            .sum::<u64>();
                        if value.get("duplicated_lines").is_some() {
                            value["duplicated_lines"] = json!(lines);
                        }
                        update_duplicate_group_sample(value, &first);
                        true
                    } else {
                        false
                    }
                });
            }
        }
    }
    if let Some(items) = payload
        .get_mut("items")
        .and_then(|value| value.as_array_mut())
    {
        let count = if duplicates {
            items.len()
        } else {
            filter_values_for_scope(items, scope)
        };
        let headline_count = items
            .iter()
            .filter(|item| item["generated"] != true)
            .count();
        let largest_cycle = items
            .iter()
            .filter_map(|item| item.get("files").and_then(Value::as_array).map(Vec::len))
            .max();
        if let Some(object) = payload.as_object_mut() {
            object.insert("count".to_string(), serde_json::json!(headline_count));
            if object.contains_key("largest") {
                object.insert("scope_relation".to_string(), json!("touches"));
                object.insert(
                    "largest".to_string(),
                    serde_json::json!(largest_cycle.unwrap_or(0)),
                );
            }
            if object.contains_key("total_groups") {
                object.insert("total_groups".to_string(), serde_json::json!(count));
            }
            if object.contains_key("groups_count") {
                object.insert("groups_count".to_string(), serde_json::json!(count));
            }
        }
    }

    if let Some(groups) = payload
        .get_mut("groups")
        .and_then(|value| value.as_array_mut())
    {
        let count = if duplicates {
            groups.len()
        } else {
            filter_values_for_scope(groups, scope)
        };
        if let Some(object) = payload.as_object_mut() {
            object.insert("count".to_string(), serde_json::json!(count));
            object.insert("total_groups".to_string(), serde_json::json!(count));
            if object.contains_key("groups_count") {
                object.insert("groups_count".to_string(), serde_json::json!(count));
            }
        }
    }

    // `by_language` is a project-wide breakdown computed before scope filtering.
    // Leaving it in a scoped payload contradicts the recomputed in-scope `count`
    // (e.g. count: 3 alongside `(rust 214, ts 143)`). The filtered items don't
    // carry per-item language, so we can't faithfully recompute it — drop it so
    // the scoped summary doesn't render a misleading project-wide breakdown.
    if let Some(object) = payload.as_object_mut() {
        for (items_key, count_key, top_key) in [
            ("test_only_items", "test_only_count", "test_only_top"),
            ("generated_items", "generated_count", "generated_top"),
            ("uncertain_items", "uncertain_count", "uncertain_top"),
        ] {
            if let Some(items) = object.get_mut(items_key).and_then(Value::as_array_mut) {
                if !duplicates {
                    filter_values_for_scope(items, scope);
                }
                let count = items.len();
                let top = scoped_top_preview(items);
                object.insert(count_key.to_string(), json!(count));
                object.insert(top_key.to_string(), top);
            } else {
                object.remove(count_key);
                object.remove(top_key);
            }
        }
        if object.contains_key("total_count") {
            let total = ["count", "test_only_count", "generated_count"]
                .iter()
                .filter_map(|key| object.get(*key).and_then(Value::as_u64))
                .sum::<u64>();
            object.insert("total_count".to_string(), json!(total));
        }
        if object.contains_key("worst") {
            let worst = object
                .get("items")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .max_by(|left, right| {
                    left["complexity"]
                        .as_u64()
                        .cmp(&right["complexity"].as_u64())
                        .then_with(|| right["file"].as_str().cmp(&left["file"].as_str()))
                        .then_with(|| right["function"].as_str().cmp(&left["function"].as_str()))
                        .then_with(|| right["line"].as_u64().cmp(&left["line"].as_u64()))
                })
                .cloned()
                .unwrap_or(Value::Null);
            object.insert("worst".to_string(), worst);
        }
        if object.contains_key("top") {
            if let Some(top) = recompute_scoped_top_preview(object) {
                object.insert("top".to_string(), top);
            } else if let Some(top) = object.get_mut("top").and_then(Value::as_array_mut) {
                filter_values_for_scope(top, scope);
            }
        }
        if duplicates {
            recompute_duplicate_payload_stats(object);
            object.insert("scope_relation".to_string(), json!("touches"));
            // No per-file denominator or suppression census survives in a
            // project aggregate. Do not pass those repo-wide totals as scoped.
            for key in [
                "duplicated_percent",
                "total_analyzed_lines",
                "generated_duplicated_lines",
                "generated_duplicated_file_count",
                "total_duplicated_lines",
                "total_duplicated_file_count",
                "suppressed_groups",
                "mirror_suppressed_groups",
                "marker_suppressed_groups",
            ] {
                object.remove(key);
            }
        }
        object.remove("by_language");
        for key in [
            "excluded_test_count",
            "excluded_test_files",
            "languages_skipped",
            "scanned_files",
        ] {
            object.remove(key);
        }
        // Parse failures belong to the files that failed, not to every scope
        // sharing a cached aggregate. Keep non-file capability gaps unchanged.
        for key in ["gaps", "parse_errors"] {
            if let Some(rows) = object.get_mut(key).and_then(Value::as_array_mut) {
                filter_values_for_scope(rows, scope);
            }
        }
        // Files the scanner could not read at all (complexity and duplicates
        // name them with a total in `skipped_files_count`) are scoped the same
        // way, and the category is complete in a scope none of them touches.
        if object.contains_key("skipped_files_count") {
            let remaining = object
                .get_mut("skipped_files")
                .and_then(Value::as_array_mut)
                .map(|rows| {
                    filter_values_for_scope(rows, scope);
                    let remaining = rows.len();
                    // Scoped rollups name every skipped file so this filter
                    // sees them all; name at most the usual number here.
                    rows.truncate(super::scanners::source_text::SKIPPED_FILES_LIMIT);
                    remaining
                })
                .unwrap_or(0);
            if remaining == 0 {
                object.remove("skipped_files");
                object.remove("skipped_files_count");
                let parse_errors_remain = object
                    .get("parse_errors")
                    .and_then(Value::as_array)
                    .is_some_and(|rows| !rows.is_empty());
                if !parse_errors_remain {
                    object.insert("complete".to_string(), Value::Bool(true));
                }
            } else {
                object.insert("skipped_files_count".to_string(), json!(remaining));
            }
        }
    }

    payload
}

fn recompute_duplicate_payload_stats(object: &mut serde_json::Map<String, Value>) {
    let values = object
        .get("items")
        .or_else(|| object.get("groups"))
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let (duplicated_lines, duplicated_file_count) = duplicate_line_stats_from_values(&values);
    object.insert("duplicated_lines".to_string(), json!(duplicated_lines));
    object.insert(
        "duplicated_file_count".to_string(),
        json!(duplicated_file_count),
    );
}

fn duplicate_line_stats_from_values(values: &[Value]) -> (u64, usize) {
    let mut by_file = BTreeMap::<String, Vec<(u64, u64)>>::new();
    for value in values {
        let Some(files) = value.get("files").and_then(Value::as_array) else {
            continue;
        };
        for occurrence in files.iter().filter_map(Value::as_str) {
            let Some((file, start, end)) = parse_duplicate_occurrence(occurrence) else {
                continue;
            };
            by_file
                .entry(file.to_string())
                .or_default()
                .push((start, end));
        }
    }
    let file_count = by_file.len();
    let duplicated_lines = by_file
        .values_mut()
        .map(|intervals| merged_duplicate_interval_lines(intervals))
        .sum();
    (duplicated_lines, file_count)
}

fn merged_duplicate_interval_lines(intervals: &mut [(u64, u64)]) -> u64 {
    if intervals.is_empty() {
        return 0;
    }
    intervals.sort_by(|left, right| left.0.cmp(&right.0).then(left.1.cmp(&right.1)));
    let (mut current_start, mut current_end) = intervals[0];
    let mut total = 0;
    for &(start, end) in &intervals[1..] {
        if start <= current_end.saturating_add(1) {
            current_end = current_end.max(end);
        } else {
            total += current_end.saturating_sub(current_start).saturating_add(1);
            current_start = start;
            current_end = end;
        }
    }
    total + current_end.saturating_sub(current_start).saturating_add(1)
}

fn recompute_scoped_top_preview(
    object: &serde_json::Map<String, Value>,
) -> Option<serde_json::Value> {
    let values = object
        .get("items")
        .or_else(|| object.get("groups"))
        .and_then(Value::as_array)?;
    let headline = values
        .iter()
        .filter(|item| item["generated"] != true)
        .cloned()
        .collect::<Vec<_>>();
    Some(scoped_top_preview(&headline))
}

fn scoped_top_preview(values: &[Value]) -> Value {
    Value::Array(
        values
            .iter()
            .take(super::entry_points::TOP_PREVIEW_ITEMS)
            .map(top_preview_value)
            .collect(),
    )
}

fn top_preview_value(value: &Value) -> Value {
    if let Some(files) = value.get("files").and_then(Value::as_array) {
        let mut object = serde_json::Map::new();
        object.insert("files".to_string(), Value::Array(files.clone()));
        if let Some(cost) = value.get("cost").cloned() {
            object.insert("cost".to_string(), cost);
        }
        return Value::Object(object);
    }

    json!({
        "file": value.get("file").and_then(Value::as_str).unwrap_or(""),
        "symbol": value.get("symbol").and_then(Value::as_str).unwrap_or(""),
    })
}

fn filter_values_for_scope(values: &mut Vec<serde_json::Value>, scope: &JobScope) -> usize {
    values.retain_mut(|value| prune_value_for_scope(value, scope));
    values.len()
}

fn prune_value_for_scope(value: &mut serde_json::Value, scope: &JobScope) -> bool {
    let is_cycle = value.get("cycle").is_some();
    if let Some(tests) = value.get_mut("used_by_tests").and_then(Value::as_array_mut) {
        tests.retain(|test| {
            test.as_str()
                .is_some_and(|file| scope.contains_display_path(file))
        });
    }
    if let Some(file) = value.get("file").and_then(|file| file.as_str()) {
        return scope.contains_display_path(file);
    }

    let first_scoped_occurrence = if let Some(files) = value
        .get_mut("files")
        .and_then(|files| files.as_array_mut())
    {
        files.retain(|file| {
            file.as_str()
                .is_some_and(|file| scope.contains_display_path(display_file_from_occurrence(file)))
        });
        if files.is_empty() || (!is_cycle && files.len() < 2) {
            return false;
        }
        files.first().and_then(Value::as_str).map(str::to_string)
    } else {
        None
    };

    if let Some(occurrence) = first_scoped_occurrence {
        update_duplicate_group_sample(value, &occurrence);
    }
    if is_cycle {
        // A cycle may depend on files outside the scope. Its scoped display is
        // a slice of that cycle, never an invented closed cycle in these files.
        let cycle = value["files"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .collect::<Vec<_>>()
            .join(" -> ");
        value["cycle"] = json!(cycle);
        if let Some(edges) = value.get_mut("edges").and_then(Value::as_array_mut) {
            edges.retain(|edge| {
                ["from", "to"].iter().all(|key| {
                    edge[*key]
                        .as_str()
                        .is_some_and(|file| scope.contains_display_path(file))
                })
            });
        }
    }

    true
}

fn update_duplicate_group_sample(value: &mut serde_json::Value, occurrence: &str) {
    let Some((file, start_line, end_line)) = parse_duplicate_occurrence(occurrence) else {
        return;
    };
    let Some(object) = value.as_object_mut() else {
        return;
    };

    if object.contains_key("sample_file") {
        object.insert("sample_file".to_string(), json!(file));
    }
    if object.contains_key("sample_start_line") {
        object.insert("sample_start_line".to_string(), json!(start_line));
    }
    if object.contains_key("sample_end_line") {
        object.insert("sample_end_line".to_string(), json!(end_line));
    }
}

fn parse_duplicate_occurrence(value: &str) -> Option<(&str, u64, u64)> {
    let (file, range) = value.rsplit_once(':')?;
    let (start, end) = range.split_once('-')?;
    if !start.chars().all(|char| char.is_ascii_digit())
        || !end.chars().all(|char| char.is_ascii_digit())
    {
        return None;
    }

    Some((file, start.parse().ok()?, end.parse().ok()?))
}

fn display_file_from_occurrence(value: &str) -> &str {
    let Some((file, range)) = value.rsplit_once(':') else {
        return value;
    };
    let Some((start, end)) = range.split_once('-') else {
        return value;
    };
    if start.chars().all(|char| char.is_ascii_digit())
        && end.chars().all(|char| char.is_ascii_digit())
    {
        file
    } else {
        value
    }
}

#[cfg(test)]
thread_local! {
    static FRESHNESS_RECORD_VISITS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
// Some tests here are `cfg(debug_assertions)` (they lean on debug-only seams);
// helpers only they use read as dead in release-profile test builds.
#[cfg_attr(not(debug_assertions), allow(dead_code))]
mod guard_tests {
    #[test]
    #[ignore = "full-checkout walk-only measurement; requires AFT_INSPECT_WALK_PROFILE_ROOT"]
    fn inspect_checkout_walk_only_profile() {
        use super::super::{inventory::InspectInventory, walk_profile};
        let root = canonicalize_for_snapshot(&PathBuf::from(
            std::env::var_os("AFT_INSPECT_WALK_PROFILE_ROOT").unwrap(),
        ));
        let output = PathBuf::from(std::env::var_os("AFT_INSPECT_WALK_PROFILE_OUTPUT").unwrap());
        let storage = tempfile::tempdir().unwrap();
        let cache = InspectCache::open(storage.path().to_path_buf(), root.clone()).unwrap();
        let full = JobScope::for_project(root.clone());
        let scoped = JobScope::from_roots(root.clone(), vec![root.join("crates/aft/src/inspect")]);
        let manifest_count = super::super::entry_points::collect_entry_point_manifests(&root).len();
        let stat = |files: &[PathBuf]| {
            files
                .iter()
                .map(|path| {
                    let metadata = std::fs::metadata(path).unwrap();
                    (path.clone(), metadata.len(), metadata.modified().unwrap())
                })
                .collect::<Vec<_>>()
        };
        let measure = |reuse: bool| {
            walk_profile::take();
            let started = Instant::now();
            let cpu = walk_profile::cpu_ms();
            // Reproduce the two freshness listings without starting analyzers.
            // Only the extra scoped listing and hash discovery differ below.
            let initial_walk = walk_profile::Walk::start("freshness_listing_proxy");
            let inventory = reuse.then(|| Arc::new(InspectInventory::collect(&root, || true)));
            let initial = inventory.as_ref().map_or_else(
                || scope_files(&root, &full),
                |inventory| inventory.sources.clone(),
            );
            let initial_stats = stat(&initial);
            drop(initial_walk);
            let guard = inventory.as_ref().map(|inventory| inventory.install());
            let selected = scope_files(&root, &scoped);
            let hash = cache
                .contribution_set_hash(InspectCategory::Duplicates)
                .unwrap();
            let final_walk = walk_profile::Walk::start("freshness_listing_proxy");
            let final_files = if reuse {
                InspectInventory::collect(&root, || true).sources
            } else {
                scope_files(&root, &full)
            };
            assert_eq!(initial_stats, stat(&final_files));
            drop(final_walk);
            drop(guard);
            let measurement = serde_json::json!({
                "case": if reuse { "after" } else { "before" },
                "ms": started.elapsed().as_secs_f64() * 1000.0,
                "cpu_ms": walk_profile::cpu_ms() - cpu,
                "walks": walk_profile::take(), "hash": hash,
                "source_files": initial.len(), "scoped_files": selected.len(), "manifests": manifest_count,
            });
            (measurement, initial, selected)
        };
        let (before, before_files, before_scope) = measure(false);
        let (after, after_files, after_scope) = measure(true);
        assert_eq!(before_files, after_files);
        assert_eq!(before_scope, after_scope);
        assert_eq!(before["hash"], after["hash"]);
        let result = serde_json::json!([before, after]);
        eprintln!("inspect walk-only profile {result}");
        std::fs::create_dir_all(output.parent().unwrap()).unwrap();
        std::fs::write(output, serde_json::to_vec_pretty(&result).unwrap()).unwrap();
    }

    #[test]
    fn inspect_scope_files_reuses_the_freshness_inventory() {
        use super::super::inventory::InspectInventory;
        let root = tempfile::tempdir().unwrap();
        let root = canonicalize_for_snapshot(root.path());
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/one.ts"), "export const one = 1;\n").unwrap();
        std::fs::write(root.join("outside.ts"), "export const outside = 1;\n").unwrap();
        let inventory = Arc::new(InspectInventory::collect(&root, || true));
        let _guard = inventory.install();
        SCOPE_FILE_WALKS.with(|walks| walks.set(0));
        assert_eq!(
            scope_files(&root, &JobScope::for_project(root.clone())),
            inventory.sources
        );
        let scope = JobScope::from_roots(root.clone(), vec![root.join("src")]);
        assert_eq!(scope_files(&root, &scope), vec![root.join("src/one.ts")]);
        assert_eq!(SCOPE_FILE_WALKS.with(std::cell::Cell::get), 0);
    }

    /// A tree where every rule of the source walk decides something: files
    /// ignored by `.gitignore`, `.aftignore` and a nested repository's own
    /// `.gitignore`, a nested linked worktree (skipped), a nested independent
    /// repository (walked), skipped folders, non-source files and symlinks;
    /// with `submodule`, also a submodule checkout (walked). Returns the
    /// root, the folder holding it, and the sorted list a walk of it finds.
    fn store_list_fixture(submodule: bool) -> (PathBuf, tempfile::TempDir, Vec<PathBuf>) {
        let dir = tempfile::tempdir().unwrap();
        let fixture =
            crate::git_probe::init_repo_with_nested_worktree_for_test(dir.path(), "store_list");
        let root = canonicalize_for_snapshot(&fixture.root);
        if !submodule {
            std::fs::remove_dir_all(&fixture.submodule).unwrap();
            std::fs::remove_dir_all(root.join(".git/modules")).unwrap();
        }
        for (path, contents) in [
            ("src/lib.ts", "export const lib = 1;\n"),
            (".gitignore", "ignored/\n"),
            (".aftignore", "aftignored/\n"),
            ("ignored/skip.rs", "fn skip() {}\n"),
            ("aftignored/skip.rs", "fn skip() {}\n"),
            ("node_modules/dep/index.js", "module.exports = 1;\n"),
            ("src/.hidden.rs", "fn hidden() {}\n"),
            ("vendor/independent/.gitignore", "generated/\n"),
            ("vendor/independent/generated/out.rs", "fn out() {}\n"),
            ("docs/readme.md", "# docs\n"),
        ] {
            let path = root.join(path);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, contents).unwrap();
        }
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink("src/main.rs", root.join("link.rs")).unwrap();
            std::os::unix::fs::symlink("src", root.join("linked_src")).unwrap();
        }
        let mut walked = crate::callgraph::walk_project_files(&root).collect::<Vec<_>>();
        walked.sort();
        for kept in ["src/main.rs", "src/lib.ts", "vendor/independent/inner.rs"] {
            assert!(walked.contains(&root.join(kept)), "{kept}: {walked:?}");
        }
        assert_eq!(walked.contains(&root.join("submod/code.rs")), submodule);
        for left_out in [
            "ignored/skip.rs",
            "aftignored/skip.rs",
            "node_modules/dep/index.js",
            "src/.hidden.rs",
            "vendor/independent/generated/out.rs",
            "link.rs",
            "linked_src/main.rs",
        ] {
            assert!(!walked.contains(&root.join(left_out)), "{left_out}");
        }
        assert!(
            !walked
                .iter()
                .any(|path| path.starts_with(&fixture.worktree)),
            "the nested linked worktree is walked: {walked:?}"
        );
        (root, dir, walked)
    }
    /// A context whose index-store checkout of `root` is bound and has
    /// published its scan, as the daemon's is once a root is configured.
    fn bound_store_context(root: &Path) -> crate::context::AppContext {
        let ctx = crate::context::AppContext::new(
            Box::new(crate::parser::TreeSitterProvider::new()),
            crate::config::Config {
                project_root: Some(root.to_path_buf()),
                ..crate::config::Config::default()
            },
        );
        crate::data_layer::surfaces::serve_text_only_for_test(&ctx);
        ctx
    }

    /// The status bar's TODO refresh enqueues its job from a maintenance
    /// thread where no request installed a census. With the root's
    /// index-store checkout bound and published, the enqueue lists the root's
    /// files from the store and walks nothing, and the list is the one a walk
    /// of the folder finds.
    #[test]
    fn background_todo_refresh_lists_files_from_the_bound_store_without_walking() {
        let (root, _dir, walked) = store_list_fixture(false);
        let ctx = bound_store_context(&root);
        assert!(ctx
            .data_checkout()
            .is_some_and(|checkout| checkout.view().is_some_and(|view| !view.rescanning())));
        assert!(
            super::super::inventory::current(&root).is_none(),
            "no census is installed on this thread"
        );
        // The job must not wait for the store's TODO records on a worker.
        crate::data_layer::tools::inspect::set_todo_store_wait_for_test(&root, Duration::ZERO);

        SCOPE_FILE_WALKS.with(|walks| walks.set(0));
        crate::callgraph::PROJECT_WALK_ENTRIES.with(|entries| entries.set(0));
        let manager = InspectManager::new();
        manager
            .submit_background(
                tier1_snapshot(&root),
                InspectCategory::Todos,
                JobScope::for_project(root.clone()),
            )
            .expect("the refresh is scheduled");
        let listed = scope_files(&root, &JobScope::for_project(root.clone()));
        assert_eq!(SCOPE_FILE_WALKS.with(std::cell::Cell::get), 0);
        assert_eq!(
            crate::callgraph::PROJECT_WALK_ENTRIES.with(std::cell::Cell::get),
            0,
            "no project walk visited an entry"
        );
        assert_same_files(&root, &listed, &walked);
        let scoped = JobScope::from_roots(root.clone(), vec![root.join("vendor")]);
        assert_eq!(
            scope_files(&root, &scoped),
            vec![root.join("vendor/independent/inner.rs")]
        );
        assert_eq!(SCOPE_FILE_WALKS.with(std::cell::Cell::get), 0);
    }

    /// While the store's list can miss changes (a rescan against the disk is
    /// pending), could differ from the walk's (the repository has
    /// submodules) or there is no store to ask, the files are walked, giving
    /// the walk's list; once the rescan ends the store's list is used again.
    #[test]
    fn scope_files_walks_only_while_the_store_cannot_stand_in() {
        use super::super::inventory::StoreListMiss;
        let (root, _dir, walked) = store_list_fixture(false);
        let unbound = tempfile::tempdir().unwrap();
        let unbound = canonicalize_for_snapshot(unbound.path());
        assert_eq!(
            crate::data_layer::tools::inspect::bound_inventory(&unbound).err(),
            Some(StoreListMiss::NotBound)
        );

        let ctx = bound_store_context(&root);
        let checkout = ctx.data_checkout().expect("bound");
        let project = JobScope::for_project(root.clone());
        checkout.checkout().begin_rescan();
        assert_eq!(
            crate::data_layer::tools::inspect::bound_inventory(&root).err(),
            Some(StoreListMiss::Rescanning)
        );
        SCOPE_FILE_WALKS.with(|walks| walks.set(0));
        assert_eq!(scope_files(&root, &project), walked);
        assert_eq!(SCOPE_FILE_WALKS.with(std::cell::Cell::get), 1);
        checkout.checkout().end_rescan();
        assert_same_files(&root, &scope_files(&root, &project), &walked);
        assert_eq!(SCOPE_FILE_WALKS.with(std::cell::Cell::get), 1);

        // The store's scan skips the submodule checkout the walk lists.
        let (root, _dir, walked) = store_list_fixture(true);
        let _ctx = bound_store_context(&root);
        assert_eq!(
            crate::data_layer::tools::inspect::bound_inventory(&root).err(),
            Some(StoreListMiss::Submodules)
        );
        SCOPE_FILE_WALKS.with(|walks| walks.set(0));
        assert_eq!(
            scope_files(&root, &JobScope::for_project(root.clone())),
            walked
        );
        assert!(walked.contains(&root.join("submod/code.rs")));
        assert_eq!(SCOPE_FILE_WALKS.with(std::cell::Cell::get), 1);
    }

    /// Fails naming the files only one of the two sorted lists holds.
    fn assert_same_files(root: &Path, listed: &[PathBuf], walked: &[PathBuf]) {
        let only = |left: &[PathBuf], right: &[PathBuf]| {
            left.iter()
                .filter(|path| right.binary_search(path).is_err())
                .map(|path| {
                    path.strip_prefix(root)
                        .unwrap_or(path)
                        .display()
                        .to_string()
                })
                .collect::<Vec<_>>()
        };
        assert!(
            listed == walked,
            "only listed: {:?}; only walked: {:?}",
            only(listed, walked),
            only(walked, listed)
        );
    }

    /// This thread's CPU time in milliseconds: the TODO refresh lists files
    /// on the thread that enqueues it, so that thread's clock is the cost.
    fn thread_cpu_ms() -> f64 {
        #[cfg(unix)]
        {
            let mut time = std::mem::MaybeUninit::<libc::timespec>::uninit();
            if unsafe { libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, time.as_mut_ptr()) } == 0
            {
                let time = unsafe { time.assume_init() };
                return time.tv_sec as f64 * 1000.0 + time.tv_nsec as f64 / 1_000_000.0;
            }
        }
        0.0
    }

    /// Opt-in replay of a busy worktree on a copy of a real checkout named by
    /// `AFT_SCOPE_WALK_REPLAY_ROOT` (it edits the files there). It edits
    /// source files one at a time (`AFT_SCOPE_WALK_REPLAY_EDITS`, default
    /// 430), records each edit in the index store as the watcher does, and
    /// after every `AFT_SCOPE_WALK_REPLAY_BATCH` edits (default 10) adds one
    /// file, removes the previous one, and lists the root's files as the TODO
    /// refresh's enqueue does: by walking, as it did before it asked the
    /// store, and through `scope_files`. It prints the walks and this
    /// thread's CPU for each, why the store could not stand in when it could
    /// not, and how long a rescan of the checkout stays pending; it fails if
    /// the two lists ever differ.
    #[test]
    #[ignore = "edits a copy of a real checkout named by AFT_SCOPE_WALK_REPLAY_ROOT"]
    fn scope_files_busy_worktree_replay() {
        let number = |name: &str, default: usize| {
            std::env::var(name)
                .ok()
                .and_then(|value| value.parse().ok())
                .unwrap_or(default)
        };
        let root = canonicalize_for_snapshot(Path::new(
            &std::env::var("AFT_SCOPE_WALK_REPLAY_ROOT").expect("AFT_SCOPE_WALK_REPLAY_ROOT"),
        ));
        let edits = number("AFT_SCOPE_WALK_REPLAY_EDITS", 430);
        let batch = number("AFT_SCOPE_WALK_REPLAY_BATCH", 10).max(1);
        let bind_started = Instant::now();
        let ctx = bound_store_context(&root);
        let bind_ms = bind_started.elapsed().as_millis();
        let checkout = ctx.data_checkout().expect("bound");
        let project = JobScope::for_project(root.clone());
        let targets = crate::callgraph::walk_project_files(&root)
            .filter(|path| {
                path.extension()
                    .is_some_and(|extension| extension == "rs" || extension == "ts")
            })
            .take(64)
            .collect::<Vec<_>>();
        assert!(!targets.is_empty(), "no source file to edit");
        let added_dir = root.join("crates/replay_added");
        std::fs::create_dir_all(&added_dir).unwrap();
        let mut previous_added: Option<PathBuf> = None;
        let (mut listings, mut mismatches) = (0usize, 0usize);
        let (mut walk_cpu, mut walk_ms, mut store_cpu, mut store_ms) = (0.0, 0.0, 0.0, 0.0);
        let mut store_walks = 0usize;
        let mut misses = BTreeMap::<&'static str, usize>::new();
        let mut files = 0usize;
        let replay_started = Instant::now();
        for edit in 0..edits {
            let target = &targets[edit % targets.len()];
            let mut contents = std::fs::read(target).unwrap();
            contents.extend_from_slice(format!("// replay edit {edit}\n").as_bytes());
            std::fs::write(target, contents).unwrap();
            checkout.note_paths(std::slice::from_ref(target));
            if (edit + 1) % batch != 0 {
                continue;
            }
            let added = added_dir.join(format!("added_{edit}.rs"));
            std::fs::write(&added, "fn added() {}\n").unwrap();
            let mut changed = vec![added.clone()];
            if let Some(previous) = previous_added.replace(added) {
                std::fs::remove_file(&previous).unwrap();
                changed.push(previous);
            }
            checkout.note_paths(&changed);

            let (cpu, started) = (thread_cpu_ms(), Instant::now());
            let mut walked = crate::callgraph::walk_project_files(&root)
                .filter(|path| project.contains(path))
                .collect::<Vec<_>>();
            walked.sort();
            walk_cpu += thread_cpu_ms() - cpu;
            walk_ms += started.elapsed().as_secs_f64() * 1000.0;

            if let Err(miss) = crate::data_layer::tools::inspect::bound_inventory(&root) {
                *misses.entry(miss.as_str()).or_default() += 1;
            }
            let walks = SCOPE_FILE_WALKS.with(std::cell::Cell::get);
            let (cpu, started) = (thread_cpu_ms(), Instant::now());
            let listed = scope_files(&root, &project);
            store_cpu += thread_cpu_ms() - cpu;
            store_ms += started.elapsed().as_secs_f64() * 1000.0;
            store_walks += SCOPE_FILE_WALKS.with(std::cell::Cell::get) - walks;

            listings += 1;
            files = walked.len();
            if listed != walked {
                mismatches += 1;
                let only = |left: &[PathBuf], right: &[PathBuf]| {
                    left.iter()
                        .filter(|path| right.binary_search(path).is_err())
                        .take(5)
                        .cloned()
                        .collect::<Vec<_>>()
                };
                eprintln!(
                    "listing {listings} differs: only listed {:?}, only walked {:?}",
                    only(&listed, &walked),
                    only(&walked, &listed)
                );
            }
        }
        let replay_ms = replay_started.elapsed().as_millis();

        // How long a rescan of the checkout against the disk stays pending,
        // the one state of a bound, published checkout that sends the
        // listing back to the walk.
        let rescan_started = Instant::now();
        checkout.rescan();
        let mut observed_pending = false;
        while checkout.view().is_some_and(|view| view.rescanning()) {
            observed_pending = true;
            assert!(rescan_started.elapsed() < Duration::from_secs(300));
            std::thread::sleep(Duration::from_millis(2));
        }
        let rescan_ms = rescan_started.elapsed().as_secs_f64() * 1000.0;

        let per = |total: f64| total / listings.max(1) as f64;
        let report = serde_json::json!({
            "root": root.display().to_string(),
            "files": files,
            "bind_ms": bind_ms,
            "edits": edits,
            "listings": listings,
            "replay_ms": replay_ms,
            "walk": {"walks": listings, "cpu_ms_per_listing": per(walk_cpu), "wall_ms_per_listing": per(walk_ms)},
            "store": {"walks": store_walks, "cpu_ms_per_listing": per(store_cpu), "wall_ms_per_listing": per(store_ms)},
            "store_misses": misses,
            "mismatches": mismatches,
            "rescan": {"observed_pending": observed_pending, "pending_ms": rescan_ms},
        });
        eprintln!("scope files replay {report}");
        assert_eq!(mismatches, 0, "{report}");
    }
    use super::*;

    #[cfg(debug_assertions)]
    #[test]
    fn audit05_debug_cold_scans_use_only_opt_in_delay() {
        let requested = std::cell::Cell::new(None);
        cold_cache_test_delay(true, |duration| requested.set(Some(duration)));
        assert_eq!(
            requested.get(),
            cfg!(feature = "test-timing-hooks").then_some(Duration::from_millis(10))
        );
        cold_cache_test_delay(false, |_| {
            panic!("warm scans never request a fixture delay")
        });
    }

    fn write_ts_project(file_count: usize) -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        for i in 0..file_count {
            std::fs::write(
                root.join(format!("mod{i}.ts")),
                format!("export function f{i}() {{ return {i}; }}\n"),
            )
            .expect("write fixture");
        }
        let canonical_root = std::fs::canonicalize(root).expect("canonical fixture root");
        let project_key = crate::search_index::artifact_cache_key(&canonical_root);
        crate::root_cache::configure_artifact_access(&canonical_root, &project_key, false);
        dir
    }

    fn tier1_snapshot(root: &Path) -> InspectSnapshot {
        use crate::config::Config;
        use crate::parser::SymbolCache;
        use std::sync::RwLock;

        InspectSnapshot::new(
            root.to_path_buf(),
            root.join(".aft-cache/inspect"),
            Arc::new(Config {
                project_root: Some(root.to_path_buf()),
                ..Config::default()
            }),
            Arc::new(RwLock::new(SymbolCache::new())),
        )
    }

    #[test]
    fn tier1_worker_panic_delivers_failed_to_waiter() {
        let dir = write_ts_project(2);
        let root = std::fs::canonicalize(dir.path()).expect("canonical fixture root");
        let snapshot = tier1_snapshot(&root);
        // The property under test is that a worker panic reaches the waiter as
        // `Failed`, not how fast the pool unwinds it; a loaded runner took 359ms
        // to deliver the panic result, so the soft deadline is generous.
        let manager = InspectManager::with_worker(
            Arc::new(|_| panic!("forced Tier-1 worker panic")),
            Duration::from_secs(10),
        );

        let outcome = manager.submit_category(
            snapshot,
            InspectCategory::Metrics,
            JobScope::for_project(root),
        );

        match outcome {
            JobOutcome::Failed { message } => assert!(
                message.contains(
                    "inspect worker panicked before completion: forced Tier-1 worker panic"
                ),
                "unexpected panic terminal: {message}"
            ),
            other => panic!("worker panic must deliver Failed, got {other:?}"),
        }
        assert!(
            manager
                .in_flight
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .is_empty(),
            "panic completion must clear its waiter registration"
        );
    }

    #[test]
    fn ready_worker_result_wins_over_simultaneously_ready_deadline() {
        let dir = write_ts_project(2);
        let root = std::fs::canonicalize(dir.path()).expect("canonical fixture root");
        let snapshot = tier1_snapshot(&root);
        let manager = InspectManager::with_worker(
            Arc::new(|job| {
                InspectResult::success(
                    &job,
                    InspectScanSuccess {
                        scanned_files: job.scope_files.clone(),
                        contributions: Vec::new(),
                        aggregate: json!({"count": 2}),
                    },
                    Duration::ZERO,
                )
            }),
            Duration::from_secs(1),
        );
        let scope = JobScope::for_project(root);
        let key = JobKey::for_category_scope(InspectCategory::Metrics, &scope);
        let cache = manager
            .cache_for_snapshot(&snapshot)
            .expect("open inspect cache");
        let (waiter_tx, waiter_rx) = bounded(1);
        manager
            .enqueue_with_waiter(
                snapshot.clone(),
                InspectCategory::Metrics,
                scope.clone(),
                key.clone(),
                waiter_tx,
                None,
            )
            .expect("enqueue metrics scan");

        let result_deadline = Instant::now() + Duration::from_secs(5);
        while manager.result_rx.is_empty() {
            assert!(
                Instant::now() < result_deadline,
                "worker result did not become ready"
            );
            std::thread::sleep(Duration::from_millis(1));
        }
        let wait_started = Instant::now();
        let outcome = manager.wait_for_outcome(
            key,
            scope,
            cache,
            waiter_rx,
            snapshot,
            wait_started,
            wait_started,
            Duration::ZERO,
        );

        assert!(
            matches!(outcome, JobOutcome::Fresh { .. }),
            "an already-ready terminal result must beat the deadline: {outcome:?}"
        );
    }

    fn write_projection_cache_file(path: &Path, contents: &str) {
        std::fs::create_dir_all(path.parent().expect("fixture file parent"))
            .expect("create fixture parent");
        std::fs::write(path, contents).expect("write fixture file");
    }

    include!("completion_perf_tests.rs");
    include!("tier2_refresh_perf_tests.rs");

    /// Encoding test fixtures such as php_codesniffer's byte-order-mark files
    /// are deliberately not UTF-8. One of them in the project must not make
    /// the call graph (and so dead code) unavailable.
    #[test]
    fn dead_code_projection_survives_an_undecodable_source_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = canonicalize_for_snapshot(dir.path());
        write_projection_cache_file(
            &root.join("src/main.ts"),
            "import { firstTarget } from './target';\nexport function main() { firstTarget(); }\n",
        );
        write_projection_cache_file(
            &root.join("src/target.ts"),
            "export function firstTarget() {}\nexport function neverCalled() {}\n",
        );
        let undecodable = root.join("src/ByteOrderMark.php");
        std::fs::write(&undecodable, b"\xff\xfe<\x00?\x00p\x00h\x00p\x00")
            .expect("write undecodable file");
        let inspect_dir = root.join(".aft-cache").join("inspect");
        let files = crate::callgraph::walk_project_files(&root).collect::<Vec<_>>();
        assert!(
            files.contains(&undecodable),
            "the walk must reach the undecodable file for this test to mean anything"
        );
        let storage = tempfile::tempdir().expect("storage");
        let manager = InspectManager::new();
        let checkout = bind_published_call_graph(&manager, &root, storage.path());

        let job = snapshot_job(&root, &inspect_dir, true);
        // A watcher event for the undecodable file must not fail the refresh.
        checkout.note_paths(std::slice::from_ref(&undecodable));
        let snapshot = manager
            .build_tier2_callgraph_snapshot_with_refresh(&job, false, false, &[undecodable])
            .expect("dead-code projection must be available");
        assert!(
            data_layer_snapshot_exports(&snapshot, &root)
                .iter()
                .any(|export| export.symbol == "neverCalled"),
            "dead-code inputs must include the decodable files"
        );
    }

    /// The dead-code graph a request builds stays in memory for
    /// `DEAD_CODE_GRAPH_IDLE_RELEASE` after the last dead-code request, is
    /// released then, and is built again by the next request. The clock is
    /// passed in, so the test does not wait out the period.
    #[test]
    fn dead_code_graph_is_released_after_ten_idle_minutes() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = canonicalize_for_snapshot(dir.path());
        write_projection_cache_file(
            &root.join("src/main.ts"),
            "import { used } from './target';\nexport function main() { used(); }\n",
        );
        write_projection_cache_file(
            &root.join("src/target.ts"),
            "export function used() {}\nexport function neverCalled() {}\n",
        );
        let inspect_dir = root.join(".aft-cache").join("inspect");
        let storage = tempfile::tempdir().expect("storage");
        let manager = InspectManager::new();
        let checkout = bind_published_call_graph(&manager, &root, storage.path());
        let job = snapshot_job(&root, &inspect_dir, true);
        let build = || {
            manager
                .build_tier2_callgraph_snapshot_with_refresh(&job, false, false, &[])
                .expect("dead-code snapshot")
        };
        let held = || {
            checkout
                .view()
                .expect("published view")
                .dead_code_snapshot_held()
        };
        assert_eq!(
            manager.release_idle_dead_code_graph(Instant::now()),
            DeadCodeGraphRelease::NotHeld
        );

        drop(build());
        assert!(held(), "a request keeps the graph for the next one");
        let requested_at = Instant::now();
        manager.record_dead_code_request(requested_at);
        let almost = requested_at + DEAD_CODE_GRAPH_IDLE_RELEASE - Duration::from_secs(1);
        assert_eq!(
            manager.release_idle_dead_code_graph(almost),
            DeadCodeGraphRelease::KeepFor(Duration::from_secs(1))
        );
        assert!(held(), "the graph stays until ten idle minutes have passed");

        assert_eq!(
            manager.release_idle_dead_code_graph(requested_at + DEAD_CODE_GRAPH_IDLE_RELEASE),
            DeadCodeGraphRelease::Released
        );
        assert!(!held(), "ten idle minutes release the graph");
        assert_eq!(
            manager.release_idle_dead_code_graph(requested_at + DEAD_CODE_GRAPH_IDLE_RELEASE * 2),
            DeadCodeGraphRelease::NotHeld
        );

        assert!(
            data_layer_snapshot_exports(&build(), &root)
                .iter()
                .any(|export| export.symbol == "neverCalled"),
            "the next request builds the graph again"
        );
        assert!(held());
    }

    /// The exports a data-layer dead-code snapshot gives for its files: it
    /// lists none itself, and its rollup reads them from the call graph it
    /// carries.
    fn data_layer_snapshot_exports(
        snapshot: &CallgraphSnapshot,
        root: &Path,
    ) -> Vec<crate::inspect::job::CallgraphExport> {
        assert!(snapshot.exported_symbols.is_empty(), "{snapshot:?}");
        snapshot
            .data_layer
            .as_ref()
            .expect("the snapshot carries the data layer's call graph")
            .exported_symbols_of(root, &snapshot.files)
            .expect("the call graph's exports")
    }

    /// Opens the index-store checkout of `root` under `storage`, waits until
    /// its call graph is published, and lets `manager`'s dead code read it.
    /// Dead code reads only that checkout's call graph.
    fn bind_published_call_graph(
        manager: &InspectManager,
        root: &Path,
        storage: &Path,
    ) -> Arc<crate::data_layer::runtime::DataCheckout> {
        let checkout = crate::data_layer::runtime::DataCheckout::open(root, storage, None)
            .expect("bind the checkout");
        // A first build publishes the text index before the call graph.
        let deadline = Instant::now() + Duration::from_secs(60);
        while !checkout
            .view()
            .is_some_and(|view| view.status().callgraph_published)
        {
            assert!(Instant::now() < deadline, "the checkout never published");
            std::thread::sleep(Duration::from_millis(20));
        }
        manager.read_dead_code_from_data_layer(root, &checkout);
        checkout
    }

    #[test]
    fn scoped_filter_narrows_every_summary_and_preview() {
        let scope = JobScope::from_roots("/repo", vec![PathBuf::from("/repo/src/in")]);
        let payload = json!({
            "count": 2, "total_count": 99, "test_only_count": 479, "generated_count": 12,
            "excluded_test_count": 80, "excluded_test_files": 20,
            "items": [{"file": "src/out/a.ts", "function": "outside", "complexity": 215},
                      {"file": "src/in/b.ts", "function": "inside", "complexity": 12}],
            "worst": {"file": "src/out/a.ts", "function": "outside", "complexity": 215},
            "test_only_items": [{"file": "src/out/a.ts", "symbol": "outside"},
                                {"file": "src/in/b.ts", "symbol": "inside"}],
            "test_only_top": [{"file": "src/out/a.ts", "symbol": "outside"}],
            "generated_items": [{"file": "src/out/a.ts", "symbol": "outside"}],
            "generated_top": [{"file": "src/out/a.ts", "symbol": "outside"}]
        });
        let filtered = filter_payload_for_scope(payload, &scope);
        assert_eq!(filtered["worst"]["function"], "inside");
        assert_eq!(filtered["test_only_count"], 1);
        assert_eq!(filtered["generated_count"], 0);
        assert_eq!(filtered["total_count"], 2);
        assert!(filtered.get("excluded_test_count").is_none());
        assert!(!filtered.to_string().contains("outside"), "{filtered}");

        let duplicates = filter_payload_for_scope(
            json!({
                "count": 2, "total_analyzed_lines": 968281, "duplicated_lines": 100,
                "duplicated_percent": 0.01, "suppressed_groups": 72,
                "items": [{"files": ["src/out/a.ts:1-20", "src/out/b.ts:1-20"]},
                          {"files": ["src/in/b.ts:1-20", "src/out/c.ts:1-20"]}]
            }),
            &scope,
        );
        assert_eq!(
            duplicates["count"], 1,
            "cross-boundary groups must touch the scope"
        );
        assert_eq!(duplicates["scope_relation"], "touches");
        assert!(duplicates.get("total_analyzed_lines").is_none());
        assert!(duplicates.get("suppressed_groups").is_none());
        assert!(!duplicates.to_string().contains("src/out"), "{duplicates}");

        let cycles = filter_payload_for_scope(
            json!({
                "count": 1, "largest": 3, "items": [{
                    "cycle": "src/in/a.ts -> src/out/b.ts -> src/in/c.ts",
                    "files": ["src/in/a.ts", "src/out/b.ts", "src/in/c.ts"],
                    "edges": [{"from": "src/in/a.ts", "to": "src/out/b.ts"},
                              {"from": "src/in/c.ts", "to": "src/in/a.ts"}]
                }]
            }),
            &scope,
        );
        assert_eq!(cycles["count"], 1);
        assert_eq!(cycles["largest"], 2);
        assert_eq!(cycles["scope_relation"], "touches");
        assert!(!cycles.to_string().contains("src/out"), "{cycles}");
    }

    #[test]
    fn scoped_filter_recomputes_top_preview_from_scoped_items() {
        let project_root = PathBuf::from("/project");
        let scope = JobScope::from_roots(project_root.clone(), vec![project_root.join("src/in")]);
        let payload = json!({
            "count": 4,
            "items": [
                { "file": "src/out/a.ts", "symbol": "outside" },
                { "file": "src/in/b.ts", "symbol": "inside_b" },
                { "file": "src/in/c.ts", "symbol": "inside_c" }
            ],
            "top": [
                { "file": "src/out/a.ts", "symbol": "outside" },
                { "file": "src/out/z.ts", "symbol": "outside_z" }
            ],
            "by_language": { "typescript": 4 }
        });

        let filtered = filter_payload_for_scope(payload, &scope);

        assert_eq!(filtered["count"], json!(2));
        assert_eq!(
            filtered["top"],
            json!([
                { "file": "src/in/b.ts", "symbol": "inside_b" },
                { "file": "src/in/c.ts", "symbol": "inside_c" }
            ])
        );
        assert!(filtered["top"]
            .as_array()
            .unwrap()
            .iter()
            .all(|item| item["file"]
                .as_str()
                .is_some_and(|file| file.starts_with("src/in/"))));
    }

    fn artifact_cache_key_for_test(project_root: &std::path::Path) -> String {
        let _git_env = crate::test_env::hermetic_git_env_guard();
        crate::search_index::artifact_cache_key(project_root)
    }

    #[test]
    fn cache_for_paths_rebinds_same_project_key_to_current_root() {
        let _git_env = crate::test_env::hermetic_git_env_guard();
        let dir = tempfile::tempdir().expect("tempdir");
        let source = dir.path().join("source");
        std::fs::create_dir_all(&source).expect("create source repo");
        std::fs::write(
            source.join("package.json"),
            r#"{"name":"inspect-cache-fixture","version":"1.0.0"}"#,
        )
        .expect("write source manifest");
        std::fs::write(source.join("index.ts"), "export const source = 1;\n")
            .expect("write source file");
        let mut init = std::process::Command::new("git");
        assert!(
            crate::test_env::apply_hermetic_git_env(init.current_dir(&source))
                .arg("init")
                .status()
                .expect("git init source repo")
                .success()
        );
        let mut add = std::process::Command::new("git");
        assert!(
            crate::test_env::apply_hermetic_git_env(add.current_dir(&source))
                .args(["add", "."])
                .status()
                .expect("git add source repo")
                .success()
        );
        let mut commit = std::process::Command::new("git");
        assert!(
            crate::test_env::apply_hermetic_git_env(commit.current_dir(&source))
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
                .expect("git commit source repo")
                .success()
        );

        let clone = dir.path().join("clone");
        let mut clone_command = std::process::Command::new("git");
        assert!(crate::test_env::apply_hermetic_git_env(&mut clone_command)
            .args(["clone", "--quiet"])
            .arg(&source)
            .arg(&clone)
            .status()
            .expect("git clone source repo")
            .success());
        std::fs::write(
            clone.join("package.json"),
            r#"{"name":"inspect-cache-fixture","version":"2.0.0"}"#,
        )
        .expect("write clone manifest edit");
        assert_eq!(
            artifact_cache_key_for_test(&source),
            artifact_cache_key_for_test(&clone),
            "clones with the same root commit should share the sqlite project key"
        );

        let source = std::fs::canonicalize(source).expect("canonical source root");
        let clone = std::fs::canonicalize(clone).expect("canonical clone root");
        let manager = InspectManager::new();
        let inspect_dir = dir.path().join("inspect");
        let key = JobKey::for_project_category(InspectCategory::DeadCode);
        let source_cache = manager
            .cache_for_paths(inspect_dir.clone(), source.clone())
            .expect("open source cache");
        let source_hash = source_cache
            .contribution_set_hash(InspectCategory::DeadCode)
            .expect("source contribution hash");
        source_cache
            .store_tier2_aggregate(
                key.clone(),
                &source_hash,
                serde_json::json!({ "count": 7, "items": [] }),
            )
            .expect("store source aggregate");
        assert_eq!(
            source_cache
                .get_aggregated(&key)
                .expect("read source aggregate")
                .and_then(|payload| payload.get("count").and_then(Value::as_u64)),
            Some(7)
        );

        let clone_cache = manager
            .cache_for_paths(inspect_dir, clone.clone())
            .expect("open clone cache");
        assert_eq!(clone_cache.project_root(), clone.as_path());
        assert!(
            clone_cache
                .get_aggregated(&key)
                .expect("read clone aggregate")
                .is_none(),
            "same-key clone with a different manifest must not reuse the source root's cached count"
        );
    }

    #[test]
    fn foreign_writer_failure_backoff_is_scope_local_and_expires() {
        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        let inspect_dir = root.join("inspect");
        let _writer = super::super::cache::tests::foreign_writer(&root, &inspect_dir, false);
        let manager = InspectManager::new();
        for (pass, expected_attempts) in [(0, 20), (1, 0), (2, 20)] {
            if pass == 2 {
                let mut waits = manager.cache_writer_waits.lock().unwrap();
                for (retry_at, _) in waits.values_mut() {
                    *retry_at = Instant::now() - Duration::from_secs(1);
                }
            }
            let clock = crate::fs_lock::retry_clock::Guard::install(Duration::ZERO);
            let error = manager
                .cache_for_paths(inspect_dir.clone(), root.clone())
                .unwrap_err();
            assert!(error.contains("writer_lease_timeout:"), "{error}");
            assert_eq!(
                clock.observations().2,
                expected_attempts,
                "backoff pass {pass}"
            );
        }
        let other = root.join("other");
        std::fs::create_dir(&other).unwrap();
        assert!(
            manager.cache_for_paths(inspect_dir, other).is_ok(),
            "one scope cannot back off another"
        );
    }

    #[test]
    fn foreign_writer_open_does_not_lock_resident_map() {
        use std::sync::atomic::AtomicUsize;
        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        let inspect_dir = root.join("inspect");
        let _writer = super::super::cache::tests::foreign_writer(&root, &inspect_dir, false);
        let manager = InspectManager::new();
        let sleeps = Arc::new(AtomicUsize::new(0));
        std::thread::scope(|scope| {
            let opening = scope.spawn(|| {
                let _observer = crate::fs_lock::observe_retry_sleeps_for_test(sleeps.clone());
                manager.cache_for_paths(inspect_dir.clone(), root.clone())
            });
            let deadline = Instant::now() + Duration::from_secs(5);
            while sleeps.load(Ordering::SeqCst) == 0 && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(1));
            }
            assert!(
                sleeps.load(Ordering::SeqCst) > 0,
                "open reached the contended lease"
            );
            assert!(
                manager.caches.try_lock().is_ok(),
                "cache open held the resident map mutex"
            );
            let start = Instant::now();
            assert_eq!(
                manager.latest_project_todos_count(inspect_dir.clone(), root.clone()),
                None
            );
            assert!(start.elapsed() < Duration::from_millis(500));
            assert!(opening
                .join()
                .unwrap()
                .unwrap_err()
                .contains("writer_lease_timeout:"));
        });
    }

    #[test]
    fn foreign_writer_regular_completion_is_resident_only() {
        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        let inspect_dir = root.join("inspect");
        let _writer = super::super::cache::tests::foreign_writer(&root, &inspect_dir, false);
        let manager = InspectManager::new();
        let job = snapshot_job(&root, &inspect_dir, false);
        let start = Instant::now();
        let failed =
            manager.completion_outcome(InspectResult::failed(&job, "scan failed", Duration::ZERO));
        assert!(matches!(failed, JobOutcome::Failed { message } if message == "scan failed"));
        let succeeded = manager.completion_outcome(InspectResult::success(
            &job,
            InspectScanSuccess {
                aggregate: json!({"count": 7}),
                scanned_files: vec![],
                contributions: vec![],
            },
            Duration::ZERO,
        ));
        assert!(matches!(succeeded, JobOutcome::Fresh { .. }));
        assert!(
            start.elapsed() < Duration::from_millis(500),
            "routing completed work waited for a writer lease"
        );
        assert!(manager.caches.lock().unwrap().is_empty());
    }

    /// The status bar's TODO count comes from a background todos scan. Its
    /// result must be readable through the same lookup the completion drain
    /// uses; before that lookup existed the drain passed no todos value at
    /// all and the bar showed `T?` for as long as nobody ran aft_inspect.
    #[test]
    fn background_todos_scan_is_visible_to_the_status_bar_lookup() {
        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        std::fs::write(
            root.join("lib.ts"),
            "// TODO: first\nexport const a = 1;\n// FIXME: second\n",
        )
        .unwrap();
        let snapshot = tier1_snapshot(&root);
        let inspect_dir = snapshot.inspect_dir.clone();
        // The count reads the index store's records of the root.
        let _store = crate::data_layer::tools::inspect::bind_published_for_test(&root);
        let manager = InspectManager::new();

        assert_eq!(
            manager.latest_project_todos_count(inspect_dir.clone(), root.clone()),
            None,
            "no todos scan has run yet"
        );

        manager
            .submit_background(
                snapshot,
                InspectCategory::Todos,
                JobScope::for_project(root.clone()),
            )
            .expect("schedule todos scan");
        let deadline = Instant::now() + Duration::from_secs(20);
        let mut drained = 0;
        while drained == 0 && Instant::now() < deadline {
            drained = manager.drain_completions();
            if drained == 0 {
                std::thread::sleep(Duration::from_millis(10));
            }
        }
        assert!(drained > 0, "todos scan did not complete");

        let count = manager
            .latest_project_todos_count(inspect_dir, root)
            .expect("todos count after the background scan");
        assert!(
            count >= 1,
            "expected the fixture's TODO markers, got {count}"
        );
    }

    #[test]
    fn category_off_helper_preserves_active_and_retired_categories() {
        let mut config = crate::config::InspectConfig::default();
        let categories = [
            InspectCategory::Diagnostics,
            InspectCategory::Metrics,
            InspectCategory::Todos,
            InspectCategory::DeadCode,
            InspectCategory::UnusedExports,
            InspectCategory::Duplicates,
            InspectCategory::Cycles,
            InspectCategory::Complexity,
            InspectCategory::CircularDeps,
            InspectCategory::OutdatedDeps,
            InspectCategory::Vulnerabilities,
            InspectCategory::TestCoverageGaps,
            InspectCategory::ApiSurface,
        ];
        for category in categories {
            assert!(!inspect_category_is_off(&config, category), "{category}");
        }
        config.enabled = false;
        for category in categories {
            let expected = matches!(
                category,
                InspectCategory::Diagnostics
                    | InspectCategory::Metrics
                    | InspectCategory::Todos
                    | InspectCategory::DeadCode
                    | InspectCategory::UnusedExports
                    | InspectCategory::Duplicates
                    | InspectCategory::Cycles
                    | InspectCategory::Complexity
            );
            assert_eq!(
                inspect_category_is_off(&config, category),
                expected,
                "{category}"
            );
        }
    }

    #[test]
    fn disabled_inspect_category_starts_zero_jobs() {
        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        let mut snapshot = tier1_snapshot(&root);
        let mut config = (*snapshot.config).clone();
        config.inspect.categories.dead_code = false;
        config.inspect.categories.todos = false;
        snapshot.config = Arc::new(config);
        let manager = Arc::new(InspectManager::new());
        let scope = JobScope::for_project(root.clone());
        let initial = manager.next_job_id.load(Ordering::Relaxed);
        assert!(manager
            .submit_background(snapshot.clone(), InspectCategory::Todos, scope.clone())
            .is_err());
        let outcome = manager.tier2_run_with_reuse_blocking(
            snapshot.clone(),
            InspectCategory::DeadCode,
            scope.clone(),
        );
        assert_eq!(outcome.payload().unwrap()["off"], true);
        let outcome = manager.tier2_read_cached_readonly(
            snapshot.clone(),
            InspectCategory::DeadCode,
            scope.clone(),
        );
        assert_eq!(outcome.payload().unwrap()["off"], true);
        assert_eq!(manager.reuse_starts.load(Ordering::SeqCst), 0);
        assert_eq!(manager.next_job_id.load(Ordering::Relaxed), initial);
        assert!(
            !snapshot.inspect_dir.exists(),
            "off categories must not even open their cache"
        );
        // Positive control: the unchanged Metrics scanner actually dispatches.
        // The short soft deadline of `submit_category` can expire on a loaded
        // runner before the scan finishes, so wait on a hang-guard deadline.
        let outcome = manager.submit_category_until(
            snapshot,
            InspectCategory::Metrics,
            scope,
            Instant::now() + Duration::from_secs(30),
        );
        assert!(matches!(outcome, JobOutcome::Fresh { .. }), "{outcome:?}");
        assert!(manager.next_job_id.load(Ordering::Relaxed) > initial);
    }

    fn snapshot_job(root: &Path, inspect_dir: &Path, callgraph_store: bool) -> InspectJob {
        use crate::config::Config;
        use crate::parser::SymbolCache;
        use std::sync::RwLock;

        InspectJob {
            job_id: 1,
            key: JobKey::for_project_category(InspectCategory::DeadCode),
            category: InspectCategory::DeadCode,
            scope_files: Vec::new(),
            project_root: root.to_path_buf(),
            inspect_dir: inspect_dir.to_path_buf(),
            config: Arc::new(Config {
                project_root: Some(root.to_path_buf()),
                indexes: crate::config::IndexesConfig {
                    trigram: false,
                    semantic: false,
                    callgraph: callgraph_store,
                },
                ..Config::default()
            }),
            symbol_cache: Arc::new(RwLock::new(SymbolCache::new())),
            inspect_writer: true,
            callgraph_writer: true,
            callgraph_snapshot: None,
        }
    }

    #[test]
    fn inspect_dead_code_busy_callgraph_reports_pending() {
        let dir = write_ts_project(3);
        let root = std::fs::canonicalize(dir.path()).unwrap();
        let inspect_dir = root.join(".aft-cache").join("inspect");
        let files = crate::callgraph::walk_project_files(&root).collect::<Vec<_>>();
        // The index store's call graph stays building until the hold drops.
        let hold = crate::data_layer::checkout::hold_call_graph_for_test(&root);
        let storage = tempfile::tempdir().expect("storage");
        let checkout = crate::data_layer::runtime::DataCheckout::open(&root, storage.path(), None)
            .expect("bind the checkout");
        let mut job = snapshot_job(&root, &inspect_dir, true);
        job.scope_files = files;
        job.callgraph_writer = false;
        let manager = InspectManager::new();
        manager.read_dead_code_from_data_layer(&root, &checkout);
        let result = manager.tier2_run_with_reuse_job_result(job.clone());
        InspectManager::store_completion(
            &manager.caches,
            &manager.cache_writer_waits,
            &manager.cache_open_gates,
            &result,
        )
        .unwrap();
        assert!(
            manager.caches.lock().unwrap().is_empty(),
            "pending must not create a reusable cache"
        );
        let outcome = manager.completion_outcome(result);
        assert!(matches!(
            builder_attempt_terminal(&outcome),
            BuilderAttemptTerminal::Inconclusive
        ));
        let payload = outcome.payload().expect("pending payload");
        assert_eq!(payload["status"], "pending", "{outcome:?}");
        assert_eq!(payload["complete"], false, "{outcome:?}");
        assert!(payload.get("count").is_none(), "{outcome:?}");
        assert!(payload.get("items").is_none(), "{outcome:?}");
        let snapshot = InspectSnapshot::new(
            root.clone(),
            inspect_dir,
            Arc::clone(&job.config),
            Arc::clone(&job.symbol_cache),
        );
        let cache = manager.cache_for_snapshot(&snapshot).unwrap();
        let cached = filter_outcome_for_scope_with_contributions(
            &manager.data_layer_dead_code,
            Some(&manager),
            JobOutcome::Fresh {
                payload: json!({"count": 0, "items": [], "complete": true}),
            },
            &snapshot,
            InspectCategory::DeadCode,
            cache.as_ref(),
            &JobScope::for_project(root.clone()),
        );
        assert_eq!(
            cached.payload().unwrap()["status"],
            "pending",
            "a cached project-wide zero cannot bypass a busy graph: {cached:?}"
        );
        assert!(cached.payload().unwrap().get("count").is_none());
        drop(hold);
        let scoped = filter_outcome_for_scope_with_contributions(
            &manager.data_layer_dead_code,
            Some(&manager),
            outcome,
            &snapshot,
            InspectCategory::DeadCode,
            cache.as_ref(),
            &JobScope::from_roots(root.clone(), vec![root.join("file0.ts")]),
        );
        assert_eq!(
            scoped.payload().unwrap()["status"],
            "pending",
            "unlock cannot complete unscanned facts: {scoped:?}"
        );
        assert!(scoped.payload().unwrap().get("count").is_none());
        let deadline = Instant::now() + Duration::from_secs(60);
        while !checkout
            .view()
            .is_some_and(|view| view.status().callgraph_published)
        {
            assert!(Instant::now() < deadline, "the call graph never published");
            std::thread::sleep(Duration::from_millis(20));
        }
        let ready = manager.completion_outcome(manager.tier2_run_with_reuse_job_result(job));
        assert_eq!(
            ready.payload().unwrap()["callgraph_available"],
            true,
            "{ready:?}"
        );
    }

    #[test]
    fn explicit_inspect_five_categories_share_one_permit_acquisition() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = std::fs::canonicalize(dir.path()).expect("canonical fixture root");
        let inspect_dir = root.join(".aft-cache").join("inspect");
        let limiter = cold_build_limiter::test_limiter(2);
        let manager = InspectManager::new();
        manager.set_cold_build_limiter(Arc::clone(&limiter));

        let mut slots = Vec::new();
        for (index, category) in InspectCategory::active()
            .iter()
            .copied()
            .filter(|category| category.is_tier2())
            .enumerate()
        {
            let mut job = snapshot_job(&root, &inspect_dir, false);
            job.job_id = index as u64 + 1;
            job.category = category;
            job.key = JobKey::for_project_category(category);
            slots.push(
                manager
                    .acquire_interactive_tier2_permit_slot(&job)
                    .expect("explicit category acquires the request permit"),
            );
        }

        assert_eq!(
            slots.len(),
            5,
            "the active inspect has five Tier-2 categories"
        );
        assert_eq!(
            manager
                .interactive_tier2_acquisitions
                .load(Ordering::SeqCst),
            1,
            "one explicit inspect calls the limiter once"
        );
        assert_eq!(
            limiter.admission_events().len(),
            1,
            "the one limiter call acquires one process-wide slot"
        );
        let holder = limiter.census().holders.pop().expect("shared root holder");
        assert_eq!(holder.root, root.display().to_string());
        assert_eq!(holder.sharers, 5);

        slots[0]
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        assert_eq!(
            limiter.census().holders[0].sharers,
            4,
            "cancelling one category leaves the other category permits live"
        );
        drop(slots);
        assert!(limiter.census().holders.is_empty());
    }

    #[test]
    fn blocking_inspect_overtakes_queued_maintenance_after_active_seed_releases() {
        use crate::config::Config;
        use crate::parser::SymbolCache;
        use std::sync::RwLock;

        let dir = tempfile::tempdir().expect("tempdir");
        let root = std::fs::canonicalize(dir.path()).expect("canonical fixture root");
        std::fs::create_dir_all(root.join("src")).expect("create source directory");
        std::fs::write(
            root.join("src/main.ts"),
            "export function plantedDead() { return 1; }\n",
        )
        .expect("write source fixture");
        let project_key = crate::search_index::artifact_cache_key(&root);
        crate::root_cache::configure_artifact_access(&root, &project_key, false);
        let inspect_dir = root.join(".aft-cache").join("inspect");
        let snapshot = InspectSnapshot::new_with_capabilities(
            root.clone(),
            inspect_dir,
            Arc::new(Config {
                project_root: Some(root.clone()),
                indexes: crate::config::IndexesConfig {
                    trigram: false,
                    semantic: false,
                    callgraph: true,
                },
                ..Config::default()
            }),
            Arc::new(RwLock::new(SymbolCache::new())),
            true,
            true,
        );

        let limiter = cold_build_limiter::test_limiter(1);
        let active_request = cold_build_limiter::ColdBuildAdmissionRequest::new(
            "active-semantic-seed",
            cold_build_limiter::ColdBuildAdmissionClass::Maintenance,
        );
        let active =
            cold_build_limiter::try_acquire_classified_with_limiter(&limiter, &active_request)
                .expect("active semantic seed holds the only slot");
        let semantic_seed_active = Arc::new(AtomicBool::new(true));
        let manager = Arc::new(InspectManager::with_root_work_gates(
            Arc::new(AtomicBool::new(true)),
            Arc::clone(&semantic_seed_active),
        ));
        manager.set_cold_build_limiter(Arc::clone(&limiter));
        // Dead code reads its call graph from the root's index-store checkout.
        let storage = tempfile::tempdir().expect("storage");
        let _checkout = bind_published_call_graph(&manager, &root, storage.path());

        let inspect_manager = Arc::clone(&manager);
        let scope = JobScope::for_project(root);
        let (outcome_tx, outcome_rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let outcome = inspect_manager.tier2_run_with_reuse_blocking_fresh(
                snapshot,
                InspectCategory::DeadCode,
                scope,
            );
            outcome_tx.send(outcome).expect("send inspect outcome");
        });

        // The inspect thread opens its cache and checks for a reusable result
        // before it reaches the seed gate; a loaded Windows runner needed more
        // than 3 s for that. The gate itself is what the test checks.
        let deadline = Instant::now() + Duration::from_secs(30);
        while manager.tier2_builder_state(InspectCategory::DeadCode)
            != InspectBuilderState::GatedBySemanticSeed
        {
            assert!(
                Instant::now() < deadline,
                "inspect must queue while the active seed owns the slot"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(
            outcome_rx.try_recv().is_err(),
            "in-flight work is not preempted"
        );

        let maintenance_limiter = Arc::clone(&limiter);
        let maintenance = std::thread::spawn(move || {
            cold_build_limiter::acquire_blocking_while_with_limiter(
                &maintenance_limiter,
                "queued background refresh",
                || true,
            )
            .expect("background refresh eventually resumes")
        });
        std::thread::sleep(Duration::from_millis(150));
        semantic_seed_active.store(false, Ordering::SeqCst);
        drop(active);

        let outcome = outcome_rx
            .recv_timeout(Duration::from_secs(10))
            .expect("blocking inspect completes after the active seed releases");
        let payload = outcome.payload().expect("blocking inspect is fresh");
        assert_eq!(
            payload.get("callgraph_available").and_then(Value::as_bool),
            Some(true)
        );
        drop(maintenance.join().expect("background waiter joins"));

        let events = limiter.admission_events();
        assert_eq!(
            events[0].class,
            cold_build_limiter::ColdBuildAdmissionClass::Maintenance
        );
        assert_eq!(
            events[1].class,
            cold_build_limiter::ColdBuildAdmissionClass::InspectTriggered,
            "explicit inspect takes the first released slot"
        );
        assert_eq!(
            events[2].class,
            cold_build_limiter::ColdBuildAdmissionClass::Maintenance
        );
    }

    #[test]
    fn post_eviction_rebind_serves_unchanged_tier2_aggregate_without_cold_slot() {
        use crate::config::Config;
        use crate::parser::SymbolCache;
        use std::sync::RwLock;

        let dir = tempfile::tempdir().expect("tempdir");
        let root = std::fs::canonicalize(dir.path()).expect("canonical fixture root");
        std::fs::create_dir_all(root.join("src")).expect("create source directory");
        std::fs::write(
            root.join("src/main.ts"),
            "export function plantedDead() { return 1; }\n",
        )
        .expect("write source fixture");
        let project_key = crate::search_index::artifact_cache_key(&root);
        crate::root_cache::configure_artifact_access(&root, &project_key, false);
        let inspect_dir = root.join(".aft-cache").join("inspect");
        let snapshot = InspectSnapshot::new_with_capabilities(
            root.clone(),
            inspect_dir,
            Arc::new(Config {
                project_root: Some(root.clone()),
                indexes: crate::config::IndexesConfig {
                    trigram: false,
                    semantic: false,
                    callgraph: true,
                },
                ..Config::default()
            }),
            Arc::new(RwLock::new(SymbolCache::new())),
            true,
            true,
        );
        let limiter = cold_build_limiter::test_limiter(1);
        let manager = Arc::new(InspectManager::new());
        manager.set_cold_build_limiter(Arc::clone(&limiter));
        // Dead code reads its call graph from the root's index-store checkout.
        let storage = tempfile::tempdir().expect("storage");
        let _checkout = bind_published_call_graph(&manager, &root, storage.path());
        let first = manager.tier2_run_with_reuse_blocking_fresh(
            snapshot.clone(),
            InspectCategory::DeadCode,
            JobScope::for_project(root.clone()),
        );
        assert!(
            first.payload().is_some(),
            "initial scan persists a fresh aggregate"
        );
        assert!(!manager.tier2_any_in_flight());
        manager.evict_idle_caches();

        let maintenance_request = cold_build_limiter::ColdBuildAdmissionRequest::new(
            "post-eviction-search-verify",
            cold_build_limiter::ColdBuildAdmissionClass::Maintenance,
        );
        let maintenance =
            cold_build_limiter::try_acquire_classified_with_limiter(&limiter, &maintenance_request)
                .expect("background verification owns the only cold slot");
        let events_before = limiter.admission_events().len();
        let rebound_manager = Arc::clone(&manager);
        let (outcome_tx, outcome_rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let outcome = rebound_manager.tier2_run_with_reuse_blocking_fresh(
                snapshot,
                InspectCategory::DeadCode,
                JobScope::for_project(root),
            );
            outcome_tx.send(outcome).expect("send rebound outcome");
        });

        // Cache reopening and source checks can be slow on a busy runner. Holding
        // the only cold slot until the reply, not a short deadline, proves bypass.
        let rebound = outcome_rx
            .recv_timeout(Duration::from_secs(60))
            .unwrap_or_else(|error| {
                panic!(
                    "unchanged persisted aggregate bypasses the occupied cold-build queue: \
                     {error:?}; cold limiter: {:?}",
                    limiter.census()
                )
            });
        assert!(rebound.payload().is_some());
        assert_eq!(
            limiter.admission_events().len(),
            events_before,
            "quick reuse must not request an interactive cold-build permit"
        );
        assert!(
            limiter.try_acquire().is_none(),
            "maintenance must still own the only cold slot when quick reuse replies"
        );
        drop(maintenance);
        assert_eq!(
            manager.tier2_builder_state(InspectCategory::DeadCode),
            InspectBuilderState::Absent,
            "quick reuse must clear the builder registry on the way out"
        );
        assert!(!manager.tier2_any_in_flight());
    }

    #[test]
    fn unfinished_tier2_flight_guard_publishes_failure() {
        let manager = InspectManager::new();
        let key = JobKey::for_project_category(InspectCategory::Duplicates);
        let (tx, rx) = bounded(1);
        assert!(manager.register_tier2_reuse_waiter(&key, tx).unwrap());
        drop(manager.tier2_flight_exit_guard(key));
        let outcome = rx.try_recv().expect("guard published its terminal outcome");
        assert!(
            matches!(
                outcome,
                JobOutcome::Failed { ref message }
                    if message == "tier2 reuse worker exited without publishing a result"
            ),
            "{outcome:?}"
        );
        assert!(!manager.tier2_any_in_flight());
        assert_eq!(manager.reuse_completion_count(), 1);
    }

    #[test]
    fn completed_tier2_flight_guard_preserves_replacement_flight() {
        let manager = InspectManager::new();
        let key = JobKey::for_project_category(InspectCategory::Duplicates);
        let (first_tx, first_rx) = bounded(1);
        assert!(manager.register_tier2_reuse_waiter(&key, first_tx).unwrap());
        let first_guard = manager.tier2_flight_exit_guard(key.clone());
        manager.finish_tier2_flight(
            &key,
            JobOutcome::Fresh {
                payload: serde_json::json!({"flight": "first"}),
            },
        );
        assert!(matches!(first_rx.try_recv(), Ok(JobOutcome::Fresh { .. })));

        let (replacement_tx, replacement_rx) = bounded(1);
        assert!(manager
            .register_tier2_reuse_waiter(&key, replacement_tx)
            .unwrap());
        let replacement_guard = manager.tier2_flight_exit_guard(key.clone());
        // Completion wakes callers before the old worker's exit guard drops.
        // A caller may register a new flight under the same key in that gap.
        drop(first_guard);
        assert!(
            manager.tier2_any_in_flight(),
            "the completed flight's guard removed its replacement"
        );
        assert_eq!(
            manager.tier2_builder_state(InspectCategory::Duplicates),
            InspectBuilderState::Building
        );
        assert!(matches!(
            replacement_rx.try_recv(),
            Err(crossbeam_channel::TryRecvError::Empty)
        ));
        assert_eq!(manager.reuse_completion_count(), 1);

        manager.finish_tier2_flight(
            &key,
            JobOutcome::Fresh {
                payload: serde_json::json!({"flight": "replacement"}),
            },
        );
        drop(replacement_guard);
        let outcome = replacement_rx.try_recv().expect("replacement published");
        assert_eq!(
            outcome.payload(),
            Some(&serde_json::json!({"flight": "replacement"}))
        );
        assert!(!manager.tier2_any_in_flight());
        assert_eq!(manager.reuse_completion_count(), 2);
    }

    #[test]
    fn tier2_reuse_panic_clears_builder_registration() {
        use crate::config::Config;
        use crate::parser::SymbolCache;
        use std::sync::RwLock;

        let dir = tempfile::tempdir().expect("tempdir");
        let root = std::fs::canonicalize(dir.path()).expect("canonical fixture root");
        std::fs::create_dir_all(root.join("src")).expect("create source directory");
        std::fs::write(
            root.join("src/dup.ts"),
            "export function planted() { return 1; }\n",
        )
        .expect("write source fixture");
        let project_key = crate::search_index::artifact_cache_key(&root);
        crate::root_cache::configure_artifact_access(&root, &project_key, false);
        let inspect_dir = root.join(".aft-cache").join("inspect");
        let snapshot = InspectSnapshot::new_with_capabilities(
            root.clone(),
            inspect_dir,
            Arc::new(Config {
                project_root: Some(root.clone()),
                ..Config::default()
            }),
            Arc::new(RwLock::new(SymbolCache::new())),
            true,
            true,
        );

        let previous_root = std::env::var_os("AFT_TEST_TIER2_REUSE_PANIC_ROOT");
        let previous_category = std::env::var_os("AFT_TEST_TIER2_REUSE_PANIC_CATEGORY");
        unsafe {
            std::env::set_var("AFT_TEST_TIER2_REUSE_PANIC_ROOT", &root);
            std::env::set_var("AFT_TEST_TIER2_REUSE_PANIC_CATEGORY", "duplicates");
        }
        struct RestorePanicEnv {
            root: Option<std::ffi::OsString>,
            category: Option<std::ffi::OsString>,
        }
        impl Drop for RestorePanicEnv {
            fn drop(&mut self) {
                unsafe {
                    match self.root.take() {
                        Some(value) => std::env::set_var("AFT_TEST_TIER2_REUSE_PANIC_ROOT", value),
                        None => std::env::remove_var("AFT_TEST_TIER2_REUSE_PANIC_ROOT"),
                    }
                    match self.category.take() {
                        Some(value) => {
                            std::env::set_var("AFT_TEST_TIER2_REUSE_PANIC_CATEGORY", value)
                        }
                        None => std::env::remove_var("AFT_TEST_TIER2_REUSE_PANIC_CATEGORY"),
                    }
                }
            }
        }
        let _restore = RestorePanicEnv {
            root: previous_root,
            category: previous_category,
        };

        let manager = Arc::new(InspectManager::new());
        let _ = manager.tier2_run_with_reuse_blocking(
            snapshot,
            InspectCategory::Duplicates,
            JobScope::for_project(root.clone()),
        );

        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            if !manager.tier2_any_in_flight()
                && manager.tier2_builder_state(InspectCategory::Duplicates)
                    == InspectBuilderState::Absent
            {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "a reuse worker that panics before the completion router must still clear the builder registry"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn generated_unused_exports_fixture() -> (tempfile::TempDir, PathBuf, Vec<PathBuf>) {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().to_path_buf();
        let files = [
            (
                "src/hand.ts",
                "export function handUnused() {}
",
            ),
            (
                "gen/schema_pb.ts",
                "export function generatedPathUnused() {}
",
            ),
            (
                "src/banner.ts",
                "// Code generated by fixture. DO NOT EDIT.
export function bannerUnused() {}
",
            ),
        ];
        let paths = files
            .iter()
            .map(|(relative, contents)| {
                let path = root.join(relative);
                if let Some(parent) = path.parent() {
                    std::fs::create_dir_all(parent).expect("create parent");
                }
                std::fs::write(&path, contents).expect("write fixture file");
                std::fs::canonicalize(path).expect("canonical fixture path")
            })
            .collect::<Vec<_>>();
        (
            dir,
            std::fs::canonicalize(root).expect("canonical root"),
            paths,
        )
    }

    fn unused_exports_job(root: &Path, scope_files: Vec<PathBuf>) -> InspectJob {
        use crate::config::Config;
        use crate::parser::SymbolCache;
        use std::sync::RwLock;

        InspectJob {
            job_id: 1,
            key: JobKey::for_project_category(InspectCategory::UnusedExports),
            category: InspectCategory::UnusedExports,
            scope_files,
            project_root: root.to_path_buf(),
            inspect_dir: root.join(".aft-cache").join("inspect"),
            config: Arc::new(Config {
                project_root: Some(root.to_path_buf()),
                ..Config::default()
            }),
            symbol_cache: Arc::new(RwLock::new(SymbolCache::new())),
            inspect_writer: true,
            callgraph_writer: true,
            callgraph_snapshot: None,
        }
    }

    #[test]
    fn unused_exports_oxc_cached_rollup_preserves_generated_exclusions() {
        let (_dir, root, paths) = generated_unused_exports_fixture();
        let job = unused_exports_job(&root, paths.clone());
        let entry_points = crate::inspect::entry_points::resolve_entry_points(&root);
        let oxc_result = crate::inspect::oxc_engine::analyze_files(
            &root,
            &paths,
            AnalyzeOptions {
                entry_points: Vec::new(),
                public_api_files: entry_points.public_api_files(),
                executable_root_exports: entry_points.executable_root_exports(),
                force_reparse_files: Vec::new(),
                entry_reachability: false,
            },
        )
        .expect("oxc analyze succeeds");
        let fresh = crate::inspect::scanners::unused_exports::run_unused_exports_scan_with_oxc(
            &job,
            Some(&oxc_result),
        )
        .outcome
        .expect("fresh scan succeeds");

        let rolled_up = roll_up_unused_exports_contributions(
            &job,
            &fresh.contributions,
            Some(MAX_DRILL_DOWN_ITEMS),
        );

        assert_eq!(
            rolled_up, fresh.aggregate,
            "cached rollup must match fresh scan"
        );
        assert_eq!(rolled_up["count"], 1, "{rolled_up:#}");
        assert_eq!(rolled_up["generated_count"], 0, "{rolled_up:#}");
        assert_eq!(rolled_up["total_count"], 1, "{rolled_up:#}");
    }

    #[test]
    fn unused_exports_oversized_js_ts_sources_are_unopened_and_cached_as_graph_wide_gaps() {
        use std::fs;

        use super::super::scanners::unused_exports::{
            run_unused_exports_scan, run_unused_exports_scan_with_oxc,
        };
        for extension in ["js", "ts"] {
            let dir = tempfile::tempdir().unwrap();
            let root = dir.path();
            let large = root.join(format!("importer.{extension}"));
            let small = root.join(format!("target.{extension}"));
            fs::write(
                &small,
                super::super::scanners::source_text::handwritten_graph_source_for_test(
                    1_572_864,
                    "export function used() {}\n",
                ),
            )
            .unwrap();
            fs::write(&large, super::super::scanners::source_text::handwritten_graph_source_for_test(5_242_880, &format!("import {{ used }} from './target.{extension}'; used();\nexport function inLarge() {{}}\n"))).unwrap();
            let paths = vec![large.clone(), small];
            let job = unused_exports_job(root, paths.clone());
            let probe = crate::project_content::Probe::new(root);
            let mut cache = super::super::oxc_engine::OxcFactsCache::new();
            for oxc in [false, true] {
                let success = if oxc {
                    let result = super::super::oxc_engine::analyze_files_with_cache(
                        root,
                        &paths,
                        AnalyzeOptions::default(),
                        &mut cache,
                    )
                    .unwrap();
                    assert!(!result.facts.iter().any(|facts| facts.path == large));
                    assert!(result
                        .files
                        .iter()
                        .flat_map(|file| &file.exports)
                        .all(|export| export.verdict == LivenessVerdict::Uncertain));
                    run_unused_exports_scan_with_oxc(&job, Some(&result))
                } else {
                    run_unused_exports_scan(&job)
                }
                .outcome
                .unwrap();
                let aggregate = &success.aggregate;
                assert_eq!(aggregate["count"], 0, "{aggregate}");
                assert_eq!(aggregate["complete"], false);
                assert_eq!(aggregate["skipped_files_count"], 1);
                assert_eq!(
                    aggregate["skipped_files"],
                    json!([{"file": format!("importer.{extension}"), "reason": "over the 4 MiB size limit"}])
                );
                let cached =
                    roll_up_unused_exports_contributions(&job, &success.contributions, None);
                for key in [
                    "count",
                    "items",
                    "skipped_files",
                    "skipped_files_count",
                    "complete",
                    "uncertain_count",
                ] {
                    assert_eq!(cached[key], aggregate[key], "{key}, oxc={oxc}");
                }
            }
            assert_eq!(
                super::super::scanners::source_text::source_read_count_for_debug(&large),
                0
            );
            assert!(
                crate::file_kind::read_probe::bytes_read(&large)
                    <= crate::file_kind::HEAD_BYTES * 4
            );
            drop(probe);
            fs::write(&large, "export function nowSmall() {}\n").unwrap();
            let result = super::super::oxc_engine::analyze_files_with_cache(
                root,
                &paths,
                AnalyzeOptions::default(),
                &mut cache,
            )
            .unwrap();
            assert!(result.errors.is_empty());
            let repaired = run_unused_exports_scan_with_oxc(&job, Some(&result))
                .outcome
                .unwrap();
            assert_eq!(repaired.aggregate["count"], 2);
            assert_eq!(repaired.aggregate["complete"], true);
            assert!(repaired.aggregate.get("skipped_files").is_none());
        }
    }

    #[test]
    fn unused_exports_generated_and_minified_inputs_are_unopened_deliberate_exclusions() {
        use super::super::scanners::unused_exports::{
            run_unused_exports_scan, run_unused_exports_scan_with_oxc,
        };
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let generated = root.join("api.generated.ts");
        let minified = root.join("bundle.min.js");
        let target = root.join("target.ts");
        std::fs::write(
            &generated,
            "import { used } from './target'; used(); export function generatedUnused() {}\n",
        )
        .unwrap();
        std::fs::write(&minified, "export function minifiedUnused() {}\n").unwrap();
        std::fs::write(&target, "export function used() {}\n").unwrap();
        let paths = vec![generated.clone(), minified.clone(), target];
        let job = unused_exports_job(root, paths.clone());
        let probe = crate::project_content::Probe::new(root);
        for oxc in [false, true] {
            let success = if oxc {
                let result = super::super::oxc_engine::analyze_files(
                    root,
                    &paths,
                    AnalyzeOptions::default(),
                )
                .unwrap();
                run_unused_exports_scan_with_oxc(&job, Some(&result))
            } else {
                run_unused_exports_scan(&job)
            }
            .outcome
            .unwrap();
            assert_eq!(success.aggregate["count"], 1, "{}", success.aggregate);
            assert_eq!(success.aggregate["generated_count"], 0);
            assert_eq!(success.aggregate["complete"], true);
            assert!(success.aggregate.get("skipped_files").is_none());
            let cached = roll_up_unused_exports_contributions(&job, &success.contributions, None);
            assert_eq!(cached["count"], 1);
            assert_eq!(cached["complete"], true);
        }
        assert!(!probe.paths().contains(&generated));
        assert!(!probe.paths().contains(&minified));
    }

    /// Capture uncapped scanner findings and importer evidence for a frozen
    /// real-repository corpus. Storage and output are supplied by the caller.
    #[test]
    #[ignore = "real-repository graph admission measurement; requires AFT_GRAPH_MEASURE_SPEC and AFT_GRAPH_MEASURE_OUTPUT"]
    fn measure_graph_source_admission_on_real_repositories() {
        let specs: Value = serde_json::from_str(
            &std::env::var("AFT_GRAPH_MEASURE_SPEC").expect("measurement spec"),
        )
        .unwrap();
        let output =
            PathBuf::from(std::env::var("AFT_GRAPH_MEASURE_OUTPUT").expect("measurement output"));
        let storage = output.with_extension("storage");
        let mut results = Vec::new();
        for spec in specs.as_array().unwrap() {
            let name = spec["name"].as_str().unwrap();
            let root = std::fs::canonicalize(spec["root"].as_str().unwrap()).unwrap();
            let prefix = spec["prefix"].as_str().unwrap_or("");
            let files = crate::callgraph::walk_project_files(&root)
                .filter(|path| {
                    path.strip_prefix(&root)
                        .is_ok_and(|path| path.starts_with(prefix))
                        && crate::parser::detect_language(path).is_some_and(is_js_ts_language)
                })
                .collect::<Vec<_>>();
            assert!(!files.is_empty(), "empty corpus: {name}");
            let excluded = files
                .iter()
                .filter_map(|path| {
                    let kind = crate::file_kind::classify(&root, path, None).content;
                    matches!(
                        kind,
                        crate::file_kind::ContentKind::Generated
                            | crate::file_kind::ContentKind::Minified
                    )
                    .then(|| json!({"file": relative_cache_key(&root, path), "kind": kind.label()}))
                })
                .collect::<Vec<_>>();
            if name != "aft-packages" {
                assert!(!excluded.is_empty(), "no generated/minified inputs: {name}");
            }
            let project_key = crate::search_index::artifact_cache_key(&root);
            crate::root_cache::configure_artifact_access(&root, &project_key, false);
            let snapshot =
                Arc::new(crate::callgraph_store::dead_code_projection::snapshot_for_test(&root));
            let mut job = snapshot_job(&root, &storage.join("inspect").join(&project_key), true);
            job.scope_files = files.clone();
            job.callgraph_snapshot = Some(snapshot.clone());
            let entries = super::super::entry_points::resolve_entry_points(&root);
            let options = |entry_reachability| AnalyzeOptions {
                entry_points: snapshot.entry_points.iter().cloned().collect(),
                public_api_files: entries.public_api_files(),
                executable_root_exports: entries.executable_root_exports(),
                force_reparse_files: Vec::new(),
                entry_reachability,
            };
            let dead_oxc =
                super::super::oxc_engine::analyze_files(&root, &files, options(true)).unwrap();
            let dead = super::super::scanners::dead_code::run_dead_code_scan_with_oxc(
                &job,
                Some(&dead_oxc),
            )
            .outcome
            .unwrap()
            .aggregate;
            job.category = InspectCategory::UnusedExports;
            job.key = JobKey::for_project_category(job.category);
            let unused_oxc =
                super::super::oxc_engine::analyze_files(&root, &files, options(false)).unwrap();
            let unused = super::super::scanners::unused_exports::run_unused_exports_scan_with_oxc(
                &job,
                Some(&unused_oxc),
            )
            .outcome
            .unwrap()
            .aggregate;
            let calls = snapshot.outbound_calls.iter().map(|call| json!({
                "caller_file": relative_cache_key(&root, &call.caller_file), "caller_symbol": call.caller_symbol,
                "target": call.target, "line": call.line,
            })).collect::<Vec<_>>();
            println!(
                "GRAPH_MEASURE {name}: files={} excluded={} dead={} unused={}",
                files.len(),
                excluded.len(),
                dead["count"],
                unused["count"]
            );
            results.push(json!({"name": name, "root": root, "files": files.len(), "excluded": excluded,
                "dead_code": dead, "unused_exports": unused, "oxc": unused_oxc, "snapshot_calls": calls}));
        }
        std::fs::create_dir_all(output.parent().unwrap()).unwrap();
        std::fs::write(output, serde_json::to_vec(&results).unwrap()).unwrap();
    }

    #[test]
    fn unused_exports_cached_generated_state_avoids_reprobe_with_legacy_fallback() {
        let (_dir, root, paths) = generated_unused_exports_fixture();
        let job = unused_exports_job(&root, paths.clone());
        let entry_points = crate::inspect::entry_points::resolve_entry_points(&root);
        let oxc_result = crate::inspect::oxc_engine::analyze_files(
            &root,
            &paths,
            AnalyzeOptions {
                entry_points: Vec::new(),
                public_api_files: entry_points.public_api_files(),
                executable_root_exports: entry_points.executable_root_exports(),
                force_reparse_files: Vec::new(),
                entry_reachability: false,
            },
        )
        .expect("oxc analyze succeeds");
        let fresh = crate::inspect::scanners::unused_exports::run_unused_exports_scan_with_oxc(
            &job,
            Some(&oxc_result),
        )
        .outcome
        .expect("fresh scan succeeds");
        let mut contributions = fresh.contributions;
        let handwritten = contributions
            .iter_mut()
            .find(|contribution| contribution.file_path.ends_with("src/hand.ts"))
            .expect("handwritten contribution");
        handwritten.contribution["generated"] = json!(false);

        crate::inspect::generated::reset_file_probe_count_for_debug(&root);
        let explicit_cached =
            roll_up_unused_exports_contributions(&job, &contributions, Some(MAX_DRILL_DOWN_ITEMS));
        assert_eq!(explicit_cached, fresh.aggregate);
        assert_eq!(
            crate::inspect::generated::file_probe_count_for_debug(&root),
            0,
            "an explicit cached generated=false must not probe the file again"
        );

        let generated_banner = contributions
            .iter_mut()
            .find(|contribution| contribution.file_path.ends_with("src/banner.ts"))
            .expect("generated banner contribution");
        generated_banner
            .contribution
            .as_object_mut()
            .expect("contribution object")
            .remove("generated");
        crate::inspect::generated::reset_file_probe_count_for_debug(&root);
        let legacy_cached =
            roll_up_unused_exports_contributions(&job, &contributions, Some(MAX_DRILL_DOWN_ITEMS));
        assert_eq!(legacy_cached, fresh.aggregate);
        assert_eq!(
            crate::inspect::generated::file_probe_count_for_debug(&root),
            0,
            "deliberate exclusions must not be reprobed when a legacy generated hint is absent"
        );
        contributions
            .iter_mut()
            .find(|contribution| contribution.file_path.ends_with("src/hand.ts"))
            .unwrap()
            .contribution
            .as_object_mut()
            .unwrap()
            .remove("generated");
        crate::inspect::generated::reset_file_probe_count_for_debug(&root);
        let legacy_handwritten =
            roll_up_unused_exports_contributions(&job, &contributions, Some(MAX_DRILL_DOWN_ITEMS));
        assert_eq!(legacy_handwritten, fresh.aggregate);
        assert_eq!(
            crate::inspect::generated::file_probe_count_for_debug(&root),
            1,
            "legacy handwritten contributions still recover a missing classification hint"
        );
    }

    #[test]
    fn callgraph_snapshot_reports_unavailable_when_store_disabled() {
        let dir = write_ts_project(3);
        let root = std::fs::canonicalize(dir.path()).expect("canonical root");
        let inspect_dir = root.join(".aft-cache").join("inspect");

        let snapshot =
            build_tier2_callgraph_snapshot(&snapshot_job(&root, &inspect_dir, false), false);

        assert!(
            snapshot.is_none(),
            "dead_code must not rebuild the legacy graph when the store is disabled"
        );
    }

    /// `snapshot_job` with views on: the data layer stands in only for the
    /// views database.
    fn views_snapshot_job(root: &Path, inspect_dir: &Path) -> InspectJob {
        let mut job = snapshot_job(root, inspect_dir, true);
        let config = (*job.config).clone();
        job.config = Arc::new(config);
        job
    }

    #[test]
    fn callgraph_snapshot_reads_a_ready_data_layer_checkout_and_never_a_pending_one() {
        let dir = write_ts_project(3);
        let root = canonicalize_for_snapshot(dir.path());
        let storage = tempfile::tempdir().expect("storage");
        let inspect_dir = storage.path().join("inspect");
        let job = views_snapshot_job(&root, &inspect_dir);
        let manager = InspectManager::new();
        let read_job = |manager: &InspectManager, job: &InspectJob| {
            manager
                .build_tier2_callgraph_snapshot_with_refresh_and_verdict(job, false, false, &[])
                .expect("snapshot readiness")
        };
        let read = |manager: &InspectManager| read_job(manager, &job);
        let unbound = manager.build_tier2_callgraph_snapshot_with_refresh_and_verdict(
            &job,
            false,
            false,
            &[],
        );
        assert!(
            matches!(unbound, Err(CallGraphStoreError::Building)),
            "without a bound checkout dead code waits for one"
        );

        let checkout = crate::data_layer::runtime::DataCheckout::open(&root, storage.path(), None)
            .expect("bind the checkout");
        // A first build publishes the text index before the call graph.
        let deadline = Instant::now() + Duration::from_secs(60);
        while !checkout
            .view()
            .is_some_and(|view| view.status().callgraph_published)
        {
            assert!(Instant::now() < deadline, "the checkout never published");
            std::thread::sleep(Duration::from_millis(20));
        }
        manager.read_dead_code_from_data_layer(&root, &checkout);
        let (snapshot, verdict, _) = read(&manager).expect("the ready data layer answers");
        assert_eq!(verdict, DATA_LAYER_SNAPSHOT);
        assert_eq!(
            snapshot.files,
            (0..3)
                .map(|index| root.join(format!("mod{index}.ts")))
                .collect::<Vec<_>>()
        );
        assert_eq!(
            data_layer_snapshot_exports(&snapshot, &root).len(),
            3,
            "{snapshot:?}"
        );

        // A checkout of another folder never answers for a root.
        let other = InspectManager::new();
        let elsewhere = tempfile::tempdir().expect("elsewhere");
        let elsewhere = canonicalize_for_snapshot(elsewhere.path());
        other.read_dead_code_from_data_layer(&elsewhere, &checkout);
        assert!(matches!(
            other.build_tier2_callgraph_snapshot_with_refresh_and_verdict(
                &views_snapshot_job(&elsewhere, &inspect_dir),
                false,
                false,
                &[],
            ),
            Err(CallGraphStoreError::Building)
        ));

        // An edit the data layer has not resolved yet: dead code waits for
        // the call graph to resolve it, and never answers from the older
        // publication.
        let edited = root.join("mod0.ts");
        std::fs::write(&edited, "export function g0() { return 0; }\n").expect("edit");
        checkout.note_paths(std::slice::from_ref(&edited));
        let (snapshot, verdict, _) = read(&manager).expect("the refreshed data layer answers");
        assert_eq!(verdict, DATA_LAYER_SNAPSHOT);
        let exports = format!("{:?}", data_layer_snapshot_exports(&snapshot, &root));
        assert!(exports.contains("\"g0\""), "{exports}");
    }

    #[test]
    fn dead_code_follows_a_refreshed_data_layer_call_graph_as_a_cold_build_would() {
        fn published(root: &Path, storage: &Path) -> Arc<crate::data_layer::runtime::DataCheckout> {
            let checkout =
                crate::data_layer::runtime::DataCheckout::open(root, storage, None).expect("bind");
            // A first build publishes the text index before the call graph.
            let deadline = Instant::now() + Duration::from_secs(60);
            while !checkout
                .view()
                .is_some_and(|view| view.status().callgraph_published)
            {
                assert!(Instant::now() < deadline, "the checkout never published");
                std::thread::sleep(Duration::from_millis(20));
            }
            checkout
        }
        fn parts(snapshot: &CallgraphSnapshot) -> [String; 6] {
            [
                format!("{:#?}", snapshot.files),
                format!("{:#?}", snapshot.exported_symbols),
                format!("{:#?}", snapshot.outbound_calls),
                format!("{:#?}", snapshot.entry_points),
                format!("{:#?}", snapshot.entry_point_symbols),
                format!("{:#?}", snapshot.go_initializers),
            ]
        }
        let dead = |job: &InspectJob, snapshot: &Arc<CallgraphSnapshot>| {
            let mut job = job.clone();
            job.callgraph_snapshot = Some(Arc::clone(snapshot));
            let aggregate = crate::inspect::scanners::dead_code::run_dead_code_scan(&job)
                .outcome
                .expect("dead-code scan")
                .aggregate;
            let items = aggregate["items"]
                .as_array()
                .expect("dead-code items")
                .iter()
                .map(|item| format!("{}::{}", item["file"], item["symbol"]))
                .collect::<Vec<_>>();
            (items, aggregate)
        };

        let dir = tempfile::tempdir().expect("project");
        let root = std::fs::canonicalize(dir.path()).expect("canonical root");
        std::fs::create_dir_all(root.join("src")).expect("src");
        std::fs::write(
            root.join("src/foo.ts"),
            "export function used() {}\nexport function spare() {}\n",
        )
        .expect("foo");
        let main = root.join("src/main.ts");
        std::fs::write(
            &main,
            "import { used } from './foo';\nexport function main() {\n  used();\n}\n",
        )
        .expect("main");
        let storage = tempfile::tempdir().expect("storage");
        let checkout = published(&root, storage.path());
        let manager = InspectManager::new();
        manager.read_dead_code_from_data_layer(&root, &checkout);
        let mut job = views_snapshot_job(&root, &storage.path().join("inspect"));
        job.scope_files = vec![root.join("src/foo.ts"), main.clone()];
        let read = |manager: &InspectManager| {
            manager
                .build_tier2_callgraph_snapshot_with_refresh_and_verdict(&job, false, false, &[])
                .expect("snapshot readiness")
        };

        let (before, verdict, _) = read(&manager).expect("the published graph answers");
        assert_eq!(verdict, DATA_LAYER_SNAPSHOT);
        assert_eq!(dead(&job, &before).0, [r#""src/foo.ts"::"spare""#]);

        // `spare` gains the only caller `used` had.
        std::fs::write(
            &main,
            "import { spare } from './foo';\nexport function main() {\n  spare();\n}\n",
        )
        .expect("edit main");
        checkout.note_paths(std::slice::from_ref(&main));
        // Dead code waits for the call graph to resolve the edit.
        let (refreshed, verdict, _) = read(&manager).expect("the refreshed graph answers");
        assert_eq!(verdict, DATA_LAYER_SNAPSHOT);
        assert_eq!(
            checkout.view().expect("view").status().callgraph,
            crate::data_layer::checkout::PlaneStatus::Ready
        );
        let (items, aggregate) = dead(&job, &refreshed);
        assert_eq!(items, [r#""src/foo.ts"::"used""#]);

        // The same answer as a cold build of the edited checkout.
        let cold_storage = tempfile::tempdir().expect("cold storage");
        let cold_view = published(&root, cold_storage.path())
            .view()
            .expect("cold view");
        let cold = cold_view
            .dead_code_snapshot_shared()
            .expect("cold snapshot");
        assert_eq!(parts(&refreshed), parts(&cold));
        // Every call row too, though dead code reads them file by file.
        let rows = |view: &crate::data_layer::checkout::CheckoutView| {
            parts(&view.rows_dead_code_snapshot().expect("every call row"))
        };
        assert_eq!(rows(&checkout.view().expect("view")), rows(&cold_view));
        assert_eq!(aggregate, dead(&job, &cold).1);
    }

    // Unix-only: the unresolvable path relies on `..` after a regular file,
    // which Windows folds lexically into a resolvable path.

    #[test]
    fn failed_builder_attempt_history_uses_locked_refusal_detail() {
        let manager = InspectManager::new();
        let unavailable = JobOutcome::Fresh {
            payload: crate::inspect::scanners::dead_code::callgraph_unavailable_aggregate(0),
        };
        manager
            .record_tier2_attempt_outcome_for_test(InspectCategory::DeadCode, unavailable.clone());
        let first = manager.tier2_builder_state_detail(InspectCategory::DeadCode);
        let first_at = first
            .rsplit("first at ")
            .next()
            .and_then(|tail| tail.strip_suffix(')'))
            .expect("first failure detail includes first-at unix time");
        for _ in 1..7 {
            manager.record_tier2_attempt_outcome_for_test(
                InspectCategory::DeadCode,
                unavailable.clone(),
            );
        }
        assert_eq!(
            manager.tier2_builder_state_detail(InspectCategory::DeadCode),
            format!("last attempt failed: callgraph_unavailable (attempt 7, first at {first_at})")
        );
        assert_eq!(
            manager.tier2_builder_state(InspectCategory::DeadCode),
            InspectBuilderState::Absent,
            "a finished failure must not keep the registry in an in-flight state"
        );
        assert_eq!(
            manager.try_tier2_builder_busy(),
            Some(false),
            "failed-attempt history must not look like a live rebuild"
        );

        manager.record_tier2_attempt_outcome_for_test(
            InspectCategory::DeadCode,
            JobOutcome::Fresh {
                payload: serde_json::json!({ "callgraph_available": true, "count": 0 }),
            },
        );
        assert_eq!(
            manager.tier2_builder_state_detail(InspectCategory::DeadCode),
            InspectBuilderState::Absent.as_str()
        );
    }

    #[test]
    fn dead_code_forced_deletion_refreshes_callgraph_store_before_rollup() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = std::fs::canonicalize(dir.path()).expect("canonical root");
        write_fixture_file(
            &root,
            "package.json",
            r#"{"name":"dead-code-delete-refresh","type":"module","main":"src/main.ts"}"#,
            3_100_000_000,
        );
        write_fixture_file(
            &root,
            "src/main.ts",
            "export function main() {}\n",
            3_100_000_001,
        );
        write_fixture_file(
            &root,
            "src/dead.ts",
            "export function plantedDead() {}\n",
            3_100_000_002,
        );

        let inspect_dir = root.join(".aft-cache").join("opencode").join("inspect");
        let manager = InspectManager::new();
        let storage = tempfile::tempdir().expect("storage");
        let checkout = bind_published_call_graph(&manager, &root, storage.path());

        let config = Arc::new(crate::config::Config {
            project_root: Some(root.clone()),
            indexes: crate::config::IndexesConfig {
                trigram: false,
                semantic: false,
                callgraph: true,
            },
            ..crate::config::Config::default()
        });
        let symbol_cache = Arc::new(std::sync::RwLock::new(crate::parser::SymbolCache::new()));
        let snapshot = InspectSnapshot::new(
            root.clone(),
            inspect_dir.clone(),
            Arc::clone(&config),
            Arc::clone(&symbol_cache),
        );
        let initial_job =
            manager.tier2_reuse_job(snapshot.clone(), InspectCategory::DeadCode, None);
        let initial = manager
            .tier2_run_with_reuse_job_result_with_options(
                initial_job,
                Tier2ReuseOptions::default(),
                None,
            )
            .outcome
            .expect("initial dead_code scan succeeds")
            .aggregate;
        assert!(
            aggregate_has_file_symbol(&initial, "src/dead.ts", "plantedDead"),
            "initial scan should report the planted dead export: {initial:#}"
        );

        let deleted = root.join("src/dead.ts");
        std::fs::remove_file(&deleted).expect("delete dead fixture");
        // The watcher hands the deletion to the root's index-store checkout.
        checkout.note_paths(std::slice::from_ref(&deleted));
        let delete_job = manager.tier2_reuse_job(snapshot, InspectCategory::DeadCode, None);
        let refreshed = manager
            .tier2_run_with_reuse_job_result_with_options(
                delete_job,
                Tier2ReuseOptions {
                    inventory: None,
                    force_rescan_paths: [deleted.clone()].into_iter().collect(),
                    allow_callgraph_cold_build: true,
                    require_callgraph_snapshot: false,
                    interactive: false,
                    return_contributions: false,
                },
                None,
            )
            .outcome
            .expect("delete refresh dead_code scan succeeds")
            .aggregate;

        assert_eq!(
            refreshed
                .get("callgraph_available")
                .and_then(Value::as_bool),
            Some(true),
            "forced watcher paths must keep the callgraph-backed aggregate available: {refreshed:#}"
        );
        assert!(
            !aggregate_has_file_symbol(&refreshed, "src/dead.ts", "plantedDead"),
            "delete refresh should remove the planted dead export: {refreshed:#}"
        );
    }

    /// The folders and index-store checkout an edge-case fixture keeps alive
    /// for the test; the checkout is dropped before its storage folder.
    struct EdgeCaseFixture {
        checkout: Arc<crate::data_layer::runtime::DataCheckout>,
        _storage: tempfile::TempDir,
        _project: tempfile::TempDir,
    }

    /// A Rust binary crate with one live and one dead function, plus `extra`
    /// edge-case files, with a published index-store call graph. Returns the
    /// manager, snapshot and a job for one dead-code pass.
    fn dead_code_edge_case_fixture(
        extra: &[(&str, &str)],
    ) -> (EdgeCaseFixture, PathBuf, InspectManager, InspectSnapshot) {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = std::fs::canonicalize(dir.path()).expect("canonical root");
        write_fixture_file(
            &root,
            "Cargo.toml",
            "[package]\nname = \"edge-cases\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
            3_200_000_000,
        );
        write_fixture_file(
            &root,
            "src/main.rs",
            "mod dead;\nfn main() {\n    dead::live();\n}\n",
            3_200_000_001,
        );
        write_fixture_file(
            &root,
            "src/dead.rs",
            "pub fn live() {}\npub fn planted_dead() {}\n",
            3_200_000_002,
        );
        for (index, (relative, contents)) in extra.iter().enumerate() {
            write_fixture_file(&root, relative, contents, 3_200_000_010 + index as i64);
        }

        let inspect_dir = root.join(".aft-cache").join("opencode").join("inspect");
        let manager = InspectManager::new();
        let storage = tempfile::tempdir().expect("storage");
        let checkout = bind_published_call_graph(&manager, &root, storage.path());

        let config = Arc::new(crate::config::Config {
            project_root: Some(root.clone()),
            indexes: crate::config::IndexesConfig {
                trigram: false,
                semantic: false,
                callgraph: true,
            },
            ..crate::config::Config::default()
        });
        let snapshot = InspectSnapshot::new(
            root.clone(),
            inspect_dir,
            config,
            Arc::new(std::sync::RwLock::new(crate::parser::SymbolCache::new())),
        );
        let fixture = EdgeCaseFixture {
            checkout,
            _storage: storage,
            _project: dir,
        };
        (fixture, root, manager, snapshot)
    }

    fn seed_dead_code_fact_cache(
        manager: &InspectManager,
        snapshot: &InspectSnapshot,
    ) -> (InspectJob, Arc<InspectCache>) {
        let mut job = manager.tier2_reuse_job(snapshot.clone(), InspectCategory::DeadCode, None);
        job.scope_files = scope_files(
            &job.project_root,
            &JobScope::for_project(job.project_root.clone()),
        );
        let (graph, _, _) = manager
            .build_tier2_callgraph_snapshot_with_refresh_and_verdict(&job, false, true, &[])
            .unwrap()
            .expect("ready fixture call graph");
        job.callgraph_snapshot = Some(graph);
        let facts = run_tier2_scan(&job, None).outcome.expect("seed file facts");
        let cache = manager.cache_for_snapshot(snapshot).expect("inspect cache");
        // Seed only per-file facts: no project aggregate has been published yet.
        cache
            .apply_contribution_updates_for_config(
                job.category,
                Tier2ContributionUpdates {
                    upserts: facts.contributions,
                    ..Tier2ContributionUpdates::default()
                },
                &job.config,
            )
            .unwrap();
        assert!(cache
            .get_aggregated_for_config(&job.key, &job.config)
            .unwrap()
            .is_none());
        (job, cache)
    }

    fn corrupt_cached_dead_code_exports(cache: &InspectCache, file: &str, corruption: &str) {
        let expected = canonicalize_for_snapshot(&cache.project_root().join(file));
        // Match fixture aliases by identity, but bind the producer's exact SQL
        // key spelling, including native Windows separators, without rewriting it.
        let file = cache
            .contribution_freshness(InspectCategory::DeadCode)
            .unwrap()
            .into_iter()
            .find(|(path, _)| {
                canonicalize_for_snapshot(&cache.project_root().join(path)) == expected
            })
            .expect("seeded facts must contain the fixture source")
            .0
            .to_string_lossy()
            .into_owned();
        let conn = rusqlite::Connection::open(cache.sqlite_path()).unwrap();
        let blob: Vec<u8> = conn
            .query_row(
                "SELECT contribution FROM tier2_contributions \
                 WHERE category = 'dead_code' AND project_key = ?1 AND file_path = ?2",
                rusqlite::params![cache.project_key(), file],
                |row| row.get(0),
            )
            .unwrap();
        let mut facts: Value = serde_json::from_slice(&blob).unwrap();
        assert!(facts["exports"].is_array());
        assert_eq!(
            facts["facts_format_version"],
            super::super::scanners::dead_code::DEAD_CODE_FACTS_FORMAT_VERSION
        );
        match corruption {
            "missing exports" => {
                facts.as_object_mut().unwrap().remove("exports");
            }
            "invalid exports" => facts["exports"] = json!("damaged exports"),
            _ => panic!("unknown corruption: {corruption}"),
        }
        // Leave file hashes, freshness and fact-version metadata intact. Only
        // the cached fact body is damaged, not the source or its cache key.
        assert_eq!(
            conn.execute(
                "UPDATE tier2_contributions SET contribution = ?3 \
                 WHERE category = 'dead_code' AND project_key = ?1 AND file_path = ?2",
                rusqlite::params![
                    cache.project_key(),
                    file,
                    serde_json::to_vec(&facts).unwrap()
                ],
            )
            .unwrap(),
            1
        );
    }

    /// Dead code over the index store never reads the inspect cache's
    /// per-file facts, so damage to them leaves its answer complete.
    fn assert_no_dead_code_gap(payload: &Value) {
        assert_eq!(payload["complete"], true, "{payload:#}");
        assert!(
            payload["parse_errors"].as_array().is_none_or(Vec::is_empty),
            "{payload:#}"
        );
    }

    #[test]
    fn damaged_cached_dead_code_rows_before_a_rollup_leave_it_complete() {
        for corruption in ["missing exports", "invalid exports"] {
            let (_dir, root, manager, snapshot) = dead_code_edge_case_fixture(&[]);
            let (job, cache) = seed_dead_code_fact_cache(&manager, &snapshot);
            corrupt_cached_dead_code_exports(&cache, "src/./main.rs", corruption);
            let first = manager
                .tier2_run_with_reuse_job_result_with_options(
                    job.clone(),
                    Tier2ReuseOptions::default(),
                    None,
                )
                .outcome
                .expect("initial project rollup")
                .aggregate;
            assert_no_dead_code_gap(&first);
            assert!(aggregate_has_file_symbol(
                &first,
                "src/dead.rs",
                "planted_dead"
            ));
            assert_eq!(
                cache
                    .get_aggregated_for_config(&job.key, &job.config)
                    .unwrap(),
                Some(first.clone())
            );

            super::super::cache::CONTRIBUTION_LOAD_WORK.with(|work| work.set((0, 0, 0)));
            let reused = manager
                .tier2_quick_reuse_success(&job, &cache, &Tier2ReuseOptions::default())
                .unwrap()
                .expect("unchanged project must reuse its saved aggregate");
            assert_eq!(
                super::super::cache::CONTRIBUTION_LOAD_WORK.with(std::cell::Cell::get),
                (0, 0, 0),
                "reuse must not decode contribution bodies"
            );
            assert_no_dead_code_gap(&reused.aggregate);
            assert_eq!(reused.aggregate, first);

            // Both writer and read-only project lookups return the saved
            // answer.
            let scope = JobScope::for_project(root);
            for outcome in [
                manager.tier2_read_cached(snapshot.clone(), job.category, scope.clone()),
                manager.tier2_read_cached_readonly(snapshot.clone(), job.category, scope),
            ] {
                let payload = fresh_payload(outcome);
                assert_no_dead_code_gap(&payload);
                assert_eq!(payload, first);
            }
        }
    }

    #[test]
    fn damaged_cached_dead_code_rows_after_a_rollup_leave_the_recompute_complete() {
        for corruption in ["missing exports", "invalid exports"] {
            let (_dir, root, manager, snapshot) = dead_code_edge_case_fixture(&[]);
            let (job, cache) = seed_dead_code_fact_cache(&manager, &snapshot);
            let first = manager
                .tier2_run_with_reuse_job_result_with_options(
                    job.clone(),
                    Tier2ReuseOptions::default(),
                    None,
                )
                .outcome
                .expect("successful initial project rollup")
                .aggregate;
            assert_eq!(first["complete"], true);
            assert!(aggregate_has_file_symbol(
                &first,
                "src/dead.rs",
                "planted_dead"
            ));
            let original_hash = cache
                .contribution_set_hash_for_config(job.category, &job.config)
                .unwrap();
            corrupt_cached_dead_code_exports(&cache, "src/../src/dead.rs", corruption);
            assert_eq!(
                cache
                    .contribution_set_hash_for_config(job.category, &job.config)
                    .unwrap(),
                original_hash,
                "fact-body damage does not change the analyzed source hashes"
            );
            let reused = manager
                .tier2_quick_reuse_success(&job, &cache, &Tier2ReuseOptions::default())
                .unwrap()
                .expect("unchanged source findings remain reusable");
            assert_eq!(reused.aggregate, first);
            assert_eq!(
                fresh_payload(manager.tier2_read_cached(
                    snapshot.clone(),
                    job.category,
                    JobScope::for_project(root.clone()),
                )),
                first
            );

            // A manifest change invalidates the aggregate key without reparsing
            // the unchanged Rust file. The next rollup works dead code out
            // from the index store's facts, never from its bad row.
            let manifest = std::fs::read_to_string(root.join("Cargo.toml")).unwrap();
            write_fixture_file(
                &root,
                "Cargo.toml",
                &format!("{manifest}\n# refresh rollup\n"),
                3_200_000_050,
            );
            assert_ne!(
                cache
                    .contribution_set_hash_for_config(job.category, &job.config)
                    .unwrap(),
                original_hash
            );
            assert!(manager
                .tier2_quick_reuse_success(&job, &cache, &Tier2ReuseOptions::default())
                .unwrap()
                .is_none());
            let recomputed = manager
                .tier2_run_with_reuse_job_result_with_options(
                    job,
                    Tier2ReuseOptions::default(),
                    None,
                )
                .outcome
                .expect("recompute after manifest change");
            let scanned = recomputed
                .scanned_files
                .iter()
                .map(|file| canonicalize_for_snapshot(file))
                .collect::<BTreeSet<_>>();
            assert!(
                scanned.contains(&canonicalize_for_snapshot(&root.join("Cargo.toml"))),
                "the changed manifest must be recognized among the rescanned files"
            );
            assert!(
                !scanned.contains(&canonicalize_for_snapshot(&root.join("src/dead.rs"))),
                "the corrupted row must not be repaired by rescanning its unchanged source"
            );
            assert_no_dead_code_gap(&recomputed.aggregate);
            assert_eq!(recomputed.aggregate, first);
            assert!(aggregate_has_file_symbol(
                &recomputed.aggregate,
                "src/dead.rs",
                "planted_dead"
            ));
        }
    }

    /// Runs one dead-code pass over an edge-case fixture and checks the rollup
    /// completes with every walked file covered by stored facts.
    fn assert_dead_code_rollup_completes_with(extra: &[(&str, &str)]) -> Value {
        let (_dir, root, manager, snapshot) = dead_code_edge_case_fixture(extra);
        let job = manager.tier2_reuse_job(snapshot.clone(), InspectCategory::DeadCode, None);
        let aggregate = manager
            .tier2_run_with_reuse_job_result_with_options(
                job,
                Tier2ReuseOptions {
                    require_callgraph_snapshot: true,
                    ..Tier2ReuseOptions::default()
                },
                None,
            )
            .outcome
            .unwrap_or_else(|error| panic!("dead-code rollup must complete: {error}"))
            .aggregate;
        assert_eq!(
            aggregate["callgraph_available"], true,
            "the rollup must use the call graph: {aggregate:#}"
        );
        assert!(
            aggregate_has_file_symbol(&aggregate, "src/dead.rs", "planted_dead"),
            "the planted dead function must be reported: {aggregate:#}"
        );
        assert!(
            !aggregate_has_file_symbol(&aggregate, "src/dead.rs", "live"),
            "{aggregate:#}"
        );

        let walked = crate::callgraph::walk_project_files(&root)
            .map(|path| relative_cache_key(&root, &path))
            .collect::<BTreeSet<_>>();
        let cache = manager
            .cache_for_snapshot(&snapshot)
            .expect("inspect cache");
        let stored = cache
            .load_tier2_contributions(InspectCategory::DeadCode)
            .expect("load contributions")
            .into_iter()
            .map(|record| record.file_path.to_string_lossy().to_string())
            .collect::<BTreeSet<_>>();
        assert_eq!(
            stored, walked,
            "every walked file must have stored dead-code facts"
        );
        aggregate
    }

    #[test]
    fn dead_code_rollup_completes_with_an_empty_rust_file() {
        assert_dead_code_rollup_completes_with(&[("src/empty.rs", "")]);
    }

    #[test]
    fn dead_code_rollup_completes_with_a_comment_only_file() {
        assert_dead_code_rollup_completes_with(&[(
            "src/comments.rs",
            "// only a line comment\n/* and a block comment */\n",
        )]);
    }

    #[test]
    fn dead_code_rollup_completes_with_a_file_that_fails_to_parse() {
        assert_dead_code_rollup_completes_with(&[(
            "src/broken.rs",
            "pub fn broken( {\n    let = ;\n",
        )]);
    }

    #[test]
    fn dead_code_rollup_completes_with_a_language_without_a_dead_code_scanner() {
        let aggregate =
            assert_dead_code_rollup_completes_with(&[("scripts/build.groovy", "println 'x'\n")]);
        assert!(
            aggregate["languages_skipped"]
                .as_array()
                .is_some_and(|languages| !languages.is_empty()),
            "the unsupported language must be reported as skipped: {aggregate:#}"
        );
    }

    #[test]
    fn dead_code_rollup_completes_with_a_generated_file() {
        let aggregate = assert_dead_code_rollup_completes_with(&[(
            "src/generated.rs",
            "// Code generated by fixture. DO NOT EDIT.\npub fn generated_dead() {}\n",
        )]);
        assert_eq!(
            aggregate["count"], 1,
            "generated findings stay out of the headline count: {aggregate:#}"
        );
        assert_eq!(aggregate["generated_count"], 1, "{aggregate:#}");
    }

    /// A file excluded by `.aftignore` is outside the walked scope, so the
    /// coverage check never requires facts for it.
    #[test]
    fn dead_code_rollup_completes_with_an_aftignored_file() {
        assert_dead_code_rollup_completes_with(&[
            (".aftignore", "ignored/\n"),
            ("ignored/skip.rs", "pub fn ignored_dead() {}\n"),
        ]);
    }

    /// Every inspect scan pass starts from `scope_files`, so a gitignored path
    /// it lists would be scanned by every tier-1 and tier-2 category.
    #[test]
    fn inspect_scope_files_honour_gitignore_in_non_git_root_like_git_root() {
        use crate::context::ignore_rules_fixture as fixture;
        let plain = tempfile::tempdir().unwrap();
        let git = tempfile::tempdir().unwrap();
        fixture::write(plain.path(), false);
        fixture::write(git.path(), true);
        let scanned = |root: &Path| {
            let root = root.to_path_buf();
            fixture::relative_set(
                &root,
                &scope_files(&root, &JobScope::for_project(root.clone())),
            )
        };

        let plain_files = scanned(plain.path());
        fixture::assert_honours_ignore_rules(&plain_files, "non-git inspect scope");
        assert_eq!(plain_files, scanned(git.path()));
    }

    /// Facts missing at rollup time (files scanned before the call graph was
    /// ready) are filled by scanning those files with the rollup's graph, so a
    /// pass completes instead of failing until some later pass.
    #[test]
    fn missing_dead_code_facts_are_filled_before_rollup() {
        let (fixture, root, manager, snapshot) =
            dead_code_edge_case_fixture(&[("src/empty.rs", "")]);
        let mut job = manager.tier2_reuse_job(snapshot.clone(), InspectCategory::DeadCode, None);
        job.scope_files = scope_files(&root, &JobScope::for_project(root.clone()));
        job.callgraph_snapshot = Some(Arc::new(
            fixture
                .checkout
                .view()
                .expect("published call graph")
                .dead_code_snapshot()
                .expect("project snapshot"),
        ));
        let cache = manager
            .cache_for_snapshot(&snapshot)
            .expect("inspect cache");
        let expected = current_project_files(&root, &job.scope_files);
        // Keys are native relative paths, so build this one the same way.
        let empty_key = relative_cache_key(&root, &root.join("src").join("empty.rs"));
        assert!(expected.contains_key(&empty_key), "{expected:?}");

        let before = load_contributions(cache.as_ref(), &job).expect("load");
        assert_eq!(
            dead_code_files_without_facts(&root, &expected, &before).len(),
            expected.len(),
            "control: a fresh cache has no facts"
        );
        let hash = manager
            .fill_missing_dead_code_facts(&job, cache.as_ref(), &expected, &before)
            .expect("fill succeeds");
        assert!(hash.is_some(), "facts were stored");
        let after = load_contributions(cache.as_ref(), &job).expect("load");
        assert!(
            dead_code_files_without_facts(&root, &expected, &after).is_empty(),
            "every expected file must have facts after the fill"
        );
        assert_eq!(
            manager
                .fill_missing_dead_code_facts(&job, cache.as_ref(), &expected, &after)
                .expect("second fill"),
            None,
            "nothing to fill once covered"
        );
    }

    fn aggregate_has_file_symbol(aggregate: &Value, file: &str, symbol: &str) -> bool {
        aggregate
            .get("items")
            .and_then(Value::as_array)
            .is_some_and(|items| {
                items.iter().any(|item| {
                    item.get("file").and_then(Value::as_str) == Some(file)
                        && item.get("symbol").and_then(Value::as_str) == Some(symbol)
                })
            })
    }

    // A scoped payload must not carry the project-wide `by_language` breakdown
    // alongside the recomputed in-scope count — that contradiction renders as
    // e.g. "Dead code: 1 (rust 214, ts 143)".
    #[test]
    fn scoped_filter_drops_project_wide_by_language() {
        let scope = JobScope::from_roots("/proj", vec![PathBuf::from("/proj/src/a")]);
        assert!(
            !scope.is_project_wide(),
            "scope must be non-project for test"
        );
        let payload = serde_json::json!({
            "count": 99,
            "by_language": { "rust": 214, "typescript": 143 },
            "items": [
                { "file": "/proj/src/a/x.rs", "symbol": "live" },
                { "file": "/proj/src/other/y.rs", "symbol": "out" },
            ],
        });
        let filtered = filter_payload_for_scope(payload, &scope);
        assert!(
            filtered.get("by_language").is_none(),
            "scoped payload must drop project-wide by_language: {filtered}"
        );
        // Count is recomputed to the in-scope items (only x.rs under src/a).
        assert_eq!(filtered.get("count").and_then(|v| v.as_u64()), Some(1));
    }
    #[cfg(debug_assertions)]
    #[test]
    fn tier2_read_cached_freshness_does_not_hash_unchanged_contributions() {
        let (_dir, manager, snapshot, scope, _files) = duplicate_cache_fixture();
        let fixture_root = snapshot.project_root.clone();

        crate::cache_freshness::reset_hash_file_if_small_count_for_debug();
        crate::cache_freshness::reset_verify_file_strict_count_for_debug();
        assert_fresh(manager.tier2_read_cached(snapshot, InspectCategory::Duplicates, scope));

        assert_eq!(
            crate::cache_freshness::verify_file_strict_count_under_for_debug(&fixture_root),
            0,
            "dispatch-thread inspect freshness must not use strict verification"
        );
        assert_eq!(
            crate::cache_freshness::hash_file_if_small_count_for_debug(),
            0,
            "unchanged contribution files must stay on the stat-only fast path"
        );
    }

    #[cfg(debug_assertions)]
    #[test]
    fn tier2_read_cached_freshness_returns_byte_identical_cold_scan_aggregate() {
        let (_dir, manager, snapshot, scope, _files) = duplicate_uncached_fixture();
        let cold_payload = fresh_payload(manager.tier2_run_with_reuse(
            snapshot.clone(),
            InspectCategory::Duplicates,
            scope.clone(),
            None,
        ));

        crate::cache_freshness::reset_hash_file_if_small_count_for_debug();
        crate::cache_freshness::reset_verify_file_strict_count_for_debug();
        let fixture_root = snapshot.project_root.clone();
        let warm_payload =
            fresh_payload(manager.tier2_read_cached(snapshot, InspectCategory::Duplicates, scope));

        let cold_bytes = serde_json::to_vec(&cold_payload).expect("serialize cold aggregate");
        let warm_bytes = serde_json::to_vec(&warm_payload).expect("serialize warm aggregate");
        assert_eq!(
            warm_bytes, cold_bytes,
            "warm unchanged read must return the byte-identical aggregate as the cold scan"
        );
        assert_eq!(
            crate::cache_freshness::verify_file_strict_count_under_for_debug(&fixture_root),
            0,
            "dispatch-thread warm read must not use strict verification"
        );
        assert_eq!(
            crate::cache_freshness::hash_file_if_small_count_for_debug(),
            0,
            "warm unchanged read must not content-hash cached contribution files"
        );
    }

    #[test]
    fn tier2_read_cached_freshness_detects_changed_added_and_deleted_files() {
        let (_dir, manager, snapshot, scope, _files) = duplicate_cache_fixture();
        write_fixture_file(
            &snapshot.project_root,
            "src/foo.ts",
            "export const foo = 101;\nexport const changed = true;\n",
            3_000_000_001,
        );
        assert_stale(manager.tier2_read_cached(snapshot, InspectCategory::Duplicates, scope));

        let (_dir, manager, snapshot, scope, _files) = duplicate_cache_fixture();
        write_fixture_file(
            &snapshot.project_root,
            "src/added.ts",
            "export const added = 3;\n",
            3_000_000_002,
        );
        assert_stale(manager.tier2_read_cached(snapshot, InspectCategory::Duplicates, scope));

        let (_dir, manager, snapshot, scope, files) = duplicate_cache_fixture();
        std::fs::remove_file(&files[0]).expect("delete cached contribution file");
        assert_stale(manager.tier2_read_cached(snapshot, InspectCategory::Duplicates, scope));
    }

    #[test]
    fn audit05_freshness_stops_after_first_mismatch() {
        let (_dir, manager, snapshot, _scope, files) = duplicate_cache_fixture();
        std::fs::remove_file(&files[1]).unwrap();
        let cache = manager.cache_for_snapshot(&snapshot).unwrap();
        FRESHNESS_RECORD_VISITS.with(|visits| visits.set(0));
        assert!(!manager
            .tier2_cached_aggregate_is_fresh(&snapshot, InspectCategory::Duplicates, cache.as_ref())
            .unwrap());
        assert_eq!(FRESHNESS_RECORD_VISITS.with(|visits| visits.get()), 1);
    }

    fn duplicate_cache_fixture() -> (
        tempfile::TempDir,
        InspectManager,
        InspectSnapshot,
        JobScope,
        Vec<PathBuf>,
    ) {
        let (dir, manager, snapshot, scope, files) = duplicate_uncached_fixture();
        store_duplicate_cache(&manager, &snapshot, &files);
        (dir, manager, snapshot, scope, files)
    }

    fn duplicate_uncached_fixture() -> (
        tempfile::TempDir,
        InspectManager,
        InspectSnapshot,
        JobScope,
        Vec<PathBuf>,
    ) {
        use crate::config::Config;
        use crate::parser::SymbolCache;
        use std::sync::RwLock;

        let dir = tempfile::tempdir().expect("tempdir");
        let root = std::fs::canonicalize(dir.path()).expect("canonical fixture root");
        let files = vec![
            write_fixture_file(
                &root,
                "src/foo.ts",
                "export const fixture = () => 1;
export const shared = 1;
",
                3_000_000_000,
            ),
            write_fixture_file(
                &root,
                "src/bar.ts",
                "export const fixture = () => 1;
export const shared = 1;
",
                3_000_000_000,
            ),
        ];
        let inspect_dir = root.join(".aft-cache").join("inspect");
        let snapshot = InspectSnapshot::new(
            root.clone(),
            inspect_dir,
            Arc::new(Config {
                project_root: Some(root.clone()),
                ..Config::default()
            }),
            Arc::new(RwLock::new(SymbolCache::new())),
        );
        let scope = JobScope::for_project(root);
        let manager = InspectManager::new();
        (dir, manager, snapshot, scope, files)
    }

    fn write_fixture_file(root: &Path, relative: &str, content: &str, mtime_secs: i64) -> PathBuf {
        let path = root.join(relative);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("create fixture parent");
        }
        std::fs::write(&path, content).expect("write fixture file");
        filetime::set_file_mtime(&path, filetime::FileTime::from_unix_time(mtime_secs, 0))
            .expect("set fixture mtime");
        path
    }

    fn store_duplicate_cache(
        manager: &InspectManager,
        snapshot: &InspectSnapshot,
        files: &[PathBuf],
    ) {
        let cache = manager
            .cache_for_snapshot(snapshot)
            .expect("open inspect cache");
        let contributions = files
            .iter()
            .map(|file| {
                let freshness = crate::cache_freshness::collect(file).expect("collect freshness");
                FileContribution::new(
                    InspectCategory::Duplicates,
                    file.clone(),
                    freshness,
                    serde_json::json!({
                        "file": relative_cache_key(&snapshot.project_root, file),
                        "fragments": [],
                    }),
                )
            })
            .collect::<Vec<_>>();
        cache
            .store_tier2_result(
                JobKey::for_project_category(InspectCategory::Duplicates),
                files,
                &contributions,
                serde_json::json!({
                    "count": 0,
                    "groups": [],
                    "scanned_files": files.len(),
                    "total_groups": 0,
                }),
            )
            .expect("store tier2 cache fixture");
    }

    fn assert_fresh(outcome: JobOutcome) {
        let _ = fresh_payload(outcome);
    }

    fn fresh_payload(outcome: JobOutcome) -> Value {
        match outcome {
            JobOutcome::Fresh { payload } => payload,
            other => panic!("expected fresh cached Tier-2 outcome, got {other:?}"),
        }
    }

    fn assert_stale(outcome: JobOutcome) {
        match outcome {
            JobOutcome::Stale { .. } => {}
            other => panic!("expected stale cached Tier-2 outcome, got {other:?}"),
        }
    }
}

#[cfg(test)]
mod dead_code_projection_tests {
    use super::*;
    use crate::callgraph::walk_project_files;
    use crate::config::Config;

    use crate::parser::SymbolCache;
    use filetime::FileTime;
    use std::sync::atomic::{AtomicI64, Ordering as AtomicOrdering};
    use std::sync::RwLock;

    static NEXT_MTIME: AtomicI64 = AtomicI64::new(1_900_000_000);

    #[test]
    fn scoped_dead_code_rollup_uses_ready_callgraph_and_degrades_without_it() {
        let dir = tempfile::tempdir().expect("tempdir");
        write_projection_fixture(dir.path());
        let root = canonical_root(dir.path());
        let inspect_dir = root.join(".aft-cache").join("inspect");
        let files = project_files(&root);
        // Dead code reads its call graph from the root's index-store checkout.
        let storage = tempfile::tempdir().expect("storage");
        let checkout = crate::data_layer::runtime::DataCheckout::open(&root, storage.path(), None)
            .expect("bind the checkout");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
        while !checkout
            .view()
            .is_some_and(|view| view.status().callgraph_published)
        {
            assert!(
                std::time::Instant::now() < deadline,
                "the checkout never published"
            );
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        let projected = checkout
            .view()
            .expect("view")
            .dead_code_snapshot()
            .expect("project snapshot");

        let config = Arc::new(Config {
            project_root: Some(root.clone()),
            indexes: crate::config::IndexesConfig {
                trigram: false,
                semantic: false,
                callgraph: true,
            },
            ..Config::default()
        });
        let symbol_cache = Arc::new(RwLock::new(SymbolCache::new()));
        let scan_job = InspectJob {
            job_id: 87,
            key: JobKey::for_project_category(InspectCategory::DeadCode),
            category: InspectCategory::DeadCode,
            scope_files: files.clone(),
            project_root: root.clone(),
            inspect_dir: inspect_dir.clone(),
            config: Arc::clone(&config),
            symbol_cache: Arc::clone(&symbol_cache),
            inspect_writer: true,
            callgraph_writer: true,
            callgraph_snapshot: Some(Arc::new(projected)),
        };
        let success = crate::inspect::scanners::dead_code::run_dead_code_scan(&scan_job)
            .outcome
            .expect("dead_code scan succeeds");
        let cache = InspectCache::open(inspect_dir.clone(), root.clone()).expect("open cache");
        cache
            .store_tier2_result(
                scan_job.key.clone(),
                &success.scanned_files,
                &success.contributions,
                success.aggregate.clone(),
            )
            .expect("store tier2 result");

        let snapshot = InspectSnapshot::new(root.clone(), inspect_dir, config, symbol_cache);
        let scope = JobScope::from_roots(root.clone(), vec![root.join("src/live.ts")]);
        assert!(
            !scope.is_project_wide(),
            "live.ts file scope must be scoped"
        );

        let manager = InspectManager::new();
        manager.read_dead_code_from_data_layer(&root, &checkout);
        // The scoped answer of a manager that has cached no rollup yet.
        let uncached = |cache: &InspectCache| {
            let fresh = InspectManager::new();
            fresh.read_dead_code_from_data_layer(&root, &checkout);
            scoped_tier2_payload_from_contributions(
                Some(&fresh),
                &snapshot,
                InspectCategory::DeadCode,
                cache,
                success.aggregate.clone(),
                &scope,
            )
            .unwrap()
        };
        let ready_payload = scoped_tier2_payload_from_contributions(
            Some(&manager),
            &snapshot,
            InspectCategory::DeadCode,
            &cache,
            success.aggregate.clone(),
            &scope,
        )
        .expect("ready scoped payload");
        assert_eq!(
            ready_payload
                .get("callgraph_available")
                .and_then(Value::as_bool),
            Some(true),
            "ready store should produce a callgraph-backed scoped rollup: {ready_payload:#}"
        );
        assert_live_item(&ready_payload, "src/live.ts", "knownLive");

        let read = || {
            scoped_tier2_payload_from_contributions(
                Some(&manager),
                &snapshot,
                InspectCategory::DeadCode,
                &cache,
                success.aggregate.clone(),
                &scope,
            )
            .expect("scoped payload")
        };
        assert_eq!(read(), ready_payload);

        // Refreshed cached contributions with the same source hash leave the
        // answer alone: scoped dead code is built from the index store's
        // facts, not from cached contributions.
        let mut refreshed = success.contributions.clone();
        refreshed[0].contribution["refresh_marker"] = json!(true);
        cache
            .store_tier2_result(
                scan_job.key.clone(),
                &success.scanned_files,
                &refreshed,
                success.aggregate.clone(),
            )
            .unwrap();
        assert_eq!(read(), ready_payload);

        let live_file = root.join("src").join("live.ts");
        let live_relative = live_file.strip_prefix(&root).expect("relative live file");
        let contribution_count_before_delete = cache
            .load_tier2_contributions(InspectCategory::DeadCode)
            .expect("load contributions before delete")
            .len();
        cache
            .delete_tier2_contribution(InspectCategory::DeadCode, live_relative)
            .unwrap();
        let contribution_count_after_delete = cache
            .load_tier2_contributions(InspectCategory::DeadCode)
            .expect("load contributions after delete")
            .len();
        assert_eq!(
            contribution_count_after_delete + 1,
            contribution_count_before_delete,
            "deleting the live file must remove exactly one contribution row"
        );
        let changed_contributions = read();
        assert_eq!(changed_contributions, uncached(&cache));

        write_file(
            &root.join("src/main.ts"),
            "import { knownDead } from './dead';\nexport function main() { knownDead(); }\n",
        );
        // The watcher hands the edit to the root's index-store checkout.
        checkout.note_paths(&[root.join("src/main.ts")]);
        let changed_graph = read();
        assert_eq!(changed_graph, uncached(&cache));
        // With the call graph switched off there is no graph to read: the
        // rollup must say so instead of treating an empty graph as truth.
        let mut graph_off = snapshot.config.as_ref().clone();
        graph_off.indexes.callgraph = false;
        let graph_off_snapshot = InspectSnapshot::new(
            root.clone(),
            snapshot.inspect_dir.clone(),
            Arc::new(graph_off),
            Arc::clone(&snapshot.symbol_cache),
        );
        let unavailable_payload = scoped_tier2_payload_from_contributions(
            Some(&manager),
            &graph_off_snapshot,
            InspectCategory::DeadCode,
            &cache,
            success.aggregate,
            &scope,
        )
        .expect("unavailable scoped payload");
        assert_eq!(
            unavailable_payload
                .get("callgraph_available")
                .and_then(Value::as_bool),
            Some(false),
            "a switched-off call graph must report callgraph_unavailable instead of fabricating an empty graph: {unavailable_payload:#}"
        );
        assert_live_item(&unavailable_payload, "src/live.ts", "knownLive");
    }

    #[test]
    fn dead_code_projection_dead_code_scan_reports_expected_verdicts() {
        let dir = tempfile::tempdir().expect("tempdir");
        write_projection_fixture(dir.path());
        let root = canonical_root(dir.path());
        let files = project_files(&root);
        let projected = fixture_callgraph_snapshot(&root);

        let projected_aggregate = dead_code_aggregate(&root, files, projected);
        assert_dead_item(&projected_aggregate, "src/dead.ts", "knownDead");
        assert_live_item(&projected_aggregate, "src/live.ts", "knownLive");
        // `service.render()` has a known receiver, the object literal
        // `service`, so the call reaches only that literal's `render`. The
        // index store resolves it exactly and adds no name-based dispatch
        // rescue, so the two exported `render` functions, which nothing
        // calls, are reported dead.
        assert_dead_item(&projected_aggregate, "src/render.ts", "render");
        assert_dead_item(&projected_aggregate, "src/other_render.ts", "render");
    }

    #[test]
    fn dead_code_projection_rust_attribute_entry_points_are_live() {
        let dir = tempfile::tempdir().expect("tempdir");
        write_rust_attribute_entry_fixture(dir.path());
        let root = canonical_root(dir.path());
        let files = project_files(&root);
        let projected = fixture_callgraph_snapshot(&root);
        let aggregate = dead_code_aggregate(&root, files, projected);
        assert_live_item(&aggregate, "src/commands.rs", "get_primers");
        assert_live_item(&aggregate, "src/db.rs", "helper");
        assert_live_item(&aggregate, "src/db.rs", "private_helper");
        assert_live_item(&aggregate, "src/imported.rs", "imported_command");
        assert_live_item(&aggregate, "src/db.rs", "imported_helper");
        assert_dead_item(&aggregate, "src/commands.rs", "planted_dead");
        assert_dead_item(&aggregate, "src/unimported.rs", "false_command");
        assert_dead_item(&aggregate, "src/db.rs", "false_helper");
    }

    #[test]
    fn dead_code_projection_rust_attribute_roots_survive_unrelated_incremental_edit() {
        use crate::data_layer::runtime::{DataCheckout, Fresh};
        let dir = tempfile::tempdir().expect("tempdir");
        write_rust_attribute_entry_fixture(dir.path());
        let root = canonical_root(dir.path());
        let storage = tempfile::tempdir().expect("storage");
        let checkout = DataCheckout::open_planes(
            &root,
            storage.path(),
            None,
            crate::data_layer::checkout::Planes {
                text: false,
                callgraph: true,
            },
        )
        .expect("bind checkout");
        assert!(matches!(
            checkout.fresh_view(Duration::from_secs(60)),
            Fresh::Ready(_)
        ));
        let changed = root.join("src/unrelated.rs");
        write_file(
            &changed,
            "// unrelated edit\npub fn unrelated() -> u32 { 2 }\n",
        );
        checkout.note_paths(&[changed]);
        let view = match checkout.fresh_view(Duration::from_secs(60)) {
            Fresh::Ready(view) => view,
            Fresh::Building => panic!("edited call graph did not publish"),
            Fresh::Failed(reason) => panic!("edited call graph failed: {reason}"),
        };
        let incremental = view.dead_code_snapshot().expect("snapshot");
        let aggregate = dead_code_aggregate(&root, project_files(&root), incremental);
        assert_live_item(&aggregate, "src/commands.rs", "get_primers");
        assert_live_item(&aggregate, "src/db.rs", "helper");
        assert_live_item(&aggregate, "src/db.rs", "private_helper");
        assert_dead_item(&aggregate, "src/commands.rs", "planted_dead");
    }

    fn fixture_callgraph_snapshot(root: &Path) -> CallgraphSnapshot {
        crate::callgraph_store::dead_code_projection::snapshot_for_test(root)
    }

    fn dead_code_aggregate(
        root: &Path,
        scope_files: Vec<PathBuf>,
        snapshot: CallgraphSnapshot,
    ) -> Value {
        let job = InspectJob {
            job_id: 86,
            key: JobKey::for_project_category(InspectCategory::DeadCode),
            category: InspectCategory::DeadCode,
            scope_files,
            project_root: root.to_path_buf(),
            inspect_dir: root.join(".aft-cache").join("inspect"),
            config: Arc::new(Config {
                project_root: Some(root.to_path_buf()),
                ..Config::default()
            }),
            symbol_cache: Arc::new(RwLock::new(SymbolCache::new())),
            inspect_writer: true,
            callgraph_writer: true,
            callgraph_snapshot: Some(Arc::new(snapshot)),
        };
        crate::inspect::scanners::dead_code::run_dead_code_scan(&job)
            .outcome
            .expect("dead_code scan succeeds")
            .aggregate
    }

    fn assert_dead_item(aggregate: &Value, file: &str, symbol: &str) {
        assert!(
            aggregate_has_item(aggregate, file, symbol),
            "expected {file}::{symbol} to be reported dead: {aggregate:#}"
        );
    }

    fn assert_live_item(aggregate: &Value, file: &str, symbol: &str) {
        assert!(
            !aggregate_has_item(aggregate, file, symbol),
            "expected {file}::{symbol} to be live/not reported dead: {aggregate:#}"
        );
    }

    fn aggregate_has_item(aggregate: &Value, file: &str, symbol: &str) -> bool {
        let Some(items) = aggregate.get("items").and_then(Value::as_array) else {
            return false;
        };
        items.iter().any(|item| {
            item.get("file").and_then(Value::as_str) == Some(file)
                && item.get("symbol").and_then(Value::as_str) == Some(symbol)
        })
    }

    fn project_files(root: &Path) -> Vec<PathBuf> {
        walk_project_files(root).collect()
    }

    fn canonical_root(root: &Path) -> PathBuf {
        std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf())
    }

    fn write_file(path: &Path, content: &str) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("create parent");
        }
        std::fs::write(path, content).expect("write fixture");
        bump_mtime(path);
    }

    fn bump_mtime(path: &Path) {
        let secs = NEXT_MTIME.fetch_add(1, AtomicOrdering::SeqCst);
        filetime::set_file_mtime(path, FileTime::from_unix_time(secs, 0)).expect("bump mtime");
    }

    fn write_projection_fixture(root: &Path) {
        write_file(
            &root.join("package.json"),
            r#"{"name":"dead-code-projection-fixture","type":"module","main":"src/main.ts"}"#,
        );
        write_file(
            &root.join("Cargo.toml"),
            r#"[package]
name = "dead_code_projection_fixture"
version = "0.1.0"
edition = "2021"
"#,
        );
        write_file(
            &root.join("src/main.ts"),
            r#"import runDefault from "./default";
import { knownLive } from "./live";
import { jsEntry } from "./app.js";

export function main() {
  dispatch();
  runDefault();
  jsEntry();
}

function dispatch() {
  knownLive();
  const service = { render() {} };
  service.render();
}
"#,
        );
        write_file(
            &root.join("src/default.ts"),
            r#"export default function runDefault() {}
"#,
        );
        write_file(
            &root.join("src/live.ts"),
            r#"export function knownLive() {}
"#,
        );
        write_file(
            &root.join("src/dead.ts"),
            r#"export function knownDead() {}
"#,
        );
        write_file(
            &root.join("src/render.ts"),
            r#"export function render() {}
"#,
        );
        write_file(
            &root.join("src/other_render.ts"),
            r#"export function render() {}
"#,
        );
        write_file(
            &root.join("src/app.js"),
            r#"import { jsHelper } from "./js_helper.js";

export function jsEntry() {
  jsHelper();
}
"#,
        );
        write_file(
            &root.join("src/js_helper.js"),
            r#"export function jsHelper() {}
"#,
        );
        write_file(
            &root.join("src/lib.rs"),
            r#"mod util;
use crate::util::rust_helper;

pub fn rust_entry() {
    rust_helper();
}
"#,
        );
        write_file(
            &root.join("src/util.rs"),
            r#"pub fn rust_helper() {}
"#,
        );
    }

    fn write_rust_attribute_entry_fixture(root: &Path) {
        write_file(
            &root.join("src/main.rs"),
            r#"mod commands;
mod db;
mod imported;
mod unimported;
mod unrelated;

fn main() {
    tauri::generate_handler![commands::get_primers, imported::imported_command];
}
"#,
        );
        write_file(
            &root.join("src/commands.rs"),
            r#"use crate::db;

#[tauri::command]
pub fn get_primers() -> String {
    db::helper()
}

pub fn planted_dead() -> String {
    "dead".to_string()
}

#[tauri::command]
fn private_command() -> String {
    db::private_helper()
}
"#,
        );
        write_file(
            &root.join("src/imported.rs"),
            r#"use crate::db;
use tauri::command;

#[command]
pub fn imported_command() -> String {
    db::imported_helper()
}
"#,
        );
        write_file(
            &root.join("src/unimported.rs"),
            r#"use crate::db;

#[command]
pub fn false_command() -> String {
    db::false_helper()
}
"#,
        );
        write_file(
            &root.join("src/db.rs"),
            r#"pub fn helper() -> String { "live".to_string() }
pub fn imported_helper() -> String { "live".to_string() }
pub fn private_helper() -> String { "live".to_string() }
pub fn false_helper() -> String { "dead".to_string() }
"#,
        );
        write_file(
            &root.join("src/unrelated.rs"),
            r#"pub fn unrelated() -> u32 { 1 }
"#,
        );
    }

    #[test]
    fn forced_paths_downgrade_only_when_strict_hash_matches_cached_fact() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = std::fs::canonicalize(dir.path()).expect("canonical root");
        let unchanged = root.join("unchanged.ts");
        let changed = root.join("changed.ts");
        let oversized = root.join("oversized.ts");
        std::fs::write(&unchanged, "export const value = 1;\n").expect("write unchanged");
        std::fs::write(&changed, "export const before = 1;\n").expect("write changed baseline");
        let unchanged_freshness =
            cache_freshness::collect(&unchanged).expect("unchanged freshness");
        let changed_freshness = cache_freshness::collect(&changed).expect("changed freshness");
        std::fs::write(&changed, "export const after_ = 2;\n").expect("change same-size content");
        let oversized_file = std::fs::File::create(&oversized).expect("create oversized");
        oversized_file
            .set_len(cache_freshness::CONTENT_HASH_SIZE_CAP + 1)
            .expect("size oversized");
        let oversized_freshness =
            cache_freshness::collect(&oversized).expect("oversized freshness");
        let cached = vec![
            CachedContributionFreshness {
                file_path: PathBuf::from("unchanged.ts"),
                freshness: unchanged_freshness,
            },
            CachedContributionFreshness {
                file_path: PathBuf::from("changed.ts"),
                freshness: changed_freshness,
            },
            CachedContributionFreshness {
                file_path: PathBuf::from("oversized.ts"),
                freshness: oversized_freshness,
            },
        ];

        let (remaining, downgraded) = downgrade_unchanged_forced_paths_with_freshness(
            &root,
            &cached,
            vec![
                PathBuf::from("unchanged.ts"),
                PathBuf::from("changed.ts"),
                PathBuf::from("oversized.ts"),
            ],
        );

        assert_eq!(downgraded, 1);
        assert_eq!(
            remaining,
            vec![PathBuf::from("changed.ts"), PathBuf::from("oversized.ts")]
        );
    }

    #[test]
    fn projection_estimator_and_verdict_report_only_projection_work() {
        let phases = Tier2PhaseTimings {
            snapshot: Duration::from_millis(2_620),
            projection_cost: Duration::from_millis(187),
            rollup: Duration::from_millis(3_186),
            projection: Some(ProjectionVerdict {
                kind: ProjectionKind::Spliced,
                reason: None,
                journal_bytes: 2_397,
                changed_files: 46,
            }),
            ..Tier2PhaseTimings::default()
        };

        assert_eq!(
            projection_estimator_duration(&phases),
            Duration::from_millis(187)
        );
        let line = phases.render(InspectCategory::DeadCode, Path::new("/root"), "test-key");
        assert!(line.contains("snapshot=2620ms projection_ms=187"));
        assert!(line.contains("rollup_ms=3186 projection="));
        assert!(line.contains("projection=spliced journal_bytes=2397 changed_files=46"));
    }

    #[test]
    fn every_tier2_phases_line_path_renders_projection_key() {
        for (category, reason) in [
            (InspectCategory::DeadCode, "no_callgraph"),
            (InspectCategory::DeadCode, "aggregate_reused"),
            (InspectCategory::DeadCode, "provided_snapshot"),
            (InspectCategory::UnusedExports, "not_required"),
            (InspectCategory::Duplicates, "not_required"),
            (InspectCategory::Cycles, "not_required"),
            (InspectCategory::Complexity, "not_required"),
        ] {
            let phases = Tier2PhaseTimings {
                projection_skip_reason: Some(reason),
                ..Tier2PhaseTimings::default()
            };
            let line = phases.render(category, Path::new("/root"), "test-key");
            assert!(
                line.contains(&format!(" projection=none reason={reason} ")),
                "{category} phases line omitted its projection verdict: {line}"
            );
            assert!(line.contains(" journal_bytes=0 changed_files=0 "));
        }
    }

    #[test]
    fn perf_tier2_phases_line_renders_each_projection_verdict() {
        let spliced = render_projection_suffix(ProjectionVerdict {
            kind: ProjectionKind::Spliced,
            reason: None,
            journal_bytes: 4096,
            changed_files: 3,
        });
        assert_eq!(
            spliced, " projection=spliced journal_bytes=4096 changed_files=3",
            "a spliced verdict must omit the reason field"
        );

        let cold = render_projection_suffix(ProjectionVerdict {
            kind: ProjectionKind::Full,
            reason: Some("cold"),
            journal_bytes: 0,
            changed_files: 0,
        });
        assert_eq!(
            cold,
            " projection=full reason=cold journal_bytes=0 changed_files=0"
        );

        let gap = render_projection_suffix(ProjectionVerdict {
            kind: ProjectionKind::Full,
            reason: Some("journal_gap"),
            journal_bytes: 0,
            changed_files: 0,
        });
        assert_eq!(
            gap,
            " projection=full reason=journal_gap journal_bytes=0 changed_files=0"
        );

        let costlier = render_projection_suffix(ProjectionVerdict {
            kind: ProjectionKind::Full,
            reason: Some("splice_costlier"),
            journal_bytes: 261_465,
            changed_files: 1_486,
        });
        assert_eq!(
            costlier,
            " projection=full reason=splice_costlier journal_bytes=261465 changed_files=1486"
        );

        let reused = render_projection_suffix(ProjectionVerdict {
            kind: ProjectionKind::Reused,
            reason: None,
            journal_bytes: 0,
            changed_files: 0,
        });
        assert_eq!(
            reused, " projection=reused journal_bytes=0 changed_files=0",
            "a cache hit is not a full projection and must not read as one"
        );
    }
}

#[cfg(test)]
mod tier2_deadline_tests {
    use super::*;

    #[test]
    fn tier2_pass_deadline_releases_limiter_while_ignoring_stub_still_runs() {
        let limiter = cold_build_limiter::isolated_limiter(1);
        let request = cold_build_limiter::ColdBuildAdmissionRequest::new(
            "tier2-timeout-test",
            cold_build_limiter::ColdBuildAdmissionClass::Maintenance,
        );
        let permit = cold_build_limiter::try_acquire_classified_with_limiter(&limiter, &request)
            .expect("test permit");
        let permit_slot = Arc::new(Mutex::new(Some(permit)));
        let (running_tx, running_rx) = bounded(1);
        let (release_tx, release_rx) = bounded::<()>(1);
        let root = tempfile::tempdir().expect("isolated deadline-test root");
        let handle = std::thread::spawn({
            let permit_slot = Arc::clone(&permit_slot);
            move || {
                run_tier2_pass_with_deadline(
                    root.path(),
                    InspectCategory::Duplicates,
                    Duration::from_millis(20),
                    Some(permit_slot),
                    || {
                        running_tx.send(()).expect("announce running stub");
                        // Keep the producer alive until the test has observed permit
                        // release, even if its assertion thread is descheduled.
                        let _ = release_rx.recv();
                        "ignored cancellation"
                    },
                )
            }
        });
        running_rx
            .recv_timeout(Duration::from_secs(30))
            .expect("stub started");
        let deadline = Instant::now() + Duration::from_secs(30);
        while !limiter.census().holders.is_empty() {
            assert!(
                Instant::now() < deadline,
                "deadline did not release limiter slot"
            );
            std::thread::yield_now();
        }
        assert!(
            !handle.is_finished(),
            "stub must still be running when slot is released"
        );
        release_tx.send(()).expect("release ignoring stub");
        let (value, timed_out) = handle.join().expect("stub thread");
        assert!(timed_out);
        assert_eq!(value, "ignored cancellation");
    }

    #[test]
    fn tier2_pass_deadline_cooperative_stub_observes_cancellation() {
        let limiter = cold_build_limiter::isolated_limiter(1);
        let permit = limiter.try_acquire().expect("test permit");
        let root = tempfile::tempdir().expect("existing deadline-test root");
        let (value, timed_out) = run_tier2_pass_with_deadline(
            root.path(),
            InspectCategory::Duplicates,
            Duration::from_millis(20),
            Some(Arc::new(Mutex::new(Some(permit)))),
            || {
                let hang_ceiling = Instant::now() + Duration::from_secs(30);
                while !crate::executor::current_job_cancelled() {
                    assert!(Instant::now() < hang_ceiling, "stub was never cancelled");
                    std::thread::sleep(Duration::from_millis(1));
                }
                "cancelled"
            },
        );
        assert!(timed_out, "deadline must own the cancellation request");
        assert_eq!(value, "cancelled");
        assert!(limiter.census().holders.is_empty());
    }
}

#[cfg(test)]
mod tier2_unreadable_source_tests {
    use super::*;
    use crate::config::Config;
    use crate::inspect::scanners::source_text::{source_read_count_for_debug, NOT_VALID_UTF8};
    use crate::parser::SymbolCache;
    use std::sync::RwLock;

    /// Eleven decisions: one complexity hotspot, and a block long enough to
    /// be reported as a duplicate when it appears twice.
    fn complex_function(name: &str) -> String {
        let mut source = format!("export function {name}(a: number): number {{\n");
        for index in 1..=10 {
            source.push_str(&format!(
                "  if (a > {index}) {{\n    return {index};\n  }}\n"
            ));
        }
        source.push_str("  return 0;\n}\n");
        source
    }

    fn fixture() -> (tempfile::TempDir, PathBuf, InspectSnapshot) {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = std::fs::canonicalize(dir.path()).expect("canonical fixture root");
        std::fs::create_dir_all(root.join("src")).expect("src dir");
        std::fs::create_dir_all(root.join("data")).expect("data dir");
        std::fs::write(root.join("src/good.ts"), complex_function("decide")).expect("good");
        std::fs::write(root.join("src/copy.ts"), complex_function("choose")).expect("copy");
        // GBK-encoded bytes: valid JSON grammar, not valid UTF-8.
        std::fs::write(root.join("data/bad.json"), b"{\"k\":\"\xb9\xe6\xce\"}").expect("bad json");
        // An analyzable source with the same GBK bytes in a string literal.
        std::fs::write(
            root.join("src/bad.ts"),
            b"export const label = \"\xb9\xe6\xce\";\n",
        )
        .expect("bad ts");
        let project_key = crate::search_index::artifact_cache_key(&root);
        crate::root_cache::configure_artifact_access(&root, &project_key, false);
        let snapshot = InspectSnapshot::new_with_capabilities(
            root.clone(),
            root.join(".aft-cache").join("inspect"),
            Arc::new(Config {
                project_root: Some(root.clone()),
                ..Config::default()
            }),
            Arc::new(RwLock::new(SymbolCache::new())),
            true,
            true,
        );
        (dir, root, snapshot)
    }

    fn aggregate(
        manager: &InspectManager,
        snapshot: &InspectSnapshot,
        category: InspectCategory,
    ) -> Value {
        manager
            .tier2_run_with_reuse_result(snapshot.clone(), category, None)
            .outcome
            .unwrap_or_else(|message| panic!("{category} build failed: {message}"))
            .aggregate
    }

    #[test]
    fn non_utf8_source_is_a_named_skip_reused_until_it_changes_then_included() {
        let (_dir, root, snapshot) = fixture();
        let bad_ts = root.join("src/bad.ts");
        let bad_json = root.join("data/bad.json");
        let manager = InspectManager::new();
        let named_skip = json!([{ "file": "src/bad.ts", "reason": NOT_VALID_UTF8 }]);

        let complexity = aggregate(&manager, &snapshot, InspectCategory::Complexity);
        assert_eq!(complexity["complete"], json!(false), "{complexity}");
        assert_eq!(complexity["skipped_files"], named_skip, "{complexity}");
        assert_eq!(complexity["skipped_files_count"], json!(1));
        assert_eq!(complexity["count"], json!(2), "{complexity}");
        assert!(complexity["items"]
            .as_array()
            .is_some_and(|items| items.iter().any(|item| item["file"] == "src/good.ts")));

        let duplicates = aggregate(&manager, &snapshot, InspectCategory::Duplicates);
        assert_eq!(duplicates["complete"], json!(false), "{duplicates}");
        assert_eq!(duplicates["skipped_files"], named_skip, "{duplicates}");
        assert_eq!(duplicates["skipped_files_count"], json!(1));
        assert!(
            duplicates["count"].as_u64().is_some_and(|count| count >= 1),
            "the two valid files still produce a duplicate group: {duplicates}"
        );

        assert_eq!(
            source_read_count_for_debug(&bad_json),
            0,
            "neither scanner analyzes JSON, so the GBK data file is never read as source"
        );
        assert_eq!(source_read_count_for_debug(&bad_ts), 2);

        // Unchanged inputs: the cached skip is reused and the file is not
        // read again.
        let complexity_again = aggregate(&manager, &snapshot, InspectCategory::Complexity);
        let duplicates_again = aggregate(&manager, &snapshot, InspectCategory::Duplicates);
        assert_eq!(complexity_again, complexity);
        assert_eq!(duplicates_again, duplicates);
        assert_eq!(source_read_count_for_debug(&bad_ts), 2);

        // The file becomes valid UTF-8 (a different size, so it is stale).
        std::fs::write(&bad_ts, "export const label = \"\u{89c4}\u{8303}\";\n")
            .expect("transcode bad ts");
        let complexity_fixed = aggregate(&manager, &snapshot, InspectCategory::Complexity);
        let duplicates_fixed = aggregate(&manager, &snapshot, InspectCategory::Duplicates);
        for fixed in [&complexity_fixed, &duplicates_fixed] {
            assert!(fixed.get("skipped_files").is_none(), "{fixed}");
            assert!(fixed.get("skipped_files_count").is_none(), "{fixed}");
        }
        assert_eq!(
            complexity_fixed["complete"],
            json!(true),
            "{complexity_fixed}"
        );
        assert_eq!(source_read_count_for_debug(&bad_ts), 4);
    }

    #[test]
    fn scoped_payload_drops_skips_outside_the_scope() {
        let payload = json!({
            "count": 0,
            "items": [],
            "complete": false,
            "skipped_files": [{ "file": "src/bad.ts", "reason": NOT_VALID_UTF8 }],
            "skipped_files_count": 1,
        });
        let root = tempfile::tempdir().expect("root");
        let elsewhere = filter_payload_for_scope(
            payload.clone(),
            &JobScope::from_roots(root.path().to_path_buf(), vec![root.path().join("lib")]),
        );
        assert!(elsewhere.get("skipped_files").is_none(), "{elsewhere}");
        assert_eq!(elsewhere["complete"], json!(true), "{elsewhere}");

        let touching = filter_payload_for_scope(
            payload,
            &JobScope::from_roots(root.path().to_path_buf(), vec![root.path().join("src")]),
        );
        assert_eq!(touching["skipped_files_count"], json!(1), "{touching}");
        assert_eq!(touching["complete"], json!(false), "{touching}");
    }

    #[test]
    fn writer_lease_release_native_watch_acknowledges_retry() {
        let (_dir, root, snapshot) = fixture();
        let project_dir = snapshot
            .inspect_dir
            .join(crate::path_identity::project_scope_key(&root));
        std::fs::create_dir_all(&project_dir).unwrap();
        let held = crate::fs_lock::try_acquire(
            &crate::root_cache::writer_lease_path(&project_dir),
            Duration::ZERO,
        )
        .unwrap();
        let manager = InspectManager::new();
        let (events, received) = crossbeam_channel::unbounded();
        *manager.cache_writer_watch_observer.lock().unwrap() = Some(events);
        let error = manager
            .cache_for_paths(snapshot.inspect_dir.clone(), root.clone())
            .err()
            .expect("live writer excludes the cache open");
        assert!(error.contains("writer_lease_timeout:"), "{error}");
        let category = InspectCategory::Complexity;
        let key = JobKey::for_project_category(category);
        manager.record_flight_start(&key);
        manager.record_builder_attempt_outcome(&key, &JobOutcome::Failed { message: error });
        assert_eq!(
            manager.tier2_builder_state(category),
            InspectBuilderState::WaitingForCacheWriterLease
        );
        let mut active = false;
        loop {
            match received
                .recv_timeout(Duration::from_secs(5))
                .expect("watch subscription acknowledgement")
            {
                CacheWriterWatchObservation::Subscribed => break,
                CacheWriterWatchObservation::Failed(error) => {
                    panic!("native watch subscription failed: {error}")
                }
                CacheWriterWatchObservation::Activity { .. } => active = true,
            }
        }
        // Prove the OS stream is delivering before releasing the lease. Merely
        // constructing a watcher does not establish that event ordering.
        std::fs::write(project_dir.join(".readiness-probe"), b"ready").unwrap();
        while !active {
            match received
                .recv_timeout(Duration::from_secs(5))
                .expect("native watch activity acknowledgement")
            {
                CacheWriterWatchObservation::Activity { .. } => active = true,
                CacheWriterWatchObservation::Failed(error) => {
                    panic!("native watch failed: {error}")
                }
                CacheWriterWatchObservation::Subscribed => {}
            }
        }
        assert_eq!(
            manager.tier2_builder_state(category),
            InspectBuilderState::WaitingForCacheWriterLease,
            "activity must not release a live holder"
        );
        drop(held);
        assert!(
            !crate::root_cache::writer_lease_path(&project_dir).exists(),
            "held lease must have been removed"
        );
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut activity = Vec::new();
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            match received
                .recv_timeout(remaining)
                .unwrap_or_else(|error| panic!("native lease release callback acknowledgement: {error:?}; observed={activity:?}"))
            {
                CacheWriterWatchObservation::Activity { release: true, detail } => {
                    eprintln!("acknowledged lease release: {detail}");
                    break;
                }
                CacheWriterWatchObservation::Activity { detail, .. } => activity.push(detail),
                CacheWriterWatchObservation::Failed(error) => {
                    panic!("native watch failed: {error}")
                }
                _ => {}
            }
        }
    }

    #[test]
    fn tier2_terminal_state_is_published_before_flight_is_removed() {
        let manager = InspectManager::new();
        let key = JobKey::for_project_category(InspectCategory::Complexity);
        manager
            .in_flight
            .lock()
            .unwrap()
            .insert(key.clone(), InFlightRegistration::new(Vec::new()));
        let mut published = false;
        let waiters = manager.take_waiters_after(&key, None, || {
            assert!(
                matches!(
                    manager.in_flight.try_lock(),
                    Err(std::sync::TryLockError::WouldBlock)
                ),
                "flight removal must remain locked until publication completes"
            );
            published = true;
        });
        assert!(published);
        assert!(waiters.is_some());
        assert!(!manager.tier2_any_in_flight());
    }
}
