use super::AppContext;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

impl AppContext {
    /// Monotonic generation bumped after every matcher rebuild/clear. The
    /// watcher filter thread uses it to wait until the main thread has rebuilt
    /// ignore rules after it reports an ignore-file change.
    pub fn gitignore_generation(&self) -> Arc<AtomicU64> {
        Arc::clone(&self.gitignore_generation)
    }

    /// Whether any matcher (including a deliberate "no matcher") has been
    /// published for this context since it was created.
    ///
    /// Rebuilding, clearing, or confirming that the ignore files still match
    /// an earlier build all bump the generation. A `None` matcher at generation
    /// zero is "not known yet", not "this project ignores nothing".
    pub fn gitignore_published(&self) -> bool {
        self.gitignore_generation.load(Ordering::SeqCst) > 0
    }

    pub(super) fn bump_gitignore_generation(&self) {
        self.gitignore_generation.fetch_add(1, Ordering::SeqCst);
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        self.watcher_counters()
            .wake_for_matcher(&self.gitignore_generation);
    }
}
