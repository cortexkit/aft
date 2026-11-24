use std::any::Any;
use std::fmt::Debug;
use std::path::Path;
use std::sync::{Arc, OnceLock};

use crate::watcher_filter::SharedGitignore;

/// The root's watcher can deliver events and losses without owning shell state.
#[doc(hidden)]
pub trait WatcherChangeSink: Any + Debug + Send + Sync {
    fn event_for_root(&self, event: &notify::Event, root: &Path, matcher: &SharedGitignore);
    fn gap(&self);
}

/// The journal is installed lazily and stays with the root's shared counters.
#[derive(Debug, Default)]
#[doc(hidden)]
pub struct WatcherChanges(OnceLock<Arc<dyn WatcherChangeSink>>);

impl WatcherChanges {
    #[doc(hidden)]
    pub fn get(&self) -> Option<&Arc<dyn WatcherChangeSink>> {
        self.0.get()
    }

    /// For composition, which owns the only initializer of this slot.
    #[doc(hidden)]
    pub fn get_or_init(
        &self,
        init: impl FnOnce() -> Arc<dyn WatcherChangeSink>,
    ) -> &Arc<dyn WatcherChangeSink> {
        self.0.get_or_init(init)
    }
}

impl super::counters::WatcherCounters {
    #[doc(hidden)]
    pub fn bash_changes(&self) -> &WatcherChanges {
        &self.bash_changes
    }
}
