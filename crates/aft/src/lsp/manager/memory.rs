use std::collections::HashMap;
use std::mem::size_of;
use std::ops::Deref;
use std::path::PathBuf;

use super::{
    AcceptedDiagnosticSnapshot, DocumentStore, LspClient, LspManager, PendingWatchedChange,
    PendingWatchedChanges, ReceivedLspEvent, ServerKey, StartReservation, StartupEventDelivery,
    StoredDiagnostic,
};
use crate::memory::{usize_to_u64, MemoryEstimate};

type FileKey = (ServerKey, PathBuf);
type MergeEntries = HashMap<FileKey, Vec<StoredDiagnostic>>;

/// The Rust push and pull caches have private copies of rows also published to
/// the diagnostic store. Count vector elements on writes, not during a census.
#[derive(Default)]
pub(super) struct DiagnosticMergeCache {
    entries: MergeEntries,
    diagnostics: usize,
}

impl Deref for DiagnosticMergeCache {
    type Target = MergeEntries;

    fn deref(&self) -> &Self::Target {
        &self.entries
    }
}

impl DiagnosticMergeCache {
    pub(super) fn insert(&mut self, key: FileKey, value: Vec<StoredDiagnostic>) {
        self.diagnostics += value.len();
        if let Some(old) = self.entries.insert(key, value) {
            self.diagnostics -= old.len();
        }
    }

    pub(super) fn remove(&mut self, key: &FileKey) -> Option<Vec<StoredDiagnostic>> {
        let old = self.entries.remove(key)?;
        self.diagnostics -= old.len();
        Some(old)
    }

    pub(super) fn retain(&mut self, mut keep: impl FnMut(&FileKey, &[StoredDiagnostic]) -> bool) {
        self.entries.retain(|key, value| {
            let keep = keep(key, value);
            if !keep {
                self.diagnostics -= value.len();
            }
            keep
        });
    }

    fn bytes(&self) -> u64 {
        items::<(FileKey, Vec<StoredDiagnostic>)>(self.entries.len()).saturating_add(items::<
            StoredDiagnostic,
        >(
            self.diagnostics,
        ))
    }
}

fn items<T>(len: usize) -> u64 {
    usize_to_u64(len.saturating_mul(size_of::<T>()))
}

