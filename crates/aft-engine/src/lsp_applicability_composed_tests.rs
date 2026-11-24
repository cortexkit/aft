#[test]
fn applicability_walk_manifest_change_invalidates_from_watcher() {
    let temp = tempfile::tempdir().unwrap();
    let root = crate::path_identity::canonicalize_normalized(temp.path());
    std::fs::write(root.join("main.go"), "package main").unwrap();
    let config = config(&root);
    let before = walk_applicable_area(&root, None, &config, None).unwrap();
    assert!(!before
        .candidates
        .iter()
        .any(|c| c.key.kind == ServerKind::Go));
    std::fs::write(root.join("go.mod"), "module example.test/app\n").unwrap();
    let ctx = crate::context::AppContext::new(
        Box::new(crate::parser::TreeSitterProvider::new()),
        config.clone(),
    );
    ctx.lsp_forward_watcher_file_events(&[root.join("go.mod")]);
    let after = walk_applicable_area(&root, None, &config, None).unwrap();
    assert!(after
        .candidates
        .iter()
        .any(|c| c.key.kind == ServerKind::Go));
}
