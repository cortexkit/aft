use super::*;

#[derive(Clone)]
pub struct LaunchContext {
    pub(crate) seed: LaunchSeed,
    pub(crate) service: Arc<Service>,
}

#[derive(Clone)]
pub struct PreparedLaunch {
    pub(crate) context: LaunchContext,
    pub(crate) policy: Option<PolicySnapshot>,
    pub(crate) classification: Option<PolicySnapshot>,
    pub(crate) bindings: BTreeMap<String, Binding>,
    pub(crate) denial: Option<Refusal>,
    pub(crate) provisioned: bool,
}
