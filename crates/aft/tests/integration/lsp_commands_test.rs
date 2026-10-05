use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use aft::commands::lsp_find_references::handle_lsp_find_references;
use aft::commands::lsp_goto_definition::handle_lsp_goto_definition;
use aft::commands::lsp_hover::handle_lsp_hover;
use aft::commands::lsp_navigation::handle_lsp_navigation_deferred;
use aft::config::Config;
use aft::context::AppContext;
use aft::lsp::diagnostics::DiagnosticsStore;
use aft::lsp::position::{uri_for_path, uri_to_path};
use aft::lsp::registry::ServerKind;
use aft::lsp::roots::ServerKey;
use aft::parser::TreeSitterProvider;
use aft::protocol::RawRequest;
use aft::response_finalize::DispatchOutcome;
use tempfile::tempdir;

use super::helpers::{warm_executable, AftProcess};

fn fake_server_path() -> PathBuf {
    crate::test_helpers::fake_lsp::fake_server_binary()
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
    let ctx = AppContext::new(
        Box::new(TreeSitterProvider::new()),
        crate::context_storage::isolate(Config::default()),
    );
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

/// Wait up to `timeout` for `path` to exist.
fn wait_for_file(path: &std::path::Path, timeout: Duration) -> bool {
    let deadline = std::time::Instant::now() + timeout;
    while std::time::Instant::now() < deadline {
        if path.exists() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    path.exists()
}

/// A hover waiting on a slow server must not hold the LSP manager lock: the
/// request loop takes that lock to drain events and render the status bar
/// for every other request, so a held lock made a sibling `read` wait for
/// the slow server.
#[test]
fn slow_hover_leaves_the_lsp_manager_free_while_the_server_works() {
    let (temp_dir, main_rs) = rust_workspace_with_file();
    let signal = temp_dir.path().join("hover-arrived");
    let ctx = Arc::new(app_context_with_fake_lsp());
    {
        let mut lsp = ctx.lsp();
        lsp.set_extra_env("AFT_FAKE_LSP_HOVER_DELAY_MS", "3000");
        lsp.set_extra_env(
            "AFT_FAKE_LSP_HOVER_DELAY_SIGNAL",
            signal.to_str().expect("utf-8 signal path"),
        );
    }
    let request: RawRequest = serde_json::from_value(serde_json::json!({
        "id": "slow-hover",
        "command": "lsp_hover",
        "file": main_rs.display().to_string(),
        "line": 1,
        "character": 1,
    }))
    .expect("request parses");

    let worker = {
        let ctx = Arc::clone(&ctx);
        std::thread::spawn(move || handle_lsp_hover(&request, &ctx))
    };
    assert!(
        wait_for_file(&signal, Duration::from_secs(30)),
        "the fake server never received the hover"
    );

    // The hover is now with the server for three seconds. The writer drops
    // the manager lock right after writing it, so allow a moment for that,
    // but far less than the server's delay.
    let probe_deadline = std::time::Instant::now() + Duration::from_millis(1000);
    let mut acquired = false;
    while std::time::Instant::now() < probe_deadline {
        if let Some(lsp) = ctx.try_lsp() {
            // A second manager user gets real work done meanwhile.
            assert_eq!(lsp.server_count(), 1);
            acquired = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(
        !worker.is_finished(),
        "the hover finished before the probe; the server delay did not apply"
    );
    assert!(
        acquired,
        "the LSP manager lock stayed held while the server worked on the hover"
    );

    let response = worker.join().expect("hover worker");
    let json = serde_json::to_value(&response).expect("response serializes");
    assert_eq!(json["success"], true, "expected success: {json:#}");
    assert!(
        json["contents"]
            .as_str()
            .is_some_and(|contents| contents.contains("const x: number")),
        "hover reply must be unchanged: {json:#}"
    );
}

/// Starting a server (spawn plus an `initialize` handshake of up to 30 s)
/// must not hold the LSP manager lock either. A second caller for the same
/// server waits for that one start and reuses its client instead of spawning
/// a twin.
#[test]
fn cold_server_start_leaves_the_lsp_manager_free_and_is_shared() {
    let (temp_dir, main_rs) = rust_workspace_with_file();
    let signal = temp_dir.path().join("initialize-arrived");
    let pid_dir = temp_dir.path().join("pids");
    fs::create_dir_all(&pid_dir).expect("create pid dir");
    let ctx = Arc::new(app_context_with_fake_lsp());
    {
        let mut lsp = ctx.lsp();
        lsp.set_extra_env("AFT_FAKE_LSP_INIT_DELAY_MS", "3000");
        lsp.set_extra_env(
            "AFT_FAKE_LSP_INIT_DELAY_SIGNAL",
            signal.to_str().expect("utf-8 signal path"),
        );
        lsp.set_extra_env(
            "AFT_FAKE_LSP_PID_DIR",
            pid_dir.to_str().expect("utf-8 pid dir"),
        );
    }
    let hover = |id: &str| -> RawRequest {
        serde_json::from_value(serde_json::json!({
            "id": id,
            "command": "lsp_hover",
            "file": main_rs.display().to_string(),
            "line": 1,
            "character": 1,
        }))
        .expect("request parses")
    };

    let first = {
        let ctx = Arc::clone(&ctx);
        let request = hover("cold-hover-1");
        std::thread::spawn(move || handle_lsp_hover(&request, &ctx))
    };
    assert!(
        wait_for_file(&signal, Duration::from_secs(30)),
        "the fake server never received initialize"
    );
    let second = {
        let ctx = Arc::clone(&ctx);
        let request = hover("cold-hover-2");
        std::thread::spawn(move || handle_lsp_hover(&request, &ctx))
    };

    let probe_deadline = std::time::Instant::now() + Duration::from_millis(1000);
    let mut acquired = false;
    while std::time::Instant::now() < probe_deadline {
        if let Some(lsp) = ctx.try_lsp() {
            // The server is still initializing: no client is published yet.
            assert_eq!(lsp.server_count(), 0);
            acquired = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(
        !first.is_finished(),
        "the start finished before the probe; the initialize delay did not apply"
    );
    assert!(
        acquired,
        "the LSP manager lock stayed held while the server initialized"
    );

    for worker in [first, second] {
        let json = serde_json::to_value(worker.join().expect("hover worker"))
            .expect("response serializes");
        assert_eq!(json["success"], true, "expected success: {json:#}");
    }
    assert_eq!(ctx.lsp().server_count(), 1);
    assert_eq!(
        fs::read_dir(&pid_dir).expect("read pid dir").count(),
        1,
        "a concurrent caller for a starting server must not spawn a second one"
    );
}

/// An edit's `didChange` that finds the LSP manager busy (here held by the
/// test, standing in for any long holder) is queued and reaches the server
/// once the lock frees, instead of being dropped and leaving the server on
/// the old contents.
#[test]
fn edit_notification_reaches_the_server_after_a_busy_manager_frees() {
    let (_temp_dir, main_rs) = rust_workspace_with_file();
    let ctx = app_context_with_fake_lsp();
    let canonical = fs::canonicalize(&main_rs).expect("canonical main.rs");
    let config = ctx.config();
    let opened = ctx
        .lsp()
        .ensure_file_open(&canonical, &config)
        .expect("open main.rs");
    assert_eq!(
        opened.server_keys.len(),
        1,
        "the fake server serves main.rs"
    );

    let new_content = "fn main() {\n    let changed = 1;\n}\n";
    fs::write(&main_rs, new_content).expect("rewrite main.rs");
    let held = ctx.lsp();
    ctx.lsp_notify_file_changed(&main_rs, new_content);
    assert!(
        ctx.lsp_pending_changes_for_test(),
        "a change that found the manager busy must be queued"
    );
    drop(held);

    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    let mut delivered = false;
    while std::time::Instant::now() < deadline {
        {
            let mut lsp = ctx.lsp();
            lsp.drain_events();
            delivered =
                published_document_version_reached(lsp.diagnostics_store_for_test(), &canonical, 1);
        }
        if delivered {
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(
        delivered,
        "the server never published for document version 1: the queued didChange was lost"
    );
    let drained_deadline = std::time::Instant::now() + Duration::from_secs(5);
    while ctx.lsp_pending_changes_for_test() && std::time::Instant::now() < drained_deadline {
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(!ctx.lsp_pending_changes_for_test(), "the backlog drained");
}

fn published_document_version_reached(
    diagnostics: &DiagnosticsStore,
    file: &Path,
    version: i32,
) -> bool {
    // The server publishes the path decoded from its file URI, not the raw
    // fs::canonicalize spelling (which has a verbatim prefix on Windows).
    // Round-trip the lookup through the same Windows-aware URI helpers; the
    // diagnostics store deliberately requires an exact key match.
    let uri = uri_for_path(file).expect("document URI");
    let published_path = uri_to_path(&uri).expect("published document path");
    diagnostics
        .entries_for_file(&published_path)
        .iter()
        .any(|(_, entry)| entry.version == Some(version))
}

#[test]
fn published_version_lookup_accepts_plain_and_verbatim_windows_paths() {
    let plain = Path::new(r"C:\aft-queued-change-path-test\src\main.rs");
    let verbatim = Path::new(r"\\?\C:\aft-queued-change-path-test\src\main.rs");
    let mut diagnostics = DiagnosticsStore::new();
    diagnostics.publish_full(
        ServerKey {
            kind: ServerKind::Rust,
            root: PathBuf::from(r"C:\aft-queued-change-path-test"),
        },
        plain.to_path_buf(),
        Vec::new(),
        None,
        Some(1),
    );

    // Literal Windows spellings exercise the lookup even on Unix hosts. The
    // store key is the non-verbatim path decoded from a server's file URI.
    for file in [plain, verbatim] {
        assert!(
            published_document_version_reached(&diagnostics, file, 1),
            "version 1 must be found for {}",
            file.display()
        );
        assert!(!published_document_version_reached(&diagnostics, file, 0));
    }
    assert!(!published_document_version_reached(
        &diagnostics,
        Path::new(r"C:\aft-queued-change-path-test\src\other.rs"),
        1,
    ));
}