impl LspManager {
    /// Approximate the manager's retained state without walking documents,
    /// diagnostics, or events. Child server processes are reported separately.
    pub fn estimated_memory(&self) -> MemoryEstimate {
        let mut document_bytes = items::<(ServerKey, DocumentStore)>(self.documents.len());
        let mut document_count = 0u64;
        for store in self.documents.values() {
            let estimate = store.estimated_memory();
            document_bytes = document_bytes.saturating_add(estimate.estimated_bytes.unwrap_or(0));
            document_count = document_count.saturating_add(estimate.counts["documents"]);
        }
        // One store indexes reports from every server by (server, file).
        let diagnostics = self.diagnostics.estimated_memory();
        let diagnostic_bytes = diagnostics.estimated_bytes.unwrap_or(0);

        let channel_items = self.event_rx.len();
        let channel_bytes = items::<ReceivedLspEvent>(channel_items);
        let deferred_events: usize = self
            .starting
            .values()
            .map(|start| start.deferred_events.len())
            .sum();
        let watched_changes: usize = self
            .pending_watched_changes
            .values()
            .map(|queue| queue.changes.len())
            .sum();
        let pending_event_bytes = items::<ReceivedLspEvent>(deferred_events)
            .saturating_add(items::<StartupEventDelivery>(
                self.ready_startup_events.len(),
            ))
            .saturating_add(items::<AcceptedDiagnosticSnapshot>(
                self.pending_diagnostic_observations.len(),
            ))
            .saturating_add(items::<(ServerKey, PendingWatchedChanges)>(
                self.pending_watched_changes.len(),
            ))
            .saturating_add(items::<(PathBuf, PendingWatchedChange)>(watched_changes));

        let push_pull_bytes = self
            .latest_push_for_pull_servers
            .bytes()
            .saturating_add(self.latest_pull_for_rust.bytes())
            .saturating_add(items::<(FileKey, Option<[u8; 32]>)>(
                self.unproven_rust_compiler_reports.len(),
            ));
        let manager_bytes = items::<Self>(1)
            .saturating_add(items::<(ServerKey, LspClient)>(self.clients.len()))
            .saturating_add(items::<(ServerKey, StartReservation)>(self.starting.len()));
        let bytes = [
            manager_bytes,
            document_bytes,
            diagnostic_bytes,
            pending_event_bytes,
            channel_bytes,
            push_pull_bytes,
        ]
        .into_iter()
        .fold(0u64, u64::saturating_add);

        MemoryEstimate::partial(bytes)
            .count("servers", self.clients.len())
            .count_u64("manager_bytes", manager_bytes)
            .count("document_stores", self.documents.len())
            .count_u64("documents", document_count)
            .count_u64("document_store_bytes", document_bytes)
            .count_u64(
                "diagnostic_entries",
                diagnostics.counts["diagnostic_entries"],
            )
            .count_u64("diagnostics", diagnostics.counts["diagnostics"])
            .count_u64("diagnostic_store_bytes", diagnostic_bytes)
            .count(
                "pending_diagnostic_observations",
                self.pending_diagnostic_observations.len(),
            )
            .count("startup_events", self.ready_startup_events.len())
            .count("deferred_startup_events", deferred_events)
            .count("pending_watched_changes", watched_changes)
            .count_u64("pending_event_bytes", pending_event_bytes)
            .count("event_channel_items", channel_items)
            .count_u64("event_channel_bytes", channel_bytes)
            .count("latest_push_files", self.latest_push_for_pull_servers.len())
            .count("latest_pull_files", self.latest_pull_for_rust.len())
            .count(
                "unproven_compiler_report_files",
                self.unproven_rust_compiler_reports.len(),
            )
            .count_u64("push_pull_bytes", push_pull_bytes)
            .gap("client_io_and_pending_requests")
            .gap("nested_event_and_push_pull_payloads")
            .gap("other_manager_maps_and_container_overhead")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lsp::client::{LspEvent, ServerInstanceId};
    use crate::lsp::diagnostics::DiagnosticSeverity;
    use crate::lsp::registry::ServerKind;

    #[test]
    fn memory_census_counts_lsp_stores_queues_and_push_pull_copies() {
        let mut manager = LspManager::new();
        let empty = manager.estimated_memory();
        assert_eq!(
            empty.counts["manager_bytes"],
            size_of::<LspManager>() as u64
        );
        let file = PathBuf::from("project/lib.rs");
        let diagnostic = StoredDiagnostic {
            file: file.clone(),
            line: 1,
            column: 1,
            end_line: 1,
            end_column: 2,
            severity: DiagnosticSeverity::Warning,
            message: "unused".into(),
            code: None,
            source: None,
        };
        let rust = ServerKey {
            kind: ServerKind::Rust,
            root: "project".into(),
        };
        let other = ServerKey {
            kind: ServerKind::Rust,
            root: "project/nested".into(),
        };
        for key in [&rust, &other] {
            manager
                .documents
                .entry(key.clone())
                .or_default()
                .open(file.clone());
            manager
                .diagnostics
                .publish(key.clone(), file.clone(), vec![diagnostic.clone()]);
        }
        let file_key = (rust.clone(), file.clone());
        manager
            .latest_push_for_pull_servers
            .insert(file_key.clone(), vec![diagnostic.clone()]);
        manager
            .latest_pull_for_rust
            .insert(file_key.clone(), vec![diagnostic.clone(); 2]);
        manager
            .unproven_rust_compiler_reports
            .insert(file_key.clone(), None);
        manager
            .pending_diagnostic_observations
            .push(AcceptedDiagnosticSnapshot::new(
                rust.clone(),
                1,
                vec![diagnostic],
            ));
        let event = ReceivedLspEvent::from(LspEvent::DocumentDiagnosticsPulled {
            server_kind: ServerKind::Rust,
            root: rust.root.clone(),
            file,
            epoch: 1,
        });
        manager.event_tx.send(event.clone()).unwrap();
        manager
            .ready_startup_events
            .push_back(StartupEventDelivery {
                event: event.clone(),
                published: None,
            });
        manager.starting.insert(
            rust,
            StartReservation {
                definition: None,
                source_file: None,
                instance: ServerInstanceId::next(),
                deferred_events: vec![event],
                clients_generation: 0,
                signal: Default::default(),
                notification_versions: Default::default(),
                pending_notifications: Default::default(),
                replay_pid: None,
            },
        );

        let estimate = manager.estimated_memory();
        let counts = &estimate.counts;
        assert_eq!(estimate.bytes_status, "partial");
        assert_eq!(counts["documents"], 2);
        assert_eq!(counts["document_stores"], 2);
        assert!(counts["document_store_bytes"] > 0);
        assert_eq!(counts["diagnostic_entries"], 2);
        assert_eq!(counts["diagnostics"], 2);
        assert!(counts["diagnostic_store_bytes"] > 0);
        assert_eq!(counts["pending_diagnostic_observations"], 1);
        assert_eq!(counts["startup_events"], 1);
        assert_eq!(counts["deferred_startup_events"], 1);
        assert_eq!(counts["pending_watched_changes"], 0);
        assert_eq!(
            counts["pending_event_bytes"],
            items::<AcceptedDiagnosticSnapshot>(1)
                + items::<StartupEventDelivery>(1)
                + items::<ReceivedLspEvent>(1)
        );
        assert_eq!(counts["event_channel_items"], 1);
        assert_eq!(
            counts["event_channel_bytes"],
            size_of::<ReceivedLspEvent>() as u64
        );
        assert_eq!(counts["latest_push_files"], 1);
        assert_eq!(counts["latest_pull_files"], 1);
        assert_eq!(counts["unproven_compiler_report_files"], 1);
        assert_eq!(
            counts["push_pull_bytes"],
            items::<(FileKey, Vec<StoredDiagnostic>)>(2)
                + items::<FileKey>(1)
                + items::<StoredDiagnostic>(3)
        );
        assert_eq!(
            estimate.estimated_bytes,
            Some(
                [
                    "manager_bytes",
                    "document_store_bytes",
                    "diagnostic_store_bytes",
                    "pending_event_bytes",
                    "event_channel_bytes",
                    "push_pull_bytes"
                ]
                .into_iter()
                .map(|field| counts[field])
                .sum()
            )
        );

        manager.event_rx.try_recv().unwrap();
        manager.latest_push_for_pull_servers.remove(&file_key);
        manager.latest_pull_for_rust.insert(file_key, Vec::new());
        assert_eq!(
            manager.latest_pull_for_rust.bytes(),
            items::<(FileKey, Vec<StoredDiagnostic>)>(1)
        );
        manager.latest_pull_for_rust.retain(|_, _| false);
        let drained = manager.estimated_memory();
        assert_eq!(drained.counts["event_channel_items"], 0);
        assert_eq!(drained.counts["event_channel_bytes"], 0);
        assert_eq!(drained.counts["push_pull_bytes"], items::<FileKey>(1));
    }
}
