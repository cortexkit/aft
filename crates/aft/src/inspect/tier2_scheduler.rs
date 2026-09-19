use std::time::{Duration, Instant};

pub const TIER2_REFRESH_DEBOUNCE: Duration = Duration::from_secs(45);
// Ceiling that forces a Tier-2 refresh during CONTINUOUS editing (when the
// debounce never gets its quiet window). Set high: mid-session refreshes show
// churning, half-applied numbers and cost a scan with no value until changes
// land — a normal continuous-coding stretch should not trigger one.
pub const TIER2_REFRESH_MAX_STALENESS: Duration = Duration::from_secs(30 * 60);
pub const TIER2_REFRESH_MIN_INTERVAL: Duration = Duration::from_secs(5 * 60);
pub const TIER2_REFRESH_COLD_CACHE_DELAY: Duration = Duration::from_secs(90);
pub const TIER2_REFRESH_STORM_DEBOUNCE: Duration = Duration::from_secs(120);
pub const TIER2_REFRESH_STORM_PATH_THRESHOLD: usize = 200;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tier2TriggerReason {
    Debounce,
    Ceiling,
    Pull,
    ConfigureWarm,
}

impl Tier2TriggerReason {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Debounce => "debounce",
            Self::Ceiling => "ceiling",
            Self::Pull => "pull",
            Self::ConfigureWarm => "configure_warm",
        }
    }
}

#[derive(Debug, Clone)]
struct Tier2DispatchRollback {
    last_change_at: Option<Instant>,
    activity_started_at: Option<Instant>,
    debounce_delay: Duration,
    last_scan_started_at: Option<Instant>,
    pull_demand_pending: bool,
    configure_warm_pending: bool,
}

#[derive(Debug, Clone)]
pub struct Tier2RefreshScheduler {
    configured_at: Option<Instant>,
    last_change_at: Option<Instant>,
    activity_started_at: Option<Instant>,
    debounce_delay: Duration,
    last_scan_started_at: Option<Instant>,
    pull_demand_pending: bool,
    configure_warm_pending: bool,
    last_trigger_reason: Option<Tier2TriggerReason>,
    dispatch_rollback: Option<Tier2DispatchRollback>,
}

impl Tier2RefreshScheduler {
    pub fn new() -> Self {
        Self {
            configured_at: None,
            last_change_at: None,
            activity_started_at: None,
            debounce_delay: TIER2_REFRESH_DEBOUNCE,
            last_scan_started_at: None,
            pull_demand_pending: false,
            configure_warm_pending: false,
            last_trigger_reason: None,
            dispatch_rollback: None,
        }
    }

    pub fn reset_after_configure(&mut self, now: Instant) {
        self.configured_at = Some(now);
        self.last_change_at = None;
        self.activity_started_at = None;
        self.debounce_delay = TIER2_REFRESH_DEBOUNCE;
        self.last_scan_started_at = None;
        self.pull_demand_pending = false;
        self.configure_warm_pending = true;
        self.last_trigger_reason = None;
        self.dispatch_rollback = None;
    }

    pub fn request_pull(&mut self, can_write: bool) -> bool {
        if !can_write {
            return false;
        }
        self.pull_demand_pending = true;
        true
    }

    pub fn tick(
        &mut self,
        now: Instant,
        changed_path_count: usize,
        can_write: bool,
        in_flight: bool,
    ) -> Option<Tier2TriggerReason> {
        self.tick_with_semantic_gate(now, changed_path_count, can_write, in_flight, false)
    }

