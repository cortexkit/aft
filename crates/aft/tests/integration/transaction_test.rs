//! Integration tests for the `transaction` command: multi-file atomicity and rollback.

use std::fs;
use std::path::PathBuf;
use std::thread;
use std::time::{Duration, Instant};

use aft::commands::transaction::handle_transaction;
use aft::config::Config;
use aft::context::AppContext;
use aft::lsp::client::LspEvent;
use aft::lsp::registry::ServerKind;
use aft::parser::TreeSitterProvider;
use aft::protocol::RawRequest;

use super::helpers::AftProcess;

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
            if path.exists() {
                Some(path)
            } else {
                None
            }
        })
        .expect("fake-lsp-server binary path")
}

fn collect_watched_file_events_from_ctx_before_deadline(
    ctx: &AppContext,
    duration: Duration,
) -> Option<serde_json::Value> {
    let deadline = Instant::now() + duration;
    while Instant::now() < deadline {
        for event in ctx.lsp().drain_events() {
            if let LspEvent::Notification { method, params, .. } = event {
                if method == "custom/watchedFilesChanged" {
                    return params;
                }
            }
        }
        thread::sleep(Duration::from_millis(25));
    }
    None
}

fn wait_for_publish(ctx: &AppContext) {
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline {
        for event in ctx.lsp().drain_events() {
            if matches!(
                event,
                LspEvent::Notification { method, .. } if method == "textDocument/publishDiagnostics"
            ) {
                return;
            }
        }
        thread::sleep(Duration::from_millis(25));
    }
    panic!("timed out waiting for publishDiagnostics");
}

// ============================================================================
// transaction_success_three_files
// ============================================================================

#[test]
fn transaction_success_three_files() {
    let mut aft = AftProcess::spawn();
    let dir = tempfile::tempdir().unwrap();

    let f1 = dir.path().join("txn_ok_1.ts");
    let f2 = dir.path().join("txn_ok_2.ts");
    let f3 = dir.path().join("txn_ok_3.ts");

    fs::write(&f1, "const a = 1;\n").unwrap();
    fs::write(&f2, "const b = 2;\n").unwrap();
    fs::write(&f3, "const c = 3;\n").unwrap();

    let resp = aft.send(&format!(
        r#"{{"id":"txn-1","command":"transaction","operations":[{{"file":"{}","command":"write","content":"const a = 10;\n"}},{{"file":"{}","command":"write","content":"const b = 20;\n"}},{{"file":"{}","command":"write","content":"const c = 30;\n"}}]}}"#,
        f1.display(), f2.display(), f3.display()
    ));

    assert_eq!(
        resp["success"], true,
        "transaction should succeed: {:?}",
        resp
    );
    assert_eq!(resp["files_modified"], 3);

    let results = resp["results"].as_array().expect("results array");
    assert_eq!(results.len(), 3);

    // All three files modified on disk
    assert_eq!(fs::read_to_string(&f1).unwrap(), "const a = 10;\n");
    assert_eq!(fs::read_to_string(&f2).unwrap(), "const b = 20;\n");
    assert_eq!(fs::read_to_string(&f3).unwrap(), "const c = 30;\n");

    // Cleanup
    let _ = fs::remove_file(&f1);
    let _ = fs::remove_file(&f2);
    let _ = fs::remove_file(&f3);
    let status = aft.shutdown();
    assert!(status.success());
}

#[test]
fn transaction_success_batches_config_file_watched_notifications() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let source = root.join("src").join("open.ts");
    let tsconfig = root.join("tsconfig.json");
    let package_json = root.join("package.json");
    let leaf = root.join("src").join("leaf.ts");
    fs::create_dir_all(source.parent().unwrap()).unwrap();
    fs::write(&source, "export const open = 1;\n").unwrap();
    fs::write(&tsconfig, "{\"compilerOptions\":{}}\n").unwrap();
    fs::write(&package_json, "{\"devDependencies\":{}}\n").unwrap();
    fs::write(&leaf, "export const leaf = 1;\n").unwrap();

    let config = Config {
        project_root: Some(root.to_path_buf()),
        ..Config::default()
    };
    let ctx = AppContext::new(Box::new(TreeSitterProvider::new()), config);
    ctx.lsp()
        .override_binary(ServerKind::TypeScript, fake_server_path());
    ctx.lsp_notify_file_changed(&source, "export const open = 2;\n");
    wait_for_publish(&ctx);

    let req: RawRequest = serde_json::from_value(serde_json::json!({
        "id": "txn-batched-configs",
        "command": "transaction",
        "operations": [
            {"file": tsconfig.display().to_string(), "command": "write", "content": "{\"compilerOptions\":{\"strict\":true}}\n"},
            {"file": package_json.display().to_string(), "command": "write", "content": "{\"devDependencies\":{},\"scripts\":{}}\n"},
            {"file": leaf.display().to_string(), "command": "write", "content": "export const leaf = 2;\n"}
        ]
    }))
    .expect("request parses");
    let response = handle_transaction(&req, &ctx);
    let json = serde_json::to_value(&response).expect("response serializes");
    assert_eq!(json["success"], true, "transaction failed: {json}");

    let params = collect_watched_file_events_from_ctx_before_deadline(&ctx, Duration::from_secs(2))
        .expect("watched-file notification");
    let changes = params["changes"].as_array().expect("changes array");
    assert_eq!(
        changes.len(),
        2,
        "expected one batched notification: {params}"
    );
    assert!(changes.iter().any(|change| change["uri"]
        .as_str()
        .is_some_and(|uri| uri.ends_with("/tsconfig.json"))));
    assert!(changes.iter().any(|change| change["uri"]
        .as_str()
        .is_some_and(|uri| uri.ends_with("/package.json"))));
    assert!(
        collect_watched_file_events_from_ctx_before_deadline(&ctx, Duration::from_millis(250))
            .is_none(),
        "expected no second watched-file notification"
    );
}

