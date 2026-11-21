use super::*;

impl AppContext {
    pub(crate) fn current_lsp_observations(
        &self,
        mut snapshots: Vec<crate::alert_state::AcceptedDiagnosticSnapshot>,
    ) -> Vec<crate::alert_state::AcceptedDiagnosticSnapshot> {
        self.lsp()
            .retain_current_watched_observations(&mut snapshots);
        snapshots
    }

    /// Access the LSP manager.
    pub fn lsp(&self) -> parking_lot::MutexGuard<'_, LspManager> {
        let mut lsp = self.lsp_manager.lock();
        lsp.deliver_pending_watched_backlog();
        lsp
    }

    /// Keep Cargo-driven rust-analyzer checks off for this root from the first
    /// worker call on (see [`LspManager::disable_rust_cargo_for_worker`]).
    ///
    /// Every worker tool call asks for this, so the flag is read without the
    /// manager lock first: once it is set the call returns at once instead of
    /// waiting behind a language-server start or a diagnostics request that
    /// holds the lock. After one call sets the flag, later calls never take
    /// the lock; only calls racing that first one may still contend for it.
    pub fn disable_rust_cargo_for_worker(&self) {
        if self
            .lsp_rust_worker_policy
            .load(std::sync::atomic::Ordering::Acquire)
        {
            return;
        }
        self.lsp_manager.lock().disable_rust_cargo_for_worker();
    }

    /// The LSP manager if no other thread holds it right now. For callers on
    /// the daemon's request loop, which must not wait behind a server start or a
    /// diagnostics request.
    pub fn try_lsp(&self) -> Option<parking_lot::MutexGuard<'_, LspManager>> {
        let mut lsp = self.lsp_manager.try_lock()?;
        lsp.deliver_pending_watched_backlog();
        Some(lsp)
    }
}
