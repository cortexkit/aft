//! When the status bar's TODO count is refreshed in the background.
//!
//! The whole-repository analyses (dead code, unused exports, duplicates,
//! cycles and complexity) never run in the background: only an explicit
//! `aft_inspect` computes them. The TODO count is cheap, and the status bar
//! shows it on every change, so this scheduler keeps it current: once after
//! configure, then after edits settle. It keeps the `Tier2` names it was
//! introduced with, when it also refreshed those analyses.

use std::time::{Duration, Instant};

pub const TIER2_REFRESH_DEBOUNCE: Duration = Duration::from_secs(45);
// Ceiling that forces a refresh during CONTINUOUS editing (when the debounce
// never gets its quiet window). Set high: a normal continuous-coding stretch
// should not trigger one.
pub const TIER2_REFRESH_MAX_STALENESS: Duration = Duration::from_secs(30 * 60);
pub const TIER2_REFRESH_MIN_INTERVAL: Duration = Duration::from_secs(5 * 60);
pub const TIER2_REFRESH_COLD_CACHE_DELAY: Duration = Duration::from_secs(90);
pub const TIER2_REFRESH_STORM_DEBOUNCE: Duration = Duration::from_secs(120);
pub const TIER2_REFRESH_STORM_PATH_THRESHOLD: usize = 200;
/// Minimum gap between two roots' first refreshes after configure.
///
/// Every root configured in the same burst (all sessions rebinding after a
/// daemon restart) used to come due at exactly `configured + 90s`, so dozens
/// of refreshes started together while the fleet was still rebinding. The
/// process-wide pacer hands out first-refresh start times at least this far
/// apart.
pub const TIER2_COLD_START_SPACING: Duration = Duration::from_secs(3);
/// Upper bound on how far past the cold-cache delay pacing may push one root's
/// first refresh. A burst larger than `MAX_SPREAD / SPACING` roots shares the
/// last slot rather than delaying later roots' first refresh without limit.
pub const TIER2_COLD_START_MAX_SPREAD: Duration = Duration::from_secs(5 * 60);

/// Hands out first-refresh times for roots configured close together, so
/// their first refreshes start one at a time instead of at once.
#[derive(Debug, Default)]
pub struct Tier2ColdStartPacer {
    last_slot: Option<Instant>,
}

impl Tier2ColdStartPacer {
    pub const fn new() -> Self {
        Self { last_slot: None }
    }

    /// Reserve the earliest moment a root configured at `now` may run its
    /// first refresh: never before `now + TIER2_REFRESH_COLD_CACHE_DELAY`,
    /// and at least `TIER2_COLD_START_SPACING` after the previous reservation.
    pub fn reserve(&mut self, now: Instant) -> Instant {
        let earliest = now + TIER2_REFRESH_COLD_CACHE_DELAY;
        let paced = self.last_slot.map_or(earliest, |last| {
            earliest.max(last + TIER2_COLD_START_SPACING)
        });
        let slot = paced.min(earliest + TIER2_COLD_START_MAX_SPREAD);
        self.last_slot = Some(self.last_slot.map_or(slot, |last| last.max(slot)));
        slot
    }
}

/// The pacer shared by every root this process hosts.
pub fn process_tier2_cold_start_pacer() -> &'static parking_lot::Mutex<Tier2ColdStartPacer> {
    static PACER: parking_lot::Mutex<Tier2ColdStartPacer> =
        parking_lot::Mutex::new(Tier2ColdStartPacer::new());
    &PACER
}

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

/// Why a dispatch did not happen even though the deadline this scheduler
/// works out (`next_dispatch_at`) has already passed. Reported once per
/// overdue stretch so the daemon log names the predicate holding a root's
/// refresh back.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tier2DispatchBlock {
    /// The root may not write inspect artifacts, or heavy root work is off.
    ReadOnly,
    /// An inspect analysis is still running for this root.
    InFlight,
    /// The semantic cold seed owns the machine until it finishes.
    SemanticColdSeed,
    /// The first refresh after configure is still inside its cold-cache delay.
    ColdCacheDelay,
    /// Nothing external is holding the dispatch back and no trigger matched:
    /// the deadline and the dispatch predicate disagree.
    NoTrigger,
}

impl Tier2DispatchBlock {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ReadOnly => "read_only",
            Self::InFlight => "tier2_in_flight",
            Self::SemanticColdSeed => "semantic_cold_seed",
            Self::ColdCacheDelay => "cold_cache_delay",
            Self::NoTrigger => "deadline_without_trigger",
        }
    }
}

