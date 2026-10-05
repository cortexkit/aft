use std::path::PathBuf;
use std::time::{Duration, Instant};

use aft::alert_state::{
    normalize_diagnostic_message, AcceptedDiagnosticSnapshot, AcceptedObservation,
    AcceptedObservationBatch, AlertDeltaState, AlertPartitionKey, ProducerKey,
};
use aft::lsp::diagnostics::{DiagnosticSeverity, StoredDiagnostic};
use aft::lsp::registry::ServerKind;
use aft::lsp::roots::ServerKey;

fn diagnostic(file: &str, line: u32, message: &str) -> StoredDiagnostic {
    StoredDiagnostic {
        file: PathBuf::from(file),
        line,
        column: 1,
        end_line: line,
        end_column: 2,
        severity: DiagnosticSeverity::Error,
        message: message.to_owned(),
        code: Some("E100".to_owned()),
        source: Some("fixture".to_owned()),
    }
}

fn observation(
    session: &str,
    root: &str,
    producer: &str,
    version: i32,
    diagnostics: Vec<StoredDiagnostic>,
) -> AcceptedObservation {
    AcceptedObservation::new(
        session,
        root,
        ProducerKey::new(producer),
        version,
        diagnostics,
    )
}

#[test]
fn identity_normalizer_applies_only_the_ruled_table() {
    let cases = [
        ("  first\t line   \nignored second line", "first line"),
        ("e\u{301}", "é"),
        (
            "  src/../quoted.rs   E100   \"exact quote\"  ",
            "src/../quoted.rs E100 \"exact quote\"",
        ),
    ];

    for (input, expected) in cases {
        assert_eq!(normalize_diagnostic_message(input), expected);
    }
}

#[test]
fn accepted_snapshots_update_only_their_own_producer_partition() {
    let root = "/alert-fixture/project";
    let first_a = diagnostic("/alert-fixture/project/a.rs", 1, "first error");
    let first_b = diagnostic("/alert-fixture/project/b.rs", 2, "other producer");
    let mut state = AlertDeltaState::default();
    let start = Instant::now();

    let baseline = AcceptedObservationBatch::new(vec![
        observation("session", root, "server-a", 1, vec![first_a.clone()]),
        observation("session", root, "server-b", 1, vec![first_b.clone()]),
    ])
    .unwrap();
    let results = state.accept_batch_at(&baseline, start).unwrap();
    assert!(results.iter().all(|result| result.baseline_established_now));

    let b_key = AlertPartitionKey::new("session", root, ProducerKey::new("server-b"));
    let b_before = state.partition(&b_key).unwrap().clone();
    let later_a = diagnostic("/alert-fixture/project/a.rs", 4, "new error");
    let update_a = AcceptedObservationBatch::new(vec![observation(
        "session",
        root,
        "server-a",
        2,
        vec![first_a.clone(), later_a],
    )])
    .unwrap();
    let result = state
        .accept_batch_at(&update_a, start + Duration::from_secs(1))
        .unwrap()
        .pop()
        .unwrap();

    assert_eq!(result.entered.len(), 1);
    assert!(result.closed.is_empty());
    assert_eq!(state.partition(&b_key), Some(&b_before));

    let a_key = AlertPartitionKey::new("session", root, ProducerKey::new("server-a"));
    let a_state = state.partition(&a_key).unwrap();
    assert!(a_state.baseline_established);
    assert_eq!(a_state.live.len(), 2);
    assert_eq!(a_state.rendered.len(), 1);

    let clean_a = AcceptedObservationBatch::new(vec![observation(
        "session",
        root,
        "server-a",
        3,
        Vec::new(),
    )])
    .unwrap();
    let result = state
        .accept_batch_at(&clean_a, start + Duration::from_secs(2))
        .unwrap()
        .pop()
        .unwrap();
    assert!(result.accepted_empty_snapshot);
    assert_eq!(result.closed.len(), 2);
    assert!(state.partition(&a_key).unwrap().live.is_empty());
    assert_eq!(state.partition(&b_key), Some(&b_before));
}

