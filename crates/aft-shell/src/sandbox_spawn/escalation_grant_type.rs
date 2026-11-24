use super::*;

#[derive(Debug, Default)]
pub struct EscalationGrantStore {
    pub(super) grants: HashMap<String, EscalationGrant>,
    pub(super) provider_timer_running: bool,
}
