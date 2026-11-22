use crate::context::AppContext;
use crate::protocol::RawRequest;
use std::path::PathBuf;

/// Publish an external checkout containing a known search hit.
pub fn external_search_fixture() -> (tempfile::TempDir, AppContext, PathBuf, RawRequest) {
    let fixture = tempfile::tempdir().unwrap();
    let storage = fixture.path().join("storage");
    let configured = fixture.path().join("configured");
    let external = fixture.path().join("external");
    for root in [&configured, &external] {
        std::fs::create_dir_all(root.join("src")).unwrap();
        assert!(std::process::Command::new("git")
            .args(["init", "-q"])
            .current_dir(root)
            .status()
            .unwrap()
            .success());
    }
    std::fs::write(
        external.join("src/lib.rs"),
        "pub fn hook_parity_witness() {}\n",
    )
    .unwrap();
    let context = |root: PathBuf| {
        let mut config = crate::config::Config {
            project_root: Some(root),
            storage_dir: Some(storage.clone()),
            ..Default::default()
        };
        config.indexes.trigram = true;
        config.indexes.semantic = false;
        config.indexes.callgraph = false;
        super::test_context(Box::new(crate::parser::TreeSitterProvider::new()), config)
    };
    let external = crate::path_identity::canonicalize_normalized(&external);
    let configured = crate::path_identity::canonicalize_normalized(&configured);
    aft_engine::test_bridge::display_data::surfaces::serve_text_index_for_test(&context(
        external.clone(),
    ));
    let request = RawRequest {
        id: "external-hook".to_owned(),
        command: "semantic_search".to_owned(),
        session_id: None,
        lsp_hints: None,
        params: serde_json::json!({"query":"hook_parity_witness", "hint":"literal", "path":external}),
    };
    (fixture, context(configured), external, request)
}