#[test]
fn reentry_mints_a_new_episode_and_idle_reap_removes_whole_session() {
    let root = "/alert-fixture/project";
    let finding = diagnostic("/alert-fixture/project/a.rs", 1, "returns later");
    let mut state = AlertDeltaState::default();
    let started = Instant::now();

    let baseline = AcceptedObservationBatch::new(vec![observation(
        "session",
        root,
        "server-a",
        1,
        vec![finding.clone()],
    )])
    .unwrap();
    let first_episode = state
        .accept_batch_at(&baseline, started)
        .unwrap()
        .pop()
        .unwrap()
        .baselined
        .pop()
        .unwrap()
        .episode_id;

    let empty = AcceptedObservationBatch::new(vec![observation(
        "session",
        root,
        "server-a",
        2,
        Vec::new(),
    )])
    .unwrap();
    let closed_episode = state
        .accept_batch_at(&empty, started + Duration::from_secs(1))
        .unwrap()
        .pop()
        .unwrap()
        .closed
        .pop()
        .unwrap()
        .episode_id;
    assert_eq!(first_episode, closed_episode);

    let reentered = AcceptedObservationBatch::new(vec![observation(
        "session",
        root,
        "server-a",
        3,
        vec![finding],
    )])
    .unwrap();
    let second_episode = state
        .accept_batch_at(&reentered, started + Duration::from_secs(2))
        .unwrap()
        .pop()
        .unwrap()
        .entered
        .pop()
        .unwrap()
        .episode_id;
    assert_ne!(first_episode, second_episode);

    let reaped =
        state.reap_idle_sessions_at(started + Duration::from_secs(4), Duration::from_secs(2));
    assert_eq!(reaped, vec!["session"]);
    assert!(state.partitions_for_session("session").next().is_none());
}

#[test]
fn accepted_empty_lsp_snapshot_is_distinct_from_absent_producer() {
    let server = ServerKey {
        kind: ServerKind::Rust,
        root: PathBuf::from("/alert-fixture/project"),
    };
    let snapshot = AcceptedDiagnosticSnapshot::new(server, 7, Vec::new());
    assert!(snapshot.is_empty());

    let batch = AcceptedObservationBatch::from_diagnostic_snapshots(
        "session",
        "/alert-fixture/project",
        vec![snapshot],
    )
    .unwrap();
    let mut state = AlertDeltaState::default();
    let result = state.accept_batch_at(&batch, Instant::now()).unwrap();
    assert!(result[0].accepted_empty_snapshot);
    assert!(result[0].baseline_established_now);
}

#[test]
fn post_edit_acceptance_keeps_clean_snapshot_and_omits_unversioned_report() {
    use aft::lsp::diagnostics::DiagnosticEntry;
    use aft::lsp::manager::{LspManager, PreEditSnapshot};

    let server = ServerKey {
        kind: ServerKind::Rust,
        root: PathBuf::from("/alert-fixture/project"),
    };
    let fresh_clean = DiagnosticEntry {
        diagnostics: Vec::new(),
        epoch: 2,
        result_id: None,
        version: Some(1),
        stale: false,
        provisional: false,
    };
    let accepted = LspManager::post_edit_outcome_for_entry_for_test(
        server.clone(),
        &fresh_clean,
        1,
        PreEditSnapshot {
            epoch: 1,
            document_version_at_capture: Some(0),
        },
    );
    assert_eq!(accepted.accepted_snapshots.len(), 1);
    assert!(accepted.accepted_snapshots[0].is_empty());
    assert!(accepted.pending_servers.is_empty());

    let unversioned = DiagnosticEntry {
        version: None,
        ..fresh_clean
    };
    let rejected = LspManager::post_edit_outcome_for_entry_for_test(
        server,
        &unversioned,
        1,
        PreEditSnapshot {
            epoch: 1,
            document_version_at_capture: Some(0),
        },
    );
    assert!(rejected.accepted_snapshots.is_empty());
    assert_eq!(rejected.pending_servers.len(), 1);
}

#[test]
fn post_edit_wait_does_not_start_a_server_on_a_cold_root() {
    use aft::config::Config;
    use aft::context::{App, AppContext};

    let directory = tempfile::tempdir().unwrap();
    let file = directory.path().join("cold.ts");
    std::fs::write(directory.path().join("package.json"), "{}").unwrap();
    std::fs::write(&file, "export const value = 1;\n").unwrap();
    let context = AppContext::from_app(
        App::default_shared(),
        crate::context_storage::isolate(Config::default()),
    );

    let outcome = context.lsp_notify_and_collect_diagnostics(
        &file,
        "export const value = 2;\n",
        Duration::ZERO,
    );

    assert!(outcome.accepted_snapshots.is_empty());
    assert!(outcome.diagnostics.is_empty());
    assert_eq!(context.lsp_server_count(), 0);
}
