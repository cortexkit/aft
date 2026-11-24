use crate::{bash_background::TaskRegistryAccess, pinned_config::PinnedConfig};

/// The bound configuration and task artifacts visible to permission scanning.
#[doc(hidden)]
pub trait PermissionScanHost: PinnedConfig + TaskRegistryAccess {}
