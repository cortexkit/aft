use super::*;

impl LspManager {
    pub(crate) fn pending_change_slot(
        &self,
    ) -> super::super::pending_changes::PendingLspChangeSlot {
        self.pending_change_slot.clone()
    }

    pub(crate) fn deliver_pending_watched_backlog(&mut self) {
        if self.delivering_watched_changes {
            return;
        }
        // Reloading rust-analyzer drains events before its request, so it can
        // enter this path again. The outer delivery owns the watched batch.
        self.delivering_watched_changes = true;
        self.pending_change_slot.clone().deliver_watched(self);
        self.retire_settled_watched_servers();
        self.delivering_watched_changes = false;
    }

    pub(in crate::lsp) fn recover_watched_overflows(&mut self) {
        if self.is_quiesced() {
            return;
        }
        let overflows = self.event_tx.take_watched_overflows(|key, instance| {
            self.clients
                .get(key)
                .is_some_and(|client| client.instance == instance)
                && !self.starting.contains_key(key)
        });
        // Withdraw every producer's old reports before any reload or retained
        // notification can run. A request already in flight may still finish.
        for (key, _, count) in &overflows {
            slog_warn!("watched-file queue overflow for {key:?}: dropped {count} events");
            self.invalidate_server_diagnostics(key);
            let client = self.clients.get_mut(key).unwrap();
            client.watched_epoch = client.watched_epoch.wrapping_add(1);
            if key.kind != ServerKind::Rust {
                client.mark_watched_stale();
            }
        }
        for (key, instance, _) in overflows {
            if key.kind == ServerKind::Rust {
                self.reload_after_watched_overflow(key, instance);
            }
        }
        self.retire_settled_watched_servers();
    }

    pub(super) fn stamp_watched_epoch(
        &self,
        mut snapshot: AcceptedDiagnosticSnapshot,
    ) -> AcceptedDiagnosticSnapshot {
        snapshot.watched_epoch = self.watched_epoch(&snapshot.server_key);
        snapshot
    }

    pub(super) fn watched_epoch(&self, key: &ServerKey) -> Option<(ServerInstanceId, u64)> {
        self.clients
            .get(key)
            .map(|client| (client.instance, client.watched_epoch))
    }

    pub(crate) fn retain_current_watched_observations(
        &self,
        snapshots: &mut Vec<AcceptedDiagnosticSnapshot>,
    ) {
        snapshots.retain(|snapshot| {
            snapshot.watched_epoch.is_none_or(|(instance, epoch)| {
                self.clients
                    .get(&snapshot.server_key)
                    .is_some_and(|client| {
                        client.instance == instance
                            && client.watched_epoch == epoch
                            && !client.watched_stale()
                    })
            })
        });
    }

    fn reload_after_watched_overflow(&mut self, key: ServerKey, instance: ServerInstanceId) {
        let (pending, previous) = match self.begin_rust_workspace_reload(&key) {
            Ok(Some(request)) => request,
            Ok(None) => return,
            Err(error) => {
                slog_warn!(
                    "rust-analyzer in {} did not accept a workspace reload: {error}",
                    key.root.display()
                );
                return;
            }
        };
        let owner = self.startup_owner.clone();
        std::thread::spawn(move || {
            let result = pending.wait(RUST_RELOAD_REQUEST_TIMEOUT);
            if let Some(manager) = owner.upgrade() {
                let mut manager = manager.lock();
                if manager
                    .clients
                    .get(&key)
                    .is_some_and(|client| client.instance == instance)
                {
                    manager.finish_rust_workspace_reload(&key, &result, previous);
                }
            }
        });
    }

    fn retire_settled_watched_servers(&mut self) {
        let keys = self
            .clients
            .iter()
            .filter_map(|(key, client)| {
                (client.watched_stale() && !client.has_pending_requests()).then(|| key.clone())
            })
            .collect::<Vec<_>>();
        let clients = keys
            .into_iter()
            .map(|key| {
                let client = self.detach_idle_client(&key);
                (key, client)
            })
            .collect();
        Self::spawn_idle_lsp_reap(clients);
    }

    pub(super) fn watched_stale_event(&self, receipt: &ReceivedLspEvent) -> bool {
        let (LspEvent::Notification {
            server_kind, root, ..
        }
        | LspEvent::ServerRequest {
            server_kind, root, ..
        }
        | LspEvent::DocumentDiagnosticsPulled {
            server_kind, root, ..
        }) = &receipt.event
        else {
            return false;
        };
        let key = ServerKey {
            kind: server_kind.clone(),
            root: root.clone(),
        };
        self.clients.get(&key).is_some_and(LspClient::watched_stale)
    }
}
