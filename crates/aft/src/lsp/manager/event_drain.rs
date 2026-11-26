use super::*;
use crate::lsp::analysis_progress::{Transition, ANALYSIS_TOKEN_LIMIT};
use crate::lsp::event_ingress::IngressOverflow;

fn earliest_settle(current: Option<Instant>, candidate: Option<Instant>) -> Option<Instant> {
    current.into_iter().chain(candidate).min()
}

impl LspManager {
    /// Drain all pending LSP events. Call from the main loop.
    pub fn drain_events(&mut self) -> DrainedLspEvents {
        self.drain_events_bounded(usize::MAX)
    }

    /// Whether the maintenance scheduler should run an LSP drain.
    pub fn has_pending_events(&self) -> bool {
        !self.event_rx.is_empty()
            || self
                .rust_publish_settle_at
                .is_some_and(|deadline| deadline <= self.retry_clock.now())
            || self.event_tx.pending(|key, id| self.ingress_ready(key, id))
            || self
                .clients
                .values()
                .any(|client| client.watched_stale() && !client.has_pending_requests())
            || !self.pending_diagnostic_observations.is_empty()
            || !self.ready_startup_events.is_empty()
            || (!self.is_quiesced()
                && self.pending_watched_changes.keys().any(|key| {
                    !self.starting.contains_key(key)
                        && self
                            .clients
                            .get(key)
                            .is_some_and(LspClient::has_watched_file_registration)
                }))
    }

    pub fn drain_events_bounded(&mut self, max_events: usize) -> DrainedLspEvents {
        self.drain_events_inner(max_events, false)
    }

