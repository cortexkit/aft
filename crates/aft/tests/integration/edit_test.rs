//! Integration tests for the write and edit_symbol commands.

use std::fs;
use std::path::PathBuf;

use aft::edit::replace_byte_range;

use super::helpers::{fixture_path, AftProcess};

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

#[cfg(unix)]
fn install_executable(dir: &std::path::Path, name: &str, script: &str) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;

    let bin_dir = dir.join("node_modules").join(".bin");
    fs::create_dir_all(&bin_dir).unwrap();
    let tool = bin_dir.join(name);
    fs::write(&tool, script).unwrap();
    let mut perms = fs::metadata(&tool).unwrap().permissions();
    perms.set_mode(0o755);
    fs::set_permissions(&tool, perms).unwrap();
    tool
}

#[cfg(unix)]
fn path_with_node_bin(dir: &std::path::Path) -> std::ffi::OsString {
    let mut paths =
        std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()).collect::<Vec<_>>();
    paths.insert(0, dir.join("node_modules").join(".bin"));
    std::env::join_paths(paths).unwrap()
}
// ============================================================================
// write command tests
// ============================================================================

#[test]
fn write_creates_new_file() {
    let mut aft = AftProcess::spawn();
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("write_new.txt");
    // Ensure file doesn't exist
    let _ = fs::remove_file(&target);

    let resp = aft.send(&format!(
        r#"{{"id":"w-1","command":"write","file":"{}","content":"hello world"}}"#,
        target.display()
    ));

    assert_eq!(resp["success"], true, "write should succeed: {:?}", resp);
    assert_eq!(resp["file"], target.display().to_string());
    assert_eq!(
        resp["created"], true,
        "file was new, created should be true"
    );
    // New files get a tombstone backup so undo can delete them.
    assert!(
        resp["backup_id"].is_string(),
        "new file should have tombstone backup_id"
    );

    // Verify content on disk
    let on_disk = fs::read_to_string(&target).unwrap();
    assert_eq!(on_disk, "hello world");

    // Cleanup
    let _ = fs::remove_file(&target);
    let status = aft.shutdown();
    assert!(status.success());
}

#[test]
fn write_backups_existing_file() {
    let mut aft = AftProcess::spawn();
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("write_backup.txt");
    fs::write(&target, "original content").unwrap();

    // Overwrite via write command
    let resp = aft.send(&format!(
        r#"{{"id":"w-2","command":"write","file":"{}","content":"new content"}}"#,
        target.display()
    ));

    assert_eq!(resp["success"], true, "write should succeed: {:?}", resp);
    assert_eq!(resp["created"], false);
    assert!(
        resp["backup_id"].is_string(),
        "should have backup_id for existing file"
    );

    // Verify new content on disk
    assert_eq!(fs::read_to_string(&target).unwrap(), "new content");

    // Undo — should restore original
    let undo_resp = aft.send(&format!(
        r#"{{"id":"w-2u","command":"undo","file":"{}"}}"#,
        target.display()
    ));
    assert_eq!(
        undo_resp["success"], true,
        "undo should succeed: {:?}",
        undo_resp
    );
    assert_eq!(fs::read_to_string(&target).unwrap(), "original content");

    // Cleanup
    let _ = fs::remove_file(&target);
    let status = aft.shutdown();
    assert!(status.success());
}

#[test]
fn write_syntax_valid() {
    let mut aft = AftProcess::spawn();
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("write_valid.ts");
    let _ = fs::remove_file(&target);

    let resp = aft.send(&format!(
        r#"{{"id":"w-3","command":"write","file":"{}","content":"export function hello(): string {{ return \"hi\"; }}"}}"#,
        target.display()
    ));

    assert_eq!(resp["success"], true, "write should succeed: {:?}", resp);
    assert_eq!(
        resp["syntax_valid"], true,
        "valid TS should have syntax_valid: true"
    );

    let _ = fs::remove_file(&target);
    let status = aft.shutdown();
    assert!(status.success());
}

#[test]
fn write_syntax_invalid() {
    let mut aft = AftProcess::spawn();
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("write_invalid.ts");
    let _ = fs::remove_file(&target);

    let resp = aft.send(&format!(
        r#"{{"id":"w-4","command":"write","file":"{}","content":"export function {{ broken syntax"}}"#,
        target.display()
    ));

    assert_eq!(
        resp["success"], true,
        "write should succeed even with bad syntax: {:?}",
        resp
    );
    assert_eq!(
        resp["syntax_valid"], false,
        "broken TS should have syntax_valid: false"
    );
    assert_eq!(resp["rolled_back"], false);

    let _ = fs::remove_file(&target);
    let status = aft.shutdown();
    assert!(status.success());
}

#[test]
fn single_file_write_rolls_back_on_invalid_syntax() {
    let mut aft = AftProcess::spawn();
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("write_rollback_valid_to_invalid.ts");
    let original = "export function ok(): string { return \"ok\"; }\n";
    fs::write(&target, original).unwrap();

    let req = serde_json::json!({
        "id": "w-rb-1",
        "command": "write",
        "file": target.display().to_string(),
        "content": "export function { broken syntax",
    });
    let resp = aft.send(&serde_json::to_string(&req).unwrap());

    assert_eq!(
        resp["success"], true,
        "write should report syntax failure: {:?}",
        resp
    );
    assert_eq!(resp["syntax_valid"], false);
    assert_eq!(resp["rolled_back"], true);
    assert_eq!(fs::read_to_string(&target).unwrap(), original);

    let _ = fs::remove_file(&target);
    let status = aft.shutdown();
    assert!(status.success());
}

#[test]
fn single_file_write_does_not_rollback_when_pre_was_invalid() {
    let mut aft = AftProcess::spawn();
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("write_no_rollback_pre_invalid.ts");
    fs::write(&target, "export function { already broken\n").unwrap();
    let replacement = "export const = still_broken\n";

    let req = serde_json::json!({
        "id": "w-rb-2",
        "command": "write",
        "file": target.display().to_string(),
        "content": replacement,
    });
    let resp = aft.send(&serde_json::to_string(&req).unwrap());

    assert_eq!(
        resp["success"], true,
        "write should preserve invalid replacement: {:?}",
        resp
    );
    assert_eq!(resp["syntax_valid"], false);
    assert_eq!(resp["rolled_back"], false);
    assert_eq!(fs::read_to_string(&target).unwrap(), replacement);

    let _ = fs::remove_file(&target);
    let status = aft.shutdown();
    assert!(status.success());
}

