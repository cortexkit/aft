use crate::{access_policy::AccessPolicy, pinned_config::PinnedConfig, protocol::Response};
use std::path::PathBuf;

#[cfg(target_os = "linux")]
#[doc(hidden)]
pub trait ConfigPin {}

/// The access policy and escalation state used to prepare a child process.
#[doc(hidden)]
pub trait SandboxHost: PinnedConfig + Sync {
    fn access_policy(&self, id: &str) -> Result<AccessPolicy, Response>;
    fn storage_dir(&self) -> PathBuf;
    #[cfg(unix)]
    fn escalation_grants(&self) -> &std::sync::Arc<super::EscalationGrantState>;
    #[cfg(target_os = "linux")]
    fn pin_config(&self) -> Box<dyn ConfigPin + '_>;
}
