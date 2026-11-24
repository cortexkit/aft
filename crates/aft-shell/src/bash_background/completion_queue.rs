//! The in-memory queue of completion notices awaiting delivery, and its
//! bounds.
//!
//! A notice leaves the queue only when its session acknowledges it. A session
//! that stops acknowledging (often one that has ended) would otherwise keep
//! its notices in memory for the daemon's lifetime, and each response to that
//! session re-sends all of them. So the queue holds a bounded number of
//! notices per session and overall. A notice that finds the queue full waits
//! as its task id; as the session acknowledges what is queued, the queue
//! refills from those tasks oldest first, a bounded batch at a time.

use std::collections::{HashMap, VecDeque};
use std::ops::{Deref, DerefMut};

use super::*;

/// Notices queued for one session.
pub(crate) const MAX_QUEUED_PER_SESSION: usize = 128;
/// Notices queued across all sessions.
const MAX_QUEUED: usize = 4096;
/// Notices one refill loads for a session.
const REFILL_BATCH: usize = 32;

/// What the bounds need to know about a queued notice.
pub(crate) trait QueuedNotice {
    fn task_id(&self) -> &str;
    fn session_id(&self) -> &str;
}

impl QueuedNotice for BgCompletion {
    fn task_id(&self) -> &str {
        &self.task_id
    }
    fn session_id(&self) -> &str {
        &self.session_id
    }
}

/// Queued notices in delivery order, plus the notices that found the queue
/// full, in the order they arrived.
pub(crate) struct NoticeQueue<N> {
    queued: VecDeque<N>,
    /// (session id, task id) of each notice waiting outside the queue.
    overflowed: VecDeque<(String, String)>,
}

impl<N> Default for NoticeQueue<N> {
    fn default() -> Self {
        Self {
            queued: VecDeque::new(),
            overflowed: VecDeque::new(),
        }
    }
}

impl<N> Deref for NoticeQueue<N> {
    type Target = VecDeque<N>;
    fn deref(&self) -> &VecDeque<N> {
        &self.queued
    }
}

impl<N> DerefMut for NoticeQueue<N> {
    fn deref_mut(&mut self) -> &mut VecDeque<N> {
        &mut self.queued
    }
}

impl<N: QueuedNotice> NoticeQueue<N> {
    fn queued_for(&self, session: &str) -> usize {
        self.queued
            .iter()
            .filter(|queued| queued.session_id() == session)
            .count()
    }

    /// Queue `notice` unless its task already has one queued or the queue is
    /// full for its session or overall; a full queue remembers the task so a
    /// refill can load its notice later. Returns whether it was queued.
    pub(crate) fn push(&mut self, notice: N) -> bool {
        if self
            .queued
            .iter()
            .any(|queued| queued.task_id() == notice.task_id())
        {
            return false;
        }
        if self.queued.len() >= MAX_QUEUED
            || self.queued_for(notice.session_id()) >= MAX_QUEUED_PER_SESSION
        {
            if !self
                .overflowed
                .iter()
                .any(|(_, task)| task == notice.task_id())
            {
                self.overflowed.push_back((
                    notice.session_id().to_string(),
                    notice.task_id().to_string(),
                ));
            }
            return false;
        }
        self.queued.push_back(notice);
        true
    }

    /// Take the task ids of the next waiting notices that fit, oldest first:
    /// at most [`REFILL_BATCH`] per session and never past either bound.
    fn take_refill(&mut self) -> Vec<String> {
        let mut free_overall = MAX_QUEUED.saturating_sub(self.queued.len());
        let mut taken_for: HashMap<String, usize> = HashMap::new();
        let mut taken = Vec::new();
        let mut index = 0;
        while index < self.overflowed.len() && free_overall > 0 {
            let session = &self.overflowed[index].0;
            let already = taken_for.get(session).copied().unwrap_or(0);
            let free = MAX_QUEUED_PER_SESSION.saturating_sub(self.queued_for(session) + already);
            if already >= REFILL_BATCH || free == 0 {
                index += 1;
                continue;
            }
            let (session, task) = self.overflowed.remove(index).expect("index in range");
            *taken_for.entry(session).or_default() += 1;
            taken.push(task);
            free_overall -= 1;
        }
        taken
    }
}

