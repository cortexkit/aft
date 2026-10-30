use aft::config::Config;
use aft::context::AppContext;
use aft::parser::TreeSitterProvider;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::process::Command;

fn git(root: &Path, args: &[&str]) {
    let output = Command::new("git")
        .current_dir(root)
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn certify_typescript(ctx: &AppContext, file: &Path) -> Value {
    let request = serde_json::from_value(json!({
        "id": "certify-worktree", "command": "inspect", "scope": file,
        "sections": ["diagnostics"],
    }))
    .unwrap();
    let response = serde_json::to_value(aft::commands::inspect::handle_inspect_tool_call(
        &request, ctx,
    ))
    .unwrap();
    assert_eq!(response["success"], true, "{response:#}");
    assert_eq!(
        response["summary"]["diagnostics"]["coverage"]["authoritative"], 1,
        "request-scoped certification must cover the file: {response:#}"
    );
    assert!(
        response["gaps"]
            .as_array()
            .into_iter()
            .flatten()
            .all(|gap| gap["kind"] != "uncovered_file"),
        "the TypeScript file must have no certification gap: {response:#}"
    );
    response
}

#[test]
fn fresh_git_worktree_produces_typescript_diagnostics_without_installing() {
    // Opt into real-server coverage with an externally installed npm bin directory.
    // Normal test runs still exercise SDK selection without network or Node.
    let Some(bin) = std::env::var_os("AFT_TEST_LSP_BIN_DIR").map(PathBuf::from) else {
        eprintln!("SKIP fresh_git_worktree_produces_typescript_diagnostics_without_installing: AFT_TEST_LSP_BIN_DIR not set (requires typescript-language-server and typescript)");
        return;
    };
    if !Command::new("node")
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success())
    {
        eprintln!("SKIP fresh_git_worktree_produces_typescript_diagnostics_without_installing: node unavailable");
        return;
    }
    assert!(
        bin.join("typescript-language-server").is_file(),
        "server missing from explicit test bin directory"
    );
    crate::test_helpers::disable_in_process_file_watcher();
    let temp = tempfile::tempdir().unwrap();
    let main = temp.path().join("main");
    let fresh = temp.path().join("fresh");
    std::fs::create_dir(&main).unwrap();
    git(&main, &["init", "-q"]);
    std::fs::write(
        main.join("package.json"),
        r#"{"devDependencies":{"typescript":"5.9.3","@biomejs/biome":"2.2.4"}}"#,
    )
    .unwrap();
    std::fs::write(main.join("tsconfig.json"), "{}").unwrap();
    std::fs::write(main.join("index.ts"), "const value: number = 'wrong';\n").unwrap();
    git(&main, &["add", "."]);
    git(
        &main,
        &[
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.com",
            "commit",
            "-qm",
            "fixture",
        ],
    );
    git(&main, &["worktree", "add", "-q", fresh.to_str().unwrap()]);
    let fresh = fresh.canonicalize().unwrap();
    let file = fresh.join("index.ts");
    let mut before: Vec<_> = std::fs::read_dir(&fresh)
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    let storage = temp.path().join("storage");
    let ctx = AppContext::new(
        Box::new(TreeSitterProvider::new()),
        Config {
            storage_dir: Some(storage.clone()),
            ..Config::default()
        },
    )
    .with_test_checkpoint_store();
    let configure = serde_json::from_value(json!({
        "id": "configure-worktree", "command": "configure", "harness": "opencode",
        "project_root": fresh, "storage_dir": storage, "lsp_paths_extra": [bin],
        "config": crate::test_helpers::user_config(json!({
            "indexes": {"trigram": false, "semantic": false, "callgraph": false},
            "lsp": {"disabled": ["biome", "oxlint"]},
        })),
    }))
    .unwrap();
    let configured =
        serde_json::to_value(aft::commands::configure::handle_configure(&configure, &ctx)).unwrap();
    assert_eq!(configured["success"], true, "{configured:#}");
    ctx.update_config(|config| config.inspect.diagnostics_timeout_ms = 15_000);
    let config = ctx.config().clone();

    // typescript-language-server pushes omit document versions. Scoped inspect
    // instead certifies sync replies against the open version, as production does.
    std::fs::write(&file, "const value: number = 1;\n").unwrap();
    ctx.lsp()
        .notify_file_changed_versioned(&file, "const value: number = 1;\n", &config)
        .unwrap();
    let before_edit = certify_typescript(&ctx, &file);
    assert_eq!(
        before_edit["summary"]["diagnostics"]["errors"], 0,
        "{before_edit:#}"
    );
    let previous_versions = ctx.lsp().snapshot_pre_edit_state(&file);
    assert_eq!(previous_versions.len(), 1);

    // Restore the committed error as a new edit, so a cached clean certificate
    // cannot satisfy the assertion about the current document.
    let content = "const value: number = 'wrong';\n";
    std::fs::write(&file, content).unwrap();
    let expected_versions = ctx
        .lsp()
        .notify_file_changed_versioned(&file, content, &config)
        .unwrap();
    assert!(
        !expected_versions.is_empty(),
        "TypeScript server should match the file"
    );
    for (key, version) in &expected_versions {
        assert!(Some(*version) > previous_versions[key].document_version_at_capture);
    }
    let response = certify_typescript(&ctx, &file);
    assert_eq!(
        response["summary"]["diagnostics"]["errors"], 1,
        "{response:#}"
    );
    let current_versions = ctx.lsp().snapshot_pre_edit_state(&file);
    for (key, version) in &expected_versions {
        assert_eq!(
            current_versions[key].document_version_at_capture,
            Some(*version)
        );
    }
    let diagnostics = response["details"]["diagnostics"].as_array().unwrap();
    assert!(
        diagnostics.iter().any(|d| d["message"]
            .as_str()
            .is_some_and(|message| message.contains("not assignable to type 'number'"))),
        "expected TS2322 certified after the edit, got {response:#}"
    );
    let notes = ctx.lsp().runtime_notes();
    assert!(
        notes
            .iter()
            .any(|n| n.contains("not the project's pinned TypeScript")),
        "{notes:?}"
    );
    ctx.lsp().shutdown_all();
    let mut after: Vec<_> = std::fs::read_dir(&fresh)
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    before.sort();
    after.sort();
    assert_eq!(before, after);
    assert!(!fresh.join("node_modules").exists());
    let status = Command::new("git")
        .current_dir(&fresh)
        .args(["status", "--porcelain", "--untracked-files=all"])
        .output()
        .unwrap();
    assert!(status.status.success());
    assert!(
        status.stdout.is_empty(),
        "worktree modified: {}",
        String::from_utf8_lossy(&status.stdout)
    );
}
