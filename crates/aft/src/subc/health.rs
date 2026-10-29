//! Dispatch-path metrics and health-report helpers for the subc transport loop.

use std::collections::BTreeMap;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use super::{
    json, Arc, AtomicBool, AtomicU64, AtomicUsize, BgSubsBySession, Duration, Executor, Frame,
    HashMap, HealthReport, HealthStatus, Instant, Ordering, PendingBind, ProjectRootId,
    RootHealthSnapshot, RouteChannel, StdMutex, Value, DISPATCH_PATH_BIND_WARN_AFTER,
    WRITER_QUEUE_CAPACITY,
};
use crate::context::{App, AppContext, ArtifactEvictionBlocker};
use crate::executor::BindBlockerSnapshot;

const EXECUTOR_SCHEDULER_CONTENTION_DETAIL: &str =
    "executor scheduler state could not be snapshotted without contention";
const UNANSWERED_TOOL_DEADLINE: Duration = Duration::from_secs(25);
const BASH_WATCH_UNANSWERED_GRACE: Duration = Duration::from_secs(5);

/// Why the idle reaper kept an existing, unbound root that is past its idle
/// TTL. Deleted roots are counted separately in the census's `blockers`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum UnboundRetention {
    BashWaits,
    MaintenancePending,
    MaintenanceQueued,
    PendingBind,
    ActorBusy,
    Artifact(ArtifactEvictionBlocker),
    ArtifactEvictionFailed,
}

const UNBOUND_RETENTION_KINDS: usize = 14;

const UNBOUND_RETENTION_LABELS: [&str; UNBOUND_RETENTION_KINDS] = [
    "bash_waits",
    "maintenance_pending",
    "maintenance_queued",
    "pending_binds",
    "actor_busy",
    "standing_root",
    "build_in_flight",
    "semantic_refresh",
    "inspect_tier2",
    "bash_running",
    "pending_reconciliation",
    "search_delta_unpersisted",
    "artifact_eviction_failed",
    "lock_contended",
];

impl UnboundRetention {
    fn index(self) -> usize {
        match self {
            Self::BashWaits => 0,
            Self::MaintenancePending => 1,
            Self::MaintenanceQueued => 2,
            Self::PendingBind => 3,
            Self::ActorBusy => 4,
            Self::Artifact(ArtifactEvictionBlocker::StandingRoot) => 5,
            Self::Artifact(ArtifactEvictionBlocker::BuildInFlight) => 6,
            Self::Artifact(ArtifactEvictionBlocker::SemanticRefresh) => 7,
            Self::Artifact(ArtifactEvictionBlocker::InspectTier2) => 8,
            Self::Artifact(ArtifactEvictionBlocker::BashRunning) => 9,
            Self::Artifact(ArtifactEvictionBlocker::PendingReconciliation) => 10,
            Self::Artifact(ArtifactEvictionBlocker::SearchDeltaUnpersisted) => 11,
            Self::ArtifactEvictionFailed => 12,
            Self::Artifact(ArtifactEvictionBlocker::LockContended) => 13,
        }
    }

    pub(super) fn label(self) -> &'static str {
        UNBOUND_RETENTION_LABELS[self.index()]
    }
}

#[derive(Clone, Copy, Debug)]
pub(super) enum DeletedTaskRetention {
    KillInFlight,
    KillBackoff,
    RegistryBusy,
}

const DELETED_TASK_RETENTION_LABELS: [&str; 3] = [
    "task_kill_in_flight",
    "task_kill_backoff",
    "task_registry_busy",
];

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct ReapBlockerCensus {
    pub(super) deleted_retained: usize,
    pub(super) absence_unconfirmed: usize,
    pub(super) bound_routes: usize,
    pub(super) unbound_quiesced: usize,
    pub(super) bash_waits: usize,
    pub(super) maintenance_pending: usize,
    pub(super) maintenance_queued: usize,
    pub(super) pending_binds: usize,
    pub(super) actor_busy: usize,
    pub(super) actor_state_busy: usize,
    pub(super) artifact_eviction_blocked: usize,
    pub(super) artifact_eviction_failed: usize,
    deleted_task_blockers: [usize; 3],
    /// Existing roots with no bound route that are past their idle TTL but
    /// were not evicted, counted once each under the reason in
    /// `unbound_blockers`.
    pub(super) unbound_retained: usize,
    pub(super) unbound_blockers: [usize; UNBOUND_RETENTION_KINDS],
}

impl ReapBlockerCensus {
    pub(super) fn note_deleted_task_retained(&mut self, reason: DeletedTaskRetention) {
        self.deleted_retained += 1;
        self.artifact_eviction_blocked += 1;
        self.deleted_task_blockers[reason as usize] += 1;
    }

    pub(super) fn note_unbound_retained(&mut self, reason: UnboundRetention) {
        self.unbound_retained += 1;
        self.unbound_blockers[reason.index()] += 1;
    }

    pub(super) fn unbound_histogram(&self) -> String {
        UNBOUND_RETENTION_LABELS
            .iter()
            .zip(self.unbound_blockers.iter())
            .map(|(label, count)| format!("{label}={count}"))
            .collect::<Vec<_>>()
            .join(",")
    }
    pub(super) fn blocker_histogram(&self) -> String {
        format!(
            "absence_unconfirmed={},bound_routes={},unbound_quiesced={},bash_waits={},maintenance_pending={},maintenance_queued={},pending_binds={},actor_busy={},actor_state_busy={},artifact_eviction_blocked={},artifact_eviction_failed={},task_kill_in_flight={},task_kill_backoff={},task_registry_busy={}",
            self.absence_unconfirmed,
            self.bound_routes,
            self.unbound_quiesced,
            self.bash_waits,
            self.maintenance_pending,
            self.maintenance_queued,
            self.pending_binds,
            self.actor_busy,
            self.actor_state_busy,
            self.artifact_eviction_blocked,
            self.artifact_eviction_failed,
            self.deleted_task_blockers[0],
            self.deleted_task_blockers[1],
            self.deleted_task_blockers[2],
        )
    }
}

struct ReapMetrics {
    last_sweep_ms: AtomicU64,
    /// Rendered `<retained>;<blockers>` of the last sweep, so the retained-roots
    /// log line is emitted on change only. Sweeps run several times a second
    /// during route churn and an unchanged census wrote ~2,000 identical lines
    /// per hour before this.
    last_retained_summary: StdMutex<String>,
    deleted_retained: AtomicUsize,
    absence_unconfirmed: AtomicUsize,
    bound_routes: AtomicUsize,
    unbound_quiesced: AtomicUsize,
    bash_waits: AtomicUsize,
    maintenance_pending: AtomicUsize,
    maintenance_queued: AtomicUsize,
    pending_binds: AtomicUsize,
    actor_busy: AtomicUsize,
    actor_state_busy: AtomicUsize,
    artifact_eviction_blocked: AtomicUsize,
    artifact_eviction_failed: AtomicUsize,
    deleted_task_blockers: [AtomicUsize; 3],
    unbound_retained: AtomicUsize,
    unbound_blockers: [AtomicUsize; UNBOUND_RETENTION_KINDS],
}

impl ReapMetrics {
    fn new() -> Self {
        Self {
            last_sweep_ms: AtomicU64::new(0),
            last_retained_summary: StdMutex::new(String::new()),
            deleted_retained: AtomicUsize::new(0),
            absence_unconfirmed: AtomicUsize::new(0),
            bound_routes: AtomicUsize::new(0),
            unbound_quiesced: AtomicUsize::new(0),
            bash_waits: AtomicUsize::new(0),
            maintenance_pending: AtomicUsize::new(0),
            maintenance_queued: AtomicUsize::new(0),
            pending_binds: AtomicUsize::new(0),
            actor_busy: AtomicUsize::new(0),
            actor_state_busy: AtomicUsize::new(0),
            artifact_eviction_blocked: AtomicUsize::new(0),
            artifact_eviction_failed: AtomicUsize::new(0),
            deleted_task_blockers: std::array::from_fn(|_| AtomicUsize::new(0)),
            unbound_retained: AtomicUsize::new(0),
            unbound_blockers: std::array::from_fn(|_| AtomicUsize::new(0)),
        }
    }

    /// Stores the census; returns whether its retained-roots summary differs
    /// from the previous sweep's.
    fn record(&self, now_ms: u64, census: ReapBlockerCensus) -> bool {
        self.last_sweep_ms.store(now_ms, Ordering::Relaxed);
        let summary = format!(
            "{};{};{};{}",
            census.deleted_retained,
            census.blocker_histogram(),
            census.unbound_retained,
            census.unbound_histogram()
        );
        let changed = {
            let mut last = self
                .last_retained_summary
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let changed = *last != summary;
            *last = summary;
            changed
        };
        self.deleted_retained
            .store(census.deleted_retained, Ordering::Relaxed);
        self.absence_unconfirmed
            .store(census.absence_unconfirmed, Ordering::Relaxed);
        self.bound_routes
            .store(census.bound_routes, Ordering::Relaxed);
        self.unbound_quiesced
            .store(census.unbound_quiesced, Ordering::Relaxed);
        self.bash_waits.store(census.bash_waits, Ordering::Relaxed);
        self.maintenance_pending
            .store(census.maintenance_pending, Ordering::Relaxed);
        self.maintenance_queued
            .store(census.maintenance_queued, Ordering::Relaxed);
        self.pending_binds
            .store(census.pending_binds, Ordering::Relaxed);
        self.actor_busy.store(census.actor_busy, Ordering::Relaxed);
        self.actor_state_busy
            .store(census.actor_state_busy, Ordering::Relaxed);
        self.artifact_eviction_blocked
            .store(census.artifact_eviction_blocked, Ordering::Relaxed);
        self.artifact_eviction_failed
            .store(census.artifact_eviction_failed, Ordering::Relaxed);
        for (slot, count) in self
            .deleted_task_blockers
            .iter()
            .zip(census.deleted_task_blockers)
        {
            slot.store(count, Ordering::Relaxed);
        }
        self.unbound_retained
            .store(census.unbound_retained, Ordering::Relaxed);
        for (slot, count) in self
            .unbound_blockers
            .iter()
            .zip(census.unbound_blockers.iter())
        {
            slot.store(*count, Ordering::Relaxed);
        }
        changed
    }

    fn snapshot(&self) -> Value {
        let deleted_task_blockers = DELETED_TASK_RETENTION_LABELS
            .iter()
            .zip(self.deleted_task_blockers.iter())
            .map(|(label, count)| ((*label).to_string(), json!(count.load(Ordering::Relaxed))))
            .collect::<serde_json::Map<_, _>>();
        let unbound_blockers = UNBOUND_RETENTION_LABELS
            .iter()
            .zip(self.unbound_blockers.iter())
            .map(|(label, count)| ((*label).to_string(), json!(count.load(Ordering::Relaxed))))
            .collect::<serde_json::Map<_, _>>();
        json!({
            "deleted_retained": self.deleted_retained.load(Ordering::Relaxed),
            "deleted_task_blockers": deleted_task_blockers,
            "blockers": {
                "absence_unconfirmed": self.absence_unconfirmed.load(Ordering::Relaxed),
                "bound_routes": self.bound_routes.load(Ordering::Relaxed),
                "unbound_quiesced": self.unbound_quiesced.load(Ordering::Relaxed),
                "bash_waits": self.bash_waits.load(Ordering::Relaxed),
                "maintenance_pending": self.maintenance_pending.load(Ordering::Relaxed),
                "maintenance_queued": self.maintenance_queued.load(Ordering::Relaxed),
                "pending_binds": self.pending_binds.load(Ordering::Relaxed),
                "actor_busy": self.actor_busy.load(Ordering::Relaxed),
                "actor_state_busy": self.actor_state_busy.load(Ordering::Relaxed),
                "artifact_eviction_blocked": self.artifact_eviction_blocked.load(Ordering::Relaxed),
                "artifact_eviction_failed": self.artifact_eviction_failed.load(Ordering::Relaxed),
            },
            "unbound_retained": self.unbound_retained.load(Ordering::Relaxed),
            "unbound_blockers": unbound_blockers,
            "last_sweep_ms": self.last_sweep_ms.load(Ordering::Relaxed),
        })
    }
}

const BG_OBSERVABILITY_INTERVAL: Duration = Duration::from_secs(60);
const STUCK_PENDING_WATCH_AGE: Duration = Duration::from_secs(10 * 60);
const STUCK_PENDING_WATCH_SCAN_INTERVAL: Duration = Duration::from_secs(60);
const STUCK_PENDING_WATCH_LOG_INTERVAL: Duration = Duration::from_secs(10 * 60);
const INTERACTIVE_OCCUPANCY_WARN_AFTER: Duration = Duration::from_secs(60);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum BgEventKind {
    ArmHit,
    ArmMiss,
    NudgeEnqueued,
    SubscriptionInstalled,
    SubscriptionEnded,
}

impl BgEventKind {
    fn index(self) -> usize {
        match self {
            Self::ArmHit => 0,
            Self::ArmMiss => 1,
            Self::NudgeEnqueued => 2,
            Self::SubscriptionInstalled => 3,
            Self::SubscriptionEnded => 4,
        }
    }
}

const BG_EVENT_RATE_BUCKETS: usize = 60;

#[derive(Clone, Copy, Default)]
struct BgEventRateBucket {
    second: u64,
    counts: [u64; 5],
}

/// Fixed-size rolling counters keep 60-second wake metrics live without
/// traversing the per-root observability map on every probe.
struct BgEventRates {
    origin: Instant,
    buckets: StdMutex<[BgEventRateBucket; BG_EVENT_RATE_BUCKETS]>,
}

impl BgEventRates {
    fn new() -> Self {
        Self {
            origin: Instant::now(),
            buckets: StdMutex::new([BgEventRateBucket::default(); BG_EVENT_RATE_BUCKETS]),
        }
    }

    fn second_at(&self, now: Instant) -> u64 {
        now.saturating_duration_since(self.origin).as_secs()
    }

    fn record(&self, kind: BgEventKind, now: Instant) {
        let second = self.second_at(now);
        let Ok(mut buckets) = self.buckets.lock() else {
            return;
        };
        let bucket = &mut buckets[(second as usize) % BG_EVENT_RATE_BUCKETS];
        if bucket.second != second {
            *bucket = BgEventRateBucket {
                second,
                counts: [0; 5],
            };
        }
        bucket.counts[kind.index()] = bucket.counts[kind.index()].saturating_add(1);
    }

    fn count_60s(&self, kind: BgEventKind) -> u64 {
        let second = self.second_at(Instant::now());
        let Ok(buckets) = self.buckets.try_lock() else {
            return 0;
        };
        buckets
            .iter()
            .filter(|bucket| second.saturating_sub(bucket.second) < 60)
            .map(|bucket| bucket.counts[kind.index()])
            .fold(0u64, u64::saturating_add)
    }
}

#[derive(Debug, PartialEq, Eq, Hash)]
struct BgEventKey {
    root: ProjectRootId,
    session: String,
    kind: BgEventKind,
}

struct BgEventRecord {
    window_start: Instant,
    event_count: u64,
    suppressed: u64,
}

#[cfg(test)]
thread_local! {
    static BG_OBSERVABILITY_TEST_LOGS: std::cell::RefCell<Vec<String>> = const { std::cell::RefCell::new(Vec::new()) };
}

fn emit_bg_observability_info(line: String) {
    log::info!("{line}");
    #[cfg(test)]
    BG_OBSERVABILITY_TEST_LOGS.with(|logs| logs.borrow_mut().push(line));
}

#[cfg(test)]
pub(super) fn take_bg_observability_logs_for_test() -> Vec<String> {
    BG_OBSERVABILITY_TEST_LOGS.with(|logs| std::mem::take(&mut *logs.borrow_mut()))
}

/// How far back the health report looks at route-bind answer latency.
const BIND_ACK_WINDOW: Duration = Duration::from_secs(5 * 60);
/// A bind answered slower than this counts as slow. The daemon refuses a
/// module's binds once relays pass 12 s, so a rising slow count is the early
/// warning before that happens.
const BIND_ACK_SLOW_THRESHOLD: Duration = Duration::from_secs(5);
/// Upper bound on retained samples; a restart storm is a few hundred binds.
const BIND_ACK_MAX_SAMPLES: usize = 4096;

/// Upper bounds, in milliseconds, of the bind-ack latency histogram buckets.
/// One more bucket holds everything slower than the last bound. The bounds are
/// dense around the daemon's 12 s relay limit and the 5 s slow threshold.
const BIND_ACK_BUCKET_BOUNDS_MS: [u64; 16] = [
    10, 25, 50, 100, 250, 500, 1_000, 2_000, 3_000, 5_000, 8_000, 10_000, 12_000, 20_000, 30_000,
    60_000,
];

/// Fixed-bucket latency histogram. Percentiles are reported as the upper bound
/// of the bucket the percentile falls in (the observed worst for the overflow
/// bucket), so recording costs one array increment and reporting never sorts.
#[derive(Debug, Default, Clone)]
struct LatencyHistogram {
    counts: [u64; BIND_ACK_BUCKET_BOUNDS_MS.len() + 1],
    total: u64,
}

impl LatencyHistogram {
    fn record(&mut self, latency: Duration) {
        let latency_ms = duration_millis_u64(latency);
        let bucket = BIND_ACK_BUCKET_BOUNDS_MS
            .iter()
            .position(|bound| latency_ms <= *bound)
            .unwrap_or(BIND_ACK_BUCKET_BOUNDS_MS.len());
        self.counts[bucket] = self.counts[bucket].saturating_add(1);
        self.total = self.total.saturating_add(1);
    }

    /// The bucket bound at or below which `percent` of the recorded latencies
    /// fall; `None` when nothing was recorded.
    fn percentile_ms(&self, percent: u64, worst: Duration) -> Option<u64> {
        if self.total == 0 {
            return None;
        }
        // Nearest-rank: the smallest bucket holding at least this many samples.
        let rank = (self.total.saturating_mul(percent)).div_ceil(100).max(1);
        let mut seen = 0u64;
        for (bucket, count) in self.counts.iter().enumerate() {
            seen = seen.saturating_add(*count);
            if seen >= rank {
                return Some(
                    BIND_ACK_BUCKET_BOUNDS_MS
                        .get(bucket)
                        .copied()
                        .unwrap_or_else(|| duration_millis_u64(worst)),
                );
            }
        }
        Some(duration_millis_u64(worst))
    }

    fn percentiles(&self, worst: Duration) -> Value {
        json!({
            "p50_ms": self.percentile_ms(50, worst),
            "p90_ms": self.percentile_ms(90, worst),
            "p99_ms": self.percentile_ms(99, worst),
        })
    }
}

/// Route-bind answer latencies over the last `BIND_ACK_WINDOW`, plus the
/// worst latency and a latency histogram since the process started.
#[derive(Debug, Default)]
struct BindAckLatencies {
    samples: std::collections::VecDeque<(Instant, Duration)>,
    worst_since_start: Duration,
    total: u64,
    slow_total: u64,
    since_start: LatencyHistogram,
}

impl BindAckLatencies {
    fn record(&mut self, now: Instant, latency: Duration) {
        self.prune(now);
        if self.samples.len() >= BIND_ACK_MAX_SAMPLES {
            self.samples.pop_front();
        }
        self.samples.push_back((now, latency));
        self.worst_since_start = self.worst_since_start.max(latency);
        self.since_start.record(latency);
        self.total = self.total.saturating_add(1);
        if latency > BIND_ACK_SLOW_THRESHOLD {
            self.slow_total = self.slow_total.saturating_add(1);
        }
    }

    fn prune(&mut self, now: Instant) {
        while self
            .samples
            .front()
            .is_some_and(|(at, _)| now.saturating_duration_since(*at) > BIND_ACK_WINDOW)
        {
            self.samples.pop_front();
        }
    }

    fn snapshot(&mut self, now: Instant) -> Value {
        self.prune(now);
        let slow_in_window = self
            .samples
            .iter()
            .filter(|(_, latency)| *latency > BIND_ACK_SLOW_THRESHOLD)
            .count();
        let worst_in_window_ms = self
            .samples
            .iter()
            .map(|(_, latency)| duration_millis_u64(*latency))
            .max();
        // The window's samples are already retained for the counts above, so
        // its histogram is rebuilt here from them into the same buckets.
        let mut window = LatencyHistogram::default();
        let mut worst_in_window = Duration::ZERO;
        for (_, latency) in &self.samples {
            window.record(*latency);
            worst_in_window = worst_in_window.max(*latency);
        }
        json!({
            "window_s": BIND_ACK_WINDOW.as_secs(),
            "slow_threshold_ms": duration_millis_u64(BIND_ACK_SLOW_THRESHOLD),
            "count": self.samples.len(),
            "slow_count": slow_in_window,
            "worst_ms": worst_in_window_ms,
            "worst_since_start_ms": (self.total > 0)
                .then(|| duration_millis_u64(self.worst_since_start)),
            "total": self.total,
            "slow_total": self.slow_total,
            "percentiles": window.percentiles(worst_in_window),
            "percentiles_since_start": self.since_start.percentiles(self.worst_since_start),
            "percentile_bucket_bounds_ms": BIND_ACK_BUCKET_BOUNDS_MS,
        })
    }
}

#[derive(Default)]
struct FrameOperation {
    corr: u64,
    op: &'static str,
    root: String,
    session: String,
    awaited: &'static str,
    awaited_root: String,
    holder: Option<Arc<crate::lock_diagnostics::LockDiagnostics>>,
}

pub(super) struct DispatchPathMetrics {
    pub(super) watch_delivery: Arc<super::watch_delivery::WatchDelivery>,
    unanswered_tools: StdMutex<HashMap<(super::RouteChannel, u64), UnansweredTool>>,
    pub(super) origin: Instant,
    pub(super) frame_loop_last_tick_ms: AtomicU64,
    /// When the frame loop next promised to wake (its drain-tick timer), as
    /// milliseconds since `origin` plus one; `0` means no loop is running.
    frame_loop_wake_deadline_ms_plus_one: AtomicU64,
    /// Diagnostic context only, not another progress signal. Zero means turn
    /// work, one means select wait, and two means handling a received frame.
    frame_loop_phase: AtomicUsize,
    /// Best-effort header context; no locks are taken by the watchdog reader.
    frame_loop_frame_header: AtomicU64,
    frame_loop_frame_corr: AtomicU64,
    frame_loop_step: super::work_step::WorkStep,
    /// Kept separate from scheduler and bash-registry locks. Publishers and the
    /// watchdog use try_lock so evidence collection cannot block either thread.
    frame_operation: StdMutex<FrameOperation>,
    /// Stall counts published by the stall watchdog thread.
    pub(super) stall_stats: Arc<super::stall_watchdog::StallStats>,
    pub(super) writer_queued: AtomicUsize,
    pub(super) writer_active: AtomicBool,
    pub(super) writer_saturation_count: AtomicU64,
    root_broadcast_over_1mib_total: AtomicU64,
    pub(super) status_push_compacted_total: AtomicU64,
    pub(super) status_push_aggregate_limited_total: AtomicU64,
    root_broadcast_warned: StdMutex<HashMap<(ProjectRootId, &'static str), Instant>>,
    pub(super) control_completion_queued: AtomicUsize,
    pub(super) maintenance_queued: AtomicUsize,
    pub(super) bash_deferred_queued: AtomicUsize,
    pub(super) bash_poll_touch_queued: AtomicUsize,
    pub(super) deferred_bash_waits_in_flight: AtomicUsize,
    /// Bash calls whose route request is still waiting for a terminal frame.
    pub(super) held_bash_calls: super::drain::HeldBashCalls,
    /// Test-only probe that learns when a bash call reaches its foreground
    /// wait. Production never installs one; integration tests wait on its
    /// `BashForegroundWait` event instead of sleeping past the spawn job.
    pub(super) lifecycle_probe: StdMutex<Option<super::SubcTestLifecycleProbe>>,
    pub(super) reliable_push_budget_deferrals: AtomicU64,
    pub(super) maintenance_budget_deferrals: AtomicU64,
    pub(super) response_tasks_live: AtomicUsize,
    bg_subscriptions: AtomicUsize,
    bg_wake_pending: AtomicUsize,
    pub(super) bg_wake_unacked_total: AtomicUsize,
    bg_wake_rearm_total: AtomicU64,
    stuck_pending_watch_next_scan_ms: AtomicU64,
    stuck_pending_watch_scan_running: AtomicBool,
    stuck_pending_watch_logs: StdMutex<HashMap<String, Instant>>,
    bg_events: StdMutex<HashMap<BgEventKey, BgEventRecord>>,
    bg_event_rates: BgEventRates,
    reap: ReapMetrics,
    bind_acks: StdMutex<BindAckLatencies>,
    /// Tool calls that named no catalog preset on a route without a scope
    /// stamp (so they ran under the unscoped default), by the route's
    /// harness. Shows when the last caller starts naming one.
    presetless_tool_calls: StdMutex<BTreeMap<String, u64>>,
    /// Routes bound with a daemon scope stamp that made at least one tool
    /// call, each counted once.
    scoped_routes_with_tool_calls: AtomicU64,
}

impl DispatchPathMetrics {
    pub(super) fn new() -> Self {
        Self {
            watch_delivery: Arc::default(),
            unanswered_tools: StdMutex::new(HashMap::new()),
            origin: Instant::now(),
            frame_loop_last_tick_ms: AtomicU64::new(0),
            frame_loop_wake_deadline_ms_plus_one: AtomicU64::new(0),
            frame_loop_phase: AtomicUsize::new(0),
            frame_loop_frame_header: AtomicU64::new(0),
            frame_loop_frame_corr: AtomicU64::new(0),
            frame_loop_step: super::work_step::WorkStep::default(),
            frame_operation: StdMutex::new(FrameOperation::default()),
            stall_stats: Arc::default(),
            writer_queued: AtomicUsize::new(0),
            writer_active: AtomicBool::new(false),
            writer_saturation_count: AtomicU64::new(0),
            root_broadcast_over_1mib_total: AtomicU64::new(0),
            status_push_compacted_total: AtomicU64::new(0),
            status_push_aggregate_limited_total: AtomicU64::new(0),
            root_broadcast_warned: StdMutex::new(HashMap::new()),
            control_completion_queued: AtomicUsize::new(0),
            maintenance_queued: AtomicUsize::new(0),
            bash_deferred_queued: AtomicUsize::new(0),
            bash_poll_touch_queued: AtomicUsize::new(0),
            deferred_bash_waits_in_flight: AtomicUsize::new(0),
            held_bash_calls: super::drain::HeldBashCalls::default(),
            lifecycle_probe: StdMutex::new(None),
            reliable_push_budget_deferrals: AtomicU64::new(0),
            maintenance_budget_deferrals: AtomicU64::new(0),
            response_tasks_live: AtomicUsize::new(0),
            bg_subscriptions: AtomicUsize::new(0),
            bg_wake_pending: AtomicUsize::new(0),
            bg_wake_unacked_total: AtomicUsize::new(0),
            bg_wake_rearm_total: AtomicU64::new(0),
            stuck_pending_watch_next_scan_ms: AtomicU64::new(0),
            stuck_pending_watch_scan_running: AtomicBool::new(false),
            stuck_pending_watch_logs: StdMutex::new(HashMap::new()),
            bg_events: StdMutex::new(HashMap::new()),
            bg_event_rates: BgEventRates::new(),
            reap: ReapMetrics::new(),
            bind_acks: StdMutex::new(BindAckLatencies::default()),
            presetless_tool_calls: StdMutex::new(BTreeMap::new()),
            scoped_routes_with_tool_calls: AtomicU64::new(0),
        }
    }

    #[cfg(test)]
    pub(super) fn tool_received(&self, frame: &super::Frame, received_at: Instant) {
        let body = serde_json::from_slice::<Value>(&frame.body).ok();
        self.tool_received_value(frame, received_at, body.as_ref());
    }

    pub(super) fn tool_received_decoded(&self, decoded: &super::DecodedFrame) {
        self.tool_received_value(
            &decoded.frame,
            decoded.phase_trace.received_at(),
            decoded.body().ok(),
        );
    }

    fn tool_received_value(
        &self,
        frame: &super::Frame,
        received_at: Instant,
        body: Option<&Value>,
    ) {
        // bg_events is a held-open subscription, not a unary call awaiting a
        // response. Its StreamEnd is sent when the subscription is drained.
        if is_long_lived_subscription(body) {
            return;
        }
        let name = body
            .and_then(|body| {
                body.get("name")
                    .or_else(|| body.get("op"))
                    .and_then(Value::as_str)
                    .map(str::to_owned)
            })
            .unwrap_or_else(|| "undecoded".into());
        let client_deadline = if name == "bash" {
            body.and_then(client_deadline)
        } else {
            None
        };
        let client_deadline_supplied = name == "bash"
            && body
                .and_then(|body| body.get("deadline_ms"))
                .and_then(Value::as_u64)
                .is_some();
        let (deadline, deadline_class_ms) = if name == "bash" {
            let deadline = client_deadline.or_else(|| {
                if client_deadline_supplied {
                    None
                } else {
                    received_at.checked_add(UNANSWERED_TOOL_DEADLINE)
                }
            });
            let class_ms = deadline.map(|deadline| {
                deadline
                    .saturating_duration_since(received_at)
                    .as_millis()
                    .min(u64::MAX as u128) as u64
            });
            (deadline, class_ms)
        } else if name == "bash_watch" {
            // The dispatch layer replaces this with the role-specific watch
            // window once the route's worker limit and config are available.
            (None, None)
        } else {
            (
                received_at.checked_add(UNANSWERED_TOOL_DEADLINE),
                Some(UNANSWERED_TOOL_DEADLINE.as_millis() as u64),
            )
        };
        if let Ok(mut tools) = self.unanswered_tools.lock() {
            tools.insert(
                (
                    super::route_key(frame.header.channel, frame.header.epoch),
                    frame.header.corr,
                ),
                UnansweredTool {
                    received_at,
                    name,
                    deadline,
                    deadline_class_ms,
                    client_deadline_supplied,
                    warned: false,
                },
            );
        }
    }

    pub(super) fn set_bash_unanswered_deadline(
        &self,
        route: super::RouteChannel,
        corr: u64,
        deadline: Instant,
    ) {
        let Ok(mut tools) = self.unanswered_tools.lock() else {
            return;
        };
        let Some(tool) = tools.get_mut(&(route, corr)) else {
            return;
        };
        if tool.name != "bash" || tool.client_deadline_supplied {
            return;
        }
        tool.deadline = Some(deadline);
        tool.deadline_class_ms = Some(
            deadline
                .saturating_duration_since(tool.received_at)
                .as_millis()
                .min(u64::MAX as u128) as u64,
        );
    }

    /// Replace the generic 25-second watchdog window with bash_watch's own
    /// synchronous wait plus a short margin for polling and response delivery.
    pub(super) fn set_bash_watch_unanswered_wait(
        &self,
        route: super::RouteChannel,
        corr: u64,
        wait_ms: u64,
    ) {
        let Ok(mut tools) = self.unanswered_tools.lock() else {
            return;
        };
        let Some(tool) = tools.get_mut(&(route, corr)) else {
            return;
        };
        if tool.name != "bash_watch" {
            return;
        }
        let warning_window =
            Duration::from_millis(wait_ms).saturating_add(BASH_WATCH_UNANSWERED_GRACE);
        tool.deadline = tool.received_at.checked_add(warning_window);
        tool.deadline_class_ms = tool.deadline.map(|deadline| {
            deadline
                .saturating_duration_since(tool.received_at)
                .as_millis()
                .min(u64::MAX as u128) as u64
        });
    }

    /// Called by the independent watchdog thread, not by an executor job or
    /// the frame runtime. A call that never finishes must still leave evidence.
    pub(super) fn warn_unanswered_tools(&self) {
        self.warn_unanswered_tools_with(Instant::now(), &|line| log::warn!("{line}"));
    }

    fn warn_unanswered_tools_with(&self, now: Instant, log: &dyn Fn(&str)) {
        let Ok(mut tools) = self.unanswered_tools.try_lock() else {
            return;
        };
        for (&(route, corr), tool) in tools.iter_mut() {
            if !tool.warned && tool.deadline.is_some_and(|deadline| now >= deadline) {
                tool.warned = true;
                log(&format!(
                    "tool_call unanswered past client deadline class name={} channel={} epoch={} corr={} elapsed_ms={} deadline_class_ms={}",
                    tool.name,
                    route.channel,
                    route.epoch,
                    corr,
                    now.saturating_duration_since(tool.received_at).as_millis(),
                    tool.deadline_class_ms.unwrap_or_default()
                ));
            }
        }
    }

    pub(super) fn tool_replied(&self, frame: &super::Frame) {
        self.tool_replied_with(frame, Instant::now(), &|line| log::info!("{line}"));
    }

    fn tool_replied_with(&self, frame: &super::Frame, now: Instant, log: &dyn Fn(&str)) {
        if matches!(
            frame.header.ty,
            super::FrameType::Response | super::FrameType::Error
        ) {
            if let Ok(mut tools) = self.unanswered_tools.lock() {
                let key = (
                    super::route_key(frame.header.channel, frame.header.epoch),
                    frame.header.corr,
                );
                if let Some(tool) = tools.remove(&key) {
                    // Bash does not travel through the ordinary phase tracer.
                    // Record the actual socket handoff, not merely queueing a
                    // completion, so a later client timeout can be localized to
                    // AFT or to the daemon/consumer leg of the connection.
                    if tool.name == "bash" || tool.name == "powershell" {
                        log(&format!("tool_call reply written name={} channel={} epoch={} corr={} total_ms={} destination=subc_daemon", tool.name, key.0.channel, key.0.epoch, key.1, now.saturating_duration_since(tool.received_at).as_millis()));
                    }
                }
            }
        }
    }

    /// A tool call on an unscoped route named no catalog preset.
    pub(super) fn record_presetless_tool_call(&self, harness: &str) {
        if let Ok(mut counts) = self.presetless_tool_calls.lock() {
            *counts.entry(harness.to_string()).or_default() += 1;
        }
    }

    /// A route bound with a scope stamp made its first tool call.
    pub(super) fn record_scoped_route_with_tool_calls(&self) {
        self.scoped_routes_with_tool_calls
            .fetch_add(1, Ordering::Relaxed);
    }

    fn tool_call_preset_snapshot(&self) -> Value {
        let by_harness = self
            .presetless_tool_calls
            .lock()
            .map(|counts| counts.clone())
            .unwrap_or_default();
        json!({
            "tool_calls_without_preset": {
                "total": by_harness.values().sum::<u64>(),
                "by_harness": by_harness,
            },
            "scoped_routes_with_tool_calls": self.scoped_routes_with_tool_calls.load(Ordering::Relaxed),
        })
    }

    /// Record how long a RouteBind took from arrival to its answer: a
    /// configure ack or error, or the deadline refusal.
    pub(super) fn record_bind_ack(&self, latency: Duration) {
        if let Ok(mut acks) = self.bind_acks.lock() {
            acks.record(Instant::now(), latency);
        }
    }

    fn bind_ack_snapshot(&self) -> Value {
        self.bind_acks
            .lock()
            .map(|mut acks| acks.snapshot(Instant::now()))
            .unwrap_or(Value::Null)
    }

    fn now_ms(&self) -> u64 {
        duration_millis_u64(self.origin.elapsed())
    }

    pub(super) fn mark_frame_loop_tick(&self) {
        self.frame_loop_phase.store(0, Ordering::Relaxed);
        self.frame_loop_last_tick_ms
            .store(self.now_ms(), Ordering::Relaxed);
    }

    pub(super) fn mark_frame_loop_step(&self, entry: &'static str, command: &str, request_id: u64) {
        self.frame_loop_step
            .enter(entry, command, request_id, self.now_ms());
        self.frame_loop_phase.store(0, Ordering::Relaxed);
    }

    /// Records that the frame loop is about to park and will wake again within
    /// `within` because its drain-tick timer fires then.
    pub(super) fn publish_frame_loop_wake_deadline(&self, within: Duration) {
        self.frame_loop_step.clear();
        self.frame_loop_phase.store(1, Ordering::Relaxed);
        let deadline = self
            .now_ms()
            .saturating_add(duration_millis_u64(within))
            .saturating_add(1);
        self.frame_loop_wake_deadline_ms_plus_one
            .store(deadline, Ordering::Relaxed);
    }

    /// Called when the frame loop exits, so a finished loop never reads as a
    /// stalled one.
    pub(super) fn clear_frame_loop_wake_deadline(&self) {
        self.frame_loop_step.clear();
        self.frame_loop_wake_deadline_ms_plus_one
            .store(0, Ordering::Relaxed);
    }

    /// Progress means starting a new loop turn, before taking any loop-owned
    /// locks. Both received frames and idle drain-timer wakes start turns. A
    /// burst's total duration is irrelevant as long as turns keep advancing;
    /// publishing a future wake deadline alone is not progress.
    pub(super) fn frame_loop_progress_age(&self) -> Duration {
        let last = self.frame_loop_last_tick_ms.load(Ordering::Relaxed);
        Duration::from_millis(self.now_ms().saturating_sub(last))
    }

    /// The frame loop always parks on a drain-tick timer at most one tick
    /// away, so once that promised wake time has passed the loop owes a turn.
    /// Before the loop starts and after it exits there is no deadline, and
    /// nothing is owed.
    pub(super) fn frame_loop_has_pending_work(&self) -> bool {
        let deadline = self
            .frame_loop_wake_deadline_ms_plus_one
            .load(Ordering::Relaxed);
        deadline != 0 && self.now_ms() >= deadline
    }

    pub(super) fn mark_frame_loop_entry(&self, entry: &'static str, frame: &Frame) {
        let command = match frame.header.ty {
            super::FrameType::Ping => "ping",
            super::FrameType::Pong => "pong",
            _ => "",
        };
        self.frame_loop_step
            .enter(entry, command, frame.header.corr, self.now_ms());
    }

    pub(super) fn mark_frame_loop_command(&self, entry: &'static str, name: &str, request_id: u64) {
        self.frame_loop_step
            .enter(entry, name, request_id, self.now_ms());
    }

    pub(super) fn mark_frame_loop_tool(&self, name: &str, request_id: u64) {
        self.mark_frame_loop_command("handle_tool_call", name, request_id);
    }

    pub(super) fn mark_frame_loop_control(
        &self,
        request: &super::ModuleControlRequest,
        frame: &Frame,
    ) {
        let (name, awaited) = match request {
            super::ModuleControlRequest::RouteBind { .. } => ("route_bind", "control_validation"),
            super::ModuleControlRequest::HealthCheck {} => ("health_check", "cached_health"),
        };
        self.mark_frame_loop_command("handle_control_request", name, frame.header.corr);
        if let Ok(mut operation) = self.frame_operation.try_lock() {
            operation.op = name;
            operation.awaited = awaited;
            if let super::ModuleControlRequest::RouteBind { identity, .. } = request {
                operation.root = identity.project_root.to_string_lossy().into_owned();
                operation.session.clone_from(&identity.session);
            }
        }
    }

    pub(super) fn mark_frame_loop_frame(&self, frame: &Frame) {
        self.mark_frame_loop_entry("run_module_loop/handle_frame", frame);
        self.frame_loop_frame_header.store(
            (frame.header.ty as u64)
                | (u64::from(frame.header.channel) << 8)
                | (u64::from(frame.header.epoch) << 24),
            Ordering::Relaxed,
        );
        self.frame_loop_frame_corr
            .store(frame.header.corr, Ordering::Relaxed);
        if let Ok(mut operation) = self.frame_operation.try_lock() {
            *operation = FrameOperation {
                corr: frame.header.corr,
                op: match frame.header.ty {
                    super::FrameType::Request => "request",
                    super::FrameType::Response => "response",
                    super::FrameType::Ping => "ping",
                    super::FrameType::Pong => "pong",
                    super::FrameType::Error => "error",
                    super::FrameType::Cancel => "cancel",
                    super::FrameType::Goodbye => "goodbye",
                    super::FrameType::Push => "push",
                    _ => "frame",
                },
                awaited: "none",
                ..FrameOperation::default()
            };
        }
        self.frame_loop_phase.store(2, Ordering::Release);
    }

    pub(super) fn mark_frame_loop_wait(
        &self,
        awaited: &'static str,
        holder: Option<Arc<crate::lock_diagnostics::LockDiagnostics>>,
    ) {
        if let Ok(mut operation) = self.frame_operation.try_lock() {
            operation.awaited = awaited;
            operation.awaited_root.clear();
            operation.holder = holder;
        }
    }

    pub(super) fn mark_frame_loop_wait_for_root(
        &self,
        awaited: &'static str,
        root: &ProjectRootId,
        holder: Arc<crate::lock_diagnostics::LockDiagnostics>,
    ) {
        if let Ok(mut operation) = self.frame_operation.try_lock() {
            operation.awaited = awaited;
            operation.awaited_root = root.as_path().to_string_lossy().into_owned();
            operation.holder = Some(holder);
        }
    }

    pub(super) fn frame_loop_context(&self) -> String {
        let context = match self.frame_loop_phase.load(Ordering::Acquire) {
            0 => "phase=turn_work frame=none".to_string(),
            1 => "phase=select_wait frame=none".to_string(),
            _ => {
                let header = self.frame_loop_frame_header.load(Ordering::Relaxed);
                let corr = self.frame_loop_frame_corr.load(Ordering::Relaxed);
                let operation = self
                    .frame_operation
                    .try_lock()
                    .ok()
                    .filter(|op| op.corr == corr);
                let detail = operation.map_or_else(
                    || "op=unavailable awaited=diagnostic_snapshot_busy".to_string(),
                    |op| {
                        format!(
                            "op={} root={:?} session={:?} awaited={} {} awaited_root={:?}",
                            op.op,
                            op.root,
                            op.session,
                            op.awaited,
                            op.holder.as_ref().map_or_else(
                                || "holder=unknown".to_string(),
                                |holder| holder.snapshot()
                            ),
                            op.awaited_root,
                        )
                    },
                );
                format!(
                    "phase=handle_frame frame_type={} channel={} epoch={} corr={} {detail}",
                    header & 0xff,
                    (header >> 8) & 0xffff,
                    header >> 24,
                    corr,
                )
            }
        };
        format!("{context} {}", self.frame_loop_step.snapshot(self.now_ms()))
    }

    /// Returns whether the retained-roots summary changed since the last sweep.
    pub(super) fn record_reap(&self, census: ReapBlockerCensus) -> bool {
        let last_sweep_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(duration_millis_u64)
            .unwrap_or(0);
        self.reap.record(last_sweep_ms, census)
    }

    /// The last sweep's census as the health surface reports it, without the
    /// wall-clock sweep time.
    #[cfg(test)]
    pub(super) fn reap_census_for_test(&self) -> Value {
        let mut census = self.reap.snapshot();
        if let Some(fields) = census.as_object_mut() {
            fields.remove("last_sweep_ms");
        }
        census
    }

    pub(super) fn record_bg_runtime(
        &self,
        subscriptions: usize,
        wake_pending: usize,
        unacked_total: usize,
    ) {
        self.bg_subscriptions
            .store(subscriptions, Ordering::Relaxed);
        self.bg_wake_pending.store(wake_pending, Ordering::Relaxed);
        self.bg_wake_unacked_total
            .store(unacked_total, Ordering::Relaxed);
    }

    #[cfg(test)]
    pub(super) fn bg_runtime_for_test(&self) -> (usize, usize, usize) {
        (
            self.bg_subscriptions.load(Ordering::Relaxed),
            self.bg_wake_pending.load(Ordering::Relaxed),
            self.bg_wake_unacked_total.load(Ordering::Relaxed),
        )
    }

    pub(super) fn record_bg_wake_rearm(&self) {
        self.bg_wake_rearm_total.fetch_add(1, Ordering::Relaxed);
    }

    #[cfg(test)]
    pub(super) fn bg_wake_rearm_total(&self) -> u64 {
        self.bg_wake_rearm_total.load(Ordering::Relaxed)
    }

    pub(super) fn warn_stuck_pending_watches(
        self: &Arc<Self>,
        executor: &Arc<Executor>,
        subscriptions: &BgSubsBySession,
    ) {
        let now_ms = self.now_ms();
        let next_scan_ms = self
            .stuck_pending_watch_next_scan_ms
            .load(Ordering::Relaxed);
        if now_ms < next_scan_ms {
            return;
        }
        let next = now_ms.saturating_add(duration_millis_u64(STUCK_PENDING_WATCH_SCAN_INTERVAL));
        if self
            .stuck_pending_watch_next_scan_ms
            .compare_exchange(next_scan_ms, next, Ordering::Relaxed, Ordering::Relaxed)
            .is_err()
        {
            return;
        }

        self.prune_bg_event_records(subscriptions, Instant::now());
        if subscriptions.is_empty()
            || self
                .stuck_pending_watch_scan_running
                .swap(true, Ordering::Relaxed)
        {
            return;
        }
        let metrics = Arc::clone(self);
        let executor = Arc::clone(executor);
        let subscriptions = subscriptions.clone();
        // Watch rows live in the shared SQLite database. Even acquiring its
        // mutex can wait behind a disk-bound retention worker. One outstanding
        // scan suffices; never queue more scans behind a stalled database.
        tokio::task::spawn_blocking(move || {
            struct ScanFinished(Arc<DispatchPathMetrics>);
            impl Drop for ScanFinished {
                fn drop(&mut self) {
                    self.0
                        .stuck_pending_watch_scan_running
                        .store(false, Ordering::Relaxed);
                }
            }
            let _finished = ScanFinished(Arc::clone(&metrics));
            metrics.scan_stuck_pending_watches(&executor, &subscriptions);
        });
    }

    fn scan_stuck_pending_watches(&self, executor: &Executor, subscriptions: &BgSubsBySession) {
        let now = Instant::now();
        let Ok(mut logged) = self.stuck_pending_watch_logs.lock() else {
            return;
        };
        for ((root, session), channels) in subscriptions {
            if channels.is_empty() {
                continue;
            }
            let Some(ctx) = executor.actor_context(root) else {
                continue;
            };
            for (task_id, watch_id, age_ms) in ctx
                .bash_background()
                .stuck_pending_watches_for_session(session, STUCK_PENDING_WATCH_AGE)
            {
                let key = format!(
                    "{}\0{session}\0{task_id}\0{watch_id}",
                    root.as_path().display()
                );
                if logged.get(&key).is_some_and(|last| {
                    now.saturating_duration_since(*last) < STUCK_PENDING_WATCH_LOG_INTERVAL
                }) {
                    continue;
                }
                logged.insert(key, now);
                // One scan covers every session's watches, so log each inside
                // its own session's scope to tag it with that session.
                crate::log_ctx::with_session(Some(session.clone()), || {
                    crate::slog_warn!(
                        "subc bg wake: pending watch remains unacked root={} session={} task={} watch={} age_ms={}",
                        root.as_path().display(),
                        session,
                        task_id,
                        watch_id,
                        age_ms
                    );
                });
            }
        }
        logged.retain(|_, last| {
            now.saturating_duration_since(*last) < STUCK_PENDING_WATCH_LOG_INTERVAL
        });
    }

    fn prune_bg_event_records(&self, subscriptions: &BgSubsBySession, now: Instant) {
        if let Ok(mut events) = self.bg_events.lock() {
            events.retain(|key, record| {
                now.saturating_duration_since(record.window_start) < BG_OBSERVABILITY_INTERVAL
                    || subscriptions
                        .get(&(key.root.clone(), key.session.clone()))
                        .is_some_and(|channels| !channels.is_empty())
            });
        }
    }

    fn record_bg_event_at(
        &self,
        root: &ProjectRootId,
        session: &str,
        kind: BgEventKind,
        now: Instant,
    ) -> Option<u64> {
        self.bg_event_rates.record(kind, now);
        let Ok(mut events) = self.bg_events.lock() else {
            return None;
        };
        let key = BgEventKey {
            root: root.clone(),
            session: session.to_string(),
            kind,
        };
        if let Some(record) = events.get_mut(&key) {
            if now.saturating_duration_since(record.window_start) < BG_OBSERVABILITY_INTERVAL {
                record.event_count = record.event_count.saturating_add(1);
                record.suppressed = record.suppressed.saturating_add(1);
                return None;
            }
            let suppressed = record.suppressed;
            *record = BgEventRecord {
                window_start: now,
                event_count: 1,
                suppressed: 0,
            };
            return Some(suppressed);
        }
        events.insert(
            key,
            BgEventRecord {
                window_start: now,
                event_count: 1,
                suppressed: 0,
            },
        );
        Some(0)
    }

    fn bg_event_count_60s(&self, kind: BgEventKind) -> u64 {
        self.bg_event_rates.count_60s(kind)
    }

    pub(super) fn bg_arm_misses_60s_total(&self) -> u64 {
        self.bg_event_count_60s(BgEventKind::ArmMiss)
    }

    fn bg_nudges_enqueued_60s_total(&self) -> u64 {
        self.bg_event_count_60s(BgEventKind::NudgeEnqueued)
    }

    pub(super) fn record_bg_arm_hit(
        &self,
        root: &ProjectRootId,
        session: &str,
        channel: RouteChannel,
    ) {
        if let Some(suppressed) =
            self.record_bg_event_at(root, session, BgEventKind::ArmHit, Instant::now())
        {
            emit_bg_observability_info(format!(
                "subc bg wake: arm HIT root={} session={} channel={} suppressed={suppressed}",
                root.as_path().display(),
                session,
                channel
            ));
        }
    }

    pub(super) fn record_bg_arm_miss(
        &self,
        root: &ProjectRootId,
        session: &str,
        live_root_subscriptions: usize,
    ) {
        if let Some(suppressed) =
            self.record_bg_event_at(root, session, BgEventKind::ArmMiss, Instant::now())
        {
            emit_bg_observability_info(format!(
                "subc bg wake: arm MISS root={} session={} live_root_subscriptions={} suppressed={suppressed}",
                root.as_path().display(),
                session,
                live_root_subscriptions
            ));
        }
    }

    pub(super) fn record_bg_nudge_enqueued(
        &self,
        root: &ProjectRootId,
        session: &str,
        channel: RouteChannel,
    ) {
        if let Some(suppressed) =
            self.record_bg_event_at(root, session, BgEventKind::NudgeEnqueued, Instant::now())
        {
            emit_bg_observability_info(format!(
                "subc bg wake: nudge enqueued root={} session={} channel={} suppressed={suppressed}",
                root.as_path().display(),
                session,
                channel
            ));
        }
    }

    pub(super) fn record_bg_subscription_installed(
        &self,
        root: &ProjectRootId,
        session: &str,
        channel: RouteChannel,
    ) {
        if let Some(suppressed) = self.record_bg_event_at(
            root,
            session,
            BgEventKind::SubscriptionInstalled,
            Instant::now(),
        ) {
            emit_bg_observability_info(format!(
                "subc bg subscription: installed root={} session={} channel={} cause=subscribe suppressed={suppressed}",
                root.as_path().display(),
                session,
                channel
            ));
        }
    }

    pub(super) fn record_bg_subscription_ended(
        &self,
        root: &ProjectRootId,
        session: &str,
        channel: RouteChannel,
        cause: &str,
    ) {
        if let Some(suppressed) = self.record_bg_event_at(
            root,
            session,
            BgEventKind::SubscriptionEnded,
            Instant::now(),
        ) {
            emit_bg_observability_info(format!(
                "subc bg subscription: ended root={} session={} channel={} cause={cause} suppressed={suppressed}",
                root.as_path().display(),
                session,
                channel
            ));
        }
    }

    fn reap_snapshot(&self) -> Value {
        self.reap.snapshot()
    }

    pub(super) fn snapshot(&self, pending_binds: &HashMap<RouteChannel, PendingBind>) -> Value {
        let now = Instant::now();
        let oldest_pending_age_ms = pending_binds
            .values()
            .map(|bind| duration_millis_u64(now.saturating_duration_since(bind.started_at)))
            .max();
        let last_tick_age_ms = duration_millis_u64(self.frame_loop_progress_age());
        json!({
            "frame_loop": {
                "last_tick_age_ms": last_tick_age_ms,
                "wake_overdue": self.frame_loop_has_pending_work(),
            },
            "stall_watchdog": {
                "stall_count": self.stall_stats.stall_count(),
                "last_stall_duration_ms": self.stall_stats.last_stall_duration_ms(),
                "active_stalls": self.stall_stats.active_stalls(),
                "captures": self.stall_stats.captures(),
            },
            "pending_binds": {
                "count": pending_binds.len(),
                "oldest_age_ms": oldest_pending_age_ms,
            },
            "bind_acks": self.bind_ack_snapshot(),
            "reliable_push": self.watch_delivery.health(),
            "tool_call_presets": self.tool_call_preset_snapshot(),
            "completion_channels": {
                "control": self.control_completion_queued.load(Ordering::Relaxed),
                "maintenance": self.maintenance_queued.load(Ordering::Relaxed),
                "bash_deferred": self.bash_deferred_queued.load(Ordering::Relaxed),
                "bash_poll_touch": self.bash_poll_touch_queued.load(Ordering::Relaxed),
            },
            "deferred_bash_waits_in_flight": self.deferred_bash_waits_in_flight.load(Ordering::Relaxed),
            "off_lane_jobs": crate::executor::view_publication::health_snapshot(),
            "budget_deferrals": {
                "reliable_push": self.reliable_push_budget_deferrals.load(Ordering::Relaxed),
                "maintenance": self.maintenance_budget_deferrals.load(Ordering::Relaxed),
            },
            "writer": {
                "queued": self.writer_queued.load(Ordering::Relaxed),
                "active": self.writer_active.load(Ordering::Relaxed),
                "capacity": WRITER_QUEUE_CAPACITY,
                "saturation_count": self.writer_saturation_count.load(Ordering::Relaxed),
                "root_broadcast_over_1mib_total": self.root_broadcast_over_1mib_total.load(Ordering::Relaxed),
                "status_push_compacted_total": self.status_push_compacted_total.load(Ordering::Relaxed),
                "status_push_aggregate_limited_total": self.status_push_aggregate_limited_total.load(Ordering::Relaxed),
            },
            "response_tasks": {
                "live": self.response_tasks_live.load(Ordering::Relaxed),
            },
        })
    }

    pub(super) fn record_root_broadcast(
        &self,
        root: &ProjectRootId,
        kind: &'static str,
        body_bytes: usize,
        routes: usize,
    ) {
        self.record_root_broadcast_with(root, kind, body_bytes, routes, Instant::now(), &|line| {
            log::warn!("{line}");
        });
    }

    fn record_root_broadcast_with(
        &self,
        root: &ProjectRootId,
        kind: &'static str,
        body_bytes: usize,
        routes: usize,
        now: Instant,
        log: &dyn Fn(&str),
    ) {
        self.record_broadcast_detail(root, kind, body_bytes, routes, now, log, None);
    }

    pub(super) fn record_status_push(
        &self,
        root: &ProjectRootId,
        body: &crate::status_push::StatusBody,
        recipients: usize,
    ) {
        self.record_status_push_with(root, body, recipients, Instant::now(), &|line| {
            log::warn!("{line}")
        });
    }

    fn record_status_push_with(
        &self,
        root: &ProjectRootId,
        body: &crate::status_push::StatusBody,
        recipients: usize,
        now: Instant,
        log: &dyn Fn(&str),
    ) {
        if body.compacted {
            self.status_push_compacted_total
                .fetch_add(1, Ordering::Relaxed);
        }
        self.record_broadcast_detail(
            root,
            "status_changed",
            body.original_bytes,
            recipients,
            now,
            log,
            Some(body),
        );
    }

    #[allow(clippy::too_many_arguments)]
    fn record_broadcast_detail(
        &self,
        root: &ProjectRootId,
        kind: &'static str,
        body_bytes: usize,
        routes: usize,
        now: Instant,
        log: &dyn Fn(&str),
        status: Option<&crate::status_push::StatusBody>,
    ) {
        let oversized = body_bytes.saturating_mul(routes) > 1024 * 1024;
        let compacted = status.is_some_and(|body| body.compacted);
        if !oversized && !compacted {
            return;
        }
        if oversized {
            self.root_broadcast_over_1mib_total
                .fetch_add(1, Ordering::Relaxed);
        }
        // Count every oversized fan-out, but warn at most once per root/kind
        // per minute. Warning entries at least one minute old are removed.
        let mut warned = self
            .root_broadcast_warned
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        warned.retain(|_, last| now.saturating_duration_since(*last) < Duration::from_secs(60));
        if let std::collections::hash_map::Entry::Vacant(entry) = warned.entry((root.clone(), kind))
        {
            entry.insert(now);
            let fields = status
                .map(|body| serde_json::to_string(&body.field_bytes).expect("status field sizes"))
                .unwrap_or_else(|| "{}".into());
            let bounded_bytes = status.map_or(body_bytes, |body| body.bytes.len());
            log(&format!("subc root broadcast pressure: root={} kind={kind} body_bytes={body_bytes} route_count={routes} recipients={routes} bounded_body_bytes={bounded_bytes} details_omitted={compacted} top_level_field_bytes={fields}", root.as_path().display()));
        }
    }
}

struct UnansweredTool {
    received_at: Instant,
    name: String,
    deadline: Option<Instant>,
    deadline_class_ms: Option<u64>,
    client_deadline_supplied: bool,
    warned: bool,
}

fn is_long_lived_subscription(body: Option<&Value>) -> bool {
    body.and_then(|body| body.get("op"))
        .and_then(Value::as_str)
        .is_some_and(|operation| operation == "bg_events")
}

fn client_deadline(body: &Value) -> Option<Instant> {
    let deadline_ms = body.get("deadline_ms")?.as_u64()?;
    // Pair the clocks here: the reader's receipt instant can predate this
    // wall-clock sample by time spent queued for dispatch.
    let now = Instant::now();
    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(u64::MAX as u128) as u64;
    now.checked_add(Duration::from_millis(deadline_ms.saturating_sub(now_ms)))
}

pub(super) struct DeferredBashWaitGuard {
    metrics: Arc<DispatchPathMetrics>,
}

impl DeferredBashWaitGuard {
    pub(super) fn new(metrics: &Arc<DispatchPathMetrics>) -> Self {
        metrics
            .deferred_bash_waits_in_flight
            .fetch_add(1, Ordering::Relaxed);
        Self {
            metrics: Arc::clone(metrics),
        }
    }
}

impl Drop for DeferredBashWaitGuard {
    fn drop(&mut self) {
        self.metrics
            .deferred_bash_waits_in_flight
            .fetch_sub(1, Ordering::Relaxed);
    }
}

pub(super) struct ResponseTaskGuard {
    metrics: Arc<DispatchPathMetrics>,
}

impl ResponseTaskGuard {
    pub(super) fn new(metrics: &Arc<DispatchPathMetrics>) -> Self {
        metrics.response_tasks_live.fetch_add(1, Ordering::Relaxed);
        Self {
            metrics: Arc::clone(metrics),
        }
    }
}

impl Drop for ResponseTaskGuard {
    fn drop(&mut self) {
        self.metrics
            .response_tasks_live
            .fetch_sub(1, Ordering::Relaxed);
    }
}

fn duration_millis_u64(duration: Duration) -> u64 {
    duration.as_millis().min(u128::from(u64::MAX)) as u64
}

pub(super) fn warn_slow_running_interactive_jobs(executor: &Executor) {
    warn_slow_running_interactive_jobs_after(executor, INTERACTIVE_OCCUPANCY_WARN_AFTER);
}

fn warn_slow_running_interactive_jobs_after(executor: &Executor, minimum_age: Duration) {
    let Some(jobs) = executor.try_take_long_running_interactive_jobs(minimum_age) else {
        return;
    };
    for job in jobs {
        let state = if job.execution_started {
            "executing"
        } else {
            "zombie_reader"
        };
        let line = format!(
            "executor occupancy census: class=Interactive job={} command={} lane={:?} state={} age_ms={} root={}",
            job.request_id,
            job.command,
            job.lane,
            state,
            job.age.as_millis(),
            job.root_id,
        );
        log::warn!("{line}");
        #[cfg(test)]
        INTERACTIVE_OCCUPANCY_TEST_LOGS.with(|logs| logs.borrow_mut().push(line));
    }
}

#[cfg(test)]
thread_local! {
    static INTERACTIVE_OCCUPANCY_TEST_LOGS: std::cell::RefCell<Vec<String>> = const { std::cell::RefCell::new(Vec::new()) };
}

#[cfg(test)]
fn take_interactive_occupancy_logs_for_test() -> Vec<String> {
    INTERACTIVE_OCCUPANCY_TEST_LOGS.with(|logs| std::mem::take(&mut *logs.borrow_mut()))
}

pub(super) fn warn_slow_pending_binds(
    pending_binds: &mut HashMap<RouteChannel, PendingBind>,
    executor: &Executor,
) {
    let now = Instant::now();
    for (route, pending) in pending_binds.iter_mut() {
        if pending.warned_half_deadline {
            continue;
        }
        let age = now.saturating_duration_since(pending.started_at);
        if age < DISPATCH_PATH_BIND_WARN_AFTER {
            continue;
        }
        pending.warned_half_deadline = true;
        let snapshot = executor
            .try_bind_blocker_snapshot(&pending.bind_root_id, &pending.configure_request_id)
            .unwrap_or_else(|| BindBlockerSnapshot {
                configure_state: "scheduler_busy",
                configure_phase_timings: None,
                blockers: vec!["scheduler_busy".to_string()],
                oldest_queued_writer_age_ms: None,
                in_flight_readers: Vec::new(),
                in_flight_writer: None,
                reader_admissions_while_promoted_writer_waited: 0,
            });
        let breadcrumb = pending_bind_breadcrumb(
            *route,
            &pending.bind_root_id,
            age,
            &pending.configure_request_id,
            &snapshot,
        );
        // The frame loop serves every session, so tag the line with the
        // session that requested this bind rather than whatever scope (if
        // any) the thread happens to be in.
        crate::log_ctx::with_session(pending.session.clone(), || {
            crate::slog_warn!("{breadcrumb}");
        });
    }
}

pub(super) fn pending_bind_breadcrumb(
    route: RouteChannel,
    root_id: &crate::path_identity::ProjectRootId,
    age: Duration,
    configure_request_id: &str,
    snapshot: &BindBlockerSnapshot,
) -> String {
    let blockers = if snapshot.blockers.is_empty() {
        "none (scheduler admission only; executing configure may wait on I/O or locks)".to_string()
    } else {
        snapshot.blockers.join(", ")
    };
    let phase_timings = snapshot
        .configure_phase_timings
        .as_deref()
        .unwrap_or("unavailable");
    let readers = snapshot
        .in_flight_readers
        .iter()
        .map(|reader| {
            format!(
                "job={} command={} lane={:?} age_ms={} execution_started={} started_before_oldest_writer={}",
                reader.request_id,
                reader.command,
                reader.lane,
                reader.started_age_ms,
                reader.execution_started,
                reader.started_before_oldest_writer
            )
        })
        .collect::<Vec<_>>()
        .join("; ");
    let writer = snapshot
        .in_flight_writer
        .as_ref()
        .map(|writer| {
            format!(
                "job={} command={} age_ms={}",
                writer.request_id, writer.command, writer.started_age_ms,
            )
        })
        .unwrap_or_default();
    format!(
        "subc attach: pending RouteBind route {route} for root {} crossed {}ms (configure_request_id={}, configure_state={}, configure_phase_timings=[{}], admission_blockers=[{}], oldest_queued_writer_age_ms={:?}, in_flight_readers=[{}], in_flight_writer=[{}], reader_admissions_while_promoted_writer_waited={})",
        root_id.as_path().display(),
        duration_millis_u64(age),
        configure_request_id,
        snapshot.configure_state,
        phase_timings,
        blockers,
        snapshot.oldest_queued_writer_age_ms,
        readers,
        writer,
        snapshot.reader_admissions_while_promoted_writer_waited,
    )
}

/// Refusal code for a route bind whose deadline passed while it waited
/// behind a reader on its root. Retryable like `actor_not_ready`: the reader
/// ends eventually, and a later bind can then be admitted.
pub(super) const BIND_BLOCKED_BY_READER: &str = "bind_blocked_by_reader";
pub(super) const BIND_BLOCKED_BY_WRITER: &str = "bind_blocked_by_writer";

/// The code and message an overdue route bind is refused with. A bind still
/// queued while readers run on its root waited for them (a bind that needs
/// exclusive use of the root cannot start beside readers), so the refusal
/// names the oldest of them: its job, its tool and how long it has run.
/// A running writer can hold the same slot even when there are no readers;
/// name it rather than suggesting that configure itself is necessarily stuck.
pub(super) fn route_bind_deadline_refusal(
    age: Duration,
    deadline: Duration,
    relay: Duration,
    snapshot: &BindBlockerSnapshot,
) -> (&'static str, String) {
    let age_ms = duration_millis_u64(age);
    let deadline_ms = duration_millis_u64(deadline);
    if let Some(writer) = snapshot.in_flight_writer.as_ref().filter(|_| {
        matches!(
            snapshot.configure_state,
            "queued" | "running" | "blocked_by_other_mutating"
        )
    }) {
        return (BIND_BLOCKED_BY_WRITER, format!(
            "route bind deadline exceeded after {age_ms}ms (deadline {deadline_ms}ms): a mutating job held this root (job={} tool={} age_ms={}); retry once it finishes or is cancelled",
            writer.request_id, writer.command, writer.started_age_ms,
        ));
    }
    let blocking_reader = (snapshot.configure_state == "queued")
        .then(|| {
            snapshot
                .in_flight_readers
                .iter()
                .max_by_key(|reader| reader.started_age_ms)
        })
        .flatten();
    match blocking_reader {
        Some(reader) => {
            let others = snapshot.in_flight_readers.len() - 1;
            let others = if others == 0 {
                String::new()
            } else {
                format!(" and {others} other reader(s)")
            };
            (
                BIND_BLOCKED_BY_READER,
                format!(
                    "route bind deadline exceeded after {age_ms}ms (deadline {deadline_ms}ms): the bind waited behind a reader on this root that has not finished (job={} tool={} age_ms={}){others}; retry once it finishes or is cancelled",
                    reader.request_id, reader.command, reader.started_age_ms,
                ),
            )
        }
        None => (
            "actor_not_ready",
            format!(
                "route bind deadline exceeded after {age_ms}ms (deadline {deadline_ms}ms): the root's configure did not finish, so AFT refuses the bind before the daemon's {}ms bind relay times out",
                relay.as_millis()
            ),
        ),
    }
}

const HEALTH_ROOT_DETAIL_CAP: usize = crate::memory::MEMORY_SNAPSHOT_ROOT_DETAIL_CAP;
// SUBC caches at most 16 KiB; reserve 4 KiB for its envelope and future counters.
const HEALTH_METRICS_BUDGET_BYTES: usize = 12 * 1024;
pub(super) const HEALTH_ROLLUP_TTL: Duration = Duration::from_secs(3);
const HEALTH_RETENTION_COUNT_MAX_AGE_MS: u64 = 30_000;
const HEALTH_ALLOCATOR_MAX_AGE_MS: u64 = 30_000;
/// How long the health rollup carries the full memory census forward while
/// no `memory.census` read asked for a new one. Measuring it renders an
/// uncapped row for every actor root, and only `memory.census` reads it; the
/// compact memory figures in the health reply are measured on every rollup.
/// A read asks the next rollup for a fresh census, and a read that finds the
/// census older than one rollup interval (`HEALTH_ROLLUP_TTL`) wakes the
/// rollup and waits for a census measured after it (see
/// [`fresh_memory_census`]). Only if none is published before the request's
/// deadline does the read get the older census, with its age.
const HEALTH_MEMORY_CENSUS_INTERVAL_MS: u64 = 30_000;
/// How long a `memory.census` request may take when its body names no
/// `deadline_ms`: the default call timeout of the subc client the census
/// clients use.
const MEMORY_CENSUS_DEFAULT_BUDGET: Duration = Duration::from_secs(30);
/// Time kept back from a `memory.census` request's budget for adding the
/// route fields, encoding and writing the reply after waiting for a census.
const MEMORY_CENSUS_REPLY_MARGIN: Duration = Duration::from_secs(1);

fn encoded_health_metrics_len(metrics: &serde_json::Map<String, Value>) -> usize {
    #[cfg(test)]
    HEALTH_SIZE_WORK.with(|work| work.borrow_mut().0 += 1);
    struct ByteCounter(usize);
    impl std::io::Write for ByteCounter {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0 = self
                .0
                .checked_add(bytes.len())
                .ok_or_else(|| std::io::Error::other("encoded health metrics size overflow"))?;
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut counter = ByteCounter(0);
    match serde_json::to_writer(&mut counter, metrics) {
        Ok(()) => counter.0,
        Err(_) => usize::MAX,
    }
}

#[cfg(test)]
thread_local! {
    static HEALTH_SIZE_WORK: std::cell::RefCell<(usize, usize)> = const { std::cell::RefCell::new((0, 0)) };
}

fn refresh_metrics_bytes(metrics: &mut serde_json::Map<String, Value>) -> usize {
    metrics.insert("metrics_bytes".to_string(), json!(0));
    let encoded_zero = encoded_health_metrics_len(metrics);
    if encoded_zero == usize::MAX {
        return usize::MAX;
    }
    // Only the decimal width of metrics_bytes changes after measuring zero.
    // Solve the same fixed point without walking and copying the map again.
    let mut encoded_bytes = encoded_zero;
    for _ in 0..4 {
        let next = encoded_zero - 1 + encoded_bytes.ilog10() as usize + 1;
        if next == encoded_bytes {
            break;
        }
        encoded_bytes = next;
    }
    metrics.insert("metrics_bytes".to_string(), json!(encoded_bytes));
    encoded_bytes
}

fn compact_write_ledger_root_ids(metrics: &mut serde_json::Map<String, Value>) {
    let Some(rows) = metrics
        .get_mut("write_ledger_top_10m")
        .and_then(Value::as_array_mut)
    else {
        return;
    };
    for row in rows {
        let Some(root_id) = row.get_mut("root_id") else {
            continue;
        };
        let Some(raw) = root_id.as_str() else {
            continue;
        };
        if raw.starts_with('<') && raw.ends_with('>') {
            continue;
        }
        let basename = raw
            .rsplit(['/', '\\'])
            .find(|segment| !segment.is_empty())
            .unwrap_or(raw);
        *root_id = json!(if basename == raw { "<key>" } else { basename });
    }
}

fn retention_report_totals(
    reports: &std::collections::BTreeMap<String, crate::storage_retention::SweepReport>,
) -> Value {
    let mut totals = serde_json::Map::from_iter([("reports".into(), json!(reports.len()))]);
    let mut trash = crate::storage_retention::TrashReport {
        at_ms: crate::pins::now_ms(),
        ..Default::default()
    };
    for report in reports.values() {
        let census = &report.trash;
        trash.entries = trash.entries.saturating_add(census.entries);
        trash.bytes_estimate = trash.bytes_estimate.saturating_add(census.bytes_estimate);
        trash.removed_entries = trash.removed_entries.saturating_add(census.removed_entries);
        trash.incomplete |= census.incomplete || census.at_ms == 0;
        if let Some(age) = census.oldest_age_ms {
            let age = age.saturating_add(trash.at_ms.saturating_sub(census.at_ms));
            trash.oldest_age_ms = Some(trash.oldest_age_ms.unwrap_or(0).max(age));
        }
        for (reason, count) in &census.kept_by_reason {
            let total = trash.kept_by_reason.entry(reason.clone()).or_default();
            *total = total.saturating_add(*count);
        }
        for (field, value) in serde_json::to_value(report).unwrap().as_object().unwrap() {
            let (key, count) = match value {
                Value::Number(number) if field != "at_ms" => {
                    (field.clone(), number.as_u64().unwrap_or(0))
                }
                Value::Bool(value) => (format!("{field}_reports"), u64::from(*value)),
                Value::Array(errors) if field == "errors" => (field.clone(), errors.len() as u64),
                _ => continue,
            };
            let total = totals.get(&key).and_then(Value::as_u64).unwrap_or(0);
            totals.insert(key, json!(total.saturating_add(count)));
        }
    }
    // Detail reports can be omitted by the health payload budget; keep the
    // trash backlog and counts of entries retained by reason in the aggregate
    // that survives trimming.
    totals.insert("trash".into(), serde_json::to_value(trash).unwrap());
    Value::Object(totals)
}

fn budget_health_metrics(metrics: &mut serde_json::Map<String, Value>) {
    let mut ledger_compacted = false;
    loop {
        if refresh_metrics_bytes(metrics) <= HEALTH_METRICS_BUDGET_BYTES {
            return;
        }
        // Historical sweep detail must yield before current root diagnostics,
        // including standing health. The snapshot is newest first.
        if metrics
            .get_mut("v2_family_sweeps")
            .and_then(Value::as_array_mut)
            .is_some_and(|sweeps| sweeps.pop().is_some())
        {
            let omitted = metrics
                .get("v2_family_sweeps_omitted")
                .and_then(Value::as_u64)
                .unwrap_or(0)
                .saturating_add(1);
            metrics.insert("v2_family_sweeps_omitted".into(), json!(omitted));
            continue;
        }
        if metrics
            .get_mut("roots")
            .and_then(Value::as_array_mut)
            .is_some_and(|roots| roots.pop().is_some())
        {
            let omitted = metrics
                .get("root_details_omitted")
                .and_then(Value::as_u64)
                .unwrap_or(0)
                .saturating_add(1);
            metrics.insert("root_details_omitted".to_string(), json!(omitted));
            continue;
        }
        if !ledger_compacted {
            compact_write_ledger_root_ids(metrics);
            ledger_compacted = true;
            continue;
        }
        let retention_key = metrics
            .get("storage_retention")
            .and_then(Value::as_object)
            .and_then(|reports| reports.keys().next_back().cloned());
        if let Some(key) = retention_key {
            metrics["storage_retention"]
                .as_object_mut()
                .unwrap()
                .remove(&key);
            let omitted = metrics
                .get("storage_retention_reports_omitted")
                .and_then(Value::as_u64)
                .unwrap_or(0)
                .saturating_add(1);
            metrics.insert("storage_retention_reports_omitted".into(), json!(omitted));
            continue;
        }
        // Cache-root paths are user-chosen and may be long; drop them last,
        // unavailable entries first because their paths repeat ones listed in
        // cache_roots, counting each one dropped.
        let mut trimmed_cache_path = false;
        for (field, omitted_field) in [
            ("cache_unavailable", "cache_unavailable_omitted"),
            ("cache_roots", "cache_roots_omitted"),
        ] {
            let removed = match metrics.get_mut(field) {
                Some(Value::Object(entries)) => {
                    let last = entries.keys().next_back().cloned();
                    last.is_some_and(|key| entries.remove(&key).is_some())
                }
                Some(Value::Array(entries)) => entries.pop().is_some(),
                _ => false,
            };
            if removed {
                let omitted = metrics
                    .get(omitted_field)
                    .and_then(Value::as_u64)
                    .unwrap_or(0)
                    .saturating_add(1);
                metrics.insert(omitted_field.into(), json!(omitted));
                trimmed_cache_path = true;
                break;
            }
        }
        if trimmed_cache_path {
            continue;
        }
        return;
    }
}

#[derive(Clone)]
struct HealthDiagnosticRollup {
    status: HealthStatus,
    detail: Option<String>,
    generated_at_ms: Option<u64>,
    /// Publication number of this census; 0 for the placeholder served before
    /// the first one.
    generation: u64,
    stale_reason: Option<&'static str>,
    metrics: Value,
    /// The full memory census `memory.census` serves. It is measured at its
    /// own cadence (see [`HEALTH_MEMORY_CENSUS_INTERVAL_MS`]), so a rollup may
    /// carry forward the census an earlier rollup measured; sharing it by
    /// `Arc` keeps that carry free.
    memory_census: Arc<Value>,
    /// When `memory_census` was measured, on the cache clock. `None` until
    /// the rollup that measured it is published, which stamps it with the
    /// publication time.
    memory_census_at_ms: Option<u64>,
    /// The census requests (see `HealthRollupCache::census_requests`) that
    /// `memory_census` answers: every request numbered up to this one was
    /// made before the rollup that measured it started.
    memory_census_answers: u64,
    /// Per-step timing of the rollup that produced this snapshot.
    #[cfg(test)]
    tick: Value,
}

impl HealthDiagnosticRollup {
    fn unavailable() -> Self {
        let metrics = json!({
            "actor_count": Value::Null,
            "root_count": Value::Null,
            "warming_roots": Value::Null,
            "root_details_omitted": Value::Null,
            "callgraph_repair_entries_60s_total": Value::Null,
            "callgraph_repair_roots_annotated": Value::Null,
            "callgraph_repair_roots_total": Value::Null,
            "callgraph_commits_60s_total": Value::Null,
            "callgraph_pages_or_bytes_written_60s_total": Value::Null,
            "lsp_children": { "spawned": Value::Null, "cwd_gone": Value::Null },
            "lsp": Value::Null,
            "threads": Value::Null,
            "sqlite": Value::Null,
            "children": Value::Null,
            "fds": Value::Null,
            "embedding_backend": {
                "available": Value::Null,
                "last_error": Value::Null,
                "since_ms": Value::Null,
            },
            "memory": memory_rollup_metrics(None),
            "bash_task_retention": {
                "eligible_but_unpruned_rows": Value::Null,
                "eligible_but_unpruned_rows_measured_age_ms": Value::Null,
                "last_sweep_skip_reason": Value::Null,
                "sweeps_skipped_total": crate::db::compression_events::retention_sweep_skip_counts().0,
                "consecutive_skips": crate::db::compression_events::retention_sweep_skip_counts().1,
                "steady_state_ceiling": crate::db::compression_events::BASH_TASK_STEADY_STATE_ROWS,
                "disk_sweeps": serde_json::to_value(crate::bash_background::persisted_gc_health())
                    .unwrap_or(Value::Null),
            },
            "storage_retention": Value::Null,
            "storage_retention_totals": Value::Null,
            "storage_retention_reports_omitted": Value::Null,
            "bash_db_schema_hints": Value::Null,
            "bash_task_refusals": Value::Null,
            "mutating_lanes": { "scheduler_busy": true },
            "process_io": crate::process_io::ProcessIoSnapshot::capture().to_value(),
            "roots": Value::Null,
        });
        Self {
            status: HealthStatus::Degraded,
            detail: Some("health diagnostic snapshot is being refreshed".to_string()),
            generated_at_ms: None,
            generation: 0,
            stale_reason: Some("not_yet_sampled"),
            metrics,
            memory_census: Arc::new(placeholder_memory_census()),
            memory_census_at_ms: None,
            memory_census_answers: 0,
            #[cfg(test)]
            tick: Value::Null,
        }
    }
}

/// The census served before a full snapshot exists, or when the executor could
/// not be read without contention. Its unmeasured roots are null, but it
/// still carries `process_io` at both census positions: that sample reads only
/// process-wide kernel counters, so it never depends on the actor snapshot.
/// Consumers can rely on the field; `available: false` marks a host or moment
/// where the counters could not be read.
fn placeholder_memory_census() -> Value {
    let process_io = crate::process_io::ProcessIoSnapshot::capture().to_value();
    json!({
        "roots": Value::Null,
        "process": { "process_io": process_io.clone() },
        "process_io": process_io,
    })
}

#[derive(Clone)]
struct PlaneTiming {
    status: String,
    since_ms: u64,
    last_progress_at_ms: u64,
    progress_signature: String,
}

struct RetentionCountSample {
    database: std::sync::Weak<std::sync::Mutex<crate::db::TrackedConnection>>,
    measured_at_ms: u64,
    rows: usize,
}

pub(super) struct HealthRollupCache {
    origin: Instant,
    #[cfg(test)]
    clock_ms: AtomicU64,
    /// Count of completed refresh attempts, including scheduler contention.
    refreshes: AtomicU64,
    step_log_at_ms: AtomicU64,
    /// Generation of the newest published census.
    published_generation: AtomicU64,
    /// One more than the highest census generation that a contended refresh
    /// found still being served, or 0 if no refresh has been contended. The
    /// offset lets a contention before the first census mark the placeholder
    /// (generation 0). It only grows, so a reader holding an older census
    /// cannot see it reported fresh after a later recovery.
    stale_through_generation: AtomicU64,
    #[cfg(test)]
    root_details_assembled: AtomicUsize,
    snapshot: std::sync::RwLock<Arc<HealthDiagnosticRollup>>,
    breakers:
        std::sync::Mutex<HashMap<std::path::PathBuf, Arc<crate::build_breaker::BuildDeathBreaker>>>,
    plane_timings: std::sync::Mutex<HashMap<(String, &'static str), PlaneTiming>>,
    allocator: std::sync::Mutex<Option<(u64, crate::memory::AllocatorObservation)>>,
    retention_count: std::sync::Mutex<Option<RetentionCountSample>>,
    #[cfg(test)]
    retention_count_reads: AtomicU64,
    census_rows: std::sync::Mutex<crate::commands::memory_census::CensusRootRows>,
    /// How many `memory.census` reads have asked for a fresh census. A
    /// rollup that measures the census answers every request counted when it
    /// started (`HealthDiagnosticRollup::memory_census_answers`); while a
    /// request is unanswered, the next rollup measures instead of carrying.
    census_requests: AtomicU64,
    /// `memory.census` reads waiting for a rollup to answer them. While any
    /// wait, a bind-completion wake ends the rollup thread's quiet period too.
    census_waiters: AtomicUsize,
    /// The running rollup thread's wake channel, so a census read can start a
    /// rollup at once. Cleared when the worker shuts down.
    census_wake: std::sync::Mutex<Option<std::sync::mpsc::SyncSender<RollupWake>>>,
    /// The generation of each publication, for census reads waiting on one.
    publications: tokio::sync::watch::Sender<u64>,
    /// Full memory censuses measured by rollups.
    #[cfg(test)]
    census_builds: AtomicU64,
}

impl HealthRollupCache {
    pub(super) fn new() -> Self {
        Self {
            origin: Instant::now(),
            #[cfg(test)]
            clock_ms: AtomicU64::new(u64::MAX),
            refreshes: AtomicU64::new(0),
            step_log_at_ms: AtomicU64::new(u64::MAX),
            published_generation: AtomicU64::new(0),
            stale_through_generation: AtomicU64::new(0),
            #[cfg(test)]
            root_details_assembled: AtomicUsize::new(0),
            snapshot: std::sync::RwLock::new(Arc::new(HealthDiagnosticRollup::unavailable())),
            breakers: std::sync::Mutex::new(HashMap::new()),
            plane_timings: std::sync::Mutex::new(HashMap::new()),
            allocator: std::sync::Mutex::new(None),
            retention_count: std::sync::Mutex::new(None),
            #[cfg(test)]
            retention_count_reads: AtomicU64::new(0),
            census_rows: std::sync::Mutex::new(Default::default()),
            census_requests: AtomicU64::new(0),
            census_waiters: AtomicUsize::new(0),
            census_wake: std::sync::Mutex::new(None),
            publications: tokio::sync::watch::Sender::new(0),
            #[cfg(test)]
            census_builds: AtomicU64::new(0),
        }
    }

    fn allocator_observation(&self) -> crate::memory::AllocatorObservation {
        let mut cached = self
            .allocator
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some((cached_at_ms, observation)) = cached.as_ref() {
            let elapsed_ms = self.now_ms().saturating_sub(*cached_at_ms);
            let age_ms = observation.age_ms.unwrap_or(0).saturating_add(elapsed_ms);
            if age_ms <= HEALTH_ALLOCATOR_MAX_AGE_MS {
                let mut reused = observation.clone();
                // Unsupported platforms keep a null age, not a measured zero.
                reused.age_ms = observation.age_ms.map(|_| age_ms);
                return reused;
            }
        }
        let observation = crate::memory::AllocatorObservation::measure();
        // The observation already includes time spent walking the allocator.
        // Start the reuse clock after that walk so its duration is not counted twice.
        *cached = Some((self.now_ms(), observation.clone()));
        observation
    }

    fn step_log_due(&self) -> bool {
        let now = self.now_ms();
        let last = self.step_log_at_ms.load(Ordering::Relaxed);
        (last == u64::MAX || now.saturating_sub(last) >= 60_000)
            && self
                .step_log_at_ms
                .compare_exchange(last, now, Ordering::Relaxed, Ordering::Relaxed)
                .is_ok()
    }

    fn retention_count(
        &self,
        database: Option<&Arc<std::sync::Mutex<crate::db::TrackedConnection>>>,
        now_ms: i64,
    ) -> (Option<usize>, Option<u64>) {
        let mut cached = self
            .retention_count
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let Some(database) = database else {
            *cached = None;
            return (None, None);
        };
        if let Some(sample) = cached.as_ref() {
            let age_ms = self.now_ms().saturating_sub(sample.measured_at_ms);
            if age_ms < HEALTH_RETENTION_COUNT_MAX_AGE_MS
                && sample
                    .database
                    .upgrade()
                    .is_some_and(|old| Arc::ptr_eq(&old, database))
            {
                return (Some(sample.rows), Some(age_ms));
            }
        }
        // COUNT visits every eligible row even with the retention index. A large
        // history must not be recounted every three seconds. Time-based expiry
        // also picks up rows that become eligible without a database write.
        let measured_at_ms = self.now_ms();
        #[cfg(test)]
        self.retention_count_reads.fetch_add(1, Ordering::Relaxed);
        let rows = crate::db::compression_events::terminal_rows_eligible_count_nonblocking(
            database, now_ms,
        )
        .ok()
        .flatten();
        *cached = rows.map(|rows| RetentionCountSample {
            database: Arc::downgrade(database),
            measured_at_ms,
            rows,
        });
        // Failed or contended reads remain unknown and are retried next tick;
        // never give an old count a fresh timestamp after an unsuccessful read.
        (
            rows,
            rows.map(|_| self.now_ms().saturating_sub(measured_at_ms)),
        )
    }

    /// Assemble outside the cache lock. Probe readers never wait for a refresh.
    pub(super) fn refresh(&self, executor: &Executor, shared_app: &App) {
        #[cfg(test)]
        self.root_details_assembled.store(0, Ordering::Relaxed);
        self.publish(build_health_diagnostic_rollup(self, executor, shared_app));
    }

    fn refresh_for_test(&self, executor: &Executor, shared_app: &App) {
        // The owned test worker may wait for the scheduler snapshot. Production
        // probes and periodic rollups retain their non-blocking census policy.
        self.publish(Some(build_health_diagnostic_rollup_from_actors(
            self,
            executor,
            shared_app,
            executor.actor_entries(),
        )));
    }

    fn publish(&self, rollup: Option<HealthDiagnosticRollup>) {
        if let Some(mut rollup) = rollup {
            // Keep the timestamp with its payload so a reader cannot pair two
            // different publications when a refresh races with a probe.
            rollup.generated_at_ms = Some(self.now_ms());
            rollup.generation = self.published_generation.load(Ordering::Acquire) + 1;
            // A census this rollup measured is as old as the rollup; one it
            // carried forward keeps the time it was measured.
            rollup.memory_census_at_ms = rollup.memory_census_at_ms.or(rollup.generated_at_ms);
            let generation = rollup.generation;
            let rollup = Arc::new(rollup);
            let mut snapshot = self
                .snapshot
                .write()
                .unwrap_or_else(|error| error.into_inner());
            *snapshot = rollup;
            self.published_generation
                .store(generation, Ordering::Release);
            drop(snapshot);
            self.publications.send_replace(generation);
        } else {
            // Mark the census being served now as stale, without touching it.
            self.stale_through_generation.fetch_max(
                self.published_generation.load(Ordering::Acquire) + 1,
                Ordering::AcqRel,
            );
        }
        self.refreshes.fetch_add(1, Ordering::AcqRel);
    }

    #[cfg(test)]
    fn root_details_assembled_for_test(&self) -> usize {
        self.root_details_assembled.load(Ordering::Relaxed)
    }

    fn refresh_build_suspensions(
        &self,
        ctx: &AppContext,
        project_root: &std::path::Path,
        project_key: Option<&str>,
        cache_dir: &std::path::Path,
    ) {
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis()
            .min(u128::from(u64::MAX)) as u64;
        // The caller resolved the cache root once for the whole tick; the
        // breaker database lives under it for every context sharing the root.
        let Some(path) = crate::context::build_breaker_path_for_health(cache_dir, project_key)
        else {
            ctx.publish_build_suspensions_for_health(Vec::new(), now_ms);
            return;
        };
        let breaker = {
            let mut breakers = self
                .breakers
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            if let Some(breaker) = breakers.get(&path) {
                Arc::clone(breaker)
            } else {
                match crate::build_breaker::BuildDeathBreaker::open(&path) {
                    Ok(breaker) => {
                        let breaker = Arc::new(breaker);
                        breakers.insert(path, Arc::clone(&breaker));
                        breaker
                    }
                    Err(error) => {
                        log::debug!("health breaker open failed; retrying next rollup: {error}");
                        ctx.publish_build_suspensions_for_health(Vec::new(), now_ms);
                        return;
                    }
                }
            }
        };
        match breaker.active_suspensions_for_root_at(&project_root.display().to_string(), now_ms) {
            Ok(suspensions) => ctx.publish_build_suspensions_for_health(suspensions, now_ms),
            Err(error) => {
                log::debug!("health breaker read failed; reopening next rollup: {error}");
                self.breakers
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .retain(|_, cached| !Arc::ptr_eq(cached, &breaker));
                ctx.publish_build_suspensions_for_health(Vec::new(), now_ms);
            }
        }
    }

    fn now_ms(&self) -> u64 {
        #[cfg(test)]
        {
            let clock_ms = self.clock_ms.load(Ordering::Relaxed);
            if clock_ms != u64::MAX {
                return clock_ms;
            }
        }
        duration_millis_u64(self.origin.elapsed())
    }

    fn snapshot(&self) -> (Arc<HealthDiagnosticRollup>, Option<u64>) {
        let snapshot = match self.snapshot.try_read() {
            Ok(snapshot) => Arc::clone(&snapshot),
            Err(std::sync::TryLockError::Poisoned(error)) => Arc::clone(&error.into_inner()),
            Err(std::sync::TryLockError::WouldBlock) => Arc::new(HealthDiagnosticRollup {
                stale_reason: Some("cache_contended"),
                ..HealthDiagnosticRollup::unavailable()
            }),
        };
        let age_ms = snapshot
            .generated_at_ms
            .map(|generated| self.now_ms().saturating_sub(generated));
        (snapshot, age_ms)
    }

    /// Whether a contended refresh has found this census still being served.
    /// Judged by the census's own generation, so a reader holding an older
    /// census never pairs it with a later recovery.
    fn census_contended(&self, rollup: &HealthDiagnosticRollup) -> bool {
        rollup.generation < self.stale_through_generation.load(Ordering::Acquire)
    }

    fn annotate_plane_timings(&self, root: &str, value: &mut Value) {
        let now_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis()
            .min(u128::from(u64::MAX)) as u64;
        let Some(object) = value.as_object_mut() else {
            return;
        };
        let mut timings = self
            .plane_timings
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for plane in ["search_index", "semantic_index", "tier2"] {
            let Some(component) = object.get_mut(plane).and_then(Value::as_object_mut) else {
                timings.remove(&(root.to_string(), plane));
                continue;
            };
            let status = component
                .get("status")
                .and_then(Value::as_str)
                .unwrap_or("unknown")
                .to_string();
            let key = (root.to_string(), plane);
            if plane == "tier2" && status == "stale" {
                timings.remove(&key);
                component.remove("since_ms");
                component.remove("last_progress_at_ms");
                continue;
            }
            let reported_since_ms = component.get("since_ms").and_then(Value::as_u64);
            let progress_signature = serde_json::to_string(component).unwrap_or_default();
            let timing = timings.entry(key).or_insert_with(|| PlaneTiming {
                status: status.clone(),
                since_ms: reported_since_ms.unwrap_or(now_ms),
                last_progress_at_ms: now_ms,
                progress_signature: progress_signature.clone(),
            });
            if timing.status != status {
                timing.status = status;
                timing.since_ms = reported_since_ms.unwrap_or(now_ms);
                timing.last_progress_at_ms = now_ms;
                timing.progress_signature = progress_signature;
            } else if timing.progress_signature != progress_signature {
                timing.last_progress_at_ms = now_ms;
                timing.progress_signature = progress_signature;
            }
            component.insert("since_ms".to_string(), json!(timing.since_ms));
            component.insert(
                "last_progress_at_ms".to_string(),
                json!(timing.last_progress_at_ms),
            );
        }
    }

    #[cfg(test)]
    pub(super) fn refresh_count_for_test(&self) -> u64 {
        self.refreshes.load(Ordering::Acquire)
    }

    /// Count a `memory.census` read that wants current figures, so the next
    /// rollup measures the census. Returns the request's number; a census
    /// whose `memory_census_answers` reaches it was measured after the read.
    fn request_census(&self) -> u64 {
        self.census_requests.fetch_add(1, Ordering::AcqRel) + 1
    }

    /// Ask the next rollup for a fresh census and return the census served
    /// now, without waiting for one. [`fresh_memory_census`] waits.
    #[cfg(test)]
    pub(super) fn memory_census(&self) -> Value {
        self.request_census();
        let (snapshot, _) = self.snapshot();
        self.render_memory_census(&snapshot)
    }

    /// How old the census in `snapshot` is, on the cache clock; `None` before
    /// the first census.
    fn census_age_ms(&self, snapshot: &HealthDiagnosticRollup) -> Option<u64> {
        snapshot
            .memory_census_at_ms
            .map(|measured| self.now_ms().saturating_sub(measured))
    }

    fn render_memory_census(&self, snapshot: &HealthDiagnosticRollup) -> Value {
        let mut census = Value::clone(&snapshot.memory_census);
        // The census can be older than the rollup serving it. Its age, and
        // the ages of the measurements inside it, count from when it was
        // measured.
        let census_age_ms = self.census_age_ms(snapshot);
        if let Some(tick) = census
            .get_mut("health_rollup_tick")
            .and_then(Value::as_object_mut)
        {
            tick.insert("age_ms".into(), json!(census_age_ms));
        }
        if let Some(census_age_ms) = census_age_ms.filter(|age| *age > 0) {
            let add_age = |age: Option<&mut Value>| {
                if let Some(age) = age {
                    if let Some(measured) = age.as_u64() {
                        *age = json!(measured.saturating_add(census_age_ms));
                    }
                }
            };
            add_age(
                census
                    .get_mut("process")
                    .and_then(|process| process.get_mut("allocator_observation_age_ms")),
            );
            if let Some(rows) = census.get_mut("roots").and_then(Value::as_object_mut) {
                for row in rows.values_mut() {
                    add_age(row.get_mut("memory_measured_age_ms"));
                }
            }
        }
        census
    }

    /// Whether the next rollup may carry the published census forward instead
    /// of measuring a new one: it covers exactly the roots in `actor_entries`,
    /// every census request made so far was made before the rollup that
    /// measured it, and it is younger than
    /// [`HEALTH_MEMORY_CENSUS_INTERVAL_MS`]. Returns the request count read
    /// now, which a census measured by this rollup answers, and the census to
    /// carry with its measurement time and answered count.
    fn reusable_memory_census(
        &self,
        actor_entries: &[(ProjectRootId, Arc<AppContext>)],
    ) -> (u64, Option<CarriedCensus>) {
        let requests = self.census_requests.load(Ordering::Acquire);
        let published = Arc::clone(
            &self
                .snapshot
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        );
        let Some(measured_at_ms) = published.memory_census_at_ms else {
            return (requests, None);
        };
        if requests > published.memory_census_answers
            || published.generation == 0
            || self.now_ms().saturating_sub(measured_at_ms) >= HEALTH_MEMORY_CENSUS_INTERVAL_MS
        {
            return (requests, None);
        }
        // A root that was bound or dropped since must show in the census at
        // once, not after the interval.
        let Some(rows) = published
            .memory_census
            .get("roots")
            .and_then(Value::as_object)
        else {
            return (requests, None);
        };
        let same_roots = rows.len() == actor_entries.len()
            && actor_entries
                .iter()
                .all(|(root_id, _)| rows.contains_key(&root_id.as_path().display().to_string()));
        let carried = same_roots.then(|| CarriedCensus {
            census: Arc::clone(&published.memory_census),
            measured_at_ms,
            answers: published.memory_census_answers,
        });
        (requests, carried)
    }

    /// Start a rollup now for a census read that is waiting, if the rollup
    /// thread runs. A full wake channel already holds a wake, and the thread
    /// ends its quiet period on any wake while a census read waits.
    fn wake_for_census(&self) {
        if let Some(wake) = self
            .census_wake
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
        {
            let _ = wake.try_send(RollupWake::Census);
        }
    }

    fn census_read_waiting(&self) -> bool {
        self.census_waiters.load(Ordering::Acquire) > 0
    }

    #[cfg(test)]
    fn census_builds_for_test(&self) -> u64 {
        self.census_builds.load(Ordering::Relaxed)
    }
}

/// A published census a rollup carries forward instead of measuring.
struct CarriedCensus {
    census: Arc<Value>,
    measured_at_ms: u64,
    answers: u64,
}

/// Counts a census read as waiting for as long as it is alive.
struct CensusWaiter<'a>(&'a HealthRollupCache);

impl<'a> CensusWaiter<'a> {
    fn new(cache: &'a HealthRollupCache) -> Self {
        cache.census_waiters.fetch_add(1, Ordering::AcqRel);
        Self(cache)
    }
}

impl Drop for CensusWaiter<'_> {
    fn drop(&mut self) {
        self.0.census_waiters.fetch_sub(1, Ordering::AcqRel);
    }
}

/// The memory census for a `memory.census` read, at most one rollup old when
/// the rollup thread can provide one in time.
///
/// A census measured within the last rollup interval is returned at once.
/// An older one (or none yet) wakes the rollup thread for an immediate
/// rollup and waits until a census measured after this read started is
/// published, but no later than `deadline` less
/// [`MEMORY_CENSUS_REPLY_MARGIN`]. If none arrives in time, the census served
/// then is returned with its age, as any census is.
pub(super) async fn fresh_memory_census(cache: &HealthRollupCache, deadline: Instant) -> Value {
    let ticket = cache.request_census();
    let answered = |snapshot: &HealthDiagnosticRollup| snapshot.memory_census_answers >= ticket;
    let published = || {
        Arc::clone(
            &cache
                .snapshot
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        )
    };
    let snapshot = published();
    let current = cache
        .census_age_ms(&snapshot)
        .is_some_and(|age| age <= duration_millis_u64(HEALTH_ROLLUP_TTL));
    if current || answered(&snapshot) {
        return cache.render_memory_census(&snapshot);
    }
    let Some(wait_until) = deadline.checked_sub(MEMORY_CENSUS_REPLY_MARGIN) else {
        return cache.render_memory_census(&snapshot);
    };
    let _waiter = CensusWaiter::new(cache);
    // Subscribe before waking, so a publication between the wake and the
    // first wait is still seen.
    let mut publications = cache.publications.subscribe();
    cache.wake_for_census();
    loop {
        let snapshot = published();
        if answered(&snapshot) {
            return cache.render_memory_census(&snapshot);
        }
        match tokio::time::timeout_at(
            tokio::time::Instant::from_std(wait_until),
            publications.changed(),
        )
        .await
        {
            Ok(Ok(())) => {}
            // Out of time, or the cache is going away: serve what is there.
            Ok(Err(_)) | Err(_) => return cache.render_memory_census(&published()),
        }
    }
}

/// When a `memory.census` request must be answered by: its `deadline_ms`, or
/// [`MEMORY_CENSUS_DEFAULT_BUDGET`] after `received_at` when it names none.
pub(super) fn memory_census_deadline(body: Option<&Value>, received_at: Instant) -> Instant {
    body.and_then(client_deadline)
        .unwrap_or_else(|| received_at + MEMORY_CENSUS_DEFAULT_BUDGET)
}

/// Owned test rendezvous for publishing a rollup after configure has committed.
/// A probe-backed worker refreshes only on explicit requests, so a later periodic
/// census cannot replace the acknowledged snapshot before the wire assertion.
#[doc(hidden)]
#[derive(Clone, Default)]
pub struct SubcTestHealthRollupProbe {
    wake: Arc<std::sync::Mutex<Option<std::sync::mpsc::SyncSender<RollupWake>>>>,
    requests: Arc<std::sync::Mutex<Vec<tokio::sync::oneshot::Sender<()>>>>,
}

impl SubcTestHealthRollupProbe {
    #[doc(hidden)]
    pub fn new() -> Self {
        Self::default()
    }

    #[doc(hidden)]
    pub fn request_refresh(&self) -> tokio::sync::oneshot::Receiver<()> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.requests.lock().unwrap().push(tx);
        self.wake
            .lock()
            .unwrap()
            .as_ref()
            .expect("health worker started")
            .send(RollupWake::Refresh)
            .expect("health worker running");
        rx
    }
}

/// A message to the rollup thread.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RollupWake {
    /// Something changed (a bind completed); refresh after the quiet period.
    Refresh,
    /// A `memory.census` read waits for a census; refresh now.
    Census,
    Stop,
}

pub(super) struct HealthRollupWorker {
    wake_tx: std::sync::mpsc::SyncSender<RollupWake>,
    cache: Arc<HealthRollupCache>,
    join: Option<std::thread::JoinHandle<()>>,
}

impl HealthRollupWorker {
    pub(super) fn start_with_test_probe(
        cache: Arc<HealthRollupCache>,
        executor: Arc<Executor>,
        shared_app: Arc<App>,
        probe: SubcTestHealthRollupProbe,
    ) -> Self {
        let (wake_tx, wake_rx) = std::sync::mpsc::sync_channel(1);
        *probe.wake.lock().unwrap() = Some(wake_tx.clone());
        let worker_cache = Arc::clone(&cache);
        let join = std::thread::Builder::new()
            .name("aft-health-rollup".to_string())
            .spawn(move || {
                // Census wakes are not registered for this worker: it refreshes
                // only for probe requests, and a census read waits out its bound.
                while let Ok(wake) = wake_rx.recv() {
                    if wake == RollupWake::Stop {
                        break;
                    }
                    let requests = std::mem::take(&mut *probe.requests.lock().unwrap());
                    if requests.is_empty() {
                        continue;
                    }
                    worker_cache.refresh_for_test(&executor, &shared_app);
                    // Tickets are taken before assembly, not after publication:
                    // a request arriving mid-census needs a subsequent rollup.
                    for request in requests {
                        let _ = request.send(());
                    }
                }
            })
            .expect("spawn test health rollup worker");
        Self {
            wake_tx,
            cache,
            join: Some(join),
        }
    }

    pub(super) fn start(
        cache: Arc<HealthRollupCache>,
        executor: Arc<Executor>,
        shared_app: Arc<App>,
    ) -> Self {
        Self::start_with_interval(cache, executor, shared_app, HEALTH_ROLLUP_TTL)
    }

    fn start_with_interval(
        cache: Arc<HealthRollupCache>,
        executor: Arc<Executor>,
        shared_app: Arc<App>,
        interval: Duration,
    ) -> Self {
        let (wake_tx, wake_rx) = std::sync::mpsc::sync_channel(1);
        *cache
            .census_wake
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(wake_tx.clone());
        let worker_cache = Arc::clone(&cache);
        let join = std::thread::Builder::new()
            .name("aft-health-rollup".to_string())
            .spawn(move || 'worker: loop {
                worker_cache.refresh(&executor, &shared_app);
                // Bind completions can arrive during a rollup. A queued request
                // must not start another full census immediately; the interval
                // is a minimum quiet period after each completed refresh. A
                // census read that waits is the exception: it is answered now.
                let next_refresh = Instant::now() + interval;
                loop {
                    let remaining = next_refresh.saturating_duration_since(Instant::now());
                    if remaining.is_zero() {
                        break;
                    }
                    match wake_rx.recv_timeout(remaining) {
                        Ok(RollupWake::Census) => break,
                        // The one-slot channel may have held a bind wake when
                        // the census read tried to send its own.
                        Ok(RollupWake::Refresh) if worker_cache.census_read_waiting() => break,
                        Ok(RollupWake::Refresh) => continue,
                        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => break,
                        Ok(RollupWake::Stop)
                        | Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break 'worker,
                    }
                }
            })
            .expect("spawn health rollup worker");
        Self {
            wake_tx,
            cache,
            join: Some(join),
        }
    }

    pub(super) fn request_refresh(&self) {
        let _ = self.wake_tx.try_send(RollupWake::Refresh);
    }

    /// Stop the worker. The wait is capped: the worker may be in the middle of
    /// a rollup, which probes every root and language server, and this runs
    /// on the exit path, where each unbounded wait eats into the time the
    /// daemon allows a draining module to exit. Dropping the sender ends the
    /// worker at its next wait even if the stop message does not fit in the
    /// one-slot channel, so a worker that outlives the cap still stops.
    pub(super) fn shutdown(self) {
        let Self {
            wake_tx,
            cache,
            join,
        } = self;
        // The cache's copy of the sender would keep the channel open; take it
        // first so dropping ours disconnects the worker.
        cache
            .census_wake
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        let _ = wake_tx.try_send(RollupWake::Stop);
        drop(wake_tx);
        let Some(join) = join else {
            return;
        };
        let deadline = Instant::now() + HEALTH_ROLLUP_SHUTDOWN_WAIT;
        while !join.is_finished() {
            if Instant::now() >= deadline {
                log::info!(
                    "subc attach: health rollup still running after {} ms; not waiting for it",
                    HEALTH_ROLLUP_SHUTDOWN_WAIT.as_millis()
                );
                return;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        let _ = join.join();
    }
}

/// Cap on waiting for the health rollup worker to stop at connection end.
const HEALTH_ROLLUP_SHUTDOWN_WAIT: Duration = Duration::from_millis(200);

/// Tag for `allocator_slack_bytes` in the health rollup: the figure is
/// allocator address space (mapped minus in-use), not resident memory. It kept
/// its name for existing readers; the full meaning is
/// `crate::memory::ALLOCATOR_SLACK_LABEL`, carried by status and the census.
const ALLOCATOR_SLACK_KIND: &str = "address_space_not_resident";

/// Build the compact memory rollup from pre-aggregated root counters. Rich
/// subsystem detail is never constructed for roots omitted by the top-N cap.
fn memory_rollup_metrics(snapshot: Option<crate::memory::MemoryRollupSnapshot>) -> Value {
    let Some(snapshot) = snapshot else {
        return json!({
            "status": "busy",
            "roots": Value::Null,
            "roots_total": Value::Null,
            "roots_omitted": Value::Null,
            "roots_omitted_bytes": Value::Null,
            "rss_bytes": Value::Null,
            "phys_footprint_bytes": Value::Null,
            "total_attributed_bytes": Value::Null,
            "semantic_arena_bytes": Value::Null,
            "semantic_resident_chunks": Value::Null,
            "sqlite_bytes": Value::Null,
            "allocator_slack_bytes": Value::Null,
            "allocator_slack_kind": ALLOCATOR_SLACK_KIND,
            "allocator_slack_measured": false,
            "allocator_observation_age_ms": Value::Null,
            "last_relief_allocator_accounting_bytes": Value::Null,
            "last_relief_rss_drop_bytes": Value::Null,
        });
    };
    let per_root: Value = snapshot
        .roots
        .iter()
        .map(|(root, detail)| {
            let mut row = json!({
                "attributed_bytes": detail.attributed_bytes,
                "status": detail.status,
            });
            if detail.standing == Some(true) {
                row["standing"] = Value::Bool(true);
            }
            if let Some(age_ms) = detail.measured_age_ms {
                // The rollup may reuse an earlier index walk; name its age so
                // a reused figure is never read as a fresh one.
                row["memory_measured_age_ms"] = json!(age_ms);
            }
            (root.clone(), row)
        })
        .collect::<serde_json::Map<String, Value>>()
        .into();
    json!({
        "status": snapshot.roots_status,
        "roots": per_root,
        "roots_total": snapshot.roots_total,
        "roots_omitted": snapshot.roots_omitted,
        "roots_omitted_bytes": snapshot.roots_omitted_bytes,
        "rss_bytes": snapshot.process.rss_bytes,
        // Zero means either measured zero slack or unavailable allocator counters;
        // the sibling boolean disambiguates "no slack" from "unmeasurable".
        "allocator_slack_bytes": snapshot.process.allocator.retained_slack_bytes.unwrap_or(0),
        "allocator_slack_kind": ALLOCATOR_SLACK_KIND,
        "allocator_slack_measured": snapshot.process.allocator_slack_measured,
        "allocator_observation_age_ms": snapshot.process.allocator_observation_age_ms,
        // The latest periodic relief pass, as two labelled figures: the
        // allocator's own accounting and the RSS drop the process showed.
        "last_relief_allocator_accounting_bytes": crate::memory::last_allocator_relief_accounting_bytes(),
        "last_relief_rss_drop_bytes": crate::memory::last_allocator_relief_rss_drop_bytes(),
        // Headline number: excludes reclaimable pages RSS still counts.
        "phys_footprint_bytes": snapshot.process.phys_footprint_bytes,
        "total_attributed_bytes": snapshot.process.total_attributed_bytes,
        // Views-on semantic arenas, counted once per family and included in
        // `total_attributed_bytes`; the chunk count is one vector per chunk.
        "semantic_arena_bytes": snapshot.process.semantic_arena_bytes,
        "semantic_resident_chunks": snapshot.process.semantic_resident_chunks,
        "sqlite_bytes": snapshot.process.sqlite.memory_used_bytes,
    })
}

fn mutating_lanes_metrics(executor: &Executor) -> Value {
    match executor.try_mutating_lane_snapshots() {
        Some(snapshots) => render_mutating_lanes(snapshots),
        None => json!({ "scheduler_busy": true }),
    }
}

fn render_mutating_lanes(snapshots: Vec<crate::executor::MutatingLaneSnapshot>) -> Value {
    Value::Array(
        snapshots
            .into_iter()
            .map(|snapshot| {
                json!({
                    "root": snapshot.root_id.as_path().to_string_lossy(),
                    "request_id": snapshot.request_id,
                    "job": snapshot.command,
                    "started_age_ms": snapshot.started_age_ms,
                })
            })
            .collect(),
    )
}

fn insert_lifecycle_metrics(
    metrics: &mut Value,
    snapshot: crate::lifecycle_census::LifecycleCensusSnapshot,
) {
    let census = serde_json::to_value(snapshot).expect("lifecycle census serializes");
    let Some(target) = metrics.as_object_mut() else {
        return;
    };
    let Some(census) = census.as_object() else {
        return;
    };
    for (key, value) in census {
        target.insert(key.clone(), value.clone());
    }
}

fn dispatch_liveness_metrics(executor: &Executor) -> Value {
    match executor.try_dispatch_liveness_snapshot() {
        Some(snapshot) => json!({
            "interactive": {
                "queued": snapshot.interactive.queued,
                "oldest_age_ms": snapshot.interactive.oldest_age_ms,
            },
            "maintenance": {
                "queued": snapshot.maintenance.queued,
                "oldest_age_ms": snapshot.maintenance.oldest_age_ms,
            },
            "running": {
                "interactive": snapshot.running.interactive,
                "maintenance": snapshot.running.maintenance,
                "oldest_worker_owned_maintenance_age_ms": snapshot
                    .running
                    .oldest_worker_owned_maintenance_age_ms,
                "completion_owned": {
                    "interactive": snapshot.running.completion_owned_interactive,
                    "maintenance": snapshot.running.completion_owned_maintenance,
                },
                "phantom": {
                    "interactive": snapshot.running.phantom_interactive,
                    "maintenance": snapshot.running.phantom_maintenance,
                },
            },
            "phantom_running_maintenance": snapshot.phantom_running_maintenance,
            "interactive_reserve": snapshot.interactive_reserve,
            "maintenance_cap": snapshot.maintenance_cap,
        }),
        None => json!({ "scheduler_busy": true }),
    }
}

fn bash_db_hint_metrics() -> Value {
    #[cfg(unix)]
    {
        crate::bash_db_hints::metrics()
    }
    #[cfg(not(unix))]
    {
        Value::Null
    }
}

fn bash_task_retention_metrics(cache: &HealthRollupCache, shared_app: &App) -> Value {
    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    let now_ms = i64::try_from(now_ms).unwrap_or(i64::MAX);
    let database = shared_app.db();
    let (eligible_but_unpruned_rows, measured_age_ms) =
        cache.retention_count(database.as_ref(), now_ms);
    let last_sweep_skip_reason = database.as_ref().and_then(|db| {
        crate::db::compression_events::last_retention_sweep_skip_reason(db)
            .map(|reason| reason.as_str())
    });
    let (sweeps_skipped_total, consecutive_skips) =
        crate::db::compression_events::retention_sweep_skip_counts();
    json!({
        "eligible_but_unpruned_rows": eligible_but_unpruned_rows,
        "eligible_but_unpruned_rows_measured_age_ms": measured_age_ms,
        "last_sweep_skip_reason": last_sweep_skip_reason,
        "sweeps_skipped_total": sweeps_skipped_total,
        "consecutive_skips": consecutive_skips,
        "steady_state_ceiling": crate::db::compression_events::BASH_TASK_STEADY_STATE_ROWS,
        // Progress of the sweep over persisted task folders, per storage root.
        "disk_sweeps": serde_json::to_value(crate::bash_background::persisted_gc_health())
            .unwrap_or(Value::Null),
    })
}

fn build_health_diagnostic_rollup(
    cache: &HealthRollupCache,
    executor: &Executor,
    shared_app: &App,
) -> Option<HealthDiagnosticRollup> {
    Some(build_health_diagnostic_rollup_from_actors(
        cache,
        executor,
        shared_app,
        executor.try_actor_entries()?,
    ))
}

#[cfg(all(test, any(target_os = "linux", target_os = "macos")))]
#[path = "health_rollup_bench.rs"]
mod rollup_bench;

macro_rules! rollup_mark {
    ($timer:ident, $step:literal) => {
        $timer.mark($step);
    };
}

struct RollupStepTimer {
    started: Instant,
    previous: Instant,
    steps: std::collections::BTreeMap<&'static str, (Duration, u64)>,
    #[cfg(all(test, any(target_os = "linux", target_os = "macos")))]
    cpu: rollup_bench::Timer,
}

impl RollupStepTimer {
    fn new() -> Self {
        let started = Instant::now();
        Self {
            started,
            previous: started,
            steps: Default::default(),
            #[cfg(all(test, any(target_os = "linux", target_os = "macos")))]
            cpu: rollup_bench::Timer::new(),
        }
    }

    fn mark(&mut self, step: &'static str) {
        let now = Instant::now();
        let entry = self.steps.entry(step).or_default();
        entry.0 += now.duration_since(self.previous);
        entry.1 += 1;
        self.previous = now;
        #[cfg(all(test, any(target_os = "linux", target_os = "macos")))]
        self.cpu.mark(step);
    }

    fn metrics(&self) -> Value {
        // Elapsed time includes scheduler waits; the offline benchmark also
        // measures thread CPU so host contention is distinguishable from work.
        let steps = self
            .steps
            .iter()
            .map(|(step, (elapsed, calls))| {
                (
                    (*step).to_owned(),
                    json!({"elapsed_ms": elapsed.as_secs_f64() * 1000.0, "calls": calls}),
                )
            })
            .collect::<serde_json::Map<_, _>>();
        json!({"elapsed_ms": self.started.elapsed().as_secs_f64() * 1000.0, "steps": steps})
    }
}

/// Storage roots resolved once per rollup tick. Every context in one tick
/// that names the same configured storage and cache roots resolves to the
/// same directories, so resolving once per distinct configuration keeps a
/// many-root tick off realpath/getcwd instead of re-resolving once per
/// context. The resolved values are identical to calling
/// [`crate::bash_background::storage_dir`] and [`crate::root_cache::cache_root`]
/// directly: the memo is keyed by the exact configured spellings and dropped
/// with the tick, so a reconfiguration on the next tick resolves afresh.
#[derive(Default)]
struct TickStoragePaths {
    storage: HashMap<Option<std::path::PathBuf>, std::path::PathBuf>,
    cache: HashMap<(Option<std::path::PathBuf>, Option<std::path::PathBuf>), std::path::PathBuf>,
}

impl TickStoragePaths {
    fn storage_dir(&mut self, configured: Option<&std::path::Path>) -> std::path::PathBuf {
        let key = configured.map(std::path::Path::to_path_buf);
        if let Some(hit) = self.storage.get(&key) {
            return hit.clone();
        }
        let resolved = crate::bash_background::storage_dir(configured);
        self.storage.insert(key, resolved.clone());
        resolved
    }

    fn cache_dir(&mut self, config: &crate::config::Config) -> std::path::PathBuf {
        let key = (config.storage_dir.clone(), config.cache_dir.clone());
        if let Some(hit) = self.cache.get(&key) {
            return hit.clone();
        }
        let resolved = crate::root_cache::cache_root(config);
        self.cache.insert(key, resolved.clone());
        resolved
    }

    fn storage_paths(&mut self, config: &crate::config::Config) -> crate::root_cache::StoragePaths {
        crate::root_cache::StoragePaths::new(
            self.storage_dir(config.storage_dir.as_deref()),
            self.cache_dir(config),
        )
    }

    fn cache_unavailable_reason(
        &mut self,
        ctx: &AppContext,
        config: &crate::config::Config,
    ) -> Option<String> {
        let cache_dir = self.cache_dir(config);
        ctx.cache_unavailable_reason_for_root(&cache_dir)
    }
}

/// The unavailable code of each context whose cache location is unavailable,
/// keyed by its cache root. The reason is read once per context: a recovery
/// can clear it at any moment, so a second read may find nothing. The
/// description computes the cache root too, so a tick that already resolved
/// it passes both together instead of resolving the same root twice.
fn cache_unavailable_codes<'a>(
    contexts: impl Iterator<Item = &'a AppContext>,
    mut describe: impl FnMut(&AppContext) -> Option<(std::path::PathBuf, String)>,
) -> std::collections::BTreeMap<std::path::PathBuf, &'static str> {
    contexts
        .filter_map(|ctx| {
            let (cache_dir, reason) = describe(ctx)?;
            Some((cache_dir, crate::root_cache::unavailable_code(&reason)))
        })
        .collect()
}

fn build_health_diagnostic_rollup_from_actors(
    cache: &HealthRollupCache,
    executor: &Executor,
    shared_app: &App,
    actor_entries: Vec<(ProjectRootId, Arc<AppContext>)>,
) -> HealthDiagnosticRollup {
    let mut timer = RollupStepTimer::new();
    struct RootCandidate {
        root_label: String,
        health: RootHealthSnapshot,
        busy: bool,
        fully_ready: bool,
        attributed_bytes: u64,
        repair_entries_60s: Option<u64>,
        // This field deliberately covers only already-open resident stores. A
        // standing report is necessary because stale rows block projection by
        // making dead-code findings disappear rather than raising an error.
        resident_callgraph_stale_backend_rows: Option<crate::callgraph_store::StalePathCensus>,
        watcher: Option<crate::context::WatcherCountersSnapshot>,
        disclaim_privacy: Option<bool>,
        standing: Option<StandingHealthEntry>,
    }

    let lifecycle_contexts = actor_entries
        .iter()
        .map(|(_, context)| Arc::clone(context))
        .collect::<Vec<_>>();
    // Decided before the roots are visited: a carried census needs no
    // per-root census rows.
    let (census_requests, reused_census) = cache.reusable_memory_census(&actor_entries);
    let measure_census = reused_census.is_none();
    let standing_entries = standing_health_entries(&actor_entries);
    rollup_mark!(timer, "standing roots / database open");
    let mut standing_matched = vec![false; standing_entries.len()];
    let actor_count = actor_entries.len();
    let mut memory_roots = std::collections::BTreeMap::new();
    let mut census_roots = std::collections::BTreeMap::new();
    let mut census_ages = std::collections::BTreeMap::new();
    let mut candidates = Vec::with_capacity(actor_count.saturating_add(standing_entries.len()));
    let mut repair_roots_annotated = 0usize;
    let mut embedding_backend_available = true;
    let mut embedding_backend_paused_until_ms: Option<u64> = None;
    let mut embedding_backend_down = false;
    let mut embedding_backend_last_error = None;
    let mut embedding_backend_since_ms = None;
    let mut task_storage_roots = std::collections::HashSet::new();
    let mut bash_task_refusals = Vec::new();
    // Process-wide storage paths cannot change mid-tick: every context that
    // names the same configured roots shares the resolution below instead of
    // repeating its realpath and current-directory probes once per context.
    let mut tick_paths = TickStoragePaths::default();
    for (root_id, ctx) in actor_entries {
        let config = ctx.config();
        let storage = tick_paths.storage_dir(config.storage_dir.as_deref());
        if task_storage_roots.insert(storage.clone()) {
            // Refused tasks are named health diagnostics, not a reason to
            // disable shell execution for every route sharing this daemon.
            bash_task_refusals.extend(
                crate::persisted_format::refusals_under(&storage)
                    .into_iter()
                    .filter(|refusal| {
                        refusal.store == crate::persisted_format::PersistedStore::BashTask
                    })
                    .map(|refusal| {
                        json!({
                            "code": crate::persisted_format::CODE,
                            "path": refusal.path.display().to_string(),
                            "found": refusal.found,
                            "supported": refusal.supported,
                            "message": refusal.to_string(),
                        })
                    }),
            );
        }
        if tick_paths
            .cache_unavailable_reason(ctx.as_ref(), &config)
            .is_none()
        {
            crate::storage_retention::schedule(
                tick_paths.storage_paths(&config),
                ctx.subc_lifecycle_admission(),
                ctx.configure_generation_flag(),
                ctx.configure_generation(),
            );
        }
        let root_label = root_id.as_path().display().to_string();
        let standing_index = standing_entries.iter().position(|entry| {
            entry
                .root_id
                .as_ref()
                .is_some_and(|entry_root| entry_root == &root_id)
        });
        if let Some(index) = standing_index {
            standing_matched[index] = true;
        }
        let standing = standing_index.map(|index| standing_entries[index].clone());
        let memory_key = standing
            .as_ref()
            .and_then(|entry| entry.artifact_key.clone())
            .unwrap_or_else(|| root_label.clone());
        rollup_mark!(timer, "root setup / retention scheduling");
        let (census_memory, memory) = ctx.memory_root_snapshot_and_rollup();
        rollup_mark!(timer, "per-root memory census");
        if measure_census {
            census_ages.insert(root_label.clone(), memory.measured_age_ms);
            census_roots.insert(root_label.clone(), census_memory);
        }
        let memory = if standing.is_some() {
            memory.with_standing()
        } else {
            memory
        };
        let attributed_bytes = memory.attributed_bytes;
        memory_roots.insert(memory_key, memory);

        let project_key = crate::search_index::artifact_cache_key_memoized_only(root_id.as_path());
        let artifact_key = standing
            .as_ref()
            .and_then(|entry| entry.artifact_key.as_deref())
            .or(project_key.as_deref());
        // Durable breaker state is loaded during the off-path rollup, then the
        // cached health reply reads only the context snapshot. Standing keys can
        // be scoped or path-based, so they must not depend on the session-key memo.
        let cache_dir = tick_paths.cache_dir(&config);
        cache.refresh_build_suspensions(ctx.as_ref(), root_id.as_path(), artifact_key, &cache_dir);
        let repair_entries_60s = artifact_key.and_then(|key| {
            repair_roots_annotated = repair_roots_annotated.saturating_add(1);
            crate::callgraph_store::repair_entry_rate(key)
                .map(|(count, _window_start)| count)
                .filter(|count| *count > 0)
        });
        let resident_callgraph_stale_backend_rows =
            ctx.callgraph_store().try_read().ok().and_then(|store| {
                store
                    .as_ref()
                    .and_then(|store| store.try_stale_path_census().ok().flatten())
            });
        let backend = ctx.semantic_backend_health_snapshot();
        if !backend.available {
            embedding_backend_available = false;
            if let Some(until) = backend.paused_until_ms {
                embedding_backend_paused_until_ms =
                    Some(embedding_backend_paused_until_ms.map_or(until, |old| old.max(until)));
            } else {
                embedding_backend_down = true;
            }
            let replace_error = embedding_backend_since_ms
                .is_none_or(|since| backend.since_ms.is_some_and(|candidate| candidate <= since));
            if replace_error {
                embedding_backend_last_error = backend.last_error;
            }
            embedding_backend_since_ms = match (embedding_backend_since_ms, backend.since_ms) {
                (Some(current), Some(candidate)) => Some(current.min(candidate)),
                (current, candidate) => current.or(candidate),
            };
        }
        if let Some(backend) =
            crate::semantic_index::embedding_backend_build_health(root_id.as_path())
                .filter(|_| crate::synapse_embed::lane_pause_for_root(root_id.as_path()).is_none())
        {
            embedding_backend_available = false;
            embedding_backend_down = true;
            if embedding_backend_since_ms.is_none_or(|since| backend.since_ms <= since) {
                embedding_backend_last_error = Some(backend.last_error);
            }
            embedding_backend_since_ms = Some(
                embedding_backend_since_ms
                    .map_or(backend.since_ms, |since| since.min(backend.since_ms)),
            );
        }
        rollup_mark!(timer, "root disk probes / breaker / callgraph");
        let health_summary = ctx.try_health_summary_for_root(Some(&cache_dir));
        rollup_mark!(timer, "index readiness / view status");
        let busy = health_summary.is_busy();
        let fully_ready = health_summary.is_fully_ready();
        candidates.push(RootCandidate {
            root_label,
            health: health_summary.into_snapshot(root_id.as_path()),
            busy,
            fully_ready,
            attributed_bytes,
            repair_entries_60s,
            resident_callgraph_stale_backend_rows,
            watcher: Some(ctx.watcher_counters().snapshot()),
            disclaim_privacy: Some(ctx.config().bash.disclaim_privacy),
            standing,
        });
        rollup_mark!(timer, "root summary assembly");
    }

    // A refusal can prevent an entry from owning an actor. Keep it visible in
    // the same health table, but do not invent a second memory aggregation row.
    for (index, standing) in standing_entries.into_iter().enumerate() {
        if standing_matched[index] {
            continue;
        }
        let health = unhosted_standing_health_snapshot(&standing);
        candidates.push(RootCandidate {
            root_label: health.project_root.clone(),
            disclaim_privacy: None,
            health,
            busy: false,
            fully_ready: false,
            attributed_bytes: 0,
            repair_entries_60s: None,
            resident_callgraph_stale_backend_rows: None,
            watcher: None,
            standing: Some(standing),
        });
    }

    let busy_roots = candidates.iter().filter(|candidate| candidate.busy).count();
    let warming_roots = candidates
        .iter()
        .filter(|candidate| !candidate.busy && !candidate.fully_ready)
        .count();
    let standing_refusals = candidates
        .iter()
        .filter(|candidate| {
            candidate
                .standing
                .as_ref()
                .and_then(|entry| entry.refusal.as_ref())
                .is_some()
        })
        .count();
    candidates.sort_by(|left, right| {
        right
            .attributed_bytes
            .cmp(&left.attributed_bytes)
            .then_with(|| left.root_label.cmp(&right.root_label))
    });
    let root_details_omitted = candidates.len().saturating_sub(HEALTH_ROOT_DETAIL_CAP);
    let roots: Vec<(String, Value)> = candidates
        .into_iter()
        .take(HEALTH_ROOT_DETAIL_CAP)
        .map(|candidate| {
            #[cfg(test)]
            cache.root_details_assembled.fetch_add(1, Ordering::Relaxed);
            let mut snapshot = candidate.health;
            snapshot.callgraph_repair_entries_60s = candidate.repair_entries_60s;
            let root_label = snapshot.project_root.clone();
            let mut value = standing_root_health_value(snapshot, candidate.standing.as_ref());
            if let Some(backend) =
                crate::semantic_index::embedding_backend_build_health(Path::new(&root_label))
                    .filter(|_| {
                        crate::synapse_embed::lane_pause_for_root(Path::new(&root_label)).is_none()
                    })
            {
                if let Some(semantic) = value
                    .get_mut("semantic_index")
                    .and_then(Value::as_object_mut)
                {
                    semantic.insert("status".to_string(), json!("backend_unavailable"));
                    semantic.insert("reason".to_string(), json!(backend.last_error));
                    semantic.insert("since_ms".to_string(), json!(backend.since_ms));
                    semantic.insert("next_retry_ms".to_string(), json!(backend.next_retry_ms));
                    semantic.remove("stage");
                    semantic.remove("embedded_chunks");
                    semantic.remove("total_chunks");
                    semantic.remove("current_batch");
                    semantic.remove("total_batches");
                }
            }
            cache.annotate_plane_timings(&root_label, &mut value);
            let query_embed =
                crate::semantic_index::query_embed_health_snapshot(Path::new(&root_label));
            if let Some(semantic) = value
                .get_mut("semantic_index")
                .and_then(Value::as_object_mut)
            {
                semantic.insert(
                    "query_embed_timeouts".to_string(),
                    json!(query_embed.query_embed_timeouts),
                );
                semantic.insert(
                    "query_embed_p50_ms".to_string(),
                    json!(query_embed.query_embed_p50_ms),
                );
            }
            if let Some(object) = value.as_object_mut() {
                if let Some(configured) = candidate.disclaim_privacy {
                    // The root snapshot may already carry a `bash` block with
                    // background task counts; add the privacy fields to it
                    // rather than replacing those counts.
                    let bash = object.entry("bash".to_owned()).or_insert_with(|| json!({}));
                    if !bash.is_object() {
                        *bash = json!({});
                    }
                    if let Some(bash) = bash.as_object_mut() {
                        bash.insert("disclaim_privacy".to_owned(), json!(configured));
                        bash.insert(
                            "privacy_disclaim_effective".to_owned(),
                            json!(configured && cfg!(target_os = "macos")),
                        );
                    }
                }
                if let Some(census) = candidate.resident_callgraph_stale_backend_rows {
                    object.insert(
                        "resident_callgraph_stale_backend_rows".to_string(),
                        json!(census.stale),
                    );
                    object.insert(
                        "resident_callgraph_absent_stale_backend_rows".to_string(),
                        json!(census.absent),
                    );
                    object.insert(
                        "resident_callgraph_unreadable_stale_backend_rows".to_string(),
                        json!(census.unreadable),
                    );
                    // Files the call graph left out because they are not
                    // valid UTF-8; reported so a missing file is explainable.
                    object.insert(
                        "resident_callgraph_undecodable_files".to_string(),
                        json!(census.undecodable),
                    );
                }
                if let Some(watcher) = candidate.watcher {
                    object.insert("watcher".to_string(), json!(watcher));
                }
            }
            (root_label, value)
        })
        .collect();
    let roots = roots
        .into_iter()
        .map(|(_, snapshot)| snapshot)
        .collect::<Vec<_>>();

    let root_count = root_details_omitted.saturating_add(roots.len());
    let callgraph_repair_entries_60s_total = crate::callgraph_store::repair_entry_rate_total();
    let callgraph_write_metrics_total = crate::callgraph_store::callgraph_write_metrics_total();
    rollup_mark!(timer, "root JSON assembly");
    let allocator = cache.allocator_observation();
    rollup_mark!(timer, "allocator observation");
    // Shared bases and semantic arenas are measured once for the compact
    // rollup; a rollup that also measures the census reuses that measurement.
    let shared_memory = crate::memory::SharedProcessMemory::measure();
    let memory = memory_rollup_metrics(Some(crate::memory::MemoryRollupSnapshot::with_shared(
        "ready",
        memory_roots,
        allocator.clone(),
        &shared_memory,
    )));
    let census_snapshot = measure_census.then(|| {
        crate::memory::MemorySnapshot::new_uncapped_with_shared(
            "ready",
            census_roots,
            allocator,
            shared_memory,
        )
    });
    rollup_mark!(timer, "process memory snapshots");
    // The tick's resolved roots are reused here: the same directories the
    // per-context loop above resolved are not resolved again per context.
    let mut cache_roots = std::collections::BTreeSet::new();
    for ctx in &lifecycle_contexts {
        cache_roots.insert(tick_paths.cache_dir(&ctx.config()));
    }
    let cache_unavailable = cache_unavailable_codes(
        lifecycle_contexts.iter().map(Arc::as_ref),
        |ctx: &AppContext| {
            let config = ctx.config();
            let cache_dir = tick_paths.cache_dir(&config);
            let reason = ctx.cache_unavailable_reason_for_root(&cache_dir)?;
            Some((cache_dir, reason))
        },
    );
    let measured_census = census_snapshot.map(|census_snapshot| {
        #[cfg(test)]
        cache.census_builds.fetch_add(1, Ordering::Relaxed);
        let census_rows = cache
            .census_rows
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .render(&census_snapshot);
        let mut memory_census = crate::commands::memory_census::render_memory_census_with_roots(
            &census_snapshot,
            census_rows,
        );
        memory_census["cache_roots"] = json!(cache_roots);
        memory_census["cache_unavailable"] = json!(cache_unavailable);
        if let Some(rows) = memory_census
            .get_mut("roots")
            .and_then(Value::as_object_mut)
        {
            for (root, age_ms) in census_ages {
                if let Some(row) = rows.get_mut(&root).and_then(Value::as_object_mut) {
                    row.insert("memory_measured_age_ms".into(), json!(age_ms));
                }
            }
        }
        memory_census
    });
    rollup_mark!(timer, "census JSON rendering");
    let lifecycle = crate::lifecycle_census::collect(shared_app, &lifecycle_contexts);
    rollup_mark!(timer, "lifecycle OS probes");
    shared_app.publish_lifecycle_census(lifecycle.clone());
    let detail = if busy_roots > 0 {
        Some(format!(
            "{busy_roots} root actor(s) could not be snapshotted without contention"
        ))
    } else if standing_refusals > 0 {
        Some(format!(
            "{standing_refusals} standing root(s) refused current resolved-path identity"
        ))
    } else if warming_roots > 0 {
        Some(format!(
            "{warming_roots} root(s) warming background indexes (serving normally)"
        ))
    } else {
        None
    };

    let storage_retention = lifecycle_contexts
        .iter()
        .filter_map(|ctx| {
            let storage = tick_paths.cache_dir(&ctx.config());
            crate::storage_retention::snapshot(&storage)
                .map(|report| (storage.display().to_string(), report))
        })
        .collect::<std::collections::BTreeMap<_, _>>();
    let storage_retention_totals = retention_report_totals(&storage_retention);
    rollup_mark!(timer, "storage retention snapshots");
    let bash_retention = bash_task_retention_metrics(cache, shared_app);
    rollup_mark!(timer, "bash retention database count");
    // Every context normally shares one cache root, but each context may name
    // its own, so these lists grow with the root count. The size-budgeted
    // reply carries at most HEALTH_ROOT_DETAIL_CAP entries of each plus an
    // omitted count; the uncapped memory census above keeps the full lists.
    let cache_roots_omitted = cache_roots.len().saturating_sub(HEALTH_ROOT_DETAIL_CAP);
    let cache_roots = cache_roots
        .into_iter()
        .take(HEALTH_ROOT_DETAIL_CAP)
        .collect::<Vec<_>>();
    let cache_unavailable_omitted = cache_unavailable
        .len()
        .saturating_sub(HEALTH_ROOT_DETAIL_CAP);
    let cache_unavailable = cache_unavailable
        .into_iter()
        .take(HEALTH_ROOT_DETAIL_CAP)
        .collect::<std::collections::BTreeMap<_, _>>();
    let mut metrics = json!({
        "cache_roots": cache_roots,
        "cache_roots_omitted": cache_roots_omitted,
        "cache_unavailable": cache_unavailable,
        "cache_unavailable_omitted": cache_unavailable_omitted,
        "actor_count": actor_count,
        "root_count": root_count,
        "warming_roots": warming_roots,
        "root_details_omitted": root_details_omitted,
        "callgraph_repair_entries_60s_total": callgraph_repair_entries_60s_total,
        "callgraph_repair_roots_annotated": repair_roots_annotated,
        "callgraph_repair_roots_total": actor_count,
        "callgraph_commits_60s_total": callgraph_write_metrics_total.commits_60s,
        "callgraph_pages_or_bytes_written_60s_total": callgraph_write_metrics_total.pages_or_bytes_written_60s,
        "lsp_children": {
            "spawned": lifecycle.lsp.children_total,
            "cwd_gone": lifecycle.lsp.children_with_deleted_cwd,
        },
        "embedding_backend": {
            "available": embedding_backend_available,
            "state": if embedding_backend_available { "available" } else if !embedding_backend_down && embedding_backend_paused_until_ms.is_some() { "paused" } else { "backend_unavailable" },
            "paused_until_ms": embedding_backend_paused_until_ms,
            "last_error": embedding_backend_last_error,
            "since_ms": embedding_backend_since_ms,
        },
        "memory": memory,
        "bash_task_retention": bash_retention,
        "storage_retention": storage_retention,
        "storage_retention_totals": storage_retention_totals,
        "storage_retention_reports_omitted": 0,
        "bash_db_schema_hints": bash_db_hint_metrics(),
        "bash_task_refusals": bash_task_refusals,
        "mutating_lanes": mutating_lanes_metrics(executor),
        "process_io": crate::process_io::ProcessIoSnapshot::capture().to_value(),
        "roots": roots,
    });
    insert_lifecycle_metrics(&mut metrics, lifecycle);
    rollup_mark!(timer, "metrics JSON assembly");
    let tick = timer.metrics();
    if log::log_enabled!(log::Level::Debug) && cache.step_log_due() {
        log::debug!("health rollup tick: {tick}");
    }
    // Keep detailed timing outside the size-budgeted health reply: adding it
    // there would displace root diagnostics. The uncapped census retains the
    // timing of the rollup that measured it.
    let (memory_census, memory_census_at_ms, memory_census_answers) =
        match (measured_census, reused_census) {
            (Some(mut memory_census), _) => {
                memory_census["health_rollup_tick"] = tick.clone();
                (Arc::new(memory_census), None, census_requests)
            }
            (None, Some(carried)) => (
                carried.census,
                Some(carried.measured_at_ms),
                carried.answers,
            ),
            (None, None) => unreachable!("a census is measured whenever none is reused"),
        };
    HealthDiagnosticRollup {
        status: if busy_roots > 0 || standing_refusals > 0 {
            HealthStatus::Degraded
        } else {
            HealthStatus::Ok
        },
        detail,
        generated_at_ms: None,
        generation: 0,
        stale_reason: None,
        metrics,
        memory_census,
        memory_census_at_ms,
        memory_census_answers,
        #[cfg(test)]
        tick,
    }
}

fn render_cold_build_limiter_census(
    limiter: crate::cold_build_limiter::ColdBuildLimiterCensus,
) -> Value {
    let render_entry = |entry: crate::cold_build_limiter::ColdBuildCensusEntry| {
        json!({
            "domain": entry.domain,
            "root": entry.root,
            "kind": entry.kind,
            "sharers": entry.sharers,
            "acquired_at_ms": entry.acquired_at_ms,
            "age_ms": entry.age_ms,
        })
    };
    json!({
        "cap": limiter.cap,
        "holders": limiter.holders.into_iter().map(&render_entry).collect::<Vec<_>>(),
        "queued": limiter.queued.into_iter().map(render_entry).collect::<Vec<_>>(),
    })
}

pub(super) fn build_health_report(
    cache: &HealthRollupCache,
    executor: &Executor,
    pending_binds: &HashMap<RouteChannel, PendingBind>,
    dispatch_path_metrics: &DispatchPathMetrics,
    shared_app: &App,
) -> HealthReport {
    build_health_report_with_snapshots(
        cache,
        executor,
        pending_binds,
        dispatch_path_metrics,
        shared_app,
        crate::cold_build_limiter::progress::snapshot,
        crate::gc::family::sweep_stats(),
    )
}

fn build_health_report_with_snapshots(
    cache: &HealthRollupCache,
    executor: &Executor,
    pending_binds: &HashMap<RouteChannel, PendingBind>,
    dispatch_path_metrics: &DispatchPathMetrics,
    shared_app: &App,
    indexing_snapshot: impl FnOnce(u64) -> crate::cold_build_limiter::progress::Snapshot,
    sweep_snapshot: crate::gc::family::SweepStatsSnapshot,
) -> HealthReport {
    // The diagnostic payload is fixed-size and cached. Only probe-purpose
    // liveness signals are read fresh, using atomics or non-blocking snapshots.
    let (rollup, snapshot_age_ms) = cache.snapshot();
    // A contended refresh, now or at the last rollup, leaves the retained
    // census in place and reports it as stale.
    let scheduler_busy = executor.try_actor_count().is_none() || cache.census_contended(&rollup);
    let stale_reason = if scheduler_busy {
        Some("executor_contended")
    } else {
        rollup.stale_reason
    };
    let mut metrics = rollup
        .metrics
        .as_object()
        .cloned()
        .unwrap_or_else(serde_json::Map::new);
    // The count's age is recorded at assembly; include time spent serving this
    // rollup too, including when scheduler contention delays the next census.
    if let Some(age) = metrics
        .get_mut("bash_task_retention")
        .and_then(|retention| retention.get_mut("eligible_but_unpruned_rows_measured_age_ms"))
    {
        if let Some(measured_age_ms) = age.as_u64() {
            *age = json!(measured_age_ms.saturating_add(snapshot_age_ms.unwrap_or(0)));
        }
    }
    let lsp_children = metrics.remove("lsp_children").unwrap_or(Value::Null);
    // Read the Mutating lanes fresh, without waiting on the scheduler, so an
    // operator sees what holds a root and for how long right now, not as of
    // the last background rollup. A busy scheduler falls back to that rollup.
    let cached_mutating_lanes = metrics.remove("mutating_lanes");
    let mutating_lanes = match executor.try_mutating_lane_snapshots() {
        Some(snapshots) => render_mutating_lanes(snapshots),
        None => cached_mutating_lanes.unwrap_or_else(|| json!({ "scheduler_busy": true })),
    };
    metrics.insert("snapshot_age_ms".to_string(), json!(snapshot_age_ms));
    metrics.insert("age_ms".to_string(), json!(snapshot_age_ms));
    metrics.insert("generated_at_ms".to_string(), json!(rollup.generated_at_ms));
    metrics.insert("stale".to_string(), json!(stale_reason.is_some()));
    metrics.insert("stale_reason".to_string(), json!(stale_reason));
    metrics.insert(
        "durable_log".to_string(),
        crate::logging::durable_log_health(),
    );
    metrics.insert(
        "runtime".to_string(),
        json!({
            "live_watchers": shared_app.watcher_count(),
            "live_actor_roots": shared_app.actor_root_count(),
            "open_routes": shared_app.open_route_count(),
            "bg_subscriptions": dispatch_path_metrics.bg_subscriptions.load(Ordering::Relaxed),
            "bg_wake_pending": dispatch_path_metrics.bg_wake_pending.load(Ordering::Relaxed),
            "bg_wake_unacked_total": dispatch_path_metrics.bg_wake_unacked_total.load(Ordering::Relaxed),
            "bg_wake_rearm_total": dispatch_path_metrics.bg_wake_rearm_total.load(Ordering::Relaxed),
            "bg_nudges_enqueued_60s_total": dispatch_path_metrics.bg_nudges_enqueued_60s_total(),
            "bg_arm_misses_60s_total": dispatch_path_metrics.bg_arm_misses_60s_total(),
            "spawned_lsp_children": lsp_children.get("spawned").cloned().unwrap_or(Value::Null),
            "lsp_children_with_deleted_cwd": lsp_children.get("cwd_gone").cloned().unwrap_or(Value::Null),
        }),
    );
    let (backup_skipped_too_large_total, backup_skipped_temp_path_total) =
        crate::backup::backup_skipped_totals();
    metrics.insert(
        "backup_skipped_too_large_total".to_string(),
        json!(backup_skipped_too_large_total),
    );
    metrics.insert(
        "backup_skipped_temp_path_total".to_string(),
        json!(backup_skipped_temp_path_total),
    );
    metrics.insert("reap".to_string(), dispatch_path_metrics.reap_snapshot());
    #[cfg(unix)]
    {
        let spawn = crate::posix_spawn::health();
        metrics.insert("bash_spawn".to_string(), spawn["backend"].clone());
        metrics.insert(
            "bash_spawn_missing_capability".to_string(),
            spawn["missing_capability"].clone(),
        );
        metrics.insert(
            "bash_spawn_fallback_spawns".to_string(),
            spawn["fallback_spawns"].clone(),
        );
        metrics.insert("bash_pty_spawn".to_string(), spawn["pty_backend"].clone());
    }
    metrics.insert(
        "cold_build_limiter".to_string(),
        render_cold_build_limiter_census(crate::cold_build_limiter::global_limiter().census()),
    );
    let indexing = indexing_snapshot(
        metrics
            .get("warming_roots")
            .and_then(Value::as_u64)
            .unwrap_or(0),
    );
    let indexing_detail = indexing_detail(&indexing);
    metrics.insert("indexing".to_string(), json!(indexing));
    metrics.insert(
        "dispatch_liveness".to_string(),
        dispatch_liveness_metrics(executor),
    );
    let mut dispatch_path = dispatch_path_metrics.snapshot(pending_binds);
    if let Some(dispatch_path) = dispatch_path.as_object_mut() {
        dispatch_path.insert("mutating_lanes".to_string(), mutating_lanes);
    }
    metrics.insert("dispatch_path".to_string(), dispatch_path);
    metrics.insert(
        "process_io".to_string(),
        crate::process_io::ProcessIoSnapshot::capture().to_value(),
    );
    metrics.insert("views_work".to_string(), crate::views::io::work_snapshot());
    metrics.insert(
        "v2_family_sweeps".to_string(),
        json!(sweep_snapshot.details),
    );
    metrics.insert(
        "v2_family_sweep_details_evicted_total".into(),
        json!(sweep_snapshot.evicted_total),
    );
    metrics.insert(
        "v2_family_sweep_details_retired_total".into(),
        json!(sweep_snapshot.retired_total),
    );
    metrics.insert("v2_family_sweeps_omitted".into(), json!(0));
    metrics.insert(
        "filesystem_helpers".to_string(),
        crate::bounded_io::helper_health(),
    );
    // Routes the fleet status dial stopped reopening after a terminal refusal,
    // each naming its target, session and refusal code. An empty list means
    // no route is stopped.
    metrics.insert(
        "stopped_routes".to_string(),
        crate::fleet_status::stopped_routes_snapshot(),
    );
    metrics.insert(
        "write_ledger_top_10m".to_string(),
        json!(crate::write_ledger::recent_top_writers()),
    );
    metrics.insert(
        "write_ledger_folds_deferred_total".to_string(),
        json!(crate::db::write_ledger::folds_deferred_total()),
    );
    // Keep the default health headline focused on root health and freshness.
    // Include a metric only when this snapshot contains a numeric value.
    let headline = ["actor_count", "warming_roots", "snapshot_age_ms"]
        .into_iter()
        .filter(|key| metrics.get(*key).is_some_and(Value::is_number))
        .map(|key| json!(key))
        .collect::<Vec<_>>();
    metrics.insert("headline".to_string(), json!(headline));
    budget_health_metrics(&mut metrics);

    let gap_health = dispatch_path_metrics.watch_delivery.health();
    let watch_gap_unresolved = gap_health["status"] != "ok";
    let warning = if watch_gap_unresolved {
        Some(format!(
            "watch delivery is degraded: {}",
            gap_health["reason"].as_str().unwrap_or("retry_overflow")
        ))
    } else if scheduler_busy {
        Some(EXECUTOR_SCHEDULER_CONTENTION_DETAIL.to_string())
    } else {
        rollup.detail.clone()
    };
    let detail = match (indexing_detail, warning) {
        (Some(indexing), Some(warning)) if !warning.contains("warming background indexes") => {
            Some(format!("{indexing}; {warning}"))
        }
        (Some(indexing), _) => Some(indexing),
        (None, warning) => warning,
    };
    HealthReport {
        status: if scheduler_busy || watch_gap_unresolved {
            HealthStatus::Degraded
        } else {
            rollup.status.clone()
        },
        detail,
        metrics: Some(Value::Object(metrics)),
    }
}

fn grouped_count(count: u64) -> String {
    let digits = count.to_string();
    let mut result = String::new();
    for (index, ch) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            result.push(',');
        }
        result.push(ch);
    }
    result
}

fn format_indexing_rate(files_per_minute: f64) -> String {
    let (rate, unit) = if files_per_minute >= 60.0 {
        (files_per_minute / 60.0, "files/s")
    } else if files_per_minute >= 1.0 {
        (files_per_minute, "files/min")
    } else {
        (files_per_minute * 60.0, "files/h")
    };
    let magnitude = rate.abs().log10().floor() as i32;
    let scale = 10_f64.powi(magnitude - 1);
    let rounded = (rate / scale).round() * scale;
    let precision = (1 - magnitude).max(0) as usize;
    let formatted = format!("{rounded:.precision$}");
    let formatted = if precision > 0 {
        formatted.trim_end_matches('0').trim_end_matches('.')
    } else {
        &formatted
    };
    format!("~{formatted} {unit}")
}

fn indexing_detail(indexing: &crate::cold_build_limiter::progress::Snapshot) -> Option<String> {
    let running = indexing.running.len() + indexing.omitted.running;
    let queued = indexing.queued.len() + indexing.omitted.queued;
    if running == 0 && queued == 0 {
        return None;
    }

    let first_running = indexing.running.first();
    let mut detail = if let Some(job) = first_running {
        let progress = match job.total {
            Some(total) => format!("{}/{} files", grouped_count(job.done), grouped_count(total)),
            None if job.done == 0 => "preparing".to_owned(),
            None => format!("{} files (total measuring)", grouped_count(job.done)),
        };
        let measured_rate = job.rate_per_minute;
        let estimate = if let Some(files_per_minute) = measured_rate {
            let rate = format_indexing_rate(files_per_minute);
            let eta = if job.total.is_some() {
                job.eta_seconds.map(|seconds| {
                    if seconds >= 3600.0 {
                        format!("~{:.0} h", seconds / 3600.0)
                    } else if seconds >= 60.0 {
                        format!("~{:.0} min", seconds / 60.0)
                    } else {
                        format!("~{:.0} s", seconds)
                    }
                })
            } else {
                None
            };
            match eta {
                Some(eta) => format!(" ({eta}, {rate})"),
                None => format!(" ({rate})"),
            }
        } else {
            String::new()
        };
        let phase = if job.phase == "starting" {
            "starting".to_owned()
        } else if measured_rate.is_none() {
            format!("{}; measuring rate", job.phase)
        } else {
            job.phase.to_owned()
        };
        format!(
            "indexing: {} {} {}{} [{}]",
            job.kind, job.root_label, progress, estimate, phase
        )
    } else if running > 0 {
        format!("indexing: {running} running; details omitted")
    } else {
        format!("indexing: {queued} queued")
    };
    if first_running.is_some() && running > 1 {
        detail.push_str(&format!(", {} other running", running - 1));
    }
    if running > 0 && queued > 0 {
        detail.push_str(&format!(", {queued} queued"));
    }
    if indexing.warming_roots > 0 {
        detail.push_str(&format!("; {} roots warming", indexing.warming_roots));
    }
    Some(detail)
}

#[cfg(test)]
mod tests {
    #[test]
    #[ignore = "release health probe sizing measurement"]
    fn bench_daemon_edge_health_probe_sizing() {
        let input = serde_json::json!({
            "roots": (0..8).map(|i| serde_json::json!({"path":i,"detail":"é\\\"".repeat(2000)})).collect::<Vec<_>>(),
            "storage_retention": (0..20).map(|i| (format!("report-{i:02}"), serde_json::json!({"detail":"r".repeat(1500)}))).collect::<serde_json::Map<_,_>>()
        }).as_object().unwrap().clone();
        super::HEALTH_SIZE_WORK.with(|work| *work.borrow_mut() = (0, 0));
        let start = std::time::Instant::now();
        for _ in 0..100 {
            let mut metrics = input.clone();
            super::budget_health_metrics(std::hint::black_box(&mut metrics));
            std::hint::black_box(metrics);
        }
        eprintln!(
            "health probes=100 wall_us={} work={:?}",
            start.elapsed().as_micros(),
            super::HEALTH_SIZE_WORK.with(|work| *work.borrow())
        );
    }

    #[test]
    fn health_self_size_uses_one_borrowed_traversal() {
        let mut metrics = serde_json::json!({"roots":[{"path":"é\\\"","detail":"x".repeat(4096)}]})
            .as_object()
            .unwrap()
            .clone();
        super::HEALTH_SIZE_WORK.with(|work| *work.borrow_mut() = (0, 0));
        let bytes = super::refresh_metrics_bytes(&mut metrics);
        let work = super::HEALTH_SIZE_WORK.with(|work| *work.borrow());
        eprintln!(
            "health sizing: traversals={} temporary_serialized_bytes={}",
            work.0, work.1
        );
        assert_eq!(bytes, serde_json::to_vec(&metrics).unwrap().len());
        assert_eq!(
            work,
            (1, 0),
            "one borrowed traversal and no serialized buffer"
        );
    }

    fn legacy_health_budget(metrics: &mut serde_json::Map<String, serde_json::Value>) {
        use serde_json::json;
        let mut ledger_compacted = false;
        loop {
            metrics.insert("metrics_bytes".into(), json!(0));
            let mut bytes = 0;
            for _ in 0..4 {
                bytes = serde_json::to_vec(&serde_json::Value::Object(metrics.clone()))
                    .unwrap()
                    .len();
                if metrics["metrics_bytes"].as_u64() == Some(bytes as u64) {
                    break;
                }
                metrics.insert("metrics_bytes".into(), json!(bytes));
            }
            if bytes <= super::HEALTH_METRICS_BUDGET_BYTES {
                return;
            }
            if metrics
                .get_mut("roots")
                .and_then(serde_json::Value::as_array_mut)
                .is_some_and(|roots| roots.pop().is_some())
            {
                let omitted = metrics
                    .get("root_details_omitted")
                    .and_then(serde_json::Value::as_u64)
                    .unwrap_or(0)
                    .saturating_add(1);
                metrics.insert("root_details_omitted".into(), json!(omitted));
                continue;
            }
            if !ledger_compacted {
                super::compact_write_ledger_root_ids(metrics);
                ledger_compacted = true;
                continue;
            }
            let key = metrics
                .get("storage_retention")
                .and_then(serde_json::Value::as_object)
                .and_then(|reports| reports.keys().next_back().cloned());
            if let Some(key) = key {
                metrics["storage_retention"]
                    .as_object_mut()
                    .unwrap()
                    .remove(&key);
                let omitted = metrics
                    .get("storage_retention_reports_omitted")
                    .and_then(serde_json::Value::as_u64)
                    .unwrap_or(0)
                    .saturating_add(1);
                metrics.insert("storage_retention_reports_omitted".into(), json!(omitted));
            } else {
                return;
            }
        }
    }

    /// A cache recovery can clear a context's unavailable reason at any
    /// moment. The health rollup reads it once per context, so a recovery
    /// landing right after the read cannot panic the health worker, and the
    /// next refresh reads the new state.
    #[test]
    fn health_cache_codes_survive_a_recovery_between_reads() {
        let temp = tempfile::tempdir().unwrap();
        let ctx = AppContext::new(
            Box::new(crate::parser::TreeSitterProvider::new()),
            crate::config::Config {
                storage_dir: Some(temp.path().join("storage")),
                ..Default::default()
            },
        );
        // Each read returns the reason once, then the recovered state. The
        // description also resolves the cache root, mirroring the tick's
        // once-per-context read of both together.
        let reads = std::sync::atomic::AtomicUsize::new(0);
        let recovering = |ctx: &AppContext| {
            (reads.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0).then(|| {
                (
                    ctx.cache_dir(),
                    "cache_dir_unavailable: volume missing".to_string(),
                )
            })
        };
        let refresh = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            super::cache_unavailable_codes(std::iter::once(&ctx), &recovering)
        }));
        let codes = refresh.expect("a recovery between reads panicked the health refresh");
        assert_eq!(
            codes.get(&ctx.cache_dir()).copied(),
            Some(crate::root_cache::CACHE_DIR_UNAVAILABLE)
        );
        // The next refresh runs and sees the recovered state.
        assert!(super::cache_unavailable_codes(std::iter::once(&ctx), &recovering).is_empty());
    }

    #[test]
    fn health_budget_drops_long_cache_root_paths_and_counts_them() {
        use serde_json::json;
        let long = |index: usize| format!("/{}/{index}", "cache-volume".repeat(200));
        let mut metrics = json!({
            "cache_roots": (0..8).map(long).collect::<Vec<_>>(),
            "cache_roots_omitted": 0,
            "cache_unavailable": (0..4)
                .map(|index| (long(index), "cache_dir_unavailable"))
                .collect::<std::collections::BTreeMap<_, _>>(),
            "cache_unavailable_omitted": 0,
        })
        .as_object()
        .unwrap()
        .clone();
        super::budget_health_metrics(&mut metrics);
        let bytes = serde_json::to_vec(&metrics).unwrap().len();
        assert!(bytes <= super::HEALTH_METRICS_BUDGET_BYTES, "{bytes}");
        assert_eq!(metrics["metrics_bytes"].as_u64(), Some(bytes as u64));
        let listed = metrics["cache_roots"].as_array().unwrap().len() as u64;
        assert_eq!(listed + metrics["cache_roots_omitted"].as_u64().unwrap(), 8);
        let unavailable = metrics["cache_unavailable"].as_object().unwrap().len() as u64;
        assert_eq!(
            unavailable + metrics["cache_unavailable_omitted"].as_u64().unwrap(),
            4
        );
        // Unavailable roots are trimmed first: they repeat paths already
        // counted in cache_roots.
        assert_eq!(unavailable, 0);
    }

    #[test]
    fn health_borrowed_budget_matches_golden_json_and_digit_boundaries() {
        use serde_json::json;
        let mut tiny = json!({"a":"é"}).as_object().unwrap().clone();
        super::budget_health_metrics(&mut tiny);
        assert_eq!(
            serde_json::to_string(&tiny).unwrap(),
            r#"{"a":"é","metrics_bytes":29}"#
        );
        for length in [
            0, 58, 59, 60, 61, 957, 958, 959, 960, 9956, 9957, 9958, 9959, 13000,
        ] {
            let mut actual = json!({"fresh_liveness":"x".repeat(length)})
                .as_object()
                .unwrap()
                .clone();
            let mut expected = actual.clone();
            legacy_health_budget(&mut expected);
            super::budget_health_metrics(&mut actual);
            assert_eq!(
                serde_json::to_vec(&actual).unwrap(),
                serde_json::to_vec(&expected).unwrap(),
                "length={length}"
            );
            assert_eq!(
                actual["metrics_bytes"].as_u64().unwrap() as usize,
                serde_json::to_vec(&actual).unwrap().len()
            );
        }
        let mut oversized = json!({
            "roots": (0..8).map(|i| json!({"path":i,"detail":"é\\\"".repeat(2000)})).collect::<Vec<_>>(),
            "root_details_omitted": 2,
            "write_ledger_top_10m": [{"root_id":format!("/very/long/{}/root", "directory/".repeat(1000)), "bytes": 123}],
            "storage_retention": (0..20).map(|i| (format!("report-{i:02}"), json!({"detail":"r".repeat(1500)}))).collect::<serde_json::Map<_,_>>()
        }).as_object().unwrap().clone();
        let mut expected = oversized.clone();
        legacy_health_budget(&mut expected);
        super::budget_health_metrics(&mut oversized);
        assert_eq!(
            serde_json::to_vec(&oversized).unwrap(),
            serde_json::to_vec(&expected).unwrap()
        );
        assert_eq!(oversized["write_ledger_top_10m"][0]["root_id"], "root");
        assert!(
            oversized["storage_retention_reports_omitted"]
                .as_u64()
                .unwrap()
                > 0
        );
        let mut cached = json!({"roots":[{"detail":"r".repeat(12000)}]})
            .as_object()
            .unwrap()
            .clone();
        super::budget_health_metrics(&mut cached);
        assert_eq!(cached["roots"].as_array().unwrap().len(), 1);
        cached.insert("fresh_liveness".into(), json!("f".repeat(1000)));
        let mut expected = cached.clone();
        legacy_health_budget(&mut expected);
        super::budget_health_metrics(&mut cached);
        assert_eq!(
            serde_json::to_vec(&cached).unwrap(),
            serde_json::to_vec(&expected).unwrap()
        );
        assert_eq!(cached["roots"].as_array().unwrap().len(), 0);
    }

    #[test]
    fn long_running_daemon_sweeps_preserve_root_and_standing_detail() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = crate::gc::family::SweepStatsStore::default();
        for family in 0..96 {
            let family_dir = dir
                .path()
                .join(format!("family-{family:03}-{}", "p".repeat(120)));
            std::fs::create_dir(&family_dir).unwrap();
            // Thirty days of ten-minute passes reuse each family's counters.
            for _ in 0..4320 {
                store.record_stale_rows(&family_dir.join("semantic.db"), "family", 1);
            }
        }
        let sweeps = store.snapshot();
        assert_eq!(sweeps.details.len(), 64);
        assert_eq!(sweeps.evicted_total, 32);
        assert_eq!(sweeps.retired_total, 0);
        assert!(
            serde_json::to_vec(&sweeps.details).unwrap().len() > super::HEALTH_METRICS_BUDGET_BYTES
        );
        let cache = HealthRollupCache::new();
        let mut rollup = HealthDiagnosticRollup::unavailable();
        rollup.metrics = json!({
            "root_count": 170,
            "root_details_omitted": 168,
            "roots": [{"root":"active-root"}, {"root":"standing-root", "standing_health":{"status":"ready"}}],
        });
        cache.publish(Some(rollup));
        let executor = Executor::new();
        let app = App::default_shared();
        let report = super::build_health_report_with_snapshots(
            &cache,
            &executor,
            &HashMap::new(),
            &DispatchPathMetrics::new(),
            &app,
            crate::cold_build_limiter::progress::snapshot,
            sweeps,
        );
        let metrics = report.metrics.unwrap();
        assert_eq!(metrics["roots"].as_array().unwrap().len(), 2);
        assert_eq!(metrics["roots"][1]["standing_health"]["status"], "ready");
        assert_eq!(metrics["root_details_omitted"], 168);
        assert_eq!(metrics["v2_family_sweep_details_evicted_total"], 32);
        assert_eq!(metrics["v2_family_sweep_details_retired_total"], 0);
        let kept = metrics["v2_family_sweeps"].as_array().unwrap();
        assert!(kept.len() < 64);
        assert_eq!(metrics["v2_family_sweeps_omitted"], 64 - kept.len());
        assert!(kept.iter().all(|entry| entry["stale_rows_touched"] == 4320));
        assert!(metrics["v2_family_sweeps_omitted"].as_u64().unwrap() > 0);
        assert!(
            metrics["metrics_bytes"].as_u64().unwrap() <= super::HEALTH_METRICS_BUDGET_BYTES as u64
        );
    }

    #[test]
    fn health_budget_trims_every_sweep_before_root_detail() {
        let roots = json!([{"root":"standing", "standing_health":{"status":"ready"}, "detail":"r".repeat(13000)}]);
        let mut metrics = json!({
            "roots": roots,
            "root_details_omitted": 0,
            "v2_family_sweeps": [{"family":"one"}, {"family":"two"}],
            "v2_family_sweeps_omitted": 3,
        })
        .as_object()
        .unwrap()
        .clone();
        super::budget_health_metrics(&mut metrics);
        assert_eq!(metrics["v2_family_sweeps"], json!([]));
        assert_eq!(metrics["v2_family_sweeps_omitted"], 5);
        assert_eq!(metrics["root_details_omitted"], 1);
        assert_eq!(metrics["roots"], json!([]));
    }

    #[test]
    fn health_fixture_ignores_process_global_sweep_telemetry() {
        let dir = tempfile::tempdir().unwrap();
        crate::gc::family::record_stale_rows(&dir.path().join("semantic.db"), "foreign-family", 1);
        let executor = Executor::new();
        let app = App::default_shared();
        let report = test_health_report(
            &executor,
            &HashMap::new(),
            &DispatchPathMetrics::new(),
            &app,
        );
        let metrics = report.metrics.unwrap();
        assert_eq!(metrics["v2_family_sweeps"], json!([]));
        assert_eq!(metrics["v2_family_sweep_details_evicted_total"], 0);
        assert_eq!(metrics["v2_family_sweep_details_retired_total"], 0);
        assert_eq!(metrics["v2_family_sweeps_omitted"], 0);
    }

    #[test]
    fn bash_deadline_reply_log_proves_actual_daemon_handoff() {
        use std::cell::RefCell;
        let metrics = super::DispatchPathMetrics::new();
        let now = super::Instant::now();
        let mut frame = crate::subc::Frame::build_with_version(
            crate::subc::PROTOCOL_VERSION,
            crate::subc::FrameType::Request,
            crate::subc::control_flags(),
            1280,
            1,
            1335,
            serde_json::to_vec(&serde_json::json!({"name":"bash"})).unwrap(),
        )
        .unwrap();
        metrics.tool_received(&frame, now);
        let lines = RefCell::new(Vec::new());
        let sink = |line: &str| lines.borrow_mut().push(line.to_string());
        metrics.tool_replied_with(&frame, now, &sink);
        assert!(
            lines.borrow().is_empty(),
            "ingress cannot stand in for actual egress"
        );
        frame.header.ty = crate::subc::FrameType::Response;
        metrics.tool_replied_with(&frame, now + super::Duration::from_millis(12), &sink);
        assert_eq!(lines.borrow().len(), 1);
        assert!(lines.borrow()[0]
            .contains("channel=1280 epoch=1 corr=1335 total_ms=12 destination=subc_daemon"));
        assert!(metrics.unanswered_tools.lock().unwrap().is_empty());
        metrics.tool_replied_with(&frame, now, &sink);
        assert_eq!(
            lines.borrow().len(),
            1,
            "an untracked response is not a second handoff"
        );
    }
    #[test]
    fn bash_deadline_watchdog_logs_unanswered_calls_until_actual_write() {
        use std::cell::RefCell;
        let metrics = super::DispatchPathMetrics::new();
        let now = super::Instant::now();
        let request = crate::subc::Frame::build_with_version(
            crate::subc::PROTOCOL_VERSION,
            crate::subc::FrameType::Request,
            crate::subc::control_flags(),
            24,
            1,
            1188,
            serde_json::to_vec(&serde_json::json!({"name":"bash"})).unwrap(),
        )
        .unwrap();
        metrics.tool_received(&request, now);
        metrics.set_bash_unanswered_deadline(
            super::super::route_key(24, 1),
            1188,
            now + super::Duration::from_secs(25),
        );
        let lines = RefCell::new(Vec::new());
        let sink = |line: &str| lines.borrow_mut().push(line.to_string());
        metrics.warn_unanswered_tools_with(now + super::Duration::from_secs(24), &sink);
        assert!(lines.borrow().is_empty());
        metrics.warn_unanswered_tools_with(now + super::Duration::from_secs(25), &sink);
        assert_eq!(lines.borrow().len(), 1);
        assert!(lines.borrow()[0].contains("name=bash channel=24 epoch=1 corr=1188"));
        metrics.warn_unanswered_tools_with(now + super::Duration::from_secs(30), &sink);
        assert_eq!(
            lines.borrow().len(),
            1,
            "warn only once per unanswered call"
        );
        let mut reply = request;
        reply.header.ty = crate::subc::FrameType::Response;
        metrics.tool_replied(&reply);
        assert!(metrics.unanswered_tools.lock().unwrap().is_empty());
    }

    #[test]
    fn bash_call_without_submission_keeps_fallback_watchdog_deadline() {
        use std::cell::RefCell;

        let metrics = super::DispatchPathMetrics::new();
        let now = super::Instant::now();
        let request = crate::subc::Frame::build_with_version(
            crate::subc::PROTOCOL_VERSION,
            crate::subc::FrameType::Request,
            crate::subc::control_flags(),
            24,
            1,
            1187,
            serde_json::to_vec(&serde_json::json!({"name":"bash"})).unwrap(),
        )
        .unwrap();
        metrics.tool_received(&request, now);

        let lines = RefCell::new(Vec::new());
        let sink = |line: &str| lines.borrow_mut().push(line.to_string());
        metrics.warn_unanswered_tools_with(now + super::Duration::from_secs(24), &sink);
        assert!(lines.borrow().is_empty());
        metrics.warn_unanswered_tools_with(now + super::Duration::from_secs(25), &sink);
        assert_eq!(lines.borrow().len(), 1);
        assert!(lines.borrow()[0].contains("name=bash channel=24 epoch=1 corr=1187"));
        metrics.warn_unanswered_tools_with(now + super::Duration::from_secs(30), &sink);
        assert_eq!(
            lines.borrow().len(),
            1,
            "warn only once per unanswered call"
        );
    }

    #[test]
    fn bash_client_deadline_watchdog_does_not_double_count_reader_queue_time() {
        use std::cell::RefCell;

        let metrics = super::DispatchPathMetrics::new();
        let now = super::Instant::now();
        let now_ms = super::SystemTime::now()
            .duration_since(super::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;
        let request = crate::subc::Frame::build_with_version(
            crate::subc::PROTOCOL_VERSION,
            crate::subc::FrameType::Request,
            crate::subc::control_flags(),
            24,
            1,
            1190,
            serde_json::to_vec(&serde_json::json!({
                "name": "bash",
                "deadline_ms": now_ms + 10_000,
            }))
            .unwrap(),
        )
        .unwrap();
        metrics.tool_received(&request, now - super::Duration::from_secs(5));

        let lines = RefCell::new(Vec::new());
        let sink = |line: &str| lines.borrow_mut().push(line.to_string());
        metrics.warn_unanswered_tools_with(now + super::Duration::from_secs(9), &sink);
        assert!(
            lines.borrow().is_empty(),
            "reader queue time must not shorten the client's remaining ten seconds: {:?}",
            lines.borrow()
        );
        metrics.warn_unanswered_tools_with(now + super::Duration::from_secs(11), &sink);
        assert_eq!(lines.borrow().len(), 1);
        assert!(lines.borrow()[0].contains("name=bash channel=24 epoch=1 corr=1190"));
        metrics.warn_unanswered_tools_with(now + super::Duration::from_secs(12), &sink);
        assert_eq!(lines.borrow().len(), 1, "each call warns only once");
    }

    #[test]
    fn subscription_streams_do_not_get_unary_deadline_warnings() {
        use std::cell::RefCell;

        let metrics = super::DispatchPathMetrics::new();
        let now = super::Instant::now();
        let request = |corr, body: serde_json::Value| {
            crate::subc::Frame::build_with_version(
                crate::subc::PROTOCOL_VERSION,
                crate::subc::FrameType::Request,
                crate::subc::control_flags(),
                24,
                1,
                corr,
                serde_json::to_vec(&body).unwrap(),
            )
            .unwrap()
        };
        let subscription = request(1187, serde_json::json!({"op": "bg_events"}));
        let unary = request(1188, serde_json::json!({"name": "configure"}));
        metrics.tool_received(&subscription, now);
        metrics.tool_received(&unary, now);

        let lines = RefCell::new(Vec::new());
        let sink = |line: &str| lines.borrow_mut().push(line.to_string());
        metrics.warn_unanswered_tools_with(now + super::Duration::from_secs(26), &sink);

        assert_eq!(lines.borrow().len(), 1);
        assert!(lines.borrow()[0].contains("corr=1188"));
        assert!(!lines.borrow()[0].contains("corr=1187"));
        metrics.warn_unanswered_tools_with(now + super::Duration::from_secs(52), &sink);
        assert_eq!(lines.borrow().len(), 1, "unary calls warn only once");
    }

    #[test]
    fn bash_watch_deadline_warning_uses_its_effective_wait_and_grace() {
        use std::cell::RefCell;

        let worker_limit_ms = 180_000;
        let primary_cap_ms = 120_000;
        let requested_ms = super::super::bash_watch_warning_wait_ms(
            &serde_json::json!({"timeout_ms": 60_000}),
            false,
            worker_limit_ms,
            primary_cap_ms,
        )
        .unwrap();
        assert_eq!(requested_ms, 60_000);
        assert_eq!(
            super::super::bash_watch_warning_wait_ms(
                &serde_json::json!({"timeoutMs": 60_000}),
                false,
                worker_limit_ms,
                primary_cap_ms,
            ),
            Some(60_000)
        );
        assert_eq!(
            super::super::bash_watch_warning_wait_ms(
                &serde_json::json!({}),
                true,
                worker_limit_ms,
                primary_cap_ms,
            ),
            Some(worker_limit_ms)
        );
        assert_eq!(
            super::super::bash_watch_warning_wait_ms(
                &serde_json::json!({}),
                false,
                worker_limit_ms,
                primary_cap_ms,
            ),
            Some(crate::commands::bash_watch::DEFAULT_PRIMARY_WATCH_TIMEOUT_MS)
        );
        assert_eq!(
            super::super::bash_watch_warning_wait_ms(
                &serde_json::json!({"background": true, "timeout_ms": 60_000}),
                true,
                worker_limit_ms,
                primary_cap_ms,
            ),
            Some(worker_limit_ms)
        );

        let metrics = super::DispatchPathMetrics::new();
        let now = super::Instant::now();
        let request = crate::subc::Frame::build_with_version(
            crate::subc::PROTOCOL_VERSION,
            crate::subc::FrameType::Request,
            crate::subc::control_flags(),
            24,
            1,
            1186,
            serde_json::to_vec(&serde_json::json!({
                "name": "bash_watch",
                "arguments": {"timeout_ms": requested_ms},
            }))
            .unwrap(),
        )
        .unwrap();
        metrics.tool_received(&request, now);
        metrics.set_bash_watch_unanswered_wait(super::super::route_key(24, 1), 1186, requested_ms);

        let lines = RefCell::new(Vec::new());
        let sink = |line: &str| lines.borrow_mut().push(line.to_string());
        metrics.warn_unanswered_tools_with(now + super::Duration::from_secs(25), &sink);
        assert!(lines.borrow().is_empty());
        metrics.warn_unanswered_tools_with(now + super::Duration::from_millis(65_000), &sink);
        assert_eq!(lines.borrow().len(), 1);
        assert!(lines.borrow()[0].contains("name=bash_watch"));
    }

    #[test]
    fn bash_deadline_watchdog_uses_each_calls_effective_deadline() {
        use std::cell::RefCell;

        let metrics = super::DispatchPathMetrics::new();
        let now = super::Instant::now();
        let request = |corr, arguments| {
            crate::subc::Frame::build_with_version(
                crate::subc::PROTOCOL_VERSION,
                crate::subc::FrameType::Request,
                crate::subc::control_flags(),
                24,
                1,
                corr,
                serde_json::to_vec(&serde_json::json!({
                    "name": "bash",
                    "arguments": arguments,
                }))
                .unwrap(),
            )
            .unwrap()
        };

        let waiting = request(1188, serde_json::json!({"wait": true}));
        let waiting_deadline = super::super::bash::unanswered_bash_reply_deadline(
            now, 120_000, 15_000, None, 1_800_000,
        );
        metrics.tool_received(&waiting, now);
        metrics.set_bash_unanswered_deadline(
            super::super::route_key(24, 1),
            1188,
            waiting_deadline,
        );

        let expiring = request(1189, serde_json::json!({"timeout": 5_000}));
        let expiring_deadline =
            super::super::bash::unanswered_bash_reply_deadline(now, 20_000, 20_000, None, 5_000);
        metrics.tool_received(&expiring, now);
        metrics.set_bash_unanswered_deadline(
            super::super::route_key(24, 1),
            1189,
            expiring_deadline,
        );

        let lines = RefCell::new(Vec::new());
        let sink = |line: &str| lines.borrow_mut().push(line.to_string());
        metrics.warn_unanswered_tools_with(now + super::Duration::from_secs(26), &sink);
        assert_eq!(lines.borrow().len(), 1);
        assert!(lines.borrow()[0].contains("corr=1189"));

        metrics.warn_unanswered_tools_with(now + super::Duration::from_secs(121), &sink);
        assert_eq!(lines.borrow().len(), 2);
        assert!(lines.borrow()[1].contains("corr=1188"));
        metrics.warn_unanswered_tools_with(now + super::Duration::from_secs(130), &sink);
        assert_eq!(lines.borrow().len(), 2, "each call warns only once");
    }
    use super::super::test_support::{test_ctx, test_root};
    use super::super::{Lane, Response};
    use super::*;

    #[test]
    fn root_broadcast_pressure_warns_per_root_and_kind_once_per_minute() {
        let (_dir_a, a) = test_root("broadcast-warning-a");
        let (_dir_b, b) = test_root("broadcast-warning-b");
        let metrics = DispatchPathMetrics::new();
        let logs = std::cell::RefCell::new(Vec::<String>::new());
        let capture = |line: &str| logs.borrow_mut().push(line.to_owned());
        let now = Instant::now();
        metrics.record_root_broadcast_with(&a, "status_changed", 1024, 1024, now, &capture);
        assert!(
            logs.borrow().is_empty(),
            "threshold is strictly greater than 1 MiB"
        );
        metrics.record_root_broadcast_with(&a, "status_changed", 22_000, 50, now, &capture);
        metrics.record_root_broadcast_with(
            &a,
            "status_changed",
            22_000,
            50,
            now + Duration::from_secs(10),
            &capture,
        );
        metrics.record_root_broadcast_with(
            &a,
            "status_changed",
            22_000,
            50,
            now + Duration::from_secs(60),
            &capture,
        );
        metrics.record_root_broadcast_with(
            &a,
            "configure_warnings",
            22_000,
            50,
            now + Duration::from_secs(60),
            &capture,
        );
        metrics.record_root_broadcast_with(
            &b,
            "status_changed",
            22_000,
            50,
            now + Duration::from_secs(60),
            &capture,
        );
        assert_eq!(logs.borrow().len(), 4);
        assert!(logs.borrow()[0].contains("kind=status_changed body_bytes=22000 route_count=50"));
        assert!(logs.borrow()[2].contains("kind=configure_warnings"));
        assert!(logs.borrow()[3].contains(&b.as_path().display().to_string()));
        assert_eq!(
            metrics.snapshot(&HashMap::new())["writer"]["root_broadcast_over_1mib_total"],
            5
        );
    }

    #[test]
    fn status_push_backstop_logs_component_sizes_and_counts_suppressed_events() {
        let (_dir, root) = test_root("status-compaction-observability");
        let frame = crate::protocol::StatusChangedFrame::new(
            None,
            json!({"semantic_index":{"status":"loading","entries_done":42_000,"error":"\u{0000}".repeat(12_000)}}),
        );
        let body = crate::status_push::bounded_body(&frame);
        assert!(body.compacted);
        let metrics = DispatchPathMetrics::new();
        let logs = std::cell::RefCell::new(Vec::<String>::new());
        let capture = |line: &str| logs.borrow_mut().push(line.to_owned());
        let now = Instant::now();
        metrics.record_status_push_with(&root, &body, 1, now, &capture);
        metrics.record_status_push_with(&root, &body, 1, now + Duration::from_secs(10), &capture);
        metrics.record_status_push_with(&root, &body, 1, now + Duration::from_secs(60), &capture);
        assert_eq!(
            logs.borrow().len(),
            2,
            "compaction warnings share the existing per-root/kind rate limit"
        );
        let logs = logs.borrow();
        assert!(logs[0].contains("kind=status_changed"));
        assert!(logs[0].contains(&format!("body_bytes={}", body.original_bytes)));
        assert!(logs[0].contains("recipients=1"));
        assert!(logs[0].contains("top_level_field_bytes={\"semantic_index\":"));
        assert!(logs[0].contains(&root.as_path().display().to_string()));
        assert_eq!(
            metrics.snapshot(&HashMap::new())["writer"]["status_push_compacted_total"],
            3
        );
        assert_eq!(
            metrics.snapshot(&HashMap::new())["writer"]["root_broadcast_over_1mib_total"],
            0
        );
    }
    use serde_json::json;

    // Health fixtures own their roots, not sweep observations made by other
    // tests in the process. Production still supplies the live store snapshot.
    fn build_health_report(
        cache: &HealthRollupCache,
        executor: &Executor,
        pending_binds: &HashMap<RouteChannel, PendingBind>,
        metrics: &DispatchPathMetrics,
        app: &App,
    ) -> HealthReport {
        build_health_report_with_indexing(
            cache,
            executor,
            pending_binds,
            metrics,
            app,
            crate::cold_build_limiter::progress::snapshot,
        )
    }

    fn build_health_report_with_indexing(
        cache: &HealthRollupCache,
        executor: &Executor,
        pending_binds: &HashMap<RouteChannel, PendingBind>,
        metrics: &DispatchPathMetrics,
        app: &App,
        indexing_snapshot: impl FnOnce(u64) -> crate::cold_build_limiter::progress::Snapshot,
    ) -> HealthReport {
        super::build_health_report_with_snapshots(
            cache,
            executor,
            pending_binds,
            metrics,
            app,
            indexing_snapshot,
            crate::gc::family::SweepStatsSnapshot::default(),
        )
    }

    fn test_health_report(
        executor: &Executor,
        pending_binds: &HashMap<RouteChannel, PendingBind>,
        metrics: &DispatchPathMetrics,
        app: &App,
    ) -> HealthReport {
        let cache = HealthRollupCache::new();
        let root_count = executor.actor_entries().len() as u64;
        if root_count == 0 {
            cache.refresh(executor, app);
        } else {
            refresh_until_root_count(&cache, executor, app, root_count);
        }
        // These tests own an executor, not the process-wide indexing work of
        // other tests. Live indexing has its own root-scoped fixture below.
        build_health_report_with_indexing(
            &cache,
            executor,
            pending_binds,
            metrics,
            app,
            |warming_roots| crate::cold_build_limiter::progress::Snapshot {
                running: Vec::new(),
                queued: Vec::new(),
                warming_roots,
                omitted: crate::cold_build_limiter::progress::Omitted {
                    running: 0,
                    queued: 0,
                },
            },
        )
    }

    fn health_report_after_scheduler_contention(
        test_name: &str,
        mut read_report: impl FnMut() -> HealthReport,
    ) -> HealthReport {
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            let report = read_report();
            if report.detail.as_deref() != Some(EXECUTOR_SCHEDULER_CONTENTION_DETAIL) {
                return report;
            }
            assert!(
                Instant::now() < deadline,
                "{test_name}: scheduler remained contended for 30 seconds"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    fn health_report_while_scheduler_locked(
        executor: &Executor,
        build_report: impl FnOnce() -> HealthReport,
    ) -> HealthReport {
        std::thread::scope(|scope| {
            // Both channel ends live inside the scope closure, so a panic here
            // drops the release sender before the scope joins the holder, and
            // the holder's recv returns instead of blocking forever.
            let (held_tx, held_rx) = crossbeam_channel::bounded(1);
            let (release_tx, release_rx) = crossbeam_channel::bounded::<()>(1);
            let holder = scope.spawn(move || {
                executor.hold_state_lock_for_test(|| {
                    held_tx.send(()).expect("signal held scheduler lock");
                    let _ = release_rx.recv();
                });
            });
            held_rx
                .recv_timeout(Duration::from_secs(30))
                .expect("scheduler lock holder starts");
            let report = build_report();
            release_tx.send(()).expect("release scheduler lock holder");
            holder.join().expect("scheduler lock holder joins");
            report
        })
    }

    #[test]
    fn health_reports_live_indexing_and_root_identity_headline() {
        let executor = Executor::new();
        let metrics = DispatchPathMetrics::new();
        let app = crate::context::App::default_shared();
        let foreign_root = tempfile::tempdir().unwrap();
        let _foreign = crate::cold_build_limiter::progress::start(
            foreign_root.path(),
            "unrelated work",
            None,
            crate::cold_build_limiter::progress::StartLog::Quiet,
        );
        let fixture = tempfile::tempdir().unwrap();
        let root = fixture.path().join("openclaw");
        let _fill = crate::cold_build_limiter::progress::start(
            &root,
            "semantic view fill",
            Some(31_705),
            crate::cold_build_limiter::progress::StartLog::Info,
        );
        crate::cold_build_limiter::progress::phase("embedding", Some(31_705));
        crate::cold_build_limiter::progress::advance(1_280);
        crate::cold_build_limiter::progress::embedded(9_000);
        let _queued =
            crate::cold_build_limiter::progress::queued(&root.to_string_lossy(), "semantic build");
        let cache = HealthRollupCache::new();
        cache.refresh(&executor, &app);
        let report = build_health_report_with_indexing(
            &cache,
            &executor,
            &HashMap::new(),
            &metrics,
            &app,
            |warming_roots| {
                crate::cold_build_limiter::progress::snapshot_for_root(&root, warming_roots)
            },
        );
        let value = report.metrics.as_ref().unwrap();
        let indexing = &value["indexing"];
        let running = indexing["running"].as_array().expect("live indexing block");
        let fill = running
            .iter()
            .find(|row| row["root"].as_str() == Some(root.to_string_lossy().as_ref()))
            .unwrap();
        assert_eq!(fill["done"], 1_280);
        assert_eq!(fill["total"], 31_705);
        assert_eq!(fill["chunks_embedded"], 9_000);
        assert_eq!(fill["phase"], "embedding");
        let headline = value["headline"]
            .as_array()
            .expect("CLI headline selection");
        let expected_headline =
            ["actor_count", "warming_roots", "snapshot_age_ms"].map(|key| json!(key));
        assert_eq!(headline, &expected_headline);
        for key in headline {
            let key = key.as_str().expect("headline keys are strings");
            assert!(value.get(key).is_some(), "headline key {key} is absent");
            assert!(value[key].is_number(), "headline key {key} is not numeric");
        }
        for row in running.iter().chain(indexing["queued"].as_array().unwrap()) {
            let identity = ["project_root", "id", "name", "module_id", "path", "root"]
                .into_iter()
                .find(|key| row[*key].is_string());
            assert_eq!(identity, Some("root"));
        }
        eprintln!("SAMPLE detail: {}", report.detail.unwrap());
        eprintln!("SAMPLE indexing: {}", indexing);
    }

    #[test]
    fn indexing_detail_renders_owned_snapshot_with_known_and_unknown_eta() {
        use crate::cold_build_limiter::progress::{Omitted, Queued, Running, Snapshot};
        let mut snapshot = Snapshot {
            running: vec![Running {
                root: "/fake/openclaw".to_owned(),
                root_label: "openclaw".to_owned(),
                kind: "semantic view fill".to_owned(),
                phase: "embedding",
                started_at_ms: 1,
                done: 1_280,
                total: Some(31_705),
                chunks_embedded: Some(9_000),
                rate_per_minute: Some(28.17),
                eta_seconds: Some(64_800.0),
                age_seconds: 60.0,
            }],
            queued: vec![Queued {
                root: "/fake/hermes-agent".to_owned(),
                kind: "semantic build".to_owned(),
                waited_seconds: 5.0,
            }],
            warming_roots: 49,
            omitted: Omitted {
                running: 0,
                queued: 2,
            },
        };
        assert_eq!(indexing_detail(&snapshot).as_deref(), Some(
            "indexing: semantic view fill openclaw 1,280/31,705 files (~18 h, ~28 files/min) [embedding], 3 queued; 49 roots warming"));
        snapshot.running[0].total = None;
        snapshot.running[0].eta_seconds = None;
        snapshot.running[0].rate_per_minute = None;
        assert_eq!(indexing_detail(&snapshot).as_deref(), Some(
            "indexing: semantic view fill openclaw 1,280 files (total measuring) [embedding; measuring rate], 3 queued; 49 roots warming"));
    }

    #[test]
    fn indexing_detail_shows_measured_rate_without_eta_when_total_is_unknown() {
        use crate::cold_build_limiter::progress::{Omitted, Running, Snapshot};
        let snapshot = Snapshot {
            running: vec![Running {
                root: "/fake/openclaw".to_owned(),
                root_label: "openclaw".to_owned(),
                kind: "semantic view fill".to_owned(),
                phase: "embedding",
                started_at_ms: 1,
                done: 1_280,
                total: None,
                chunks_embedded: None,
                rate_per_minute: Some(28.17),
                eta_seconds: None,
                age_seconds: 60.0,
            }],
            queued: Vec::new(),
            warming_roots: 0,
            omitted: Omitted {
                running: 0,
                queued: 0,
            },
        };
        let detail = indexing_detail(&snapshot).unwrap();
        assert!(detail.contains("total measuring"), "{detail}");
        assert!(detail.contains("~28 files/min"), "{detail}");
        assert_eq!(detail.matches('~').count(), 1, "{detail}");
        assert!(!detail.contains("?"), "{detail}");

        let mut varied = snapshot;
        varied.running[0].rate_per_minute = Some(0.2);
        let detail = indexing_detail(&varied).unwrap();
        assert!(detail.contains("~12 files/h"), "{detail}");

        varied.running[0].rate_per_minute = Some(720.0);
        let detail = indexing_detail(&varied).unwrap();
        assert!(detail.contains("~12 files/s"), "{detail}");
    }

    #[test]
    fn indexing_detail_does_not_estimate_from_an_unmeasured_rate() {
        use crate::cold_build_limiter::progress::{Omitted, Running, Snapshot};
        let snapshot = Snapshot {
            running: vec![Running {
                root: "/fake/openclaw".to_owned(),
                root_label: "openclaw".to_owned(),
                kind: "semantic view fill".to_owned(),
                phase: "embedding",
                started_at_ms: 1,
                done: 0,
                total: Some(100),
                chunks_embedded: None,
                rate_per_minute: None,
                eta_seconds: Some(100.0),
                age_seconds: 0.5,
            }],
            queued: Vec::new(),
            warming_roots: 0,
            omitted: Omitted {
                running: 0,
                queued: 0,
            },
        };
        let detail = indexing_detail(&snapshot).unwrap();
        assert!(detail.contains("measuring rate"), "{detail}");
        assert!(!detail.contains("files/s"), "{detail}");
        assert!(!detail.contains("~"), "{detail}");
    }

    #[test]
    fn indexing_detail_uses_preparing_for_unknown_totals_at_start() {
        use crate::cold_build_limiter::progress::{Omitted, Running, Snapshot};
        let snapshot = Snapshot {
            running: vec![Running {
                root: "/fake/openclaw".to_owned(),
                root_label: "openclaw".to_owned(),
                kind: "view publication".to_owned(),
                phase: "starting",
                started_at_ms: 1,
                done: 0,
                total: None,
                chunks_embedded: None,
                rate_per_minute: None,
                eta_seconds: None,
                age_seconds: 0.0,
            }],
            queued: Vec::new(),
            warming_roots: 0,
            omitted: Omitted {
                running: 0,
                queued: 0,
            },
        };

        let detail = indexing_detail(&snapshot).unwrap();
        assert!(detail.contains("preparing [starting]"), "{detail}");
        assert!(!detail.contains("0/?"), "{detail}");
        assert!(!detail.contains("none"), "{detail}");
        assert!(!detail.contains("files/s"), "{detail}");
        assert!(!detail.contains("~"), "{detail}");
    }

    #[test]
    fn indexing_detail_never_emits_an_empty_header_for_queued_or_omitted_work() {
        use crate::cold_build_limiter::progress::{Omitted, Queued, Snapshot};
        let queued = Snapshot {
            running: Vec::new(),
            queued: vec![Queued {
                root: "/fake/openclaw".to_owned(),
                kind: "semantic build".to_owned(),
                waited_seconds: 1.0,
            }],
            warming_roots: 2,
            omitted: Omitted {
                running: 0,
                queued: 0,
            },
        };
        assert_eq!(
            indexing_detail(&queued).as_deref(),
            Some("indexing: 1 queued; 2 roots warming")
        );

        let omitted = Snapshot {
            running: Vec::new(),
            queued: Vec::new(),
            warming_roots: 0,
            omitted: Omitted {
                running: 2,
                queued: 0,
            },
        };
        assert_eq!(
            indexing_detail(&omitted).as_deref(),
            Some("indexing: 2 running; details omitted")
        );
        assert!(indexing_detail(&Snapshot {
            running: Vec::new(),
            queued: Vec::new(),
            warming_roots: 0,
            omitted: Omitted {
                running: 0,
                queued: 0,
            },
        })
        .is_none());
    }

    #[test]
    fn health_headline_omits_indexing_when_there_are_no_work_rows() {
        use crate::cold_build_limiter::progress::{Omitted, Snapshot};
        let executor = Executor::new();
        let dispatch_metrics = DispatchPathMetrics::new();
        let app = crate::context::App::default_shared();
        let cache = HealthRollupCache::new();
        cache.refresh(&executor, &app);
        let report = build_health_report_with_indexing(
            &cache,
            &executor,
            &HashMap::new(),
            &dispatch_metrics,
            &app,
            |_| Snapshot {
                running: Vec::new(),
                queued: Vec::new(),
                warming_roots: 0,
                omitted: Omitted {
                    running: 0,
                    queued: 0,
                },
            },
        );
        let metrics = report.metrics.unwrap();
        assert!(metrics["indexing"].is_object());
        let headline = metrics["headline"]
            .as_array()
            .expect("CLI headline selection");
        let expected_headline =
            ["actor_count", "warming_roots", "snapshot_age_ms"].map(|key| json!(key));
        assert_eq!(headline, &expected_headline);
        for key in headline {
            let key = key.as_str().expect("headline keys are strings");
            assert!(metrics.get(key).is_some(), "headline key {key} is absent");
            assert!(
                metrics[key].is_number(),
                "headline key {key} is not numeric"
            );
        }
    }

    #[test]
    fn frame_loop_owes_a_turn_only_after_its_promised_wake_passes() {
        let metrics = DispatchPathMetrics::new();
        assert!(
            !metrics.frame_loop_has_pending_work(),
            "a loop that never parked promised nothing"
        );

        metrics.mark_frame_loop_tick();
        metrics.publish_frame_loop_wake_deadline(Duration::from_secs(60));
        assert!(!metrics.frame_loop_has_pending_work());

        metrics.publish_frame_loop_wake_deadline(Duration::ZERO);
        std::thread::sleep(Duration::from_millis(20));
        assert!(metrics.frame_loop_has_pending_work());
        assert!(metrics.frame_loop_progress_age() >= Duration::from_millis(20));

        metrics.clear_frame_loop_wake_deadline();
        assert!(!metrics.frame_loop_has_pending_work());
    }

    #[test]
    fn health_reports_slow_bind_acks_in_the_window_and_the_worst_latency() {
        let metrics = DispatchPathMetrics::new();
        let executor = Executor::new();
        let app = crate::context::App::default_shared();
        let quiet = test_health_report(&executor, &HashMap::new(), &metrics, &app);
        let quiet_acks = &quiet.metrics.as_ref().unwrap()["dispatch_path"]["bind_acks"];
        assert_eq!(quiet_acks["count"], 0);
        assert_eq!(quiet_acks["slow_count"], 0);
        assert!(quiet_acks["worst_ms"].is_null());

        for latency_ms in [40, 6_100, 26_500, 900, 17_800] {
            metrics.record_bind_ack(Duration::from_millis(latency_ms));
        }
        let report = test_health_report(&executor, &HashMap::new(), &metrics, &app);
        let acks = &report.metrics.as_ref().unwrap()["dispatch_path"]["bind_acks"];
        assert_eq!(acks["count"], 5);
        assert_eq!(acks["slow_count"], 3, "acks over 5 s: {acks}");
        assert_eq!(acks["slow_threshold_ms"], 5_000);
        assert_eq!(acks["worst_ms"], 26_500);
        assert_eq!(acks["worst_since_start_ms"], 26_500);
        // Buckets: 40 -> 50, 900 -> 1000, 6100 -> 8000, 17800 -> 20000,
        // 26500 -> 30000; nearest rank of p50 is the third sample.
        assert_eq!(acks["percentiles"]["p50_ms"], 8_000, "{acks}");
        assert_eq!(acks["percentiles"]["p90_ms"], 30_000, "{acks}");
        assert_eq!(acks["percentiles"]["p99_ms"], 30_000, "{acks}");
        assert_eq!(acks["percentiles_since_start"], acks["percentiles"]);
        assert!(quiet_acks["percentiles"]["p50_ms"].is_null());
    }

    #[test]
    fn bind_ack_window_forgets_old_acks_but_keeps_the_all_time_worst() {
        let mut acks = BindAckLatencies::default();
        let start = Instant::now();
        acks.record(start, Duration::from_secs(9));
        acks.record(start + Duration::from_secs(1), Duration::from_millis(30));
        let later = start + BIND_ACK_WINDOW + Duration::from_secs(1);
        acks.record(later, Duration::from_millis(50));
        let snapshot = acks.snapshot(later);
        assert_eq!(snapshot["count"], 2);
        assert_eq!(snapshot["slow_count"], 0);
        assert_eq!(snapshot["worst_ms"], 50);
        assert_eq!(snapshot["worst_since_start_ms"], 9_000);
        assert_eq!(snapshot["slow_total"], 1);
        assert_eq!(snapshot["total"], 3);
        // The window forgot the 9 s ack; the since-start histogram did not.
        assert_eq!(snapshot["percentiles"]["p99_ms"], 50);
        assert_eq!(snapshot["percentiles_since_start"]["p50_ms"], 50);
        assert_eq!(snapshot["percentiles_since_start"]["p99_ms"], 10_000);
    }

    #[test]
    fn bind_ack_percentiles_report_the_worst_for_the_overflow_bucket() {
        let mut acks = BindAckLatencies::default();
        let now = Instant::now();
        for _ in 0..98 {
            acks.record(now, Duration::from_millis(3));
        }
        acks.record(now, Duration::from_millis(700));
        acks.record(now, Duration::from_secs(90));
        let snapshot = acks.snapshot(now);
        assert_eq!(snapshot["percentiles"]["p50_ms"], 10);
        assert_eq!(snapshot["percentiles"]["p90_ms"], 10);
        assert_eq!(snapshot["percentiles"]["p99_ms"], 1_000);
        acks.record(now, Duration::from_secs(90));
        let snapshot = acks.snapshot(now);
        assert_eq!(snapshot["percentiles"]["p99_ms"], 90_000);
    }

    fn view_health_fixture() -> (
        tempfile::TempDir,
        Executor,
        Arc<AppContext>,
        std::path::PathBuf,
    ) {
        let (dir, root) = super::super::test_support::test_root("view-health-next-tick");
        let view_dir = dir.path().join("view");
        let mut status = crate::path_status::PathStatusStore::open(&view_dir).unwrap();
        status.mark_pending(b"a.rs", "fixture", 1).unwrap();
        drop(status);
        let ctx = Arc::new(AppContext::new(
            Box::new(crate::parser::TreeSitterProvider::new()),
            crate::config::Config {
                project_root: Some(root.as_path().into()),
                storage_dir: Some(dir.path().join("storage")),
                ..Default::default()
            },
        ));
        ctx.update_config(|config| config.views.enabled = true);
        ctx.install_view_runtime(
            crate::context::ViewRuntimeSnapshot {
                query_pin: None,
                storage: dir.path().join("storage"),
                family: "health-fixture".into(),
                scope: "health-checkout".into(),
                view_dir: view_dir.clone(),
                generation: Some("1-fixture".into()),
                manifest: None,
                head_fingerprint: String::new(),
                head_metadata: crate::alias::GitHeadMetadata {
                    head_path: Default::default(),
                    head_mtime: None,
                    resolved_ref_path: None,
                    resolved_ref_mtime: None,
                },
                pending_paths: Default::default(),
                pending_inputs: Default::default(),
            },
            None,
        );
        let executor = Executor::new();
        assert!(executor.register_actor(root, Arc::clone(&ctx)));
        (dir, executor, ctx, view_dir)
    }

    fn rolled_up_view(cache: &HealthRollupCache, executor: &Executor, app: &App) -> Value {
        cache.refresh_for_test(executor, app);
        cache.snapshot().0.metrics["roots"][0]["views"].clone()
    }

    #[test]
    fn view_health_local_batches_are_in_next_rollup_with_one_refresh() {
        let (_dir, executor, _ctx, view_dir) = view_health_fixture();
        let app = App::default_shared();
        let cache = HealthRollupCache::new();
        assert_eq!(rolled_up_view(&cache, &executor, &app)["pending_paths"], 1);
        let before = crate::path_status::HEALTH_READS.with(std::cell::Cell::get);
        let scans = crate::path_status::HEALTH_SUMMARY_READS.with(std::cell::Cell::get);
        {
            let mut status = crate::path_status::PathStatusStore::open(&view_dir).unwrap();
            for _ in 0..20 {
                status.mark_pending(b"a.rs", "pending", 1).unwrap();
            }
            status.mark_failed(b"a.rs", "missing payload", 1).unwrap();
        }
        assert_eq!(
            crate::path_status::HEALTH_SUMMARY_READS.with(std::cell::Cell::get),
            scans,
            "local batches must not scan health on the writer"
        );
        let next = rolled_up_view(&cache, &executor, &app);
        assert_eq!(
            next["failed_paths"], 1,
            "committed failure must show next tick"
        );
        assert_eq!(next["pending_paths"], 0);
        assert_eq!(
            crate::path_status::HEALTH_READS.with(std::cell::Cell::get),
            before + 1,
            "all local batches must share one refresh on the next tick"
        );
        assert_eq!(rolled_up_view(&cache, &executor, &app)["failed_paths"], 1);
        assert_eq!(
            crate::path_status::HEALTH_READS.with(std::cell::Cell::get),
            before + 1,
            "an unchanged tick must reuse the projection"
        );
    }

    #[test]
    fn view_health_external_failure_is_in_next_rollup() {
        const CHILD_VIEW: &str = "AFT_HEALTH_EXTERNAL_WRITE_VIEW";
        if let Some(view_dir) = std::env::var_os(CHILD_VIEW) {
            let mut status =
                crate::path_status::PathStatusStore::open(Path::new(&view_dir)).unwrap();
            status
                .mark_failed(b"a.rs", "external missing payload", 1)
                .unwrap();
            return;
        }
        let (_dir, executor, _ctx, view_dir) = view_health_fixture();
        let app = App::default_shared();
        let cache = HealthRollupCache::new();
        assert_eq!(rolled_up_view(&cache, &executor, &app)["pending_paths"], 1);
        // First make a local write and refresh health. The later child-process
        // signature change must still trigger a fresh SQLite read.
        {
            let mut status = crate::path_status::PathStatusStore::open(&view_dir).unwrap();
            status.mark_pending(b"b.rs", "local", 1).unwrap();
        }
        assert_eq!(rolled_up_view(&cache, &executor, &app)["pending_paths"], 2);
        let before = crate::path_status::HEALTH_READS.with(std::cell::Cell::get);
        let child = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "subc::health::tests::view_health_external_failure_is_in_next_rollup",
                "--nocapture",
            ])
            .env(CHILD_VIEW, &view_dir)
            .output()
            .unwrap();
        assert!(
            child.status.success(),
            "child: {}",
            String::from_utf8_lossy(&child.stderr)
        );
        let next = rolled_up_view(&cache, &executor, &app);
        assert_eq!(
            next["failed_paths"], 1,
            "external commit must show next tick"
        );
        assert_eq!(next["pending_paths"], 1);
        assert_eq!(
            crate::path_status::HEALTH_READS.with(std::cell::Cell::get),
            before + 1,
            "external signature changes must refresh the summary"
        );
    }

    #[test]
    fn view_health_runtime_replacement_disable_and_clear_are_in_next_rollup() {
        let (_dir, executor, ctx, _view_dir) = view_health_fixture();
        let app = App::default_shared();
        let cache = HealthRollupCache::new();
        assert_eq!(rolled_up_view(&cache, &executor, &app)["generation"], 1);
        let mut next = ctx.view_runtime_snapshot().unwrap();
        next.generation = Some("2-fixture".into());
        ctx.install_view_runtime(next, None);
        assert_eq!(rolled_up_view(&cache, &executor, &app)["generation"], 2);
        ctx.update_config(|config| config.views.enabled = false);
        assert!(rolled_up_view(&cache, &executor, &app).is_null());
        ctx.update_config(|config| config.views.enabled = true);
        assert_eq!(rolled_up_view(&cache, &executor, &app)["generation"], 2);
        ctx.clear_view_runtime();
        assert!(rolled_up_view(&cache, &executor, &app).is_null());
    }

    #[test]
    fn view_health_runtime_drop_closes_reader_even_with_live_writer() {
        let (_dir, executor, ctx, view_dir) = view_health_fixture();
        let app = App::default_shared();
        let cache = HealthRollupCache::new();
        assert_eq!(rolled_up_view(&cache, &executor, &app)["pending_paths"], 1);
        let writer = crate::path_status::PathStatusStore::open(&view_dir).unwrap();
        let path = writer.path().to_path_buf();
        assert_eq!(crate::db::file_identity::open_connections(&path), 2);
        ctx.clear_view_runtime();
        assert_eq!(
            crate::db::file_identity::open_connections(&path),
            1,
            "a writer must not retain the retired runtime's health connection"
        );
        drop(writer);
        assert_eq!(crate::db::file_identity::open_connections(&path), 0);
    }

    #[test]
    fn view_health_readiness_replacement_is_in_next_rollup_without_ledger_change() {
        let (_dir, executor, ctx, _view_dir) = view_health_fixture();
        let app = App::default_shared();
        let cache = HealthRollupCache::new();
        let initial = rolled_up_view(&cache, &executor, &app);
        assert!(initial["callgraph_readiness_deferred"].is_null());
        let reads = crate::path_status::HEALTH_READS.with(std::cell::Cell::get);
        for reason in [
            crate::views::read::CALLGRAPH_READINESS_BUSY,
            crate::views::read::CALLGRAPH_READINESS_PROBE_FAILED,
        ] {
            let mut next = ctx.view_runtime_snapshot().unwrap();
            next.pending_inputs.insert(
                b"a.rs".to_vec(),
                crate::views::assembly::PendingInput {
                    plane: "callgraph",
                    reason,
                    blob_key: None,
                },
            );
            ctx.install_view_runtime(next, None);
            let view = rolled_up_view(&cache, &executor, &app);
            assert_eq!(view["generation"], 1);
            assert_eq!(
                view["callgraph_readiness_deferred"], reason,
                "runtime readiness changes must show on the next tick"
            );
        }
        let mut next = ctx.view_runtime_snapshot().unwrap();
        next.pending_inputs.clear();
        ctx.install_view_runtime(next, None);
        assert!(rolled_up_view(&cache, &executor, &app)["callgraph_readiness_deferred"].is_null());
        assert_eq!(crate::path_status::HEALTH_READS.with(std::cell::Cell::get), reads,
            "readiness replacement must invalidate the projection without relying on a ledger change");
    }

    fn refresh_until_root_count(
        cache: &HealthRollupCache,
        executor: &Executor,
        app: &App,
        expected: u64,
    ) -> HealthReport {
        let metrics = DispatchPathMetrics::new();
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            cache.refresh(executor, app);
            let report = build_health_report(cache, executor, &HashMap::new(), &metrics, app);
            // Both the rollup and the live probe can skip a contended scheduler.
            // Wait for the roots, not the diagnostic values being asserted, so
            // a missing diagnostic still fails after a completed refresh.
            let scheduler_busy = report
                .detail
                .as_deref()
                .is_some_and(|detail| detail.contains(EXECUTOR_SCHEDULER_CONTENTION_DETAIL));
            if report
                .metrics
                .as_ref()
                .and_then(|metrics| metrics["root_count"].as_u64())
                == Some(expected)
                && !scheduler_busy
            {
                return report;
            }
            assert!(
                Instant::now() < deadline,
                "health cache did not capture {expected} roots without contention: {report:?}"
            );
            std::thread::yield_now();
        }
    }

    fn install_resident_stale_callgraph(
        ctx: &Arc<AppContext>,
        root: &ProjectRootId,
        file_name: &str,
        remove_file: bool,
    ) {
        let project_root = root.as_path().to_path_buf();
        let file = project_root.join(file_name);
        std::fs::create_dir_all(file.parent().expect("fixture file parent")).unwrap();
        std::fs::write(&file, "export function staleFixture() {}\n").unwrap();
        let store_dir = project_root.join(".callgraph-health-test");
        let writer =
            crate::callgraph_store::CallGraphStore::open(store_dir.clone(), project_root.clone())
                .unwrap();
        writer.cold_build(std::slice::from_ref(&file)).unwrap();
        writer
            .mark_files_stale(std::slice::from_ref(&file))
            .unwrap();
        if remove_file {
            std::fs::remove_file(&file).unwrap();
        }
        drop(writer);
        let reader = crate::callgraph_store::CallGraphStore::open_readonly(store_dir, project_root)
            .unwrap()
            .expect("ready resident callgraph store");
        *ctx.callgraph_store()
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(Arc::new(reader));
    }

    #[test]
    fn cold_build_holder_census_renders_one_row_per_root_with_sharers() {
        let limiter = crate::cold_build_limiter::isolated_limiter(2);
        let first = crate::cold_build_limiter::try_acquire_classified_with_limiter(
            &limiter,
            &crate::cold_build_limiter::ColdBuildAdmissionRequest::for_root(
                "/project/shared",
                "health-first",
                crate::cold_build_limiter::ColdBuildAdmissionClass::Maintenance,
            ),
        )
        .expect("first holder");
        let second = crate::cold_build_limiter::try_acquire_classified_with_limiter(
            &limiter,
            &crate::cold_build_limiter::ColdBuildAdmissionRequest::for_root(
                "/project/shared",
                "health-second",
                crate::cold_build_limiter::ColdBuildAdmissionClass::InspectTriggered,
            ),
        )
        .expect("shared holder");

        let rendered = render_cold_build_limiter_census(limiter.census());
        let holders = rendered["holders"].as_array().expect("holder rows");
        assert_eq!(holders.len(), 1);
        assert_eq!(holders[0]["root"], "/project/shared");
        assert_eq!(holders[0]["sharers"], 2);

        drop((first, second));
    }

    #[test]
    fn health_reports_resident_stale_backend_rows_with_absent_paths_distinguished() {
        let executor = Executor::with_config(crate::executor::ExecutorConfig {
            pool_size: 1,
            read_cap: 1,
            actor_cap: 2,
            heavy_permits: 1,
            drr_quantum: 1,
        });
        let (_absent_dir, absent_root) = test_root("health-stale-absent");
        let absent_ctx = test_ctx();
        install_resident_stale_callgraph(&absent_ctx, &absent_root, "absent.ts", true);
        assert!(executor.register_actor(absent_root.clone(), absent_ctx.clone()));
        let (_existing_dir, existing_root) = test_root("health-stale-existing");
        let existing_ctx = test_ctx();
        install_resident_stale_callgraph(&existing_ctx, &existing_root, "existing.ts", false);
        assert!(executor.register_actor(existing_root.clone(), existing_ctx.clone()));

        // Make response-budget pressure deterministic instead of depending on
        // process-wide telemetry contributed by concurrently running tests.
        for ctx in [&absent_ctx, &existing_ctx] {
            *ctx.semantic_index_status().write().unwrap() =
                crate::context::SemanticIndexStatus::Building {
                    stage: "x".repeat(HEALTH_METRICS_BUDGET_BYTES),
                    files: None,
                    entries_done: None,
                    entries_total: None,
                };
        }

        // Assert the rollup's per-root census before building the byte-budgeted
        // report; the oversized stages above intentionally make it omit details.
        let app = App::default_shared();
        let cache = HealthRollupCache::new();
        cache.refresh_for_test(&executor, &app);
        let (rollup, _) = cache.snapshot();
        let metrics = &rollup.metrics;
        let roots = metrics["roots"].as_array().expect("per-root health rows");
        let row_for = |root: &ProjectRootId| {
            roots
                .iter()
                .find(|row| row["project_root"] == root.as_path().display().to_string())
                .unwrap_or_else(|| panic!("root health row: {metrics}"))
        };
        let absent = row_for(&absent_root);
        assert_eq!(absent["resident_callgraph_stale_backend_rows"], 1);
        assert_eq!(absent["resident_callgraph_absent_stale_backend_rows"], 1);
        assert_eq!(
            absent["resident_callgraph_unreadable_stale_backend_rows"],
            0
        );
        let existing = row_for(&existing_root);
        assert_eq!(existing["resident_callgraph_stale_backend_rows"], 1);
        assert_eq!(existing["resident_callgraph_absent_stale_backend_rows"], 0);
        assert_eq!(
            existing["resident_callgraph_unreadable_stale_backend_rows"],
            0
        );

        let report = build_health_report(
            &cache,
            &executor,
            &HashMap::new(),
            &DispatchPathMetrics::new(),
            &app,
        );
        let bounded = report.metrics.expect("bounded health metrics");
        assert_eq!(bounded["root_count"], 2);
        assert_eq!(bounded["root_details_omitted"], 2);
        assert!(bounded["roots"].as_array().unwrap().is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn health_counts_unreadable_resident_stale_backend_paths_separately() {
        use std::os::unix::fs::PermissionsExt;

        let executor = Executor::with_config(crate::executor::ExecutorConfig {
            pool_size: 1,
            read_cap: 1,
            actor_cap: 1,
            heavy_permits: 1,
            drr_quantum: 1,
        });
        let (_dir, root) = test_root("health-stale-unreadable");
        let ctx = test_ctx();
        install_resident_stale_callgraph(&ctx, &root, "guarded/unreadable.ts", false);
        assert!(executor.register_actor(root.clone(), ctx));
        let guarded = root.as_path().join("guarded");
        let original_mode = std::fs::metadata(&guarded).unwrap().permissions().mode();
        std::fs::set_permissions(&guarded, std::fs::Permissions::from_mode(0)).unwrap();

        let report = test_health_report(
            &executor,
            &HashMap::new(),
            &DispatchPathMetrics::new(),
            &App::default_shared(),
        );

        std::fs::set_permissions(&guarded, std::fs::Permissions::from_mode(original_mode)).unwrap();
        let metrics = report.metrics.expect("health metrics");
        assert_eq!(
            metrics["roots"][0]["resident_callgraph_stale_backend_rows"],
            1
        );
        assert_eq!(
            metrics["roots"][0]["resident_callgraph_absent_stale_backend_rows"],
            0
        );
        assert_eq!(
            metrics["roots"][0]["resident_callgraph_unreadable_stale_backend_rows"],
            1
        );
    }

    fn cached_reply_median(
        cache: &HealthRollupCache,
        executor: &Executor,
        metrics: &DispatchPathMetrics,
        app: &App,
    ) -> Duration {
        let mut samples = Vec::with_capacity(31);
        for _ in 0..31 {
            let started = Instant::now();
            let report = build_health_report(cache, executor, &HashMap::new(), metrics, app);
            let response = subc_protocol::session::ModuleControlResponse::from(report);
            std::hint::black_box(
                serde_json::to_vec(&response).expect("serialize health.check response"),
            );
            samples.push(started.elapsed());
        }
        samples.sort_unstable();
        samples[samples.len() / 2]
    }

    #[test]
    fn health_retention_count_reuses_sql_for_30s_with_honest_age() {
        let storage = tempfile::tempdir().unwrap();
        let db = Arc::new(std::sync::Mutex::new(
            crate::db::open(&storage.path().join("aft.db")).unwrap(),
        ));
        db.lock().unwrap().execute_batch(
            "INSERT INTO bash_tasks
             (harness, session_id, task_id, project_key, command, cwd, status, started_at, completed_at, completion_delivered)
             VALUES ('opencode', 'session', 'old-task', 'project', 'true', '.', 'completed', 0, 1, 1);"
        ).unwrap();
        let app = App::default_shared();
        app.set_db(Arc::clone(&db));
        let cache = HealthRollupCache::new();
        cache.clock_ms.store(0, Ordering::Relaxed);
        let executor = Executor::new();
        cache.refresh_for_test(&executor, &app);
        let first = cache.snapshot().0.metrics["bash_task_retention"].clone();
        assert_eq!(first["eligible_but_unpruned_rows"], 1);
        assert_eq!(cache.retention_count_reads.load(Ordering::Relaxed), 1);

        db.lock()
            .unwrap()
            .execute("DELETE FROM bash_tasks", [])
            .unwrap();
        cache.clock_ms.store(29_999, Ordering::Relaxed);
        let report = build_health_report(
            &cache,
            &executor,
            &HashMap::new(),
            &DispatchPathMetrics::new(),
            &app,
        );
        assert_eq!(
            report.metrics.unwrap()["bash_task_retention"]
                ["eligible_but_unpruned_rows_measured_age_ms"],
            29_999
        );
        let reused = bash_task_retention_metrics(&cache, &app);
        assert_eq!(
            cache.retention_count_reads.load(Ordering::Relaxed),
            1,
            "unchanged health ticks must not recount retention rows"
        );
        assert_eq!(reused["eligible_but_unpruned_rows"], 1);
        assert_eq!(first["eligible_but_unpruned_rows_measured_age_ms"], 0);
        assert_eq!(reused["eligible_but_unpruned_rows_measured_age_ms"], 29_999);

        cache.clock_ms.store(30_000, Ordering::Relaxed);
        let refreshed = bash_task_retention_metrics(&cache, &app);
        assert_eq!(cache.retention_count_reads.load(Ordering::Relaxed), 2);
        assert_eq!(refreshed["eligible_but_unpruned_rows"], 0);
        assert_eq!(refreshed["eligible_but_unpruned_rows_measured_age_ms"], 0);
    }

    #[test]
    fn health_retention_count_invalidates_on_database_change_and_retries_unknown() {
        let storage = tempfile::tempdir().unwrap();
        let db = Arc::new(std::sync::Mutex::new(
            crate::db::open(&storage.path().join("aft.db")).unwrap(),
        ));
        let cache = HealthRollupCache::new();
        cache.clock_ms.store(0, Ordering::Relaxed);
        assert_eq!(
            cache.retention_count(Some(&db), i64::MAX),
            (Some(0), Some(0))
        );
        let other = Arc::new(std::sync::Mutex::new(
            crate::db::open(&storage.path().join("other.db")).unwrap(),
        ));
        assert_eq!(
            cache.retention_count(Some(&other), i64::MAX),
            (Some(0), Some(0))
        );
        assert_eq!(cache.retention_count_reads.load(Ordering::Relaxed), 2);
        assert_eq!(cache.retention_count(None, i64::MAX), (None, None));
        assert_eq!(
            cache.retention_count(Some(&other), i64::MAX),
            (Some(0), Some(0))
        );
        assert_eq!(cache.retention_count_reads.load(Ordering::Relaxed), 3);

        cache.clock_ms.store(30_000, Ordering::Relaxed);
        let held = other.lock().unwrap();
        assert_eq!(cache.retention_count(Some(&other), i64::MAX), (None, None));
        drop(held);
        assert_eq!(
            cache.retention_count(Some(&other), i64::MAX),
            (Some(0), Some(0))
        );
        assert_eq!(cache.retention_count_reads.load(Ordering::Relaxed), 5);
        other
            .lock()
            .unwrap()
            .execute_batch("DROP TABLE bash_tasks")
            .unwrap();
        cache.clock_ms.store(60_000, Ordering::Relaxed);
        assert_eq!(cache.retention_count(Some(&other), i64::MAX), (None, None));
        assert_eq!(cache.retention_count(Some(&other), i64::MAX), (None, None));
        assert_eq!(cache.retention_count_reads.load(Ordering::Relaxed), 7);
    }

    #[test]
    fn health_check_reports_eligible_but_unpruned_bash_task_rows() {
        let storage = tempfile::tempdir().expect("storage tempdir");
        let conn = crate::db::open(&storage.path().join("aft.db")).expect("open database");
        let now_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis();
        let old_completed_at = i64::try_from(now_ms)
            .unwrap_or(i64::MAX)
            .saturating_sub(crate::db::bash_tasks::TERMINAL_ROW_RETENTION_AGE_MS)
            .saturating_sub(1);
        for task_id in ["bash-0000000000000001", "bash-0000000000000002"] {
            conn.execute(
                "INSERT INTO bash_tasks (
                    harness, session_id, task_id, project_key, command, cwd, status,
                    started_at, completed_at, completion_delivered
                 ) VALUES ('opencode', 'health-session', ?1, 'project', 'true', '.',
                           'completed', 1, ?2, 1)",
                rusqlite::params![task_id, old_completed_at],
            )
            .expect("insert eligible bash task");
        }
        let app = crate::context::App::default_shared();
        app.set_db(Arc::new(std::sync::Mutex::new(conn)));

        let report = test_health_report(
            &Executor::new(),
            &HashMap::new(),
            &DispatchPathMetrics::new(),
            &app,
        );
        let retention = &report.metrics.expect("health metrics")["bash_task_retention"];

        assert_eq!(retention["eligible_but_unpruned_rows"].as_u64(), Some(2));
        assert!(retention["eligible_but_unpruned_rows_measured_age_ms"].is_u64());
        assert!(retention["last_sweep_skip_reason"].is_null());
        assert_eq!(
            retention["steady_state_ceiling"].as_u64(),
            Some(crate::db::compression_events::BASH_TASK_STEADY_STATE_ROWS as u64)
        );
        // Totals for the sweep over task folders, fixed in size however many
        // storage roots the process has swept.
        assert!(
            retention["disk_sweeps"]["roots_out_of_budget"].is_u64(),
            "{retention}"
        );
        assert!(retention["disk_sweeps"]["roots_failing"].is_u64());
    }

    #[test]
    fn health_check_reports_retention_sweep_lock_budget_skip() {
        let storage = tempfile::tempdir().expect("storage tempdir");
        let conn = crate::db::open(&storage.path().join("aft.db")).expect("open database");
        let db = Arc::new(std::sync::Mutex::new(conn));
        let held = db.lock().expect("hold database mutex");
        let outcome =
            crate::db::compression_events::prune_retention_sweep(&db, i64::MAX, Some(&[]))
                .expect("retention sweep outcome");
        drop(held);
        let crate::db::compression_events::RetentionSweepOutcome::Skipped(skip) = outcome else {
            panic!("contention was not reported as a skipped sweep: {outcome:?}");
        };
        assert_eq!(
            skip.reason,
            crate::db::compression_events::RetentionSweepSkipReason::OpeningCountLockBudgetExhausted
        );

        let app = crate::context::App::default_shared();
        app.set_db(db);
        let report = test_health_report(
            &Executor::new(),
            &HashMap::new(),
            &DispatchPathMetrics::new(),
            &app,
        );
        let retention = &report.metrics.expect("health metrics")["bash_task_retention"];

        assert_eq!(
            retention["last_sweep_skip_reason"].as_str(),
            Some("opening_count_lock_budget_exhausted")
        );
    }

    #[test]
    fn health_reports_a_running_mutation_with_its_root_and_current_age() {
        let (_dir, root) = test_root("health-running-mutation");
        let executor = Executor::new();
        assert!(executor.register_actor(
            root.clone(),
            Arc::new(AppContext::new(
                Box::new(crate::parser::TreeSitterProvider::new()),
                crate::config::Config::default(),
            )),
        ));
        let (started_tx, started_rx) = crossbeam_channel::bounded::<()>(1);
        let (release_tx, release_rx) = crossbeam_channel::bounded::<()>(1);
        let mutation = executor.submit_async(
            root.clone(),
            crate::executor::Lane::Mutating,
            "tool-delete-held".to_string(),
            Box::new(move |_| {
                started_tx.send(()).expect("signal start");
                let _ = release_rx.recv_timeout(Duration::from_secs(30));
                crate::protocol::Response::success("tool-delete-held", json!({}))
            }),
        );
        started_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("mutation starts");
        std::thread::sleep(Duration::from_millis(50));

        // A rollup cache that has not been refreshed since the mutation began:
        // what holds a root must not wait for the next background rollup.
        let cache = HealthRollupCache::new();
        let app = crate::context::App::default_shared();
        let report = build_health_report(
            &cache,
            &executor,
            &HashMap::new(),
            &DispatchPathMetrics::new(),
            &app,
        );
        let _ = release_tx.send(());
        let _ = mutation.blocking_recv();

        let metrics = report.metrics.expect("health metrics");
        let lanes = metrics["dispatch_path"]["mutating_lanes"]
            .as_array()
            .unwrap_or_else(|| panic!("mutating lanes: {}", metrics["dispatch_path"]));
        let lane = lanes
            .iter()
            .find(|lane| lane["request_id"] == "tool-delete-held")
            .unwrap_or_else(|| panic!("running mutation missing: {lanes:?}"));
        assert_eq!(
            lane["root"].as_str(),
            Some(root.as_path().to_string_lossy().as_ref())
        );
        assert!(
            lane["started_age_ms"].as_u64().is_some_and(|age| age >= 50),
            "{lane}"
        );
    }

    #[test]
    fn health_worker_publishes_a_fresh_snapshot_without_loop_refresh() {
        let cache = Arc::new(HealthRollupCache::new());
        let executor = Arc::new(Executor::new());
        let app = crate::context::App::default_shared();
        let worker = HealthRollupWorker::start(Arc::clone(&cache), executor, Arc::clone(&app));
        // Generous under a loaded parallel suite: the property is "publishes
        // without the loop", not how fast a starved thread gets scheduled.
        let deadline = Instant::now() + Duration::from_secs(10);
        while cache.refreshes.load(Ordering::Acquire) == 0 {
            assert!(
                Instant::now() < deadline,
                "background health rollup did not publish"
            );
            std::thread::yield_now();
        }
        let report = build_health_report(
            &cache,
            &Executor::new(),
            &HashMap::new(),
            &DispatchPathMetrics::new(),
            &app,
        );
        let report_metrics = report.metrics.expect("health metrics");
        assert!(report_metrics["backup_skipped_too_large_total"].is_u64());
        assert!(report_metrics["backup_skipped_temp_path_total"].is_u64());
        assert!(
            report_metrics["snapshot_age_ms"]
                .as_u64()
                .is_some_and(|age| age < 1_000),
            "the background publication must reset snapshot age"
        );
        worker.shutdown();
    }

    #[test]
    fn health_worker_refresh_requests_respect_minimum_interval() {
        let cache = Arc::new(HealthRollupCache::new());
        let worker = HealthRollupWorker::start_with_interval(
            Arc::clone(&cache),
            Arc::new(Executor::new()),
            crate::context::App::default_shared(),
            Duration::from_millis(100),
        );
        let started = Instant::now();
        while started.elapsed() < Duration::from_millis(350) {
            worker.request_refresh();
            std::thread::sleep(Duration::from_millis(5));
        }
        worker.shutdown();
        let refreshes = cache.refreshes.load(Ordering::Acquire);
        assert!(
            refreshes >= 2,
            "requests must not prevent periodic refreshes"
        );
        assert!(
            refreshes <= 4,
            "refresh requests bypassed the interval: {refreshes} builds in 350 ms"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    #[ignore = "60-second offline rollup thread CPU measurement with 50 actor roots"]
    fn bench_health_rollup_cpu_per_minute() {
        use std::os::unix::thread::JoinHandleExt;
        let executor = Arc::new(Executor::with_config(crate::executor::ExecutorConfig {
            pool_size: 1,
            read_cap: 1,
            actor_cap: 64,
            heavy_permits: 1,
            drr_quantum: 1,
        }));
        let mut dirs = Vec::new();
        for i in 0..50 {
            let (dir, root) = test_root(&format!("rollup-cpu-{i}"));
            assert!(executor.register_actor(root, test_ctx()));
            dirs.push(dir);
        }
        let cache = Arc::new(HealthRollupCache::new());
        let worker = HealthRollupWorker::start(
            Arc::clone(&cache),
            executor,
            crate::context::App::default_shared(),
        );
        let mut clock = 0;
        assert_eq!(
            unsafe {
                libc::pthread_getcpuclockid(
                    worker.join.as_ref().unwrap().as_pthread_t(),
                    &mut clock,
                )
            },
            0
        );
        let cpu_ns = || {
            let mut time = libc::timespec {
                tv_sec: 0,
                tv_nsec: 0,
            };
            assert_eq!(unsafe { libc::clock_gettime(clock, &mut time) }, 0);
            time.tv_sec as u64 * 1_000_000_000 + time.tv_nsec as u64
        };
        let before = cpu_ns();
        let started = Instant::now();
        // Model route-bind completions; sleeping generates no synthetic CPU load.
        while started.elapsed() < Duration::from_secs(60) {
            worker.request_refresh();
            std::thread::sleep(Duration::from_millis(20));
        }
        let cpu = cpu_ns() - before;
        println!(
            "roots=50 wall_ms={} rollup_cpu_ms_per_minute={} refreshes={}",
            started.elapsed().as_millis(),
            cpu / 1_000_000,
            cache.refreshes.load(Ordering::Acquire)
        );
        worker.shutdown();
    }

    #[test]
    #[ignore = "offline measurement of per-root rollup costs on a populated root"]
    fn bench_health_rollup_per_root_costs() {
        const FILES: usize = 2_000;
        const SYMBOLS: usize = 40;
        let (_dir, root) = test_root("rollup-per-root");
        let ctx = test_ctx();
        {
            let cache = ctx.symbol_cache();
            let mut cache = cache.write().unwrap();
            for file in 0..FILES {
                let symbols = (0..SYMBOLS)
                    .map(|index| crate::symbols::Symbol {
                        name: format!("symbol_{file}_{index}"),
                        kind: crate::symbols::SymbolKind::Function,
                        range: crate::symbols::Range {
                            start_line: 0,
                            start_col: 0,
                            end_line: 0,
                            end_col: 10,
                        },
                        signature: Some(format!("fn symbol_{file}_{index}()")),
                        scope_chain: vec!["module".into()],
                        exported: true,
                        parent: None,
                    })
                    .collect();
                cache.insert(
                    root.as_path().join(format!("src/file_{file}.rs")),
                    std::time::SystemTime::UNIX_EPOCH,
                    1,
                    blake3::hash(&file.to_le_bytes()),
                    symbols,
                );
            }
        }
        let files: Vec<_> = (0..FILES)
            .map(|file| {
                let path = root.as_path().join(format!("src/f{file}.ts"));
                std::fs::create_dir_all(path.parent().unwrap()).unwrap();
                std::fs::write(
                    &path,
                    format!("export function f{file}() {{ return {file}; }}\n"),
                )
                .unwrap();
                path
            })
            .collect();
        let store_dir = root.as_path().join(".callgraph-bench");
        let writer = crate::callgraph_store::CallGraphStore::open(
            store_dir.clone(),
            root.as_path().to_path_buf(),
        )
        .unwrap();
        writer.cold_build(&files).unwrap();
        writer.mark_files_stale(&files[..FILES / 10]).unwrap();
        drop(writer);
        let reader = crate::callgraph_store::CallGraphStore::open_readonly(
            store_dir,
            root.as_path().to_path_buf(),
        )
        .unwrap()
        .expect("ready store");
        *ctx.callgraph_store().write().unwrap() = Some(Arc::new(reader));
        let time = |label: &str, run: &dyn Fn()| {
            run();
            let started = Instant::now();
            for _ in 0..20 {
                run();
            }
            println!(
                "per-root {label}: {} us per rollup",
                started.elapsed().as_micros() / 20
            );
        };
        time("memory walk (status path, always walks)", &|| {
            std::hint::black_box(ctx.memory_root_snapshot());
        });
        time("memory (rollup path, unchanged root)", &|| {
            std::hint::black_box(ctx.memory_root_snapshot_and_rollup());
        });
        time("stale_path_census", &|| {
            let store = ctx.callgraph_store().read().unwrap();
            std::hint::black_box(store.as_ref().unwrap().try_stale_path_census().unwrap());
        });
        time("health_summary", &|| {
            std::hint::black_box(ctx.try_health_summary());
        });
        let executor = Executor::new();
        assert!(executor.register_actor(root.clone(), Arc::clone(&ctx)));
        let cache = HealthRollupCache::new();
        let app = crate::context::App::default_shared();
        time("full rollup (1 root)", &|| {
            cache.refresh_for_test(&executor, &app);
            assert_eq!(cache.snapshot().0.metrics["root_count"], 1);
        });
    }

    #[test]
    fn health_names_future_bash_tasks_without_global_shell_degradation() {
        let (dir, root) = test_root("health-task-refusal");
        let storage = dir.path().join("storage");
        let ctx = Arc::new(AppContext::new(
            Box::new(crate::parser::TreeSitterProvider::new()),
            crate::config::Config {
                storage_dir: Some(storage.clone()),
                project_root: Some(root.as_path().into()),
                ..crate::config::Config::default()
            },
        ));
        let harness_storage = storage.join("opencode");
        let task = crate::bash_background::persistence::create_task_layout(
            &harness_storage,
            "session",
            "bash-0000000000000001",
        )
        .unwrap();
        std::fs::write(
            &task.paths.json,
            serde_json::to_vec(
                &json!({"schema_version": crate::bash_background::persistence::SCHEMA_VERSION + 1}),
            )
            .unwrap(),
        )
        .unwrap();
        ctx.bash_background()
            .maybe_gc_persisted(&harness_storage)
            .unwrap();
        let executor = Executor::new();
        assert!(executor.register_actor(root, Arc::clone(&ctx)));
        let cache = HealthRollupCache::new();
        let app = crate::context::App::default_shared();
        let report = refresh_until_root_count(&cache, &executor, &app, 1);
        let metrics = report.metrics.unwrap();
        let refusals = metrics["bash_task_refusals"].as_array().unwrap();
        assert_eq!(refusals.len(), 1);
        assert_eq!(refusals[0]["path"], task.paths.json.display().to_string());
        assert_eq!(refusals[0]["code"], crate::persisted_format::CODE);
        assert_eq!(report.status, HealthStatus::Ok);
        std::fs::remove_dir_all(&task.paths.session_dir).unwrap();
        ctx.bash_background()
            .maybe_gc_persisted(&harness_storage)
            .unwrap();
        let report = refresh_until_root_count(&cache, &executor, &app, 1);
        assert!(report.metrics.unwrap()["bash_task_refusals"]
            .as_array()
            .unwrap()
            .is_empty());
    }

    #[test]
    fn health_unavailable_rollup_reports_degraded_status_and_reason() {
        let executor = Executor::new();
        let cache = HealthRollupCache::new();
        let app = crate::context::App::default_shared();
        let metrics = DispatchPathMetrics::new();
        let report = build_health_report(&cache, &executor, &HashMap::new(), &metrics, &app);
        assert_eq!(report.status, HealthStatus::Degraded);
        let detail = report.detail.unwrap();
        assert!(
            detail.contains("health diagnostic snapshot is being refreshed")
                || detail.contains(EXECUTOR_SCHEDULER_CONTENTION_DETAIL),
            "{detail}"
        );

        executor.hold_state_lock_for_test(|| cache.refresh(&executor, &app));
        let report = build_health_report(&cache, &executor, &HashMap::new(), &metrics, &app);
        assert_eq!(report.status, HealthStatus::Degraded);
        assert!(report
            .detail
            .unwrap()
            .contains(EXECUTOR_SCHEDULER_CONTENTION_DETAIL));
    }

    fn contended_health_fixture() -> (tempfile::TempDir, Executor, HealthRollupCache, Arc<App>) {
        let (dir, root) = test_root("health-contended-census");
        let ctx = Arc::new(AppContext::new(
            Box::new(crate::parser::TreeSitterProvider::new()),
            crate::config::Config {
                storage_dir: Some(dir.path().join("storage")),
                project_root: Some(root.as_path().into()),
                ..crate::config::Config::default()
            },
        ));
        let executor = Executor::new();
        assert!(executor.register_actor(root, ctx));
        let cache = HealthRollupCache::new();
        cache.clock_ms.store(1_000, Ordering::Relaxed);
        let app = App::default_shared();
        app.lsp_child_registry().track(std::process::id());
        cache.refresh_for_test(&executor, &app);
        assert_eq!(cache.snapshot().0.metrics["actor_count"], 1);
        (dir, executor, cache, app)
    }

    #[test]
    fn health_contention_preserves_census_and_timestamp_with_age() {
        let (_dir, executor, cache, app) = contended_health_fixture();
        let good = cache.snapshot().0;
        assert_eq!(good.metrics["lsp_children"]["spawned"], 1);
        cache.clock_ms.store(1_300, Ordering::Relaxed);
        executor.hold_state_lock_for_test(|| cache.refresh(&executor, &app));

        let stale = cache.snapshot().0;
        // A failed refresh only marks the retained census stale. It must not
        // replace or copy the payload while holding the cache lock.
        assert!(
            Arc::ptr_eq(&good, &stale),
            "failed refresh copied the census"
        );
        assert_eq!(
            stale.metrics, good.metrics,
            "last measured census must survive"
        );
        assert_eq!(stale.memory_census, good.memory_census);
        let report = build_health_report(
            &cache,
            &executor,
            &HashMap::new(),
            &DispatchPathMetrics::new(),
            &app,
        );
        assert_eq!(report.status, HealthStatus::Degraded);
        assert_eq!(
            report.detail.as_deref(),
            Some(EXECUTOR_SCHEDULER_CONTENTION_DETAIL)
        );
        let metrics = report.metrics.unwrap();
        assert_eq!(metrics["generated_at_ms"], 1_000);
        assert_eq!(metrics["stale"], true);
        assert_eq!(metrics["stale_reason"], "executor_contended");
        assert_eq!(metrics["age_ms"], 300);
        assert_eq!(metrics["snapshot_age_ms"], 300);
        assert_eq!(metrics["runtime"]["spawned_lsp_children"], 1);
    }

    #[test]
    fn health_contention_before_first_census_reports_unknown_metrics() {
        let executor = Executor::new();
        let cache = HealthRollupCache::new();
        cache.clock_ms.store(250, Ordering::Relaxed);
        let app = App::default_shared();
        executor.hold_state_lock_for_test(|| cache.refresh(&executor, &app));
        let report = build_health_report(
            &cache,
            &executor,
            &HashMap::new(),
            &DispatchPathMetrics::new(),
            &app,
        );
        assert_eq!(report.status, HealthStatus::Degraded);
        let metrics = report.metrics.unwrap();
        for key in [
            "actor_count",
            "root_count",
            "root_details_omitted",
            "warming_roots",
            "roots",
            "callgraph_repair_entries_60s_total",
            "callgraph_repair_roots_annotated",
            "callgraph_repair_roots_total",
            "callgraph_commits_60s_total",
            "callgraph_pages_or_bytes_written_60s_total",
            "lsp",
            "threads",
            "sqlite",
            "children",
            "fds",
            "generated_at_ms",
            "age_ms",
            "snapshot_age_ms",
        ] {
            assert_eq!(metrics.get(key), Some(&Value::Null), "unmeasured {key}");
        }
        assert_eq!(metrics["stale"], true);
        assert_eq!(metrics["stale_reason"], "executor_contended");
        assert_eq!(metrics["embedding_backend"]["available"], Value::Null);
        assert_eq!(metrics["runtime"]["spawned_lsp_children"], Value::Null);
        assert_eq!(
            metrics["runtime"]["lsp_children_with_deleted_cwd"],
            Value::Null
        );
        for key in [
            "allocator_slack_bytes",
            "roots",
            "roots_total",
            "rss_bytes",
            "total_attributed_bytes",
        ] {
            assert_eq!(
                metrics["memory"].get(key),
                Some(&Value::Null),
                "unmeasured memory.{key}"
            );
        }
        assert_eq!(cache.memory_census()["roots"], Value::Null);
    }

    #[test]
    fn a_census_read_before_recovery_stays_stale_after_it() {
        let (_dir, executor, cache, app) = contended_health_fixture();
        executor.hold_state_lock_for_test(|| cache.refresh(&executor, &app));
        // A probe takes the stale census, then a refresh recovers before the
        // probe judges its freshness.
        let held = cache.snapshot().0;
        cache.refresh_for_test(&executor, &app);
        assert!(
            cache.census_contended(&held),
            "a census read before recovery was reported fresh after it"
        );
        assert!(!cache.census_contended(&cache.snapshot().0));
    }

    #[test]
    fn health_contention_recovery_clears_staleness() {
        let (_dir, executor, cache, app) = contended_health_fixture();
        cache.clock_ms.store(1_300, Ordering::Relaxed);
        executor.hold_state_lock_for_test(|| cache.refresh(&executor, &app));
        cache.clock_ms.store(1_900, Ordering::Relaxed);
        cache.refresh_for_test(&executor, &app);
        let report = build_health_report(
            &cache,
            &executor,
            &HashMap::new(),
            &DispatchPathMetrics::new(),
            &app,
        );
        assert_eq!(report.status, HealthStatus::Ok);
        let metrics = report.metrics.unwrap();
        assert_eq!(metrics["stale"], false);
        assert_eq!(metrics["stale_reason"], Value::Null);
        assert_eq!(metrics["generated_at_ms"], 1_900);
        assert_eq!(metrics["age_ms"], 0);
        assert_eq!(metrics["actor_count"], 1);
    }

    #[test]
    fn health_contention_repeated_attempts_do_not_reset_age() {
        let (_dir, executor, cache, app) = contended_health_fixture();
        let dispatch = DispatchPathMetrics::new();
        for (now_ms, age_ms) in [(1_300, 300), (1_700, 700)] {
            cache.clock_ms.store(now_ms, Ordering::Relaxed);
            executor.hold_state_lock_for_test(|| cache.refresh(&executor, &app));
            let report = build_health_report(&cache, &executor, &HashMap::new(), &dispatch, &app);
            let metrics = report.metrics.unwrap();
            assert_eq!(metrics["age_ms"], age_ms);
            assert_eq!(metrics["generated_at_ms"], 1_000);
            assert_eq!(metrics["stale_reason"], "executor_contended");
        }
        cache.clock_ms.store(2_000, Ordering::Relaxed);
        let metrics = build_health_report(&cache, &executor, &HashMap::new(), &dispatch, &app)
            .metrics
            .unwrap();
        assert_eq!(
            metrics["age_ms"], 1_000,
            "age advances on reads, not only refreshes"
        );
    }

    #[test]
    fn health_wait_retries_an_unavailable_rollup() {
        let (dir, root) = test_root("health-contended-rollup");
        let ctx = Arc::new(AppContext::new(
            Box::new(crate::parser::TreeSitterProvider::new()),
            crate::config::Config {
                storage_dir: Some(dir.path().join("storage")),
                project_root: Some(root.as_path().into()),
                ..crate::config::Config::default()
            },
        ));
        let executor = Executor::new();
        assert!(executor.register_actor(root, ctx));
        let cache = HealthRollupCache::new();
        let app = crate::context::App::default_shared();
        let (locked_tx, locked_rx) = std::sync::mpsc::channel();
        std::thread::scope(|scope| {
            scope.spawn(|| {
                executor.hold_state_lock_for_test(|| {
                    locked_tx.send(()).unwrap();
                    // Keep the first refresh unavailable regardless of scheduling
                    // speed, then let the polling helper capture the actor.
                    let deadline = Instant::now() + Duration::from_secs(30);
                    while cache.refresh_count_for_test() == 0 {
                        assert!(Instant::now() < deadline, "health refresh never attempted");
                        std::thread::yield_now();
                    }
                });
            });
            locked_rx.recv_timeout(Duration::from_secs(30)).unwrap();
            let report = refresh_until_root_count(&cache, &executor, &app, 1);
            assert_eq!(report.metrics.unwrap()["root_count"], 1);
            assert!(cache.refresh_count_for_test() >= 2);
        });
    }

    fn allocator_rollup(cache: &HealthRollupCache) -> HealthDiagnosticRollup {
        build_health_diagnostic_rollup_from_actors(
            cache,
            &Executor::new(),
            &crate::context::App::default_shared(),
            Vec::new(),
        )
    }

    #[test]
    fn rollup_allocator_both_surfaces_share_one_measurement() {
        let cache = HealthRollupCache::new();
        let before = crate::memory::allocator_snapshot_calls_for_test();
        let rollup = allocator_rollup(&cache);
        assert_eq!(
            crate::memory::allocator_snapshot_calls_for_test() - before,
            1
        );
        assert_eq!(
            rollup.metrics["memory"]["allocator_observation_age_ms"],
            rollup.memory_census["process"]["allocator_observation_age_ms"]
        );
        assert_eq!(
            rollup.metrics["memory"]["allocator_slack_bytes"],
            rollup.memory_census["process"]["allocator_slack_bytes"]
        );
    }

    #[test]
    fn rollup_allocator_reuses_measurement_across_refreshes() {
        let cache = HealthRollupCache::new();
        let before = crate::memory::allocator_snapshot_calls_for_test();
        for now_ms in [0, 3_000, 6_000, 12_000, 24_000, 27_000] {
            cache.clock_ms.store(now_ms, Ordering::Relaxed);
            allocator_rollup(&cache);
        }
        assert_eq!(
            crate::memory::allocator_snapshot_calls_for_test() - before,
            1
        );
    }

    #[test]
    fn rollup_allocator_remeasures_expired_observation() {
        let cache = HealthRollupCache::new();
        cache.clock_ms.store(0, Ordering::Relaxed);
        allocator_rollup(&cache);
        let before = crate::memory::allocator_snapshot_calls_for_test();
        cache.clock_ms.store(30_001, Ordering::Relaxed);
        allocator_rollup(&cache);
        assert_eq!(
            crate::memory::allocator_snapshot_calls_for_test() - before,
            1
        );
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn rollup_allocator_reports_reused_age_on_both_surfaces() {
        let cache = HealthRollupCache::new();
        cache.clock_ms.store(0, Ordering::Relaxed);
        let first = allocator_rollup(&cache);
        let first_age = first.metrics["memory"]["allocator_observation_age_ms"]
            .as_u64()
            .expect("measured observation age");
        cache.clock_ms.store(12_000, Ordering::Relaxed);
        let reused = allocator_rollup(&cache);
        for age in [
            &reused.metrics["memory"]["allocator_observation_age_ms"],
            &reused.memory_census["process"]["allocator_observation_age_ms"],
        ] {
            assert_eq!(age.as_u64(), Some(first_age + 12_000));
        }
    }

    #[test]
    fn a_rollup_estimates_each_roots_memory_once() {
        let (storage, root) = test_root("health-memory-once");
        let mut config = crate::config::Config::default();
        config.storage_dir = Some(storage.path().join("state"));
        let ctx = Arc::new(AppContext::new(
            Box::new(crate::parser::TreeSitterProvider::new()),
            config,
        ));
        let executor = Executor::new();
        assert!(executor.register_actor(root, Arc::clone(&ctx)));
        let cache = HealthRollupCache::new();
        let app = crate::context::App::default_shared();

        let before_contended_attempt = crate::context::memory_estimate_walks_for_test();
        executor.hold_state_lock_for_test(|| cache.refresh(&executor, &app));
        assert_eq!(
            crate::context::memory_estimate_walks_for_test() - before_contended_attempt,
            0,
            "a contended refresh walked the root's indexes"
        );

        let epoch = Instant::now();
        let refresh_at = |now_ms| {
            cache.clock_ms.store(now_ms, Ordering::Relaxed);
            let walks_before = crate::context::memory_estimate_walks_for_test();
            let census_before = cache.census_builds_for_test();
            crate::context::with_memory_walk_clock_for_test(
                epoch + Duration::from_millis(now_ms),
                || cache.refresh_for_test(&executor, &app),
            );
            let rollup = cache.snapshot().0;
            // Root membership is valid for both a new census and a reused one;
            // root_count does not say whether this refresh walked any indexes.
            assert_eq!(rollup.metrics["root_count"], 1);
            (
                crate::context::memory_estimate_walks_for_test() - walks_before,
                cache.census_builds_for_test() - census_before,
                rollup.memory_census_at_ms,
            )
        };
        // The census row and rollup row share one root-index walk. Both reuse
        // it inside the window and measure again exactly at the 30 s boundary.
        assert_eq!(refresh_at(0), (1, 1, Some(0)));
        assert_eq!(refresh_at(29_999), (0, 0, Some(0)));
        assert_eq!(refresh_at(30_000), (1, 1, Some(30_000)));
    }

    #[test]
    fn rollup_resolves_shared_storage_paths_once_per_tick() {
        // One shared App, so every context names the same storage root the way
        // the daemon's contexts do; only the project roots differ.
        let app = crate::context::App::default_shared();
        let executor = Executor::new();
        let cache = HealthRollupCache::new();
        let shared_app = crate::context::App::default_shared();
        let mut entries = Vec::new();
        let mut dirs = Vec::new();
        for index in 0..32 {
            let (dir, root) = test_root(&format!("health-shared-storage-{index:02}"));
            let ctx = Arc::new(AppContext::from_app(
                Arc::clone(&app),
                crate::config::Config::default(),
            ));
            entries.push((root, ctx));
            dirs.push(dir);
        }
        // The fixture really shares one storage root. Comparing configured
        // spellings resolves nothing, so the tick below measures the first
        // registration of this root.
        let storage = entries[0].1.config().storage_dir.clone();
        assert!(storage.is_some());
        for (_, ctx) in &entries {
            assert_eq!(ctx.config().storage_dir, storage);
        }
        crate::bash_background::reset_storage_dir_calls_for_test();
        crate::project_content::reset_register_canonicalizations_for_test();
        let rollup =
            build_health_diagnostic_rollup_from_actors(&cache, &executor, &shared_app, entries);
        assert_eq!(
            rollup.metrics["root_count"], 32,
            "the tick must have visited every actor"
        );
        // One tick over 32 contexts sharing a root resolves it once (plus the
        // one standing-roots probe, which runs once per tick by construction).
        // Resolving once per context would report hundreds here.
        let resolutions = crate::bash_background::storage_dir_calls_for_test();
        let canonicalizations = crate::project_content::register_canonicalizations_for_test();
        println!(
            "health tick over 32 contexts: {resolutions} storage resolutions, {canonicalizations} canonicalizations"
        );
        assert!(
            resolutions <= 8,
            "shared storage resolved once per context: {resolutions}",
        );
        // The first registration canonicalizes the root; repeats reuse the
        // retained spelling without another realpath probe.
        assert!(
            canonicalizations <= 4,
            "storage root canonicalized once per context: {canonicalizations}",
        );
        drop(dirs);
    }

    #[test]
    fn rollup_110_root_step_counters_keep_current_numbers_and_age() {
        let executor = Executor::new();
        let app = crate::context::App::default_shared();
        let cache = HealthRollupCache::new();
        cache.clock_ms.store(0, Ordering::Relaxed);
        let mut dirs = Vec::new();
        let mut first = None;
        for index in 0..110 {
            let (dir, root) = test_root(&format!("rollup-counters-{index}"));
            let ctx = test_ctx();
            if index == 0 {
                first = Some((root.clone(), Arc::clone(&ctx)));
            }
            assert!(executor.register_actor(root, ctx));
            dirs.push(dir);
        }
        cache.refresh_for_test(&executor, &app);
        let (root, ctx) = first.unwrap();
        let label = root.as_path().display().to_string();
        let before = cache.memory_census()["roots"][&label]["attributed_bytes"]
            .as_u64()
            .unwrap();
        cache.clock_ms.store(3000, Ordering::Relaxed);
        assert_eq!(cache.snapshot().1, Some(3000));
        assert_eq!(cache.memory_census()["health_rollup_tick"]["age_ms"], 3000);
        ctx.symbol_cache().write().unwrap().insert(
            root.as_path().join("current.rs"),
            SystemTime::UNIX_EPOCH,
            12,
            blake3::hash(b"current"),
            Vec::new(),
        );
        cache.refresh_for_test(&executor, &app);
        let (snapshot, age) = cache.snapshot();
        assert_eq!(age, Some(0));
        let tick = &snapshot.memory_census["health_rollup_tick"];
        assert!(snapshot.metrics.get("health_rollup_tick").is_none());
        assert_eq!(tick["steps"]["per-root memory census"]["calls"], 110);
        assert_eq!(tick["steps"]["lifecycle OS probes"]["calls"], 1);
        assert_eq!(tick["steps"].as_object().unwrap().len(), 14);
        assert!(tick["elapsed_ms"].as_f64().unwrap() >= 0.0);
        let census = cache.memory_census();
        assert_eq!(census["health_rollup_tick"]["age_ms"], 0);
        assert_eq!(census["roots"].as_object().unwrap().len(), 110);
        assert!(
            census["roots"][&label]["attributed_bytes"]
                .as_u64()
                .unwrap()
                > before
        );
        assert!(census["roots"][&label]["memory_measured_age_ms"].is_u64());
    }

    #[test]
    fn rollup_step_debug_log_is_due_once_per_minute() {
        let cache = HealthRollupCache::new();
        cache.clock_ms.store(0, Ordering::Relaxed);
        assert!(cache.step_log_due());
        assert!(!cache.step_log_due());
        cache.clock_ms.store(59_999, Ordering::Relaxed);
        assert!(!cache.step_log_due());
        cache.clock_ms.store(60_000, Ordering::Relaxed);
        assert!(cache.step_log_due());
        assert!(!cache.step_log_due());
    }

    /// The rollup asks every root for its memory every few seconds. While a
    /// root's indexes keep their shape it must reuse the last index walk, a
    /// shape change must walk again at once, and the reported age must say how
    /// old the reused figures are.
    #[test]
    fn rollup_reuses_an_unchanged_roots_memory_walk_and_reports_its_age() {
        let ctx = test_ctx();
        let walks = crate::context::memory_estimate_walks_for_test;
        let measure = || {
            let before = walks();
            let (_, rollup) = ctx.memory_root_snapshot_and_rollup();
            (
                walks() - before,
                rollup.measured_age_ms.expect("rollup age"),
            )
        };
        let (first_walks, first_age) = measure();
        assert_eq!(first_walks, 1, "the first rollup must walk the indexes");
        assert!(first_age < 1_000, "a fresh walk reported age {first_age}");
        std::thread::sleep(Duration::from_millis(30));
        let (reused_walks, reused_age) = measure();
        assert_eq!(reused_walks, 0, "an unchanged root was walked again");
        assert!(
            reused_age >= 30,
            "a reused walk must report its age, got {reused_age}"
        );
        ctx.symbol_cache().write().unwrap().insert(
            std::path::PathBuf::from("/rollup-shape/changed.rs"),
            std::time::SystemTime::UNIX_EPOCH,
            1,
            blake3::hash(b"changed"),
            Vec::new(),
        );
        let (changed_walks, changed_age) = measure();
        assert_eq!(changed_walks, 1, "a shape change must walk again at once");
        assert!(
            changed_age < reused_age,
            "a new walk must reset the age: {changed_age} >= {reused_age}"
        );
        let (_, rollup) = ctx.memory_root_snapshot_and_rollup();
        let rendered = memory_rollup_metrics(Some(crate::memory::MemoryRollupSnapshot::new(
            "ready",
            std::collections::BTreeMap::from([("/rollup-shape".to_string(), rollup)]),
            crate::memory::AllocatorObservation::measure(),
        )));
        assert!(
            rendered["roots"]["/rollup-shape"]["memory_measured_age_ms"].is_u64(),
            "the per-root memory row must carry its age: {rendered}"
        );
        // A status read outside the rollup always walks and carries no age.
        let before = walks();
        let _ = ctx.memory_root_snapshot();
        assert_eq!(walks() - before, 1);
        // The memory census the rollup publishes also carries, per root, the
        // age of the index walk behind its memory figures.
        let (_root_dir, root) = test_root("rollup-shape-census");
        let executor = Executor::new();
        assert!(executor.register_actor(root.clone(), Arc::clone(&ctx)));
        let cache = HealthRollupCache::new();
        cache.refresh_for_test(&executor, &crate::context::App::default_shared());
        let census = cache.memory_census();
        let label = root.as_path().display().to_string();
        assert!(
            census["roots"][label.as_str()]["memory_measured_age_ms"].is_u64(),
            "the census row must carry its age: {census}"
        );
    }

    fn install_arena_runs(
        arena: &crate::views::semantic_arena::SemanticArena,
        runs: std::ops::Range<u64>,
        chunks: usize,
    ) {
        for run in runs {
            let mut key = [0u8; 32];
            key[..8].copy_from_slice(&run.to_le_bytes());
            arena.install(
                crate::blob_store::v2::FamilyKey::new(
                    crate::blob_store::v2::FamilyPlane::Semantic,
                    key,
                ),
                crate::semantic_index::SemanticVectors::synthetic_for_test(
                    Path::new(&format!("src/run_{run}.rs")),
                    chunks,
                    2,
                ),
            );
        }
    }

    /// The census row of one arena family, as `memory.census` reports it.
    fn census_family_row(census: &Value, family: &str) -> Value {
        census["process"]["semantic_arenas"]
            .as_array()
            .and_then(|rows| rows.iter().find(|row| row["family"] == family))
            .cloned()
            .unwrap_or(Value::Null)
    }

    /// Walking an arena's runs visits every vector they hold. Nothing about
    /// the runs changes between rollups unless a run is added or dropped, so
    /// only those two changes may walk them again, and each must show at once.
    #[test]
    fn arena_memory_is_walked_again_only_when_its_runs_change() {
        let storage = tempfile::tempdir().unwrap();
        let arena = crate::views::semantic_arena::arena_for(storage.path(), "walk-on-change");
        install_arena_runs(&arena, 0..4, 10);
        let walks = || arena.run_memory_walks_for_test();
        let first = arena.memory();
        assert_eq!((first.runs, first.chunks), (4, 40));
        // Rollups of tests running in parallel census every live arena, so
        // they may have walked this one between the installs above. Once the
        // runs stop changing, each change is walked exactly once, by
        // whichever thread reads it first.
        let after_first = walks();
        assert!(after_first >= 1, "the first read must walk the runs");
        for _ in 0..5 {
            assert_eq!(arena.memory(), first, "unchanged runs reported new totals");
        }
        assert_eq!(walks(), after_first, "unchanged runs were walked again");

        install_arena_runs(&arena, 4..5, 10);
        let grown = arena.memory();
        assert_eq!(
            (grown.runs, grown.chunks),
            (5, 50),
            "an added run was missed"
        );
        assert!(grown.bytes > first.bytes);
        assert_eq!(walks(), after_first + 1);

        // Nothing but the arena holds the runs, so trimming drops them all.
        assert_eq!(arena.trim(), 5);
        let trimmed = arena.memory();
        assert_eq!(
            (trimmed.runs, trimmed.chunks),
            (0, 0),
            "dropped runs were still counted"
        );
        assert_eq!(walks(), after_first + 2);
    }

    #[test]
    fn quiet_rollup_does_not_rebuild_inspect_memory_payloads() {
        let executor = Executor::new();
        let app = App::default_shared();
        let cache = HealthRollupCache::new();
        let storage = tempfile::tempdir().unwrap();
        let mut dirs = Vec::new();
        for index in 0..200 {
            let (dir, root) = test_root(&format!("quiet-memory-payload-{index}"));
            let ctx = Arc::new(AppContext::new(
                Box::new(crate::parser::TreeSitterProvider::new()),
                crate::config::Config {
                    storage_dir: Some(storage.path().to_path_buf()),
                    project_root: Some(root.as_path().to_path_buf()),
                    ..crate::config::Config::default()
                },
            ));
            let inspect = ctx
                .inspect_manager()
                .cache_for_paths(ctx.inspect_dir(), root.as_path().to_path_buf())
                .unwrap();
            let scope = crate::inspect::JobScope::for_project(root.as_path().to_path_buf());
            let key = crate::inspect::JobKey::for_category_scope(
                crate::inspect::InspectCategory::Diagnostics,
                &scope,
            );
            inspect
                .store_aggregated(
                    key,
                    json!({"items": [{"message": format!("warning {index}")}]}),
                )
                .unwrap();
            assert!(executor.register_actor(root, ctx));
            dirs.push(dir);
        }
        let now = Instant::now();
        cache.clock_ms.store(0, Ordering::Relaxed);
        crate::context::with_memory_walk_clock_for_test(now, || {
            cache.refresh_for_test(&executor, &app);
        });
        let before = crate::inspect::cache::aggregate_memory_walks_for_test();
        cache.clock_ms.store(3_000, Ordering::Relaxed);
        crate::context::with_memory_walk_clock_for_test(now + Duration::from_secs(3), || {
            cache.refresh_for_test(&executor, &app);
        });
        assert_eq!(
            crate::inspect::cache::aggregate_memory_walks_for_test() - before,
            0,
            "a quiet 200-root tick rebuilt retained inspect payload estimates",
        );
        let (root, ctx) = executor.actor_entries().into_iter().next().unwrap();
        let inspect = ctx
            .inspect_manager()
            .cache_for_paths(ctx.inspect_dir(), root.as_path().to_path_buf())
            .unwrap();
        let scope = crate::inspect::JobScope::for_project(root.as_path().to_path_buf());
        let key = crate::inspect::JobKey::for_category_scope(
            crate::inspect::InspectCategory::Diagnostics,
            &scope,
        );
        inspect
            .store_aggregated(
                key,
                json!({"items": [{"message": "a changed diagnostic with a longer message"}]}),
            )
            .unwrap();
        let before = crate::inspect::cache::aggregate_memory_walks_for_test();
        cache.clock_ms.store(6_000, Ordering::Relaxed);
        crate::context::with_memory_walk_clock_for_test(now + Duration::from_secs(6), || {
            cache.refresh_for_test(&executor, &app);
        });
        assert_eq!(
            crate::inspect::cache::aggregate_memory_walks_for_test() - before,
            1,
            "only the changed root may rebuild its inspect estimate"
        );
        let label = root.as_path().display().to_string();
        let snapshot = cache.snapshot().0;
        assert!(snapshot.metrics["memory"]["roots"]
            .as_object()
            .unwrap()
            .contains_key(&label));
        assert!(snapshot.metrics["roots"]
            .as_array()
            .unwrap()
            .iter()
            .any(|row| row["project_root"] == label));
    }

    #[test]
    fn inspect_memory_rollup_json_matches_legacy_under_churn() {
        let root = tempfile::tempdir().unwrap();
        let storage = tempfile::tempdir().unwrap();
        let inspect = crate::inspect::InspectCache::open(
            storage.path().to_path_buf(),
            root.path().to_path_buf(),
        )
        .unwrap();
        let peers = (0..2)
            .map(|index| {
                let cache = crate::inspect::InspectCache::open(
                    storage.path().join(format!("peer-{index}")),
                    root.path().to_path_buf(),
                )
                .unwrap();
                cache
                    .store_aggregated(
                        crate::inspect::JobKey::for_project_category(
                            crate::inspect::InspectCategory::Todos,
                        ),
                        json!({"items": ["fixed peer diagnostic".repeat(30 * (index + 1))]}),
                    )
                    .unwrap();
                cache
            })
            .collect::<Vec<_>>();
        let assert_parity = || {
            let caches = std::iter::once(&inspect).chain(peers.iter());
            let legacy = caches
                .clone()
                .map(|cache| cache.legacy_estimated_memory_for_test())
                .collect::<Vec<_>>();
            let current = caches
                .map(|cache| cache.estimated_memory())
                .collect::<Vec<_>>();
            assert_eq!(
                serde_json::to_vec(&current).unwrap(),
                serde_json::to_vec(&legacy).unwrap()
            );
            let shared = crate::memory::SharedProcessMemory::measure();
            let allocator = crate::memory::AllocatorObservation::measure();
            let build = |estimates: &[crate::memory::MemoryEstimate]| {
                let roots = (0..200usize)
                    .map(|index| {
                        let other = crate::memory::MemoryEstimate::estimated(index as u64 * 100);
                        let row = crate::memory::RootMemoryRollup::from_estimates(&[
                            &estimates[index % estimates.len()],
                            &other,
                        ])
                        .with_measured_age_ms(3_000);
                        let row = if index % 3 == 0 {
                            row.with_standing()
                        } else {
                            row
                        };
                        (format!("/parity/root-{index:03}"), row)
                    })
                    .collect();
                crate::memory::MemoryRollupSnapshot::with_shared(
                    "ready",
                    roots,
                    allocator.clone(),
                    &shared,
                )
            };
            let expected = build(&legacy);
            let mut actual = build(&current);
            let root_and_shared = |process: &crate::memory::ProcessMemorySnapshot| {
                process.total_attributed_bytes
                    - process.sqlite.memory_used_bytes
                    - process.embedding_model_bytes
            };
            assert_eq!(
                root_and_shared(&actual.process),
                root_and_shared(&expected.process)
            );
            assert_eq!(actual.process.root_count, expected.process.root_count);
            assert_eq!(
                actual.process.busy_subsystems,
                expected.process.busy_subsystems
            );
            assert_eq!(
                actual.process.not_estimated_subsystems,
                expected.process.not_estimated_subsystems
            );
            // Process counters can change between constructors. Their root-derived
            // totals were checked above; use one observation for the wire comparison.
            actual.process = expected.process.clone();
            assert_eq!(
                serde_json::to_vec(&memory_rollup_metrics(Some(actual))).unwrap(),
                serde_json::to_vec(&memory_rollup_metrics(Some(expected))).unwrap(),
            );
        };
        assert_parity();
        let project =
            crate::inspect::JobKey::for_project_category(crate::inspect::InspectCategory::Todos);
        for revision in 0..140 {
            inspect.store_aggregated(project.clone(), json!({"items": [{"message": "warning".repeat(revision + 1), "revision": revision}]})).unwrap();
            inspect
                .store_aggregated(
                    crate::inspect::JobKey {
                        category: crate::inspect::InspectCategory::Todos,
                        scope_hash: Some(format!("scope-{revision}")),
                    },
                    json!({"nested": [{"unicode": "λ", "values": [null, false, revision]}]}),
                )
                .unwrap();
            assert_parity();
            inspect.get_aggregated(&project).unwrap().unwrap();
            assert_parity();
        }
    }

    /// What a rollup costs while nothing changes, counted rather than timed:
    /// it walks no arena runs and no root index, and it measures no memory
    /// census, however many roots and resident chunks there are. The compact
    /// memory figures are still measured on every rollup.
    #[test]
    fn an_unchanged_rollup_does_constant_work_per_tick() {
        let storage = tempfile::tempdir().unwrap();
        let arena = crate::views::semantic_arena::arena_for(storage.path(), "constant-work");
        install_arena_runs(&arena, 0..50, 100);
        let executor = Executor::new();
        let app = crate::context::App::default_shared();
        let mut dirs = Vec::new();
        for index in 0..20 {
            let (dir, root) = test_root(&format!("constant-work-{index}"));
            assert!(executor.register_actor(root, test_ctx()));
            dirs.push(dir);
        }
        let cache = HealthRollupCache::new();
        cache.clock_ms.store(0, Ordering::Relaxed);
        let counters = || {
            (
                arena.run_memory_walks_for_test(),
                crate::context::memory_estimate_walks_for_test(),
                cache.census_builds_for_test(),
            )
        };
        let before_first = counters();
        cache.refresh_for_test(&executor, &app);
        let after_first = counters();
        // Parallel tests' rollups also census this arena, so the walk of its
        // runs may have been made on another thread; it was made once.
        assert!(after_first.0 >= 1, "no rollup walked the arena");
        assert_eq!(
            after_first.1 - before_first.1,
            20,
            "one index walk per root"
        );
        assert_eq!(
            after_first.2 - before_first.2,
            1,
            "the first rollup measures the census"
        );
        // Nine more rollups, three seconds apart: 27 s, inside the census
        // interval, with no census read in between.
        for tick in 1..=9u64 {
            cache.clock_ms.store(tick * 3_000, Ordering::Relaxed);
            let before = counters();
            cache.refresh_for_test(&executor, &app);
            let after = counters();
            assert_eq!(
                (after.0 - before.0, after.1 - before.1, after.2 - before.2),
                (0, 0, 0),
                "rollup {tick} redid work for unchanged inputs (arena walks, index walks, census builds)"
            );
            let (snapshot, age) = cache.snapshot();
            assert_eq!(age, Some(0));
            assert_eq!(snapshot.metrics["root_count"], 20);
            // The rollup still times every step, including the census steps
            // it passed through without measuring a census.
            assert_eq!(snapshot.tick["steps"].as_object().unwrap().len(), 14);
            assert!(
                snapshot.metrics["memory"]["semantic_arena_bytes"]
                    .as_u64()
                    .unwrap()
                    >= arena.memory().bytes,
                "the compact memory figures must still count the arena"
            );
        }
    }

    /// The full census is measured when a read asks for it, when its roots
    /// change, or when it reaches the census interval; in between the rollup
    /// carries it, and a read reports how old it and its figures are.
    #[test]
    fn the_memory_census_is_measured_on_demand_and_reports_its_age() {
        let storage = tempfile::tempdir().unwrap();
        let family = "census-on-demand";
        let arena = crate::views::semantic_arena::arena_for(storage.path(), family);
        install_arena_runs(&arena, 0..2, 10);
        let executor = Executor::new();
        let app = crate::context::App::default_shared();
        let (_first_dir, first_root) = test_root("census-on-demand-first");
        assert!(executor.register_actor(first_root.clone(), test_ctx()));
        let first_label = first_root.as_path().display().to_string();
        let cache = HealthRollupCache::new();
        let builds = || cache.census_builds_for_test();
        let refresh_at = |now_ms: u64| {
            cache.clock_ms.store(now_ms, Ordering::Relaxed);
            let before = builds();
            cache.refresh_for_test(&executor, &app);
            builds() - before
        };
        assert_eq!(refresh_at(0), 1, "the first rollup measures the census");
        let measured_bytes = census_family_row(&cache.memory_census(), family)["bytes"].clone();
        assert!(measured_bytes.as_u64().is_some_and(|bytes| bytes > 0));
        // That read asked for a fresh census; the next rollup measures it.
        assert_eq!(refresh_at(3_000), 1, "a census read was not answered");
        assert_eq!(refresh_at(6_000), 0, "a census was measured with no read");

        // A change no read has asked about yet: the carried census still
        // shows the old figures, and says they are 4 s old.
        install_arena_runs(&arena, 2..3, 10);
        cache.clock_ms.store(7_000, Ordering::Relaxed);
        let carried = cache.memory_census();
        assert_eq!(carried["health_rollup_tick"]["age_ms"], 4_000);
        assert_eq!(census_family_row(&carried, family)["bytes"], measured_bytes);
        assert!(
            carried["roots"][&first_label]["memory_measured_age_ms"]
                .as_u64()
                .is_some_and(|age| age >= 4_000),
            "a carried row must count the census age: {carried}"
        );
        assert_eq!(refresh_at(9_000), 1);
        let fresh = cache.memory_census();
        assert_eq!(fresh["health_rollup_tick"]["age_ms"], 0);
        assert!(
            census_family_row(&fresh, family)["bytes"].as_u64().unwrap()
                > measured_bytes.as_u64().unwrap(),
            "the census measured after the read missed the added run"
        );

        // A root bound since the last census is measured at once.
        assert_eq!(refresh_at(12_000), 1, "the read above was not answered");
        let (_second_dir, second_root) = test_root("census-on-demand-second");
        assert!(executor.register_actor(second_root.clone(), test_ctx()));
        assert_eq!(
            refresh_at(15_000),
            1,
            "a newly bound root waited for the interval"
        );
        let second_label = second_root.as_path().display().to_string();
        assert!(cache.snapshot().0.memory_census["roots"]
            .get(&second_label)
            .is_some());

        // With no read and no root change, the census is measured again once
        // it reaches the interval.
        assert_eq!(refresh_at(44_999), 0);
        assert_eq!(refresh_at(45_000), 1, "the census outlived its interval");
    }

    fn census_runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("census test runtime")
    }

    /// After a quiet period longer than the census interval, the served
    /// census is old. The first read must not get it: it starts a rollup and
    /// receives a census measured after the read began.
    #[test]
    fn a_census_read_after_a_quiet_period_gets_a_census_measured_after_it() {
        let (_dir, root) = test_root("census-after-quiet");
        let executor = Arc::new(Executor::new());
        assert!(executor.register_actor(root.clone(), test_ctx()));
        let cache = Arc::new(HealthRollupCache::new());
        let app = crate::context::App::default_shared();
        cache.clock_ms.store(0, Ordering::Relaxed);
        // Seed a known publication without relying on a periodic worker's
        // nonblocking scheduler snapshot, which may legitimately skip a tick.
        cache.refresh_for_test(&executor, &app);
        let builds_before = cache.census_builds_for_test();
        cache.clock_ms.store(31_000, Ordering::Relaxed);
        let (wake_tx, wake_rx) = std::sync::mpsc::sync_channel(1);
        *cache.census_wake.lock().unwrap() = Some(wake_tx);
        let census = std::thread::scope(|scope| {
            // No periodic refresh: only the read's census wake can publish.
            // The blocking test snapshot isolates freshness from lock contention.
            let cache = &cache;
            let executor = &executor;
            let app = &app;
            scope.spawn(move || {
                assert_eq!(
                    wake_rx.recv_timeout(Duration::from_secs(5)).unwrap(),
                    RollupWake::Census
                );
                cache.refresh_for_test(&executor, &app);
            });
            let started = Instant::now();
            census_runtime().block_on(fresh_memory_census(
                &cache,
                started + MEMORY_CENSUS_REPLY_MARGIN + Duration::from_secs(5),
            ))
        });
        assert_eq!(
            census["health_rollup_tick"]["age_ms"], 0,
            "the read was served the census from before the quiet period"
        );
        assert_eq!(cache.census_builds_for_test() - builds_before, 1);
        assert!(census["roots"]
            .get(root.as_path().display().to_string().as_str())
            .is_some());
    }

    /// A rollup thread that never publishes must not hold the reply past its
    /// budget: the read returns the old census, with its age, within the
    /// bound.
    #[test]
    fn a_census_read_returns_the_old_census_when_no_rollup_publishes_in_time() {
        let (_dir, root) = test_root("census-no-publish");
        let executor = Executor::new();
        assert!(executor.register_actor(root, test_ctx()));
        let cache = HealthRollupCache::new();
        cache.clock_ms.store(0, Ordering::Relaxed);
        cache.refresh_for_test(&executor, &crate::context::App::default_shared());
        // A wake channel nobody reads, as a stuck rollup thread leaves it.
        let (wake_tx, _wake_rx) = std::sync::mpsc::sync_channel(1);
        *cache.census_wake.lock().unwrap() = Some(wake_tx);
        cache.clock_ms.store(31_000, Ordering::Relaxed);
        let runtime = census_runtime();

        let wait = Duration::from_millis(300);
        let started = Instant::now();
        let census = runtime.block_on(fresh_memory_census(
            &cache,
            started + MEMORY_CENSUS_REPLY_MARGIN + wait,
        ));
        let elapsed = started.elapsed();
        assert_eq!(census["health_rollup_tick"]["age_ms"], 31_000);
        assert!(
            elapsed >= wait,
            "the read did not wait for a rollup: {elapsed:?}"
        );
        assert!(
            elapsed < wait + Duration::from_secs(2),
            "the read outlived its bound: {elapsed:?}"
        );

        // A request whose budget is already inside the reply margin is
        // answered at once.
        let started = Instant::now();
        let census = runtime.block_on(fresh_memory_census(&cache, started));
        assert_eq!(census["health_rollup_tick"]["age_ms"], 31_000);
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    /// Reads add files to a root's symbol cache far more often than edits
    /// change its trigram index, and the trigram walk visits every indexed
    /// file. A symbol-cache change must re-walk only the symbol cache, an
    /// edit only the trigram index, and the reported age must stay that of
    /// the oldest walk still in use.
    #[test]
    fn rollup_rewalks_only_the_index_whose_shape_changed() {
        let ctx = test_ctx();
        let mut index = crate::search_index::SearchIndex::new();
        index.index_file(Path::new("/rollup-split/a.rs"), b"fn a() {}");
        *ctx.search_index().write().unwrap() = Some(index);
        let measure = || {
            let before = crate::context::memory_index_walks_for_test();
            let (_, rollup) = ctx.memory_root_snapshot_and_rollup();
            let after = crate::context::memory_index_walks_for_test();
            (
                [
                    after[0] - before[0],
                    after[1] - before[1],
                    after[2] - before[2],
                ],
                rollup.measured_age_ms.expect("rollup age"),
            )
        };
        // No semantic index is installed, so there is nothing to walk there.
        assert_eq!(
            measure().0,
            [0, 1, 1],
            "the first rollup walks both indexes"
        );
        std::thread::sleep(Duration::from_millis(30));
        assert_eq!(measure().0, [0, 0, 0], "an unchanged root was walked again");

        ctx.symbol_cache().write().unwrap().insert(
            std::path::PathBuf::from("/rollup-split/read.rs"),
            std::time::SystemTime::UNIX_EPOCH,
            1,
            blake3::hash(b"read"),
            Vec::new(),
        );
        let (walks, age) = measure();
        assert_eq!(
            walks,
            [0, 0, 1],
            "a read must re-walk only the symbol cache"
        );
        assert!(
            age >= 30,
            "the reused trigram walk is the oldest figure, got age {age}"
        );

        ctx.search_index()
            .write()
            .unwrap()
            .as_mut()
            .unwrap()
            .index_file(Path::new("/rollup-split/b.rs"), b"fn b() {}");
        let (walks, age) = measure();
        assert_eq!(
            walks,
            [0, 1, 0],
            "an edit must re-walk only the trigram index"
        );
        assert!(age < 30, "every walk in use is recent, got age {age}");
    }

    #[test]
    fn rollup_reuses_breaker_connection_and_reopens_after_read_error() {
        let (storage, root) = test_root("health-breaker-cache");
        let mut config = crate::config::Config::default();
        config.storage_dir = Some(storage.path().join("state"));
        let ctx = Arc::new(AppContext::new(
            Box::new(crate::parser::TreeSitterProvider::new()),
            config,
        ));
        let executor = Executor::new();
        assert!(executor.register_actor(root.clone(), Arc::clone(&ctx)));
        let key = crate::search_index::artifact_cache_key(root.as_path());
        let path = ctx
            .storage_dir()
            .join("callgraph")
            .join(&key)
            .join("build-breaker.sqlite");
        crate::build_breaker::BuildDeathBreaker::open(path).expect("create fixture breaker");
        crate::build_breaker::BuildDeathBreaker::reset_open_calls_for_test();

        let cache = HealthRollupCache::new();
        let app = crate::context::App::default_shared();
        cache.refresh(&executor, &app);
        cache.refresh(&executor, &app);
        assert_eq!(
            crate::build_breaker::BuildDeathBreaker::open_calls_for_test(),
            1,
            "consecutive rollups over the same root reuse one cached breaker"
        );

        crate::build_breaker::BuildDeathBreaker::fail_next_active_suspensions_for_test();
        cache.refresh(&executor, &app);
        cache.refresh(&executor, &app);
        assert_eq!(
            crate::build_breaker::BuildDeathBreaker::open_calls_for_test(),
            2,
            "a read error evicts the cached breaker and the next rollup reopens it"
        );
    }

    #[test]
    fn bg_observability_rate_limit_reports_suppressed_count_and_lifecycle_lines() {
        let (_dir, root) = test_root("health-bg-observability-rate");
        let metrics = DispatchPathMetrics::new();
        let now = Instant::now();
        let session = "bg-health-session";

        assert_eq!(
            metrics.record_bg_event_at(&root, session, BgEventKind::ArmHit, now),
            Some(0)
        );
        assert_eq!(
            metrics.record_bg_event_at(
                &root,
                session,
                BgEventKind::ArmHit,
                now + Duration::from_secs(1),
            ),
            None
        );
        assert_eq!(
            metrics.record_bg_event_at(
                &root,
                session,
                BgEventKind::ArmHit,
                now + BG_OBSERVABILITY_INTERVAL,
            ),
            Some(1)
        );

        take_bg_observability_logs_for_test();
        let channel = super::super::route_key(21, 3);
        metrics.record_bg_subscription_installed(&root, session, channel);
        metrics.record_bg_subscription_ended(&root, session, channel, "goodbye");
        assert_eq!(
            take_bg_observability_logs_for_test(),
            vec![
                format!(
                    "subc bg subscription: installed root={} session=bg-health-session channel=21@3 cause=subscribe suppressed=0",
                    root.as_path().display()
                ),
                format!(
                    "subc bg subscription: ended root={} session=bg-health-session channel=21@3 cause=goodbye suppressed=0",
                    root.as_path().display()
                ),
            ]
        );
    }

    #[test]
    fn interactive_occupancy_census_names_job_lane_age_and_root_once() {
        let executor = Executor::new();
        let (_dir, root) = test_root("interactive-occupancy-census");
        executor.register_actor(root.clone(), test_ctx());
        let (started_tx, started_rx) = std::sync::mpsc::sync_channel(1);
        let (release_tx, release_rx) = std::sync::mpsc::sync_channel(1);
        let response = executor.submit_async(
            root.clone(),
            Lane::PureRead,
            "long-search".to_string(),
            Box::new(move |_| {
                started_tx.send(()).expect("signal long search");
                release_rx.recv().expect("release long search");
                Response::success("long-search", json!({}))
            }),
        );
        started_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("long search starts");
        take_interactive_occupancy_logs_for_test();

        // The production census deliberately uses try_lock, so a scheduler turn
        // may make one observation a no-op. Retry until the nonblocking snapshot
        // succeeds, then prove later observations do not report the same job.
        let deadline = Instant::now() + Duration::from_secs(1);
        let logs = loop {
            warn_slow_running_interactive_jobs_after(&executor, Duration::ZERO);
            let logs = take_interactive_occupancy_logs_for_test();
            if !logs.is_empty() {
                break logs;
            }
            assert!(Instant::now() < deadline, "occupancy census stayed busy");
            std::thread::yield_now();
        };
        warn_slow_running_interactive_jobs_after(&executor, Duration::ZERO);
        assert!(
            take_interactive_occupancy_logs_for_test().is_empty(),
            "a reported running job must not emit a second census line"
        );
        release_tx.send(()).expect("release long search");
        assert!(response.blocking_recv().expect("search response").success);

        assert_eq!(logs.len(), 1, "one running job emits one census line");
        let line = &logs[0];
        assert!(line.contains("class=Interactive"));
        assert!(line.contains("job=long-search"));
        assert!(line.contains("command="));
        assert!(line.contains("lane=PureRead"));
        assert!(line.contains("state=executing"));
        assert!(line.contains("age_ms="));
        assert!(line.contains(&format!("root={root}")));
    }

    #[cfg(unix)]
    #[test]
    fn bash_posix_spawn_health_reports_forced_fallback() {
        let executor = Executor::new();
        let metrics = DispatchPathMetrics::new();
        let app = crate::context::App::default_shared();
        let cache = HealthRollupCache::new();
        crate::posix_spawn::FORCE_FALLBACK.set(true);
        struct Reset;
        impl Drop for Reset {
            fn drop(&mut self) {
                crate::posix_spawn::FORCE_FALLBACK.set(false);
            }
        }
        let _reset = Reset;
        let health = build_health_report(&cache, &executor, &HashMap::new(), &metrics, &app)
            .metrics
            .unwrap();
        assert_eq!(health["bash_spawn"], "fork_fallback");
        assert!(health["bash_spawn_missing_capability"]
            .as_str()
            .unwrap()
            .contains("test-forced"));
        assert!(health["bash_spawn_fallback_spawns"].is_u64());
        assert_eq!(health["bash_pty_spawn"], "fork");
    }

    #[test]
    fn bg_runtime_health_fields_are_always_present() {
        let executor = Executor::new();
        let metrics = DispatchPathMetrics::new();
        let app = crate::context::App::default_shared();
        let (_dir, root) = test_root("health-bg-runtime-fields");
        let channel = super::super::route_key(22, 4);
        let cache = HealthRollupCache::new();
        cache.refresh(&executor, &app);

        let cold = build_health_report(&cache, &executor, &HashMap::new(), &metrics, &app);
        let cold_metrics = cold.metrics.expect("cold health metrics");
        let cold_runtime = &cold_metrics["runtime"];
        for (section, field) in [
            ("lsp", "children_total"),
            ("lsp", "children_by_root"),
            ("lsp", "children_without_client"),
            ("lsp", "children_with_deleted_cwd"),
            ("threads", "total"),
            ("threads", "classified"),
            ("threads", "by_class"),
            ("sqlite", "open_connections"),
            ("sqlite", "open_by_store"),
            ("children", "detached_total"),
            ("fds", "open"),
            ("fds", "soft_limit"),
        ] {
            assert!(
                cold_metrics[section].get(field).is_some(),
                "health omitted {section}.{field}: {cold_metrics:#}"
            );
        }
        assert_eq!(cold_runtime["bg_subscriptions"].as_u64(), Some(0));
        assert_eq!(cold_runtime["bg_wake_pending"].as_u64(), Some(0));
        assert_eq!(cold_runtime["bg_wake_unacked_total"].as_u64(), Some(0));
        assert_eq!(cold_runtime["bg_wake_rearm_total"].as_u64(), Some(0));
        assert_eq!(
            cold_runtime["bg_nudges_enqueued_60s_total"].as_u64(),
            Some(0)
        );
        assert_eq!(cold_runtime["bg_arm_misses_60s_total"].as_u64(), Some(0));
        assert_eq!(
            cold_metrics["cold_build_limiter"]["cap"].as_u64(),
            Some(crate::cold_build_limiter::limit() as u64)
        );
        assert!(cold_metrics["cold_build_limiter"]["holders"].is_array());
        assert!(cold_metrics["cold_build_limiter"]["queued"].is_array());

        metrics.record_bg_runtime(2, 1, 3);
        metrics.record_bg_wake_rearm();
        metrics.record_bg_arm_miss(&root, "missing-session", 2);
        metrics.record_bg_nudge_enqueued(&root, "live-session", channel);

        // The rollup remains unchanged; liveness counters must bypass it.
        let hot = build_health_report(&cache, &executor, &HashMap::new(), &metrics, &app);
        let hot_metrics = hot.metrics.expect("hot health metrics");
        let hot_runtime = &hot_metrics["runtime"];
        assert_eq!(hot_runtime["bg_subscriptions"].as_u64(), Some(2));
        assert_eq!(hot_runtime["bg_wake_pending"].as_u64(), Some(1));
        assert_eq!(hot_runtime["bg_wake_unacked_total"].as_u64(), Some(3));
        assert_eq!(hot_runtime["bg_wake_rearm_total"].as_u64(), Some(1));
        assert_eq!(
            hot_runtime["bg_nudges_enqueued_60s_total"].as_u64(),
            Some(1)
        );
        assert_eq!(hot_runtime["bg_arm_misses_60s_total"].as_u64(), Some(1));
    }

    #[test]
    fn tier2_first_scan_keeps_root_in_warming_rollup() {
        let executor = Executor::with_config(crate::executor::ExecutorConfig {
            pool_size: 1,
            read_cap: 1,
            actor_cap: 1,
            heavy_permits: 1,
            drr_quantum: 1,
        });
        let (_dir, root) = test_root("health-tier2-first-scan");
        let mut config = crate::config::Config::default();
        config.project_root = Some(root.as_path().to_path_buf());
        config.indexes.trigram = false;
        config.indexes.semantic = false;
        config.indexes.callgraph = false;
        config.inspect.enabled = true;
        let ctx = Arc::new(AppContext::new(
            Box::new(crate::parser::TreeSitterProvider::new()),
            config,
        ));
        ctx.inspect_manager()
            .set_tier2_in_flight_for_test(crate::inspect::InspectCategory::DeadCode, true);
        assert!(executor.register_actor(root.clone(), Arc::clone(&ctx)));

        // Health reads try-locks by design and reports "could not be snapshotted
        // without contention" when a parallel test holds one; that is the busy
        // signal, not the verdict under test, so retry until a real snapshot.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let report = loop {
            let report = test_health_report(
                &executor,
                &HashMap::new(),
                &DispatchPathMetrics::new(),
                &crate::context::App::default_shared(),
            );
            let busy = report
                .detail
                .as_deref()
                .is_some_and(|detail| detail.contains("without contention"));
            if !busy {
                break report;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "health snapshot stayed contended: {report:?}"
            );
            std::thread::sleep(std::time::Duration::from_millis(20));
        };
        assert_eq!(
            report.detail.as_deref(),
            Some("1 root(s) warming background indexes (serving normally)")
        );
        let metrics = report.metrics.expect("health metrics");
        assert_eq!(metrics["roots"][0]["search_index"]["status"], "disabled");
        assert_eq!(metrics["roots"][0]["semantic_index"]["status"], "disabled");
        assert_eq!(metrics["roots"][0]["callgraph_store"]["status"], "disabled");
        assert_eq!(metrics["roots"][0]["tier2"]["status"], "building");

        ctx.inspect_manager()
            .set_tier2_in_flight_for_test(crate::inspect::InspectCategory::DeadCode, false);
    }

    #[test]
    fn health_exposes_the_live_privacy_disclaim_setting_per_root() {
        let executor = Executor::with_config(crate::executor::ExecutorConfig {
            pool_size: 1,
            read_cap: 1,
            actor_cap: 1,
            heavy_permits: 1,
            drr_quantum: 1,
        });
        let (_dir, root) = test_root("health-privacy-disclaim");
        let ctx = Arc::new(AppContext::new(
            Box::new(crate::parser::TreeSitterProvider::new()),
            crate::config::Config {
                project_root: Some(root.as_path().to_owned()),
                ..Default::default()
            },
        ));
        assert!(executor.register_actor(root.clone(), Arc::clone(&ctx)));
        for enabled in [true, false] {
            ctx.update_config(|config| config.bash.disclaim_privacy = enabled);
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
            loop {
                let report = test_health_report(
                    &executor,
                    &HashMap::new(),
                    &DispatchPathMetrics::new(),
                    &crate::context::App::default_shared(),
                );
                if let Some(roots) = report
                    .metrics
                    .as_ref()
                    .and_then(|metrics| metrics["roots"].as_array())
                {
                    if let Some(snapshot) = roots.iter().find(|snapshot| {
                        snapshot["project_root"].as_str() == root.as_path().to_str()
                    }) {
                        assert_eq!(snapshot["bash"]["disclaim_privacy"], enabled);
                        assert_eq!(
                            snapshot["bash"]["privacy_disclaim_effective"],
                            enabled && cfg!(target_os = "macos")
                        );
                        break;
                    }
                }
                assert!(std::time::Instant::now() < deadline, "{report:?}");
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
        }
    }

    #[test]
    fn disabled_callgraph_store_reports_disabled_despite_lingering_build_receiver() {
        let executor = Executor::new();
        let (_dir, root) = test_root("health-callgraph-disabled-lingering-rx");
        let mut config = crate::config::Config::default();
        config.indexes.callgraph = false;
        let ctx = Arc::new(AppContext::new(
            Box::new(crate::parser::TreeSitterProvider::new()),
            config,
        ));
        // Reproduce the configure window in which the disabling config is
        // already published while the previous generation's build receiver
        // has not been retired yet.
        let (_tx, rx) = crossbeam_channel::unbounded::<crate::context::CallGraphStoreBuildEvent>();
        *ctx.callgraph_store_rx().lock() = Some(rx);
        assert!(executor.register_actor(root.clone(), ctx));

        let app = crate::context::App::default_shared();
        let cache = HealthRollupCache::new();
        cache.refresh_for_test(&executor, &app);
        let metrics = DispatchPathMetrics::new();
        let mut reads = 0;
        let report = health_report_after_scheduler_contention(
            "disabled_callgraph_store_reports_disabled_despite_lingering_build_receiver",
            || {
                reads += 1;
                let read_report =
                    || build_health_report(&cache, &executor, &HashMap::new(), &metrics, &app);
                if reads <= 3 {
                    health_report_while_scheduler_locked(&executor, read_report)
                } else {
                    read_report()
                }
            },
        );
        // The scheduler thread itself may also hold the lock on a later read, so
        // more retries are valid; three forced collisions need at least four.
        assert!(reads >= 4, "the test must retry the three contended reads");

        assert_ne!(
            report.status,
            HealthStatus::Degraded,
            "a disabled store is not a degradation: {report:?}"
        );
        // build_health_report reads one rollup Arc for both the verdict and
        // the roots detail, so they describe the same moment: the component
        // must not claim "building" for a store this config disables.
        let metrics = report.metrics.expect("health metrics");
        assert_eq!(
            metrics["roots"][0]["callgraph_store"]["status"], "disabled",
            "a disabled store must never report building: {metrics:#}"
        );
    }

    #[test]
    fn overdue_bind_refusal_blames_the_oldest_reader_only_while_the_bind_is_queued() {
        let reader = |request_id: &str, command: &str, started_age_ms: u64| {
            crate::executor::BindBlockerReaderSnapshot {
                request_id: request_id.to_string(),
                command: command.to_string(),
                lane: Lane::PureRead,
                started_age_ms,
                execution_started: true,
                started_before_oldest_writer: true,
            }
        };
        let snapshot = |configure_state: &'static str| BindBlockerSnapshot {
            configure_state,
            configure_phase_timings: None,
            blockers: Vec::new(),
            oldest_queued_writer_age_ms: Some(10_600),
            in_flight_readers: vec![
                reader("subc-1-790", "read", 900),
                reader("subc-1-789", "grep", 840_000),
            ],
            in_flight_writer: None,
            reader_admissions_while_promoted_writer_waited: 0,
        };
        let age = Duration::from_millis(10_600);
        let deadline = Duration::from_millis(10_500);
        let relay = Duration::from_secs(12);

        let (code, message) =
            route_bind_deadline_refusal(age, deadline, relay, &snapshot("queued"));
        assert_eq!(code, BIND_BLOCKED_BY_READER);
        assert!(
            message.contains("job=subc-1-789 tool=grep age_ms=840000")
                && message.contains("and 1 other reader(s)"),
            "{message}"
        );

        // A bind whose configure is still running when the deadline passes was
        // not held back by readers, so it keeps the generic code.
        let (code, message) =
            route_bind_deadline_refusal(age, deadline, relay, &snapshot("running"));
        assert_eq!(code, "actor_not_ready");
        assert!(message.contains("configure did not finish"), "{message}");
    }

    #[test]
    fn pending_bind_breadcrumb_does_not_claim_running_configure_has_no_io_waits() {
        let (_dir, root) = test_root("breadcrumb-running");
        let breadcrumb = pending_bind_breadcrumb(
            RouteChannel {
                channel: 7,
                epoch: 1,
            },
            &root,
            Duration::from_secs(6),
            "subc-bind-7",
            &BindBlockerSnapshot {
                configure_state: "running",
                configure_phase_timings: Some(
                    "standing_bind=6000ms(reconciliation_lock=6000ms)".into(),
                ),
                blockers: Vec::new(),
                oldest_queued_writer_age_ms: None,
                in_flight_readers: Vec::new(),
                in_flight_writer: None,
                reader_admissions_while_promoted_writer_waited: 0,
            },
        );
        assert!(breadcrumb.contains("admission_blockers=[none (scheduler admission only; executing configure may wait on I/O or locks)]"), "{breadcrumb}");
        assert!(breadcrumb.contains("standing_bind=6000ms(reconciliation_lock=6000ms)"));
    }

    #[test]
    fn pending_bind_breadcrumb_names_every_blocker_class() {
        let (_dir, root) = test_root("breadcrumb-blockers");
        let cases = [
            "queued_behind_configure(2)",
            "queued_behind_maintenance(job=subc-maintenance-drain-watcher lane=Mutating root=/tmp/a age_ms=1)",
            "waiting_on_readers",
            "idle_workers==0(job=subc-bind-other lane=Mutating root=/tmp/b age_ms=2)",
        ];

        for blocker in cases {
            let breadcrumb = pending_bind_breadcrumb(
                RouteChannel {
                    channel: 7,
                    epoch: 1,
                },
                &root,
                Duration::from_secs(6),
                "subc-bind-7",
                &BindBlockerSnapshot {
                    configure_state: "queued",
                    configure_phase_timings: Some("artifact_owner_claim=12ms".to_string()),
                    blockers: vec![blocker.to_string()],
                    oldest_queued_writer_age_ms: Some(6_000),
                    in_flight_readers: Vec::new(),
                    in_flight_writer: None,
                    reader_admissions_while_promoted_writer_waited: 0,
                },
            );
            assert!(
                breadcrumb.contains(blocker),
                "breadcrumb omitted blocker class: {breadcrumb}"
            );
            assert!(
                breadcrumb.contains("configure_phase_timings=[artifact_owner_claim=12ms]"),
                "breadcrumb omitted configure phase timings: {breadcrumb}"
            );
        }
    }

    #[test]
    fn callgraph_repair_rate_is_always_present_and_hot_root_scoped() {
        let executor = Executor::with_config(crate::executor::ExecutorConfig {
            pool_size: 1,
            read_cap: 1,
            actor_cap: 1,
            heavy_permits: 1,
            drr_quantum: 1,
        });
        let (_dir, root) = test_root("health-callgraph-repair-rate");
        assert!(executor.register_actor(root.clone(), test_ctx()));
        let metrics = DispatchPathMetrics::new();
        let app = crate::context::App::default_shared();
        // The production annotation path reads the derivation map only
        // (never spawns git), so derive the key the way configure would —
        // artifact_cache_key records it as a side effect.
        let project_key = crate::search_index::artifact_cache_key(root.as_path());

        let quiet = test_health_report(&executor, &HashMap::new(), &metrics, &app);
        let quiet_metrics = quiet.metrics.expect("quiet health metrics");
        assert_eq!(
            quiet_metrics["callgraph_repair_entries_60s_total"].as_u64(),
            Some(0)
        );
        assert!(quiet_metrics["callgraph_commits_60s_total"].is_u64());
        assert!(quiet_metrics["callgraph_pages_or_bytes_written_60s_total"].is_u64());
        assert!(quiet_metrics["roots"][0]
            .get("callgraph_commits_60s")
            .is_none());
        assert!(quiet_metrics["roots"][0]
            .get("callgraph_repair_entries_60s")
            .is_none());

        for _ in 0..3 {
            crate::callgraph_store::note_repair_entry(&project_key);
        }
        let hot = test_health_report(&executor, &HashMap::new(), &metrics, &app);
        let hot_metrics = hot.metrics.expect("hot health metrics");
        assert_eq!(
            hot_metrics["callgraph_repair_entries_60s_total"].as_u64(),
            Some(3)
        );
        assert_eq!(
            hot_metrics["roots"][0]["callgraph_repair_entries_60s"].as_u64(),
            Some(3)
        );

        crate::callgraph_store::expire_repair_entry_window_for_test(&project_key);
        let decayed = test_health_report(&executor, &HashMap::new(), &metrics, &app);
        let decayed_metrics = decayed.metrics.expect("decayed health metrics");
        assert_eq!(
            decayed_metrics["callgraph_repair_entries_60s_total"].as_u64(),
            Some(0)
        );
        assert!(decayed_metrics["roots"][0]
            .get("callgraph_repair_entries_60s")
            .is_none());
    }

    #[test]
    fn health_report_exposes_stall_watchdog_counters() {
        let executor = Executor::new();
        let metrics = Arc::new(DispatchPathMetrics::new());
        let app = App::default_shared();

        let report = test_health_report(&executor, &HashMap::new(), &metrics, &app);
        let stall = &report.metrics.expect("health metrics")["dispatch_path"]["stall_watchdog"];
        assert_eq!(stall["stall_count"].as_u64(), Some(0), "{stall}");
        assert!(stall["last_stall_duration_ms"].is_null(), "{stall}");
        assert_eq!(stall["active_stalls"].as_u64(), Some(0), "{stall}");
        assert_eq!(stall["captures"].as_u64(), Some(0), "{stall}");
    }

    #[test]
    fn health_report_exposes_live_durable_log_counters() {
        let executor = Executor::new();
        let metrics = DispatchPathMetrics::new();
        let app = App::default_shared();
        let report = build_health_report(
            &HealthRollupCache::new(),
            &executor,
            &HashMap::new(),
            &metrics,
            &app,
        );
        let health = &report.metrics.expect("health metrics")["durable_log"];
        assert!(health["dropped_lines_total"].is_u64(), "{health}");
        assert!(health.get("last_write_error").is_some(), "{health}");
        assert!(health.get("failing_since").is_some(), "{health}");
        assert!(
            health["stderr_mirror"]["dropped_lines_total"].is_u64(),
            "{health}"
        );
        assert!(
            health["stderr_mirror"]["dropped_flushes_total"].is_u64(),
            "{health}"
        );
        assert!(
            health["stderr_mirror"]["best_effort"].is_boolean(),
            "{health}"
        );
    }

    #[test]
    fn health_report_lists_stopped_routes() {
        let executor = Executor::new();
        let metrics = DispatchPathMetrics::new();
        let app = App::default_shared();
        let report = build_health_report(
            &HealthRollupCache::new(),
            &executor,
            &HashMap::new(),
            &metrics,
            &app,
        );
        let stopped = &report.metrics.expect("health metrics")["stopped_routes"];
        assert!(stopped.is_array(), "{stopped}");
    }

    #[test]
    fn health_report_exposes_deferred_bash_waits_in_flight() {
        let executor = Executor::new();
        let metrics = Arc::new(DispatchPathMetrics::new());
        let app = App::default_shared();
        let guard = DeferredBashWaitGuard::new(&metrics);

        let report = test_health_report(&executor, &HashMap::new(), &metrics, &app);
        assert_eq!(
            report
                .metrics
                .as_ref()
                .and_then(
                    |metrics| metrics["dispatch_path"]["deferred_bash_waits_in_flight"].as_u64()
                ),
            Some(1)
        );

        drop(guard);
        let report = test_health_report(&executor, &HashMap::new(), &metrics, &app);
        assert_eq!(
            report
                .metrics
                .as_ref()
                .and_then(
                    |metrics| metrics["dispatch_path"]["deferred_bash_waits_in_flight"].as_u64()
                ),
            Some(0)
        );
    }

    #[test]
    fn health_report_includes_nonblocking_dispatch_liveness_for_queued_interactive() {
        let executor = Executor::with_config(crate::executor::ExecutorConfig {
            pool_size: 2,
            read_cap: 1,
            actor_cap: 1,
            heavy_permits: 2,
            drr_quantum: 1,
        });
        let (_dir_a, root_a) = test_root("health-liveness-a");
        let (_dir_b, root_b) = test_root("health-liveness-b");
        let (_dir_c, root_c) = test_root("health-liveness-c");
        executor.register_actor(root_a.clone(), test_ctx());
        executor.register_actor(root_b.clone(), test_ctx());
        executor.register_actor(root_c.clone(), test_ctx());

        let (started_tx, started_rx) = crossbeam_channel::bounded(2);
        let (release_tx, release_rx) = crossbeam_channel::bounded(2);
        let mut blockers = Vec::new();
        for (index, root) in [root_a, root_b].into_iter().enumerate() {
            let started_tx = started_tx.clone();
            let release_rx = release_rx.clone();
            blockers.push(executor.submit(
                root,
                Lane::PureRead,
                format!("health-blocker-{index}"),
                Box::new(move |_| {
                    started_tx.send(index).expect("signal blocker start");
                    release_rx
                        .recv_timeout(Duration::from_secs(2))
                        .expect("release blocker");
                    Response::success(format!("blocker-{index}"), json!({ "ok": true }))
                }),
            ));
        }
        for _ in 0..2 {
            started_rx
                .recv_timeout(Duration::from_secs(1))
                .expect("blocker starts");
        }

        let queued = executor.submit(
            root_c,
            Lane::PureRead,
            "queued-interactive".to_string(),
            Box::new(|_| Response::success("queued-interactive", json!({ "ok": true }))),
        );
        std::thread::sleep(Duration::from_millis(75));

        let metrics = DispatchPathMetrics::new();
        let pending_binds = HashMap::new();
        let report = test_health_report(
            &executor,
            &pending_binds,
            &metrics,
            &crate::context::App::default_shared(),
        );
        let dispatch = report
            .metrics
            .as_ref()
            .and_then(|metrics| metrics.get("dispatch_liveness"))
            .expect("dispatch_liveness metric");
        assert_eq!(dispatch.get("scheduler_busy"), None);
        assert_eq!(
            report
                .metrics
                .as_ref()
                .and_then(
                    |metrics| metrics["dispatch_path"]["deferred_bash_waits_in_flight"].as_u64()
                ),
            Some(0)
        );
        assert_eq!(dispatch["interactive"]["queued"].as_u64(), Some(1));
        assert_eq!(
            dispatch["running"]["phantom"]["interactive"].as_u64(),
            Some(0)
        );
        assert_eq!(
            dispatch["running"]["phantom"]["maintenance"].as_u64(),
            Some(0)
        );
        assert_eq!(dispatch["phantom_running_maintenance"].as_u64(), Some(0));
        assert!(dispatch["interactive"]["oldest_age_ms"].as_u64().is_some());

        for _ in 0..2 {
            release_tx.send(()).expect("release blocker");
        }
        for blocker in blockers {
            blocker
                .recv_timeout(Duration::from_secs(1))
                .expect("blocker completion response");
        }
        queued
            .recv_timeout(Duration::from_secs(1))
            .expect("queued completion response");
    }

    #[test]
    fn health_metrics_memory_rollup_reports_per_root_and_process_totals() {
        let executor = Executor::with_config(crate::executor::ExecutorConfig {
            pool_size: 1,
            read_cap: 1,
            actor_cap: 1,
            heavy_permits: 1,
            drr_quantum: 1,
        });
        let dispatch_path_metrics = Arc::new(DispatchPathMetrics::new());
        let app = crate::context::App::default_shared();
        let registry = app.lsp_child_registry();
        registry.track(std::process::id());
        let report = test_health_report(&executor, &HashMap::new(), &dispatch_path_metrics, &app);
        let metrics = report.metrics.expect("health metrics present");
        let memory = metrics.get("memory").expect("memory rollup present");
        // No actors registered: ready rollup with zero roots and process totals.
        assert_eq!(memory.get("status").and_then(Value::as_str), Some("ready"));
        assert_eq!(memory.get("roots_total").and_then(Value::as_u64), Some(0));
        assert!(memory.get("total_attributed_bytes").is_some());
        assert!(memory.get("rss_bytes").is_some());
        assert!(memory
            .get("allocator_slack_bytes")
            .is_some_and(Value::is_u64));
        assert!(memory
            .get("allocator_slack_measured")
            .is_some_and(Value::is_boolean));
        assert_eq!(memory["allocator_slack_kind"], ALLOCATOR_SLACK_KIND);
        assert!(memory["last_relief_allocator_accounting_bytes"].is_u64());
        assert!(memory.get("last_relief_rss_drop_bytes").is_some());
        if memory
            .get("allocator_slack_measured")
            .and_then(Value::as_bool)
            .is_some_and(|measured| !measured)
        {
            assert_eq!(memory["allocator_slack_bytes"], 0);
        }
        // Lifecycle audit counters ride the same probe so operator drill-downs
        // and fleet leak checks read one surface.
        let runtime = metrics.get("runtime").expect("runtime counters present");
        for key in ["live_watchers", "live_actor_roots", "open_routes"] {
            assert!(
                runtime.get(key).and_then(Value::as_u64).is_some(),
                "runtime.{key} must be a number"
            );
        }
        assert_eq!(runtime["spawned_lsp_children"].as_u64(), Some(1));
        assert_eq!(runtime["lsp_children_with_deleted_cwd"].as_u64(), Some(0));

        let busy_memory = memory_rollup_metrics(None);
        assert_eq!(busy_memory["allocator_slack_bytes"], Value::Null);
        assert_eq!(
            busy_memory["allocator_slack_measured"].as_bool(),
            Some(false)
        );
        registry.untrack(std::process::id());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_allocator_relief_smoke_keeps_health_fields_present() {
        let mut allocation = vec![0u8; 32 * 1024 * 1024];
        for byte in allocation.iter_mut().step_by(4096) {
            *byte = 1;
        }
        std::hint::black_box(&allocation);
        drop(allocation);
        let _relief = crate::memory::relieve_allocator_pressure();

        let executor = Executor::with_config(crate::executor::ExecutorConfig {
            pool_size: 1,
            read_cap: 1,
            actor_cap: 1,
            heavy_permits: 1,
            drr_quantum: 1,
        });
        let app = crate::context::App::default_shared();
        let report = test_health_report(
            &executor,
            &HashMap::new(),
            &DispatchPathMetrics::new(),
            &app,
        );
        let memory = report
            .metrics
            .expect("health metrics present")
            .get("memory")
            .cloned()
            .expect("memory rollup present");
        assert!(memory["allocator_slack_bytes"].is_u64());
        assert!(memory["allocator_slack_measured"].is_boolean());
    }

    #[test]
    fn health_snapshot_fast_fails_while_mutating_job_holds_component_lock() {
        let executor = Executor::with_config(crate::executor::ExecutorConfig {
            pool_size: 1,
            read_cap: 1,
            actor_cap: 1,
            heavy_permits: 1,
            drr_quantum: 1,
        });
        let (_dir, root) = test_root("health-mutating-lock");
        let ctx = test_ctx();
        executor.register_actor(root.clone(), Arc::clone(&ctx));
        let (started_tx, started_rx) = crossbeam_channel::bounded(1);
        let (release_tx, release_rx) = crossbeam_channel::bounded(1);
        let blocker = executor.submit(
            root,
            Lane::Mutating,
            "health-lock-blocker".to_string(),
            Box::new(move |ctx| {
                let _index = ctx
                    .search_index()
                    .write()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                started_tx.send(()).expect("signal held health lock");
                release_rx.recv().expect("release held health lock");
                Response::success("health-lock-blocker", json!({ "ok": true }))
            }),
        );
        started_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("mutating lock holder starts");

        let report = test_health_report(
            &executor,
            &HashMap::new(),
            &DispatchPathMetrics::new(),
            &crate::context::App::default_shared(),
        );

        // Settle the holder before asserting so a verdict failure cannot drop the release
        // sender and make the worker panic while the test unwinds.
        release_tx.send(()).expect("release held health lock");
        blocker
            .recv_timeout(Duration::from_secs(3))
            .expect("mutating lock holder completes");

        assert_eq!(report.status, HealthStatus::Degraded);
    }

    #[test]
    fn health_retention_reports_fit_budget_with_exact_omissions_and_totals() {
        let reports = (0..128)
            .map(|index| {
                (
                    format!(
                        "/storage/long-checkout-name-{index:04}/{}",
                        "subdirectory/".repeat(12)
                    ),
                    crate::storage_retention::SweepReport {
                        examined: 17,
                        removed_roots: 3,
                        removed_bytes: 4096,
                        errors: vec!["example maintenance error".repeat(8)],
                        trash: crate::storage_retention::TrashReport {
                            at_ms: crate::pins::now_ms(),
                            entries: 2,
                            bytes_estimate: 1_500_000_000,
                            oldest_age_ms: Some(7_200_000),
                            removed_entries: 1,
                            incomplete: true,
                            kept_by_reason: std::collections::BTreeMap::from([(
                                "witness".into(),
                                2,
                            )]),
                            ..Default::default()
                        },
                        ..Default::default()
                    },
                )
            })
            .collect::<std::collections::BTreeMap<_, _>>();
        let totals = retention_report_totals(&reports);
        let mut metrics = json!({
            "roots": [],
            "storage_retention": reports,
            "storage_retention_totals": totals,
            "storage_retention_reports_omitted": 0,
        })
        .as_object()
        .unwrap()
        .clone();
        budget_health_metrics(&mut metrics);
        let retained = metrics["storage_retention"].as_object().unwrap().len() as u64;
        let omitted = metrics["storage_retention_reports_omitted"]
            .as_u64()
            .unwrap();
        assert!(omitted > 0, "the fixture must exceed the detail budget");
        assert_eq!(retained + omitted, 128);
        assert_eq!(metrics["storage_retention_totals"]["reports"], 128);
        assert_eq!(metrics["storage_retention_totals"]["examined"], 2176);
        assert_eq!(metrics["storage_retention_totals"]["removed_roots"], 384);
        assert_eq!(metrics["storage_retention_totals"]["removed_bytes"], 524288);
        assert_eq!(metrics["storage_retention_totals"]["errors"], 128);
        let trash = &metrics["storage_retention_totals"]["trash"];
        assert_eq!(trash["entries"], 256);
        assert_eq!(trash["bytes_estimate"], 192_000_000_000_u64);
        assert_eq!(trash["removed_entries"], 128);
        assert_eq!(trash["kept_by_reason"]["witness"], 256);
        assert_eq!(trash["incomplete"], true);
        assert!(trash["oldest_age_ms"].as_u64().unwrap() >= 7_200_000);
        let encoded = serde_json::to_vec(&metrics).unwrap();
        assert!(encoded.len() <= 12 * 1024);
        assert_eq!(metrics["metrics_bytes"], encoded.len());
    }

    #[test]
    fn cached_health_reply_is_fixed_cost_and_caps_root_detail_before_assembly() {
        fn fixture(root_count: usize) -> (Executor, Vec<tempfile::TempDir>) {
            let executor = Executor::with_config(crate::executor::ExecutorConfig {
                pool_size: 1,
                read_cap: 1,
                actor_cap: 64,
                heavy_permits: 1,
                drr_quantum: 1,
            });
            let mut dirs = Vec::with_capacity(root_count);
            for index in 0..root_count {
                let (dir, root) = test_root(&format!("health-cost-{root_count}-{index:02}"));
                assert!(executor.register_actor(root, test_ctx()));
                dirs.push(dir);
            }
            (executor, dirs)
        }

        let app = crate::context::App::default_shared();
        let metrics = DispatchPathMetrics::new();
        let (five, five_dirs) = fixture(5);
        let five_cache = HealthRollupCache::new();
        refresh_until_root_count(&five_cache, &five, &app, 5);
        let five_work = five_cache.root_details_assembled_for_test();
        let five_bytes = serde_json::to_vec(&build_health_report(
            &five_cache,
            &five,
            &HashMap::new(),
            &metrics,
            &app,
        ))
        .expect("serialize five-root health report")
        .len();

        let (fifty, fifty_dirs) = fixture(50);
        let fifty_cache = HealthRollupCache::new();
        refresh_until_root_count(&fifty_cache, &fifty, &app, 50);
        let fifty_work = fifty_cache.root_details_assembled_for_test();
        let report = build_health_report(&fifty_cache, &fifty, &HashMap::new(), &metrics, &app);
        let fifty_bytes = serde_json::to_vec(&report)
            .expect("serialize fifty-root health report")
            .len();
        let report_metrics = report.metrics.expect("health metrics");

        assert_eq!(report_metrics["root_count"].as_u64(), Some(50));
        let rendered_roots = report_metrics["roots"]
            .as_array()
            .expect("rendered root details")
            .len();
        let omitted_roots = report_metrics["root_details_omitted"]
            .as_u64()
            .expect("omitted root count") as usize;
        assert_eq!(rendered_roots + omitted_roots, 50);
        assert!(rendered_roots <= HEALTH_ROOT_DETAIL_CAP);
        assert_eq!(five_work, 5);
        assert_eq!(
            fifty_work, HEALTH_ROOT_DETAIL_CAP,
            "cached root detail assembly scaled with roots"
        );
        assert!(report_metrics["metrics_bytes"]
            .as_u64()
            .is_some_and(|bytes| bytes <= HEALTH_METRICS_BUDGET_BYTES as u64));
        // Each test context has its own storage, so there are fifty distinct
        // cache roots; the reply lists a bounded number and counts the rest.
        let listed_cache_roots = report_metrics["cache_roots"]
            .as_array()
            .expect("cache roots")
            .len();
        assert!(listed_cache_roots <= HEALTH_ROOT_DETAIL_CAP);
        assert_eq!(
            listed_cache_roots as u64
                + report_metrics["cache_roots_omitted"]
                    .as_u64()
                    .expect("omitted cache root count"),
            50
        );
        assert_eq!(report_metrics["memory"]["roots_total"].as_u64(), Some(50));
        assert_eq!(
            report_metrics["memory"]["roots"]
                .as_object()
                .map(serde_json::Map::len),
            Some(8)
        );
        assert!(
            fifty_bytes <= five_bytes.saturating_mul(2),
            "cached reply payload scaled with roots: five={five_bytes}, fifty={fifty_bytes}"
        );
        std::hint::black_box((five_dirs, fifty_dirs));
    }

    #[test]
    fn health_payload_exposes_root_local_query_embed_timeout_and_p50_metrics() {
        let executor = Executor::new();
        let (_dir, root) = test_root("semantic-query-embed-health");
        let ctx = test_ctx();
        crate::semantic_index::record_query_embed_observation_for_test(root.as_path(), 120, false);
        crate::semantic_index::record_query_embed_observation_for_test(root.as_path(), 200, true);
        crate::semantic_index::record_query_embed_observation_for_test(root.as_path(), 900, false);
        assert!(executor.register_actor(root.clone(), ctx));
        let app = App::default_shared();
        let metrics = DispatchPathMetrics::new();
        let cache = HealthRollupCache::new();

        refresh_until_root_count(&cache, &executor, &app, 1);
        let report = build_health_report(&cache, &executor, &HashMap::new(), &metrics, &app)
            .metrics
            .expect("health metrics");
        let semantic = &report["roots"][0]["semantic_index"];
        assert_eq!(semantic["query_embed_timeouts"], 1);
        assert_eq!(semantic["query_embed_p50_ms"], 200);
    }

    #[test]
    fn semantic_backend_outage_is_distinct_per_root_and_aggregated_once() {
        let executor = Executor::new();
        let (_dir, root) = test_root("semantic-backend-unavailable-health");
        let ctx = test_ctx();
        *ctx.semantic_index_status().write().unwrap() =
            crate::context::SemanticIndexStatus::Building {
                stage: "embedding_symbols".to_string(),
                files: Some(1),
                entries_done: Some(0),
                entries_total: Some(12),
            };
        ctx.trip_semantic_refresh_circuit(1, "connection refused by embedding backend");
        assert!(executor.register_actor(root.clone(), ctx));
        let app = App::default_shared();
        let metrics = DispatchPathMetrics::new();
        let cache = HealthRollupCache::new();

        refresh_until_root_count(&cache, &executor, &app, 1);
        let report = build_health_report(&cache, &executor, &HashMap::new(), &metrics, &app)
            .metrics
            .expect("health metrics");
        let semantic = &report["roots"][0]["semantic_index"];
        assert_eq!(semantic["status"], "backend_unavailable");
        assert_eq!(
            semantic["reason"],
            "connection refused by embedding backend"
        );
        assert!(semantic["since_ms"].is_u64());
        assert!(semantic["next_retry_ms"].is_u64());
        assert_eq!(report["embedding_backend"]["available"], false);
        assert_eq!(
            report["embedding_backend"]["last_error"],
            "connection refused by embedding backend"
        );
        assert!(report["embedding_backend"]["since_ms"].is_u64());
    }

    #[test]
    fn cold_semantic_retry_reports_backend_unavailable_with_retry_deadline() {
        let executor = Executor::new();
        let (_dir, root) = test_root("semantic-cold-backend-unavailable-health");
        let ctx = test_ctx();
        *ctx.semantic_index_status().write().unwrap() =
            crate::context::SemanticIndexStatus::Building {
                stage: "waiting_for_embedding_backend: connection refused".to_string(),
                files: None,
                entries_done: None,
                entries_total: None,
            };
        crate::semantic_index::record_embedding_backend_build_failure_for_test(
            root.as_path(),
            "connection refused",
        );
        assert!(executor.register_actor(root.clone(), ctx));
        let app = App::default_shared();
        let metrics = DispatchPathMetrics::new();
        let cache = HealthRollupCache::new();

        refresh_until_root_count(&cache, &executor, &app, 1);
        let report = build_health_report(&cache, &executor, &HashMap::new(), &metrics, &app)
            .metrics
            .expect("health metrics");
        let semantic = &report["roots"][0]["semantic_index"];
        assert_eq!(semantic["status"], "backend_unavailable");
        assert_eq!(semantic["reason"], "connection refused");
        assert!(semantic["since_ms"].is_u64());
        assert!(semantic["next_retry_ms"].as_u64() > semantic["since_ms"].as_u64());
        assert_eq!(report["embedding_backend"]["available"], false);
        assert_eq!(
            report["embedding_backend"]["last_error"],
            "connection refused"
        );
    }

    #[test]
    fn health_payload_carries_live_semantic_build_progress_only_while_running() {
        let executor = Executor::new();
        let (_dir, root) = test_root("semantic-build-progress-health");
        let ctx = test_ctx();
        let progress = crate::context::SemanticBuildProgress::default();
        progress.report(6, 12, 3);
        ctx.set_semantic_build_progress(Some(progress));
        *ctx.semantic_index_status().write().unwrap() =
            crate::context::SemanticIndexStatus::Building {
                stage: "embedding_symbols".to_string(),
                files: Some(1),
                entries_done: Some(6),
                entries_total: Some(12),
            };
        assert!(executor.register_actor(root.clone(), ctx.clone()));
        let app = App::default_shared();
        let metrics = DispatchPathMetrics::new();
        let cache = HealthRollupCache::new();
        refresh_until_root_count(&cache, &executor, &app, 1);
        let building = build_health_report(&cache, &executor, &HashMap::new(), &metrics, &app)
            .metrics
            .expect("health metrics");
        let semantic = &building["roots"][0]["semantic_index"];
        assert_eq!(semantic["status"], "building");
        assert_eq!(semantic["stage"], "embedding_symbols");
        assert_eq!(semantic["embedded_chunks"], 6);
        assert_eq!(semantic["total_chunks"], 12);
        assert_eq!(semantic["current_batch"], 2);
        assert_eq!(semantic["total_batches"], 4);

        ctx.set_semantic_build_progress(None);
        *ctx.semantic_index_status().write().unwrap() =
            crate::context::SemanticIndexStatus::ready();
        refresh_until_root_count(&cache, &executor, &app, 1);
        let ready = build_health_report(&cache, &executor, &HashMap::new(), &metrics, &app)
            .metrics
            .expect("health metrics");
        assert!(ready["roots"][0]["semantic_index"]
            .get("embedded_chunks")
            .is_none());
        assert!(ready["roots"][0]["semantic_index"]
            .get("total_chunks")
            .is_none());
    }

    #[test]
    fn completed_tier2_pass_returns_plane_to_ready_and_advances_progress() {
        let executor = Executor::new();
        let (_dir, root) = test_root("tier2-pass-health-transition");
        let mut config = crate::config::Config::default();
        config.project_root = Some(root.as_path().to_path_buf());
        config.indexes.trigram = false;
        config.indexes.semantic = false;
        config.indexes.callgraph = false;
        config.inspect.enabled = true;
        let ctx = Arc::new(AppContext::new(
            Box::new(crate::parser::TreeSitterProvider::new()),
            config,
        ));
        ctx.update_status_bar_tier2(Some(1), Some(2), Some(3), Some(4), false);
        ctx.mark_status_bar_tier2_stale();
        ctx.inspect_manager()
            .set_tier2_in_flight_for_test(crate::inspect::InspectCategory::Duplicates, true);
        assert!(executor.register_actor(root.clone(), Arc::clone(&ctx)));
        let app = App::default_shared();
        let dispatch = DispatchPathMetrics::new();
        let cache = HealthRollupCache::new();

        refresh_until_root_count(&cache, &executor, &app, 1);
        let building = build_health_report(&cache, &executor, &HashMap::new(), &dispatch, &app)
            .metrics
            .expect("building health metrics");
        assert_eq!(building["roots"][0]["tier2"]["status"], "building");
        let building_progress = building["roots"][0]["tier2"]["last_progress_at_ms"]
            .as_u64()
            .expect("building progress timestamp");

        std::thread::sleep(Duration::from_millis(2));
        ctx.inspect_manager().record_tier2_attempt_outcome_for_test(
            crate::inspect::InspectCategory::Duplicates,
            crate::inspect::JobOutcome::Fresh {
                payload: json!({ "count": 3 }),
            },
        );
        refresh_until_root_count(&cache, &executor, &app, 1);
        let ready = build_health_report(&cache, &executor, &HashMap::new(), &dispatch, &app)
            .metrics
            .expect("ready health metrics");
        assert_eq!(ready["roots"][0]["tier2"]["status"], "ready");
        assert!(
            ready["roots"][0]["tier2"]["last_progress_at_ms"]
                .as_u64()
                .expect("ready progress timestamp")
                > building_progress
        );
    }

    #[test]
    fn budgeted_health_metrics_fit_subc_headroom_with_many_roots() {
        let long_segment = "root-segment".repeat(18);
        let roots = (0..71)
            .map(|index| {
                json!({
                    "project_root": format!("/synthetic/{index:02}/{long_segment}"),
                    "state": "ready",
                    "search_index": { "status": "ready", "since_ms": 1, "last_progress_at_ms": 1 },
                    "semantic_index": { "status": "ready", "since_ms": 1, "last_progress_at_ms": 1 },
                    "tier2": { "status": "ready", "since_ms": 1, "last_progress_at_ms": 1 },
                })
            })
            .collect::<Vec<_>>();
        let ledger = (0..3)
            .map(|index| {
                json!({
                    "domain": "callgraph_refresh",
                    "root_id": format!("/very/long/root/{index}/{long_segment}"),
                    "logical_bytes": 1024,
                    "physical_bytes": 2048,
                })
            })
            .collect::<Vec<_>>();
        let mut metrics = json!({
            "actor_count": 71,
            "root_count": 71,
            "root_details_omitted": 0,
            "embedding_backend": { "available": true, "last_error": null, "since_ms": null },
            "write_ledger_top_10m": ledger,
            "roots": roots,
        })
        .as_object()
        .expect("metrics object")
        .clone();

        budget_health_metrics(&mut metrics);
        let encoded =
            serde_json::to_vec(&Value::Object(metrics.clone())).expect("encode budgeted metrics");
        assert!(
            encoded.len() <= 12 * 1024,
            "health metrics exceeded SUBC headroom: {} bytes",
            encoded.len()
        );
        assert_eq!(metrics["actor_count"], 71);
        assert_eq!(metrics["root_count"], 71);
        assert!(metrics["embedding_backend"].is_object());
        assert_eq!(
            metrics["write_ledger_top_10m"].as_array().map(Vec::len),
            Some(3)
        );
        assert_eq!(
            metrics["metrics_bytes"].as_u64(),
            Some(encoded.len() as u64)
        );
    }

    #[test]
    fn budget_compacts_write_ledger_roots_without_dropping_rows() {
        let long_prefix = "nested-root/".repeat(700);
        let mut metrics = json!({
            "actor_count": 0,
            "root_count": 0,
            "root_details_omitted": 0,
            "embedding_backend": { "available": false, "last_error": "connection refused", "since_ms": 1 },
            "write_ledger_top_10m": [
                { "domain": "callgraph_refresh", "root_id": format!("/{long_prefix}project-a"), "logical_bytes": 1, "physical_bytes": 2 },
                { "domain": "semantic_compaction", "root_id": format!("/{long_prefix}project-b"), "logical_bytes": 3, "physical_bytes": 4 },
            ],
            "roots": [],
        })
        .as_object()
        .expect("metrics object")
        .clone();

        budget_health_metrics(&mut metrics);
        let encoded =
            serde_json::to_vec(&Value::Object(metrics.clone())).expect("encode budgeted metrics");
        assert!(encoded.len() <= HEALTH_METRICS_BUDGET_BYTES);
        let rows = metrics["write_ledger_top_10m"]
            .as_array()
            .expect("write ledger rows");
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0]["root_id"], "project-a");
        assert_eq!(rows[1]["root_id"], "project-b");
        assert_eq!(metrics["embedding_backend"]["available"], false);
        assert_eq!(
            metrics["metrics_bytes"].as_u64(),
            Some(encoded.len() as u64)
        );
    }

    #[test]
    fn cached_health_reply_exposes_snapshot_age_and_counter_coverage() {
        let executor = Executor::with_config(crate::executor::ExecutorConfig {
            pool_size: 1,
            read_cap: 1,
            actor_cap: 1,
            heavy_permits: 1,
            drr_quantum: 1,
        });
        let (_dir, root) = test_root("health-snapshot-age-coverage");
        let ctx = test_ctx();
        let watcher = ctx.watcher_counters();
        watcher.set_exclusion_planning(crate::context::WatcherExclusionPlanningSnapshot {
            matcher_generation: 7,
            attempts: 8,
            attempt_limit: 8,
            status: "exhausted",
            last_failure: Some("io_deadline"),
        });
        watcher.note_fsevents_stream_creation();
        watcher.note_raw_event();
        watcher.note_raw_event();
        watcher.note_invalidating_event();
        watcher.note_paths_after_gitignore(7);
        watcher.note_paths_dispatched(6);
        watcher.note_overflow(
            crate::watcher_filter::RescanReason::UserDropped,
            vec![crate::context::WatcherOverflowPrefix {
                prefix: "packages/opencode-plugin".to_string(),
                count: 23,
            }],
        );
        assert_eq!(
            watcher
                .begin_rescan(crate::watcher_filter::RescanReason::UserDropped)
                .raw_events,
            2
        );
        watcher.finish_rescan(42, Some(-4096));
        watcher.note_raw_event();
        assert!(executor.register_actor(root.clone(), ctx));
        let app = crate::context::App::default_shared();
        let metrics = DispatchPathMetrics::new();
        let cache = HealthRollupCache::new();
        refresh_until_root_count(&cache, &executor, &app, 1);
        std::thread::sleep(Duration::from_millis(2));

        let absent = build_health_report(&cache, &executor, &HashMap::new(), &metrics, &app)
            .metrics
            .expect("health metrics");
        assert!(absent["snapshot_age_ms"].is_u64());
        assert_eq!(absent["callgraph_repair_roots_annotated"].as_u64(), Some(0));
        assert_eq!(absent["callgraph_repair_roots_total"].as_u64(), Some(1));
        let watcher = &absent["roots"][0]["watcher"];
        assert_eq!(watcher["exclusion_planning"]["status"], "exhausted");
        assert_eq!(watcher["exclusion_planning"]["matcher_generation"], 7);
        assert_eq!(watcher["exclusion_planning"]["attempts"], 8);
        assert_eq!(watcher["exclusion_planning"]["attempt_limit"], 8);
        assert_eq!(watcher["exclusion_planning"]["last_failure"], "io_deadline");
        assert_eq!(watcher["raw_events_total"].as_u64(), Some(3));
        assert_eq!(watcher["raw_events_since_last_rescan"].as_u64(), Some(1));
        assert_eq!(watcher["invalidating_events_total"].as_u64(), Some(1));
        assert_eq!(watcher["paths_after_gitignore_total"].as_u64(), Some(7));
        assert_eq!(watcher["paths_dispatched_total"].as_u64(), Some(6));
        assert_eq!(watcher["fsevents_stream_creations_total"].as_u64(), Some(1));
        assert_eq!(watcher["overflows_total"].as_u64(), Some(1));
        assert_eq!(watcher["overflows_during_rescan"].as_u64(), Some(0));
        assert_eq!(
            watcher["last_overflow_prefixes"][0]["prefix"].as_str(),
            Some("packages/opencode-plugin")
        );
        assert_eq!(
            watcher["last_overflow_prefixes"][0]["count"].as_u64(),
            Some(23)
        );
        assert_eq!(watcher["rescans_kernel_dropped_total"].as_u64(), Some(0));
        assert_eq!(watcher["rescans_user_dropped_total"].as_u64(), Some(1));
        assert_eq!(watcher["rescans_unknown_total"].as_u64(), Some(0));
        assert!(watcher["last_rescan_at_ms"].is_u64());
        assert_eq!(watcher["last_rescan_cost_ms"].as_u64(), Some(42));
        assert_eq!(watcher["last_rescan_rss_delta_bytes"].as_i64(), Some(-4096));

        let _project_key = crate::search_index::artifact_cache_key(root.as_path());
        refresh_until_root_count(&cache, &executor, &app, 1);
        let measured_zero = build_health_report(&cache, &executor, &HashMap::new(), &metrics, &app)
            .metrics
            .expect("health metrics");
        assert_eq!(
            measured_zero["callgraph_repair_roots_annotated"].as_u64(),
            Some(1)
        );
        assert_eq!(
            measured_zero["callgraph_repair_roots_total"].as_u64(),
            Some(1)
        );
        assert!(measured_zero["roots"][0]
            .get("callgraph_repair_entries_60s")
            .is_none());
    }

    #[test]
    #[ignore = "manual production-shape health profiling harness"]
    fn profile_fifty_root_cached_health_reply() {
        let executor = Executor::with_config(crate::executor::ExecutorConfig {
            pool_size: 1,
            read_cap: 1,
            actor_cap: 64,
            heavy_permits: 1,
            drr_quantum: 1,
        });
        let mut dirs = Vec::new();
        for index in 0..50 {
            let (dir, root) = test_root(&format!("health-profile-{index:02}"));
            dirs.push(dir);
            assert!(executor.register_actor(root, test_ctx()));
        }
        let app = crate::context::App::default_shared();
        let metrics = DispatchPathMetrics::new();
        let cache = HealthRollupCache::new();
        refresh_until_root_count(&cache, &executor, &app, 50);
        let median = cached_reply_median(&cache, &executor, &metrics, &app);
        std::hint::black_box(dirs);
        eprintln!(
            "health-profile roots=50 cached_health_check_reply_us={}",
            median.as_micros()
        );
    }

    fn standing_config(root: &std::path::Path, storage: &std::path::Path) -> crate::config::Config {
        crate::config::Config {
            project_root: Some(root.to_path_buf()),
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

    fn register_standing_health_actor(
        executor: &Executor,
        app: &Arc<App>,
        root: &std::path::Path,
        config: crate::config::Config,
    ) {
        let root_id = ProjectRootId::from_path(root).expect("test root identity");
        let ctx = Arc::new(crate::context::AppContext::from_app(
            Arc::clone(app),
            config,
        ));
        ctx.set_canonical_cache_root(root.to_path_buf());
        assert!(executor.register_actor(root_id, ctx));
    }

    fn assert_standing_health_preserves_legacy_memo(old_entry: bool) {
        assert!(crate::developer_tools::git_usable());
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("repository");
        let storage = directory.path().join("storage");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir(&storage).unwrap();
        std::fs::write(root.join("lib.rs"), "pub fn health_fixture() {}\n").unwrap();
        let git = |args: &[&str]| {
            let mut command = std::process::Command::new("git");
            crate::test_env::apply_hermetic_git_env(command.current_dir(&root));
            let output = command.args(args).output().unwrap();
            assert!(
                output.status.success(),
                "health fixture git: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            output
        };
        git(&["init", "-q"]);
        git(&["add", "."]);
        git(&[
            "-c",
            "user.name=Health fixture",
            "-c",
            "user.email=fixture@example.invalid",
            "commit",
            "-q",
            "-m",
            "health fixture",
        ]);
        let root = std::fs::canonicalize(&root).unwrap();
        let commit = String::from_utf8(git(&["rev-parse", "HEAD"]).stdout)
            .unwrap()
            .trim()
            .to_string();
        let memo_root = if old_entry {
            root.clone()
        } else {
            directory.path().join("another-root")
        };
        std::fs::create_dir_all(&memo_root).unwrap();
        let memo = storage.join("cache-keys.json");
        std::fs::write(
            &memo,
            serde_json::to_vec(&json!({ (memo_root.to_string_lossy().to_string()): {
                "key": crate::search_index::artifact_key_from_git_identity(&commit),
                "git_root_commit": commit, "recorded_at_ms": 0
            }}))
            .unwrap(),
        )
        .unwrap();
        filetime::set_file_mtime(&memo, filetime::FileTime::from_unix_time(60, 0)).unwrap();
        let before = (
            std::fs::read(&memo).unwrap(),
            std::fs::metadata(&memo).unwrap().modified().unwrap(),
        );
        let app = App::default_shared();
        let executor = Executor::new();
        register_standing_health_actor(&executor, &app, &root, standing_config(&root, &storage));
        let metrics = DispatchPathMetrics::new();
        let report = test_health_report(&executor, &HashMap::new(), &metrics, &app);
        let report = report.metrics.unwrap();
        let row = report["roots"]
            .as_array()
            .unwrap()
            .iter()
            .find(|row| row["standing_entry"] == root.display().to_string())
            .unwrap();
        assert!(
            row.get("standing_refusal").is_none(),
            "the fixture must complete identity observation"
        );
        let after = (
            std::fs::read(&memo).unwrap(),
            std::fs::metadata(&memo).unwrap().modified().unwrap(),
        );
        assert_eq!(
            after, before,
            "health changed legacy memo bytes or mtime (old entry: {old_entry})"
        );
        assert!(crate::root_identity::read(&root, &storage)
            .unwrap()
            .is_none());
    }

    #[test]
    fn standing_health_probe_does_not_write_legacy_memo() {
        assert_standing_health_preserves_legacy_memo(false);
    }

    #[test]
    fn standing_health_lookup_does_not_refresh_legacy_memo() {
        assert_standing_health_preserves_legacy_memo(true);
    }

    #[test]
    fn standing_health_and_memory_reuse_existing_per_root_tables() {
        let storage = tempfile::tempdir().expect("storage directory");
        let root = tempfile::tempdir().expect("standing root");
        let config = standing_config(root.path(), storage.path());
        let standing = crate::standing_roots::StandingRoots::default();
        standing
            .reconcile(&config)
            .expect("pin configured standing root");
        let entry = standing
            .entries()
            .into_iter()
            .next()
            .expect("configured standing entry");

        let app = App::default_shared();
        let executor = Executor::new();
        register_standing_health_actor(&executor, &app, root.path(), config.clone());
        let metrics = DispatchPathMetrics::new();
        let report = test_health_report(&executor, &HashMap::new(), &metrics, &app);
        let metrics = report.metrics.expect("health metrics");
        let root_row = metrics["roots"]
            .as_array()
            .and_then(|rows| {
                rows.iter()
                    .find(|row| row["standing_entry"] == json!(entry.literal_path))
            })
            .expect("standing health row");
        assert_eq!(root_row["standing"], true);
        assert!(root_row.get("standing_refusal").is_none());

        let memory = &metrics["memory"];
        assert_eq!(memory["roots_total"].as_u64(), Some(1));
        assert_eq!(memory["roots"][entry.artifact_key]["standing"], true);
        assert!(memory["roots"]
            .get(&root.path().display().to_string())
            .is_none());
    }

    #[cfg(unix)]
    #[test]
    fn standing_health_names_resolved_path_drift_without_re_recording() {
        let storage = tempfile::tempdir().expect("storage directory");
        let original = tempfile::tempdir().expect("original standing root");
        let retargeted = tempfile::tempdir().expect("retargeted standing root");
        let link = storage.path().join("standing-link");
        std::os::unix::fs::symlink(original.path(), &link).expect("initial standing symlink");
        let config = standing_config(&link, storage.path());
        let literal_path = config.index.roots[0].path.clone();
        let standing = crate::standing_roots::StandingRoots::default();
        standing
            .reconcile(&config)
            .expect("pin original resolved path");
        let before = {
            let conn = crate::db::open(&storage.path().join("aft.db")).expect("standing database");
            crate::db::standing_roots::get_standing_root(&conn, &literal_path)
                .expect("read original record")
                .expect("original record exists")
        };

        let app = App::default_shared();
        let executor = Executor::new();
        register_standing_health_actor(&executor, &app, original.path(), config.clone());
        std::fs::remove_file(&link).expect("replace standing symlink");
        std::os::unix::fs::symlink(retargeted.path(), &link).expect("retarget standing symlink");

        let metrics = DispatchPathMetrics::new();
        // A loaded runner can report scheduler contention instead of the root
        // rows; wait that out like the other health tests do.
        let report = health_report_after_scheduler_contention(
            "standing_health_names_resolved_path_drift_without_re_recording",
            || test_health_report(&executor, &HashMap::new(), &metrics, &app),
        );
        assert_eq!(report.status, HealthStatus::Degraded);
        let metrics = report.metrics.expect("health metrics");
        let root_row = metrics["roots"]
            .as_array()
            .and_then(|rows| {
                rows.iter()
                    .find(|row| row["standing_entry"] == json!(literal_path))
            })
            .expect("drifted standing health row");
        assert_eq!(root_row["standing"], true);
        let refusal = root_row["standing_refusal"]
            .as_str()
            .expect("named drift refusal");
        assert!(refusal.contains("resolved-path-drift refusal"));
        assert!(refusal.contains("resolved_target"));

        let conn = crate::db::open(&storage.path().join("aft.db")).expect("standing database");
        let after = crate::db::standing_roots::get_standing_root(&conn, &literal_path)
            .expect("read retained record")
            .expect("retained record exists");
        assert_eq!(after.resolved_target, before.resolved_target);
    }

    #[test]
    fn standing_health_preserves_durable_breaker_reason() {
        let storage = tempfile::tempdir().expect("storage directory");
        let root = tempfile::tempdir().expect("standing root");
        let config = standing_config(root.path(), storage.path());
        let standing = crate::standing_roots::StandingRoots::default();
        standing
            .reconcile(&config)
            .expect("pin configured standing root");
        let entry = standing
            .entries()
            .into_iter()
            .next()
            .expect("configured standing entry");
        let breaker_path = storage
            .path()
            .join("callgraph")
            .join(&entry.artifact_key)
            .join("build-breaker.sqlite");
        let breaker =
            crate::build_breaker::BuildDeathBreaker::open(breaker_path).expect("durable breaker");
        let breaker_root = ProjectRootId::from_path(root.path()).expect("standing root identity");
        let breaker_key = crate::build_breaker::BreakerKey::new(
            breaker_root.as_path().display().to_string(),
            crate::build_breaker::BuildDomain::CallgraphCold,
            "standing-health",
        );
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock after epoch")
            .as_millis() as u64;
        for _ in 0..3 {
            let crate::build_breaker::BreakerAdmission::Admitted(attempt) = breaker
                .admit_at(&breaker_key, 10, now_ms)
                .expect("breaker admission")
            else {
                panic!("breaker must admit before the third attributed death");
            };
            breaker
                .record_attributed_death_at(&breaker_key, &attempt.attempt_id, 10, 0, now_ms)
                .expect("record attributed death");
        }

        let app = App::default_shared();
        let executor = Executor::new();
        register_standing_health_actor(&executor, &app, root.path(), config);
        let metrics = DispatchPathMetrics::new();
        let report = test_health_report(&executor, &HashMap::new(), &metrics, &app);
        let metrics = report.metrics.expect("health metrics");
        let root_row = metrics["roots"]
            .as_array()
            .and_then(|rows| {
                rows.iter()
                    .find(|row| row["standing_entry"] == json!(entry.literal_path))
            })
            .expect("standing health row");
        assert_eq!(root_row["standing"], true);
        assert_eq!(
            root_row["suspended_domains"][0]["reason"],
            "zero_credit_death_limit"
        );
    }

    #[test]
    fn placeholder_memory_census_carries_process_io() {
        // A cache that has not refreshed yet serves the placeholder census.
        let census = HealthRollupCache::new().memory_census();
        assert_eq!(census["roots"], Value::Null, "{census}");
        for io in [&census["process_io"], &census["process"]["process_io"]] {
            assert!(io["available"].is_boolean(), "{census}");
            assert!(io["sampled_at_ms"].is_u64(), "{census}");
        }
    }

    #[test]
    fn health_report_carries_filesystem_helper_limits_and_replacements() {
        let report = test_health_report(
            &Executor::new(),
            &HashMap::new(),
            &DispatchPathMetrics::new(),
            &App::default_shared(),
        );
        let metrics = report.metrics.expect("health metrics");
        let helpers = &metrics["filesystem_helpers"];
        assert_eq!(helpers["pool_size"], 8);
        assert_eq!(helpers["thread_cap"], 64);
        match helpers["status"].as_str() {
            Some("unavailable") => {
                assert!(matches!(
                    helpers["reason"].as_str(),
                    Some("state_busy" | "state_poisoned")
                ));
                assert!(helpers.get("live").is_none());
                return;
            }
            Some("not_started" | "ready") => {}
            status => panic!("unexpected helper health status: {status:?}"),
        }
        for field in [
            "live",
            "idle",
            "usable",
            "abandoned",
            "spawned_total",
            "abandoned_total",
            "replacements_total",
            "cap_hits_total",
            "queue_timeouts_total",
        ] {
            assert!(
                helpers[field].is_u64(),
                "missing helper health field: {field}"
            );
        }
    }

    #[test]
    fn health_report_carries_process_io_and_write_ledger_top_writers() {
        let app = App::default_shared();
        let executor = Executor::new();
        let metrics = DispatchPathMetrics::new();
        let report = test_health_report(&executor, &HashMap::new(), &metrics, &app);
        let metrics = report.metrics.expect("health metrics");
        let io = metrics.get("process_io").expect("process_io field");
        assert!(io["available"].is_boolean());
        assert!(io["sampled_at_ms"].is_u64());
        if io["available"].as_bool() == Some(true) {
            assert!(io["diskio_bytes_read"].is_u64());
            assert!(io["diskio_bytes_written"].is_u64());
            assert!(io["logical_bytes_written"].is_u64());
        } else {
            assert!(io.get("diskio_bytes_read").is_none());
            assert!(io.get("diskio_bytes_written").is_none());
            assert!(io.get("logical_bytes_written").is_none());
        }
        assert!(metrics["write_ledger_top_10m"].is_array());
    }
}

#[derive(Clone)]
struct StandingHealthEntry {
    literal_path: String,
    root_id: Option<ProjectRootId>,
    artifact_key: Option<String>,
    refusal: Option<String>,
}

/// Read the configuration snapshot already held by an actor and inspect the
/// standing database without creating or reconciling any durable state. Health
/// must report a pinned-path refusal without turning the report itself into a
/// lifecycle owner.
fn standing_health_entries(
    actor_entries: &[(ProjectRootId, Arc<crate::context::AppContext>)],
) -> Vec<StandingHealthEntry> {
    let mut snapshots = actor_entries
        .iter()
        .map(|(root_id, ctx)| (root_id.clone(), ctx.config()))
        .collect::<Vec<_>>();
    snapshots.sort_by(|(left, _), (right, _)| left.as_path().cmp(right.as_path()));
    let Some((_, config)) = snapshots
        .iter()
        .find(|(_, config)| config.harness.is_some())
        .or_else(|| snapshots.first())
    else {
        return Vec::new();
    };

    let database_path =
        crate::bash_background::storage_dir(config.storage_dir.as_deref()).join("aft.db");
    let database = crate::db::open_readonly(&database_path).ok();
    let mut entries = std::collections::BTreeMap::new();
    for root in &config.index.roots {
        entries.entry(root.path.clone()).or_insert_with(|| {
            standing_health_entry(root, database.as_ref(), database_path.parent().unwrap())
        });
    }
    entries.into_values().collect()
}

fn standing_health_entry(
    root: &crate::config::IndexRootConfig,
    database: Option<&crate::db::TrackedConnection>,
    storage: &Path,
) -> StandingHealthEntry {
    let recorded = database
        .and_then(|database| {
            crate::db::standing_roots::get_standing_root(database, &root.path).ok()
        })
        .flatten();
    match crate::scoped_key::resolve_standing_root_in_storage(&root.path, storage, false) {
        Ok(resolved) => {
            let refusal = recorded
                .as_ref()
                .and_then(|record| resolved_path_drift_refusal(record, &resolved));
            let use_recorded_identity = refusal.is_some();
            let root_path = if use_recorded_identity {
                recorded
                    .as_ref()
                    .map(|record| record.resolved_target.as_str())
                    .unwrap_or(&resolved.resolved_target)
            } else {
                &resolved.resolved_target
            };
            let artifact_key = if use_recorded_identity {
                recorded
                    .as_ref()
                    .and_then(standing_artifact_key_from_record)
                    .or_else(|| Some(resolved.artifact_key.clone()))
            } else {
                Some(resolved.artifact_key)
            };
            StandingHealthEntry {
                literal_path: root.path.clone(),
                root_id: ProjectRootId::from_path(Path::new(root_path)).ok(),
                artifact_key,
                refusal,
            }
        }
        Err(error) => StandingHealthEntry {
            literal_path: root.path.clone(),
            root_id: recorded.as_ref().and_then(|record| {
                ProjectRootId::from_path(Path::new(&record.resolved_target)).ok()
            }),
            artifact_key: recorded
                .as_ref()
                .and_then(standing_artifact_key_from_record),
            refusal: Some(format!("standing root resolution failed: {error}")),
        },
    }
}

fn resolved_path_drift_refusal(
    recorded: &crate::db::standing_roots::StandingRootRecord,
    resolved: &crate::scoped_key::ResolvedStandingRoot,
) -> Option<String> {
    for (field, recorded_value, resolved_value) in [
        (
            "resolved_target",
            Some(recorded.resolved_target.clone()),
            Some(resolved.resolved_target.clone()),
        ),
        (
            "resolved_git_toplevel",
            recorded.resolved_git_toplevel.clone(),
            resolved.resolved_git_toplevel.clone(),
        ),
        (
            "scoped_relative_path",
            recorded.scoped_relative_path.clone(),
            resolved.scoped_relative_path.clone(),
        ),
    ] {
        if recorded_value != resolved_value {
            return Some(format!(
                "resolved-path-drift refusal for {:?}: {field} changed from {recorded_value:?} to {resolved_value:?}",
                resolved.literal_path
            ));
        }
    }
    None
}

fn standing_artifact_key_from_record(
    record: &crate::db::standing_roots::StandingRootRecord,
) -> Option<String> {
    crate::scoped_key::classify_resolved_standing_root(
        Path::new(&record.resolved_target),
        record.resolved_git_toplevel.as_deref().map(Path::new),
    )
    .ok()
    .map(|identity| identity.artifact_key().to_string())
}

fn standing_root_health_value(
    snapshot: RootHealthSnapshot,
    standing: Option<&StandingHealthEntry>,
) -> Value {
    let mut value = serde_json::to_value(snapshot).expect("root health snapshots serialize");
    let Some(standing) = standing else {
        return value;
    };
    let object = value
        .as_object_mut()
        .expect("root health snapshots serialize as objects");
    object.insert("standing".to_string(), Value::Bool(true));
    object.insert(
        "standing_entry".to_string(),
        Value::String(standing.literal_path.clone()),
    );
    if let Some(refusal) = &standing.refusal {
        object.insert(
            "standing_refusal".to_string(),
            Value::String(refusal.clone()),
        );
    }
    value
}

fn unhosted_standing_health_snapshot(entry: &StandingHealthEntry) -> RootHealthSnapshot {
    RootHealthSnapshot {
        cache_unavailable_reason: None,
        project_root: entry.root_id.as_ref().map_or_else(
            || entry.literal_path.clone(),
            |root_id| root_id.as_path().display().to_string(),
        ),
        actor_count: 0,
        state: crate::context::RootHealthState::Ready,
        search_index: None,
        semantic_index: None,
        callgraph_store: None,
        callgraph_repair_entries_60s: None,
        callgraph_commits_60s: None,
        callgraph_pages_or_bytes_written_60s: None,
        views: None,
        tier2: None,
        bash: None,
        suspended_domains: Vec::new(),
    }
}

#[cfg(test)]
mod plane_timing_tests {
    use super::*;

    #[test]
    fn plane_timing_tracks_build_entry_and_progress_changes() {
        let cache = HealthRollupCache::new();
        let mut first = json!({
            "search_index": { "status": "building" },
            "semantic_index": { "status": "building", "stage": "embedding", "embedded_chunks": 1 },
            "tier2": { "status": "ready" },
        });
        cache.annotate_plane_timings("/tmp/root", &mut first);
        let search_since = first["search_index"]["since_ms"]
            .as_u64()
            .expect("search since");
        let semantic_progress = first["semantic_index"]["last_progress_at_ms"]
            .as_u64()
            .expect("semantic progress");

        let mut unchanged = json!({
            "search_index": { "status": "building" },
            "semantic_index": { "status": "building", "stage": "embedding", "embedded_chunks": 1 },
            "tier2": { "status": "ready" },
        });
        cache.annotate_plane_timings("/tmp/root", &mut unchanged);
        assert_eq!(
            unchanged["search_index"]["since_ms"].as_u64(),
            Some(search_since)
        );
        assert_eq!(
            unchanged["semantic_index"]["last_progress_at_ms"].as_u64(),
            Some(semantic_progress)
        );

        std::thread::sleep(Duration::from_millis(2));
        let mut progressed = json!({
            "search_index": { "status": "ready" },
            "semantic_index": { "status": "building", "stage": "embedding", "embedded_chunks": 2 },
            "tier2": { "status": "ready" },
        });
        cache.annotate_plane_timings("/tmp/root", &mut progressed);
        assert!(
            progressed["search_index"]["since_ms"]
                .as_u64()
                .expect("ready since")
                >= search_since
        );
        assert!(
            progressed["semantic_index"]["last_progress_at_ms"]
                .as_u64()
                .expect("new progress")
                > semantic_progress
        );
    }

    #[test]
    fn stale_tier2_uses_scheduler_fields_instead_of_build_timings() {
        let cache = HealthRollupCache::new();
        let mut stale = json!({
            "tier2": {
                "status": "stale",
                "stale_since_ms": 10,
                "next_refresh_at_ms": 20,
                "pending_paths": 3,
            },
        });
        cache.annotate_plane_timings("/tmp/root", &mut stale);

        assert_eq!(stale["tier2"]["stale_since_ms"], 10);
        assert_eq!(stale["tier2"]["next_refresh_at_ms"], 20);
        assert_eq!(stale["tier2"]["pending_paths"], 3);
        assert!(stale["tier2"].get("since_ms").is_none());
        assert!(stale["tier2"].get("last_progress_at_ms").is_none());

        let mut building = json!({ "tier2": { "status": "building" } });
        cache.annotate_plane_timings("/tmp/root", &mut building);
        assert!(building["tier2"]["since_ms"].is_u64());
        assert!(building["tier2"]["last_progress_at_ms"].is_u64());
    }
}

#[cfg(test)]
mod observability_retention_tests {
    use super::*;

    #[test]
    fn inactive_background_event_windows_are_pruned_but_live_windows_remain() {
        let (_dir, root) = crate::subc::test_support::test_root("bg-observability-prune");
        let metrics = DispatchPathMetrics::new();
        let start = Instant::now();
        metrics.record_bg_event_at(&root, "inactive", BgEventKind::ArmHit, start);
        metrics.record_bg_event_at(&root, "active", BgEventKind::ArmHit, start);
        let now = start + BG_OBSERVABILITY_INTERVAL;
        metrics.record_bg_event_at(&root, "recent", BgEventKind::ArmHit, now);
        let subscriptions = HashMap::from([(
            (root, "active".to_string()),
            std::collections::HashSet::from([super::super::route_key(42, 1)]),
        )]);
        metrics.prune_bg_event_records(&subscriptions, now);
        let events = metrics.bg_events.lock().unwrap();
        assert_eq!(events.len(), 2);
        assert!(events.keys().all(|key| key.session != "inactive"));
    }
}

#[cfg(test)]
mod watch_gap_tests {

    #[test]
    fn retry_overflow_health_keeps_evictions_separate_and_clears_after_ack() {
        use crate::bash_background::watch_lines::{LineBatch, RetryGap};
        use crate::db::bash_watches::*;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("aft.db");
        let conn = crate::db::open(&path).unwrap();
        let batch = LineBatch {
            retry_gap: Some(RetryGap {
                reason: "retry_overflow".into(),
                dropped_notifications: 3,
                known_missing_occurrences: 1,
                ..Default::default()
            }),
            version: Some(5),
            ..Default::default()
        };
        let row = BashPatternWatchRow {
            harness: "opencode".into(),
            session_id: "session".into(),
            task_id: "task".into(),
            watch_id: "watch".into(),
            pattern_kind: "substring".into(),
            pattern: "READY".into(),
            once: false,
            each_line: false,
            match_batch: Some(serde_json::to_string(&batch).unwrap()),
            created_at: 0,
            stdout_offset: 0,
            stderr_offset: 0,
            pty_offset: 0,
            scanning: true,
            pending_match: true,
            match_text: Some("watch delivery gap".into()),
            match_offset: Some(5),
            match_context: None,
        };
        upsert_delivery_gap(&conn, &row).unwrap();
        let (gaps, missing, uncertain) = watch_gap_health_for(&conn);
        assert_eq!((gaps, missing, uncertain), (1, 1, false));
        let health = super::super::watch_delivery::health_from_census(gaps, missing, uncertain, 0);
        assert_eq!(health["status"], "degraded");
        assert_eq!(health["reason"], "retry_overflow");
        assert_eq!(health["outstanding_watch_gaps"], 1);
        assert_eq!(health["known_missing_occurrences"], 1);
        assert!(health["evicted_transport_copies_total"].is_u64());
        acknowledge_delivery(
            &conn,
            "opencode",
            "session",
            "task",
            "watch",
            5,
            batch.retry_gap.as_ref(),
        )
        .unwrap();
        delete_delivery_gap(&conn, "opencode", "session", "task", "watch").unwrap();
        let (gaps, missing, uncertain) = watch_gap_health_for(&conn);
        let cleared = super::super::watch_delivery::health_from_census(gaps, missing, uncertain, 0);
        assert_eq!(cleared["status"], "ok");
        assert!(cleared["reason"].is_null());
        assert_eq!(cleared["outstanding_watch_gaps"], 0);
        assert_eq!(cleared["known_missing_occurrences"], 0);
        let unconfirmed = super::super::watch_delivery::health_from_census(0, 0, true, 1);
        assert_eq!(unconfirmed["status"], "degraded");
        assert_eq!(unconfirmed["reason"], "watch_gap_persistence_unconfirmed");
    }
}
