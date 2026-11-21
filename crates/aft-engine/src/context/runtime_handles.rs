use super::*;

#[derive(Default)]
pub(super) struct WatcherGap {
    // Odd epochs are unverified. A delayed rescan clears only its own epoch.
    epoch: AtomicU64,
    #[cfg(test)]
    cleared: tokio::sync::Notify,
}

impl App {
    pub(crate) fn set_subc_connection_file(&self, path: PathBuf) {
        *self.subc_connection_file.write() = Some(path);
    }

    pub(crate) fn subc_connection_file(&self) -> Option<PathBuf> {
        self.subc_connection_file.read().clone()
    }
}

pub(super) fn configure_background_scope(registry: &BgTaskRegistry, app: &App, config: &Config) {
    registry.configure_task_scope_runtime(
        app.subc_connection_file().is_some(),
        Duration::from_secs(config.bash.linux_scope_max_runtime_secs),
    );
}

impl AppContext {
    pub(crate) fn watcher_gap_pending(&self) -> bool {
        self.watcher_gap_epoch() & 1 != 0
    }

    pub(crate) fn watcher_gap_epoch(&self) -> u64 {
        self.watcher_gap.epoch.load(Ordering::Acquire)
    }

    pub(crate) fn mark_watcher_gap_pending(&self) {
        let mut epoch = self.watcher_gap_epoch();
        loop {
            match self.watcher_gap.epoch.compare_exchange_weak(
                epoch,
                epoch.wrapping_add(2) | 1,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return,
                Err(current) => epoch = current,
            }
        }
    }

    #[cfg(test)]
    pub(crate) async fn await_watcher_gap_rescan_for_test(&self) {
        while self.watcher_gap_pending() {
            let cleared = self.watcher_gap.cleared.notified();
            tokio::pin!(cleared);
            cleared.as_mut().enable();
            if !self.watcher_gap_pending() {
                return;
            }
            cleared.await;
        }
    }

    /// The watcher is live before taking this snapshot of bound checkouts.
    /// New checkouts bind against disk; current ones must catch up before
    /// their retained text and call-graph views are eligible again.
    pub(crate) fn rescan_after_watcher_gap(&self, expected_generation: u64, epoch: u64) {
        if epoch & 1 == 0
            || self.watcher_gap_epoch() != epoch
            || self.subc_unbound_quiesced()
            || self.configure_generation() != expected_generation
        {
            return;
        }
        let gap = Arc::clone(&self.watcher_gap);
        let generation = Arc::clone(&self.configure_generation);
        let clear = move || {
            if generation.load(Ordering::Acquire) == expected_generation
                && gap
                    .epoch
                    .compare_exchange(
                        epoch,
                        epoch.wrapping_add(1),
                        Ordering::AcqRel,
                        Ordering::Acquire,
                    )
                    .is_ok()
            {
                #[cfg(test)]
                gap.cleared.notify_waiters();
            }
        };
        let mut checkouts = self.data_children.bound();
        checkouts.extend(self.data_checkout.lock().as_ref().cloned());
        if checkouts.is_empty() {
            clear();
            return;
        }
        let remaining = Arc::new(AtomicUsize::new(checkouts.len()));
        let clear = Arc::new(clear);
        for checkout in checkouts {
            let remaining = remaining.clone();
            let clear = clear.clone();
            // A failed or refused rescan leaves the gap open, rather than
            // treating incomplete coverage as a fresh view.
            checkout.rescan_watcher_gap(move |scanned| {
                if scanned && remaining.fetch_sub(1, Ordering::AcqRel) == 1 {
                    clear();
                }
            });
        }
    }

    /// Gate every watcher-maintained artifact after a watcher stops. Files
    /// may change before it starts again, so a later bind must reconcile
    /// from disk instead of serving snapshots that missed those edits.
    pub(crate) fn invalidate_artifacts_after_watcher_gap(&self) {
        self.mark_watcher_gap_pending();
        self.next_semantic_persist_epoch();
        self.semantic_index
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        // Enabled semantic indexes must remain reloadable through strict disk
        // verification. A Disabled status would strand them until configure.
        *self
            .semantic_index_status
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = if self.config().indexes.semantic
        {
            SemanticIndexStatus::ready()
        } else {
            SemanticIndexStatus::Disabled
        };
        if let Some(root) = self
            .canonical_cache_root_opt()
            .or_else(|| self.config().project_root.clone())
        {
            crate::cache_freshness::invalidate_verify_memo_strict(&root);
        }
        self.inspect_manager.evict_idle_caches();
        self.reset_symbol_cache();
        self.clear_tsconfig_membership_cache();
    }

    pub fn app(&self) -> Arc<App> {
        Arc::clone(&self.app)
    }

    pub fn is_daemon_mode(&self) -> bool {
        self.app().subc_connection_file().is_some()
    }

    /// Clone the LSP child registry handle. Used by main.rs to give the
    /// signal handler thread a way to SIGKILL LSP children on shutdown.
    pub fn lsp_child_registry(&self) -> crate::lsp::child_registry::LspChildRegistry {
        self.app.lsp_child_registry()
    }

    pub fn stdout_writer(&self) -> SharedStdoutWriter {
        self.app.stdout_writer()
    }
}
