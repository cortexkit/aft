use super::*;

const MESSAGE: &str = "Index storage is paused: 21.0 GiB is free on this disk and AFT keeps 32.0 GiB free. Unused view data is being cleaned up; retry later.";

struct Refused(String);

impl crate::views::trigram::LexicalSnapshotSource for Refused {
    fn acquire(&self) -> Result<crate::views::snapshot::Snapshot, String> {
        Err(self.0.clone())
    }
    fn record_change(&self, _: &Path) {}
}

impl crate::views::callgraph::CallgraphReaderSource for Refused {
    fn acquire(
        &self,
    ) -> Result<
        Arc<crate::callgraph_store::ReadonlyCallGraphStore>,
        crate::views::callgraph::CallgraphViewError,
    > {
        Err(self.0.clone().into())
    }
    fn record_change(&self, _: &Path) {}
}

fn assert_refusal(answer: Response, code: &str, message: &str) {
    assert!(!answer.success, "{:?}", answer.data);
    assert_eq!(answer.data["code"], code);
    assert_eq!(answer.data["message"], message);
}

#[test]
fn disk_reserve_lexical_routes_use_the_approved_sentence_and_recover() {
    let _git_env = crate::test_env::hermetic_git_env_guard();
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("project");
    init_git_fixture(&root);
    std::fs::write(root.join("visible.ts"), "export const needle = 1;\n").unwrap();
    let storage = temp.path().join("storage");
    let ctx = test_context();
    assert!(handle_configure_for_test(&configure_lexical_views(&root, &storage), &ctx).success);
    assert!(ctx.lexical_view_snapshot("bind").unwrap().is_ok());
    let working = ctx.lexical_source_for_test();
    let low = crate::disk_floor::set_free_space_for_test(
        &storage,
        21 * 1024 * 1024 * 1024,
        3_900_000_000_000,
    );
    let refusal = crate::blob_store::v3::publish(&storage, &"f".repeat(64), 1, None, |_| Ok(()))
        .unwrap_err()
        .to_string();
    assert_eq!(refusal, "device_free_space_floor");
    ctx.install_lexical_source_for_test(Arc::new(Refused(format!(
        "view loader [driver]: v3 base admission refused: storage_gap:{refusal}"
    ))));
    assert_refusal(
        crate::commands::grep::handle_grep(&grep_request("needle"), &ctx),
        "internal_error",
        MESSAGE,
    );
    assert_refusal(
        crate::commands::semantic_search::handle_semantic_search(
            &semantic_search_request("needle"),
            &ctx,
        ),
        "internal_error",
        MESSAGE,
    );
    let glob: RawRequest = serde_json::from_value(json!({
        "id": "reserve-glob", "command": "glob", "pattern": "**/*.ts",
    }))
    .unwrap();
    assert_refusal(
        crate::commands::glob::handle_glob(&glob, &ctx),
        "internal_error",
        MESSAGE,
    );

    let health = ctx.try_health_snapshot(&root);
    assert!(health.view_load_failures.is_empty());
    assert!(health.view_load_failure_last.is_none());
    assert!(!ctx
        .degraded_reasons()
        .contains(&crate::context::VIEW_LOAD_DEGRADED_REASON.to_string()));
    assert_eq!(health.view_storage_reserve["trigram"].message, MESSAGE);
    assert_eq!(
        health.view_storage_reserve["trigram"].code,
        "device_free_space_floor"
    );
    assert!(!health.is_fully_ready());
    assert!(!serde_json::to_value(health).unwrap()["view_storage_reserve"].is_null());
    drop(low);
    ctx.install_lexical_source_for_test(working);
    assert!(crate::commands::grep::handle_grep(&grep_request("needle"), &ctx).success);
    assert!(ctx
        .try_health_snapshot(&root)
        .view_storage_reserve
        .is_empty());
}