#[test]
fn transaction_rollback_restores_first_config_and_sends_no_lsp_notification() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let source = root.join("src").join("open.ts");
    let package_json = root.join("package.json");
    let outside = dir.path().join("outside").join("tsconfig.json");
    fs::create_dir_all(source.parent().unwrap()).unwrap();
    fs::write(&source, "export const open = 1;\n").unwrap();
    let original_package = "{\"devDependencies\":{}}\n";
    fs::write(&package_json, original_package).unwrap();

    let config = Config {
        project_root: Some(root.to_path_buf()),
        restrict_to_project_root: true,
        ..Config::default()
    };
    let ctx = AppContext::new(Box::new(TreeSitterProvider::new()), config);
    ctx.lsp()
        .override_binary(ServerKind::TypeScript, fake_server_path());
    ctx.lsp_notify_file_changed(&source, "export const open = 2;\n");
    wait_for_publish(&ctx);

    let req: RawRequest = serde_json::from_value(serde_json::json!({
        "id": "txn-rollback-no-lsp",
        "command": "transaction",
        "operations": [
            {"file": package_json.display().to_string(), "command": "write", "content": "{\"devDependencies\":{},\"scripts\":{}}\n"},
            {"file": outside.display().to_string(), "command": "write", "content": "{\"compilerOptions\":{}}\n"}
        ]
    }))
    .expect("request parses");
    let response = handle_transaction(&req, &ctx);
    let json = serde_json::to_value(&response).expect("response serializes");
    assert_eq!(json["success"], false, "transaction should fail: {json}");
    assert_eq!(json["code"], "transaction_failed");
    assert!(json["message"]
        .as_str()
        .is_some_and(|message| message.contains("No such file")));
    assert_eq!(fs::read_to_string(&package_json).unwrap(), original_package);
    assert!(
        collect_watched_file_events_from_ctx_before_deadline(&ctx, Duration::from_millis(250))
            .is_none(),
        "rollback path must not notify LSP"
    );
}

// ============================================================================
// transaction_rollback_syntax_error (milestone acceptance scenario)
// ============================================================================

#[test]
fn transaction_rollback_syntax_error() {
    let mut aft = AftProcess::spawn();
    let dir = tempfile::tempdir().unwrap();

    let f1 = dir.path().join("txn_rb_1.ts");
    let f2 = dir.path().join("txn_rb_2.ts");
    let f3 = dir.path().join("txn_rb_3.ts");

    let orig1 = "const a = 1;\n";
    let orig2 = "const b = 2;\n";
    let orig3 = "const c = 3;\n";

    fs::write(&f1, orig1).unwrap();
    fs::write(&f2, orig2).unwrap();
    fs::write(&f3, orig3).unwrap();

    // Third file gets intentionally broken syntax
    let resp = aft.send(&format!(
        r#"{{"id":"txn-rb","command":"transaction","operations":[{{"file":"{}","command":"write","content":"const a = 10;\n"}},{{"file":"{}","command":"write","content":"const b = 20;\n"}},{{"file":"{}","command":"write","content":"const c = {{{{;\n"}}]}}"#,
        f1.display(), f2.display(), f3.display()
    ));

    assert_eq!(
        resp["success"], false,
        "transaction should fail: {:?}",
        resp
    );
    assert_eq!(resp["code"], "transaction_failed");
    assert_eq!(
        resp["failed_operation"], 2,
        "third op (index 2) should fail"
    );

    // rolled_back array should list all 3 files
    let rolled_back = resp["rolled_back"].as_array().expect("rolled_back array");
    assert_eq!(
        rolled_back.len(),
        3,
        "all 3 files should be rolled back: {:?}",
        rolled_back
    );

    // All three files should be restored to original content
    assert_eq!(
        fs::read_to_string(&f1).unwrap(),
        orig1,
        "f1 should be restored"
    );
    assert_eq!(
        fs::read_to_string(&f2).unwrap(),
        orig2,
        "f2 should be restored"
    );
    assert_eq!(
        fs::read_to_string(&f3).unwrap(),
        orig3,
        "f3 should be restored"
    );

    // Cleanup
    let _ = fs::remove_file(&f1);
    let _ = fs::remove_file(&f2);
    let _ = fs::remove_file(&f3);
    let status = aft.shutdown();
    assert!(status.success());
}

