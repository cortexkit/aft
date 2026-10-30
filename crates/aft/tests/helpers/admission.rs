#![allow(dead_code)]

use aft::context::AppContext;
use std::time::{Duration, Instant};

/// Drive post-ack maintenance before a fixture observes configured index work.
/// This does not admit a folder: a negative decision is returned unchanged.
pub fn settle(ctx: &AppContext) -> serde_json::Value {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        aft::runtime_drain::drain_deferred_configure_maintenance(ctx);
        let status = ctx.build_status_snapshot();
        if status["folder"]["state"] == "decided" {
            return status["folder"].clone();
        }
        assert!(
            Instant::now() < deadline,
            "folder admission did not settle: {status:?}"
        );
        std::thread::sleep(Duration::from_millis(1));
    }
}

/// Complete a known project's folder check with all indexes disabled. Tests of
/// deferred index loading can then send an index-enabled configure request and
/// inspect state before background maintenance releases the index loaders.
pub fn prepare_project(ctx: &AppContext, root: &std::path::Path, storage: &std::path::Path) {
    let request = serde_json::from_value(serde_json::json!({
        "id": "fixture-admission", "command": "configure", "harness": "opencode",
        "project_root": root, "storage_dir": storage,
        "config": [{"tier": "user", "source": root.join("fixture-admission.jsonc"),
            "doc": serde_json::json!({
                "indexes": {"trigram": false, "semantic": false, "callgraph": false},
                "views": {"enabled": false}
            }).to_string()}]
    }))
    .unwrap();
    let response = aft::commands::configure::handle_configure(&request, ctx);
    assert!(response.success, "project setup failed: {response:?}");
    let folder = settle(ctx);
    assert_eq!(folder["classification"], "project", "{folder}");
}