/// Work on the same root, outside this scheduler, that a dispatch must wait
/// for. These are conditions other planes own, so they are passed in on every
/// tick rather than remembered here.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Tier2ExternalGates {
    /// The semantic cold seed owns the machine until it finishes.
    pub semantic_cold_seed_active: bool,
}

#[derive(Debug, Clone)]
pub struct Tier2RefreshScheduler {
    /// When the first refresh after the last configure may start: the
    /// cold-cache delay, pushed later by the process-wide pacer when many
    /// roots were configured together.
    cold_ready_at: Option<Instant>,
    last_change_at: Option<Instant>,
    activity_started_at: Option<Instant>,
    debounce_delay: Duration,
    last_scan_started_at: Option<Instant>,
    pull_demand_pending: bool,
    configure_warm_pending: bool,
    last_trigger_reason: Option<Tier2TriggerReason>,
    overdue_reported: bool,
}

impl Tier2RefreshScheduler {
    pub fn new() -> Self {
        Self {
            cold_ready_at: None,
            last_change_at: None,
            activity_started_at: None,
            debounce_delay: TIER2_REFRESH_DEBOUNCE,
            last_scan_started_at: None,
            pull_demand_pending: false,
            configure_warm_pending: false,
            last_trigger_reason: None,
            overdue_reported: false,
        }
    }

    pub fn reset_after_configure(&mut self, now: Instant) {
        self.reset_after_configure_with_cold_ready_at(now + TIER2_REFRESH_COLD_CACHE_DELAY);
    }

    /// Reset after a configure whose first refresh must wait until
    /// `cold_ready_at` (a slot from [`Tier2ColdStartPacer::reserve`]).
    pub fn reset_after_configure_with_cold_ready_at(&mut self, cold_ready_at: Instant) {
        self.cold_ready_at = Some(cold_ready_at);
        self.last_change_at = None;
        self.activity_started_at = None;
        self.debounce_delay = TIER2_REFRESH_DEBOUNCE;
        self.last_scan_started_at = None;
        self.pull_demand_pending = false;
        self.configure_warm_pending = true;
        self.last_trigger_reason = None;
        self.overdue_reported = false;
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
        self.tick_with_gates(
            now,
            changed_path_count,
            can_write,
            in_flight,
            Tier2ExternalGates::default(),
        )
    }

    pub fn tick_with_gates(
        &mut self,
        now: Instant,
        changed_path_count: usize,
        can_write: bool,
        in_flight: bool,
        gates: Tier2ExternalGates,
    ) -> Option<Tier2TriggerReason> {
        if changed_path_count > 0 {
            self.record_changes(now, changed_path_count);
        }

        if !can_write || in_flight {
            return None;
        }

        if gates.semantic_cold_seed_active {
            return None;
        }

        if !self.min_interval_elapsed(now) {
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
        self.overdue_reported = false;
        self.clear_activity_window();
    }

    pub fn last_trigger_reason(&self) -> Option<Tier2TriggerReason> {
        self.last_trigger_reason
    }

    pub fn pull_demand_pending(&self) -> bool {
        self.pull_demand_pending
    }

    /// Earliest instant at which the scheduler's current demand can dispatch,
    /// before external gates such as an in-flight analysis or the semantic
    /// cold seed.
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
            self.cold_ready_at
        } else {
            None
        }?;

        let cold_cache_at = (self.last_scan_started_at.is_none() && !self.pull_demand_pending)
            .then_some(self.cold_ready_at)
            .flatten();

