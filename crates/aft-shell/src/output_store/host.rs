use crate::{bash_background::TaskRegistryAccess, harness::Harness, pinned_config::PinnedConfig};
use std::{path::PathBuf, sync::Arc};

/// The output namespace and live output index routed for a request.
#[doc(hidden)]
pub trait OutputStoreHost: PinnedConfig + TaskRegistryAccess {
    fn storage_dir(&self) -> PathBuf;
    fn harness_opt(&self) -> Option<Harness>;
    fn output_files_arc(&self) -> Arc<super::OutputFiles>;
}
