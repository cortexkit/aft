use super::*;

impl AppContext {
    #[doc(hidden)]
    pub fn lsp_diagnostics_activity(&self) -> impl Drop {
        crate::lsp::manager::DiagnosticsActivityGuard::begin(&self.lsp_diagnostics_activity)
    }

    /// Start one inspect producer, holding the LSP manager lock only to
    /// reserve the server and to publish it; the spawn and `initialize`
    /// handshake run unlocked (see
    /// [`crate::lsp::manager::start_applicable_server_unlocked`]).
    pub fn lsp_start_applicable_server_until(
        &self,
        snapshot: &crate::lsp::manager::ApplicableServerSnapshot,
        server: &crate::lsp::roots::ServerKey,
        config: &Config,
        deadline: Instant,
    ) -> crate::lsp::manager::ApplicableServerStartOutcomes {
        crate::lsp::manager::start_applicable_server_unlocked(
            &self.lsp_manager,
            snapshot,
            server,
            config,
            deadline,
        )
    }

    #[doc(hidden)]
    pub fn current_lsp_observations(
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
