use super::*;

/// Sandbox-owned grants for one context and its weakly owned expiry timer.
#[doc(hidden)]
#[derive(Default)]
pub struct EscalationGrantState(parking_lot::Mutex<EscalationGrantStore>);

impl EscalationGrantState {
    pub(super) fn lock(&self) -> parking_lot::MutexGuard<'_, EscalationGrantStore> {
        self.0.lock()
    }
}

#[derive(Debug, Default)]
pub struct EscalationGrantStore {
    pub(super) grants: HashMap<String, EscalationGrant>,
    pub(super) provider_timer_running: bool,
}
