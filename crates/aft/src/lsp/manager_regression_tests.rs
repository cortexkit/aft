use super::*;
use crate::lsp::client::ServerExitReason;
use serde_json::json;

fn key(root: &Path) -> ServerKey {
    ServerKey {
        kind: ServerKind::Rust,
        root: root.to_path_buf(),
    }
}

fn workspace_reply(
    manager: &LspManager,
    key: &ServerKey,
    reply: serde_json::Value,
) -> WorkspacePullReply {
    WorkspacePullReply {
        reply: Ok(reply),
        previous_result_ids: manager
            .diagnostics
            .result_ids_for_server(key)
            .into_iter()
            .collect(),
    }
}

fn seed_instance_state(manager: &mut LspManager, key: &ServerKey, file: &Path) {
    manager
        .documents
        .entry(key.clone())
        .or_default()
        .open(file.to_path_buf());
    manager
        .server_binaries
        .insert(key.clone(), "stand-in".into());
    manager
        .diagnostics
        .publish(key.clone(), file.to_path_buf(), Vec::new());
    manager
        .latest_push_for_pull_servers
        .insert((key.clone(), file.to_path_buf()), Vec::new());
    manager
        .latest_pull_for_rust
        .insert((key.clone(), file.to_path_buf()), Vec::new());
    manager.inspect_closed_documents.insert(
        (key.clone(), file.to_path_buf()),
        InspectClosedDocument {
            sequence: 1,
            answered: false,
        },
    );
    manager.watched_file_skip_logged.insert(key.clone());
}

#[test]
fn taking_clients_forgets_every_instance_cache() {
    let tmp = tempfile::tempdir().unwrap();
    let file = tmp.path().join("lib.rs");
    let mut manager = LspManager::new();
    seed_instance_state(&mut manager, &key(tmp.path()), &file);
    assert!(manager.take_all_clients().is_empty());
    assert!(manager.latest_push_for_pull_servers.is_empty());
    assert!(manager.latest_pull_for_rust.is_empty());
    assert!(manager.inspect_closed_documents.is_empty());
    assert!(manager.watched_file_skip_logged.is_empty());
    assert!(manager.documents.is_empty());
    assert!(manager.server_binaries.is_empty());
    assert!(manager.diagnostics.is_empty());
}

#[test]
fn exiting_server_forgets_only_its_instance_state() {
    let tmp = tempfile::tempdir().unwrap();
    let exited = key(&tmp.path().join("old"));
    let live = key(&tmp.path().join("live"));
    let file = tmp.path().join("lib.rs");
    let mut manager = LspManager::new();
    for key in [&exited, &live] {
        seed_instance_state(&mut manager, key, &file);
    }
    let _ = manager.handle_event(
        &LspEvent::ServerExited {
            server_kind: exited.kind.clone(),
            root: exited.root.clone(),
            pid: 4242,
            reason: ServerExitReason::Eof,
        }
        .into(),
    );
    assert!(!manager.watched_file_skip_logged.contains(&exited));
    assert!(manager.watched_file_skip_logged.contains(&live));
    for cache in [
        &manager.latest_pull_for_rust,
        &manager.latest_push_for_pull_servers,
    ] {
        assert!(!cache.contains_key(&(exited.clone(), file.clone())));
        assert!(cache.contains_key(&(live.clone(), file.clone())));
    }
    assert!(!manager
        .inspect_closed_documents
        .contains_key(&(exited.clone(), file.clone())));
    assert!(manager
        .inspect_closed_documents
        .contains_key(&(live.clone(), file.clone())));
    assert!(!manager.documents.contains_key(&exited));
    assert!(manager.documents.contains_key(&live));
    assert!(!manager
        .diagnostics
        .has_report_for_server_file(&exited, &file));
    assert!(manager.diagnostics.has_report_for_server_file(&live, &file));
}