#[test]
fn disk_reserve_callgraph_routes_use_the_approved_sentence() {
    let _git_env = crate::test_env::hermetic_git_env_guard();
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("project");
    init_git_fixture(&root);
    std::fs::write(
        root.join("a.ts"),
        "export function target() { return 1; }\nexport function caller() { return target(); }\n",
    )
    .unwrap();
    let root = root.canonicalize().unwrap();
    let storage = temp.path().join("storage");
    let ctx = test_context();
    assert!(
        handle_configure_for_test(&configure_callgraph_views(&root, &storage, true), &ctx).success
    );
    let _low = crate::disk_floor::set_free_space_for_test(
        &storage,
        21 * 1024 * 1024 * 1024,
        3_900_000_000_000,
    );
    crate::blob_store::v3::publish(&storage, &"f".repeat(64), 1, None, |_| Ok(())).unwrap_err();
    ctx.install_callgraph_source_for_test(Arc::new(Refused(
        "view loader [driver]: device_free_space_floor".into(),
    )));
    let file = root.join("a.ts");
    let ops: [(&str, fn(&RawRequest, &AppContext) -> Response); 6] = [
        ("callers", crate::commands::callers::handle_callers),
        ("impact", crate::commands::impact::handle_impact),
        ("call_tree", crate::commands::call_tree::handle_call_tree),
        ("trace_to", crate::commands::trace_to::handle_trace_to),
        (
            "trace_to_symbol",
            crate::commands::trace_to_symbol::handle_trace_to_symbol,
        ),
        ("trace_data", crate::commands::trace_data::handle_trace_data),
    ];
    for (command, handle) in ops {
        let request: RawRequest = serde_json::from_value(json!({
            "id": format!("reserve-{command}"), "command": command,
            "file": file, "symbol": "target", "toSymbol": "caller", "expression": "target", "depth": 1,
        })).unwrap();
        assert_refusal(handle(&request, &ctx), "callgraph_unavailable", MESSAGE);
    }
    let zoom: RawRequest = serde_json::from_value(json!({
        "id": "reserve-zoom", "command": "zoom", "file": file, "symbols": ["target"], "callgraph": true,
    })).unwrap();
    let answer = crate::commands::zoom::handle_zoom(&zoom, &ctx);
    assert!(answer.success, "{:?}", answer.data);
    assert_eq!(answer.data["callgraph"]["code"], "callgraph_unavailable");
    assert_eq!(answer.data["callgraph"]["index"]["reason"], MESSAGE);
    let inspect: RawRequest = serde_json::from_value(json!({
        "id": "reserve-inspect", "command": "inspect", "scope": ["a.ts"],
        "sections": ["dead_code", "unused_exports"],
    }))
    .unwrap();
    let answer = crate::commands::inspect::handle_inspect(&inspect, &ctx);
    assert!(answer.success, "{:?}", answer.data);
    assert_eq!(answer.data["complete"], false);
    for category in ["dead_code", "unused_exports"] {
        assert_eq!(answer.data["summary"][category]["status"], "unavailable");
        assert_eq!(
            answer.data["summary"][category]["gaps"][0]["kind"],
            "tier2_unavailable"
        );
        assert_eq!(
            answer.data["summary"][category]["gaps"][0]["reason"],
            MESSAGE
        );
    }
    assert!(
        !serde_json::to_string(&answer.data)
            .unwrap()
            .contains("callgraph store unavailable:"),
        "{:?}",
        answer.data
    );
    assert!(!serde_json::to_string(&answer.data)
        .unwrap()
        .contains("This is an AFT bug"));
    let health = ctx.try_health_snapshot(&root);
    assert!(health.view_load_failures.is_empty());
    assert!(health.view_load_failure_last.is_none());
    assert_eq!(health.view_storage_reserve["callgraph"].message, MESSAGE);
    ctx.note_view_load_success("trigram");
    assert!(ctx
        .try_health_snapshot(&root)
        .view_storage_reserve
        .contains_key("callgraph"));
    ctx.note_view_load_success("callgraph");
    assert!(ctx
        .try_health_snapshot(&root)
        .view_storage_reserve
        .is_empty());
}

#[test]
fn disk_reserve_probe_failure_keeps_the_existing_bug_treatment() {
    let temp = tempfile::tempdir().unwrap();
    let storage = temp.path().join("storage");
    let mut config = crate::config::Config::default();
    config.storage_dir = Some(storage.clone());
    let ctx = AppContext::new(Box::new(crate::parser::TreeSitterProvider::new()), config);
    let _failed_probe = crate::disk_floor::set_free_space_probe_failure_for_test(&storage);
    let error = crate::blob_store::v3::publish(&storage, &"f".repeat(64), 1, None, |_| Ok(()))
        .unwrap_err()
        .to_string();
    assert_eq!(error, "device_free_space_floor");
    // Even if probing starts working before the reply, the failed admission
    // has no measurement and must retain the unexpected-error treatment.
    drop(_failed_probe);
    let _now_measured = crate::disk_floor::set_free_space_for_test(
        &storage,
        21 * 1024 * 1024 * 1024,
        3_900_000_000_000,
    );
    ctx.note_view_load_failure("trigram", &error);
    let message = crate::context::view_load_failure_message(&error);
    assert_eq!(message, "device_free_space_floor\nThis is an AFT bug: tell the user to report it with `npx @cortexkit/aft doctor --issue`; don't run it yourself.");
    assert_refusal(
        crate::context::view_load_error_response("probe", message.clone()),
        "internal_error",
        &message,
    );
    let health = ctx.try_health_snapshot(temp.path());
    assert_eq!(health.view_load_failures.get("trigram:other"), Some(&1));
    assert_eq!(health.view_load_failure_last.unwrap().error, error);
    assert!(health.view_storage_reserve.is_empty());
    assert!(ctx
        .degraded_reasons()
        .contains(&crate::context::VIEW_LOAD_DEGRADED_REASON.to_string()));
}