    pub fn tick_with_semantic_gate(
        &mut self,
        now: Instant,
        changed_path_count: usize,
        can_write: bool,
        in_flight: bool,
        semantic_cold_seed_active: bool,
    ) -> Option<Tier2TriggerReason> {
        if changed_path_count > 0 {
            self.record_changes(now, changed_path_count);
        }

        if !can_write || in_flight || !self.min_interval_elapsed(now) {
            return None;
        }

        if semantic_cold_seed_active {
            return None;
        }

        if self.pull_demand_pending {
            return Some(self.record_scan_start(now, Tier2TriggerReason::Pull));
        }

        let cold_delay_elapsed = self.cold_delay_elapsed(now);
        if cold_delay_elapsed {
            if self.ceiling_elapsed(now) {
                return Some(self.record_scan_start(now, Tier2TriggerReason::Ceiling));
            }
            if self.debounce_elapsed(now) {
                return Some(self.record_scan_start(now, Tier2TriggerReason::Debounce));
            }
            if self.configure_warm_pending && self.last_change_at.is_none() {
                return Some(self.record_scan_start(now, Tier2TriggerReason::ConfigureWarm));
            }
        }

        None
    }

    pub fn note_external_scan_started(&mut self, now: Instant) {
        self.last_scan_started_at = Some(now);
        self.pull_demand_pending = false;
        self.configure_warm_pending = false;
        self.dispatch_rollback = None;
        self.clear_activity_window();
    }

    /// Restore the trigger state when the cold-build permit rejects a dispatch.
    /// An automatic debounce must not become pull demand, because pull demand
    /// intentionally bypasses the watcher quiet window.
    pub fn note_dispatch_deferred(&mut self) {
        let Some(rollback) = self.dispatch_rollback.take() else {
            return;
        };
        self.last_change_at = rollback.last_change_at;
        self.activity_started_at = rollback.activity_started_at;
        self.debounce_delay = rollback.debounce_delay;
        self.last_scan_started_at = rollback.last_scan_started_at;
        self.pull_demand_pending = rollback.pull_demand_pending;
        self.configure_warm_pending = rollback.configure_warm_pending;
    }

    pub fn last_trigger_reason(&self) -> Option<Tier2TriggerReason> {
        self.last_trigger_reason
    }

    pub fn pull_demand_pending(&self) -> bool {
        self.pull_demand_pending
    }

    /// Earliest instant at which the scheduler's current demand can dispatch,
    /// before external gates such as an in-flight builder or semantic cold seed.
    pub fn next_dispatch_at(&self, now: Instant) -> Option<Instant> {
        let min_interval_at = self
            .last_scan_started_at
            .map(|started| started + TIER2_REFRESH_MIN_INTERVAL);

        let requested_at = if self.pull_demand_pending {
            Some(now)
        } else if self.last_change_at.is_some() {
            let debounce_at = self
                .last_change_at
                .map(|changed| changed + self.debounce_delay);
            let ceiling_at = self
                .activity_started_at
                .map(|started| started + TIER2_REFRESH_MAX_STALENESS);
            match (debounce_at, ceiling_at) {
                (Some(debounce), Some(ceiling)) => Some(debounce.min(ceiling)),
                (deadline, None) | (None, deadline) => deadline,
            }
        } else if self.configure_warm_pending {
            self.configured_at
                .map(|configured| configured + TIER2_REFRESH_COLD_CACHE_DELAY)
        } else {
            None
        }?;

        let cold_cache_at = (self.last_scan_started_at.is_none() && !self.pull_demand_pending)
            .then(|| {
                self.configured_at
                    .map(|configured| configured + TIER2_REFRESH_COLD_CACHE_DELAY)
            })
            .flatten();

        Some(
            [min_interval_at, cold_cache_at]
                .into_iter()
                .flatten()
                .fold(requested_at, Instant::max),
        )
    }

    fn record_changes(&mut self, now: Instant, changed_path_count: usize) {
        if self.activity_started_at.is_none() {
            self.activity_started_at = Some(now);
            self.debounce_delay = TIER2_REFRESH_DEBOUNCE;
        }
        self.last_change_at = Some(now);
        if changed_path_count > TIER2_REFRESH_STORM_PATH_THRESHOLD {
            self.debounce_delay = self.debounce_delay.max(TIER2_REFRESH_STORM_DEBOUNCE);
        }
    }

