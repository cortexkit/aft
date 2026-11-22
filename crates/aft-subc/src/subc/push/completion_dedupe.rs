use super::*;

pub(super) type TaskKey = (ProjectRootId, String);
const TOMBSTONE_CAP: usize = 4_096;
const TOMBSTONE_TTL: Duration = Duration::from_secs(10 * 60);

#[derive(Debug, Default)]
pub(super) struct Tombstones {
    keys: HashSet<TaskKey>,
    order: VecDeque<(TaskKey, Instant)>,
}

impl Tombstones {
    pub(super) fn contains(&mut self, key: &TaskKey) -> bool {
        self.expire(Instant::now());
        self.keys.contains(key)
    }

    pub(super) fn insert(&mut self, key: TaskKey) {
        let now = Instant::now();
        self.expire(now);
        if self.keys.insert(key.clone()) {
            self.order.push_back((key, now));
        }
        if self.order.len() > TOMBSTONE_CAP {
            self.keys.remove(&self.order.pop_front().unwrap().0);
        }
    }

    fn expire(&mut self, now: Instant) {
        while self
            .order
            .front()
            .is_some_and(|(_, at)| now.saturating_duration_since(*at) >= TOMBSTONE_TTL)
        {
            self.keys.remove(&self.order.pop_front().unwrap().0);
        }
    }
}

pub(super) fn finished(state: &std::sync::Mutex<CompletionOffers>, key: TaskKey) {
    let mut state = state.lock().unwrap();
    state.settling.remove(&key);
    state.admitted.remove(&key);
    state.recipients.remove(&key);
    state.settled.insert(key);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn completion_tombstones_bound_delivered_history_by_age_and_count() {
        let (_dir, root) = test_support::test_root("completion-tombstones");
        let mut tombstones = Tombstones::default();
        for i in 0..=TOMBSTONE_CAP {
            tombstones.insert((root.clone(), i.to_string()));
        }
        assert!(!tombstones.contains(&(root.clone(), "0".into())));
        assert!(tombstones.contains(&(root, "1".into())));
        assert_eq!(tombstones.keys.len(), TOMBSTONE_CAP);
        tombstones.expire(Instant::now() + TOMBSTONE_TTL);
        assert!(tombstones.keys.is_empty());
        assert!(tombstones.order.is_empty());
    }
}