    pub(super) fn drain_events_inner(
        &mut self,
        max_events: usize,
        preserve_pull_observations: bool,
    ) -> DrainedLspEvents {
        let max_events = max_events.min(EVENT_CAP * 2);
        let _ = self.ingress_rx.try_recv();
        let now = Instant::now();
        for cache in self.watched_lifecycle.values_mut() {
            cache.expire(now);
        }
        let mut overflowed = self.apply_ingress_overflows();
        self.publish_completed_starts();
        let mut events = Vec::new();
        let mut instances = Vec::new();
        let mut processed = 0;
        let mut deferred_pull_events = Vec::new();
        let mut accepted_snapshots = if preserve_pull_observations {
            Vec::new()
        } else {
            std::mem::take(&mut self.pending_diagnostic_observations)
        };
        let mut diagnostics_changed = !accepted_snapshots.is_empty();
        while processed < max_events {
            if !preserve_pull_observations {
                if let Some(delivery) = self.ready_startup_events.pop_front() {
                    processed += 1;
                    if self.event_tx.predates_overflow(&delivery.event)
                        || self.watched_stale_event(&delivery.event)
                    {
                        continue;
                    }
                    diagnostics_changed |= delivery.published.is_some();
                    if let Some(ref published) = delivery.published {
                        if self.startup_publish_is_current(published) {
                            if let Some(snapshot) =
                                self.accepted_snapshot_for_file(&published.key, &published.file)
                            {
                                accepted_snapshots.push(snapshot);
                            }
                        }
                    }
                    instances.push(delivery.event.instance);
                    events.push(delivery.event.event);
                    continue;
                }
            }
            let Some(event) = self
                .event_tx
                .take_exit()
                .or_else(|| self.event_rx.try_recv().ok())
            else {
                break;
            };
            processed += 1;
            if self.event_tx.overflow_pending_for(&event) {
                overflowed.extend(self.apply_ingress_overflows());
            }
            if preserve_pull_observations
                && matches!(event.event, LspEvent::DocumentDiagnosticsPulled { .. })
            {
                if self.accept_event_instance(&event.event, event.instance) {
                    deferred_pull_events.push(event);
                }
                continue;
            }
            if !self.admit_event(&event) {
                continue;
            }
            if self.defer_startup_event(&event) {
                continue;
            }
            // `handle_ready_event` returns the file of a stored publish; the
            // snapshot reuses it instead of parsing the payload again.
            if let Some(file) = self.handle_ready_event(&event) {
                diagnostics_changed = true;
                if let Some(snapshot) = self.accepted_live_publish_snapshot(&event.event, &file) {
                    accepted_snapshots.push(snapshot);
                }
            }
            instances.push(event.instance);
            events.push(event.event);
        }
        for event in deferred_pull_events {
            let _ = self.event_tx.send(event);
        }
        let has_more = processed >= max_events
            && (!self.event_rx.is_empty()
                || self.event_tx.pending(|key, id| self.ingress_ready(key, id))
                || (!preserve_pull_observations && !self.ready_startup_events.is_empty()));
        self.send_due_rust_saves();
        let mut promoted_reports = Vec::new();
        for (key, client) in &mut self.clients {
            if client.uses_analysis_progress() {
                // Keep previously certified reports as warm snapshots. A fresh
                // inspection still waits for this server to finish analysing
                // because a change can affect dependent files as well.
                let mut no_run = false;
                let promoted = self.diagnostics.promote_provisional_reports(
                    key,
                    |file, epoch, published_at| {
                        let certified =
                            client.analysis_progress_certifies(file, epoch, published_at);
                        no_run |= certified && !client.analysis_progress_covered_by_end(epoch);
                        certified
                    },
                );
                if !promoted.is_empty() {
                    diagnostics_changed = true;
                    if self.diagnostics.has_authoritative_report_for_server(key) {
                        client.note_certified_diagnostics(no_run);
                    }
                }
                promoted_reports.extend(promoted.into_iter().map(|file| (key.clone(), file)));
            }
        }
        // A provisional publication was deliberately absent from live alert
        // ingestion. Deliver its version-checked snapshot once it is certified.
        for (key, file) in promoted_reports {
            if let Some(snapshot) = self.accepted_snapshot_for_file(&key, &file) {
                accepted_snapshots.push(snapshot);
            }
        }
        let mut completed_checks = Vec::new();
        let now = self.retry_clock.now();
        // Only future deadlines can need another timer-driven drain. A due
        // check that still lacks authority needs a new server event instead.
        self.rust_publish_settle_at = None;
        for (key, client) in &mut self.clients {
            let Some(deadline) = client.rust_publish_settle_deadline() else {
                continue;
            };
            if deadline > now {
                self.rust_publish_settle_at =
                    earliest_settle(self.rust_publish_settle_at, Some(deadline));
                continue;
            }
            if client.rust_check_completed_current(now, FLYCHECK_PUBLISH_SETTLE)
                && !client.diagnostics_are_provisional()
            {
                if let Some(check) = client
                    .completed_rust_check
                    .as_mut()
                    .and_then(|cache| cache.complete())
                {
                    completed_checks.push((key.clone(), check));
                }
            }
        }
        for (key, check) in completed_checks {
            for (file, compiler) in &check.diagnostics {
                let Some(hash) = check.content_hash(file) else {
                    continue;
                };
                let document = self.documents.get(&key).and_then(|store| store.entry(file));
                if document.is_some_and(|document| document.content_hash != Some(hash)) {
                    continue;
                }
                // A bounded inspect can close and reopen a document before a
                // delayed compiler push arrives. Its old or absent version is
                // not document authority, but the completed check's verified
                // inputs prove which bytes its compiler diagnostics describe.
                self.latest_push_for_pull_servers
                    .insert((key.clone(), file.clone()), compiler.clone());
                // The completed check's verified inputs match these rows to
                // the file's content.
                self.unproven_rust_compiler_reports
                    .remove(&(key.clone(), file.clone()));
                let pulled = self.latest_pull_for_rust.get(&(key.clone(), file.clone()));
                let merged =
                    union_of_diagnostics(pulled.cloned().unwrap_or_default(), compiler.clone());
                let version = document.map(|document| document.version);
                self.diagnostics.publish_full_with_provisional(
                    key.clone(),
                    file.clone(),
                    merged,
                    None,
                    version,
                    false,
                );
                self.diagnostics.certify_content(&key, file, Some(hash));
                if pulled.is_some() {
                    self.diagnostics.mark_document_pull(&key, file);
                }
                diagnostics_changed = true;
                if let Some(snapshot) = self.accepted_snapshot_for_file(&key, file) {
                    accepted_snapshots.push(snapshot);
                }
            }
        }
        overflowed.extend(self.apply_ingress_overflows());
        if !overflowed.is_empty() {
            diagnostics_changed = true;
            accepted_snapshots.retain(|snapshot| !overflowed.contains(&snapshot.server_key));
        }
        DrainedLspEvents {
            events,
            instances,
            processed,
            diagnostics_changed,
            accepted_snapshots,
            has_more,
        }
    }

    fn ingress_ready(&self, key: &ServerKey, id: ServerInstanceId) -> bool {
        self.clients.get(key).is_some_and(|client| {
            client.instance == id
                && (!client.uses_analysis_progress() || !self.starting.contains_key(key))
        })
    }

