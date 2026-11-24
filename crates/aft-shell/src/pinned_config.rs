use crate::config::Config;
use std::sync::Arc;

/// A read view of the calling context's effective configuration.
#[doc(hidden)]
pub trait PinnedConfig {
    fn config(&self) -> Arc<Config>;
}

impl<T: PinnedConfig + ?Sized> PinnedConfig for Arc<T> {
    fn config(&self) -> Arc<Config> {
        (**self).config()
    }
}