// ============================================================================
// transaction_rollback_new_file
// ============================================================================

#[test]
fn transaction_rollback_new_file() {
    let mut aft = AftProcess::spawn();
    let dir = tempfile::tempdir().unwrap();

    let existing = dir.path().join("txn_new_existing.ts");
    let new_file = dir.path().join("txn_new_created.ts");
    let bad_file = dir.path().join("txn_new_bad.ts");

    let orig = "const x = 1;\n";
    fs::write(&existing, orig).unwrap();
    // new_file does not exist yet
    let _ = fs::remove_file(&new_file);
    let _ = fs::remove_file(&bad_file);

    // Op 0: modify existing, Op 1: create new file, Op 2: broken syntax triggers rollback
    let resp = aft.send(&format!(
        r#"{{"id":"txn-nf","command":"transaction","operations":[{{"file":"{}","command":"write","content":"const x = 10;\n"}},{{"file":"{}","command":"write","content":"const y = 1;\n"}},{{"file":"{}","command":"write","content":"const z = {{{{;\n"}}]}}"#,
        existing.display(), new_file.display(), bad_file.display()
    ));

    assert_eq!(
        resp["success"], false,
        "transaction should fail: {:?}",
        resp
    );
    assert_eq!(resp["code"], "transaction_failed");

    // Existing file restored
    assert_eq!(
        fs::read_to_string(&existing).unwrap(),
        orig,
        "existing file should be restored"
    );

    // New file should be deleted on rollback
    assert!(!new_file.exists(), "new file should be deleted on rollback");

    // Bad file should also be deleted (it was new)
    assert!(!bad_file.exists(), "bad file should be deleted on rollback");

    // Cleanup
    let _ = fs::remove_file(&existing);
    let status = aft.shutdown();
    assert!(status.success());
}

// ============================================================================
// transaction_edit_match_operation
// ============================================================================

#[test]
fn transaction_edit_match_operation() {
    let mut aft = AftProcess::spawn();
    let dir = tempfile::tempdir().unwrap();

    let f1 = dir.path().join("txn_em_1.ts");
    let f2 = dir.path().join("txn_em_2.ts");

    fs::write(&f1, "const greeting = \"hello\";\n").unwrap();
    fs::write(&f2, "const name = \"world\";\n").unwrap();

    let resp = aft.send(&format!(
        r#"{{"id":"txn-em","command":"transaction","operations":[{{"file":"{}","command":"edit_match","match":"hello","replacement":"hi"}},{{"file":"{}","command":"edit_match","match":"world","replacement":"rust"}}]}}"#,
        f1.display(), f2.display()
    ));

    assert_eq!(
        resp["success"], true,
        "transaction should succeed: {:?}",
        resp
    );
    assert_eq!(resp["files_modified"], 2);

    assert_eq!(
        fs::read_to_string(&f1).unwrap(),
        "const greeting = \"hi\";\n"
    );
    assert_eq!(fs::read_to_string(&f2).unwrap(), "const name = \"rust\";\n");

    // Cleanup
    let _ = fs::remove_file(&f1);
    let _ = fs::remove_file(&f2);
    let status = aft.shutdown();
    assert!(status.success());
}

#[test]
fn transaction_edit_match_requires_replacement() {
    let mut aft = AftProcess::spawn();
    let dir = tempfile::tempdir().unwrap();

    let f1 = dir.path().join("txn_missing_replacement.ts");
    fs::write(&f1, "const greeting = \"hello\";\n").unwrap();

    let resp = aft.send(&format!(
        r#"{{"id":"txn-em-missing","command":"transaction","operations":[{{"file":"{}","command":"edit_match","match":"hello"}}]}}"#,
        f1.display()
    ));

    assert_eq!(
        resp["success"], false,
        "transaction should fail: {:?}",
        resp
    );
    assert_eq!(resp["code"], "invalid_request");
    assert_eq!(
        resp["message"],
        "transaction: edit_match operation requires 'replacement' field"
    );

    let _ = fs::remove_file(&f1);
    let status = aft.shutdown();
    assert!(status.success());
}

