use std::fs;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};

use aft::commands::lsp_prepare_rename::handle_lsp_prepare_rename;
use aft::commands::lsp_rename::handle_lsp_rename;
use aft::context::AppContext;
use aft::lsp::client::LspEvent;
use aft::lsp::registry::ServerKind;
use aft::parser::TreeSitterProvider;
use aft::{config::Config, protocol::RawRequest};
use tempfile::tempdir;

fn fake_server_path() -> PathBuf {
    option_env!("CARGO_BIN_EXE_fake-lsp-server")
        .or(option_env!("CARGO_BIN_EXE_fake_lsp_server"))
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("CARGO_BIN_EXE_fake-lsp-server").map(PathBuf::from))
        .or_else(|| std::env::var_os("CARGO_BIN_EXE_fake_lsp_server").map(PathBuf::from))
        .or_else(|| {
            let mut path = std::env::current_exe().ok()?;
            path.pop();
            path.pop();
            path.push("fake-lsp-server");
            Some(path)
        })
        .filter(|path| path.exists())
        .expect("fake-lsp-server binary path not set")
}

fn rust_workspace_with_file() -> (tempfile::TempDir, PathBuf) {
    let temp_dir = tempdir().expect("tempdir");
    let root = temp_dir.path().join("workspace");
    let src_dir = root.join("src");

    fs::create_dir_all(&src_dir).expect("create src dir");
    fs::write(root.join("Cargo.toml"), "[package]\nname = \"demo\"\n").expect("write Cargo.toml");

    let main_rs = src_dir.join("main.rs");
    fs::write(&main_rs, "let hello = 1;\nprintln!(\"hello\");\nhello\n").expect("write main.rs");

    (temp_dir, main_rs)
}

fn app_context_with_fake_lsp() -> AppContext {
    let ctx = AppContext::new(Box::new(TreeSitterProvider::new()), Config::default());
    ctx.lsp()
        .override_binary(ServerKind::Rust, fake_server_path());
    ctx
}

fn wait_for_server(ctx: &AppContext) {
    thread::sleep(Duration::from_millis(200));
    ctx.lsp().drain_events();
}

fn collect_event<F>(ctx: &AppContext, predicate: F) -> Option<LspEvent>
where
    F: Fn(&LspEvent) -> bool,
{
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline {
        for event in ctx.lsp().drain_events() {
            if predicate(&event) {
                return Some(event);
            }
        }
        thread::sleep(Duration::from_millis(25));
    }
    None
}

fn custom_event(ctx: &AppContext, method: &str) -> serde_json::Value {
    let maybe_event = collect_event(ctx, |event| {
        matches!(
            event,
            LspEvent::Notification {
                method: event_method,
                ..
            } if event_method == method
        )
    });

    match maybe_event {
        Some(LspEvent::Notification {
            method: event_method,
            params,
            ..
        }) if event_method == method => params.expect("custom notification params"),
        Some(other) => panic!("unexpected event: {other:?}"),
        None => panic!("timed out waiting for {method}"),
    }
}

fn file_uri(path: &Path) -> String {
    let canonical = fs::canonicalize(path).expect("canonical path");
    let url = url::Url::from_file_path(&canonical).expect("file url");
    url.to_string()
}

#[test]
fn test_prepare_rename_success() {
    let (_temp_dir, main_rs) = rust_workspace_with_file();
    let ctx = app_context_with_fake_lsp();

    let req: RawRequest = serde_json::from_value(serde_json::json!({
        "id": "prepare-1",
        "command": "lsp_prepare_rename",
        "file": main_rs.display().to_string(),
        "line": 1,
        "character": 5,
    }))
    .expect("request parses");

    let response = handle_lsp_prepare_rename(&req, &ctx);
    wait_for_server(&ctx);
    let json = serde_json::to_value(&response).expect("response serializes");

    assert_eq!(json["success"], true, "expected success: {json:#}");
    assert_eq!(json["can_rename"], true);
    assert_eq!(json["range"]["start_line"], 1);
    assert_eq!(json["range"]["start_column"], 5);
    assert_eq!(json["range"]["end_line"], 1);
    assert_eq!(json["range"]["end_column"], 10);
    assert_eq!(json["placeholder"], "hello");
}

#[test]
fn test_rename_applies_changes() {
    let (_temp_dir, main_rs) = rust_workspace_with_file();
    let ctx = app_context_with_fake_lsp();

    let req: RawRequest = serde_json::from_value(serde_json::json!({
        "id": "rename-1",
        "command": "lsp_rename",
        "file": main_rs.display().to_string(),
        "line": 1,
        "character": 5,
        "new_name": "greet",
    }))
    .expect("request parses");

    let response = handle_lsp_rename(&req, &ctx);
    wait_for_server(&ctx);
    let json = serde_json::to_value(&response).expect("response serializes");

    assert_eq!(json["success"], true, "expected success: {json:#}");
    assert_eq!(json["renamed"], true);
    assert_eq!(json["total_files"], 1);
    assert_eq!(json["total_edits"], 2);
    assert_eq!(json["changes"][0]["edits"], 2);
    let content = fs::read_to_string(&main_rs).expect("read renamed file");
    assert!(content.contains("let greet = 1;"), "content was: {content}");
    assert!(
        content.contains("println!(\"hello\");"),
        "content was: {content}"
    );
    assert!(content.contains("\ngreet\n"), "content was: {content}");
    assert!(content.contains("\ngreet\n"), "content was: {content}");
}

#[test]
fn test_rename_rollback_on_failure() {
    let (_temp_dir, main_rs) = rust_workspace_with_file();
    let ctx = app_context_with_fake_lsp();
    let original = fs::read_to_string(&main_rs).expect("read original file");

    let req: RawRequest = serde_json::from_value(serde_json::json!({
        "id": "rename-rollback",
        "command": "lsp_rename",
        "file": main_rs.display().to_string(),
        "line": 1,
        "character": 5,
        "new_name": "__force_failure__",
    }))
    .expect("request parses");

    let response = handle_lsp_rename(&req, &ctx);
    wait_for_server(&ctx);
    let json = serde_json::to_value(&response).expect("response serializes");

    assert_eq!(json["success"], false, "expected failure: {json:#}");
    let current = fs::read_to_string(&main_rs).expect("read rolled back file");
    assert_eq!(current, original);

    let history = ctx
        .backup()
        .borrow()
        .history(aft::protocol::DEFAULT_SESSION_ID, &main_rs);
    assert!(
        history.is_empty(),
        "rollback should pop consumed backup entries"
    );
}

#[test]
fn test_rename_notifies_lsp() {
    let (_temp_dir, main_rs) = rust_workspace_with_file();
    let ctx = app_context_with_fake_lsp();
    let expected_uri = file_uri(&main_rs);

    let req: RawRequest = serde_json::from_value(serde_json::json!({
        "id": "rename-notify",
        "command": "lsp_rename",
        "file": main_rs.display().to_string(),
        "line": 1,
        "character": 5,
        "new_name": "greet",
    }))
    .expect("request parses");

    let response = handle_lsp_rename(&req, &ctx);
    let json = serde_json::to_value(&response).expect("response serializes");
    assert_eq!(json["success"], true, "expected success: {json:#}");

    let changed = custom_event(&ctx, "custom/documentChanged");
    assert_eq!(changed["uri"], expected_uri);
    assert_eq!(changed["version"], 1);
}
