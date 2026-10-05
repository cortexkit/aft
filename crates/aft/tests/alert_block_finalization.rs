use std::path::Path;

use aft::config::Config;
use aft::context::AppContext;
use aft::parser::TreeSitterProvider;
use aft::protocol::Response;
use aft::response_finalize::alert_render::{
    AlertDiagnostic, AlertEngine, AlertObservation, EXCLUDED_FINALIZATION_COMMANDS,
};
use aft::response_finalize::finalize_response_for_dispatch_root;
use serde_json::{json, Value};

fn error(file: &str, line: u32, message: &str) -> AlertDiagnostic {
    AlertDiagnostic::error(file, line, message)
}

#[test]
fn cross_root_finalization_consumes_only_the_explicit_dispatch_root() {
    let fixture: Value = serde_json::from_str(include_str!("fixtures/alert_block/cross_root.json"))
        .expect("cross-root fixture JSON");
    let session = fixture["session"].as_str().expect("fixture session");
    let dispatch_root = Path::new(
        fixture["dispatch_root"]
            .as_str()
            .expect("fixture dispatch root"),
    );
    let other_root = Path::new(fixture["other_root"].as_str().expect("fixture other root"));
    let producer = fixture["producer"].as_str().expect("fixture producer");
    let dispatch_alert = fixture["dispatch_alert"]
        .as_str()
        .expect("fixture dispatch alert");
    let other_alert = fixture["other_alert"]
        .as_str()
        .expect("fixture other alert");
    let mut alerts = AlertEngine::default();

    for (root, message) in [(dispatch_root, dispatch_alert), (other_root, other_alert)] {
        alerts.observe_authoritative_batch(
            session,
            root,
            [AlertObservation::new(producer, Vec::new())],
        );
        alerts.observe_authoritative(
            session,
            root,
            producer,
            vec![error("src/lib.rs", 11, message)],
        );
    }

    let dispatch = alerts
        .finalize(session, dispatch_root, "inspect")
        .expect("explicit dispatch root must render its own alert");
    assert!(dispatch.text.contains(dispatch_alert));
    assert!(!dispatch.text.contains(other_alert));
    assert!(alerts
        .finalize(session, other_root, "read")
        .expect("other root remains pending")
        .text
        .contains(other_alert));
}

#[test]
fn finalization_appends_one_server_reminder_without_a_status_bar_envelope() {
    let root = tempfile::tempdir().expect("dispatch root");
    let root_path = root.path();
    let ctx = AppContext::new(
        Box::new(TreeSitterProvider::new()),
        crate::context_storage::isolate(Config {
            project_root: Some(root_path.to_path_buf()),
            ..Config::default()
        }),
    );
    let mut alerts = AlertEngine::default();
    alerts.observe_authoritative("session", root_path, "server", Vec::new());
    alerts.observe_authoritative(
        "session",
        root_path,
        "server",
        vec![error("src/lib.rs", 8, "E0308 expected type")],
    );
    let mut response = Response::success("response", json!({ "text": "ordinary output" }));

    finalize_response_for_dispatch_root(
        &mut response,
        &ctx,
        &mut alerts,
        "session",
        root_path,
        "inspect",
        true,
    );

    let text = response.data["text"].as_str().expect("finalized text");
    assert_eq!(text.matches("<system-reminder>").count(), 1);
    assert!(text.contains("E0308 expected type"));
    assert!(response.data.get("status_bar").is_none());
}

#[test]
fn excluded_commands_are_a_closed_non_consuming_set() {
    let root = Path::new("/fixture/root");
    let mut alerts = AlertEngine::default();
    alerts.observe_authoritative("session", root, "server", Vec::new());
    alerts.observe_authoritative("session", root, "server", vec![error("a.rs", 1, "boom")]);

    for command in EXCLUDED_FINALIZATION_COMMANDS {
        assert!(alerts.finalize("session", root, command).is_none());
    }
    assert_eq!(alerts.agent_visible_response_ordinal("session"), 0);
    assert!(alerts.finalize("session", root, "inspect").is_some());
}
