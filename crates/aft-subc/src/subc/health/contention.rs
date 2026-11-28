use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use super::{demand, HealthStatus};

pub(super) const EXECUTOR_SCHEDULER_CONTENTION_DETAIL: &str =
    "executor scheduler state could not be snapshotted without contention";

// Three ordinary refreshes must miss their snapshots before contention becomes
// a degradation. The worker's one-second contention retries do not shorten this
// wall-clock grace period.
pub(super) const SNAPSHOT_CONTENTION_DEGRADED_AFTER: Duration =
    demand::BACKGROUND_INTERVAL.saturating_mul(3);

fn contention_is_degraded(elapsed_ms: u64) -> bool {
    elapsed_ms >= SNAPSHOT_CONTENTION_DEGRADED_AFTER.as_millis() as u64
}

pub(super) fn scheduler_contention_degraded(snapshot_age_ms: Option<u64>) -> bool {
    // An unavailable first census remains degraded: unlike a retained census,
    // it has no recent successful measurement that can excuse a missed refresh.
    snapshot_age_ms.is_none_or(contention_is_degraded)
}

#[derive(Default)]
pub(super) struct RootContention {
    // Last successful snapshot for each current root, on the health cache
    // clock. A root first seen contended uses that observation until it has a
    // success. Recovery replaces the timestamp; root removal clears the entry.
    last_successful_at_ms: BTreeMap<String, u64>,
}

impl RootContention {
    pub(super) fn observe(&mut self, roots: &[(&str, bool)], now_ms: u64) -> usize {
        let current = roots.iter().map(|(root, _)| *root).collect::<BTreeSet<_>>();
        self.last_successful_at_ms
            .retain(|root, _| current.contains(root.as_str()));
        for (root, busy) in roots {
            let last_successful = self
                .last_successful_at_ms
                .entry((*root).to_string())
                .or_insert(now_ms);
            if !*busy {
                *last_successful = now_ms;
            }
        }
        roots
            .iter()
            .filter(|(root, busy)| {
                *busy
                    && self
                        .last_successful_at_ms
                        .get(*root)
                        .is_some_and(|last_successful| {
                            contention_is_degraded(now_ms.saturating_sub(*last_successful))
                        })
            })
            .count()
    }
}

/// The detail names every currently contended root, but only roots whose misses
/// have lasted through the grace period contribute to the fleet verdict.
pub(super) fn root_health_rollup_state(
    busy_roots: usize,
    degraded_busy_roots: usize,
    standing_refusals: usize,
    warming_roots: usize,
    disk_reserve_detail: Option<String>,
) -> (HealthStatus, Option<String>) {
    let status =
        if degraded_busy_roots > 0 || standing_refusals > 0 || disk_reserve_detail.is_some() {
            HealthStatus::Degraded
        } else {
            HealthStatus::Ok
        };
    let detail = if busy_roots > 0 {
        Some(format!(
            "{busy_roots} root actor(s) could not be snapshotted without contention"
        ))
    } else if standing_refusals > 0 {
        Some(format!(
            "{standing_refusals} standing root(s) refused current resolved-path identity"
        ))
    } else if disk_reserve_detail.is_some() {
        disk_reserve_detail
    } else if warming_roots > 0 {
        Some(format!(
            "{warming_roots} root(s) warming background indexes (serving normally)"
        ))
    } else {
        None
    };
    (status, detail)
}
