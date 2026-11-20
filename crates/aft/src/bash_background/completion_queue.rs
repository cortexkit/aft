//! Bounds on the in-memory queue of completion notices awaiting delivery.
//!
//! A notice leaves the queue only when its session acknowledges it. A session
//! that stops acknowledging (often one that has ended) would otherwise keep
//! its notices in memory for the daemon's lifetime, and each response to that
//! session re-sends all of them. A notice dropped here stays undelivered on
//! disk, so restart replay offers it again.

use std::collections::VecDeque;

use super::BgCompletion;

/// Notices kept for one session.
const MAX_QUEUED_PER_SESSION: usize = 128;
/// Notices kept across all sessions.
const MAX_QUEUED: usize = 4096;

/// What the bounds need to know about a queued notice.
pub(super) trait QueuedNotice {
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

/// Queue `notice` unless its task already has one queued, then drop the
/// oldest notices past the bounds. Returns whether it was queued.
pub(super) fn push<N: QueuedNotice>(queue: &mut VecDeque<N>, notice: N) -> bool {
    if queue
        .iter()
        .any(|queued| queued.task_id() == notice.task_id())
    {
        return false;
    }
    let session = notice.session_id().to_string();
    queue.push_back(notice);
    let mut queued_for_session = queue
        .iter()
        .filter(|queued| queued.session_id() == session)
        .count();
    while queued_for_session > MAX_QUEUED_PER_SESSION {
        if let Some(oldest) = queue
            .iter()
            .position(|queued| queued.session_id() == session)
        {
            queue.remove(oldest);
        }
        queued_for_session -= 1;
    }
    while queue.len() > MAX_QUEUED {
        queue.pop_front();
    }
    true
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
    fn a_session_that_never_acknowledges_keeps_only_its_newest_notices() {
        let mut queue = VecDeque::new();
        for task in 0..MAX_QUEUED_PER_SESSION + 10 {
            assert!(push(&mut queue, notice("gone", task)));
        }
        assert!(push(&mut queue, notice("live", 1_000)));
        let gone: Vec<_> = queue
            .iter()
            .filter(|queued| queued.session_id() == "gone")
            .collect();
        assert_eq!(gone.len(), MAX_QUEUED_PER_SESSION);
        assert_eq!(gone[0].task_id(), format!("bash-{:016x}", 10));
        assert!(queue.iter().any(|queued| queued.session_id() == "live"));
        // A task's notice is queued once.
        assert!(!push(&mut queue, notice("live", 1_000)));
    }

    #[test]
    fn the_whole_queue_is_bounded_across_sessions() {
        let mut queue = VecDeque::new();
        for task in 0..MAX_QUEUED + 5 {
            push(&mut queue, notice(&format!("session-{}", task % 64), task));
        }
        assert_eq!(queue.len(), MAX_QUEUED);
    }
}
