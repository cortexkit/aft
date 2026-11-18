use super::*;

impl Drop for DocumentPullWorker {
    fn drop(&mut self) {
        if let Some(owner) = self.0.upgrade() {
            let mut manager = owner.lock();
            manager.background_pull_running = false;
            manager.background_pull_inflight = false;
            for waiter in std::mem::take(&mut manager.background_pull_idle_waiters) {
                let _ = waiter.send(());
            }
            manager.start_background_document_pull_worker();
        }
    }
}