#[test]
fn transaction_edit_match_uses_fuzzy_matching() {
    let mut aft = AftProcess::spawn();
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("txn_fuzzy.txt");

    fs::write(&target, "    hello world   \n").unwrap();

    let req = serde_json::json!({
        "id": "txn-fuzzy",
        "command": "transaction",
        "operations": [
            {
                "file": target.display().to_string(),
                "command": "edit_match",
                "match": "hello world\n",
                "replacement": "hello rust\n"
            }
        ]
    });
    let resp = aft.send(&serde_json::to_string(&req).unwrap());

    assert_eq!(
        resp["success"], true,
        "transaction should succeed: {:?}",
        resp
    );
    assert_eq!(resp["files_modified"], 1);
    assert_eq!(fs::read_to_string(&target).unwrap(), "hello rust\n");

    let status = aft.shutdown();
    assert!(status.success());
}

#[test]
fn transaction_edit_match_apply_rejects_ambiguous_fuzzy_match() {
    let mut aft = AftProcess::spawn();
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("txn_ambiguous.txt");
    let original = "    duplicate target   \n\n\tduplicate target\t\n";

    fs::write(&target, original).unwrap();

    let req = serde_json::json!({
        "id": "txn-ambiguous",
        "command": "transaction",
        "operations": [
            {
                "file": target.display().to_string(),
                "command": "edit_match",
                "match": "duplicate target\n",
                "replacement": "replacement\n"
            }
        ]
    });
    let resp = aft.send(&serde_json::to_string(&req).unwrap());

    assert_eq!(resp["success"], false, "transaction should fail: {resp:?}");
    assert_eq!(resp["code"], "ambiguous_match");
    assert_eq!(resp["failed_operation"], 0);
    assert_eq!(
        fs::read_to_string(&target).unwrap(),
        original,
        "ambiguous apply must leave file unchanged"
    );

    let status = aft.shutdown();
    assert!(status.success());
}

#[test]
fn transaction_returns_inline_lsp_diagnostics_when_requested() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let file = root.join("main.rs");
    let cargo_toml = root.join("Cargo.toml");
    fs::write(
        &cargo_toml,
        "[package]\nname = \"txn-inline-diag\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )
    .unwrap();
    fs::write(
        &file,
        "fn main() { let value = 1; println!(\"{}\", value); }\n",
    )
    .unwrap();

    let fake_server = fake_server_path();
    let mut aft = AftProcess::spawn_with_env(&[("AFT_LSP_RUST_BINARY", fake_server.as_os_str())]);

    let configure = aft.send(&format!(
        r#"{{"id":"cfg-txn-inline","command":"configure","harness":"opencode","project_root":"{}"}}"#,
        root.display()
    ));
    assert_eq!(
        configure["success"], true,
        "configure failed: {configure:?}"
    );

    let req = serde_json::json!({
        "id": "txn-inline-diag",
        "command": "transaction",
        "operations": [
            {
                "file": file.display().to_string(),
                "command": "write",
                "content": "fn main() { let answer = 1; println!(\"{}\", answer); }\n"
            }
        ],
        "diagnostics": true
    });
    let resp = aft.send(&serde_json::to_string(&req).unwrap());

    assert_eq!(
        resp["success"], true,
        "transaction should succeed: {resp:?}"
    );
    let diagnostics = resp["lsp_diagnostics"]
        .as_array()
        .expect("lsp_diagnostics array");
    assert_eq!(
        diagnostics.len(),
        2,
        "expected inline diagnostics from fake LSP: {resp:?}"
    );

    let canonical_file = fs::canonicalize(&file).expect("canonical file");
    assert_eq!(diagnostics[0]["file"], canonical_file.display().to_string());
    assert_eq!(diagnostics[0]["severity"], "error");
    assert_eq!(diagnostics[0]["message"], "test diagnostic error");
    assert_eq!(diagnostics[1]["severity"], "warning");

    let status = aft.shutdown();
    assert!(status.success());
}

// ============================================================================
// transaction_empty_operations
// ============================================================================

#[test]
fn transaction_empty_operations() {
    let mut aft = AftProcess::spawn();

    let resp = aft.send(r#"{"id":"txn-empty","command":"transaction","operations":[]}"#);

    assert_eq!(resp["success"], false, "empty ops should fail: {:?}", resp);
    assert_eq!(resp["code"], "invalid_request");
    assert!(
        resp["message"]
            .as_str()
            .unwrap()
            .contains("must not be empty"),
        "message should mention empty: {:?}",
        resp
    );

    let status = aft.shutdown();
    assert!(status.success());
}
