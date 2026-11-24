use std::sync::Arc;

use aft_inspect::inspect::diagnostics_host::DiagnosticsHost;
use aft_inspect::lsp::manager::LspManager;
use parking_lot::Mutex;

use crate::context::AppContext;

impl DiagnosticsHost for AppContext {
    fn lsp_manager(&self) -> Arc<Mutex<LspManager>> {
        Arc::clone(&self.lsp_manager)
    }

    fn forward_queued_watcher_changes(&self) {
        self.lsp_forward_queued_watcher_changes();
    }
}
