use std::collections::VecDeque;
use std::sync::{LazyLock, OnceLock};
use std::time::Duration;
use tokio::sync::watch;
use tokio::time::Instant;

pub struct PullRecord {
    pub id: String,
    pub bytes: Vec<u8>,
}

const MAX_PENDING: usize = 64;

pub static NOTICE_VIEWS_READY: LazyLock<NoticeViewsReady> =
    LazyLock::new(NoticeViewsReady::default);

#[derive(Clone, Copy, Default, PartialEq, Eq)]
enum Publication {
    #[default]
    Waiting,
    Published,
    Failed,
}

#[derive(Default)]
struct Startup {
    status: Publication,
    pending: VecDeque<PullRecord>,
    overflow_warned: bool,
}

/// The startup gate opens only after the durable bucket acknowledges all of
/// AFT's notice views. A reconnect does not revoke a successful publication.
#[derive(Default)]
pub struct NoticeViewsReady {
    startup: watch::Sender<Startup>,
    deadline: OnceLock<Instant>,
}

impl NoticeViewsReady {
    pub(crate) fn confirm(&self) {
        self.startup.send_if_modified(|startup| {
            if startup.status != Publication::Waiting {
                return false;
            }
            startup.status = Publication::Published;
            true
        });
    }

    pub(crate) fn fail(&self, reason: &'static str) {
        self.startup.send_if_modified(|startup| {
            if startup.status != Publication::Waiting {
                return false;
            }
            startup.status = Publication::Failed;
            if !startup.pending.is_empty() {
                log::warn!(
                    "discarding {} pending notice display records: view publication stopped ({reason})",
                    startup.pending.len(),
                );
                startup.pending.clear();
            }
            true
        });
    }

    pub async fn wait(&self, budget: Duration) -> bool {
        let mut ready = self.startup.subscribe();
        let status = ready.borrow_and_update().status;
        if status != Publication::Waiting {
            return status == Publication::Published;
        }
        // All sends share one startup budget. A missing bus must not delay
        // each notice in a backlog (or prevent the original text from sending).
        let deadline = *self.deadline.get_or_init(|| {
            log::warn!("notice display records are waiting for aft.views publication");
            Instant::now() + budget
        });
        let result = tokio::time::timeout_at(
            deadline,
            ready.wait_for(|startup| startup.status != Publication::Waiting),
        )
        .await;
        matches!(result, Ok(Ok(startup)) if startup.status == Publication::Published)
    }

    pub fn defer(&self, record: PullRecord) {
        self.startup.send_if_modified(|startup| {
            if startup.status == Publication::Failed {
                return false;
            }
            if startup.pending.len() == MAX_PENDING {
                startup.pending.pop_front();
                if !startup.overflow_warned {
                    log::warn!("notice display startup queue is full; dropping oldest records");
                    startup.overflow_warned = true;
                }
            }
            startup.pending.push_back(record);
            // A send can exhaust its budget just as the views are published.
            // Wake the drain even if it already handled the earlier records.
            startup.status == Publication::Published
        });
    }

    /// Wait for a deferred record, or return `None` if publication cannot succeed.
    /// Only one drain task may consume these records.
    pub async fn next_pending(&self) -> Option<PullRecord> {
        let mut ready = self.startup.subscribe();
        drop(
            ready
                .wait_for(|startup| match startup.status {
                    Publication::Waiting => false,
                    Publication::Published => !startup.pending.is_empty(),
                    Publication::Failed => true,
                })
                .await
                .ok()?,
        );
        let mut record = None;
        self.startup.send_if_modified(|startup| {
            record = startup.pending.pop_front();
            startup.overflow_warned = false;
            false
        });
        record
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::{poll, FutureExt};

    #[tokio::test]
    async fn pending_notice_queue_keeps_the_newest_64_in_order() {
        let views = NoticeViewsReady::default();
        for i in 0..66 {
            views.defer(PullRecord {
                id: i.to_string(),
                bytes: Vec::new(),
            });
        }
        assert!(views.next_pending().now_or_never().is_none());
        views.confirm();
        for i in 2..66 {
            let record = views.next_pending().now_or_never().flatten().unwrap();
            assert_eq!(record.id, i.to_string());
        }
        assert!(views.next_pending().now_or_never().is_none());
    }

    #[tokio::test]
    async fn notice_deferring_at_publication_boundary_wakes_idle_drain() {
        let views = NoticeViewsReady::default();
        views.confirm();
        let mut next = std::pin::pin!(views.next_pending());
        assert!(poll!(&mut next).is_pending());
        views.defer(PullRecord {
            id: "late".into(),
            bytes: Vec::new(),
        });
        assert_eq!(next.now_or_never().flatten().unwrap().id, "late");
    }
}
