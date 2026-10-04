//! A bounded request drain can leave a ready index behind the live filesystem.
use aft::commands::{glob::handle_glob, grep::handle_grep, read::handle_read};
use aft::config::Config;
use aft::context::{default_language_provider_factory, AppContext};
use aft::protocol::RawRequest;
use aft::run_tool_call::{run_tool_call, ToolCallContext, ToolCallOutcome};
use aft::runtime_drain::{drain_watcher_events, WATCHER_PATH_DRAIN_BATCH_CAP};
use aft::search_index::SearchIndex;
use aft::subc_format::FormatContext;
use aft::watcher_filter::WatcherDispatchEvent;
use serde_json::{json, Value};

fn call(ctx: &AppContext, name: &str, arguments: Value) -> aft::run_tool_call::ToolCallResult {
    call_with_drain(ctx, name, arguments, false)
}

fn call_with_drain(
    ctx: &AppContext,
    name: &str,
    arguments: Value,
    drain_after: bool,
) -> aft::run_tool_call::ToolCallResult {
    let root = ctx.config().project_root.clone().unwrap();
    let outcome = run_tool_call(
        name,
        arguments,
        &FormatContext::default(),
        &ToolCallContext {
            request_id: name.into(),
            project_root: root,
            standard_edit_grammar: true,
            session_id: None,
            diagnostics_on_edit: false,
            preview: false,
            edit_slot_survives: None,
            report_registration_downgrade: false,
            disabled_tools: None,
            worker_session: false,
        },
        ctx,
        &|request: RawRequest, ctx| {
            let response = match request.command.as_str() {
                "grep" => handle_grep(&request, ctx),
                "glob" => handle_glob(&request, ctx),
                "read" => handle_read(&request, ctx),
                "outline" => aft::commands::outline::handle_outline(&request, ctx),
                "zoom" => aft::commands::zoom::handle_zoom(&request, ctx),
                "semantic_search" => {
                    aft::commands::semantic_search::handle_semantic_search(&request, ctx)
                }
                "callers" => aft::commands::callers::handle_callers(&request, ctx),
                other => panic!("unexpected command {other}"),
            };
            if drain_after {
                while ctx.watcher_drain_has_work() {
                    drain_watcher_events(ctx);
                }
            }
            response
        },
        None,
        None,
    );
    let ToolCallOutcome::Unary(result) = outcome;
    assert!(result.response.success, "{}", result.text);
    result
}

