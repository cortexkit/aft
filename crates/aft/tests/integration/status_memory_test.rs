use std::sync::Arc;

use aft::commands::status::handle_status;
use aft::config::Config;
use aft::context::{App, AppContext};
use aft::executor::Executor;
use aft::protocol::RawRequest;
use cortexkit_paths::ProjectRootId;
use serde_json::json;

fn status_request() -> RawRequest {
    RawRequest {
        id: "memory-status".to_string(),
        command: "status".to_string(),
        lsp_hints: None,
        session_id: None,
        params: json!({}),
    }
}

#[test]
fn status_memory_attributes_every_registered_root_and_exposes_residual() {
    let first = tempfile::tempdir().expect("first root");
    let second = tempfile::tempdir().expect("second root");
    let first_root = std::fs::canonicalize(first.path()).expect("canonical first root");
    let second_root = std::fs::canonicalize(second.path()).expect("canonical second root");
    let app = App::default_shared();
    let first_ctx = Arc::new(AppContext::from_app(
        Arc::clone(&app),
        crate::context_storage::isolate(Config {
            project_root: Some(first_root.clone()),
            ..Config::default()
        }),
    ));
    let second_ctx = Arc::new(AppContext::from_app(
        app,
        crate::context_storage::isolate(Config {
            project_root: Some(second_root.clone()),
            ..Config::default()
        }),
    ));
    let executor = Executor::new();
    assert!(executor.register_actor(
        ProjectRootId::from_path(&first_root).expect("first root id"),
        Arc::clone(&first_ctx),
    ));
    assert!(executor.register_actor(
        ProjectRootId::from_path(&second_root).expect("second root id"),
        second_ctx,
    ));

    let response = handle_status(&status_request(), &first_ctx);
    let memory = &response.data["memory"];
    assert_eq!(memory["roots_status"], "ready");
    let roots = memory["roots"].as_object().expect("memory roots object");
    // Presence, not exact count: the actor registry backing the memory view is
    // process-wide, so under plain `cargo test` (thread-per-test, as on the
    // Windows CI job) a concurrently running test can legitimately register
    // additional roots. Nextest's process-per-test isolation hides this.
    assert!(
        roots.len() >= 2,
        "expected at least the two registered roots: {roots:?}"
    );
    for root in [first_root, second_root] {
        // Look up by the registry's own key form: register_actor keys memory
        // contexts on ProjectRootId::as_path, which on Windows is the
        // non-verbatim shape while fs::canonicalize returns `\\?\` paths.
        let key = ProjectRootId::from_path(&root)
            .expect("root id for lookup")
            .as_path()
            .display()
            .to_string();
        let estimate = roots.get(&key).expect("registered root estimate");
        for subsystem in [
            "semantic",
            "trigram",
            "symbols",
            "callgraph",
            "inspect",
            "bash",
            "lsp",
            "parser_pool",
        ] {
            assert!(estimate.get(subsystem).is_some(), "missing {subsystem}");
        }
    }
    let process = &memory["process"];
    let sqlite_used = process["sqlite"]["memory_used_bytes"]
        .as_u64()
        .expect("SQLite memory used");
    assert_eq!(process["sqlite"]["status"], "measured");
    assert!(
        process["sqlite"]["memory_highwater_bytes"]
            .as_u64()
            .expect("SQLite memory highwater")
            >= sqlite_used
    );
    assert!(process["allocator"].get("status").is_some());
    assert!(process["allocator"].get("bytes_in_use").is_some());
    assert!(process["allocator"].get("size_allocated").is_some());
    assert!(process["allocator"].get("retained_slack_bytes").is_some());

    let attributed = process["total_attributed_bytes"]
        .as_u64()
        .expect("attributed bytes");
    assert!(attributed >= sqlite_used);
    assert!(attributed < 16 * 1024 * 1024 * 1024);
    assert!(process.get("unattributed_bytes").is_some());
    assert!(process.get("rss_status").is_some());
    assert!(
        !memory.to_string().contains("sqlite_internal_bytes"),
        "process-wide SQLite counters replace per-root SQLite gaps"
    );
}
