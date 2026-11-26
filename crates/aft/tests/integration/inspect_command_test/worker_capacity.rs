use super::super::lsp_manager_test::rust_worker_policy_test_guard;
use super::*;

#[test]
fn rust_worker_inspect_capacity_refusal_is_partial_and_names_cap() {
    let _guard = rust_worker_policy_test_guard();
    let fixtures = (0..2)
        .map(|_| single_crate_fixture("occupied-worker-slot"))
        .collect::<Vec<_>>();
    let mut occupied = Vec::new();
    for (_, _, file) in &fixtures {
        let mut manager = aft::lsp::manager::LspManager::new();
        manager.override_binary(ServerKind::Rust, fake_server_path());
        manager.disable_rust_cargo_for_worker();
        assert_eq!(
            crate::test_helpers::settled_lsp(&mut manager, |startup_manager| startup_manager
                .ensure_server_for_file(file, &Config::default()))
            .len(),
            1
        );
        occupied.push(manager);
    }
    let (_temp, root, _) = single_crate_fixture("refused-worker-slot");
    let ctx = configured_context(&root);
    ctx.lsp()
        .override_binary(ServerKind::Rust, fake_server_path());
    let response = inspect_tool_call(
        &ctx,
        json!({
            "id": "worker-inspect-cap", "command": "inspect", "scope": "src/lib.rs",
            "sections": "diagnostics", "worker_session": true,
        }),
    );
    assert_eq!(response["inspect_terminal"], "partial", "{response:#}");
    let text = response["text"].as_str().unwrap();
    assert!(text.contains("rust_analyzer_capacity_exhausted"), "{text}");
    assert!(text.contains("cap=2"), "{text}");
    assert_eq!(ctx.lsp().server_count(), 0);
}

#[test]
fn rust_worker_inspect_capacity_zero_reports_partial_without_spawn() {
    let _guard = rust_worker_policy_test_guard();
    let (_temp, root, _) = single_crate_fixture("worker-disabled-by-cap");
    let ctx = configured_context(&root);
    ctx.update_config(|config| config.lsp_worker_rust_analyzers = 0);
    ctx.lsp()
        .override_binary(ServerKind::Rust, fake_server_path());
    let response = inspect_tool_call(
        &ctx,
        json!({
            "id": "worker-inspect-cap-zero", "command": "inspect", "scope": "src/lib.rs",
            "sections": "diagnostics", "worker_session": true,
        }),
    );
    assert_eq!(response["inspect_terminal"], "partial", "{response:#}");
    let text = response["text"].as_str().unwrap();
    assert!(text.contains("rust_analyzer_capacity_exhausted"), "{text}");
    assert!(text.contains("cap=0 reached (current=0)"), "{text}");
    assert_eq!(ctx.lsp().server_count(), 0);
}
