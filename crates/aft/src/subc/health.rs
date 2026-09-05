//! Dispatch-path metrics and health-report helpers for the subc transport loop.

use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use super::{
    json, Arc, AtomicBool, AtomicU64, AtomicUsize, BgSubsBySession, Duration, Executor, HashMap,
    HealthReport, HealthStatus, Instant, Ordering, PendingBind, ProjectRootId, RootHealthSnapshot,
    RouteChannel, StdMutex, Value, DISPATCH_PATH_BIND_WARN_AFTER, WRITER_QUEUE_CAPACITY,
};
use crate::context::{App, AppContext};
use crate::executor::BindBlockerSnapshot;

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
}

impl ReapBlockerCensus {
    pub(super) fn blocker_histogram(&self) -> String {
        format!(
            "absence_unconfirmed={},bound_routes={},unbound_quiesced={},bash_waits={},maintenance_pending={},maintenance_queued={},pending_binds={},actor_busy={},actor_state_busy={},artifact_eviction_blocked={},artifact_eviction_failed={}",
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
        )
    }
}

struct ReapMetrics {
    last_sweep_ms: AtomicU64,
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
}

impl ReapMetrics {
    fn new() -> Self {
        Self {
            last_sweep_ms: AtomicU64::new(0),
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
        }
    }

    fn record(&self, now_ms: u64, census: ReapBlockerCensus) {
        self.last_sweep_ms.store(now_ms, Ordering::Relaxed);
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
    }

