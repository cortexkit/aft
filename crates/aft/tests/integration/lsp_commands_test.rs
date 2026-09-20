use std::fs;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use aft::commands::lsp_find_references::handle_lsp_find_references;
use aft::commands::lsp_goto_definition::handle_lsp_goto_definition;
use aft::commands::lsp_hover::handle_lsp_hover;
use aft::commands::lsp_navigation::handle_lsp_navigation_deferred;
use aft::config::Config;
use aft::context::AppContext;
use aft::lsp::registry::ServerKind;
use aft::parser::TreeSitterProvider;
use aft::protocol::RawRequest;
use aft::response_finalize::DispatchOutcome;
use tempfile::tempdir;

use super::helpers::{warm_executable, AftProcess};

fn fake_server_path() -> PathBuf {
    std::env::var_os("NEXTEST_BIN_EXE_fake_lsp_server")
        .or_else(|| std::env::var_os("NEXTEST_BIN_EXE_fake-lsp-server"))
        .map(PathBuf::from)
        .or_else(|| {
            option_env!("CARGO_BIN_EXE_fake-lsp-server")
                .or(option_env!("CARGO_BIN_EXE_fake_lsp_server"))
                .map(PathBuf::from)
        })
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
    fs::write(&main_rs, "fn main() {\n    println!(\"hello\");\n}\n").expect("write main.rs");

    (temp_dir, main_rs)
}

fn app_context_with_fake_lsp() -> AppContext {
    let ctx = AppContext::new(Box::new(TreeSitterProvider::new()), Config::default());
    ctx.lsp()
        .override_binary(ServerKind::Rust, fake_server_path());
    ctx
}

#[test]
fn test_lsp_hover_returns_content() {
    let (_temp_dir, main_rs) = rust_workspace_with_file();
    let ctx = app_context_with_fake_lsp();

    let req: RawRequest = serde_json::from_value(serde_json::json!({
        "id": "hover-1",
        "command": "lsp_hover",
        "file": main_rs.display().to_string(),
        "line": 1,
        "character": 1,
    }))
    .expect("request parses");

    let response = handle_lsp_hover(&req, &ctx);
    let json = serde_json::to_value(&response).expect("response serializes");

    assert_eq!(json["success"], true, "expected success: {json:#}");
    let contents = json["contents"].as_str().expect("contents string");
    assert!(
        contents.contains("const x: number"),
        "hover should contain fake server markdown: {contents}"
    );
    assert_eq!(json["language"], "typescript");
    assert_eq!(json["range"]["start_line"], 1);
    assert_eq!(json["range"]["start_column"], 1);
    assert_eq!(json["range"]["end_line"], 1);
    assert_eq!(json["range"]["end_column"], 8);
}

#[test]
fn test_lsp_hover_no_info() {
    let (_temp_dir, main_rs) = rust_workspace_with_file();
    let ctx = app_context_with_fake_lsp();

    let req: RawRequest = serde_json::from_value(serde_json::json!({
        "id": "hover-2",
        "command": "lsp_hover",
        "file": main_rs.display().to_string(),
        "line": 6,
        "character": 1,
    }))
    .expect("request parses");

    let response = handle_lsp_hover(&req, &ctx);
    let json = serde_json::to_value(&response).expect("response serializes");

    assert_eq!(json["success"], true, "expected success: {json:#}");
    assert!(
        json["contents"].is_null(),
        "expected null contents: {json:#}"
    );
}

#[test]
fn test_lsp_goto_definition_single() {
    let (_temp_dir, main_rs) = rust_workspace_with_file();
    let ctx = app_context_with_fake_lsp();

    let req: RawRequest = serde_json::from_value(serde_json::json!({
        "id": "def-1",
        "command": "lsp_goto_definition",
        "file": main_rs.display().to_string(),
        "line": 1,
        "character": 4,
    }))
    .expect("request parses");

    let response = handle_lsp_goto_definition(&req, &ctx);
    let json = serde_json::to_value(&response).expect("response serializes");

    assert_eq!(json["success"], true, "expected success: {json:#}");
    let definitions = json["definitions"].as_array().expect("definitions array");
    assert_eq!(definitions.len(), 1, "expected 1 definition: {json:#}");

    let definition = &definitions[0];
    assert_eq!(definition["line"], 1);
    assert_eq!(definition["column"], 1);
    assert_eq!(definition["end_line"], 1);
    assert_eq!(definition["end_column"], 11);
    assert!(
        definition["file"].is_string(),
        "expected file path: {definition:#}"
    );
}

#[test]
fn test_lsp_find_references_multiple() {
    let (_temp_dir, main_rs) = rust_workspace_with_file();
    let ctx = app_context_with_fake_lsp();

    let req: RawRequest = serde_json::from_value(serde_json::json!({
        "id": "ref-1",
        "command": "lsp_find_references",
        "file": main_rs.display().to_string(),
        "line": 1,
        "character": 4,
        "include_declaration": true,
    }))
    .expect("request parses");

    let response = handle_lsp_find_references(&req, &ctx);
    let json = serde_json::to_value(&response).expect("response serializes");

    assert_eq!(json["success"], true, "expected success: {json:#}");
    let references = json["references"].as_array().expect("references array");
    assert_eq!(
        references.len(),
        2,
        "expected 2 refs with declaration: {json:#}"
    );
    assert_eq!(json["total"], 2);

    assert_eq!(references[0]["line"], 1);
    assert_eq!(references[0]["column"], 1);
    assert_eq!(references[0]["end_line"], 1);
    assert_eq!(references[0]["end_column"], 6);

    assert_eq!(references[1]["line"], 3);
    assert_eq!(references[1]["column"], 1);
    assert_eq!(references[1]["end_line"], 3);
    assert_eq!(references[1]["end_column"], 6);
}