#[test]
fn single_file_write_does_not_rollback_when_new_file() {
    let mut aft = AftProcess::spawn();
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("write_no_rollback_new_invalid.ts");
    let _ = fs::remove_file(&target);

    let req = serde_json::json!({
        "id": "w-rb-3",
        "command": "write",
        "file": target.display().to_string(),
        "content": "export function { broken syntax",
    });
    let resp = aft.send(&serde_json::to_string(&req).unwrap());

    assert_eq!(
        resp["success"], true,
        "new invalid file should be written: {:?}",
        resp
    );
    assert_eq!(resp["syntax_valid"], false);
    assert_eq!(resp["rolled_back"], false);
    assert!(fs::read_to_string(&target)
        .unwrap()
        .contains("broken syntax"));

    let _ = fs::remove_file(&target);
    let status = aft.shutdown();
    assert!(status.success());
}

// ============================================================================
// edit_symbol command tests
// ============================================================================

#[test]
fn edit_symbol_replace() {
    let mut aft = AftProcess::spawn();
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("edit_replace.ts");

    // Copy fixture
    let fixture = fixture_path("sample.ts");
    fs::copy(&fixture, &target).unwrap();

    // Build request with serde_json to avoid escaping issues
    // Note: symbol range starts at `function` (col 7), not at `export`
    let req = serde_json::json!({
        "id": "es-1",
        "command": "edit_symbol",
        "file": target.display().to_string(),
        "symbol": "greet",
        "operation": "replace",
        "content": "function greet(name: string): string {\n  return `Hey, ${name}!`;\n}"
    });

    let resp = aft.send(&serde_json::to_string(&req).unwrap());

    assert_eq!(
        resp["success"], true,
        "edit_symbol replace should succeed: {:?}",
        resp
    );
    assert_eq!(resp["symbol"], "greet");
    assert_eq!(resp["operation"], "replace");
    assert_eq!(resp["syntax_valid"], true, "replacement should be valid TS");
    assert!(resp["backup_id"].is_string(), "should have backup_id");
    assert!(resp["range"].is_object(), "should have original range");

    // Verify content changed on disk
    let content = fs::read_to_string(&target).unwrap();
    assert!(
        content.contains("Hey,"),
        "should contain new greeting: {}",
        content
    );
    assert!(
        !content.contains("Hello,"),
        "should not contain old greeting"
    );

    let _ = fs::remove_file(&target);
    let status = aft.shutdown();
    assert!(status.success());
}

#[test]
fn edit_symbol_delete() {
    let mut aft = AftProcess::spawn();
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("edit_delete.ts");

    // Copy fixture
    let fixture = fixture_path("sample.ts");
    fs::copy(&fixture, &target).unwrap();

    // Verify internalHelper exists
    let before = fs::read_to_string(&target).unwrap();
    assert!(before.contains("internalHelper"));

    let resp = aft.send(&format!(
        r#"{{"id":"es-2","command":"edit_symbol","file":"{}","symbol":"internalHelper","operation":"delete"}}"#,
        target.display()
    ));

    assert_eq!(
        resp["success"], true,
        "edit_symbol delete should succeed: {:?}",
        resp
    );
    assert_eq!(resp["operation"], "delete");

    // Verify symbol is gone from disk
    let after = fs::read_to_string(&target).unwrap();
    assert!(
        !after.contains("function internalHelper"),
        "internalHelper should be deleted"
    );

    let _ = fs::remove_file(&target);
    let status = aft.shutdown();
    assert!(status.success());
}

#[test]
fn edit_symbol_ambiguous() {
    let mut aft = AftProcess::spawn();
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("edit_ambig.ts");

    // Copy ambiguous fixture
    let fixture = fixture_path("ambiguous.ts");
    fs::copy(&fixture, &target).unwrap();

    let resp = aft.send(&format!(
        r#"{{"id":"es-3","command":"edit_symbol","file":"{}","symbol":"process","operation":"replace","content":"// replaced"}}"#,
        target.display()
    ));

    assert_eq!(
        resp["success"], false,
        "ambiguous response should be an error: {:?}",
        resp
    );
    assert_eq!(resp["code"], "ambiguous_symbol");
    let candidates = resp["candidates"]
        .as_array()
        .expect("should have candidates array");
    assert!(
        candidates.len() >= 2,
        "should have at least 2 candidates: {:?}",
        candidates
    );

    // Each candidate should have name, qualified, line, kind
    for c in candidates {
        assert!(c["name"].is_string(), "candidate should have name: {:?}", c);
        assert!(
            c["qualified"].is_string(),
            "candidate should have qualified: {:?}",
            c
        );
        assert!(c["line"].is_number(), "candidate should have line: {:?}", c);
        assert!(c["kind"].is_string(), "candidate should have kind: {:?}", c);
    }

    let _ = fs::remove_file(&target);
    let status = aft.shutdown();
    assert!(status.success());
}

#[test]
fn edit_symbol_not_found() {
    let mut aft = AftProcess::spawn();
    let file = fixture_path("sample.ts");

    let resp = aft.send(&format!(
        r#"{{"id":"es-4","command":"edit_symbol","file":"{}","symbol":"nonexistent_symbol","operation":"replace","content":"// nope"}}"#,
        file.display()
    ));

    assert_eq!(
        resp["success"], false,
        "should fail for nonexistent symbol: {:?}",
        resp
    );
    assert_eq!(resp["code"], "symbol_not_found");
    assert!(resp["message"]
        .as_str()
        .unwrap()
        .contains("nonexistent_symbol"));

    let status = aft.shutdown();
    assert!(status.success());
}

// ============================================================================
// edit_match command tests
// ============================================================================

