//! The integration harness switches semantic indexing off by default so the
//! hundreds of spawned aft processes do not each load ONNX Runtime and the
//! embedding model. These checks keep that default, the explicit opt-in, and
//! a test's own explicit choice honest.

use serde_json::json;

use super::helpers::{user_config, AftProcess};

fn semantic_feature_after_configure(
    aft: &mut AftProcess,
    config: Option<serde_json::Value>,
) -> bool {
    let project = tempfile::tempdir().expect("project dir");
    std::fs::write(project.path().join("lib.rs"), "pub fn marker() {}\n").unwrap();
    let mut request = json!({
        "id": "cfg",
        "command": "configure",
        "harness": "opencode",
        "project_root": project.path().display().to_string(),
    });
    if let Some(config) = config {
        request["config"] = config;
    }
    let configure = aft.send(&request.to_string());
    assert_eq!(
        configure["success"], true,
        "configure failed: {configure:?}"
    );
    let status = aft.send(&json!({ "id": "status", "command": "status" }).to_string());
    status["features"]["semantic_search"]
        .as_bool()
        .unwrap_or_else(|| panic!("status lacks features.semantic_search: {status:?}"))
}

#[test]
fn default_spawn_configures_semantic_off() {
    let mut aft = AftProcess::spawn();
    assert!(!semantic_feature_after_configure(&mut aft, None));
    // A test's own user config without a semantic decision gets the same default.
    assert!(!semantic_feature_after_configure(
        &mut aft,
        Some(user_config(json!({ "format_on_edit": true })))
    ));
    assert!(aft.shutdown().success());
}

#[test]
fn opted_in_spawn_keeps_the_product_default() {
    let mut aft = AftProcess::spawn_with_semantic();
    assert!(semantic_feature_after_configure(&mut aft, None));
    assert!(aft.shutdown().success());
}

#[test]
fn an_explicit_semantic_choice_in_the_request_is_respected() {
    let mut aft = AftProcess::spawn();
    assert!(semantic_feature_after_configure(
        &mut aft,
        Some(user_config(json!({ "indexes": { "semantic": true } })))
    ));
    assert!(aft.shutdown().success());
}
