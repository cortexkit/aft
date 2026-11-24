use crate::{access_policy::AccessPolicy, pinned_config::PinnedConfig, protocol::Response};
use std::{path::PathBuf, sync::Arc};

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

impl<T: SandboxHost + Send + ?Sized> SandboxHost for Arc<T> {
    fn access_policy(&self, id: &str) -> Result<AccessPolicy, Response> {
        (**self).access_policy(id)
    }

    fn storage_dir(&self) -> PathBuf {
        (**self).storage_dir()
    }

    #[cfg(unix)]
    fn escalation_grants(&self) -> &Arc<super::EscalationGrantState> {
        (**self).escalation_grants()
    }

    #[cfg(target_os = "linux")]
    fn pin_config(&self) -> Box<dyn ConfigPin + '_> {
        (**self).pin_config()
    }
}