#[test]
fn edit_match_single_occurrence() {
    let mut aft = AftProcess::spawn();
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("match_single.ts");

    // Copy fixture
    let fixture = fixture_path("sample.ts");
    fs::copy(&fixture, &target).unwrap();

    // "internalHelper" appears exactly once in sample.ts
    let req = serde_json::json!({
        "id": "em-1",
        "command": "edit_match",
        "file": target.display().to_string(),
        "match": "internalHelper",
        "replacement": "secretHelper"
    });
    let resp = aft.send(&serde_json::to_string(&req).unwrap());

    assert_eq!(
        resp["success"], true,
        "edit_match should succeed: {:?}",
        resp
    );
    assert_eq!(resp["replacements"], 1);
    assert_eq!(resp["syntax_valid"], true);
    assert!(resp["backup_id"].is_string(), "should have backup_id");

    // Verify content on disk
    let content = fs::read_to_string(&target).unwrap();
    assert!(
        content.contains("secretHelper"),
        "should contain replacement"
    );
    assert!(
        !content.contains("internalHelper"),
        "should not contain original match"
    );

    let _ = fs::remove_file(&target);
    let status = aft.shutdown();
    assert!(status.success());
}

#[test]
fn edit_match_returns_inline_lsp_diagnostics_when_requested() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let file = root.join("main.rs");
    let cargo_toml = root.join("Cargo.toml");
    fs::write(
        &cargo_toml,
        "[package]\nname = \"inline-diag\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
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
        r#"{{"id":"cfg-inline","command":"configure","harness":"opencode","project_root":"{}"}}"#,
        root.display()
    ));
    assert_eq!(
        configure["success"], true,
        "configure failed: {configure:?}"
    );

    let req = serde_json::json!({
        "id": "em-inline-diag",
        "command": "edit_match",
        "file": file.display().to_string(),
        "match": "let value = 1",
        "replacement": "let answer = 1",
        "diagnostics": true,
    });
    let resp = aft.send(&serde_json::to_string(&req).unwrap());

    assert_eq!(resp["success"], true, "edit_match should succeed: {resp:?}");
    let diagnostics = resp["lsp_diagnostics"]
        .as_array()
        .expect("lsp_diagnostics array");
    assert_eq!(
        diagnostics.len(),
        2,
        "expected inline diagnostics: {resp:?}"
    );

    let canonical_file = fs::canonicalize(&file).expect("canonical file");
    assert_eq!(diagnostics[0]["file"], canonical_file.display().to_string());
    assert_eq!(diagnostics[0]["line"], 1);
    assert_eq!(diagnostics[0]["column"], 1);
    assert_eq!(diagnostics[0]["end_line"], 1);
    assert_eq!(diagnostics[0]["end_column"], 6);
    assert_eq!(diagnostics[0]["severity"], "error");
    assert_eq!(diagnostics[0]["message"], "test diagnostic error");
    assert_eq!(diagnostics[0]["code"], "E0001");
    assert_eq!(diagnostics[0]["source"], "fake-lsp");
    assert_eq!(diagnostics[1]["severity"], "warning");

    let status = aft.shutdown();
    assert!(status.success());
}

#[test]
fn edit_match_inline_lsp_diagnostics_respects_wait_ms() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let file = root.join("main.rs");
    let cargo_toml = root.join("Cargo.toml");
    fs::write(
        &cargo_toml,
        "[package]\nname = \"inline-diag-fast\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
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
        r#"{{"id":"cfg-inline-fast","command":"configure","harness":"opencode","project_root":"{}"}}"#,
        root.display()
    ));
    assert_eq!(
        configure["success"], true,
        "configure failed: {configure:?}"
    );

    let start = std::time::Instant::now();
    let req = serde_json::json!({
        "id": "em-inline-diag-fast",
        "command": "edit_match",
        "file": file.display().to_string(),
        "match": "let value = 1",
        "replacement": "let answer = 1",
        "diagnostics": true,
        "wait_ms": 2_000,
    });
    let resp = aft.send(&serde_json::to_string(&req).unwrap());
    let elapsed = start.elapsed();

    assert_eq!(resp["success"], true, "edit_match should succeed: {resp:?}");
    let diagnostics = resp["lsp_diagnostics"]
        .as_array()
        .expect("lsp_diagnostics array");
    assert_eq!(
        diagnostics.len(),
        2,
        "expected inline diagnostics: {resp:?}"
    );
    assert!(
        elapsed < std::time::Duration::from_millis(3_000),
        "expected event-driven wait, elapsed: {elapsed:?}, resp: {resp:?}"
    );

    let status = aft.shutdown();
    assert!(status.success());
}

#[test]
fn edit_match_multiple_occurrences_returns_candidates() {
    let mut aft = AftProcess::spawn();
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("match_ambig.ts");

    // Write a file with a repeated string
    let content = "const a = \"hello\";\nconst b = \"hello\";\nconst c = \"world\";\n";
    fs::write(&target, content).unwrap();

    let req = serde_json::json!({
        "id": "em-2",
        "command": "edit_match",
        "file": target.display().to_string(),
        "match": "hello",
        "replacement": "bye"
    });
    let resp = aft.send(&serde_json::to_string(&req).unwrap());

    assert_eq!(
        resp["success"], false,
        "ambiguous response should be an error: {:?}",
        resp
    );
    assert_eq!(resp["code"], "ambiguous_match");
    let occurrences = resp["occurrences"]
        .as_array()
        .expect("should have occurrences array");
    assert_eq!(occurrences.len(), 2, "should have exactly 2 occurrences");

    // Each occurrence should have index, line, context
    for occ in occurrences {
        assert!(occ["index"].is_number(), "should have index: {:?}", occ);
        assert!(occ["line"].is_number(), "should have line: {:?}", occ);
        assert!(occ["context"].is_string(), "should have context: {:?}", occ);
    }

    // File should be unchanged
    assert_eq!(
        fs::read_to_string(&target).unwrap(),
        content,
        "file should not be modified"
    );

    let _ = fs::remove_file(&target);
    let status = aft.shutdown();
    assert!(status.success());
}

