use std::any::Any;
use std::ops::Deref;
use std::sync::Arc;

use crate::bash_changed_files::Journal;
use crate::watcher_backend::change_sink::WatcherChangeSink;
use crate::watcher_backend::counters::WatcherCounters;

#[path = "inspect_diagnostics_host.rs"]
mod inspect_diagnostics_host;

impl WatcherChangeSink for Journal {
    fn event_for_root(
        &self,
        event: &notify::Event,
        root: &std::path::Path,
        matcher: &crate::watcher_filter::SharedGitignore,
    ) {
        Journal::event_for_root(self, event, root, matcher);
    }

    fn gap(&self) {
        Journal::gap(self);
    }
}

/// Retains field-style access to Bash change tracking on a context's counters.
#[doc(hidden)]
pub struct WatcherCountersHandle {
    pub bash_changes: BashChanges,
}

impl WatcherCountersHandle {
    pub(crate) fn new(counters: Arc<WatcherCounters>) -> Self {
        Self {
            bash_changes: BashChanges(counters),
        }
    }
}

impl Deref for WatcherCountersHandle {
    type Target = WatcherCounters;

    fn deref(&self) -> &Self::Target {
        &self.bash_changes.0
    }
}

/// Typed access to the journal installed in the root's single watcher slot.
#[doc(hidden)]
pub struct BashChanges(Arc<WatcherCounters>);

impl BashChanges {
    fn journal(sink: &Arc<dyn WatcherChangeSink>) -> Arc<Journal> {
        let sink: Arc<dyn Any + Send + Sync> = sink.clone();
        sink.downcast().unwrap_or_else(|_| {
            panic!("WatcherCounters::bash_changes must hold a Bash change journal")
        })
    }

    #[cfg(test)]
    pub(crate) fn get(&self) -> Option<Arc<Journal>> {
        self.0.bash_changes().get().map(Self::journal)
    }

    #[doc(hidden)]
    pub fn get_or_init(&self, init: impl FnOnce() -> Arc<Journal>) -> Arc<Journal> {
        Self::journal(self.0.bash_changes().get_or_init(|| init()))
    }
}

impl super::AppContext {
    pub fn watcher_counters(&self) -> WatcherCountersHandle {
        WatcherCountersHandle::new(self.index_watcher_counters())
    }

    pub(crate) fn index_watcher_counters(&self) -> Arc<WatcherCounters> {
        Arc::clone(
            &self
                .watcher_counters
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        )
    }
}