#[test]
fn document_pull_ingests_related_full_and_unchanged_reports() {
    let tmp = tempfile::tempdir().unwrap();
    let key = key(tmp.path());
    let requested = tmp.path().join("main.rs");
    let related = tmp.path().join("lib.rs");
    let related_uri = uri_for_path(&related).unwrap().to_string();
    let mut manager = LspManager::new();
    let full = json!({"kind": "full", "items": [], "relatedDocuments": {
        related_uri.clone(): {"kind": "full", "resultId": "related-1", "items": [{
            "range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 1}},
            "message": "dependent error", "severity": 1
        }]}
    }});
    manager.ingest_document_report(
        &key,
        &requested,
        Some(1),
        None,
        serde_json::from_value(full).unwrap(),
    );
    let entries = manager.diagnostics.entries_for_file(&related);
    assert_eq!(
        entries.len(),
        1,
        "dependent-file diagnostics were discarded"
    );
    assert_eq!(entries[0].1.diagnostics[0].message, "dependent error");
    manager.diagnostics.mark_stale_for_file(&related);
    let unchanged = json!({"kind": "unchanged", "resultId": "main-1", "relatedDocuments": {
        related_uri: {"kind": "unchanged", "resultId": "related-1"}
    }});
    manager.ingest_document_report(
        &key,
        &requested,
        Some(1),
        None,
        serde_json::from_value(unchanged).unwrap(),
    );
    assert!(!manager
        .diagnostics
        .has_fresh_report_for_server_file(&key, &related));
    assert_eq!(
        manager.diagnostics.entries_for_file(&related)[0]
            .1
            .diagnostics[0]
            .message,
        "dependent error"
    );
}

#[test]
fn workspace_pull_preserves_versions_and_refreshes_unchanged_reports() {
    let tmp = tempfile::tempdir().unwrap();
    let key = key(tmp.path());
    let file = tmp.path().join("lib.rs");
    let uri = uri_for_path(&file).unwrap();
    let mut manager = LspManager::new();
    std::fs::write(&file, "pub fn f() {}\n").unwrap();
    manager.sync_document_version_for_test(&key, &file, 7);
    manager
        .finish_workspace_pull(
            &key,
            workspace_reply(
                &manager,
                &key,
                json!({"items": [{
                    "uri": uri, "version": 7, "kind": "full", "resultId": "v1", "items": []
                }]}),
            ),
        )
        .unwrap();
    assert_eq!(
        manager.diagnostics.entries_for_file(&file)[0].1.version,
        Some(7)
    );
    let reply = workspace_reply(
        &manager,
        &key,
        json!({"items": [{
            "uri": uri, "version": 8, "kind": "unchanged", "resultId": "v1"
        }]}),
    );
    manager.diagnostics.mark_stale_for_file(&file);
    manager.sync_document_version_for_test(&key, &file, 8);
    let result = manager.finish_workspace_pull(&key, reply).unwrap();
    assert_eq!(result.files_reported, vec![file.clone()]);
    assert!(manager
        .diagnostics
        .has_fresh_report_for_server_file(&key, &file));
    assert_eq!(
        manager.diagnostics.entries_for_file(&file)[0].1.version,
        Some(8)
    );
    manager.sync_document_version_for_test(&key, &file, 9);
    manager
        .finish_workspace_pull(
            &key,
            workspace_reply(
                &manager,
                &key,
                json!({"items": [{
                    "uri": uri_for_path(&file).unwrap(), "version": 9, "kind": "full", "items": []
                }]}),
            ),
        )
        .unwrap();
    assert!(
        !manager.diagnostics.entries_for_file(&file)[0]
            .1
            .from_document_pull
    );
}

#[test]
fn overflow_drift_scan_hashes_each_path_once_across_servers() {
    let tmp = tempfile::tempdir().unwrap();
    let file = tmp.path().join("main.rs");
    std::fs::write(&file, "fn main() {}\n").unwrap();
    let mut manager = LspManager::new();
    for kind in [ServerKind::Rust, ServerKind::TypeScript, ServerKind::Biome] {
        manager
            .documents
            .entry(ServerKey {
                kind,
                root: tmp.path().to_path_buf(),
            })
            .or_default()
            .open(file.clone());
    }
    let before = crate::lsp::document::CONTENT_READS.with(|reads| reads.get());
    assert_eq!(manager.resync_drifted_open_documents(&Config::default()), 0);
    let after = crate::lsp::document::CONTENT_READS.with(|reads| reads.get());
    assert_eq!(
        after - before,
        1,
        "each server independently hashed the same path"
    );
}

#[cfg(unix)]
fn sleeping_rust_client(manager: &LspManager, root: &Path) -> LspClient {
    LspClient::spawn_with_reclaim_root(
        ServerKind::Rust,
        root.to_path_buf(),
        Path::new("sh"),
        &["-c".into(), "exec sleep 60".into()],
        &HashMap::new(),
        manager.event_sender_for_test(),
        manager.child_registry.clone(),
        None,
        false,
    )
    .unwrap()
}

