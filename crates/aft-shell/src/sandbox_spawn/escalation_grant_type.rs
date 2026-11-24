use super::*;

/// Sandbox-owned grants for one context and its weakly owned expiry timer.
#[doc(hidden)]
#[derive(Default)]
pub struct EscalationGrantState(parking_lot::Mutex<EscalationGrantStore>);

impl EscalationGrantState {
    pub(super) fn inner_lock(&self) -> parking_lot::MutexGuard<'_, EscalationGrantStore> {
        self.0.lock()
    }

    #[cfg(feature = "test-support")]
    #[doc(hidden)]
    pub fn lock(&self) -> EscalationGrantInspection<'_> {
        EscalationGrantInspection(self.inner_lock())
    }
}

#[cfg(feature = "test-support")]
#[doc(hidden)]
pub struct EscalationGrantInspection<'a>(parking_lot::MutexGuard<'a, EscalationGrantStore>);

#[cfg(feature = "test-support")]
impl EscalationGrantInspection<'_> {
    pub(crate) fn count(&self) -> usize {
        self.0.len_for_test()
    }
}

#[derive(Debug, Default)]
pub struct EscalationGrantStore {
    pub(super) grants: HashMap<String, EscalationGrant>,
    pub(super) provider_timer_running: bool,
}
