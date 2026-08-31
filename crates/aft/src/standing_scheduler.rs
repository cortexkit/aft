use std::collections::{HashMap, HashSet, VecDeque};
use std::hash::Hash;

#[derive(Debug)]
pub struct DeficitRoundRobin<K> {
    quantum: u64,
    queue: VecDeque<K>,
    deficits: HashMap<K, i128>,
    in_flight: HashSet<K>,
}

impl<K> DeficitRoundRobin<K>
where
    K: Clone + Eq + Hash,
{
    pub fn new(quantum: u64) -> Self {
        assert!(quantum > 0, "DRR quantum must be positive");
        Self {
            quantum,
            queue: VecDeque::new(),
            deficits: HashMap::new(),
            in_flight: HashSet::new(),
        }
    }

    pub fn reconcile<I>(&mut self, keys: I)
    where
        I: IntoIterator<Item = K>,
    {
        let keys = keys.into_iter().collect::<Vec<_>>();
        let active = keys.iter().cloned().collect::<HashSet<_>>();
        self.queue.retain(|key| active.contains(key));
        self.deficits.retain(|key, _| active.contains(key));
        self.in_flight.retain(|key| active.contains(key));
        for key in keys {
            if !self.deficits.contains_key(&key) {
                self.deficits.insert(key.clone(), 0);
                self.queue.push_back(key);
            }
        }
    }

    pub fn next(&mut self) -> Option<K> {
        let rounds = self.queue.len();
        for _ in 0..rounds {
            let Some(key) = self.queue.pop_front() else {
                return None;
            };
            // A key can sit in the queue without a deficits entry when a
            // reconcile removed its root after complete() requeued it. Skip
            // the stale key instead of aborting the whole dispatch turn.
            let Some(deficit) = self.deficits.get_mut(&key) else {
                continue;
            };
            *deficit += i128::from(self.quantum);
            if *deficit >= 0 {
                self.in_flight.insert(key.clone());
                return Some(key);
            }
            self.queue.push_back(key);
        }
        None
    }

    pub fn complete(&mut self, key: K, cost: u64, has_more: bool) {
        if !self.in_flight.remove(&key) {
            return;
        }
        if let Some(deficit) = self.deficits.get_mut(&key) {
            *deficit -= i128::from(cost);
        }
        if has_more {
            // Only requeue roots that still have a deficits entry; a
            // reconcile may have removed the root while its slice ran.
            if self.deficits.contains_key(&key) {
                self.queue.push_back(key);
            }
        } else {
            self.deficits.remove(&key);
        }
    }

    pub fn len(&self) -> usize {
        self.deficits.len()
    }

    pub fn is_empty(&self) -> bool {
        self.deficits.is_empty()
    }
}
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize)]
pub struct StandingSchedulerTelemetry {
    pub queued_roots: usize,
    pub running_slices: usize,
    pub completed_slices: u64,
    pub yielded_slices: u64,
    pub pause_reason: Option<String>,
    pub resource_policy: String,
}

static TELEMETRY: std::sync::LazyLock<parking_lot::RwLock<StandingSchedulerTelemetry>> =
    std::sync::LazyLock::new(|| parking_lot::RwLock::new(StandingSchedulerTelemetry::default()));

pub fn publish_telemetry(snapshot: StandingSchedulerTelemetry) {
    *TELEMETRY.write() = snapshot;
}

pub fn telemetry() -> StandingSchedulerTelemetry {
    TELEMETRY.read().clone()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn active_roots_rotate_without_starvation() {
        let mut scheduler = DeficitRoundRobin::new(10);
        scheduler.reconcile(["large", "small", "medium"]);
        let mut order = Vec::new();
        for cost in [10, 10, 10, 10, 10, 10] {
            let key = scheduler.next().unwrap();
            order.push(key);
            scheduler.complete(key, cost, true);
        }
        assert_eq!(
            order,
            ["large", "small", "medium", "large", "small", "medium"]
        );
    }

    #[test]
    fn expensive_root_pays_debt_before_its_next_slice() {
        let mut scheduler = DeficitRoundRobin::new(10);
        scheduler.reconcile(["large", "small"]);
        let large = scheduler.next().unwrap();
        scheduler.complete(large, 30, true);
        let small = scheduler.next().unwrap();
        scheduler.complete(small, 5, true);
        assert_eq!(scheduler.next(), Some("small"));
    }

    #[test]
    fn reconcile_removes_stale_and_appends_new_roots_deterministically() {
        let mut scheduler = DeficitRoundRobin::new(10);
        scheduler.reconcile(["a", "b"]);
        let a = scheduler.next().unwrap();
        scheduler.complete(a, 10, true);
        scheduler.reconcile(["b", "c"]);
        assert_eq!(scheduler.next(), Some("b"));
        scheduler.complete("b", 10, true);
        assert_eq!(scheduler.next(), Some("c"));
    }

    #[test]
    fn completed_root_leaves_the_queue() {
        let mut scheduler = DeficitRoundRobin::new(10);
        scheduler.reconcile(["a", "b"]);
        let a = scheduler.next().unwrap();
        scheduler.complete(a, 3, false);
        assert_eq!(scheduler.len(), 1);
        assert_eq!(scheduler.next(), Some("b"));
    }

    #[test]
    fn up_to_two_distinct_roots_can_be_in_flight() {
        let mut scheduler = DeficitRoundRobin::new(10);
        scheduler.reconcile(["a", "b", "c"]);
        assert_eq!(scheduler.next(), Some("a"));
        assert_eq!(scheduler.next(), Some("b"));
        scheduler.complete("a", 10, true);
        scheduler.complete("b", 10, true);
        assert_eq!(scheduler.next(), Some("c"));
    }

    #[test]
    fn scheduler_telemetry_round_trips_health_fields() {
        let expected = StandingSchedulerTelemetry {
            queued_roots: 8,
            running_slices: 2,
            completed_slices: 21,
            yielded_slices: 3,
            pause_reason: Some("io_pressure".to_string()),
            resource_policy: "balanced".to_string(),
        };
        publish_telemetry(expected.clone());
        assert_eq!(telemetry(), expected);
    }

    #[test]
    fn next_skips_stale_queued_keys_without_aborting_dispatch() {
        let mut scheduler = DeficitRoundRobin::new(100);
        scheduler.reconcile(["a", "b"]);
        assert_eq!(scheduler.next(), Some("a"));
        // Simulate a reconcile removing "a" after complete() requeued it:
        // the queue holds "a" but deficits no longer contains it.
        scheduler.deficits.remove("a");
        // next() must skip the stale "a" and still dispatch "b".
        assert_eq!(scheduler.next(), Some("b"));
    }

    #[test]
    fn complete_ignores_requeue_for_removed_root() {
        let mut scheduler = DeficitRoundRobin::new(100);
        scheduler.reconcile(["a", "b"]);
        assert_eq!(scheduler.next(), Some("a"));
        // Root "a" is removed while its slice runs.
        scheduler.deficits.remove("a");
        scheduler.complete("a", 10, true);
        // "a" must not be requeued without a deficits entry.
        assert!(!scheduler.queue.contains(&"a"));
    }
}
