use crate::config::Config;
use std::sync::Arc;

/// A read view of the calling context's effective configuration.
#[doc(hidden)]
pub trait PinnedConfig {
    fn config(&self) -> Arc<Config>;
}