    fn min_interval_elapsed(&self, now: Instant) -> bool {
        self.last_scan_started_at
            .map(|started| elapsed_since(now, started) >= TIER2_REFRESH_MIN_INTERVAL)
            .unwrap_or(true)
    }

    fn cold_delay_elapsed(&self, now: Instant) -> bool {
        self.last_scan_started_at.is_some()
            || self
                .configured_at
                .map(|configured| elapsed_since(now, configured) >= TIER2_REFRESH_COLD_CACHE_DELAY)
                .unwrap_or(false)
    }

    fn ceiling_elapsed(&self, now: Instant) -> bool {
        self.activity_started_at
            .map(|started| elapsed_since(now, started) >= TIER2_REFRESH_MAX_STALENESS)
            .unwrap_or(false)
    }

    fn debounce_elapsed(&self, now: Instant) -> bool {
        self.last_change_at
            .map(|changed| elapsed_since(now, changed) >= self.debounce_delay)
            .unwrap_or(false)
    }

    fn record_scan_start(
        &mut self,
        now: Instant,
        reason: Tier2TriggerReason,
    ) -> Tier2TriggerReason {
        self.dispatch_rollback = Some(Tier2DispatchRollback {
            last_change_at: self.last_change_at,
            activity_started_at: self.activity_started_at,
            debounce_delay: self.debounce_delay,
            last_scan_started_at: self.last_scan_started_at,
            pull_demand_pending: self.pull_demand_pending,
            configure_warm_pending: self.configure_warm_pending,
        });
        self.last_scan_started_at = Some(now);
        self.pull_demand_pending = false;
        self.configure_warm_pending = false;
        self.last_trigger_reason = Some(reason);
        self.clear_activity_window();
        reason
    }

    fn clear_activity_window(&mut self) {
        self.last_change_at = None;
        self.activity_started_at = None;
        self.debounce_delay = TIER2_REFRESH_DEBOUNCE;
    }
}

impl Default for Tier2RefreshScheduler {
    fn default() -> Self {
        Self::new()
    }
}