#[test]
fn edit_match_with_occurrence_selector() {
    let mut aft = AftProcess::spawn();
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("match_occ.ts");

    // Write a file with a repeated string
    let content = "const a = \"hello\";\nconst b = \"hello\";\nconst c = \"world\";\n";
    fs::write(&target, content).unwrap();

    // Select occurrence index 1 (second match)
    let req = serde_json::json!({
        "id": "em-3",
        "command": "edit_match",
        "file": target.display().to_string(),
        "match": "hello",
        "replacement": "bye",
        "occurrence": 1
    });
    let resp = aft.send(&serde_json::to_string(&req).unwrap());

    assert_eq!(
        resp["success"], true,
        "edit_match with occurrence should succeed: {:?}",
        resp
    );
    assert_eq!(resp["replacements"], 1);

    // Verify: first "hello" untouched, second replaced
    let result = fs::read_to_string(&target).unwrap();
    assert!(
        result.contains("const a = \"hello\""),
        "first occurrence should be untouched"
    );
    assert!(
        result.contains("const b = \"bye\""),
        "second occurrence should be replaced"
    );

    let _ = fs::remove_file(&target);
    let status = aft.shutdown();
    assert!(status.success());
}

#[test]
fn edit_match_no_match() {
    let mut aft = AftProcess::spawn();
    let file = fixture_path("sample.ts");

    let req = serde_json::json!({
        "id": "em-4",
        "command": "edit_match",
        "file": file.display().to_string(),
        "match": "this_string_does_not_exist_anywhere",
        "replacement": "nope"
    });
    let resp = aft.send(&serde_json::to_string(&req).unwrap());

    assert_eq!(
        resp["success"], false,
        "should fail for no match: {:?}",
        resp
    );
    assert_eq!(resp["code"], "match_not_found");

    let status = aft.shutdown();
    assert!(status.success());
}

// ============================================================================
// batch command tests
// ============================================================================

#[test]
fn batch_multiple_edits() {
    let mut aft = AftProcess::spawn();
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("batch_multi.ts");

    let content = "const greeting = \"hello\";\nconst farewell = \"goodbye\";\nconst count = 42;\n";
    fs::write(&target, content).unwrap();

    let req = serde_json::json!({
        "id": "b-1",
        "command": "batch",
        "file": target.display().to_string(),
        "edits": [
            { "match": "hello", "replacement": "hi" },
            { "match": "goodbye", "replacement": "bye" }
        ]
    });
    let resp = aft.send(&serde_json::to_string(&req).unwrap());

    assert_eq!(resp["success"], true, "batch should succeed: {:?}", resp);
    assert_eq!(resp["edits_applied"], 2);
    assert!(resp["backup_id"].is_string(), "should have backup_id");

    let result = fs::read_to_string(&target).unwrap();
    assert!(
        result.contains("\"hi\""),
        "first edit should apply: {}",
        result
    );
    assert!(
        result.contains("\"bye\""),
        "second edit should apply: {}",
        result
    );
    assert!(
        result.contains("count = 42"),
        "untouched line should remain"
    );

    let _ = fs::remove_file(&target);
    let status = aft.shutdown();
    assert!(status.success());
}

#[test]
fn batch_rollback_on_failure() {
    let mut aft = AftProcess::spawn();
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("batch_rollback.ts");

    let content = "const greeting = \"hello\";\nconst count = 42;\n";
    fs::write(&target, content).unwrap();

    // First edit is valid, second has a match that doesn't exist
    let req = serde_json::json!({
        "id": "b-2",
        "command": "batch",
        "file": target.display().to_string(),
        "edits": [
            { "match": "hello", "replacement": "hi" },
            { "match": "nonexistent_string", "replacement": "nope" }
        ]
    });
    let resp = aft.send(&serde_json::to_string(&req).unwrap());

    assert_eq!(resp["success"], false, "batch should fail: {:?}", resp);
    assert_eq!(resp["code"], "batch_edit_failed");

    // File should be unchanged — no partial application, no backup taken
    let on_disk = fs::read_to_string(&target).unwrap();
    assert_eq!(
        on_disk, content,
        "file should be unchanged after failed batch"
    );

    let _ = fs::remove_file(&target);
    let status = aft.shutdown();
    assert!(status.success());
}

#[test]
fn batch_fuzzy_match() {
    let mut aft = AftProcess::spawn();
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("batch_fuzzy.ts");

    // Source has specific whitespace; edits use slightly different whitespace
    let content = "function greet() {\n    console.log(\"hello\");  \n    return true;\n}\n";
    fs::write(&target, content).unwrap();

    let req = serde_json::json!({
        "id": "b-fuzzy",
        "command": "batch",
        "file": target.display().to_string(),
        "edits": [
            // Trailing whitespace mismatch: source line has trailing spaces, match text doesn't
            { "match": "    console.log(\"hello\");", "replacement": "    console.log(\"hi\");" },
        ]
    });
    let resp = aft.send(&serde_json::to_string(&req).unwrap());

    assert_eq!(
        resp["success"], true,
        "batch fuzzy should succeed: {:?}",
        resp
    );
    assert_eq!(resp["edits_applied"], 1);

    let result = fs::read_to_string(&target).unwrap();
    assert!(
        result.contains("\"hi\""),
        "fuzzy edit should apply: {}",
        result
    );
    // Trailing spaces from the original line should be replaced (not left behind)
    assert!(
        !result.contains("\"hello\""),
        "old text should be gone: {}",
        result
    );

    let _ = fs::remove_file(&target);
    let status = aft.shutdown();
    assert!(status.success());
}

#[test]
fn batch_line_range_edit() {
    let mut aft = AftProcess::spawn();
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("batch_linerange.ts");

    let content = "line zero\nline one\nline two\nline three\n";
    fs::write(&target, content).unwrap();

    // Replace line 2 (1-indexed, i.e. "line one") with new content
    let req = serde_json::json!({
        "id": "b-3",
        "command": "batch",
        "file": target.display().to_string(),
        "edits": [
            { "line_start": 2, "line_end": 2, "content": "replaced line\n" }
        ]
    });
    let resp = aft.send(&serde_json::to_string(&req).unwrap());

    assert_eq!(
        resp["success"], true,
        "batch line-range should succeed: {:?}",
        resp
    );
    assert_eq!(resp["edits_applied"], 1);

    let result = fs::read_to_string(&target).unwrap();
    assert!(result.contains("line zero"), "line 0 untouched");
    assert!(
        result.contains("replaced line"),
        "line 1 should be replaced"
    );
    assert!(
        !result.contains("line one"),
        "original line 1 should be gone"
    );
    assert!(result.contains("line two"), "line 2 untouched");

    let _ = fs::remove_file(&target);
    let status = aft.shutdown();
    assert!(status.success());
}

