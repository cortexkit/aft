use super::*;
use crate::commands::data_search::ExternalSearch;

fn fixture() -> (tempfile::TempDir, AppContext, PathBuf, RawRequest) {
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
        AppContext::new(Box::new(crate::parser::TreeSitterProvider::new()), config)
    };
    let external = crate::path_identity::canonicalize_normalized(&external);
    let configured = crate::path_identity::canonicalize_normalized(&configured);
    crate::data_layer::surfaces::serve_text_index_for_test(&context(external.clone()));
    let request = RawRequest {
        id: "external-hook".to_owned(),
        command: "semantic_search".to_owned(),
        session_id: None,
        lsp_hints: None,
        params: serde_json::json!({"query":"hook_parity_witness", "hint":"literal", "path":external}),
    };
    (fixture, context(configured), external, request)
}

#[test]
fn external_search_hook_preserves_direct_child_response_bytes() {
    let (_fixture, caller, external, req) = fixture();
    crate::subc::lower_hooks::register();
    let handle = ExternalSearch::of(&caller, &external).expect("published external checkout");
    let direct = with_absolute_result_paths(|| handle_parent_child_search(&req, &handle.engine));
    let (hooked, drift) = handle.search(&req, &caller);
    assert!(direct.success, "{direct:?}");
    assert_eq!(direct.data["fully_degraded"], false, "{direct:?}");
    assert_eq!(drift, 0);
    assert_eq!(
        serde_json::to_vec(&hooked).unwrap(),
        serde_json::to_vec(&direct).unwrap()
    );
}

#[test]
fn external_search_without_hook_uses_existing_bounded_fallback() {
    const CHILD: &str = "AFT_TEST_EXTERNAL_SEARCH_WITHOUT_HOOK";
    if std::env::var_os(CHILD).is_none() {
        let name = concat!(
            module_path!(),
            "::external_search_without_hook_uses_existing_bounded_fallback"
        );
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", name.split_once("::").unwrap().1, "--nocapture"])
            .env(CHILD, "1")
            .output()
            .unwrap();
        assert!(
            output.status.success() && String::from_utf8_lossy(&output.stdout).contains("1 passed"),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        return;
    }

    let (_fixture, caller, external, req) = fixture();
    assert!(ExternalSearch::of(&caller, &external).is_none());
    // Avoid the public entry's registration, but run the same request body.
    let reply = crate::bounded_io::with_skip_disclosure(|| {
        handle_semantic_search_with_disclosure(&req, &caller)
    });
    assert!(reply.success, "{reply:?}");
    assert_eq!(
        reply.data["semantic_status"], "external_unindexed",
        "{reply:?}"
    );
    assert!(reply.data["text"]
        .as_str()
        .unwrap()
        .contains(&external.join("src/lib.rs").display().to_string()));
}