fn elapsed_since(now: Instant, earlier: Instant) -> Duration {
    now.checked_duration_since(earlier)
        .unwrap_or(Duration::ZERO)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn configured_scheduler() -> (Tier2RefreshScheduler, Instant) {
        let base = Instant::now();
        let mut scheduler = Tier2RefreshScheduler::new();
        scheduler.reset_after_configure(base);
        (scheduler, base)
    }

    #[test]
    fn debounce_resets_on_each_change() {
        let (mut scheduler, base) = configured_scheduler();
        let warm = base + TIER2_REFRESH_COLD_CACHE_DELAY;

        assert_eq!(scheduler.tick(warm, 1, true, false), None);
        assert_eq!(
            scheduler.tick(
                warm + TIER2_REFRESH_DEBOUNCE - Duration::from_secs(1),
                1,
                true,
                false
            ),
            None
        );
        assert_eq!(
            scheduler.tick(warm + TIER2_REFRESH_DEBOUNCE, 0, true, false),
            None,
            "second change should reset the debounce deadline"
        );
        assert_eq!(
            scheduler.tick(
                warm + TIER2_REFRESH_DEBOUNCE + TIER2_REFRESH_DEBOUNCE,
                0,
                true,
                false,
            ),
            Some(Tier2TriggerReason::Debounce)
        );
    }

    #[test]
    fn ceiling_fires_during_continuous_activity() {
        let (mut scheduler, base) = configured_scheduler();
        let start = base + TIER2_REFRESH_COLD_CACHE_DELAY;
        assert_eq!(scheduler.tick(start, 1, true, false), None);

        let mut now = start;
        while now < start + TIER2_REFRESH_MAX_STALENESS {
            now += Duration::from_secs(30);
            let changed_paths = if now < start + TIER2_REFRESH_MAX_STALENESS {
                1
            } else {
                0
            };
            let decision = scheduler.tick(now, changed_paths, true, false);
            if now < start + TIER2_REFRESH_MAX_STALENESS {
                assert_eq!(decision, None);
            } else {
                assert_eq!(decision, Some(Tier2TriggerReason::Ceiling));
            }
        }
    }

    #[test]
    fn min_interval_throttles_second_scan() {
        let (mut scheduler, base) = configured_scheduler();
        let first = base + TIER2_REFRESH_COLD_CACHE_DELAY;
        assert_eq!(
            scheduler.tick(first, 0, true, false),
            Some(Tier2TriggerReason::ConfigureWarm)
        );

        let change = first + Duration::from_secs(1);
        assert_eq!(scheduler.tick(change, 1, true, false), None);
        assert_eq!(
            scheduler.tick(change + TIER2_REFRESH_DEBOUNCE, 0, true, false),
            None,
            "min interval should throttle scans inside five minutes"
        );
        assert_eq!(
            scheduler.tick(first + TIER2_REFRESH_MIN_INTERVAL, 0, true, false),
            Some(Tier2TriggerReason::Debounce)
        );
    }

    #[test]
    fn storm_extends_debounce_window() {
        let (mut scheduler, base) = configured_scheduler();
        let warm = base + TIER2_REFRESH_COLD_CACHE_DELAY;
        assert_eq!(
            scheduler.tick(warm, TIER2_REFRESH_STORM_PATH_THRESHOLD + 1, true, false),
            None
        );
        assert_eq!(
            scheduler.tick(
                warm + TIER2_REFRESH_STORM_DEBOUNCE - Duration::from_secs(1),
                0,
                true,
                false
            ),
            None
        );
        assert_eq!(
            scheduler.tick(warm + TIER2_REFRESH_STORM_DEBOUNCE, 0, true, false),
            Some(Tier2TriggerReason::Debounce)
        );
    }

    #[test]
    fn next_dispatch_tracks_min_interval_and_continuous_activity_ceiling() {
        let (mut scheduler, base) = configured_scheduler();
        let first_scan = base + TIER2_REFRESH_COLD_CACHE_DELAY;
        assert_eq!(
            scheduler.tick(first_scan, 0, true, false),
            Some(Tier2TriggerReason::ConfigureWarm)
        );
        let first_change = first_scan + Duration::from_secs(1);
        assert_eq!(scheduler.tick(first_change, 1, true, false), None);
        assert_eq!(
            scheduler.next_dispatch_at(first_change),
            Some(first_scan + TIER2_REFRESH_MIN_INTERVAL)
        );

        let (mut continuous, base) = configured_scheduler();
        let activity_start = base + TIER2_REFRESH_COLD_CACHE_DELAY;
        assert_eq!(continuous.tick(activity_start, 1, true, false), None);
        assert_eq!(
            continuous.next_dispatch_at(activity_start),
            Some(activity_start + TIER2_REFRESH_DEBOUNCE)
        );
        let last_change = activity_start + TIER2_REFRESH_MAX_STALENESS - Duration::from_secs(1);
        assert_eq!(continuous.tick(last_change, 1, true, false), None);
        assert_eq!(
            continuous.next_dispatch_at(last_change),
            Some(activity_start + TIER2_REFRESH_MAX_STALENESS)
        );
    }

    #[test]
    fn semantic_cold_seed_gate_defers_without_consuming_pending_work() {
        let (mut scheduler, base) = configured_scheduler();
        let warm = base + TIER2_REFRESH_COLD_CACHE_DELAY;

        assert_eq!(
            scheduler.tick_with_semantic_gate(warm, 0, true, false, true),
            None
        );
        assert!(
            scheduler.configure_warm_pending,
            "configure-warm scan must remain pending while a cold semantic seed is active"
        );
        assert_eq!(
            scheduler.tick_with_semantic_gate(warm + Duration::from_secs(1), 0, true, false, false),
            Some(Tier2TriggerReason::ConfigureWarm)
        );

        assert!(scheduler.request_pull(true));
        assert_eq!(
            scheduler.tick_with_semantic_gate(
                warm + TIER2_REFRESH_MIN_INTERVAL,
                0,
                true,
                false,
                true
            ),
            None
        );
        assert!(
            scheduler.pull_demand_pending(),
            "pull demand must not be consumed while a cold semantic seed is active"
        );
        assert_eq!(
            scheduler.tick_with_semantic_gate(
                warm + TIER2_REFRESH_MIN_INTERVAL + Duration::from_secs(1),
                0,
                true,
                false,
                false,
            ),
            Some(Tier2TriggerReason::Pull)
        );
    }

    #[test]
    fn deferred_debounce_keeps_trickle_in_the_quiet_window() {
        let (mut scheduler, base) = configured_scheduler();
        let first_change = base + TIER2_REFRESH_COLD_CACHE_DELAY;
        assert_eq!(scheduler.tick(first_change, 1, true, false), None);

        let first_deadline = first_change + TIER2_REFRESH_DEBOUNCE;
        assert_eq!(
            scheduler.tick(first_deadline, 0, true, false),
            Some(Tier2TriggerReason::Debounce)
        );
        scheduler.note_dispatch_deferred();

        let mut last_change = first_deadline;
        for edit in 1..=6 {
            last_change = first_deadline + Duration::from_secs(edit * 10);
            assert_eq!(
                scheduler.tick(last_change, 1, true, false),
                None,
                "edit {edit} must extend the quiet window instead of retrying dispatch"
            );
        }
        assert_eq!(
            scheduler.tick(
                last_change + TIER2_REFRESH_DEBOUNCE - Duration::from_secs(1),
                0,
                true,
                false,
            ),
            None
        );
        assert_eq!(
            scheduler.tick(last_change + TIER2_REFRESH_DEBOUNCE, 0, true, false),
            Some(Tier2TriggerReason::Debounce),
            "six edits over one minute should produce one dispatch after quiet"
        );
    }

    #[test]
    fn explicit_pull_mid_debounce_still_dispatches_immediately() {
        let (mut scheduler, base) = configured_scheduler();
        let change = base + TIER2_REFRESH_COLD_CACHE_DELAY;
        assert_eq!(scheduler.tick(change, 1, true, false), None);
        assert!(scheduler.request_pull(true));
        assert_eq!(
            scheduler.tick(change + Duration::from_secs(10), 0, true, false),
            Some(Tier2TriggerReason::Pull)
        );
    }

    #[test]
    fn worktree_bridge_never_schedules_write() {
        let (mut scheduler, base) = configured_scheduler();
        let warm = base + TIER2_REFRESH_COLD_CACHE_DELAY;
        assert_eq!(scheduler.tick(warm, 1, false, false), None);
        assert_eq!(
            scheduler.tick(warm + TIER2_REFRESH_MAX_STALENESS, 0, false, false),
            None
        );
        assert!(!scheduler.request_pull(false));
        assert_eq!(
            scheduler.tick(warm + TIER2_REFRESH_MAX_STALENESS * 2, 0, false, false),
            None
        );
    }

    #[test]
    fn pull_demand_sets_but_respects_min_interval_and_in_flight() {
        let (mut scheduler, base) = configured_scheduler();
        let first = base + TIER2_REFRESH_COLD_CACHE_DELAY;
        assert_eq!(
            scheduler.tick(first, 0, true, false),
            Some(Tier2TriggerReason::ConfigureWarm)
        );

        assert!(scheduler.request_pull(true));
        assert!(scheduler.pull_demand_pending());
        assert_eq!(
            scheduler.tick(first + Duration::from_secs(60), 0, true, false),
            None,
            "pull demand should wait for the min interval"
        );
        assert!(scheduler.pull_demand_pending());
        assert_eq!(
            scheduler.tick(first + TIER2_REFRESH_MIN_INTERVAL, 0, true, true),
            None,
            "pull demand should wait for in-flight tier2 work to finish"
        );
        assert!(scheduler.pull_demand_pending());
        assert_eq!(
            scheduler.tick(first + TIER2_REFRESH_MIN_INTERVAL, 0, true, false),
            Some(Tier2TriggerReason::Pull)
        );
    }
}
