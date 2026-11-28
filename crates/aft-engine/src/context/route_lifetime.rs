use super::{App, AppContext};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

impl App {
    pub fn set_open_route_count(&self, count: usize) {
        self.open_routes.store(count, Ordering::SeqCst);
    }

    pub fn open_route_count(&self) -> usize {
        self.open_routes.load(Ordering::SeqCst)
    }
}

/// Owned by an installed route that can open meaning search for this actor.
#[doc(hidden)]
#[derive(Debug)]
pub struct MeaningOpenerGuard(Arc<AtomicUsize>);

impl Drop for MeaningOpenerGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

impl AppContext {
    #[doc(hidden)]
    pub fn track_meaning_opener(&self) -> MeaningOpenerGuard {
        self.meaning_openers.fetch_add(1, Ordering::AcqRel);
        MeaningOpenerGuard(Arc::clone(&self.meaning_openers))
    }

    #[doc(hidden)]
    pub fn has_meaning_opener(&self) -> bool {
        self.meaning_openers.load(Ordering::Acquire) != 0
    }
}