        Some(
            [min_interval_at, cold_cache_at]
                .into_iter()
                .flatten()
                .fold(requested_at, Instant::max),
        )
    }

    /// True when the deadline this scheduler works out has arrived, so a tick
    /// now would dispatch unless an external gate blocks it.
    ///
    /// The maintenance tick uses this to pump the scheduler while a root sits
    /// quiet. Every other tick arrives with a watcher change in hand, which
    /// pushes the debounce forward — so the quiet window the debounce waits
    /// for is exactly the window in which nothing else would look.
    pub fn dispatch_due(&self, now: Instant) -> bool {
        self.next_dispatch_at(now)
            .is_some_and(|deadline| now >= deadline)
    }

    /// Name the predicate that held a dispatch back after the deadline
    /// passed, at most once per overdue stretch. Returns `None` while no
    /// deadline is due, when the current stretch was already reported, and
    /// again after a dispatch or reset clears the stretch.
    pub fn take_overdue_dispatch_block(
        &mut self,
        now: Instant,
        can_write: bool,
        in_flight: bool,
        gates: Tier2ExternalGates,
    ) -> Option<Tier2DispatchBlock> {
        let due = self.dispatch_due(now);
        if !due {
            self.overdue_reported = false;
            return None;
        }
        let block = if !can_write {
            Tier2DispatchBlock::ReadOnly
        } else if in_flight {
            Tier2DispatchBlock::InFlight
        } else if gates.semantic_cold_seed_active {
            Tier2DispatchBlock::SemanticColdSeed
        } else if !self.cold_delay_elapsed(now) {
            Tier2DispatchBlock::ColdCacheDelay
        } else {
            Tier2DispatchBlock::NoTrigger
        };
        if std::mem::replace(&mut self.overdue_reported, true) {
            return None;
        }
        Some(block)
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
            || self.cold_ready_at.is_some_and(|ready_at| now >= ready_at)
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
        self.last_scan_started_at = Some(now);
        self.pull_demand_pending = false;
        self.configure_warm_pending = false;
        self.last_trigger_reason = Some(reason);
        self.overdue_reported = false;
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

    fn no_gates() -> Tier2ExternalGates {
        Tier2ExternalGates::default()
    }

    fn semantic_cold_seed() -> Tier2ExternalGates {
        Tier2ExternalGates {
            semantic_cold_seed_active: true,
            ..Tier2ExternalGates::default()
        }
    }

    #[test]
    fn cold_refreshes_of_roots_configured_together_start_one_at_a_time() {
        const ROOTS: usize = 40;
        let base = Instant::now();
        let mut pacer = Tier2ColdStartPacer::new();
        let mut schedulers = (0..ROOTS)
            .map(|_| {
                let mut scheduler = Tier2RefreshScheduler::new();
                scheduler.reset_after_configure_with_cold_ready_at(pacer.reserve(base));
                scheduler
            })
            .collect::<Vec<_>>();

        // Tick every root once a second, as the maintenance loop would, from
        // the plain cold-cache deadline until the last paced slot.
        let first = base + TIER2_REFRESH_COLD_CACHE_DELAY;
        let last = first + TIER2_COLD_START_SPACING * (ROOTS as u32 - 1);
        let mut started_at = vec![None; ROOTS];
        let mut now = base + TIER2_REFRESH_COLD_CACHE_DELAY - Duration::from_secs(1);
        while now <= last {
            let mut started_this_tick = 0;
            for (index, scheduler) in schedulers.iter_mut().enumerate() {
                if started_at[index].is_none() && scheduler.tick(now, 0, true, false).is_some() {
                    started_at[index] = Some(now);
                    started_this_tick += 1;
                }
            }
            assert!(
                started_this_tick <= 1,
                "{started_this_tick} cold refreshes started together at +{:?}",
                now - base
            );
            now += Duration::from_secs(1);
        }

        assert!(
            started_at.iter().all(Option::is_some),
            "every root's cold refresh still runs by the last paced slot"
        );
        assert_eq!(started_at[0], Some(first), "the first root is not delayed");
        let mut starts = started_at.into_iter().flatten().collect::<Vec<_>>();
        starts.sort();
        for pair in starts.windows(2) {
            assert!(pair[1] - pair[0] >= TIER2_COLD_START_SPACING);
        }
    }

    #[test]
    fn cold_start_pacing_is_bounded_and_idle_after_a_quiet_gap() {
        let base = Instant::now();
        let mut pacer = Tier2ColdStartPacer::new();
        let slots = (0..1000).map(|_| pacer.reserve(base)).collect::<Vec<_>>();
        let ceiling = base + TIER2_REFRESH_COLD_CACHE_DELAY + TIER2_COLD_START_MAX_SPREAD;
        assert!(slots.iter().all(|slot| *slot <= ceiling));
        assert_eq!(slots.last().copied(), Some(ceiling));

        // A root configured long after the burst gets the plain delay.
        let later = ceiling + Duration::from_secs(60);
        assert_eq!(pacer.reserve(later), later + TIER2_REFRESH_COLD_CACHE_DELAY);
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
            scheduler.tick_with_gates(warm, 0, true, false, semantic_cold_seed()),
            None
        );
        assert!(
            scheduler.configure_warm_pending,
            "configure-warm scan must remain pending while a cold semantic seed is active"
        );
        assert_eq!(
            scheduler.tick_with_gates(warm + Duration::from_secs(1), 0, true, false, no_gates()),
            Some(Tier2TriggerReason::ConfigureWarm)
        );

        assert!(scheduler.request_pull(true));
        assert_eq!(
            scheduler.tick_with_gates(
                warm + TIER2_REFRESH_MIN_INTERVAL,
                0,
                true,
                false,
                semantic_cold_seed()
            ),
            None
        );
        assert!(
            scheduler.pull_demand_pending(),
            "pull demand must not be consumed while a cold semantic seed is active"
        );
        assert_eq!(
            scheduler.tick_with_gates(
                warm + TIER2_REFRESH_MIN_INTERVAL + Duration::from_secs(1),
                0,
                true,
                false,
                no_gates(),
            ),
            Some(Tier2TriggerReason::Pull)
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

    #[test]
    fn debounce_deadline_can_only_be_consumed_by_a_change_free_tick() {
        let (mut scheduler, base) = configured_scheduler();
        let change = base + TIER2_REFRESH_COLD_CACHE_DELAY;
        assert_eq!(scheduler.tick(change, 1, true, false), None);

        let deadline = change + TIER2_REFRESH_DEBOUNCE;
        assert_eq!(scheduler.next_dispatch_at(change), Some(deadline));
        assert!(!scheduler.dispatch_due(deadline - Duration::from_secs(1)));
        assert!(scheduler.dispatch_due(deadline));

        // A tick that arrives WITH a change restarts the quiet window it is
        // being measured against, so it can never satisfy its own debounce.
        // Only a tick with nothing new in hand can, and the watcher drain --
        // the scheduler's only tick site -- runs when paths arrive.
        let mut arriving_changes = scheduler.clone();
        assert_eq!(arriving_changes.tick(deadline, 1, true, false), None);
        assert_eq!(
            arriving_changes.tick(deadline + Duration::from_secs(1), 1, true, false),
            None
        );

        assert_eq!(
            scheduler.tick(deadline, 0, true, false),
            Some(Tier2TriggerReason::Debounce)
        );
    }

    #[test]
    fn overdue_deadline_names_the_blocking_predicate_once() {
        let (mut scheduler, base) = configured_scheduler();
        let change = base + TIER2_REFRESH_COLD_CACHE_DELAY;
        assert_eq!(scheduler.tick(change, 1, true, false), None);
        let deadline = change + TIER2_REFRESH_DEBOUNCE;

        assert_eq!(
            scheduler.take_overdue_dispatch_block(
                deadline - Duration::from_secs(1),
                true,
                false,
                no_gates()
            ),
            None,
            "a deadline that has not arrived is not overdue"
        );

        assert_eq!(
            scheduler.take_overdue_dispatch_block(deadline, true, true, no_gates()),
            Some(Tier2DispatchBlock::InFlight)
        );
        assert_eq!(
            scheduler.take_overdue_dispatch_block(
                deadline + Duration::from_secs(1),
                true,
                true,
                no_gates()
            ),
            None,
            "one overdue stretch reports once"
        );

        assert_eq!(
            scheduler.tick(deadline + Duration::from_secs(2), 0, true, false),
            Some(Tier2TriggerReason::Debounce)
        );
        assert_eq!(
            scheduler.take_overdue_dispatch_block(
                deadline + Duration::from_secs(3),
                true,
                true,
                no_gates()
            ),
            None,
            "a dispatched refresh ends the overdue stretch"
        );
    }

    #[test]
    fn overdue_block_distinguishes_read_only_and_semantic_cold_seed() {
        let (mut scheduler, base) = configured_scheduler();
        let change = base + TIER2_REFRESH_COLD_CACHE_DELAY;
        assert_eq!(scheduler.tick(change, 1, true, false), None);
        let deadline = change + TIER2_REFRESH_DEBOUNCE;

        assert_eq!(
            scheduler
                .clone()
                .take_overdue_dispatch_block(deadline, false, false, no_gates()),
            Some(Tier2DispatchBlock::ReadOnly)
        );
        assert_eq!(
            scheduler.take_overdue_dispatch_block(deadline, true, false, semantic_cold_seed()),
            Some(Tier2DispatchBlock::SemanticColdSeed)
        );
    }
}
