//! Retained configure request and its generation fence.
use super::*;

impl AppContext {
    /// A connect is starting: any deferred cache recovery queued for an
    /// earlier connect is now stale.
    pub fn note_connect_started(&self) {
        self.connect_generation.fetch_add(1, Ordering::SeqCst);
    }

    /// A connect was refused and the configuration it would have replaced
    /// stays active: give that configuration's retained request the current
    /// connect generation, so later cache recoveries re-run it again. Work
    /// queued before the refused connect keeps the older generation and is
    /// still dropped.
    pub fn restamp_cache_recovery_request(&self) {
        let generation = self.connect_generation();
        if let Some((_, _, stamped)) = self.cache_recovery.lock().as_mut() {
            *stamped = generation;
        }
    }

    pub fn connect_generation(&self) -> u64 {
        self.connect_generation.load(Ordering::SeqCst)
    }

    pub fn retain_cache_recovery_request(&self, request: &crate::protocol::RawRequest) {
        let generation = self.connect_generation();
        *self.cache_recovery.lock() = self
            .config()
            .cache_dir
            .as_ref()
            .map(|_| (request.clone(), None, generation));
    }

    pub fn retained_configure_request(&self) -> Option<(crate::protocol::RawRequest, u64)> {
        self.cache_recovery
            .lock()
            .as_ref()
            .map(|(request, _, generation)| (request.clone(), *generation))
    }

    /// The retained connect request, with the connect generation it belongs
    /// to, at most once per minute.
    pub fn cache_recovery_request_if_due(
        &self,
        now: Instant,
    ) -> Option<(crate::protocol::RawRequest, u64)> {
        let mut recovery = self.cache_recovery.lock();
        let (request, checked, generation) = recovery.as_mut()?;
        if checked.is_some_and(|last| now.saturating_duration_since(last) < Duration::from_secs(60))
        {
            return None;
        }
        *checked = Some(now);
        Some((request.clone(), *generation))
    }
}