#[test]
fn test_lsp_find_references_with_declaration() {
    let (_temp_dir, main_rs) = rust_workspace_with_file();
    let ctx = app_context_with_fake_lsp();

    let req: RawRequest = serde_json::from_value(serde_json::json!({
        "id": "ref-2",
        "command": "lsp_find_references",
        "file": main_rs.display().to_string(),
        "line": 1,
        "character": 4,
        "include_declaration": false,
    }))
    .expect("request parses");

    let response = handle_lsp_find_references(&req, &ctx);
    let json = serde_json::to_value(&response).expect("response serializes");

    assert_eq!(json["success"], true, "expected success: {json:#}");
    let references = json["references"].as_array().expect("references array");
    assert_eq!(
        references.len(),
        1,
        "expected 1 ref without declaration: {json:#}"
    );
    assert_eq!(json["total"], 1);

    assert_eq!(references[0]["line"], 3);
    assert_eq!(references[0]["column"], 1);
    assert_eq!(references[0]["end_line"], 3);
    assert_eq!(references[0]["end_column"], 6);
}

#[test]
fn test_lsp_find_references_defaults_include_declaration_true() {
    let (_temp_dir, main_rs) = rust_workspace_with_file();
    let ctx = app_context_with_fake_lsp();

    let req: RawRequest = serde_json::from_value(serde_json::json!({
        "id": "ref-3",
        "command": "lsp_find_references",
        "file": main_rs.display().to_string(),
        "line": 1,
        "character": 4,
    }))
    .expect("request parses");

    let response = handle_lsp_find_references(&req, &ctx);
    let json = serde_json::to_value(&response).expect("response serializes");

    assert_eq!(json["success"], true, "expected success: {json:#}");
    let references = json["references"].as_array().expect("references array");
    assert_eq!(
        references.len(),
        2,
        "default should include declaration: {json:#}"
    );
    assert_eq!(json["total"], 2);
}

#[test]
fn warm_navigation_stays_synchronous_and_preserves_response_bytes() {
    let (_temp_dir, main_rs) = rust_workspace_with_file();
    let ctx = Arc::new(app_context_with_fake_lsp());
    let request: RawRequest = serde_json::from_value(serde_json::json!({
        "id": "warm-hover",
        "command": "lsp_hover",
        "file": main_rs.display().to_string(),
        "line": 1,
        "character": 1,
    }))
    .expect("request parses");

    let initial = handle_lsp_hover(&request, &ctx);
    assert!(initial.success, "fixture server should warm successfully");
    let expected = handle_lsp_hover(&request, &ctx);
    let actual = match handle_lsp_navigation_deferred(&request, Arc::clone(&ctx)) {
        DispatchOutcome::Immediate(response) => response,
        DispatchOutcome::Deferred(_) => panic!("warm navigation must remain synchronous"),
    };

    assert_eq!(
        serde_json::to_vec(&actual).expect("serialize actual response"),
        serde_json::to_vec(&expected).expect("serialize expected response")
    );
}

#[test]
fn standalone_ndjson_polls_cold_navigation_off_the_input_loop() {
    let (temp_dir, main_rs) = rust_workspace_with_file();
    let bin_dir = temp_dir.path().join("bin");
    fs::create_dir_all(&bin_dir).expect("create fake bin dir");
    let installed = bin_dir.join(if cfg!(windows) {
        "rust-analyzer.exe"
    } else {
        "rust-analyzer"
    });
    fs::copy(fake_server_path(), &installed).expect("install fake rust analyzer");
    warm_executable(&installed, &[]);

    let mut paths = vec![bin_dir];
    paths.extend(std::env::split_paths(
        &std::env::var_os("PATH").unwrap_or_default(),
    ));
    let joined_path = std::env::join_paths(paths).expect("join fixture PATH");
    let mut aft = AftProcess::spawn_with_env(&[
        ("PATH", joined_path.as_os_str()),
        ("AFT_FAKE_LSP_INIT_DELAY_MS", std::ffi::OsStr::new("1500")),
    ]);
    let ready = aft.send(r#"{"id":"input-loop-ready","command":"ping"}"#);
    assert_eq!(ready["id"], "input-loop-ready");

    aft.send_silent(
        &serde_json::json!({
            "id": "cold-hover",
            "command": "lsp_hover",
            "file": main_rs.display().to_string(),
            "line": 1,
            "character": 1,
        })
        .to_string(),
    );
    aft.send_silent(r#"{"id":"input-loop-probe","command":"ping"}"#);

    // The proof is the ORDER of the two replies, asserted below: a serialized
    // input loop would answer the hover (after the fake server's 1.5 s init)
    // before the ping. The read timeout is liveness only, so it stays well above
    // the init delay; a contended CI runner failed a 1 s bound twice in a row
    // while the same test ran green in under 4 s on an idle machine.
    let first = aft
        .try_read_next_timeout(Duration::from_secs(30))
        .expect("input loop should answer while the cold server initializes");
    assert_eq!(
        first["id"], "input-loop-probe",
        "cold navigation must not serialize the next NDJSON request: {first:#}"
    );

    let navigation = aft.read_next();
    assert_eq!(navigation["id"], "cold-hover", "response: {navigation:#}");
    assert_eq!(navigation["success"], true, "response: {navigation:#}");
    assert!(aft.shutdown().success());
}