#[test]
fn batch_with_undo() {
    let mut aft = AftProcess::spawn();
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("batch_undo.ts");

    let original = "const x = 1;\nconst y = 2;\n";
    fs::write(&target, original).unwrap();

    // Apply batch
    let req = serde_json::json!({
        "id": "b-4",
        "command": "batch",
        "file": target.display().to_string(),
        "edits": [
            { "match": "const x = 1", "replacement": "const x = 100" },
            { "match": "const y = 2", "replacement": "const y = 200" }
        ]
    });
    let resp = aft.send(&serde_json::to_string(&req).unwrap());
    assert_eq!(resp["success"], true, "batch should succeed: {:?}", resp);

    // Verify edits applied
    let modified = fs::read_to_string(&target).unwrap();
    assert!(modified.contains("x = 100"), "should have x = 100");
    assert!(modified.contains("y = 200"), "should have y = 200");

    // Undo
    let undo_resp = aft.send(&format!(
        r#"{{"id":"b-4u","command":"undo","file":"{}"}}"#,
        target.display()
    ));
    assert_eq!(
        undo_resp["success"], true,
        "undo should succeed: {:?}",
        undo_resp
    );

    // Verify original restored
    let restored = fs::read_to_string(&target).unwrap();
    assert_eq!(restored, original, "undo should restore original content");

    let _ = fs::remove_file(&target);
    let status = aft.shutdown();
    assert!(status.success());
}

#[test]
fn batch_overlapping_ranges_returns_overlapping_edits_error() {
    let mut aft = AftProcess::spawn();
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("batch_overlap.txt");
    let original = "abcdefghij\n";
    fs::write(&target, original).unwrap();

    let req = serde_json::json!({
        "id": "b-overlap",
        "command": "batch",
        "file": target.display().to_string(),
        "edits": [
            { "match": "cdef", "replacement": "XXXX" },
            { "match": "defg", "replacement": "YYYY" }
        ]
    });
    let resp = aft.send(&serde_json::to_string(&req).unwrap());

    assert_eq!(resp["success"], false, "batch should fail: {:?}", resp);
    assert_eq!(resp["code"], "overlapping_edits");
    assert!(resp["message"].as_str().unwrap().contains("overlaps"));
    assert_eq!(fs::read_to_string(&target).unwrap(), original);

    let status = aft.shutdown();
    assert!(status.success());
}

#[test]
fn batch_accepts_old_string_new_string_keys() {
    let mut aft = AftProcess::spawn();
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("batch_old_new.txt");
    let original = "alpha beta gamma\n";
    fs::write(&target, original).unwrap();

    let req = serde_json::json!({
        "id": "b-old-new",
        "command": "batch",
        "file": target.display().to_string(),
        "edits": [
            { "oldString": "alpha", "newString": "ALPHA" },
            { "oldString": "gamma", "newString": "GAMMA" }
        ]
    });
    let resp = aft.send(&serde_json::to_string(&req).unwrap());

    assert_eq!(resp["success"], true, "batch should succeed: {:?}", resp);
    assert_eq!(resp["edits_applied"], 2);
    assert_eq!(fs::read_to_string(&target).unwrap(), "ALPHA beta GAMMA\n");

    let status = aft.shutdown();
    assert!(status.success());
}

#[test]
fn batch_fuzzy_matching_covers_all_progressive_passes() {
    let mut aft = AftProcess::spawn();
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("batch_fuzzy_passes.txt");
    let original = concat!(
        "exact = 1\n",
        "trailing = 2   \n",
        "    trimmed = 3   \n",
        "normalized\u{00a0}space = 4\n"
    );
    fs::write(&target, original).unwrap();

    let req = serde_json::json!({
        "id": "b-fuzzy-passes",
        "command": "batch",
        "file": target.display().to_string(),
        "edits": [
            { "match": "exact = 1\n", "replacement": "exact = 10\n" },
            { "match": "trailing = 2\n", "replacement": "trailing = 20\n" },
            { "match": "trimmed = 3\n", "replacement": "trimmed = 30\n" },
            { "match": "normalized space = 4", "replacement": "normalized space = 40\n" }
        ]
    });
    let resp = aft.send(&serde_json::to_string(&req).unwrap());

    assert_eq!(resp["success"], true, "batch should succeed: {:?}", resp);
    assert_eq!(resp["edits_applied"], 4);
    assert_eq!(
        fs::read_to_string(&target).unwrap(),
        concat!(
            "exact = 10\n",
            "trailing = 20\n",
            "trimmed = 30\n",
            "normalized space = 40\n"
        )
    );

    let status = aft.shutdown();
    assert!(status.success());
}

#[test]
fn edit_match_replace_all_replaces_multiple_occurrences() {
    let mut aft = AftProcess::spawn();
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("match_replace_all.txt");
    let original = "hello hello hello\n";
    fs::write(&target, original).unwrap();

    let req = serde_json::json!({
        "id": "em-replace-all",
        "command": "edit_match",
        "file": target.display().to_string(),
        "match": "hello",
        "replacement": "bye",
        "replace_all": true,
        "replaceAll": true
    });
    let resp = aft.send(&serde_json::to_string(&req).unwrap());

    assert_eq!(
        resp["success"], true,
        "edit_match should succeed: {:?}",
        resp
    );
    assert_eq!(resp["replacements"], 3);
    assert_eq!(fs::read_to_string(&target).unwrap(), "bye bye bye\n");

    let status = aft.shutdown();
    assert!(status.success());
}

#[test]
fn replace_byte_range_invalid_ranges_return_errors() {
    let aft = AftProcess::spawn();
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("replace_invalid.txt");
    fs::write(&target, "abcdef").unwrap();
    let source = fs::read_to_string(&target).unwrap();

    let start_after_end = replace_byte_range(&source, 4, 2, "x").unwrap_err();
    assert_eq!(start_after_end.code(), "invalid_request");
    assert!(start_after_end.to_string().contains("start must be <= end"));

    let end_out_of_bounds = replace_byte_range(&source, 0, source.len() + 1, "x").unwrap_err();
    assert_eq!(end_out_of_bounds.code(), "invalid_request");
    assert!(end_out_of_bounds
        .to_string()
        .contains("end exceeds source length"));

    assert_eq!(fs::read_to_string(&target).unwrap(), "abcdef");

    let status = aft.shutdown();
    assert!(status.success());
}