    /// The next moment a rust-analyzer check can change without a new event,
    /// such as a deferred save falling due or the publish-settle window closing.
    pub(crate) fn rust_check_next_timed_change(&self, server_key: &ServerKey) -> Option<Instant> {
        self.clients.get(server_key).and_then(|client| {
            client.rust_check_next_timed_change(Instant::now(), FLYCHECK_PUBLISH_SETTLE)
        })
    }

    pub(super) fn handle_analysis_progress(&mut self, receipt: &ReceivedLspEvent) {
        let Some((token, transition)) = Transition::from_event(&receipt.event) else {
            return;
        };
        let (LspEvent::Notification {
            server_kind, root, ..
        }
        | LspEvent::ServerRequest {
            server_kind, root, ..
        }) = &receipt.event
        else {
            return;
        };
        let key = ServerKey {
            kind: server_kind.clone(),
            root: root.clone(),
        };
        if let Some(client) = self
            .clients
            .get_mut(&key)
            .filter(|client| client.uses_analysis_progress())
        {
            client.record_analysis_progress(
                token,
                transition,
                receipt.receive_order,
                self.diagnostics.current_epoch(),
            );
        }
    }

    pub(super) fn handle_rust_progress(
        &mut self,
        root: PathBuf,
        params: &serde_json::Value,
        order: Option<u64>,
    ) {
        let token = match params.get("token") {
            Some(serde_json::Value::String(token)) => token.clone(),
            Some(serde_json::Value::Number(token)) => token.to_string(),
            _ => return,
        };
        let Some(value) = params.get("value") else {
            return;
        };
        let kind = value
            .get("kind")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default();
        let title = value.get("title").and_then(serde_json::Value::as_str);
        let key = ServerKey {
            kind: ServerKind::Rust,
            root,
        };
        if let Some(client) = self.clients.get_mut(&key) {
            if matches!(kind, "begin" | "end") {
                slog_info!(
                    "lsp_protocol server=rust root={} method=$/progress event={} token={} title={}",
                    key.root.display(),
                    kind,
                    token,
                    title.unwrap_or("-")
                );
            }
            if !client.record_rust_progress(&token, kind, title, order) {
                return;
            }
            if value
                .get("message")
                .and_then(serde_json::Value::as_str)
                .is_some_and(|message| {
                    let message = message.to_lowercase();
                    message.contains("cancel") || message.contains("fail")
                })
            {
                if let Some(cache) = client.completed_rust_check.as_mut() {
                    cache.abort();
                }
            }
            // A synchronous waiter can handle END outside the main drain.
            self.rust_publish_settle_at = earliest_settle(
                self.rust_publish_settle_at,
                client.rust_publish_settle_deadline(),
            );
        }
    }

    pub(super) fn apply_ingress_overflows(&mut self) -> Vec<ServerKey> {
        let overflows = self
            .event_tx
            .take_overflows(|key, id| self.ingress_ready(key, id));
        let mut affected = Vec::new();
        let mut retired = Vec::new();
        for (key, overflow) in overflows {
            match overflow {
                IngressOverflow::Dropped { through, progress } => {
                    let client = self.clients.get_mut(&key).unwrap();
                    if let Some(progress) = progress {
                        client
                            .recover_analysis_progress(progress, self.diagnostics.current_epoch());
                    } else if key.kind == ServerKind::Rust {
                        client.lose_flycheck_progress(through);
                    }
                    slog_warn!(
                        "discarded language server events for {key:?}: event queue overflow"
                    );
                    self.invalidate_server_diagnostics(&key);
                }
                IngressOverflow::ProgressLimit => {
                    slog_warn!("retiring language server {key:?}: exceeded the limit of {ANALYSIS_TOKEN_LIMIT} unfinished analysis progress tokens");
                    retired.push((key.clone(), self.detach_idle_client(&key)));
                }
            }
            affected.push(key);
        }
        if !affected.is_empty() {
            self.wake_post_edit_waiters();
        }
        Self::spawn_idle_lsp_reap(retired);
        affected
    }

    pub(super) fn invalidate_server_diagnostics(&mut self, key: &ServerKey) {
        self.diagnostics.clear_for_server(key);
        self.pending_diagnostic_observations
            .retain(|snapshot| &snapshot.server_key != key);
        self.latest_push_for_pull_servers
            .retain(|(server, _), _| server != key);
        self.latest_pull_for_rust
            .retain(|(server, _), _| server != key);
        self.unproven_rust_compiler_reports
            .retain(|(server, _), _| server != key);
        if let Some(client) = self.clients.get_mut(key) {
            client.completed_rust_check = None;
        }
    }
}