impl BgTaskRegistry {
    /// Refill the queue from notices that found it full, oldest first, as
    /// acknowledgements make room. Each is enqueued again from its task, which
    /// re-checks the delivered mark and the queued task ids, so a notice is
    /// never delivered twice; one that finds the queue full again waits again.
    pub(super) fn refill_completion_queue(&self) {
        let Some(waiting) = self
            .inner
            .completions
            .lock()
            .ok()
            .map(|mut queue| queue.take_refill())
        else {
            return;
        };
        for task_id in waiting {
            let Some(task) = self.task(&task_id) else {
                continue;
            };
            let Some(metadata) = task.lock_state().ok().map(|state| state.metadata.clone()) else {
                continue;
            };
            // Its first routing found the queue full and queued nothing; let
            // this enqueue route it again.
            if let Ok(mut routed) = task.terminal_notification_routed.lock() {
                *routed = false;
            }
            self.enqueue_completion_if_needed(&metadata, Some(&task.paths), true);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Notice(String, String);

    impl QueuedNotice for Notice {
        fn task_id(&self) -> &str {
            &self.0
        }
        fn session_id(&self) -> &str {
            &self.1
        }
    }

    fn notice(session: &str, task: usize) -> Notice {
        Notice(format!("bash-{task:016x}"), session.to_string())
    }

    #[test]
    fn a_full_session_keeps_its_oldest_notices_and_refills_in_order() {
        let mut queue = NoticeQueue::default();
        for task in 0..MAX_QUEUED_PER_SESSION + 40 {
            assert_eq!(
                queue.push(notice("busy", task)),
                task < MAX_QUEUED_PER_SESSION
            );
        }
        assert!(queue.push(notice("other", 1_000)));
        // Refused again while waiting: still one entry for the task.
        assert!(!queue.push(notice("busy", MAX_QUEUED_PER_SESSION)));
        assert_eq!(queue.queued_for("busy"), MAX_QUEUED_PER_SESSION);
        assert!(queue.take_refill().is_empty(), "no room until an ack");
        for _ in 0..40 {
            queue.pop_front();
        }
        let refill = queue.take_refill();
        assert_eq!(refill.len(), REFILL_BATCH);
        assert_eq!(refill[0], format!("bash-{:016x}", MAX_QUEUED_PER_SESSION));
        assert_eq!(
            refill[REFILL_BATCH - 1],
            format!("bash-{:016x}", MAX_QUEUED_PER_SESSION + REFILL_BATCH - 1)
        );
        // A task's notice is queued once.
        assert!(!queue.push(notice("other", 1_000)));
    }

    #[test]
    fn the_whole_queue_is_bounded_across_sessions() {
        let mut queue = NoticeQueue::default();
        for task in 0..MAX_QUEUED + 5 {
            queue.push(notice(&format!("session-{}", task % 64), task));
        }
        assert_eq!(queue.len(), MAX_QUEUED);
    }

    #[test]
    fn acknowledgements_deliver_every_notice_past_the_cap_without_a_restart() {
        let fixture = tempfile::tempdir().unwrap();
        let registry = BgTaskRegistry::default();
        let total = MAX_QUEUED_PER_SESSION * 2 + 7;
        let mut finished = Vec::new();
        for _ in 0..total {
            let (task_id, task) = super::super::tests::insert_terminal_piped_task(
                &registry, &fixture, "cmd", "done\n", "", false,
            );
            let metadata = task.lock_state().unwrap().metadata.clone();
            registry.enqueue_completion_if_needed(&metadata, Some(&task.paths), false);
            finished.push(task_id);
        }
        let mut delivered = Vec::new();
        loop {
            let resident = registry.drain_completions_for_session(Some("session"));
            assert!(
                resident.len() <= MAX_QUEUED_PER_SESSION,
                "{}",
                resident.len()
            );
            if resident.is_empty() {
                break;
            }
            let ids: Vec<String> = resident.into_iter().map(|notice| notice.task_id).collect();
            for id in &ids {
                assert!(!delivered.contains(id), "{id} delivered twice");
            }
            registry.ack_completions_for_session(Some("session"), &ids);
            delivered.extend(ids);
        }
        // Every notice arrived, in the order its task finished.
        assert_eq!(delivered, finished);
    }
}
