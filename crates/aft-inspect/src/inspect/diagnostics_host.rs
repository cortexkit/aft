use std::sync::Arc;

use parking_lot::Mutex;

use crate::lsp::manager::LspManager;

/// The shared LSP state used by diagnostic collection.
#[doc(hidden)]
pub trait DiagnosticsHost {
    fn lsp_manager(&self) -> Arc<Mutex<LspManager>>;

    /// Deliver pending watcher changes before inspecting the live reports.
    fn forward_queued_watcher_changes(&self);
}
