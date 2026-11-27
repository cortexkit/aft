use crate::config::Config;
use crate::lsp::position::uri_for_path;
use crate::lsp::registry::ServerKind;
use crate::lsp::roots::ServerKey;
use aft_engine::test_bridge::search_misc::lsp::manager::regression_tests::publish_after_document_sync_for_test;
use serde_json::json;

#[path = "../../../../aft/tests/helpers/fake_lsp.rs"]
mod fake_lsp;

#[test]
fn lsp_inspect_keeps_push_only_document_open_to_certify_a_later_push() {
    let dir = tempfile::tempdir().unwrap();
    let root = crate::path_identity::canonicalize_normalized(dir.path());
    let file = root.join("src").join("lib.rs");
    std::fs::create_dir(root.join("src")).unwrap();
    std::fs::write(&file, "pub fn f() {}\n").unwrap();
    std::fs::write(root.join("Cargo.toml"), "[package]\nname = 'push-only'\n").unwrap();
    let key = ServerKey {
        kind: ServerKind::Rust,
        root: root.clone(),
    };
    let ctx = crate::test_context(
        crate::context::default_language_provider_factory(),
        Config {
            project_root: Some(root),
            ..Config::default()
        },
    );
    {
        let mut manager = ctx.lsp();
        manager.override_binary(key.kind.clone(), fake_lsp::fake_server_binary());
        manager.set_extra_env("AFT_FAKE_LSP_PULL", "0");
        manager.set_extra_env("AFT_FAKE_LSP_DISABLE_PUSH", "1");
    }
    let req = serde_json::from_value(json!({"id":"inspect", "command":"lsp_inspect", "file":file}))
        .unwrap();
    let inspect = crate::commands::lsp_inspect::handle_lsp_inspect(&req, &ctx);
    assert_eq!(
        inspect.data["pull_results"][0]["status"],
        "pull_not_supported"
    );

    let mut manager = ctx.lsp();
    let version = publish_after_document_sync_for_test(
        &mut manager,
        &key,
        &file,
        &json!({
            "uri": uri_for_path(&file).unwrap(),
            "diagnostics": [{
                "range": {"start":{"line":0,"character":0},"end":{"line":0,"character":3}},
                "severity": 1, "code": "late", "message": "late push-only diagnostic"
            }]
        }),
    )
    .expect("push-only document stays open and has a sync order");
    let (_, entry) = manager
        .diagnostics_store_for_test()
        .entries_for_file(&file)
        .into_iter()
        .find(|(server, _)| *server == &key)
        .unwrap();
    assert_eq!(entry.version, Some(version));
    assert_eq!(
        entry.content_hash,
        Some(*blake3::hash(b"pub fn f() {}\n").as_bytes())
    );
    assert_eq!(
        manager.get_diagnostics_for_file(&file)[0].message,
        "late push-only diagnostic"
    );
}
