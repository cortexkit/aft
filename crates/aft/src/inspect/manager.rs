use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crossbeam_channel::{after, bounded, select_biased, Receiver, Sender};
use serde::Deserialize;
use serde_json::{json, Value};

use super::cache::{InspectCache, InspectCacheRead, InspectDbTimings, Tier2ContributionUpdates};
use super::dispatch::{default_worker, start_dispatch_loop, InspectWorker};
use super::freshness::{verify_contribution_file, ContributionFreshness};
use super::job::{
    is_test_file, CallgraphSnapshot, FileContribution, InspectCategory, InspectJob, InspectResult,
    InspectScanSuccess, InspectSnapshot, JobKey, JobOutcome, JobScope, PendingWaitCause,
};
use super::oxc_engine::LivenessVerdict;
use super::oxc_engine::{
    analyze_file_facts, analyze_files_with_cache, normalize_input_path, AnalyzeOptions,
    DynamicImportFact, ExportFact, FileFacts, FileId, ImportFact, OxcEngineResult, OxcFactsCache,
    ReExportFact, FACTS_FORMAT_VERSION, OXC_PROVENANCE,
};
use crate::cache_freshness::{self, FileFreshness, FreshnessVerdict};
#[cfg(test)]
use crate::callgraph_store::project_dead_code_snapshot;
use crate::callgraph_store::{
    project_dead_code_snapshot_with_revision, CallGraphStore, CallGraphStoreError,
    ReadonlyCallGraphStore,
};
use crate::cold_build_limiter;

const DEFAULT_SOFT_DEADLINE: Duration = Duration::from_secs(1);

type WaiterTx = Sender<JobOutcome>;

#[derive(Clone)]
struct Waiter {
    tx: WaiterTx,
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
}

#[derive(Debug, Clone)]
pub struct Tier2RunSubmissionError {
    pub category: InspectCategory,
    pub message: String,
}

#[derive(Debug, Clone, Default)]
pub struct Tier2RunSubmission {
    pub queued_categories: Vec<InspectCategory>,
    pub newly_queued_categories: Vec<InspectCategory>,
    pub deferred_categories: Vec<InspectCategory>,
    pub errors: Vec<Tier2RunSubmissionError>,
}

impl Tier2RunSubmission {
    pub fn has_new_work(&self) -> bool {
        !self.newly_queued_categories.is_empty()
    }
}

#[derive(Debug, Clone)]
struct Tier2ReuseOptions {
    force_rescan_paths: BTreeSet<PathBuf>,
    allow_callgraph_cold_build: bool,
    require_callgraph_snapshot: bool,
    interactive: bool,
}

impl Tier2ReuseOptions {
    fn has_force_paths(&self) -> bool {
        !self.force_rescan_paths.is_empty()
    }
}