    fn snapshot(&self) -> Value {
        json!({
            "deleted_retained": self.deleted_retained.load(Ordering::Relaxed),
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

pub(super) struct DispatchPathMetrics {
    pub(super) origin: Instant,
    pub(super) frame_loop_last_tick_ms: AtomicU64,
    pub(super) writer_queued: AtomicUsize,
    pub(super) writer_active: AtomicBool,
    pub(super) writer_saturation_count: AtomicU64,
    pub(super) control_completion_queued: AtomicUsize,
    pub(super) maintenance_queued: AtomicUsize,
    pub(super) bash_deferred_queued: AtomicUsize,
    pub(super) bash_poll_touch_queued: AtomicUsize,
    pub(super) reliable_push_budget_deferrals: AtomicU64,
    pub(super) maintenance_budget_deferrals: AtomicU64,
    pub(super) response_tasks_live: AtomicUsize,
    bg_subscriptions: AtomicUsize,
    bg_wake_pending: AtomicUsize,
    pub(super) bg_wake_unacked_total: AtomicUsize,
    bg_wake_rearm_total: AtomicU64,
    stuck_pending_watch_next_scan_ms: AtomicU64,
    stuck_pending_watch_logs: StdMutex<HashMap<String, Instant>>,
    bg_events: StdMutex<HashMap<BgEventKey, BgEventRecord>>,
    bg_event_rates: BgEventRates,
    reap: ReapMetrics,
}

impl DispatchPathMetrics {
    pub(super) fn new() -> Self {
        Self {
            origin: Instant::now(),
            frame_loop_last_tick_ms: AtomicU64::new(0),
            writer_queued: AtomicUsize::new(0),
            writer_active: AtomicBool::new(false),
            writer_saturation_count: AtomicU64::new(0),
            control_completion_queued: AtomicUsize::new(0),
            maintenance_queued: AtomicUsize::new(0),
            bash_deferred_queued: AtomicUsize::new(0),
            bash_poll_touch_queued: AtomicUsize::new(0),
            reliable_push_budget_deferrals: AtomicU64::new(0),
            maintenance_budget_deferrals: AtomicU64::new(0),
            response_tasks_live: AtomicUsize::new(0),
            bg_subscriptions: AtomicUsize::new(0),
            bg_wake_pending: AtomicUsize::new(0),
            bg_wake_unacked_total: AtomicUsize::new(0),
            bg_wake_rearm_total: AtomicU64::new(0),
            stuck_pending_watch_next_scan_ms: AtomicU64::new(0),
            stuck_pending_watch_logs: StdMutex::new(HashMap::new()),
            bg_events: StdMutex::new(HashMap::new()),
            bg_event_rates: BgEventRates::new(),
            reap: ReapMetrics::new(),
        }
    }

    fn now_ms(&self) -> u64 {
        duration_millis_u64(self.origin.elapsed())
    }

    pub(super) fn mark_frame_loop_tick(&self) {
        self.frame_loop_last_tick_ms
            .store(self.now_ms(), Ordering::Relaxed);
    }

    pub(super) fn record_reap(&self, census: ReapBlockerCensus) {
        let last_sweep_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(duration_millis_u64)
            .unwrap_or(0);
        self.reap.record(last_sweep_ms, census);
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
        &self,
        executor: &Executor,
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
                crate::slog_warn!(
                    "subc bg wake: pending watch remains unacked root={} session={} task={} watch={} age_ms={}",
                    root.as_path().display(),
                    session,
                    task_id,
                    watch_id,
                    age_ms
                );
            }
        }
        logged.retain(|_, last| {
            now.saturating_duration_since(*last) < STUCK_PENDING_WATCH_LOG_INTERVAL
        });
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

    fn snapshot(&self, pending_binds: &HashMap<RouteChannel, PendingBind>) -> Value {
        let now = Instant::now();
        let oldest_pending_age_ms = pending_binds
            .values()
            .map(|bind| duration_millis_u64(now.saturating_duration_since(bind.started_at)))
            .max();
        let last_tick_ms = self.frame_loop_last_tick_ms.load(Ordering::Relaxed);
        json!({
            "frame_loop": {
                "last_tick_age_ms": self.now_ms().saturating_sub(last_tick_ms),
            },
            "pending_binds": {
                "count": pending_binds.len(),
                "oldest_age_ms": oldest_pending_age_ms,
            },
            "completion_channels": {
                "control": self.control_completion_queued.load(Ordering::Relaxed),
                "maintenance": self.maintenance_queued.load(Ordering::Relaxed),
                "bash_deferred": self.bash_deferred_queued.load(Ordering::Relaxed),
                "bash_poll_touch": self.bash_poll_touch_queued.load(Ordering::Relaxed),
            },
            "budget_deferrals": {
                "reliable_push": self.reliable_push_budget_deferrals.load(Ordering::Relaxed),
                "maintenance": self.maintenance_budget_deferrals.load(Ordering::Relaxed),
            },
            "writer": {
                "queued": self.writer_queued.load(Ordering::Relaxed),
                "active": self.writer_active.load(Ordering::Relaxed),
                "capacity": WRITER_QUEUE_CAPACITY,
                "saturation_count": self.writer_saturation_count.load(Ordering::Relaxed),
            },
            "response_tasks": {
                "live": self.response_tasks_live.load(Ordering::Relaxed),
            },
        })
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
                reader_admissions_while_promoted_writer_waited: 0,
            });
        crate::slog_warn!(
            "{}",
            pending_bind_breadcrumb(
                *route,
                &pending.bind_root_id,
                age,
                &pending.configure_request_id,
                &snapshot,
            )
        );
    }
}

fn pending_bind_breadcrumb(
    route: RouteChannel,
    root_id: &crate::path_identity::ProjectRootId,
    age: Duration,
    configure_request_id: &str,
    snapshot: &BindBlockerSnapshot,
) -> String {
    let blockers = if snapshot.blockers.is_empty() {
        "none".to_string()
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
    format!(
        "subc attach: pending RouteBind route {route} for root {} crossed {}ms (configure_request_id={}, configure_state={}, configure_phase_timings=[{}], blockers=[{}], oldest_queued_writer_age_ms={:?}, in_flight_readers=[{}], reader_admissions_while_promoted_writer_waited={})",
        root_id.as_path().display(),
        duration_millis_u64(age),
        configure_request_id,
        snapshot.configure_state,
        phase_timings,
        blockers,
        snapshot.oldest_queued_writer_age_ms,
        readers,
        snapshot.reader_admissions_while_promoted_writer_waited,
    )
}

const HEALTH_ROOT_DETAIL_CAP: usize = crate::memory::MEMORY_SNAPSHOT_ROOT_DETAIL_CAP;
pub(super) const HEALTH_ROLLUP_TTL: Duration = Duration::from_secs(3);

#[derive(Clone)]
struct HealthDiagnosticRollup {
    status: HealthStatus,
    detail: Option<String>,
    metrics: Value,
    memory_census: Value,
}

impl HealthDiagnosticRollup {
    fn unavailable() -> Self {
        let mut metrics = json!({
            "actor_count": 0,
            "root_count": 0,
            "root_details_omitted": 0,
            "callgraph_repair_entries_60s_total": 0,
            "callgraph_repair_roots_annotated": 0,
            "callgraph_repair_roots_total": 0,
            "callgraph_commits_60s_total": 0,
            "callgraph_pages_or_bytes_written_60s_total": 0,
            "lsp_children": { "spawned": 0, "cwd_gone": 0 },
            "memory": memory_rollup_metrics(None),
            "mutating_lanes": { "scheduler_busy": true },
            "roots": [],
        });
        insert_lifecycle_metrics(
            &mut metrics,
            crate::lifecycle_census::LifecycleCensusSnapshot::default(),
        );
        Self {
            status: HealthStatus::Degraded,
            detail: Some("health diagnostic snapshot is being refreshed".to_string()),
            metrics,
            memory_census: json!({ "roots": {}, "process": {} }),
        }
    }
}

pub(super) struct HealthRollupCache {
    origin: Instant,
    generated_at_ms: AtomicU64,
    /// Count of completed refreshes. `generated_at_ms` cannot signal "published"
    /// on its own: a refresh that finishes within the cache's first millisecond
    /// stores 0, the same value as never-published.
    refreshes: AtomicU64,
    snapshot: std::sync::RwLock<Arc<HealthDiagnosticRollup>>,
    breakers:
        std::sync::Mutex<HashMap<std::path::PathBuf, Arc<crate::build_breaker::BuildDeathBreaker>>>,
}

impl HealthRollupCache {
    pub(super) fn new() -> Self {
        Self {
            origin: Instant::now(),
            generated_at_ms: AtomicU64::new(0),
            refreshes: AtomicU64::new(0),
            snapshot: std::sync::RwLock::new(Arc::new(HealthDiagnosticRollup::unavailable())),
            breakers: std::sync::Mutex::new(HashMap::new()),
        }
    }

    /// Assemble outside the cache lock, then hold the write lock only long
    /// enough to replace one `Arc`. Probe readers never wait for a refresh.
    pub(super) fn refresh(&self, executor: &Executor, shared_app: &App) {
        let rollup = Arc::new(build_health_diagnostic_rollup(self, executor, shared_app));
        let generated_at_ms = duration_millis_u64(self.origin.elapsed());
        match self.snapshot.write() {
            Ok(mut snapshot) => *snapshot = rollup,
            Err(error) => *error.into_inner() = rollup,
        }
        self.generated_at_ms
            .store(generated_at_ms, Ordering::Release);
        self.refreshes.fetch_add(1, Ordering::AcqRel);
    }

    fn refresh_build_suspensions(
        &self,
        ctx: &AppContext,
        project_root: &std::path::Path,
        project_key: Option<&str>,
    ) {
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis()
            .min(u128::from(u64::MAX)) as u64;
        let Some(path) = ctx.build_breaker_path_for_health(project_key) else {
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

    fn snapshot(&self) -> (Arc<HealthDiagnosticRollup>, u64) {
        let generated_at_ms = self.generated_at_ms.load(Ordering::Acquire);
        let age_ms = duration_millis_u64(self.origin.elapsed()).saturating_sub(generated_at_ms);
        let snapshot = match self.snapshot.try_read() {
            Ok(snapshot) => Arc::clone(&snapshot),
            Err(std::sync::TryLockError::Poisoned(error)) => Arc::clone(&error.into_inner()),
            Err(std::sync::TryLockError::WouldBlock) => {
                Arc::new(HealthDiagnosticRollup::unavailable())
            }
        };
        (snapshot, age_ms)
    }

    pub(super) fn memory_census(&self) -> Value {
        self.snapshot().0.memory_census.clone()
    }
}

pub(super) struct HealthRollupWorker {
    wake_tx: std::sync::mpsc::SyncSender<bool>,
    join: Option<std::thread::JoinHandle<()>>,
}

impl HealthRollupWorker {
    pub(super) fn start(
        cache: Arc<HealthRollupCache>,
        executor: Arc<Executor>,
        shared_app: Arc<App>,
    ) -> Self {
        let (wake_tx, wake_rx) = std::sync::mpsc::sync_channel(1);
        let join = std::thread::Builder::new()
            .name("aft-health-rollup".to_string())
            .spawn(move || loop {
                cache.refresh(&executor, &shared_app);
                match wake_rx.recv_timeout(HEALTH_ROLLUP_TTL) {
                    Ok(true) | Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
                    Ok(false) | Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
                }
            })
            .expect("spawn health rollup worker");
        Self {
            wake_tx,
            join: Some(join),
        }
    }

    pub(super) fn request_refresh(&self) {
        let _ = self.wake_tx.try_send(true);
    }

    pub(super) fn shutdown(mut self) {
        let _ = self.wake_tx.send(false);
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

/// Build the compact memory rollup from pre-aggregated root counters. Rich
/// subsystem detail is never constructed for roots omitted by the top-N cap.
fn memory_rollup_metrics(
    roots: Option<std::collections::BTreeMap<String, crate::memory::RootMemoryRollup>>,
) -> Value {
    let Some(roots) = roots else {
        return json!({
            "status": "busy",
            "allocator_slack_bytes": 0,
            "allocator_slack_measured": false,
            "allocator_observation_age_ms": Value::Null,
        });
    };
    let snapshot = crate::memory::MemoryRollupSnapshot::new("ready", roots);
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
        "allocator_slack_measured": snapshot.process.allocator_slack_measured,
        "allocator_observation_age_ms": snapshot.process.allocator_observation_age_ms,
        // Headline number: excludes reclaimable pages RSS still counts.
        "phys_footprint_bytes": snapshot.process.phys_footprint_bytes,
        "total_attributed_bytes": snapshot.process.total_attributed_bytes,
        "sqlite_bytes": snapshot.process.sqlite.memory_used_bytes,
    })
}

fn mutating_lanes_metrics(executor: &Executor) -> Value {
    match executor.try_mutating_lane_snapshots() {
        Some(snapshots) => Value::Array(
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
        ),
        None => json!({ "scheduler_busy": true }),
    }
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
            },
            "interactive_reserve": snapshot.interactive_reserve,
            "maintenance_cap": snapshot.maintenance_cap,
        }),
        None => json!({ "scheduler_busy": true }),
    }
}