#[test]
fn replace_byte_range_rejects_non_char_boundaries() {
    let aft = AftProcess::spawn();
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("replace_utf8.txt");
    fs::write(&target, "aéz").unwrap();
    let source = fs::read_to_string(&target).unwrap();

    let invalid_end = replace_byte_range(&source, 1, 2, "x").unwrap_err();
    assert_eq!(invalid_end.code(), "invalid_request");
    assert!(invalid_end
        .to_string()
        .contains("end is not a char boundary"));

    let invalid_start = replace_byte_range(&source, 2, 3, "x").unwrap_err();
    assert_eq!(invalid_start.code(), "invalid_request");
    assert!(invalid_start
        .to_string()
        .contains("start is not a char boundary"));

    assert_eq!(fs::read_to_string(&target).unwrap(), "aéz");

    let status = aft.shutdown();
    assert!(status.success());
}

// ============================================================================
// glob edit_match tests
// ============================================================================

#[test]
fn edit_match_glob_replaces_across_files() {
    let mut aft = AftProcess::spawn();
    let dir = tempfile::tempdir().unwrap();
    let dir_path = dir.path();

    fs::write(dir_path.join("a.ts"), "const x = \"OLD_VALUE\";\n").unwrap();
    fs::write(
        dir_path.join("b.ts"),
        "const y = \"OLD_VALUE\";\nconst z = \"OLD_VALUE\";\n",
    )
    .unwrap();
    fs::write(dir_path.join("c.json"), "{\"key\": \"OLD_VALUE\"}\n").unwrap();

    let req = serde_json::json!({
        "id": "glob-1",
        "command": "edit_match",
        "file": format!("{}/**/*.ts", dir_path.display()),
        "match": "OLD_VALUE",
        "replacement": "NEW_VALUE"
    });
    let resp = aft.send(&serde_json::to_string(&req).unwrap());

    assert_eq!(
        resp["success"], true,
        "glob edit should succeed: {:?}",
        resp
    );
    assert_eq!(resp["total_files"], 2, "should match 2 .ts files");
    assert_eq!(
        resp["total_replacements"], 3,
        "should replace 3 occurrences"
    );
    assert!(resp["files"].is_array(), "should have files array");
    assert_eq!(resp["files"].as_array().unwrap().len(), 2);

    let a_content = fs::read_to_string(dir_path.join("a.ts")).unwrap();
    assert!(
        a_content.contains("NEW_VALUE"),
        "a.ts should have NEW_VALUE"
    );
    assert!(
        !a_content.contains("OLD_VALUE"),
        "a.ts should not have OLD_VALUE"
    );

    let b_content = fs::read_to_string(dir_path.join("b.ts")).unwrap();
    assert_eq!(b_content.matches("NEW_VALUE").count(), 2);

    let c_content = fs::read_to_string(dir_path.join("c.json")).unwrap();
    assert!(
        c_content.contains("OLD_VALUE"),
        "c.json should be unchanged"
    );

    let status = aft.shutdown();
    assert!(status.success());
}

#[test]
fn edit_match_glob_no_matches_in_files() {
    let mut aft = AftProcess::spawn();
    let dir = tempfile::tempdir().unwrap();
    let dir_path = dir.path();

    fs::write(dir_path.join("a.ts"), "const x = 1;\n").unwrap();

    let req = serde_json::json!({
        "id": "glob-nomatch-1",
        "command": "edit_match",
        "file": format!("{}/*.ts", dir_path.display()),
        "match": "NONEXISTENT",
        "replacement": "whatever"
    });
    let resp = aft.send(&serde_json::to_string(&req).unwrap());

    assert_eq!(resp["success"], false);

    let status = aft.shutdown();
    assert!(status.success());
}

#[test]
fn edit_match_glob_no_files_matched() {
    let mut aft = AftProcess::spawn();
    let dir = tempfile::tempdir().unwrap();
    let dir_path = dir.path();

    let req = serde_json::json!({
        "id": "glob-nofiles-1",
        "command": "edit_match",
        "file": format!("{}/*.xyz", dir_path.display()),
        "match": "something",
        "replacement": "else"
    });
    let resp = aft.send(&serde_json::to_string(&req).unwrap());

    assert_eq!(resp["success"], false);

    let status = aft.shutdown();
    assert!(status.success());
}

#[test]
#[cfg(unix)]
fn edit_match_glob_formats_each_matched_file() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    fs::write(root.join("biome.json"), "{}\n").unwrap();
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(root.join("src/a.ts"), "export   const   a=\"OLD\";\n").unwrap();
    fs::write(root.join("src/b.ts"), "export   const   b=\"OLD\";\n").unwrap();
    install_executable(
        root,
        "biome",
        r#"#!/bin/sh
if [ "$1" = "--version" ]; then echo "biome 2.0.0"; exit 0; fi
file=""
for arg in "$@"; do file="$arg"; done
echo "$file" >> "$(dirname "$file")/formatter-count.log"
python3 - "$file" <<'PY'
import re, sys
p=sys.argv[1]
s=open(p).read()
s=re.sub(r"export\s+const\s+(\w+)\s*=\s*([^;\n]+);?", r"export const \1 = \2;", s)
open(p,"w").write(s)
PY
exit 0
"#,
    );

    let path = path_with_node_bin(root);
    let mut aft = AftProcess::spawn_with_env(&[("PATH", path.as_os_str())]);
    let cfg = aft.configure(root);
    assert_eq!(cfg["success"], true, "configure should succeed: {cfg:?}");
    let resp = aft.send(
        &serde_json::json!({
            "id": "glob-fmt-success",
            "command": "edit_match",
            "file": format!("{}/*.ts", root.join("src").display()),
            "match": "OLD",
            "replacement": "NEW  VALUE"
        })
        .to_string(),
    );

    assert_eq!(resp["success"], true, "glob edit should succeed: {resp:?}");
    assert_eq!(resp["total_files"], 2);
    assert_eq!(resp["format_skipped_count"], 0);
    assert_eq!(resp["format_skip_reasons"].as_array().unwrap().len(), 0);
    let files = resp["files"].as_array().unwrap();
    assert!(files.iter().all(|f| f["formatted"] == true), "{resp:?}");
    assert!(files
        .iter()
        .all(|f| f.get("format_skipped_reason").is_none()));
    let count_log = fs::read_to_string(root.join("src/formatter-count.log")).unwrap();
    assert_eq!(count_log.lines().count(), 2);
    assert!(fs::read_to_string(root.join("src/a.ts"))
        .unwrap()
        .contains("export const a = \"NEW  VALUE\";"));

    let status = aft.shutdown();
    assert!(status.success());
}

