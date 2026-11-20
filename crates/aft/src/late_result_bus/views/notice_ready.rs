use std::sync::{LazyLock, OnceLock};
use std::time::Duration;
use tokio::sync::watch;
use tokio::time::Instant;

pub(crate) static NOTICE_VIEWS_READY: LazyLock<NoticeViewsReady> =
    LazyLock::new(NoticeViewsReady::default);

/// The startup gate opens only after the durable bucket acknowledges all of
/// AFT's notice views. A reconnect does not revoke a successful publication.
#[derive(Default)]
pub(crate) struct NoticeViewsReady {
    ready: watch::Sender<bool>,
    deadline: OnceLock<Instant>,
}

impl NoticeViewsReady {
    pub(crate) fn confirm(&self) {
        self.ready.send_if_modified(|ready| {
            let changed = !*ready;
            *ready = true;
            changed
        });
    }

    pub(crate) async fn wait(&self, budget: Duration) -> bool {
        let mut ready = self.ready.subscribe();
        if *ready.borrow_and_update() {
            return true;
        }
        // All sends share one startup budget. A missing bus must not delay
        // each notice in a backlog (or prevent the original text from sending).
        let deadline = *self.deadline.get_or_init(|| {
            log::warn!("notice display records are waiting for aft.views publication");
            Instant::now() + budget
        });
        tokio::time::timeout_at(deadline, ready.wait_for(|ready| *ready))
            .await
            .is_ok_and(|result| result.is_ok())
    }
}
