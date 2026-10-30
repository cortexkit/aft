use super::*;
use crate::lsp::child_registry::LspChildRegistry;
use crate::lsp::client::{LspClient, LspEvent};
use crate::lsp::position::uri_for_path;
use crate::lsp::registry::ServerKind;
use serde_json::json;
use std::cell::Cell;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

struct ParkProbe;

impl Drop for ParkProbe {
    fn drop(&mut self) {
        PARK_PROBE.with(|probe| *probe.borrow_mut() = None);
    }
}

#[test]
fn fresh_wait_releases_manager_and_requires_each_producer_version() {
    let root = tempfile::tempdir().unwrap();
    let file = root.path().join("lib.rs");
    std::fs::write(&file, "pub fn f() {}\n").unwrap();
    let manager = Arc::new(parking_lot::Mutex::new(LspManager::new()));
    let keys = [ServerKind::Rust, ServerKind::TypeScript].map(|kind| ServerKey {
        kind,
        root: root.path().to_path_buf(),
    });
    let publish = |key: &ServerKey, version, diagnostics| LspEvent::Notification {
        server_kind: key.kind.clone(),
        root: key.root.clone(),
        method: "textDocument/publishDiagnostics".into(),
        params: Some(json!({
            "uri": uri_for_path(&file).unwrap(),
            "version": version,
            "diagnostics": diagnostics,
        })),
    };
    let expected = keys.iter().cloned().map(|key| (key, 2)).collect::<Vec<_>>();
    let pre = {
        let mut lsp = manager.lock();
        for key in &keys {
            let mut client = LspClient::spawn_with_reclaim_root(
                key.kind.clone(),
                key.root.clone(),
                Path::new("sh"),
                &["-c".into(), "exec sleep 60".into()],
                &HashMap::new(),
                lsp.event_sender_for_test(),
                LspChildRegistry::new(),
                None,
                false,
            )
            .unwrap();
            client.set_rust_analyzer_quiescent(true);
            lsp.insert_client_for_test(client);
            lsp.enqueue_event_for_test(publish(key, 1, json!([])));
        }
        lsp.drain_events();
        lsp.snapshot_pre_edit_state(&file)
    };
    let events = vec![
        publish(
            &keys[0],
            1,
            json!([{
                "range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 1}},
                "message": "stale error", "severity": 1
            }]),
        ),
        publish(&keys[1], 2, json!([])),
        publish(&keys[0], 2, json!([])),
    ];
    let parks = Rc::new(Cell::new(0));
    let observed_parks = Rc::clone(&parks);
    let parked_manager = Arc::clone(&manager);
    let _reset_probe = ParkProbe;
    PARK_PROBE.with(|probe| {
        *probe.borrow_mut() = Some(Box::new(move || {
            let lsp = parked_manager
                .try_lock()
                .expect("manager lock held across a diagnostics wait");
            observed_parks.set(observed_parks.get() + 1);
            if observed_parks.get() == 1 {
                for event in &events {
                    lsp.enqueue_event_for_test(event.clone());
                }
            }
        }));
    });
    let result = wait_for_post_edit_diagnostics_unlocked(
        || manager.lock(),
        &file,
        &expected,
        &pre,
        Instant::now() + Duration::from_secs(5),
    );
    assert!(
        parks.get() > 0,
        "the lock probe must observe a channel wait"
    );
    assert!(result.complete(), "expected both producers: {result:?}");
    assert_eq!(result.accepted_snapshots.len(), 2);
    assert!(
        result.diagnostics.is_empty(),
        "an old-version error was accepted as fresh: {result:?}"
    );
    assert!(result
        .accepted_snapshots
        .iter()
        .all(|snapshot| snapshot.document_version == 2));
}