fn build_health_diagnostic_rollup(
    cache: &HealthRollupCache,
    executor: &Executor,
    shared_app: &App,
) -> HealthDiagnosticRollup {
    struct RootCandidate {
        root_label: String,
        health: RootHealthSnapshot,
        busy: bool,
        fully_ready: bool,
        attributed_bytes: u64,
        repair_entries_60s: Option<u64>,
        standing: Option<StandingHealthEntry>,
    }

    let Some(actor_entries) = executor.try_actor_entries() else {
        return HealthDiagnosticRollup {
            status: HealthStatus::Degraded,
            detail: Some(
                "executor scheduler state could not be snapshotted without contention".to_string(),
            ),
            metrics: HealthDiagnosticRollup::unavailable().metrics,
            memory_census: json!({ "roots": {}, "process": {} }),
        };
    };

    let lifecycle_contexts = actor_entries
        .iter()
        .map(|(_, context)| Arc::clone(context))
        .collect::<Vec<_>>();
    let standing_entries = standing_health_entries(&actor_entries);
    let mut standing_matched = vec![false; standing_entries.len()];
    let actor_count = actor_entries.len();
    let mut memory_roots = std::collections::BTreeMap::new();
    let mut census_roots = std::collections::BTreeMap::new();
    let mut candidates = Vec::with_capacity(actor_count.saturating_add(standing_entries.len()));
    let mut repair_roots_annotated = 0usize;
    for (root_id, ctx) in actor_entries {
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
        let census_memory = ctx.memory_root_snapshot();
        census_roots.insert(root_label.clone(), census_memory);
        let memory = if standing.is_some() {
            ctx.memory_root_rollup().with_standing()
        } else {
            ctx.memory_root_rollup()
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
        cache.refresh_build_suspensions(ctx.as_ref(), root_id.as_path(), artifact_key);
        let repair_entries_60s = artifact_key.and_then(|key| {
            repair_roots_annotated = repair_roots_annotated.saturating_add(1);
            crate::callgraph_store::repair_entry_rate(key)
                .map(|(count, _window_start)| count)
                .filter(|count| *count > 0)
        });
        let health_summary = ctx.try_health_summary();
        let busy = health_summary.is_busy();
        let fully_ready = health_summary.is_fully_ready();
        candidates.push(RootCandidate {
            root_label,
            health: health_summary.into_snapshot(root_id.as_path()),
            busy,
            fully_ready,
            attributed_bytes,
            repair_entries_60s,
            standing,
        });
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
            health,
            busy: false,
            fully_ready: false,
            attributed_bytes: 0,
            repair_entries_60s: None,
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
    let mut roots: Vec<(String, Value)> = candidates
        .into_iter()
        .take(HEALTH_ROOT_DETAIL_CAP)
        .map(|candidate| {
            let mut snapshot = candidate.health;
            snapshot.callgraph_repair_entries_60s = candidate.repair_entries_60s;
            let root_label = snapshot.project_root.clone();
            (
                root_label,
                standing_root_health_value(snapshot, candidate.standing.as_ref()),
            )
        })
        .collect();
    roots.sort_by(|(left, _), (right, _)| left.cmp(right));
    let roots = roots
        .into_iter()
        .map(|(_, snapshot)| snapshot)
        .collect::<Vec<_>>();

    let root_count = root_details_omitted.saturating_add(roots.len());
    let callgraph_repair_entries_60s_total = crate::callgraph_store::repair_entry_rate_total();
    let callgraph_write_metrics_total = crate::callgraph_store::callgraph_write_metrics_total();
    let memory = memory_rollup_metrics(Some(memory_roots));
    let census_snapshot = crate::memory::MemorySnapshot::new_uncapped("ready", census_roots);
    let memory_census =
        crate::commands::memory_census::render_memory_census(&census_snapshot, None);
    let lifecycle = crate::lifecycle_census::collect(shared_app, &lifecycle_contexts);
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

    let mut metrics = json!({
        "actor_count": actor_count,
        "root_count": root_count,
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
        "memory": memory,
        "mutating_lanes": mutating_lanes_metrics(executor),
        "roots": roots,
    });
    insert_lifecycle_metrics(&mut metrics, lifecycle);
    HealthDiagnosticRollup {
        status: if busy_roots > 0 || standing_refusals > 0 {
            HealthStatus::Degraded
        } else {
            HealthStatus::Ok
        },
        detail,
        metrics,
        memory_census,
    }
}

pub(super) fn build_health_report(
    cache: &HealthRollupCache,
    executor: &Executor,
    pending_binds: &HashMap<RouteChannel, PendingBind>,
    dispatch_path_metrics: &DispatchPathMetrics,
    shared_app: &App,
) -> HealthReport {
    // The diagnostic payload is fixed-size and cached. Only probe-purpose
    // liveness signals are read fresh, using atomics or non-blocking snapshots.
    let (rollup, snapshot_age_ms) = cache.snapshot();
    let mut metrics = rollup
        .metrics
        .as_object()
        .cloned()
        .unwrap_or_else(serde_json::Map::new);
    let lsp_children = metrics.remove("lsp_children").unwrap_or(Value::Null);
    let mutating_lanes = metrics
        .remove("mutating_lanes")
        .unwrap_or_else(|| json!({ "scheduler_busy": true }));
    metrics.insert("snapshot_age_ms".to_string(), json!(snapshot_age_ms));
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
    metrics.insert(
        "dispatch_liveness".to_string(),
        dispatch_liveness_metrics(executor),
    );
    let mut dispatch_path = dispatch_path_metrics.snapshot(pending_binds);
    if let Some(dispatch_path) = dispatch_path.as_object_mut() {
        dispatch_path.insert("mutating_lanes".to_string(), mutating_lanes);
    }
    metrics.insert("dispatch_path".to_string(), dispatch_path);

    let scheduler_busy = executor.try_actor_count().is_none();
    HealthReport {
        status: if scheduler_busy {
            HealthStatus::Degraded
        } else {
            rollup.status.clone()
        },
        detail: if scheduler_busy {
            Some("executor scheduler state could not be snapshotted without contention".to_string())
        } else {
            rollup.detail.clone()
        },
        metrics: Some(Value::Object(metrics)),
    }
}

#[cfg(test)]
mod tests {
    use super::super::test_support::{test_ctx, test_root};
    use super::super::{Lane, Response};
    use super::*;
    use serde_json::json;

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
        build_health_report(&cache, executor, pending_binds, metrics, app)
    }

    fn refresh_until_root_count(
        cache: &HealthRollupCache,
        executor: &Executor,
        app: &App,
        expected: u64,
    ) {
        let metrics = DispatchPathMetrics::new();
        let deadline = Instant::now() + Duration::from_secs(1);
        loop {
            cache.refresh(executor, app);
            let report = build_health_report(cache, executor, &HashMap::new(), &metrics, app);
            if report
                .metrics
                .as_ref()
                .and_then(|metrics| metrics["root_count"].as_u64())
                == Some(expected)
            {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "health cache did not capture {expected} roots"
            );
            std::thread::yield_now();
        }
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
        config.search_index = false;
        config.semantic_search = false;
        config.callgraph_store = false;
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
    fn disabled_callgraph_store_reports_disabled_despite_lingering_build_receiver() {
        let executor = Executor::new();
        let (_dir, root) = test_root("health-callgraph-disabled-lingering-rx");
        let mut config = crate::config::Config::default();
        config.callgraph_store = false;
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

        let report = test_health_report(
            &executor,
            &HashMap::new(),
            &DispatchPathMetrics::new(),
            &crate::context::App::default_shared(),
        );
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
        assert_eq!(dispatch["interactive"]["queued"].as_u64(), Some(1));
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
        assert_eq!(busy_memory["allocator_slack_bytes"].as_u64(), Some(0));
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
        let five_median = cached_reply_median(&five_cache, &five, &metrics, &app);
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
        let fifty_median = cached_reply_median(&fifty_cache, &fifty, &metrics, &app);
        let report = build_health_report(&fifty_cache, &fifty, &HashMap::new(), &metrics, &app);
        let fifty_bytes = serde_json::to_vec(&report)
            .expect("serialize fifty-root health report")
            .len();
        let report_metrics = report.metrics.expect("health metrics");

        assert_eq!(report_metrics["root_count"].as_u64(), Some(50));
        assert_eq!(report_metrics["root_details_omitted"].as_u64(), Some(42));
        assert_eq!(report_metrics["roots"].as_array().map(Vec::len), Some(8));
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
        assert!(
            fifty_median <= five_median.saturating_mul(4) + Duration::from_millis(2),
            "cached reply scaled with roots: five={five_median:?}, fifty={fifty_median:?}"
        );
        assert!(
            fifty_median < Duration::from_millis(50),
            "cached 50-root reply exceeded CI bound: {fifty_median:?}"
        );
        std::hint::black_box((five_dirs, fifty_dirs));
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
    fn cached_health_reply_exposes_snapshot_age_and_counter_coverage() {
        let executor = Executor::with_config(crate::executor::ExecutorConfig {
            pool_size: 1,
            read_cap: 1,
            actor_cap: 1,
            heavy_permits: 1,
            drr_quantum: 1,
        });
        let (_dir, root) = test_root("health-snapshot-age-coverage");
        assert!(executor.register_actor(root.clone(), test_ctx()));
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
        let report = test_health_report(&executor, &HashMap::new(), &metrics, &app);
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
        entries
            .entry(root.path.clone())
            .or_insert_with(|| standing_health_entry(root, database.as_ref()));
    }
    entries.into_values().collect()
}

fn standing_health_entry(
    root: &crate::config::IndexRootConfig,
    database: Option<&crate::db::TrackedConnection>,
) -> StandingHealthEntry {
    let recorded = database
        .and_then(|database| {
            crate::db::standing_roots::get_standing_root(database, &root.path).ok()
        })
        .flatten();
    match crate::scoped_key::resolve_standing_root(&root.path) {
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