impl Default for Tier2ReuseOptions {
    fn default() -> Self {
        Self {
            force_rescan_paths: BTreeSet::new(),
            allow_callgraph_cold_build: true,
            require_callgraph_snapshot: false,
            interactive: false,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum InspectBuilderState {
    Building,
    QueuedBehindColdBuilds,
    GatedBySemanticSeed,
    Suspended,
    BuildDenied,
    Absent,
}

impl InspectBuilderState {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Building => "building",
            Self::QueuedBehindColdBuilds => "queued_behind_cold_builds",
            Self::GatedBySemanticSeed => "gated_by_semantic_seed",
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

    fn record_failure(&mut self, terminal: String) {
        self.state = None;
        self.suspension = None;
        self.attempt_count = self.attempt_count.saturating_add(1);
        self.last_failure = Some(terminal);
    }

    fn record_suspension(&mut self, suspension: crate::build_breaker::BuildSuspension) {
        self.state = Some(InspectBuilderState::Suspended);
        self.last_failure = None;
        self.suspension = Some(suspension);
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

enum BuilderAttemptTerminal {
    Succeeded,
    Failed(String),
    Inconclusive,
}

fn builder_attempt_terminal(outcome: &JobOutcome) -> BuilderAttemptTerminal {
    match outcome {
        JobOutcome::Fresh { payload } if callgraph_unavailable_payload(payload) => {
            BuilderAttemptTerminal::Failed("callgraph_unavailable".to_string())
        }
        JobOutcome::Fresh { .. } => BuilderAttemptTerminal::Succeeded,
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

fn callgraph_store_ready_for_dead_code(callgraph_dir: PathBuf, project_root: PathBuf) -> bool {
    match CallGraphStore::open_readonly(callgraph_dir, project_root) {
        Ok(Some(store)) => store
            .stale_files()
            .ok()
            .is_some_and(|files| files.is_empty()),
        _ => false,
    }
}

/// Completes the builder registration and sends `Failed` to leftover waiters
/// if a reuse worker returns, panics, or is cancelled without going through the
/// completion router. Admission and exit must be paired; a leftover registration
/// reads as `building` with no running job.
struct Tier2FlightExitGuard<'a> {
    manager: &'a InspectManager,
    key: JobKey,
}

impl Drop for Tier2FlightExitGuard<'_> {
    fn drop(&mut self) {
        self.manager.finish_tier2_flight(
            &self.key,
            JobOutcome::Failed {
                message: "tier2 reuse worker exited without publishing a result".to_string(),
            },
        );
    }
}

fn cached_tier2_aggregate_usable(
    category: InspectCategory,
    options: &Tier2ReuseOptions,
    aggregate: &Value,
) -> bool {
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
    request_tx: Sender<InspectJob>,
    result_rx: Receiver<InspectResult>,
    #[allow(dead_code)]
    pool: Arc<rayon::ThreadPool>,
    in_flight: Mutex<HashMap<JobKey, Vec<Waiter>>>,
    in_flight_changed: Condvar,
    caches: Mutex<HashMap<InspectCacheIdentity, Arc<InspectCache>>>,
    /// One root-scoped dead-code graph projection. It is cleared with the
    /// manager's other idle artifacts rather than on a separate timer.
    callgraph_projection: Mutex<Option<CachedCallgraphProjection>>,
    oxc_facts_cache: Mutex<OxcFactsCache>,
    soft_deadline: Duration,
    next_job_id: AtomicU64,
    heavy_root_work_allowed: Arc<AtomicBool>,
    semantic_cold_seed_active: Arc<AtomicBool>,
    cold_build_limiter: Mutex<Arc<cold_build_limiter::ColdBuildLimiter>>,
    /// Inspect refusals (`builder_state=...`) and health's `tier2` field both
    /// read this registry. The waiter map (`in_flight`) fans out completions;
    /// both surfaces treat a category as busy when it has an entry here, and
    /// fall back to the waiter map if the registry is empty.
    builder_states: Mutex<HashMap<JobKey, BuilderStateEntry>>,
    automatic_tier2_refresh_allowed: AtomicBool,
    automatic_tier2_skip_logged: AtomicBool,
    automatic_tier2_schedule_count: AtomicU64,
    /// Monotonic count of Tier-2 completions delivered via the reuse path
    /// (watcher-driven scheduler runs). These bypass `result_rx`/
    /// `drain_completions`, so the `&AppContext`-side drain polls this counter
    /// to know when to refresh the agent status bar after a background scan.
    reuse_completions: AtomicU64,
    /// Test observability for distinguishing queued reuse work from a worker that
    /// has actually begun executing it.
    reuse_starts: AtomicU64,
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
        let handles = start_dispatch_loop(worker);
        Self {
            request_tx: handles.request_tx,
            result_rx: handles.result_rx,
            pool: handles.pool,
            in_flight: Mutex::new(HashMap::new()),
            in_flight_changed: Condvar::new(),
            caches: Mutex::new(HashMap::new()),
            callgraph_projection: Mutex::new(None),
            oxc_facts_cache: Mutex::new(OxcFactsCache::new()),
            soft_deadline,
            next_job_id: AtomicU64::new(1),
            heavy_root_work_allowed,
            semantic_cold_seed_active,
            cold_build_limiter: Mutex::new(cold_build_limiter::global_limiter()),
            builder_states: Mutex::new(HashMap::new()),
            automatic_tier2_refresh_allowed: AtomicBool::new(true),
            automatic_tier2_skip_logged: AtomicBool::new(false),
            automatic_tier2_schedule_count: AtomicU64::new(0),
            reuse_completions: AtomicU64::new(0),
            reuse_starts: AtomicU64::new(0),
        }
    }

    fn heavy_root_work_allowed(&self) -> bool {
        self.heavy_root_work_allowed.load(Ordering::SeqCst)
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
    }

    fn tier2_flight_exit_guard(&self, key: JobKey) -> Tier2FlightExitGuard<'_> {
        Tier2FlightExitGuard { manager: self, key }
    }

    /// Record the attempt outcome and wake leftover waiters. Idempotent: a
    /// second call after the completion router already ran is a no-op.
    fn finish_tier2_flight(&self, key: &JobKey, outcome: JobOutcome) {
        let Some(waiters) = self.take_waiters(key) else {
            return;
        };
        self.record_builder_attempt_outcome(key, &outcome);
        self.reuse_completions.fetch_add(1, Ordering::SeqCst);
        Self::deliver_waiters(waiters, outcome);
    }

    fn take_waiters(&self, key: &JobKey) -> Option<Vec<Waiter>> {
        let waiters = self
            .in_flight
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(key);
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

    fn record_tier2_build_suspension(
        &self,
        key: &JobKey,
        suspension: crate::build_breaker::BuildSuspension,
    ) {
        if let Ok(mut states) = self.builder_states.lock() {
            if let Some(entry) = states.get_mut(key) {
                entry.record_suspension(suspension);
            } else {
                let mut entry = BuilderStateEntry::new(InspectBuilderState::Suspended);
                entry.record_suspension(suspension);
                states.insert(key.clone(), entry);
            }
        }
    }

    #[cfg(test)]
    pub(crate) fn record_tier2_build_suspension_for_test(
        &self,
        category: InspectCategory,
        suspension: crate::build_breaker::BuildSuspension,
    ) {
        self.record_tier2_build_suspension(&JobKey::for_project_category(category), suspension);
    }

    fn builder_state_detail_for_job(&self, job: &InspectJob) -> String {
        if !job.inspect_writer || !job.callgraph_writer {
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

    /// Whether a published callgraph store can back a dead_code snapshot.
    ///
    /// This is the same readiness predicate the builder uses before projection:
    /// a store that opens but still has `backend_file_state='stale'` rows is
    /// not ready, because `project_dead_code_snapshot` refuses those rows.
    pub(crate) fn callgraph_ready_for_snapshot(&self, snapshot: &InspectSnapshot) -> bool {
        if !snapshot.config.callgraph_store {
            return false;
        }
        callgraph_store_dirs_from_inspect_dir(&snapshot.inspect_dir, &snapshot.project_root)
            .into_iter()
            .any(|dir| callgraph_store_ready_for_dead_code(dir, snapshot.project_root.clone()))
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
            crate::slog_debug!("automatic Tier-2 scan scheduling skipped for linked worktree root");
        }
        allowed
    }

    #[doc(hidden)]
    pub fn inspect_pool_for_test(&self) -> Arc<rayon::ThreadPool> {
        Arc::clone(&self.pool)
    }

    #[doc(hidden)]
    pub fn automatic_tier2_schedule_count_for_test(&self) -> u64 {
        self.automatic_tier2_schedule_count.load(Ordering::SeqCst)
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

    pub fn submit_tier2_run_with_reuse_background(
        self: &Arc<Self>,
        snapshot: InspectSnapshot,
        category: InspectCategory,
    ) -> Result<Option<JobKey>, String> {
        if !category.is_active() {
            return Err(format!(
                "inspect category '{category}' is disabled in v0.33"
            ));
        }
        if !category.is_tier2() {
            return Err(format!(
                "inspect category '{category}' is not a Tier 2 category"
            ));
        }
        if !self.heavy_root_work_allowed() {
            return Err(Self::heavy_root_work_block_message(category));
        }
        if !self.automatic_tier2_refresh_allowed() {
            return Ok(None);
        }
        self.automatic_tier2_schedule_count
            .fetch_add(1, Ordering::SeqCst);

        let job = self.tier2_reuse_job(snapshot, category, None);
        let key = job.key.clone();
        let mut in_flight = self
            .in_flight
            .lock()
            .map_err(|_| "inspect in-flight map lock poisoned".to_string())?;
        if in_flight.contains_key(&key) {
            return Ok(Some(key));
        }
        let limiter = self.cold_build_limiter();
        let request = cold_build_limiter::ColdBuildAdmissionRequest::new(
            format!("tier2-background:{}", category.as_str()),
            cold_build_limiter::ColdBuildAdmissionClass::Maintenance,
        );
        let Some(permit) =
            cold_build_limiter::try_acquire_classified_with_limiter(&limiter, &request)
        else {
            return Err(format!(
                "cold build concurrency limit ({}) reached; retrying later",
                limiter.limit()
            ));
        };
        in_flight.insert(key.clone(), Vec::new());
        drop(in_flight);
        self.record_flight_start(&key);

        let manager = Arc::clone(self);
        let pool = Arc::clone(&self.pool);
        pool.spawn_fifo(move || {
            let _permit = permit;
            let _flight = manager.tier2_flight_exit_guard(job.key.clone());
            let result =
                manager.tier2_run_with_reuse_job_result_catching(job, Tier2ReuseOptions::default());
            manager.route_tier2_reuse_completion(result);
        });

        Ok(Some(key))
    }

    pub fn submit_tier2_run_with_reuse_serial_background(
        self: &Arc<Self>,
        snapshot: InspectSnapshot,
        categories: Vec<InspectCategory>,
    ) -> Tier2RunSubmission {
        let mut submission = Tier2RunSubmission::default();
        let mut requested = Vec::new();

        for category in categories {
            if !category.is_active() {
                submission.errors.push(Tier2RunSubmissionError {
                    category,
                    message: format!("inspect category '{category}' is disabled in v0.33"),
                });
                continue;
            }
            if !category.is_tier2() {
                submission.errors.push(Tier2RunSubmissionError {
                    category,
                    message: format!("inspect category '{category}' is not a Tier 2 category"),
                });
                continue;
            }
            requested.push(category);
        }

        if requested.is_empty() {
            return submission;
        }
        if !self.heavy_root_work_allowed() {
            for category in requested {
                submission.errors.push(Tier2RunSubmissionError {
                    category,
                    message: Self::heavy_root_work_block_message(category),
                });
            }
            return submission;
        }
        if !self.automatic_tier2_refresh_allowed() {
            return submission;
        }
        self.automatic_tier2_schedule_count
            .fetch_add(requested.len() as u64, Ordering::SeqCst);

        let mut in_flight = match self.in_flight.lock() {
            Ok(in_flight) => in_flight,
            Err(_) => {
                for category in requested {
                    submission.errors.push(Tier2RunSubmissionError {
                        category,
                        message: "inspect in-flight map lock poisoned".to_string(),
                    });
                }
                return submission;
            }
        };

        let mut started = Vec::new();
        for category in requested {
            let key = JobKey::for_project_category(category);
            submission.queued_categories.push(category);
            if in_flight.contains_key(&key) {
                continue;
            }
            in_flight.insert(key.clone(), Vec::new());
            started.push(key);
            submission.newly_queued_categories.push(category);
        }
        drop(in_flight);
        for key in &started {
            self.record_flight_start(key);
        }

        if submission.newly_queued_categories.is_empty() {
            return submission;
        }

        let limiter = self.cold_build_limiter();
        let request = cold_build_limiter::ColdBuildAdmissionRequest::new(
            "tier2-serial-background",
            cold_build_limiter::ColdBuildAdmissionClass::Maintenance,
        );
        let Some(permit) =
            cold_build_limiter::try_acquire_classified_with_limiter(&limiter, &request)
        else {
            let deferred = submission.newly_queued_categories.clone();
            if let Ok(mut in_flight) = self.in_flight.lock() {
                for category in &deferred {
                    in_flight.remove(&JobKey::for_project_category(*category));
                }
            }
            for category in &deferred {
                self.clear_builder_state(&JobKey::for_project_category(*category));
            }
            submission
                .queued_categories
                .retain(|category| !deferred.contains(category));
            submission.deferred_categories = deferred;
            submission.newly_queued_categories.clear();
            return submission;
        };

        let categories_for_worker = submission.newly_queued_categories.clone();
        let manager = Arc::clone(self);
        let pool = Arc::clone(&self.pool);
        pool.spawn_fifo(move || {
            let _permit = permit;
            for category in categories_for_worker {
                let job = manager.tier2_reuse_job(snapshot.clone(), category, None);
                let _flight = manager.tier2_flight_exit_guard(job.key.clone());
                let result = manager
                    .tier2_run_with_reuse_job_result_catching(job, Tier2ReuseOptions::default());
                manager.route_tier2_reuse_completion(result);
            }
        });

        submission
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
        if let Ok(mut caches) = self.caches.lock() {
            caches.clear();
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

    fn cached_callgraph_projection(
        &self,
        identity: &CallgraphProjectionIdentity,
    ) -> Option<Arc<CallgraphSnapshot>> {
        let projection = self.callgraph_projection.lock().ok()?;
        projection
            .as_ref()
            .filter(|cached| cached.identity == *identity)
            .map(|cached| Arc::clone(&cached.snapshot))
    }

    fn cache_callgraph_projection(
        &self,
        identity: CallgraphProjectionIdentity,
        snapshot: Arc<CallgraphSnapshot>,
    ) {
        let estimated_bytes = estimate_callgraph_snapshot_bytes(snapshot.as_ref());
        if let Ok(mut cached) = self.callgraph_projection.lock() {
            *cached = Some(CachedCallgraphProjection {
                identity,
                snapshot,
                estimated_bytes,
            });
        }
    }

    fn clear_callgraph_projection(&self) {
        if let Ok(mut cached) = self.callgraph_projection.lock() {
            cached.take();
        }
    }

    fn build_tier2_callgraph_snapshot_with_refresh(
        &self,
        job: &InspectJob,
        allow_cold_build: bool,
        build_if_missing: bool,
        refresh_paths: &[PathBuf],
    ) -> Option<Arc<CallgraphSnapshot>> {
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
            if let Some(waiters) = self.take_waiters(&result.key) {
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

    /// Latest persisted counts for the three Tier-2 categories, in
    /// `(dead_code, unused_exports, duplicates)` order. Reads the most recent
    /// aggregate regardless of contribution-hash freshness (last-known), so the
    /// agent status bar can refresh after a background scan completes without a
    /// freshness round-trip. A category with no readable aggregate reports
    /// `None` (never a fabricated `0`), so the status bar can preserve any
    /// last-known value and stay suppressed until every category is real (#1).
    pub fn latest_tier2_counts(
        &self,
        inspect_dir: PathBuf,
        project_root: PathBuf,
    ) -> (Option<usize>, Option<usize>, Option<usize>) {
        let Ok(cache) = self.cache_for_paths(inspect_dir, project_root) else {
            return (None, None, None);
        };
        let count_of = |category: InspectCategory| -> Option<usize> {
            cache
                .latest_aggregate_any_hash(category)
                .ok()
                .flatten()
                .and_then(|payload| {
                    if category == InspectCategory::DeadCode
                        && payload
                            .get("callgraph_available")
                            .and_then(serde_json::Value::as_bool)
                            == Some(false)
                    {
                        return None;
                    }
                    payload
                        .get("count")
                        .and_then(serde_json::Value::as_u64)
                        .map(|count| count as usize)
                })
        };
        (
            count_of(InspectCategory::DeadCode),
            count_of(InspectCategory::UnusedExports),
            count_of(InspectCategory::Duplicates),
        )
    }

    /// Whether the latest persisted dead_code aggregate reported
    /// `callgraph_available:false` — i.e. dead_code was suppressed because the
    /// callgraph store was not ready when it scanned. Health uses this to avoid
    /// reporting tier2 as permanently "building" for a root whose only missing
    /// category is dead_code blocked on the callgraph store. Mirrors the
    /// suppression rule in [`Self::latest_tier2_counts`].
    pub fn dead_code_blocked_on_callgraph(
        &self,
        inspect_dir: PathBuf,
        project_root: PathBuf,
    ) -> bool {
        let Ok(cache) = self.cache_for_paths(inspect_dir, project_root) else {
            return false;
        };
        cache
            .latest_aggregate_any_hash(InspectCategory::DeadCode)
            .ok()
            .flatten()
            .and_then(|payload| {
                payload
                    .get("callgraph_available")
                    .and_then(serde_json::Value::as_bool)
            })
            == Some(false)
    }

    pub fn cache_for_paths(
        &self,
        inspect_dir: PathBuf,
        project_root: PathBuf,
    ) -> Result<Arc<InspectCache>, String> {
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
        let mut caches = self
            .caches
            .lock()
            .map_err(|_| "inspect manager cache map lock poisoned".to_string())?;
        if let Some(cache) = caches.get(&identity) {
            return Ok(Arc::clone(cache));
        }
        let cache = Arc::new(
            InspectCache::open(inspect_dir, project_root)
                .map_err(|error| format!("failed to open inspect cache: {error}"))?,
        );
        caches.insert(identity, Arc::clone(&cache));
        Ok(cache)
    }

    fn oxc_result_for_scan(
        &self,
        job: &InspectJob,
        files: &[PathBuf],
        force_reparse_files: &[PathBuf],
    ) -> Result<Option<OxcEngineResult>, String> {
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
        analyze_files_with_cache(&job.project_root, files, options, &mut cache)
            .map(Some)
            .map_err(|message| format!("oxc analyze failed: {message}"))
    }

    pub fn tier2_run_with_reuse(
        &self,
        snapshot: InspectSnapshot,
        category: InspectCategory,
        caller_scope: JobScope,
        callgraph_snapshot: Option<Arc<CallgraphSnapshot>>,
    ) -> JobOutcome {
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
            let result =
                self.tier2_run_with_reuse_job_result_catching(job, Tier2ReuseOptions::default());
            self.route_tier2_reuse_completion(result);
        }

        match waiter_rx.recv() {
            Ok(outcome) => filter_outcome_for_scope_with_contributions(
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
        if let Some(waiters) = in_flight.get_mut(key) {
            waiters.push(Waiter { tx: waiter_tx });
            self.in_flight_changed.notify_all();
            return Ok(false);
        }

        in_flight.insert(key.clone(), vec![Waiter { tx: waiter_tx }]);
        drop(in_flight);
        self.record_flight_start(key);
        Ok(true)
    }

    fn wait_for_tier2_reuse_waiter_for_debug(&self, job: &InspectJob) {
        #[cfg(not(debug_assertions))]
        let _ = job;
        #[cfg(debug_assertions)]
        {
            const WAIT_ROOT_ENV: &str = "AFT_TEST_TIER2_REUSE_WAIT_FOR_WAITER_ROOT";
            if std::env::var_os(WAIT_ROOT_ENV).is_none()
                || !env_project_root_matches(WAIT_ROOT_ENV, &job.project_root)
            {
                return;
            }

            // This test gate releases on the actual waiter registration, not elapsed
            // wall-clock time, so a queued background job cannot finish before the
            // direct-reuse request has attached on a contended runner.
            let deadline = Instant::now() + Duration::from_secs(30);
            let mut in_flight = self
                .in_flight
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            loop {
                match in_flight.get(&job.key) {
                    Some(waiters) if waiters.is_empty() => {}
                    _ => return,
                }
                let now = Instant::now();
                if now >= deadline {
                    return;
                }
                let (next, wait_result) = self
                    .in_flight_changed
                    .wait_timeout(in_flight, deadline.saturating_duration_since(now))
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                in_flight = next;
                if wait_result.timed_out() {
                    return;
                }
            }
        }
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
            let result = manager.tier2_run_with_reuse_job_result_catching(job, options);
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
            Ok(outcome) => filter_outcome_for_scope_with_contributions(
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
    /// command path; Tier 2 scans run via the watcher-driven scheduler or the
    /// compatibility `aft_inspect_tier2_run` command.
    pub fn tier2_read_cached(
        &self,
        snapshot: InspectSnapshot,
        category: InspectCategory,
        caller_scope: JobScope,
    ) -> JobOutcome {
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
                        JobOutcome::Fresh { payload },
                        snapshot,
                        category,
                        cache,
                        caller_scope,
                    ),
                    Ok(false) => filter_outcome_for_scope_with_contributions(
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

        let mut records_match = true;
        for record in &cached_records {
            let absolute = if record.file_path.is_absolute() {
                record.file_path.clone()
            } else {
                snapshot.project_root.join(&record.file_path)
            };
            match verify_contribution_file(&absolute, &record.freshness) {
                ContributionFreshness::Fresh { .. } => {}
                ContributionFreshness::Stale | ContributionFreshness::Deleted => {
                    records_match = false;
                }
            }
        }

        Ok(records_match
            && current_by_relative.len() == cached_relative.len()
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
        self.tier2_run_with_reuse_job_result_with_options(job, Tier2ReuseOptions::default())
    }

    fn tier2_run_with_reuse_job_result_catching(
        &self,
        job: InspectJob,
        options: Tier2ReuseOptions,
    ) -> InspectResult {
        let started = Instant::now();
        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            self.tier2_run_with_reuse_job_result_with_options(job.clone(), options)
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
    ) -> InspectResult {
        let started = Instant::now();
        self.reuse_starts.fetch_add(1, Ordering::SeqCst);
        self.wait_for_tier2_reuse_waiter_for_debug(&job);
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

        let project_scope = JobScope::for_project(job.project_root.clone());
        job.scope_files = scope_files(&job.project_root, &project_scope);
        log_tier2_benchmark_category_start(&job);
        let cache = match self.cache_for_paths(job.inspect_dir.clone(), job.project_root.clone()) {
            Ok(cache) => cache,
            Err(message) => {
                let result = InspectResult::failed(&job, message, started.elapsed());
                log_tier2_benchmark_category_end(&result);
                return result;
            }
        };
        delay_tier2_reuse_for_debug(&job.project_root);
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
        let _interactive_permit = if options.interactive {
            let queued_state = if self.semantic_cold_seed_active.load(Ordering::SeqCst) {
                InspectBuilderState::GatedBySemanticSeed
            } else {
                InspectBuilderState::QueuedBehindColdBuilds
            };
            self.set_builder_state(&job.key, queued_state);
            let request = cold_build_limiter::ColdBuildAdmissionRequest::new(
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
            );
            let Some(permit) = permit else {
                let result = InspectResult::failed(
                    &job,
                    "explicit inspect Tier-2 cold-build admission was cancelled",
                    started.elapsed(),
                );
                log_tier2_benchmark_category_end(&result);
                return result;
            };
            self.set_builder_state(&job.key, InspectBuilderState::Building);
            Some(permit)
        } else {
            None
        };

        let result = match self.tier2_run_with_reuse_job(&job, &cache, &options) {
            Ok(success) => InspectResult::success(&job, success, started.elapsed()),
            Err(message) => InspectResult::failed(&job, message, started.elapsed()),
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
    ) -> Result<InspectScanSuccess, String> {
        let mut phases = Tier2PhaseTimings::default();
        let phase_started = Instant::now();
        let cached_records = load_contribution_freshness(cache, job.category)?;
        let current_by_relative = current_project_files(&job.project_root, &job.scope_files);
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
        if !scan_files.is_empty() {
            let mut scan_job = job.clone();
            scan_job.job_id = self.next_job_id.fetch_add(1, Ordering::Relaxed);
            scan_job.scope_files = scan_files.clone();
            if scan_job.category == InspectCategory::DeadCode
                && scan_job.callgraph_snapshot.is_none()
            {
                let snapshot_started = Instant::now();
                scan_job.callgraph_snapshot = self.build_tier2_callgraph_snapshot_with_refresh(
                    &scan_job,
                    options.allow_callgraph_cold_build,
                    options.require_callgraph_snapshot,
                    &callgraph_refresh_files,
                );
                phases.snapshot += snapshot_started.elapsed();
            }
            aggregate_job.callgraph_snapshot = scan_job.callgraph_snapshot.clone();
            #[cfg(debug_assertions)]
            if debug_cold_cache {
                std::thread::sleep(Duration::from_millis(10));
            }
            let scan_started = Instant::now();
            let oxc_result =
                self.oxc_result_for_scan(&scan_job, &scan_job.scope_files, &force_reparse_files)?;
            let scan_result = run_tier2_scan(&scan_job, oxc_result.as_ref());
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
                    let contributions = load_contributions(cache, job)?;
                    phases.log(job.category, &job.project_root);
                    return Ok(InspectScanSuccess {
                        scanned_files: scan_files,
                        contributions,
                        aggregate,
                    });
                }
            }
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
                    rescan_job.callgraph_snapshot = self
                        .build_tier2_callgraph_snapshot_with_refresh(
                            &rescan_job,
                            options.allow_callgraph_cold_build,
                            options.require_callgraph_snapshot,
                            &callgraph_refresh_files,
                        );
                    phases.snapshot += snapshot_started.elapsed();
                }
                let scan_started = Instant::now();
                let oxc_result = self.oxc_result_for_scan(
                    &rescan_job,
                    &rescan_job.scope_files,
                    &force_reparse_files,
                )?;
                let scan_result = run_tier2_scan(&rescan_job, oxc_result.as_ref());
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
                            let contributions = load_contributions(cache, job)?;
                            phases.log(job.category, &job.project_root);
                            return Ok(InspectScanSuccess {
                                scanned_files: scan_files,
                                contributions,
                                aggregate,
                            });
                        }
                    }
                }
            }
        }

        if aggregate_job.category == InspectCategory::DeadCode
            && aggregate_job.callgraph_snapshot.is_none()
        {
            let snapshot_started = Instant::now();
            aggregate_job.callgraph_snapshot = self.build_tier2_callgraph_snapshot_with_refresh(
                &aggregate_job,
                options.allow_callgraph_cold_build,
                options.require_callgraph_snapshot,
                &callgraph_refresh_files,
            );
            phases.snapshot += snapshot_started.elapsed();
        }
        if options.require_callgraph_snapshot
            && aggregate_job.category == InspectCategory::DeadCode
            && aggregate_job.callgraph_snapshot.is_none()
        {
            if let Some(reason) = callgraph_path_identity_gap(job) {
                return Ok(InspectScanSuccess {
                    scanned_files: scan_files,
                    contributions: Vec::new(),
                    aggregate: crate::inspect::scanners::dead_code::callgraph_unavailable_aggregate_with_reason(
                        job.scope_files.len(),
                        Some(&reason),
                    ),
                });
            }
            return Err(format!(
                "tier2 dead_code aggregate did not complete; builder_state={}",
                self.builder_state_detail_for_job(job)
            ));
        }
        let rollup_started = Instant::now();
        let contributions = load_contributions(cache, &aggregate_job)?;
        let aggregate = roll_up_tier2_contributions(&aggregate_job, &contributions);
        cache
            .store_tier2_aggregate(job.key.clone(), &contribution_set_hash, aggregate.clone())
            .map_err(|error| error.to_string())?;
        phases.rollup = rollup_started.elapsed();
        phases.log(job.category, &job.project_root);

        Ok(InspectScanSuccess {
            scanned_files: scan_files,
            contributions,
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
        if let Some(waiters) = in_flight.get_mut(&key) {
            waiters.push(Waiter { tx: waiter_tx });
            return Ok(());
        }

        in_flight.insert(key.clone(), vec![Waiter { tx: waiter_tx }]);
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
            if let Some(waiters) = self.take_waiters(&key) {
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
        in_flight.insert(key.clone(), Vec::new());
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
        self.request_tx
            .send(job)
            .map_err(|_| "inspect dispatch loop is unavailable".to_string())
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
        let result_rx = self.result_rx.clone();
        loop {
            // Route a worker result before an equally ready deadline notification.
            // Returning Pending while a terminal result is already available makes
            // the response depend on thread scheduling rather than job completion.
            select_biased! {
                recv(waiter_rx) -> outcome => {
                    return match outcome {
                        Ok(outcome) => filter_outcome_for_scope_with_contributions(
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
        let outcome = self.completion_outcome(result.clone());
        self.record_builder_attempt_outcome(&result.key, &outcome);
        if let Some(waiters) = self.take_waiters(&result.key) {
            Self::deliver_waiters(waiters, outcome);
        }
    }

    fn route_tier2_reuse_completion(&self, result: InspectResult) {
        let outcome = match result.outcome.clone() {
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

    /// Snapshot the cumulative count of reuse-path (watcher-driven) Tier-2
    /// completions. The main-thread drain compares this against its last-seen
    /// value to detect background scans that finished since the previous tick.
    pub fn reuse_completion_count(&self) -> u64 {
        self.reuse_completions.load(Ordering::SeqCst)
    }

    #[doc(hidden)]
    pub fn reuse_start_count_for_test(&self) -> u64 {
        self.reuse_starts.load(Ordering::SeqCst)
    }

    fn completion_outcome(&self, result: InspectResult) -> JobOutcome {
        let cache =
            match self.cache_for_paths(result.inspect_dir.clone(), result.project_root.clone()) {
                Ok(cache) => cache,
                Err(message) => return JobOutcome::Failed { message },
            };

        match result.outcome {
            Ok(success) => {
                let store_result = if result.category.is_tier2() {
                    cache.store_tier2_result_for_config(
                        result.key.clone(),
                        &success.scanned_files,
                        &success.contributions,
                        success.aggregate.clone(),
                        result.config.as_ref(),
                    )
                } else {
                    cache.store_aggregated(result.key, success.aggregate.clone())
                };

                match store_result {
                    Ok(()) => JobOutcome::Fresh {
                        payload: success.aggregate,
                    },
                    Err(error) => JobOutcome::Failed {
                        message: error.to_string(),
                    },
                }
            }
            Err(message) => JobOutcome::Failed { message },
        }
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
    /// Callgraph store snapshot projection (dead_code only).
    snapshot: Duration,
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
}

const TIER2_WORK_LOG_THRESHOLD: Duration = Duration::from_millis(50);

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
        let key = crate::search_index::artifact_cache_key(project_root);
        crate::slog_info!(
            "perf tier2 phases category={} freshness={}ms snapshot={}ms scan={}ms({} files) db={}ms(lock={},txn={}) rollup={}ms root={} key={}",
            category,
            self.freshness.as_millis(),
            self.snapshot.as_millis(),
            self.scan.as_millis(),
            self.scanned_files,
            self.db.as_millis(),
            self.db_lock.as_millis(),
            self.db_txn.as_millis(),
            self.rollup.as_millis(),
            crate::logging::normalize_index_root(project_root),
            key
        );
    }
}

fn scope_files(project_root: &Path, scope: &JobScope) -> Vec<PathBuf> {
    let mut files = crate::callgraph::walk_project_files(project_root)
        .filter(|path| scope.contains(path))
        .collect::<Vec<_>>();
    files.sort();
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

fn delay_tier2_reuse_for_debug(project_root: &Path) {
    #[cfg(not(debug_assertions))]
    let _ = project_root;
    #[cfg(debug_assertions)]
    {
        if std::env::var_os("AFT_TEST_TIER2_REUSE_GATE_ROOT").is_some()
            && env_project_root_matches("AFT_TEST_TIER2_REUSE_GATE_ROOT", project_root)
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
    let key = crate::search_index::artifact_cache_key(&job.project_root);
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

fn build_tier2_callgraph_snapshot(
    job: &InspectJob,
    allow_cold_build: bool,
) -> Option<Arc<CallgraphSnapshot>> {
    build_tier2_callgraph_snapshot_with_refresh_inner(job, allow_cold_build, false, &[], None)
}

#[cfg(test)]
fn build_tier2_callgraph_snapshot_with_refresh(
    job: &InspectJob,
    allow_cold_build: bool,
    refresh_paths: &[PathBuf],
) -> Option<Arc<CallgraphSnapshot>> {
    build_tier2_callgraph_snapshot_with_refresh_inner(
        job,
        allow_cold_build,
        false,
        refresh_paths,
        None,
    )
}

const BLOCKING_CALLGRAPH_STORE_RETRY_TIMEOUT: Duration = Duration::from_secs(30);
const BLOCKING_CALLGRAPH_STORE_RETRY_INTERVAL: Duration = Duration::from_millis(20);

fn open_ready_for_blocking_inspect(
    callgraph_dir: &Path,
    project_root: &Path,
    wait_for_publication: bool,
) -> Result<Option<CallGraphStore>, CallGraphStoreError> {
    let deadline = Instant::now() + BLOCKING_CALLGRAPH_STORE_RETRY_TIMEOUT;
    loop {
        match CallGraphStore::open_ready_repairing(
            callgraph_dir.to_path_buf(),
            project_root.to_path_buf(),
        ) {
            Ok(None) if wait_for_publication && Instant::now() < deadline => {}
            Err(error) if error.is_transient_lock_contention() && Instant::now() < deadline => {}
            result => return result,
        }
        std::thread::sleep(BLOCKING_CALLGRAPH_STORE_RETRY_INTERVAL);
    }
}

fn open_or_build_blocking_callgraph_store(
    callgraph_dir: PathBuf,
    project_root: PathBuf,
    allow_cold_build: bool,
    refresh_paths: &[PathBuf],
) -> Result<Option<CallGraphStore>, CallGraphStoreError> {
    if let Some(store) = open_ready_for_blocking_inspect(&callgraph_dir, &project_root, false)? {
        return Ok(Some(store));
    }
    if !allow_cold_build || refresh_paths.is_empty() {
        return Ok(None);
    }

    match CallGraphStore::cold_build_with_lease(
        callgraph_dir.clone(),
        project_root.clone(),
        refresh_paths,
    ) {
        Ok((store, _)) => Ok(Some(store)),
        Err(error)
            if matches!(error, CallGraphStoreError::Unavailable(_))
                || error.is_transient_lock_contention() =>
        {
            // The background builder and an inspect-triggered cold build can
            // briefly meet on the same generation. Keep either lock loser on
            // the Building/retry path instead of terminally failing inspect.
            match open_ready_for_blocking_inspect(&callgraph_dir, &project_root, true)? {
                Some(store) => Ok(Some(store)),
                None => Err(error),
            }
        }
        Err(error) => Err(error),
    }
}

fn merge_callgraph_refresh_paths(
    project_root: &Path,
    refresh_paths: &[PathBuf],
    stale: impl IntoIterator<Item = String>,
) -> Vec<PathBuf> {
    let mut paths = refresh_paths.to_vec();
    for rel in stale {
        let absolute = project_root.join(rel);
        if !paths.iter().any(|path| path == &absolute) {
            paths.push(absolute);
        }
    }
    paths
}

fn refresh_writable_dead_code_store(
    store: &CallGraphStore,
    callgraph_dir: &Path,
    refresh_paths: &[PathBuf],
) {
    match store.refresh_files(refresh_paths) {
        Ok(stats) => {
            crate::slog_info!(
                "tier2 dead_code: refreshed callgraph store at {} for {} watcher path(s): changed={} deleted={} refreshed_own={}",
                callgraph_dir.display(),
                refresh_paths.len(),
                stats.changed_files.len(),
                stats.deleted_files.len(),
                stats.refreshed_own_files
            );
        }
        Err(error) => {
            crate::slog_warn!(
                "tier2 dead_code: failed to refresh callgraph store at {} before projection: {}",
                callgraph_dir.display(),
                error
            );
            if let Err(mark_error) = store.mark_files_stale(refresh_paths) {
                crate::slog_warn!(
                    "tier2 dead_code: failed to mark callgraph store files stale at {} after refresh failure: {}",
                    callgraph_dir.display(),
                    mark_error
                );
            }
        }
    }
}

fn callgraph_path_identity_gap(job: &InspectJob) -> Option<String> {
    for callgraph_dir in callgraph_store_dirs_from_inspect_dir(&job.inspect_dir, &job.project_root)
    {
        let Ok(Some(store)) =
            CallGraphStore::open_readonly(callgraph_dir, job.project_root.clone())
        else {
            continue;
        };
        let Err(CallGraphStoreError::Unavailable(reason)) =
            project_dead_code_snapshot_with_revision(store.sqlite_path())
        else {
            continue;
        };
        if reason.starts_with("callgraph_path_identity_mismatch ") {
            return Some(reason);
        }
    }
    None
}

fn open_writable_dead_code_store(
    callgraph_dir: PathBuf,
    project_root: PathBuf,
    allow_cold_build: bool,
    build_if_missing: bool,
    refresh_paths: &[PathBuf],
) -> Result<Option<CallGraphStore>, CallGraphStoreError> {
    if build_if_missing {
        open_or_build_blocking_callgraph_store(
            callgraph_dir,
            project_root,
            allow_cold_build,
            refresh_paths,
        )
    } else if allow_cold_build {
        CallGraphStore::open_ready_repairing(callgraph_dir, project_root)
    } else {
        CallGraphStore::open_ready_no_rebuild(callgraph_dir, project_root)
    }
}

fn build_tier2_callgraph_snapshot_with_refresh_inner(
    job: &InspectJob,
    allow_cold_build: bool,
    build_if_missing: bool,
    refresh_paths: &[PathBuf],
    projection_cache: Option<&InspectManager>,
) -> Option<Arc<CallgraphSnapshot>> {
    let started = Instant::now();
    if !job.config.callgraph_store {
        crate::slog_info!(
            "tier2 dead_code: callgraph store disabled; reporting callgraph_unavailable"
        );
        return None;
    }

    let callgraph_dirs = callgraph_store_dirs_from_inspect_dir(&job.inspect_dir, &job.project_root);
    if callgraph_dirs.is_empty() {
        crate::slog_info!(
            "tier2 dead_code: inspect_dir has no root-keyed storage parent ({}); reporting callgraph_unavailable",
            job.inspect_dir.display()
        );
        return None;
    };
    for callgraph_dir in &callgraph_dirs {
        match CallGraphStore::cold_build_suspension(callgraph_dir, &job.project_root) {
            Ok(Some(suspension)) => {
                // This is a durable admission refusal, not a failed scan attempt.
                // Preserve it separately so blocking inspect reports the same
                // breaker tuple that navigation and health expose.
                if let Some(manager) = projection_cache {
                    manager.record_tier2_build_suspension(&job.key, suspension.clone());
                }
                crate::slog_info!(
                    "tier2 dead_code: callgraph build suspended for {} after {} deaths",
                    suspension.domain.as_str(),
                    suspension.death_count
                );
                return None;
            }
            Ok(None) => {}
            Err(error) => {
                crate::slog_warn!(
                    "tier2 dead_code: failed to read callgraph breaker at {}: {}",
                    callgraph_dir.display(),
                    error
                );
            }
        }
    }

    enum ProjectionStore {
        ReadOnly(ReadonlyCallGraphStore),
        Writable(CallGraphStore),
    }

    impl ProjectionStore {
        fn sqlite_path(&self) -> &Path {
            match self {
                Self::ReadOnly(store) => store.sqlite_path(),
                Self::Writable(store) => store.sqlite_path(),
            }
        }

        fn projection_identity(
            &self,
            project_root: &Path,
            write_revision: u64,
        ) -> CallgraphProjectionIdentity {
            let generation = match self {
                Self::ReadOnly(store) => store.projection_generation(),
                Self::Writable(store) => store.projection_generation(),
            }
            .map(str::to_owned);
            let legacy_sqlite_path = generation
                .is_none()
                .then(|| self.sqlite_path().to_path_buf());
            CallgraphProjectionIdentity {
                project_root: project_root.to_path_buf(),
                generation,
                legacy_sqlite_path,
                write_revision,
            }
        }

        fn current_projection_identity(
            &self,
            project_root: &Path,
        ) -> Result<Option<CallgraphProjectionIdentity>, CallGraphStoreError> {
            let write_revision = match self {
                Self::ReadOnly(store) => store.projection_write_revision()?,
                Self::Writable(store) => store.projection_write_revision()?,
            };
            Ok(write_revision.map(|revision| self.projection_identity(project_root, revision)))
        }
    }

    for (index, callgraph_dir) in callgraph_dirs.iter().enumerate() {
        // Paths without an explicit refresh stay read-only unless the published
        // store still has stale backend rows. The background refresh worker is
        // the usual writer for those rows; when it never runs for this root,
        // dead_code refreshes them inline so projection is not stuck forever.
        let projection_store = if refresh_paths.is_empty() || !job.callgraph_writer {
            let store = match CallGraphStore::open_readonly(
                callgraph_dir.clone(),
                job.project_root.clone(),
            ) {
                Ok(Some(store)) => store,
                Ok(None) => {
                    crate::slog_info!(
                        "tier2 dead_code: callgraph store unavailable at {} (cold/building/not ready); trying fallback={}",
                        callgraph_dir.display(),
                        index + 1 < callgraph_dirs.len()
                    );
                    continue;
                }
                Err(error) => {
                    crate::slog_warn!(
                        "tier2 dead_code: failed to open callgraph store read-only at {}: {}; trying fallback={}",
                        callgraph_dir.display(),
                        error,
                        index + 1 < callgraph_dirs.len()
                    );
                    continue;
                }
            };
            let stale = job
                .callgraph_writer
                .then(|| store.stale_files().ok())
                .flatten()
                .unwrap_or_default();
            if stale.is_empty() {
                ProjectionStore::ReadOnly(store)
            } else {
                drop(store);
                let refresh =
                    merge_callgraph_refresh_paths(&job.project_root, refresh_paths, stale);
                let store = match open_writable_dead_code_store(
                    callgraph_dir.clone(),
                    job.project_root.clone(),
                    allow_cold_build,
                    build_if_missing,
                    &refresh,
                ) {
                    Ok(Some(store)) => store,
                    Ok(None) => {
                        crate::slog_info!(
                            "tier2 dead_code: callgraph store unavailable at {} (cold/building/not ready); trying fallback={}",
                            callgraph_dir.display(),
                            index + 1 < callgraph_dirs.len()
                        );
                        continue;
                    }
                    Err(error) => {
                        crate::slog_warn!(
                            "tier2 dead_code: failed to open callgraph writer at {}: {}; trying fallback={}",
                            callgraph_dir.display(),
                            error,
                            index + 1 < callgraph_dirs.len()
                        );
                        continue;
                    }
                };
                refresh_writable_dead_code_store(&store, callgraph_dir, &refresh);
                ProjectionStore::Writable(store)
            }
        } else {
            let store = match open_writable_dead_code_store(
                callgraph_dir.clone(),
                job.project_root.clone(),
                allow_cold_build,
                build_if_missing,
                refresh_paths,
            ) {
                Ok(Some(store)) => store,
                Ok(None) => {
                    crate::slog_info!(
                        "tier2 dead_code: callgraph store unavailable at {} (cold/building/not ready); trying fallback={}",
                        callgraph_dir.display(),
                        index + 1 < callgraph_dirs.len()
                    );
                    continue;
                }
                Err(error) => {
                    crate::slog_warn!(
                        "tier2 dead_code: failed to open callgraph writer at {}: {}; trying fallback={}",
                        callgraph_dir.display(),
                        error,
                        index + 1 < callgraph_dirs.len()
                    );
                    continue;
                }
            };
            let stale = store.stale_files().unwrap_or_default();
            let refresh = merge_callgraph_refresh_paths(&job.project_root, refresh_paths, stale);
            refresh_writable_dead_code_store(&store, callgraph_dir, &refresh);
            ProjectionStore::Writable(store)
        };

        let cache_identity = match projection_store.current_projection_identity(&job.project_root) {
            Ok(identity) => identity,
            Err(error) => {
                crate::slog_warn!(
                    "tier2 dead_code: failed to read callgraph projection identity at {}: {}; trying fallback={}",
                    callgraph_dir.display(),
                    error,
                    index + 1 < callgraph_dirs.len()
                );
                continue;
            }
        };
        if let (Some(cache), Some(identity)) = (projection_cache, cache_identity.as_ref()) {
            // The pointer names immutable cold-build generations, while the durable
            // revision advances in the same SQLite transaction as every in-place
            // graph mutation. Equal identities therefore prove identical store
            // bytes for dead-code projection: this cache is exact, not heuristic.
            if let Some(snapshot) = cache.cached_callgraph_projection(identity) {
                return Some(snapshot);
            }
        } else if cache_identity.is_none() {
            // Stores from older binaries lack a durable revision, so keeping an
            // earlier snapshot would make an in-place refresh indistinguishable.
            if let Some(cache) = projection_cache {
                cache.clear_callgraph_projection();
            }
        }

        let (write_revision, snapshot) = match project_dead_code_snapshot_with_revision(
            projection_store.sqlite_path(),
        ) {
            Ok(projected) => projected,
            Err(CallGraphStoreError::Unavailable(message)) => {
                crate::slog_info!(
                        "tier2 dead_code: callgraph store projection unavailable at {} ({}); trying fallback={}",
                        callgraph_dir.display(),
                        message,
                        index + 1 < callgraph_dirs.len()
                    );
                continue;
            }
            Err(error) => {
                crate::slog_warn!(
                        "tier2 dead_code: callgraph store projection failed at {}: {}; trying fallback={}",
                        callgraph_dir.display(),
                        error,
                        index + 1 < callgraph_dirs.len()
                    );
                continue;
            }
        };
        let snapshot = Arc::new(snapshot);
        if let (Some(cache), Some(write_revision)) = (projection_cache, write_revision) {
            cache.cache_callgraph_projection(
                projection_store.projection_identity(&job.project_root, write_revision),
                Arc::clone(&snapshot),
            );
        }

        if index > 0 {
            crate::slog_info!(
                "tier2 dead_code: using ready callgraph store fallback {} for inspect_dir {}",
                callgraph_dir.display(),
                job.inspect_dir.display()
            );
        }

        crate::slog_info!(
            "perf tier2_callgraph_snapshot: source=callgraph_store files={} exports={} edges={} entry_points={} ms={}",
            snapshot.files.len(),
            snapshot.exported_symbols.len(),
            snapshot.outbound_calls.len(),
            snapshot.entry_points.len(),
            started.elapsed().as_millis()
        );

        return Some(snapshot);
    }

    crate::slog_info!(
        "tier2 dead_code: no ready callgraph store found for inspect_dir {}; reporting callgraph_unavailable",
        job.inspect_dir.display()
    );
    None
}

fn estimate_callgraph_snapshot_bytes(snapshot: &CallgraphSnapshot) -> u64 {
    let files = snapshot.files.iter().fold(0u64, |bytes, path| {
        bytes
            .saturating_add(std::mem::size_of::<PathBuf>() as u64)
            .saturating_add(crate::memory::path_bytes(path))
    });
    let exports = snapshot
        .exported_symbols
        .iter()
        .fold(0u64, |bytes, export| {
            bytes
                .saturating_add(std::mem::size_of::<super::job::CallgraphExport>() as u64)
                .saturating_add(crate::memory::path_bytes(&export.file))
                .saturating_add(crate::memory::usize_to_u64(export.symbol.len()))
                .saturating_add(crate::memory::usize_to_u64(export.kind.len()))
        });
    let calls = snapshot.outbound_calls.iter().fold(0u64, |bytes, call| {
        bytes
            .saturating_add(std::mem::size_of::<super::job::CallgraphOutboundCall>() as u64)
            .saturating_add(crate::memory::path_bytes(&call.caller_file))
            .saturating_add(crate::memory::usize_to_u64(call.caller_symbol.len()))
            .saturating_add(crate::memory::usize_to_u64(call.target.len()))
            .saturating_add(crate::memory::usize_to_u64(call.provenance.len()))
    });
    let entry_points = snapshot.entry_points.iter().fold(0u64, |bytes, path| {
        bytes
            .saturating_add(std::mem::size_of::<PathBuf>() as u64)
            .saturating_add(crate::memory::path_bytes(path))
    });
    let entry_point_symbols =
        snapshot
            .entry_point_symbols
            .iter()
            .fold(0u64, |bytes, (path, symbols)| {
                let symbols_bytes = symbols.iter().fold(0u64, |bytes, symbol| {
                    bytes
                        .saturating_add(std::mem::size_of::<String>() as u64)
                        .saturating_add(crate::memory::usize_to_u64(symbol.len()))
                });
                bytes
                    .saturating_add(std::mem::size_of::<(PathBuf, BTreeSet<String>)>() as u64)
                    .saturating_add(crate::memory::path_bytes(path))
                    .saturating_add(symbols_bytes)
            });
    (std::mem::size_of::<CallgraphSnapshot>() as u64)
        .saturating_add(files)
        .saturating_add(exports)
        .saturating_add(calls)
        .saturating_add(entry_points)
        .saturating_add(entry_point_symbols)
}

fn callgraph_store_dir_from_inspect_dir(
    inspect_dir: &Path,
    project_root: &Path,
) -> Option<PathBuf> {
    let scope_key = crate::path_identity::project_scope_key(project_root);
    let storage_dir = if inspect_dir
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name == scope_key)
    {
        inspect_dir.parent()?.parent()?
    } else {
        inspect_dir.parent()?
    };
    let project_key = crate::search_index::artifact_cache_key(project_root);
    Some(storage_dir.join("callgraph").join(project_key))
}

fn callgraph_store_dirs_from_inspect_dir(inspect_dir: &Path, project_root: &Path) -> Vec<PathBuf> {
    callgraph_store_dir_from_inspect_dir(inspect_dir, project_root)
        .into_iter()
        .collect()
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
    cache: &(impl InspectCacheRead + ?Sized),
    job: &InspectJob,
) -> Result<bool, String> {
    let contributions = load_contributions(cache, job)?;
    Ok(contributions
        .iter()
        .any(dead_code_contribution_needs_fact_refresh))
}

fn dead_code_contribution_needs_fact_refresh(contribution: &FileContribution) -> bool {
    let Ok(parsed) =
        serde_json::from_value::<DeadCodeRefreshContribution>(contribution.contribution.clone())
    else {
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
    cache: &(impl InspectCacheRead + ?Sized),
    job: &InspectJob,
) -> Result<bool, String> {
    let contributions = load_contributions(cache, job)?;
    Ok(contributions
        .iter()
        .any(unused_exports_contribution_needs_fact_refresh))
}

/// Duplicates contributions written before v0.44 lack the `line_count` field
/// (serde defaults it to 0), so a cached roll-up computes total_analyzed_lines
/// as 0 and the summary renders "0.0% of 0 analyzed lines". One full rescan
/// repopulates the counts; fresh contributions always carry line_count.
fn duplicates_contributions_need_fact_refresh(
    cache: &(impl InspectCacheRead + ?Sized),
    job: &InspectJob,
) -> Result<bool, String> {
    let contributions = load_contributions(cache, job)?;
    Ok(contributions
        .iter()
        .any(|contribution| contribution.contribution.get("line_count").is_none()))
}

fn unused_exports_contribution_needs_fact_refresh(contribution: &FileContribution) -> bool {
    let top_level_oxc = contribution
        .contribution
        .get("provenance")
        .and_then(Value::as_str)
        == Some(OXC_PROVENANCE);
    let Ok(parsed) =
        serde_json::from_value::<UnusedExportsContribution>(contribution.contribution.clone())
    else {
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

    match job.category {
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
    }
}

fn roll_up_tier2_contributions(job: &InspectJob, contributions: &[FileContribution]) -> Value {
    roll_up_tier2_contributions_with_limit(job, contributions, Some(MAX_DRILL_DOWN_ITEMS))
}

fn roll_up_tier2_contributions_with_limit(
    job: &InspectJob,
    contributions: &[FileContribution],
    drill_down_limit: Option<usize>,
) -> Value {
    match job.category {
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
    }
}

fn scoped_tier2_payload_from_contributions(
    snapshot: &InspectSnapshot,
    category: InspectCategory,
    cache: &(impl InspectCacheRead + ?Sized),
    project_payload: Value,
    scope: &JobScope,
) -> Result<Value, String> {
    if scope.is_project_wide() {
        return Ok(project_payload);
    }

    let project_scope = JobScope::for_project(snapshot.project_root.clone());
    let rollup_job = scoped_tier2_rollup_job(snapshot, category, &project_scope);
    let contributions = load_contributions(cache, &rollup_job)?;
    let full_payload = roll_up_tier2_contributions_with_limit(&rollup_job, &contributions, None);
    let scoped_payload = filter_payload_for_scope(full_payload, scope);
    Ok(cap_payload_drill_down(scoped_payload, MAX_DRILL_DOWN_ITEMS))
}

fn scoped_tier2_rollup_job(
    snapshot: &InspectSnapshot,
    category: InspectCategory,
    scope: &JobScope,
) -> InspectJob {
    let mut job = InspectJob {
        job_id: 0,
        key: JobKey::for_project_category(category),
        category,
        scope_files: scope_files(&snapshot.project_root, scope),
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
        job.callgraph_snapshot = build_tier2_callgraph_snapshot(&job, false);
    }

    job
}

fn roll_up_dead_code_contributions(
    job: &InspectJob,
    contributions: &[FileContribution],
    drill_down_limit: Option<usize>,
) -> Value {
    let Some(snapshot) = job.callgraph_snapshot.as_deref() else {
        return super::scanners::dead_code::callgraph_unavailable_aggregate(job.scope_files.len());
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
    for scan in &parsed {
        if public_api_files.contains(&scan.file) {
            continue;
        }
        // Mirror the fresh-scan path: fixtures/corpora/mock data are consumed
        // by path, never imported, so their exports always look unused.
        if super::job::is_test_support_file(&scan.file) {
            continue;
        }
        let generated_file = super::generated::is_generated_file_with_cached_hint(
            &job.project_root,
            &scan.file,
            scan.generated,
        );

        for export in &scan.exports {
            if export_uses_oxc(export) {
                match export.verdict.unwrap_or(LivenessVerdict::Unused) {
                    LivenessVerdict::Used => continue,
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
                if uncertain {
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
        "drill_down_capped": drill_down_limit.is_some_and(|limit| count + generated_count > limit),
        "generated_drill_down_capped": drill_down_limit.is_some_and(|limit| generated_count > limit),
        "test_only_drill_down_capped": drill_down_limit.is_some_and(|limit| test_only_count > limit),
        "scanned_files": parsed.len(),
        "languages_skipped": skipped_languages(&job.scope_files, LanguageSkipMode::UnusedExports),
        "uncertain_count": uncertain_count,
        "uncertain_items": uncertain_items,
        "complete": parse_errors.is_empty() && skipped_files.is_empty(),
    });
    if !parse_errors.is_empty() {
        aggregate["parse_errors"] = Value::Array(parse_errors);
    }
    if !skipped_files.is_empty() {
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
                super::generated::is_generated_file_with_cached_hint(
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
    for file in &oxc_result.files {
        if public_api_files.contains(&file.relative_file)
            || super::job::is_test_support_file(&file.relative_file)
        {
            continue;
        }
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
                    if !is_test_file(&file.relative_file)
                        && !export.test_only_reference_files.is_empty()
                    {
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
                    }
                }
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
                    if !is_test_file(&file.relative_file)
                        && !export.test_only_reference_files.is_empty()
                    {
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
        "drill_down_capped": drill_down_limit.is_some_and(|limit| count + generated_count > limit),
        "generated_drill_down_capped": drill_down_limit.is_some_and(|limit| generated_count > limit),
        "test_only_drill_down_capped": drill_down_limit.is_some_and(|limit| test_only_count > limit),
        "scanned_files": parsed.len(),
        "languages_skipped": skipped_languages(&job.scope_files, LanguageSkipMode::UnusedExports),
        "uncertain_count": uncertain_count,
        "uncertain_items": uncertain_items,
        "complete": parse_errors.is_empty() && skipped_files.is_empty(),
    });
    if !parse_errors.is_empty() {
        aggregate["parse_errors"] = Value::Array(parse_errors);
    }
    if !skipped_files.is_empty() {
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

fn cap_payload_drill_down(mut payload: Value, limit: usize) -> Value {
    let mut capped = false;
    if let Some(items) = payload.get_mut("items").and_then(Value::as_array_mut) {
        capped |= items.len() > limit;
        items.truncate(limit);
    }
    if let Some(groups) = payload.get_mut("groups").and_then(Value::as_array_mut) {
        capped |= groups.len() > limit;
        groups.truncate(limit);
    }
    if let Some(object) = payload.as_object_mut() {
        object.insert("drill_down_capped".to_string(), json!(capped));
    }
    payload
}

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
    outcome: JobOutcome,
    snapshot: &InspectSnapshot,
    category: InspectCategory,
    cache: &(impl InspectCacheRead + ?Sized),
    scope: &JobScope,
) -> JobOutcome {
    if !category.is_tier2() || scope.is_project_wide() {
        return filter_outcome_for_scope(outcome, scope);
    }

    match outcome {
        JobOutcome::Fresh { payload } => {
            match scoped_tier2_payload_from_contributions(snapshot, category, cache, payload, scope)
            {
                Ok(payload) => JobOutcome::Fresh { payload },
                Err(message) => JobOutcome::Failed { message },
            }
        }
        JobOutcome::Stale { cached, in_flight } => match cached {
            Some(payload) => {
                match scoped_tier2_payload_from_contributions(
                    snapshot, category, cache, payload, scope,
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

fn filter_payload_for_scope(mut payload: serde_json::Value, scope: &JobScope) -> serde_json::Value {
    if scope.is_project_wide() {
        return payload;
    }

    // Scoped Tier 2 callers pass an uncapped rollup into this filter and cap
    // drill-down only afterwards, so the recomputed count below remains the
    // true in-scope total rather than the size of a capped sample.
    if let Some(items) = payload
        .get_mut("items")
        .and_then(|value| value.as_array_mut())
    {
        let count = filter_values_for_scope(items, scope);
        let largest_cycle = items
            .iter()
            .filter_map(|item| item.get("files").and_then(Value::as_array).map(Vec::len))
            .max();
        if let Some(object) = payload.as_object_mut() {
            object.insert("count".to_string(), serde_json::json!(count));
            if object.contains_key("largest") {
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
        let count = filter_values_for_scope(groups, scope);
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
        if object.contains_key("top") {
            if let Some(top) = recompute_scoped_top_preview(object) {
                object.insert("top".to_string(), top);
            } else if let Some(top) = object.get_mut("top").and_then(Value::as_array_mut) {
                filter_values_for_scope(top, scope);
            }
        }
        if object.contains_key("duplicated_lines") {
            recompute_duplicate_payload_stats(object);
        }
        object.remove("by_language");
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
    let total_analyzed_lines = object
        .get("total_analyzed_lines")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let duplicated_percent = if total_analyzed_lines == 0 {
        0.0
    } else {
        (duplicated_lines as f64 * 100.0) / total_analyzed_lines as f64
    };
    object.insert("duplicated_lines".to_string(), json!(duplicated_lines));
    object.insert(
        "duplicated_file_count".to_string(),
        json!(duplicated_file_count),
    );
    object.insert("duplicated_percent".to_string(), json!(duplicated_percent));
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
    Some(Value::Array(
        values
            .iter()
            .take(super::entry_points::TOP_PREVIEW_ITEMS)
            .map(top_preview_value)
            .collect(),
    ))
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
        if files.len() < 2 {
            return false;
        }
        files.first().and_then(Value::as_str).map(str::to_string)
    } else {
        None
    };

    if let Some(occurrence) = first_scoped_occurrence {
        update_duplicate_group_sample(value, &occurrence);
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
mod guard_tests {
    use super::*;

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

    struct ProjectionObserverReset;

    impl Drop for ProjectionObserverReset {
        fn drop(&mut self) {
            crate::callgraph_store::set_projection_before_open_observer(None);
        }
    }

    fn count_projections() -> (Arc<std::sync::atomic::AtomicUsize>, ProjectionObserverReset) {
        let count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let observed = Arc::clone(&count);
        crate::callgraph_store::set_projection_before_open_observer(Some(Arc::new(move |_| {
            observed.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        })));
        (count, ProjectionObserverReset)
    }

    fn write_projection_cache_file(path: &Path, contents: &str) {
        std::fs::create_dir_all(path.parent().expect("fixture file parent"))
            .expect("create fixture parent");
        std::fs::write(path, contents).expect("write fixture file");
    }

    fn published_projection_fixture() -> (tempfile::TempDir, PathBuf, PathBuf, InspectJob) {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = std::fs::canonicalize(dir.path()).expect("canonical fixture root");
        write_projection_cache_file(
            &root.join("src/main.ts"),
            "import { firstTarget } from './target';\nexport function main() { firstTarget(); }\n",
        );
        write_projection_cache_file(
            &root.join("src/target.ts"),
            "export function firstTarget() {}\n",
        );
        let inspect_dir = root.join(".aft-cache").join("inspect");
        let project_key = crate::search_index::artifact_cache_key(&root);
        crate::root_cache::configure_artifact_access(&root, &project_key, false);
        let callgraph_dir =
            callgraph_store_dir_from_inspect_dir(&inspect_dir, &root).expect("callgraph dir");
        let files = crate::callgraph::walk_project_files(&root).collect::<Vec<_>>();
        let (store, _) = CallGraphStore::cold_build_with_lease(callgraph_dir, root.clone(), &files)
            .expect("publish initial generation");
        drop(store);
        let mut job = snapshot_job(&root, &inspect_dir, true);
        job.callgraph_writer = false;
        (dir, root, inspect_dir, job)
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
    fn dead_code_blocked_on_callgraph_reads_latest_aggregate_flag() {
        // Health asks the manager whether dead_code is only missing because the
        // callgraph store was not ready when it scanned. The answer must track
        // the latest persisted dead_code aggregate's `callgraph_available` flag
        // (mirroring the suppression rule in `latest_tier2_counts`).
        let dir = tempfile::tempdir().unwrap();
        let project_root = std::fs::canonicalize(dir.path()).unwrap();
        std::fs::write(project_root.join("lib.rs"), "pub fn marker() {}\n").unwrap();
        let manager = InspectManager::new();
        let inspect_dir = dir.path().join("inspect");

        // No aggregate yet → not blocked.
        assert!(!manager.dead_code_blocked_on_callgraph(inspect_dir.clone(), project_root.clone()));

        let cache = manager
            .cache_for_paths(inspect_dir.clone(), project_root.clone())
            .expect("open cache");
        let key = JobKey::for_project_category(InspectCategory::DeadCode);
        let hash = cache
            .contribution_set_hash(InspectCategory::DeadCode)
            .expect("contribution hash");

        // A callgraph-backed dead_code aggregate → not blocked, count surfaced.
        cache
            .store_tier2_aggregate(
                key.clone(),
                &hash,
                serde_json::json!({ "count": 3, "callgraph_available": true }),
            )
            .expect("store callgraph-backed aggregate");
        assert!(!manager.dead_code_blocked_on_callgraph(inspect_dir.clone(), project_root.clone()));
        assert_eq!(
            manager
                .latest_tier2_counts(inspect_dir.clone(), project_root.clone())
                .0,
            Some(3)
        );

        // A callgraph_unavailable aggregate (store not ready) → blocked, and the
        // count stays suppressed so the status bar never fabricates a zero.
        cache
            .store_tier2_aggregate(
                key,
                &hash,
                crate::inspect::scanners::dead_code::callgraph_unavailable_aggregate(1),
            )
            .expect("store callgraph_unavailable aggregate");
        assert!(manager.dead_code_blocked_on_callgraph(inspect_dir.clone(), project_root.clone()));
        assert_eq!(
            manager.latest_tier2_counts(inspect_dir, project_root).0,
            None,
            "callgraph_unavailable dead_code must stay suppressed"
        );
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
                callgraph_store,
                ..Config::default()
            }),
            symbol_cache: Arc::new(RwLock::new(SymbolCache::new())),
            inspect_writer: true,
            callgraph_writer: true,
            callgraph_snapshot: None,
        }
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
                callgraph_store: true,
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

        let deadline = Instant::now() + Duration::from_secs(3);
        while manager.tier2_builder_state(InspectCategory::DeadCode)
            != InspectBuilderState::GatedBySemanticSeed
        {
            assert!(
                Instant::now() < deadline,
                "inspect must queue while the active seed owns the slot"
            );
            std::thread::yield_now();
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
                callgraph_store: true,
                ..Config::default()
            }),
            Arc::new(RwLock::new(SymbolCache::new())),
            true,
            true,
        );
        let limiter = cold_build_limiter::test_limiter(1);
        let manager = Arc::new(InspectManager::new());
        manager.set_cold_build_limiter(Arc::clone(&limiter));
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

        let rebound = outcome_rx
            .recv_timeout(Duration::from_secs(3))
            .expect("unchanged persisted aggregate bypasses the occupied cold-build queue");
        assert!(rebound.payload().is_some());
        assert_eq!(
            limiter.admission_events().len(),
            events_before,
            "quick reuse must not request an interactive cold-build permit"
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
    fn background_tier2_reuse_panic_clears_builder_registration() {
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
        manager
            .submit_tier2_run_with_reuse_background(snapshot, InspectCategory::Duplicates)
            .expect("queue background duplicates scan");

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
    fn unused_exports_oxc_cached_rollup_preserves_generated_split() {
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
        assert_eq!(rolled_up["generated_count"], 2, "{rolled_up:#}");
        assert_eq!(rolled_up["total_count"], 3, "{rolled_up:#}");
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
            1,
            "a legacy contribution without generated must probe and recover its classification"
        );
    }

    #[test]
    fn inspect_callgraph_open_waits_out_transient_sqlite_writer_lock() {
        let dir = write_ts_project(3);
        let root = std::fs::canonicalize(dir.path()).expect("canonical root");
        let inspect_dir = root.join(".aft-cache").join("inspect");
        let callgraph_dir =
            callgraph_store_dir_from_inspect_dir(&inspect_dir, &root).expect("store dir");
        let files = crate::callgraph::walk_project_files(&root).collect::<Vec<_>>();
        let (store, _) =
            CallGraphStore::cold_build_with_lease(callgraph_dir.clone(), root.clone(), &files)
                .expect("publish initial generation");
        let sqlite_path = store.sqlite_path().to_path_buf();
        drop(store);

        let blocker = rusqlite::Connection::open(&sqlite_path).expect("open blocking connection");
        blocker
            .execute_batch(
                "PRAGMA journal_mode=DELETE;
                 BEGIN EXCLUSIVE;
                 UPDATE meta SET v = v WHERE k = 'ready';",
            )
            .expect("hold exclusive write transaction");
        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let open = std::thread::spawn(move || {
            started_tx.send(()).expect("signal inspect open start");
            open_or_build_blocking_callgraph_store(callgraph_dir, root, true, &files)
        });
        started_rx.recv().expect("inspect open thread started");
        std::thread::sleep(Duration::from_millis(100));
        blocker.execute_batch("COMMIT").expect("release write lock");

        assert!(
            open.join()
                .expect("inspect open thread joined")
                .expect("transient contention must stay on the Building/retry path")
                .is_some(),
            "inspect must reopen the ready callgraph instead of failing terminally"
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

    #[test]
    fn callgraph_snapshot_reports_unavailable_when_store_not_ready() {
        let dir = write_ts_project(3);
        let root = std::fs::canonicalize(dir.path()).expect("canonical root");
        let inspect_dir = root.join(".aft-cache").join("inspect");
        let callgraph_dir =
            callgraph_store_dir_from_inspect_dir(&inspect_dir, &root).expect("store dir");
        let _store = CallGraphStore::open(callgraph_dir, root.clone()).expect("open empty store");

        let snapshot =
            build_tier2_callgraph_snapshot(&snapshot_job(&root, &inspect_dir, true), false);

        assert!(
            snapshot.is_none(),
            "a cold/mid-build store must surface callgraph_unavailable instead of rebuilding inline"
        );
    }

    #[test]
    fn suspended_callgraph_build_sets_distinct_builder_state() {
        let dir = write_ts_project(3);
        let root = std::fs::canonicalize(dir.path()).expect("canonical root");
        let inspect_dir = root.join(".aft-cache").join("inspect");
        let callgraph_dir =
            callgraph_store_dir_from_inspect_dir(&inspect_dir, &root).expect("store dir");
        let files = crate::callgraph::walk_project_files(&root).collect::<Vec<_>>();
        let key = crate::build_breaker::BreakerKey::new(
            root.display().to_string(),
            crate::build_breaker::BuildDomain::CallgraphCold,
            crate::callgraph_store::callgraph_corpus_fingerprint_for_test(&root, &files)
                .expect("corpus fingerprint"),
        );
        let breaker = crate::build_breaker::BuildDeathBreaker::open(
            callgraph_dir.join("build-breaker.sqlite"),
        )
        .expect("open breaker");
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time")
            .as_millis() as u64;
        for _ in 0..3 {
            let crate::build_breaker::BreakerAdmission::Admitted(attempt) =
                breaker.admit_at(&key, 0, now).expect("admit build")
            else {
                panic!("early suspension before the threshold");
            };
            breaker
                .record_attributed_death_at(&key, &attempt.attempt_id, 0, 0, now)
                .expect("record death");
        }

        let manager = InspectManager::new();
        let job = snapshot_job(&root, &inspect_dir, true);
        assert!(manager
            .build_tier2_callgraph_snapshot_with_refresh(&job, true, true, &files)
            .is_none());
        assert_eq!(
            manager.tier2_builder_state(InspectCategory::DeadCode),
            InspectBuilderState::Suspended
        );
        let detail = manager.tier2_builder_state_detail(InspectCategory::DeadCode);
        assert!(detail.starts_with("suspended domain=callgraph_cold deaths=3 age_s="));
        assert!(detail.ends_with("reason=zero_credit_death_limit"));
    }

    #[test]
    fn readonly_tier2_projection_keeps_generation_pinned_through_concurrent_gc() {
        let dir = write_ts_project(3);
        let root = std::fs::canonicalize(dir.path()).expect("canonical root");
        let inspect_dir = root.join(".aft-cache").join("inspect");
        let callgraph_dir =
            callgraph_store_dir_from_inspect_dir(&inspect_dir, &root).expect("store dir");
        let files = crate::callgraph::walk_project_files(&root).collect::<Vec<_>>();
        let (store, _) =
            CallGraphStore::cold_build_with_lease(callgraph_dir.clone(), root.clone(), &files)
                .expect("initial generation");
        let initial_generation = store.sqlite_path().to_path_buf();
        drop(store);
        let project_key = crate::search_index::artifact_cache_key(&root);
        crate::root_cache::enable_writer_lease_acquisition_counts_for_test();
        // Delta-based counting: the enable flag is process-global and another
        // parallel test may have switched it on before this test's own setup
        // acquired its cold-build lease, so the absolute count is
        // enable-order-dependent. Only acquisitions inside the observed window
        // below are this assertion's business.
        let lease_count_before = crate::root_cache::writer_lease_acquisition_count_for_test(
            crate::root_cache::RootCacheDomain::Callgraph,
            &project_key,
            &root,
        );

        let root_for_observer = root.clone();
        let dir_for_observer = callgraph_dir.clone();
        let files_for_observer = files.clone();
        crate::callgraph_store::set_projection_before_open_observer(Some(Arc::new(
            move |projected_path| {
                for _ in 0..3 {
                    let (published, _) = CallGraphStore::cold_build_with_lease(
                        dir_for_observer.clone(),
                        root_for_observer.clone(),
                        &files_for_observer,
                    )
                    .expect("concurrent generation publication");
                    drop(published);
                }
                assert!(
                    projected_path.is_file(),
                    "the tier2 reader marker must pin the selected generation through GC"
                );
            },
        )));
        let mut job = snapshot_job(&root, &inspect_dir, true);
        job.callgraph_writer = false;

        let snapshot =
            build_tier2_callgraph_snapshot_with_refresh(&job, false, &[root.join("mod0.ts")]);
        crate::callgraph_store::set_projection_before_open_observer(None);

        assert!(snapshot.is_some());
        assert!(initial_generation.is_file());
        assert_eq!(
            crate::root_cache::writer_lease_acquisition_count_for_test(
                crate::root_cache::RootCacheDomain::Callgraph,
                &project_key,
                &root,
            ) - lease_count_before,
            3,
            "only the three observer publications may acquire a writer lease; tier2 must stay read-only"
        );
    }

    #[test]
    fn direct_callgraph_snapshot_does_not_cold_rebuild_when_store_needs_rebuild() {
        let dir = write_ts_project(3);
        let root = std::fs::canonicalize(dir.path()).expect("canonical root");
        let inspect_dir = root.join(".aft-cache").join("inspect");
        let callgraph_dir =
            callgraph_store_dir_from_inspect_dir(&inspect_dir, &root).expect("store dir");
        let store = CallGraphStore::open(callgraph_dir.clone(), root.clone()).expect("open store");
        let files = crate::callgraph::walk_project_files(&root).collect::<Vec<_>>();
        store.cold_build(&files).expect("cold build store");
        let sqlite_path = store.sqlite_path().to_path_buf();
        drop(store);

        let still_existing_previous_root = root.with_file_name("previous-root-still-exists");
        std::fs::create_dir_all(&still_existing_previous_root).expect("create previous root");
        let conn = rusqlite::Connection::open(&sqlite_path).expect("open store sqlite");
        conn.execute(
            "UPDATE backend_file_state SET workspace_root = ?1",
            rusqlite::params![still_existing_previous_root.display().to_string()],
        )
        .expect("force root repair rebuild state");
        drop(conn);

        let snapshot =
            build_tier2_callgraph_snapshot(&snapshot_job(&root, &inspect_dir, true), false)
                .expect("readonly snapshot should avoid cold-rebuilding the store");

        assert_eq!(snapshot.files.len(), 3);
        let conn = rusqlite::Connection::open(&sqlite_path).expect("reopen store sqlite");
        let stored_root: String = conn
            .query_row(
                "SELECT workspace_root FROM backend_file_state LIMIT 1",
                [],
                |row| row.get(0),
            )
            .expect("read stored root");
        assert_eq!(
            stored_root,
            still_existing_previous_root.display().to_string(),
            "direct inspect must not cold-rebuild or re-root a read-only snapshot"
        );
    }

    #[test]
    fn callgraph_snapshot_reads_ready_callgraph_store() {
        let dir = write_ts_project(3);
        let root = std::fs::canonicalize(dir.path()).expect("canonical root");
        let inspect_dir = root.join(".aft-cache").join("inspect");
        let callgraph_dir =
            callgraph_store_dir_from_inspect_dir(&inspect_dir, &root).expect("store dir");
        let store = CallGraphStore::open(callgraph_dir, root.clone()).expect("open store");
        let files = crate::callgraph::walk_project_files(&root).collect::<Vec<_>>();
        store.cold_build(&files).expect("cold build store");

        let snapshot =
            build_tier2_callgraph_snapshot(&snapshot_job(&root, &inspect_dir, true), false)
                .expect("ready store snapshot");

        assert_eq!(snapshot.files.len(), 3);
        assert_eq!(snapshot.exported_symbols.len(), 3);
    }

    #[test]
    fn path_identity_mismatch_is_a_named_dead_code_terminal_gap() {
        let dir = write_ts_project(1);
        let root = std::fs::canonicalize(dir.path()).expect("canonical root");
        let inspect_dir = root.join(".aft-cache").join("inspect");
        let callgraph_dir =
            callgraph_store_dir_from_inspect_dir(&inspect_dir, &root).expect("store dir");
        let source = root.join("mod0.ts");
        let foreign_dir = tempfile::tempdir().expect("foreign tempdir");
        let foreign = foreign_dir.path().join("foreign.ts");
        std::fs::write(&foreign, "export function foreign() {}\n").expect("write foreign source");
        let store = CallGraphStore::open(callgraph_dir, root.clone()).expect("open store");
        store.cold_build(&[source]).expect("cold build store");
        let error = store
            .refresh_files(&[foreign.clone()])
            .expect_err("foreign watcher path cannot be assigned a store-relative key");
        assert!(matches!(
            error,
            CallGraphStoreError::PathIdentityMismatch { .. }
        ));
        drop(store);

        let job = snapshot_job(&root, &inspect_dir, true);
        let reason = callgraph_path_identity_gap(&job).expect("durable path identity gap");
        assert!(reason.contains("callgraph_path_identity_mismatch"));
        assert!(reason.contains(&foreign.display().to_string()));
        let aggregate =
            crate::inspect::scanners::dead_code::callgraph_unavailable_aggregate_with_reason(
                1,
                Some(&reason),
            );
        assert_eq!(
            aggregate["notes"],
            serde_json::json!(["callgraph_unavailable", "callgraph_path_identity_mismatch"])
        );
        assert_eq!(aggregate["callgraph_unavailable_reason"], reason);
    }

    #[test]
    fn stale_callgraph_store_refreshes_inline_when_refresh_worker_does_not_run() {
        let dir = write_ts_project(2);
        let root = std::fs::canonicalize(dir.path()).expect("canonical root");
        let inspect_dir = root.join(".aft-cache").join("inspect");
        let callgraph_dir =
            callgraph_store_dir_from_inspect_dir(&inspect_dir, &root).expect("store dir");
        let project_key = crate::search_index::artifact_cache_key(&root);
        crate::root_cache::configure_artifact_access(&root, &project_key, false);
        let store = CallGraphStore::open(callgraph_dir.clone(), root.clone()).expect("open store");
        let files = crate::callgraph::walk_project_files(&root).collect::<Vec<_>>();
        store.cold_build(&files).expect("cold build store");
        store
            .mark_files_stale(&files)
            .expect("mark published store stale");
        drop(store);

        let job = snapshot_job(&root, &inspect_dir, true);
        let manager = InspectManager::new();
        assert!(
            !manager.callgraph_ready_for_snapshot(&InspectSnapshot::new(
                root.clone(),
                inspect_dir.clone(),
                Arc::clone(&job.config),
                Arc::clone(&job.symbol_cache),
            )),
            "a store with leftover stale rows must not look callgraph-ready"
        );

        let snapshot = manager
            .build_tier2_callgraph_snapshot_with_refresh(&job, false, false, &[])
            .expect("dead_code must refresh stale rows inline when the refresh worker never ran");
        assert_eq!(snapshot.files.len(), 2);

        let ready = InspectSnapshot::new(
            root,
            inspect_dir,
            Arc::clone(&job.config),
            Arc::clone(&job.symbol_cache),
        );
        assert!(
            manager.callgraph_ready_for_snapshot(&ready),
            "after the inline refresh, callgraph_ready must agree with a successful projection"
        );
    }

    #[test]
    fn callgraph_ready_and_builder_projection_agree_on_stale_rows() {
        let dir = write_ts_project(1);
        let root = std::fs::canonicalize(dir.path()).expect("canonical root");
        let inspect_dir = root.join(".aft-cache").join("inspect");
        let callgraph_dir =
            callgraph_store_dir_from_inspect_dir(&inspect_dir, &root).expect("store dir");
        let project_key = crate::search_index::artifact_cache_key(&root);
        crate::root_cache::configure_artifact_access(&root, &project_key, false);
        let store = CallGraphStore::open(callgraph_dir, root.clone()).expect("open store");
        let files = crate::callgraph::walk_project_files(&root).collect::<Vec<_>>();
        store.cold_build(&files).expect("cold build store");
        let sqlite_path = store.sqlite_path().to_path_buf();
        drop(store);

        let job = snapshot_job(&root, &inspect_dir, true);
        let snapshot = InspectSnapshot::new(
            root.clone(),
            inspect_dir.clone(),
            Arc::clone(&job.config),
            Arc::clone(&job.symbol_cache),
        );
        let manager = InspectManager::new();
        assert!(
            manager.callgraph_ready_for_snapshot(&snapshot),
            "a fresh store must be ready for both the phase check and projection"
        );
        project_dead_code_snapshot(&sqlite_path).expect("fresh store should project");

        let store = CallGraphStore::open_ready_no_rebuild(
            callgraph_store_dir_from_inspect_dir(&inspect_dir, &root).expect("store dir"),
            root.clone(),
        )
        .expect("reopen writer")
        .expect("ready writer");
        store.mark_files_stale(&files).expect("mark stale");
        drop(store);

        assert!(
            !manager.callgraph_ready_for_snapshot(&snapshot),
            "callgraph_ready must use the same stale-row predicate as dead_code projection"
        );
        let error =
            project_dead_code_snapshot(&sqlite_path).expect_err("stale rows must block projection");
        match error {
            CallGraphStoreError::Unavailable(message) => {
                assert_eq!(message, "callgraph has stale files pending refresh")
            }
            other => panic!("expected Unavailable, got {other:?}"),
        }
    }

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
    fn generation_keyed_projection_cache_reuses_unchanged_snapshot() {
        let (_dir, _root, _inspect_dir, job) = published_projection_fixture();
        let manager = InspectManager::new();
        let (projections, _observer_reset) = count_projections();

        let first = manager
            .build_tier2_callgraph_snapshot_with_refresh(&job, false, false, &[])
            .expect("first projection");
        let second = manager
            .build_tier2_callgraph_snapshot_with_refresh(&job, false, false, &[])
            .expect("cached projection");

        assert!(
            Arc::ptr_eq(&first, &second),
            "an unchanged generation and write revision must reuse the projected Arc"
        );
        assert_eq!(
            projections.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "two dead-code scans without a callgraph mutation must project once"
        );
        let memory = manager.callgraph_projection_estimated_memory();
        assert_eq!(
            memory.counts["callgraph_projection_snapshots"], 1,
            "the resident projection must be attributed to the root"
        );
        assert!(
            memory.estimated_bytes.unwrap_or_default() > 0,
            "a populated projection must report an estimated residency"
        );
    }

    #[test]
    fn projection_cache_invalidates_on_in_place_refresh_for_readonly_scans() {
        let (_dir, root, inspect_dir, job) = published_projection_fixture();
        let manager = InspectManager::new();
        let (projections, _observer_reset) = count_projections();
        let target = root.join("src/target.ts");
        let callgraph_dir =
            callgraph_store_dir_from_inspect_dir(&inspect_dir, &root).expect("callgraph dir");

        let first = manager
            .build_tier2_callgraph_snapshot_with_refresh(&job, false, false, &[])
            .expect("initial projection");
        let writer = CallGraphStore::open_ready_no_rebuild(callgraph_dir, root.clone())
            .expect("open writer")
            .expect("ready writer");
        let revision_before = writer
            .projection_write_revision()
            .expect("read initial revision")
            .expect("new stores write a projection revision");
        write_projection_cache_file(&target, "export function secondTarget() {}\n");
        writer
            .refresh_files(&[target])
            .expect("refresh changed target");
        let revision_after = writer
            .projection_write_revision()
            .expect("read refreshed revision")
            .expect("refreshed stores retain a projection revision");
        assert!(
            revision_after > revision_before,
            "the in-place refresh must advance the durable cache identity"
        );
        drop(writer);

        let second = manager
            .build_tier2_callgraph_snapshot_with_refresh(&job, false, false, &[])
            .expect("refreshed readonly projection");
        assert!(
            !first
                .exported_symbols
                .iter()
                .any(|export| export.symbol == "secondTarget"),
            "the initial snapshot must not already contain the refreshed export"
        );
        assert!(
            second
                .exported_symbols
                .iter()
                .any(|export| export.symbol == "secondTarget"),
            "the readonly scan must expose graph data from the refreshed store"
        );
        assert_eq!(
            projections.load(std::sync::atomic::Ordering::SeqCst),
            2,
            "an in-place refresh must force the next scan to re-project"
        );
    }

    #[test]
    fn projection_cache_invalidates_when_cold_build_publishes_new_generation() {
        let (_dir, root, inspect_dir, job) = published_projection_fixture();
        let manager = InspectManager::new();
        let (projections, _observer_reset) = count_projections();
        let target = root.join("src/target.ts");
        let callgraph_dir =
            callgraph_store_dir_from_inspect_dir(&inspect_dir, &root).expect("callgraph dir");

        let first = manager
            .build_tier2_callgraph_snapshot_with_refresh(&job, false, false, &[])
            .expect("initial projection");
        let before = CallGraphStore::open_readonly(callgraph_dir.clone(), root.clone())
            .expect("open initial reader")
            .expect("initial reader");
        let revision_before = before
            .projection_write_revision()
            .expect("read initial revision")
            .expect("new stores write a projection revision");
        drop(before);
        write_projection_cache_file(&target, "export function coldBuildTarget() {}\n");
        let files = crate::callgraph::walk_project_files(&root).collect::<Vec<_>>();
        let (published, _) =
            CallGraphStore::cold_build_with_lease(callgraph_dir, root.clone(), &files)
                .expect("publish replacement generation");
        let revision_after = published
            .projection_write_revision()
            .expect("read replacement revision")
            .expect("replacement stores write a projection revision");
        assert_eq!(
            revision_after, revision_before,
            "cold builds begin with the same revision, so this assertion exercises the generation half of the cache identity"
        );
        drop(published);

        let second = manager
            .build_tier2_callgraph_snapshot_with_refresh(&job, false, false, &[])
            .expect("replacement projection");
        assert!(
            !first
                .exported_symbols
                .iter()
                .any(|export| export.symbol == "coldBuildTarget"),
            "the initial snapshot must not already contain the replacement export"
        );
        assert!(
            second
                .exported_symbols
                .iter()
                .any(|export| export.symbol == "coldBuildTarget"),
            "the next scan must expose the generation published by the cold build"
        );
        assert_eq!(
            projections.load(std::sync::atomic::Ordering::SeqCst),
            2,
            "a new pointer generation must force the next scan to re-project"
        );
    }

    #[test]
    fn idle_eviction_drops_generation_keyed_projection_cache() {
        let (_dir, _root, _inspect_dir, job) = published_projection_fixture();
        let manager = InspectManager::new();
        let (projections, _observer_reset) = count_projections();

        let first = manager
            .build_tier2_callgraph_snapshot_with_refresh(&job, false, false, &[])
            .expect("initial projection");
        manager.evict_idle_caches();
        assert_eq!(
            manager.callgraph_projection_estimated_memory().counts
                ["callgraph_projection_snapshots"],
            0,
            "idle artifact eviction must release the root projection slot"
        );
        let second = manager
            .build_tier2_callgraph_snapshot_with_refresh(&job, false, false, &[])
            .expect("reloaded projection");

        assert!(
            !Arc::ptr_eq(&first, &second),
            "eviction must drop the previous projection Arc"
        );
        assert_eq!(
            projections.load(std::sync::atomic::Ordering::SeqCst),
            2,
            "the next scan after idle eviction must reload the projection"
        );
    }

    #[test]
    fn callgraph_snapshot_uses_ready_root_keyed_store() {
        let _git_env = crate::test_env::hermetic_git_env_guard();
        let dir = write_ts_project(3);
        let root = std::fs::canonicalize(dir.path()).expect("canonical root");
        let storage_dir = root.join(".aft-cache");
        let inspect_dir = storage_dir
            .join("inspect")
            .join(crate::path_identity::project_scope_key(&root));
        let warm_callgraph_dir = storage_dir
            .join("callgraph")
            .join(artifact_cache_key_for_test(&root));
        let store = CallGraphStore::open(warm_callgraph_dir, root.clone()).expect("open store");
        let files = crate::callgraph::walk_project_files(&root).collect::<Vec<_>>();
        store.cold_build(&files).expect("cold build store");

        let snapshot =
            build_tier2_callgraph_snapshot(&snapshot_job(&root, &inspect_dir, true), false)
                .expect("ready sibling store snapshot");

        assert_eq!(snapshot.files.len(), 3);
        assert_eq!(snapshot.exported_symbols.len(), 3);
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
        let callgraph_dir =
            callgraph_store_dir_from_inspect_dir(&inspect_dir, &root).expect("store dir");
        let project_key = crate::search_index::artifact_cache_key(&root);
        crate::root_cache::configure_artifact_access(&root, &project_key, false);
        let store = CallGraphStore::open(callgraph_dir.clone(), root.clone()).expect("open store");
        let project_files = crate::callgraph::walk_project_files(&root).collect::<Vec<_>>();
        store.cold_build(&project_files).expect("cold build store");
        drop(store);

        let config = Arc::new(crate::config::Config {
            project_root: Some(root.clone()),
            callgraph_store: true,
            ..crate::config::Config::default()
        });
        let symbol_cache = Arc::new(std::sync::RwLock::new(crate::parser::SymbolCache::new()));
        let snapshot = InspectSnapshot::new(
            root.clone(),
            inspect_dir.clone(),
            Arc::clone(&config),
            Arc::clone(&symbol_cache),
        );
        let manager = InspectManager::new();
        let initial_job =
            manager.tier2_reuse_job(snapshot.clone(), InspectCategory::DeadCode, None);
        let initial = manager
            .tier2_run_with_reuse_job_result_with_options(initial_job, Tier2ReuseOptions::default())
            .outcome
            .expect("initial dead_code scan succeeds")
            .aggregate;
        assert!(
            aggregate_has_file_symbol(&initial, "src/dead.ts", "plantedDead"),
            "initial scan should report the planted dead export: {initial:#}"
        );

        let deleted = root.join("src/dead.ts");
        std::fs::remove_file(&deleted).expect("delete dead fixture");
        let delete_job = manager.tier2_reuse_job(snapshot, InspectCategory::DeadCode, None);
        let refreshed = manager
            .tier2_run_with_reuse_job_result_with_options(
                delete_job,
                Tier2ReuseOptions {
                    force_rescan_paths: [deleted.clone()].into_iter().collect(),
                    allow_callgraph_cold_build: true,
                    require_callgraph_snapshot: false,
                    interactive: false,
                },
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

        let store = CallGraphStore::open_ready_no_rebuild(callgraph_dir, root)
            .expect("open refreshed store")
            .expect("refreshed store is ready");
        let projected = project_dead_code_snapshot(store.sqlite_path()).expect("project snapshot");
        assert!(
            projected
                .files
                .iter()
                .all(|file| !file.ends_with("src/dead.ts")),
            "watcher deletion should be applied to the persisted callgraph store: {:#?}",
            projected.files
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
    use crate::callgraph_store::{project_dead_code_snapshot, CallGraphStore};
    use crate::config::Config;
    use crate::inspect::job::DISPATCHED_CALLEE_SEPARATOR;
    use crate::inspect::scanners::DEFAULT_EXPORT_MARKER_KIND;
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
        let callgraph_dir =
            callgraph_store_dir_from_inspect_dir(&inspect_dir, &root).expect("store dir");
        let project_key = crate::search_index::artifact_cache_key(&root);
        crate::root_cache::configure_artifact_access(&root, &project_key, false);
        let store = CallGraphStore::open(callgraph_dir.clone(), root.clone()).expect("open store");
        let files = project_files(&root);
        store.cold_build(&files).expect("cold build store");
        let projected = project_dead_code_snapshot(store.sqlite_path()).expect("project snapshot");
        drop(store);

        let config = Arc::new(Config {
            project_root: Some(root.clone()),
            callgraph_store: true,
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

        let ready_payload = scoped_tier2_payload_from_contributions(
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

        std::fs::remove_dir_all(&callgraph_dir).expect("remove ready callgraph store");
        let unavailable_payload = scoped_tier2_payload_from_contributions(
            &snapshot,
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
            "missing store must report callgraph_unavailable instead of fabricating an empty graph: {unavailable_payload:#}"
        );
        assert_live_item(&unavailable_payload, "src/live.ts", "knownLive");
    }
    #[derive(Debug, PartialEq, Eq)]
    struct ComparableSnapshot {
        files: BTreeSet<PathBuf>,
        exported_symbols: BTreeSet<(PathBuf, String, String, u32)>,
        outbound_calls: BTreeSet<(PathBuf, String, String, u32)>,
        entry_points: BTreeSet<PathBuf>,
        entry_point_symbols: BTreeMap<PathBuf, BTreeSet<String>>,
    }

    #[test]
    fn dead_code_projection_contains_expected_fixture_surface() {
        let dir = tempfile::tempdir().expect("tempdir");
        write_projection_fixture(dir.path());
        let root = canonical_root(dir.path());
        let projected = store_projected_snapshot(&root, ".store-dead-code-surface");

        assert_projection_fixture_coverage(&root, &projected);
    }

    #[test]
    fn dead_code_projection_incremental_scenario_matrix_matches_cold_rebuild() {
        run_projection_scenario("rename", setup_projection_rename, edit_projection_rename);
        run_projection_scenario("delete", setup_projection_delete, edit_projection_delete);
        run_projection_scenario(
            "barrel delete",
            setup_projection_barrel,
            edit_projection_barrel_delete,
        );
        run_projection_scenario(
            "dispatch edit",
            setup_projection_dispatch,
            edit_projection_dispatch,
        );
        run_projection_scenario(
            "body-only edit",
            setup_projection_body_only,
            edit_projection_body_only,
        );
    }

    #[test]
    fn dead_code_projection_dead_code_scan_reports_expected_verdicts() {
        let dir = tempfile::tempdir().expect("tempdir");
        write_projection_fixture(dir.path());
        let root = canonical_root(dir.path());
        let files = project_files(&root);
        let projected = store_projected_snapshot(&root, ".store-dead-code-e2e");

        let projected_aggregate = dead_code_aggregate(&root, files, projected);
        assert_dead_item(&projected_aggregate, "src/dead.ts", "knownDead");
        assert_live_item(&projected_aggregate, "src/live.ts", "knownLive");
        assert_live_item(&projected_aggregate, "src/render.ts", "render");
        assert_live_item(&projected_aggregate, "src/other_render.ts", "render");
    }

    #[test]
    fn dead_code_projection_rust_attribute_entry_points_are_live() {
        let dir = tempfile::tempdir().expect("tempdir");
        write_rust_attribute_entry_fixture(dir.path());
        let root = canonical_root(dir.path());
        let files = project_files(&root);
        let store = CallGraphStore::open(root.join(".store-tauri-commands"), root.clone())
            .expect("open store");
        store.cold_build(&files).expect("cold build store");
        let command = store
            .node_for(Path::new("src/commands.rs"), "get_primers")
            .expect("command node");
        assert!(
            command.is_entry_point,
            "attribute-rooted commands must be labeled as callgraph entry points"
        );
        let private_command = store
            .node_for(Path::new("src/commands.rs"), "private_command")
            .expect("private command node");
        assert!(
            private_command.is_entry_point,
            "private attribute-rooted commands must also be callgraph entry points"
        );

        let projected = project_dead_code_snapshot(store.sqlite_path()).expect("project snapshot");
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
    fn dead_code_projection_rust_attribute_roots_are_cold_deterministic() {
        let dir = tempfile::tempdir().expect("tempdir");
        write_rust_attribute_entry_fixture(dir.path());
        let root = canonical_root(dir.path());
        let first = store_projected_snapshot(&root, ".store-tauri-cold-a");
        let second = store_projected_snapshot(&root, ".store-tauri-cold-b");

        assert_snapshot_parts_eq("rust attribute roots cold", &first, &second);
    }

    #[test]
    fn dead_code_projection_rust_attribute_roots_survive_unrelated_incremental_edit() {
        let dir = tempfile::tempdir().expect("tempdir");
        write_rust_attribute_entry_fixture(dir.path());
        let root = canonical_root(dir.path());
        let files_before = project_files(&root);
        let incremental_store =
            CallGraphStore::open(root.join(".store-tauri-incremental"), root.clone())
                .expect("open incremental store");
        incremental_store
            .cold_build(&files_before)
            .expect("initial cold build");

        write_file(
            &root.join("src/unrelated.rs"),
            r#"// unrelated edit should not refresh command attribute facts
pub fn unrelated() -> u32 { 2 }
"#,
        );
        let stats = incremental_store
            .refresh_files(&[root.join("src/unrelated.rs")])
            .expect("refresh unrelated file");
        assert_eq!(stats.refreshed_own_files, 1);
        assert_eq!(stats.changed_files, vec!["src/unrelated.rs".to_string()]);
        assert!(
            !stats
                .surface_changed
                .iter()
                .any(|file| file == "src/commands.rs"),
            "unrelated edit must not refresh the command module: {stats:#?}"
        );
        let incremental = project_dead_code_snapshot(incremental_store.sqlite_path())
            .expect("project incremental snapshot");

        let cold_store = CallGraphStore::open(root.join(".store-tauri-cold"), root.clone())
            .expect("open cold store");
        cold_store
            .cold_build(&project_files(&root))
            .expect("cold rebuild");
        let cold = project_dead_code_snapshot(cold_store.sqlite_path()).expect("project cold");
        assert_snapshot_parts_eq("rust attribute roots unrelated edit", &cold, &incremental);

        let aggregate = dead_code_aggregate(&root, project_files(&root), incremental);
        assert_live_item(&aggregate, "src/commands.rs", "get_primers");
        assert_live_item(&aggregate, "src/db.rs", "helper");
        assert_live_item(&aggregate, "src/db.rs", "private_helper");
        assert_dead_item(&aggregate, "src/commands.rs", "planted_dead");
    }

    fn assert_projection_fixture_coverage(root: &Path, snapshot: &CallgraphSnapshot) {
        let comparable = comparable_snapshot(snapshot);
        assert!(
            comparable
                .files
                .iter()
                .any(|file| file.extension().and_then(|ext| ext.to_str()) == Some("ts")),
            "fixture must include TypeScript files: {:#?}",
            comparable.files
        );
        assert!(
            comparable
                .files
                .iter()
                .any(|file| file.extension().and_then(|ext| ext.to_str()) == Some("js")),
            "fixture must include JavaScript files: {:#?}",
            comparable.files
        );
        assert!(
            comparable
                .files
                .iter()
                .any(|file| file.extension().and_then(|ext| ext.to_str()) == Some("rs")),
            "fixture must include Rust files: {:#?}",
            comparable.files
        );

        let main_file = canonicalize_for_snapshot(&root.join("src/main.ts"));
        let private_dispatch_target = format!("{}::dispatch", main_file.display());
        assert!(
            comparable
                .outbound_calls
                .iter()
                .any(
                    |(caller_file, caller_symbol, target, _)| caller_file == &main_file
                        && caller_symbol == "main"
                        && target == &private_dispatch_target
                ),
            "fixture must cover same-file private fallback target {private_dispatch_target}: {:#?}",
            comparable.outbound_calls
        );
        assert!(
            comparable
                .outbound_calls
                .iter()
                .any(|(_, _, target, _)| target.contains(DISPATCHED_CALLEE_SEPARATOR)),
            "fixture must cover method-dispatch suffixes: {:#?}",
            comparable.outbound_calls
        );
        assert!(
            comparable
                .exported_symbols
                .iter()
                .any(|(_, symbol, kind, _)| symbol == "runDefault"
                    && kind == DEFAULT_EXPORT_MARKER_KIND),
            "fixture must cover default-export marker rows: {:#?}",
            comparable.exported_symbols
        );
    }

    fn run_projection_scenario(name: &str, setup: fn(&Path), edit: fn(&Path) -> Vec<PathBuf>) {
        let dir = tempfile::tempdir().expect("tempdir");
        setup(dir.path());
        let root = canonical_root(dir.path());
        let files_before = project_files(&root);
        let incremental_store = CallGraphStore::open(
            root.join(format!(".store-dead-code-projection-{name}-incremental")),
            root.clone(),
        )
        .expect("open incremental store");
        incremental_store
            .cold_build(&files_before)
            .expect("initial cold build");

        let changed = edit(&root);
        incremental_store
            .refresh_files(&changed)
            .expect("refresh changed files");
        let incremental = project_dead_code_snapshot(incremental_store.sqlite_path())
            .expect("project incremental snapshot");

        let cold_store = CallGraphStore::open(
            root.join(format!(".store-dead-code-projection-{name}-cold")),
            root.clone(),
        )
        .expect("open cold store");
        cold_store
            .cold_build(&project_files(&root))
            .expect("cold rebuild");
        let cold =
            project_dead_code_snapshot(cold_store.sqlite_path()).expect("project cold snapshot");

        assert_snapshot_parts_eq(name, &cold, &incremental);
    }

    /// Store-backed dead_code benchmark. Measures, on a real checkout, the
    /// persisted-store cold build, the warm SQLite projection cost, and the
    /// remaining `run_dead_code_scan` cost (per-file reexport/type-ref reparse +
    /// BFS roll-up). Production Tier-2 reads a warm store; cold_build is included
    /// here only to make end-to-end store cost visible.
    /// Ignored by default; run with:
    ///   AFT_BENCH_REPO=/path/to/large/repo cargo test -p agent-file-tools --lib \
    ///     -- --ignored --nocapture --test-threads=1 dead_code_decision_b_benchmark
    #[test]
    #[ignore = "manual benchmark; needs AFT_BENCH_REPO pointing at a large checkout"]
    fn dead_code_decision_b_benchmark() {
        let Ok(repo) = std::env::var("AFT_BENCH_REPO") else {
            eprintln!("AFT_BENCH_REPO unset; skipping");
            return;
        };
        // Each phase flushes immediately so a file-redirected run shows live progress.
        macro_rules! mark {
            ($($a:tt)*) => {{ eprintln!($($a)*); let _ = std::io::Write::flush(&mut std::io::stderr()); }};
        }
        let root = canonical_root(Path::new(&repo));
        let files = project_files(&root);
        mark!(
            "\n=== Store-backed dead_code benchmark ===\nrepo: {}\nsource files (walk_project_files): {}\nstarted store cold_build...",
            root.display(),
            files.len()
        );

        // Store cold_build + projection. Production warm runs skip cold_build and
        // pay only the projection below.
        let store_dir = root.join(".aft-bench-store");
        let _ = std::fs::remove_dir_all(&store_dir);
        let store = CallGraphStore::open(store_dir.clone(), root.clone()).expect("open store");
        let t = Instant::now();
        let cold_stats = store.cold_build(&files).expect("store cold build");
        let store_build_ms = t.elapsed().as_millis();
        let t = Instant::now();
        let projected = project_dead_code_snapshot(store.sqlite_path()).expect("projection");
        let proj_ms = t.elapsed().as_millis();
        mark!(
            "store cold_build: {} ms ({:?}) + projection: {} ms = {} ms  (exports={}, outbound={})\nstarted scan...",
            store_build_ms, cold_stats, proj_ms, store_build_ms + proj_ms,
            projected.exported_symbols.len(), projected.outbound_calls.len()
        );

        // Remaining scanner cost: run_dead_code_scan given a ready snapshot.
        let t = Instant::now();
        let _result = dead_code_aggregate(&root, files.clone(), projected.clone());
        let scan_ms = t.elapsed().as_millis();
        mark!("run_dead_code_scan (cold contributions): {} ms", scan_ms);

        mark!(
            "\nSUMMARY  files={}  store_cold_plus_projection={}ms  projection={}ms  scan_cold={}ms  total={}ms",
            files.len(),
            store_build_ms + proj_ms,
            proj_ms,
            scan_ms,
            store_build_ms + proj_ms + scan_ms
        );
        let _ = std::fs::remove_dir_all(&store_dir);
    }

    fn store_projected_snapshot(root: &Path, store_name: &str) -> CallgraphSnapshot {
        let store =
            CallGraphStore::open(root.join(store_name), root.to_path_buf()).expect("open store");
        store
            .cold_build(&project_files(root))
            .expect("store cold build");
        project_dead_code_snapshot(store.sqlite_path()).expect("project snapshot")
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

    fn assert_snapshot_parts_eq(
        label: &str,
        expected: &CallgraphSnapshot,
        actual: &CallgraphSnapshot,
    ) {
        let expected = comparable_snapshot(expected);
        let actual = comparable_snapshot(actual);
        assert_eq!(
            actual, expected,
            "{label} store-projected snapshot must match cold store snapshot"
        );
    }

    fn comparable_snapshot(snapshot: &CallgraphSnapshot) -> ComparableSnapshot {
        ComparableSnapshot {
            files: snapshot.files.iter().cloned().collect(),
            exported_symbols: snapshot
                .exported_symbols
                .iter()
                .map(|export| {
                    (
                        export.file.clone(),
                        export.symbol.clone(),
                        export.kind.clone(),
                        export.line,
                    )
                })
                .collect(),
            outbound_calls: snapshot
                .outbound_calls
                .iter()
                .map(|call| {
                    (
                        call.caller_file.clone(),
                        call.caller_symbol.clone(),
                        call.target.clone(),
                        call.line,
                    )
                })
                .collect(),
            entry_points: snapshot.entry_points.clone(),
            entry_point_symbols: snapshot.entry_point_symbols.clone(),
        }
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

    fn remove_file(path: &Path) {
        std::fs::remove_file(path).expect("remove fixture");
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

    fn setup_projection_rename(root: &Path) {
        write_file(
            &root.join("a.ts"),
            r#"export function outer() {
  inner();
}

export function inner() {}
"#,
        );
    }

    fn edit_projection_rename(root: &Path) -> Vec<PathBuf> {
        let path = root.join("a.ts");
        write_file(
            &path,
            r#"export function outer() {
  renamed();
}

export function renamed() {}
"#,
        );
        vec![path]
    }

    fn setup_projection_delete(root: &Path) {
        write_file(
            &root.join("main.ts"),
            r#"import { foo } from "./foo";
export function main() { foo(); }
"#,
        );
        write_file(&root.join("foo.ts"), "export function foo() {}\n");
    }

    fn edit_projection_delete(root: &Path) -> Vec<PathBuf> {
        let path = root.join("foo.ts");
        remove_file(&path);
        vec![path]
    }

    fn setup_projection_barrel(root: &Path) {
        write_file(
            &root.join("main.ts"),
            r#"import { foo } from "./barrel";
export function main() { foo(); }
"#,
        );
        write_file(&root.join("barrel.ts"), "export { foo } from \"./foo\";\n");
        write_file(&root.join("foo.ts"), "export function foo() {}\n");
    }

    fn edit_projection_barrel_delete(root: &Path) -> Vec<PathBuf> {
        let path = root.join("barrel.ts");
        remove_file(&path);
        vec![path]
    }

    fn setup_projection_dispatch(root: &Path) {
        write_file(
            &root.join("main.ts"),
            r#"export function main() {
  const service = { render() {}, paint() {} };
  service.render();
}
"#,
        );
        write_file(&root.join("render.ts"), "export function render() {}\n");
        write_file(&root.join("paint.ts"), "export function paint() {}\n");
    }

    fn edit_projection_dispatch(root: &Path) -> Vec<PathBuf> {
        let path = root.join("main.ts");
        write_file(
            &path,
            r#"export function main() {
  const service = { render() {}, paint() {} };
  service.paint();
}
"#,
        );
        vec![path]
    }

    fn setup_projection_body_only(root: &Path) {
        write_file(
            &root.join("main.ts"),
            r#"import { foo } from "./foo";
export function main() { foo(); }
"#,
        );
        write_file(
            &root.join("foo.ts"),
            r#"export function foo() {
  return 1;
}
"#,
        );
    }

    fn edit_projection_body_only(root: &Path) -> Vec<PathBuf> {
        let path = root.join("foo.ts");
        write_file(
            &path,
            r#"export function foo() {
  return 2;
}
"#,
        );
        vec![path]
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
}