#[test]
#[cfg(unix)]
fn edit_match_glob_reports_formatter_excluded_path_per_file_and_summary() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(root.join("src/a.ts"), "export const a = \"OLD\";\n").unwrap();
    fs::write(root.join("src/b.ts"), "export const b = \"OLD\";\n").unwrap();
    install_executable(
        root,
        "biome",
        "#!/bin/sh\nif [ \"$1\" = \"--version\" ]; then echo \"biome 2.0.0\"; exit 0; fi\necho \"No files were processed in the specified paths.\" >&2\nexit 1\n",
    );

    let path = path_with_node_bin(root);
    let mut aft = AftProcess::spawn_with_env(&[("PATH", path.as_os_str())]);
    let cfg = aft.send(
        &serde_json::json!({
            "id": "cfg-glob-excluded",
            "command": "configure",
            "harness": "opencode",
            "project_root": root.display().to_string(),
            "format_on_edit": true,
            "formatter": { "typescript": "biome" }
        })
        .to_string(),
    );
    assert_eq!(cfg["success"], true, "configure should succeed: {cfg:?}");
    let resp = aft.send(
        &serde_json::json!({
            "id": "glob-fmt-excluded",
            "command": "edit_match",
            "file": format!("{}/*.ts", root.join("src").display()),
            "match": "OLD",
            "replacement": "NEW"
        })
        .to_string(),
    );

    assert_eq!(resp["success"], true, "glob edit should succeed: {resp:?}");
    assert_eq!(resp["format_skipped_count"], 2);
    assert_eq!(
        resp["format_skip_reasons"],
        serde_json::json!(["formatter_excluded_path"])
    );
    let files = resp["files"].as_array().unwrap();
    assert!(files.iter().all(|f| f["formatted"] == false), "{resp:?}");
    assert!(
        files
            .iter()
            .all(|f| f["format_skipped_reason"] == "formatter_excluded_path"),
        "{resp:?}"
    );

    let status = aft.shutdown();
    assert!(status.success());
}

#[test]
fn edit_match_glob_reports_format_on_edit_false_for_each_file() {
    let mut aft = AftProcess::spawn();
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    fs::write(root.join("biome.json"), "{}\n").unwrap();
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(root.join("src/a.ts"), "export const a = \"OLD\";\n").unwrap();
    fs::write(root.join("src/b.ts"), "export const b = \"OLD\";\n").unwrap();
    let cfg = aft.send(
        &serde_json::json!({
            "id": "cfg-glob-disabled",
            "command": "configure",
            "harness": "opencode",
            "project_root": root.display().to_string(),
            "format_on_edit": false
        })
        .to_string(),
    );
    assert_eq!(cfg["success"], true, "configure should succeed: {cfg:?}");
    let resp = aft.send(
        &serde_json::json!({
            "id": "glob-fmt-disabled",
            "command": "edit_match",
            "file": format!("{}/*.ts", root.join("src").display()),
            "match": "OLD",
            "replacement": "NEW"
        })
        .to_string(),
    );

    assert_eq!(resp["success"], true, "glob edit should succeed: {resp:?}");
    assert_eq!(resp["format_skipped_count"], 2);
    assert_eq!(
        resp["format_skip_reasons"],
        serde_json::json!(["no_formatter_configured"])
    );
    let files = resp["files"].as_array().unwrap();
    assert!(files.iter().all(|f| f["formatted"] == false), "{resp:?}");
    assert!(
        files
            .iter()
            .all(|f| f["format_skipped_reason"] == "no_formatter_configured"),
        "{resp:?}"
    );

    let status = aft.shutdown();
    assert!(status.success());
}

#[test]
#[cfg(unix)]
fn edit_match_glob_mixed_success_and_skip_reasons() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(root.join("src/a.ts"), "export   const   a=\"OLD\";\n").unwrap();
    fs::write(root.join("src/b.py"), "x = 'OLD'\n").unwrap();
    fs::write(root.join("src/c.txt"), "OLD\n").unwrap();
    install_executable(
        root,
        "biome",
        r#"#!/bin/sh
if [ "$1" = "--version" ]; then echo "biome 2.0.0"; exit 0; fi
file=""
for arg in "$@"; do file="$arg"; done
case "$file" in
  *.ts) sed -E 's/  +/ /g' "$file" > "$file.tmp" && mv "$file.tmp" "$file"; exit 0 ;;
  *.py) echo "No files were processed in the specified paths." >&2; exit 1 ;;