#[cfg(unix)]
#[test]
fn workspace_pull_from_warming_server_remains_provisional() {
    let tmp = tempfile::tempdir().unwrap();
    let key = key(tmp.path());
    let file = tmp.path().join("lib.rs");
    let mut manager = LspManager::new();
    manager
        .clients
        .insert(key.clone(), sleeping_rust_client(&manager, tmp.path()));
    manager.finish_workspace_pull(&key, workspace_reply(&manager, &key, json!({"items": [{
        "uri": uri_for_path(&file).unwrap(), "version": 7, "kind": "full", "resultId": "v1", "items": []
    }]}))).unwrap();
    assert!(manager.diagnostics.entries_for_file(&file)[0].1.provisional);
    let reply = workspace_reply(
        &manager,
        &key,
        json!({"items": [{
            "uri": uri_for_path(&file).unwrap(), "version": 7, "kind": "unchanged", "resultId": "v1"
        }]}),
    );
    manager.diagnostics.mark_stale_for_file(&file);
    manager.finish_workspace_pull(&key, reply).unwrap();
    assert!(manager.diagnostics.entries_for_file(&file)[0].1.provisional);
    manager
        .clients
        .get_mut(&key)
        .unwrap()
        .set_rust_analyzer_quiescent(true);
    manager.finish_workspace_pull(&key, workspace_reply(&manager, &key, json!({"items": [{
        "uri": uri_for_path(&file).unwrap(), "version": 7, "kind": "unchanged", "resultId": "v1"
    }]}))).unwrap();
    assert!(!manager.diagnostics.entries_for_file(&file)[0].1.provisional);
}

#[cfg(unix)]
#[test]
fn rejected_close_preserves_open_document_and_diagnostics() {
    let tmp = tempfile::tempdir().unwrap();
    let file = tmp.path().join("lib.rs");
    std::fs::write(&file, "fn main() {}\n").unwrap();
    let file = canonicalize_for_lsp(&file).unwrap();
    let key = key(tmp.path());
    let mut manager = LspManager::new();
    let client = sleeping_rust_client(&manager, tmp.path());
    client.mark_unresponsive_if_silent(client.received_message_count());
    manager.clients.insert(key.clone(), client);
    seed_instance_state(&mut manager, &key, &file);
    assert!(matches!(
        manager.close_file_for_servers(&file, std::slice::from_ref(&key)),
        Err(LspError::ServerNotReady(_))
    ));
    assert!(
        manager.documents[&key].is_open(&file),
        "unqueued didClose lost the open document"
    );
    assert!(manager.diagnostics.has_report_for_server_file(&key, &file));
}

#[test]
fn owned_pull_ingestion_reuses_diagnostic_and_result_id_allocations() {
    let tmp = tempfile::tempdir().unwrap();
    let key = key(tmp.path());
    let file = tmp.path().join("lib.rs");
    let report: lsp_types::FullDocumentDiagnosticReport = serde_json::from_value(json!({
        "resultId": "owned-result-id",
        "items": [{
            "range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 1}},
            "message": "an owned diagnostic message", "source": "owned-source", "code": "owned-code"
        }]
    }))
    .unwrap();
    let allocations = [
        report.result_id.as_ref().unwrap().as_ptr(),
        report.items[0].message.as_ptr(),
        report.items[0].source.as_ref().unwrap().as_ptr(),
    ];
    let mut manager = LspManager::new();
    manager.ingest_document_report_kind(
        &key,
        &file,
        Some(1),
        None,
        true,
        lsp_types::DocumentDiagnosticReportKind::Full(report),
    );
    let entries = manager.diagnostics.entries_for_file(&file);
    let stored = &entries[0].1;
    // Each nonempty string must retain its allocation across the ownership
    // transfer. Cloning the report would allocate replacement strings while
    // the originals are still live, so this also measures the copies avoided.
    let retained = [
        stored.result_id.as_ref().unwrap().as_ptr(),
        stored.diagnostics[0].message.as_ptr(),
        stored.diagnostics[0].source.as_ref().unwrap().as_ptr(),
    ];
    assert_eq!(
        allocations, retained,
        "owned pull strings were unnecessarily cloned"
    );
}