#[test]
fn request_watcher_burst_discloses_unapplied_index_changes() {
    let temp = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(temp.path()).unwrap();
    let storage = tempfile::tempdir().unwrap();
    std::fs::write(root.join("old.rs"), "pub fn old() {}\n").unwrap();
    let ctx = AppContext::new(
        default_language_provider_factory(),
        Config {
            project_root: Some(root.clone()),
            storage_dir: Some(storage.path().to_path_buf()),
            ..Config::default()
        },
    );
    ctx.set_heavy_root_work_allowed(true);
    ctx.set_harness(aft::harness::Harness::Opencode);
    let project_key = ctx.memoized_artifact_cache_key(&root);
    aft::root_cache::configure_artifact_access(&root, &project_key, false);
    let graph = aft::callgraph_store::CallGraphStore::open(ctx.callgraph_store_dir(), root.clone())
        .unwrap();
    graph.cold_build(&[root.join("old.rs")]).unwrap();
    drop(graph);
    let graph = aft::callgraph_store::CallGraphStore::open_readonly(
        ctx.callgraph_store_dir(),
        root.clone(),
    )
    .unwrap()
    .unwrap();
    *ctx.callgraph_store().write().unwrap() = Some(std::sync::Arc::new(graph));
    let mut index = SearchIndex::build(&root);
    index.ready = true;
    *ctx.search_index().write().unwrap() = Some(index);
    let (tx, rx) = crossbeam_channel::unbounded();
    *ctx.watcher_rx().lock() = Some(rx);
    let total = WATCHER_PATH_DRAIN_BATCH_CAP * 2 + 1;
    let mut paths: Vec<_> = (0..total - 1)
        .map(|i| root.join(format!("a-{i:05}.rs")))
        .collect();
    let fresh = root.join("z-new.rs");
    std::fs::write(&fresh, "pub fn burst_marker() {}\n").unwrap();
    paths.push(fresh.clone());
    tx.send(WatcherDispatchEvent::Paths(paths)).unwrap();
    drain_watcher_events(&ctx);
    let applied = ctx.pending_tier2_paths().len();
    assert!(
        applied > 0 && applied <= WATCHER_PATH_DRAIN_BATCH_CAP,
        "applied {applied} of {total}"
    );
    assert!(ctx.watcher_drain_has_work());

    // The candidate index has not seen the new file. This is a real false
    // negative, not a synthetic response marked partial by the test itself.
    let grep = call(&ctx, "grep", json!({"pattern":"burst_marker"}));
    assert_eq!(grep.response.data["total_matches"], 0);
    let glob = call(&ctx, "glob", json!({"pattern":"z-new.rs"}));
    assert_eq!(glob.response.data["total"], 0);
    for result in [grep, glob] {
        assert_eq!(result.response.data["complete"], false);
        assert!(result.response.data["gaps"]
            .as_array()
            .unwrap()
            .iter()
            .any(|gap| gap["kind"] == "watcher_pending"));
        assert!(
            result.text.contains("Watcher changes pending"),
            "{}",
            result.text
        );
    }
    // File reads are live even while the same index is stale.
    let read = call(&ctx, "read", json!({"path":fresh}));
    assert!(read.text.contains("burst_marker"));
    assert!(read.response.data.get("gaps").is_none());

    // Outline and zoom parse the file on disk (the symbol cache is checked
    // against its mtime and size), so a watcher backlog cannot make them stale.
    for (name, arguments) in [
        ("outline", json!({"target":root.join("old.rs")})),
        ("zoom", json!({"path":root.join("old.rs"), "symbols":"old"})),
    ] {
        let result = call(&ctx, name, arguments);
        assert!(
            !result.text.contains("Watcher changes pending"),
            "{name}: {}",
            result.text
        );
    }

    for (name, arguments) in [
        ("search", json!({"pattern":"old"})),
        (
            "callgraph",
            json!({"op":"callers", "path":root.join("old.rs"), "symbol":"old"}),
        ),
        ("inspect", json!({"sections":["todos"]})),
    ] {
        let result = call(&ctx, name, arguments);
        assert_eq!(
            result.response.data["complete"], false,
            "{name}: {}",
            result.text
        );
        assert!(
            result.text.contains("Watcher changes pending"),
            "{name}: {}",
            result.text
        );
    }

    // Maintenance can finish between execution and formatting. That does not
    // make the already-computed false negative fresh retroactively.
    let deferred_request: RawRequest = serde_json::from_value(json!({
        "id":"deferred-inspect", "command":"inspect", "sections":["todos"]
    }))
    .unwrap();
    let ctx = std::sync::Arc::new(ctx);
    let aft::response_finalize::DispatchOutcome::Deferred(mut deferred) =
        aft::commands::inspect::handle_inspect_deferred(
            &deferred_request,
            std::sync::Arc::clone(&ctx),
        )
    else {
        panic!("inspect should register its detached producer");
    };
    let raced = call_with_drain(&ctx, "grep", json!({"pattern":"burst_marker"}), true);
    assert_eq!(raced.response.data["total_matches"], 0);
    assert!(raced.text.contains("Watcher changes pending"));
    assert!(!ctx.watcher_drain_has_work());
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
    let inspect = loop {
        if let Some(response) = (deferred.poll)(&ctx) {
            break response;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "deferred inspect completion"
        );
        std::thread::sleep(std::time::Duration::from_millis(10));
    };
    assert!(inspect.success, "{}", inspect.data);
    assert_eq!(inspect.data["complete"], false);
    assert!(
        aft::subc_format::format_response("inspect", &inspect, false)
            .contains("Watcher changes pending")
    );
    assert_eq!(ctx.pending_tier2_paths().len(), total);
    let grep = call(&ctx, "grep", json!({"pattern":"burst_marker"}));
    assert_eq!(grep.response.data["total_matches"], 1);
    assert!(!grep.text.contains("Watcher changes pending"));
    let glob = call(&ctx, "glob", json!({"pattern":"z-new.rs"}));
    assert_eq!(glob.response.data["total"], 1);
}