esac
exit 0
"#,
    );

    let path = path_with_node_bin(root);
    let mut aft = AftProcess::spawn_with_env(&[("PATH", path.as_os_str())]);
    let cfg = aft.send(
        &serde_json::json!({
            "id": "cfg-glob-mixed",
            "command": "configure",
            "harness": "opencode",
            "project_root": root.display().to_string(),
            "format_on_edit": true,
            "formatter": { "typescript": "biome", "python": "biome" }
        })
        .to_string(),
    );
    assert_eq!(cfg["success"], true, "configure should succeed: {cfg:?}");
    let resp = aft.send(
        &serde_json::json!({
            "id": "glob-fmt-mixed",
            "command": "edit_match",
            "file": format!("{}/*", root.join("src").display()),
            "match": "OLD",
            "replacement": "NEW"
        })
        .to_string(),
    );

    assert_eq!(resp["success"], true, "glob edit should succeed: {resp:?}");
    assert_eq!(resp["total_files"], 3);
    assert_eq!(resp["format_skipped_count"], 2);
    assert_eq!(
        resp["format_skip_reasons"],
        serde_json::json!(["formatter_excluded_path", "unsupported_language"])
    );
    let files = resp["files"].as_array().unwrap();
    let ts = files
        .iter()
        .find(|f| f["file"].as_str().unwrap().ends_with("a.ts"))
        .unwrap();
    let py = files
        .iter()
        .find(|f| f["file"].as_str().unwrap().ends_with("b.py"))
        .unwrap();
    let txt = files
        .iter()
        .find(|f| f["file"].as_str().unwrap().ends_with("c.txt"))
        .unwrap();
    assert_eq!(ts["formatted"], true);
    assert!(ts.get("format_skipped_reason").is_none());
    assert_eq!(py["format_skipped_reason"], "formatter_excluded_path");
    assert_eq!(txt["format_skipped_reason"], "unsupported_language");

    let status = aft.shutdown();
    assert!(status.success());
}

// ============================================================================
// no_op honest reporting tests (v0.27.1, GitHub #45)
//
// Rust sets `no_op: true` on the response when the post-write file content is
// byte-identical to the pre-write state. This separates "matched but produced
// no change" from a real failure mode so Pi/OpenCode UIs can render an
// informative message instead of a bare +0/-0 that looks like a tool bug.
// ============================================================================

#[test]
fn edit_match_no_op_when_old_string_equals_new_string() {
    let mut aft = AftProcess::spawn();
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("identity.txt");
    fs::write(&target, "hello world\nfoo bar\n").unwrap();
    let original = fs::read(&target).unwrap();

    let req = serde_json::json!({
        "id": "no-op-identity",
        "command": "edit_match",
        "file": target.display().to_string(),
        "match": "foo",
        "replacement": "foo",
        "include_diff": true
    });
    let resp = aft.send(&serde_json::to_string(&req).unwrap());

    assert_eq!(resp["success"], true, "edit should succeed: {resp:?}");
    assert_eq!(resp["replacements"], 1, "match was found and applied once");
    assert_eq!(
        resp["no_op"], true,
        "byte-identical replacement must surface no_op: true"
    );
    assert_eq!(resp["diff"]["additions"], 0);
    assert_eq!(resp["diff"]["deletions"], 0);

    // File should genuinely be unchanged on disk
    assert_eq!(fs::read(&target).unwrap(), original);

    let _ = fs::remove_file(&target);
    let status = aft.shutdown();
    assert!(status.success());
}

#[test]
fn edit_match_no_op_absent_when_real_change_applied() {
    let mut aft = AftProcess::spawn();
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("real_change.txt");
    fs::write(&target, "hello world\nfoo bar\n").unwrap();

    let req = serde_json::json!({
        "id": "no-op-real",
        "command": "edit_match",
        "file": target.display().to_string(),
        "match": "foo",
        "replacement": "BAR",
        "include_diff": true
    });
    let resp = aft.send(&serde_json::to_string(&req).unwrap());

    assert_eq!(resp["success"], true);
    assert_eq!(resp["replacements"], 1);
    assert!(
        resp.get("no_op").is_none() || resp["no_op"] != true,
        "real change must NOT set no_op: true (got {resp:?})"
    );
    assert_eq!(resp["diff"]["additions"], 1);
    assert_eq!(resp["diff"]["deletions"], 1);

    let _ = fs::remove_file(&target);
    let status = aft.shutdown();
    assert!(status.success());
}

#[test]
fn write_no_op_when_content_identical_to_existing_file() {
    let mut aft = AftProcess::spawn();
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("same.txt");
    let body = "identical content\n";
    fs::write(&target, body).unwrap();

    let req = serde_json::json!({
        "id": "write-same",
        "command": "write",
        "file": target.display().to_string(),
        "content": body,
        "include_diff": true
    });
    let resp = aft.send(&serde_json::to_string(&req).unwrap());

    assert_eq!(resp["success"], true, "write should succeed: {resp:?}");
    assert_eq!(resp["created"], false);
    assert_eq!(
        resp["no_op"], true,
        "writing the same bytes must surface no_op: true"
    );
    assert_eq!(resp["diff"]["additions"], 0);
    assert_eq!(resp["diff"]["deletions"], 0);

    let _ = fs::remove_file(&target);
    let status = aft.shutdown();
    assert!(status.success());
}

#[test]
fn write_no_op_absent_when_file_created() {
    // Creating a new file is not a "no-op" — there was no pre-state to compare
    // against. `created: true` is the signal; `no_op` should be unset.
    let mut aft = AftProcess::spawn();
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("brand_new.txt");
    let _ = fs::remove_file(&target);

    let req = serde_json::json!({
        "id": "write-new",
        "command": "write",
        "file": target.display().to_string(),
        "content": "fresh content\n",
        "include_diff": true
    });
    let resp = aft.send(&serde_json::to_string(&req).unwrap());

    assert_eq!(resp["success"], true);
    assert_eq!(resp["created"], true);
    assert!(
        resp.get("no_op").is_none() || resp["no_op"] != true,
        "create-file path must NOT set no_op: true (got {resp:?})"
    );

    let _ = fs::remove_file(&target);
    let status = aft.shutdown();
    assert!(status.success());
}

#[test]
fn write_no_op_absent_when_content_actually_differs() {
    let mut aft = AftProcess::spawn();
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("differs.txt");
    fs::write(&target, "original\n").unwrap();

    let req = serde_json::json!({
        "id": "write-diff",
        "command": "write",
        "file": target.display().to_string(),
        "content": "modified\n",
        "include_diff": true
    });
    let resp = aft.send(&serde_json::to_string(&req).unwrap());

    assert_eq!(resp["success"], true);
    assert_eq!(resp["created"], false);
    assert!(
        resp.get("no_op").is_none() || resp["no_op"] != true,
        "real overwrite must NOT set no_op: true (got {resp:?})"
    );
    assert_eq!(resp["diff"]["additions"], 1);
    assert_eq!(resp["diff"]["deletions"], 1);

    let _ = fs::remove_file(&target);
    let status = aft.shutdown();
    assert!(status.success());
}
