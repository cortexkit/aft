//! Reply text for files that `write`, `edit` and `apply_patch` create with
//! content that does not parse. Existing files are rolled back instead; a new
//! file has nothing to roll back to, so it is kept and the reply says where
//! the first parse error is.

use std::fs;
use std::path::Path;

use serde_json::{json, Value};

use super::helpers::AftProcess;

const BROKEN_TS: &str =
    "export const first = 1;\nexport const second = 2;\nexport function broken( {{\n";
const VALID_TS: &str = "export const value = 1;\n";

fn syntax_notice(line: usize) -> String {
    format!(
        "Syntax check: the new file does not parse (first error near line {line}); it was written as given."
    )
}

fn configured(root: &Path) -> AftProcess {
    // Keep fixture paths stable across temp-directory aliases and Windows short names.
    let root = fs::canonicalize(root).expect("canonical temporary project root");
    let mut aft = AftProcess::spawn();
    let configured = aft.configure(&root);
    assert_eq!(configured["success"], true, "{configured:?}");
    aft
}

fn tool_call(aft: &mut AftProcess, id: &str, name: &str, arguments: Value) -> Value {
    let resp = aft.send(
        &json!({ "id": id, "command": "tool_call", "name": name, "arguments": arguments })
            .to_string(),
    );
    assert_eq!(resp["success"], true, "{name} tool call failed: {resp:?}");
    resp
}

fn text(resp: &Value) -> &str {
    resp["text"]
        .as_str()
        .unwrap_or_else(|| panic!("tool call reply has no text: {resp:?}"))
}

#[test]
fn write_new_broken_file_reports_first_error_line_last() {
    let dir = tempfile::tempdir().unwrap();
    let mut aft = configured(dir.path());

    let resp = tool_call(
        &mut aft,
        "write-new-broken",
        "write",
        json!({ "path": "src/broken.ts", "content": BROKEN_TS }),
    );

    assert_eq!(resp["created"], true, "{resp:?}");
    assert_eq!(resp["syntax_valid"], false, "{resp:?}");
    assert_eq!(resp["syntax_error_line"], 3, "{resp:?}");
    let reply = text(&resp);
    assert_eq!(
        reply.lines().last(),
        Some(syntax_notice(3).as_str()),
        "{reply}"
    );
    assert_eq!(reply.matches("Syntax check:").count(), 1, "{reply}");
    assert_eq!(
        fs::read_to_string(dir.path().join("src/broken.ts")).unwrap(),
        BROKEN_TS,
        "a new file is written as given"
    );
    assert!(aft.shutdown().success());
}

#[test]
fn write_new_valid_file_has_no_syntax_line() {
    let dir = tempfile::tempdir().unwrap();
    let mut aft = configured(dir.path());

    let resp = tool_call(
        &mut aft,
        "write-new-valid",
        "write",
        json!({ "path": "src/valid.ts", "content": VALID_TS }),
    );

    assert_eq!(resp["created"], true, "{resp:?}");
    assert_eq!(resp["syntax_valid"], true, "{resp:?}");
    assert!(resp.get("syntax_error_line").is_none(), "{resp:?}");
    assert!(!text(&resp).contains("Syntax check"), "{}", text(&resp));
    assert!(aft.shutdown().success());
}

/// A real script that `bash -n` accepts but tree-sitter-bash rejects, for its
/// escaped `[ ... \< ... ]` comparison on line 19.
#[cfg(unix)]
const BASH_GRAMMAR_GAP: &str = include_str!("../fixtures/aft-window-6deceda4.sh");
/// Truly broken: the second `case` item lacks its `)`. The recovery ERROR
/// starts on line 3; bash and the reply both name line 5.
#[cfg(unix)]
const BROKEN_BASH: &str = "set -u\necho a\ncase x in\n  a) echo ;;\n  b echo ;;\nesac\n";

#[cfg(unix)]
#[test]
fn bash_grammar_gap_written_or_appended_is_not_called_broken() {
    let dir = tempfile::tempdir().unwrap();
    let mut aft = configured(dir.path());
    for (name, arguments) in [
        (
            "write",
            json!({ "path": "window.sh", "content": BASH_GRAMMAR_GAP }),
        ),
        (
            "edit",
            json!({ "path": "appended.sh", "appendContent": BASH_GRAMMAR_GAP }),
        ),
    ] {
        let resp = tool_call(&mut aft, "bash-gap", name, arguments);
        assert_eq!(resp["syntax_valid"], true, "{resp:?}");
        assert!(resp.get("syntax_error_line").is_none(), "{resp:?}");
        assert!(!text(&resp).contains("Syntax check"), "{}", text(&resp));
    }
    assert!(aft.shutdown().success());
}

#[cfg(unix)]
#[test]
fn bash_broken_written_or_appended_reports_real_line() {
    let dir = tempfile::tempdir().unwrap();
    let mut aft = configured(dir.path());
    for (name, arguments) in [
        (
            "write",
            json!({ "path": "broken.sh", "content": BROKEN_BASH }),
        ),
        (
            "edit",
            json!({ "path": "appended.sh", "appendContent": BROKEN_BASH }),
        ),
    ] {
        let resp = tool_call(&mut aft, "bash-broken", name, arguments);
        assert_eq!(resp["syntax_valid"], false, "{resp:?}");
        assert_eq!(resp["syntax_error_line"], 5, "{resp:?}");
        assert!(text(&resp).ends_with(&syntax_notice(5)), "{resp:?}");
    }
    assert!(aft.shutdown().success());
}

#[test]
fn existing_clean_file_broken_by_write_or_edit_keeps_rollback_reply() {
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("src/clean.ts");
    fs::create_dir_all(target.parent().unwrap()).unwrap();
    fs::write(&target, VALID_TS).unwrap();
    let mut aft = configured(dir.path());

    let write = tool_call(
        &mut aft,
        "write-existing-broken",
        "write",
        json!({ "path": "src/clean.ts", "content": BROKEN_TS }),
    );
    assert_eq!(write["rolled_back"], true, "{write:?}");
    assert!(write.get("syntax_error_line").is_none(), "{write:?}");
    assert_eq!(
        text(&write),
        "Write rolled back: the content produced invalid syntax, so the file was left unchanged."
    );

    let edit = tool_call(
        &mut aft,
        "edit-existing-broken",
        "edit",
        json!({
            "path": "src/clean.ts",
            "edits": [{ "oldString": "= 1;", "newString": "= ;" }],
        }),
    );
    assert_eq!(edit["rolled_back"], true, "{edit:?}");
    assert!(edit.get("syntax_error_line").is_none(), "{edit:?}");
    assert_eq!(
        text(&edit),
        "Edit rolled back: the change produced invalid syntax, so the file was left unchanged."
    );
    assert_eq!(fs::read_to_string(&target).unwrap(), VALID_TS);
    assert!(aft.shutdown().success());
}

#[test]
fn edit_append_creating_broken_file_reports_first_error_line_last() {
    let dir = tempfile::tempdir().unwrap();
    let mut aft = configured(dir.path());

    let resp = tool_call(
        &mut aft,
        "append-new-broken",
        "edit",
        json!({ "path": "src/appended.ts", "appendContent": BROKEN_TS }),
    );

    assert_eq!(resp["created"], true, "{resp:?}");
    assert_eq!(resp["syntax_valid"], false, "{resp:?}");
    assert_eq!(resp["syntax_error_line"], 3, "{resp:?}");
    let reply = text(&resp);
    assert_eq!(
        reply.lines().last(),
        Some(syntax_notice(3).as_str()),
        "{reply}"
    );
    assert_eq!(
        fs::read_to_string(dir.path().join("src/appended.ts")).unwrap(),
        BROKEN_TS
    );

    // Appending to the now-existing file is not a creation: no line.
    let again = tool_call(
        &mut aft,
        "append-existing-broken",
        "edit",
        json!({ "path": "src/appended.ts", "appendContent": "export const later = 3;\n" }),
    );
    assert_eq!(again["created"], false, "{again:?}");
    assert!(again.get("syntax_error_line").is_none(), "{again:?}");
    assert!(!text(&again).contains("Syntax check"));
    assert!(aft.shutdown().success());
}

#[test]
fn apply_patch_places_notice_under_the_broken_created_file_only() {
    let dir = tempfile::tempdir().unwrap();
    let mut aft = configured(dir.path());

    let mut patch = String::from("*** Begin Patch\n*** Add File: src/good.ts\n");
    for line in VALID_TS.lines() {
        patch.push_str(&format!("+{line}\n"));
    }
    patch.push_str("*** Add File: src/bad.ts\n");
    for line in BROKEN_TS.lines() {
        patch.push_str(&format!("+{line}\n"));
    }
    patch.push_str("*** End Patch");

    let resp = tool_call(
        &mut aft,
        "patch-two-adds",
        "apply_patch",
        json!({ "patchText": patch }),
    );
    let reply = text(&resp);
    let notice = syntax_notice(3);
    let lines: Vec<&str> = reply.lines().collect();
    assert_eq!(
        lines,
        vec!["Created src/good.ts", "Created src/bad.ts", notice.as_str()],
        "{reply}"
    );

    let files = resp["metadata"]["files"]
        .as_array()
        .expect("metadata.files");
    let field = |rel: &str| {
        files
            .iter()
            .find(|file| file["relativePath"] == rel)
            .unwrap_or_else(|| panic!("no metadata for {rel}: {files:?}"))
            .get("syntax_error_line")
            .cloned()
    };
    assert_eq!(field("src/bad.ts"), Some(json!(3)));
    assert_eq!(field("src/good.ts"), None);
    assert_eq!(
        fs::read_to_string(dir.path().join("src/bad.ts")).unwrap(),
        BROKEN_TS
    );
    assert!(aft.shutdown().success());
}

/// tree-sitter-swift rejects a parenthesized tuple metatype that `swiftc`
/// accepts. Compiler confirmation may time out on a loaded runner; an
/// unfinished confirmation must be unknown rather than a definite syntax error.
#[test]
fn swift_new_file_does_not_report_unconfirmed_syntax_error() {
    if !cfg!(target_os = "macos") {
        eprintln!("skipping: the swiftc confirmation test runs only on macOS, where swiftc ships with the Xcode toolchain");
        return;
    }
    let swiftc_available = std::process::Command::new("xcrun")
        .args(["--find", "swiftc"])
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false);
    if !swiftc_available {
        eprintln!("skipping: swiftc is unavailable (xcrun --find swiftc failed)");
        return;
    }

    let dir = tempfile::tempdir().unwrap();
    let mut aft = configured(dir.path());
    let content = "_ = (Int, Result<String, Error>).self\n";

    let resp = tool_call(
        &mut aft,
        "write-new-swift",
        "write",
        json!({ "path": "Sources/tuple.swift", "content": content }),
    );

    assert_eq!(resp["created"], true, "{resp:?}");
    if resp.get("syntax_valid").is_some() {
        assert_eq!(resp["syntax_valid"], true, "{resp:?}");
        assert!(resp.get("syntax_skipped_reason").is_none(), "{resp:?}");
    } else {
        assert!(resp["syntax_skipped_reason"].is_string(), "{resp:?}");
        assert!(text(&resp).contains("Syntax check: unknown"), "{resp:?}");
    }
    assert!(resp.get("syntax_error_line").is_none(), "{resp:?}");
    assert!(
        !text(&resp).contains("new file does not parse"),
        "{}",
        text(&resp)
    );
    assert!(aft.shutdown().success());
}

/// Patch text that adds `path` with `content`, followed by `rest` (more hunks).
fn add_then(path: &str, content: &str, rest: &str) -> String {
    let mut patch = format!("*** Begin Patch\n*** Add File: {path}\n");
    for line in content.lines() {
        patch.push_str(&format!("+{line}\n"));
    }
    patch.push_str(rest);
    patch.push_str("*** End Patch");
    patch
}

fn metadata_syntax_line(resp: &Value, rel: &str) -> Option<Value> {
    let files = resp["metadata"]["files"]
        .as_array()
        .expect("metadata.files");
    let entries: Vec<&Value> = files
        .iter()
        .filter(|file| file["relativePath"] == rel)
        .collect();
    assert_eq!(entries.len(), 1, "one metadata entry for {rel}: {files:?}");
    entries[0].get("syntax_error_line").cloned()
}

/// A later hunk that updates a broken new file without fixing it: the notice
/// stays under the Created line but names the error line of the final file,
/// and metadata keeps `syntax_error_line`.
#[test]
fn apply_patch_add_broken_then_update_still_broken_reports_final_error_line() {
    let dir = tempfile::tempdir().unwrap();
    let mut aft = configured(dir.path());
    let patch = add_then(
        "src/bad.ts",
        BROKEN_TS,
        "*** Update File: src/bad.ts\n@@\n+export const zero = 0;\n+export const alsoZero = 0;\n export const first = 1;\n",
    );

    let resp = tool_call(
        &mut aft,
        "patch-add-broken-update-broken",
        "apply_patch",
        json!({ "patchText": patch }),
    );
    let reply = text(&resp);
    let notice = syntax_notice(5);
    let lines: Vec<&str> = reply.lines().collect();
    assert_eq!(
        lines,
        vec!["Created src/bad.ts", notice.as_str(), "Updated src/bad.ts"],
        "{reply}"
    );
    assert_eq!(metadata_syntax_line(&resp, "src/bad.ts"), Some(json!(5)));
    assert_eq!(
        fs::read_to_string(dir.path().join("src/bad.ts")).unwrap(),
        format!("export const zero = 0;\nexport const alsoZero = 0;\n{BROKEN_TS}")
    );
    assert!(aft.shutdown().success());
}

/// A later hunk fixes the broken new file: no notice and no metadata field.
#[test]
fn apply_patch_add_broken_then_update_fixed_has_no_syntax_line() {
    let dir = tempfile::tempdir().unwrap();
    let mut aft = configured(dir.path());
    let patch = add_then(
        "src/bad.ts",
        BROKEN_TS,
        "*** Update File: src/bad.ts\n@@\n export const second = 2;\n-export function broken( {{\n+export function fixed() {}\n",
    );

    let resp = tool_call(
        &mut aft,
        "patch-add-broken-update-fixed",
        "apply_patch",
        json!({ "patchText": patch }),
    );
    let reply = text(&resp);
    let lines: Vec<&str> = reply.lines().collect();
    assert_eq!(
        lines,
        vec!["Created src/bad.ts", "Updated src/bad.ts"],
        "{reply}"
    );
    assert_eq!(metadata_syntax_line(&resp, "src/bad.ts"), None);
    assert!(aft.shutdown().success());
}

/// A later hunk breaks a valid new file. The file did not exist before the
/// patch, so the update is kept rather than rolled back, and the notice
/// appears under the Created line.
#[test]
fn apply_patch_add_valid_then_update_broken_reports_under_created_line() {
    let dir = tempfile::tempdir().unwrap();
    let mut aft = configured(dir.path());
    let patch = add_then(
        "src/good.ts",
        VALID_TS,
        "*** Update File: src/good.ts\n@@\n export const value = 1;\n+export function broken( {{\n",
    );

    let resp = tool_call(
        &mut aft,
        "patch-add-valid-update-broken",
        "apply_patch",
        json!({ "patchText": patch }),
    );
    let reply = text(&resp);
    let notice = syntax_notice(2);
    let lines: Vec<&str> = reply.lines().collect();
    assert_eq!(
        lines,
        vec![
            "Created src/good.ts",
            notice.as_str(),
            "Updated src/good.ts"
        ],
        "{reply}"
    );
    assert_eq!(metadata_syntax_line(&resp, "src/good.ts"), Some(json!(2)));
    assert_eq!(
        fs::read_to_string(dir.path().join("src/good.ts")).unwrap(),
        "export const value = 1;\nexport function broken( {{\n"
    );
    assert!(aft.shutdown().success());
}

/// A broken new file that a later hunk deletes gets no notice.
#[test]
fn apply_patch_add_broken_then_delete_has_no_syntax_line() {
    let dir = tempfile::tempdir().unwrap();
    let mut aft = configured(dir.path());
    let patch = add_then("src/bad.ts", BROKEN_TS, "*** Delete File: src/bad.ts\n");

    let resp = tool_call(
        &mut aft,
        "patch-add-broken-delete",
        "apply_patch",
        json!({ "patchText": patch }),
    );
    let reply = text(&resp);
    let lines: Vec<&str> = reply.lines().collect();
    assert_eq!(
        lines,
        vec!["Created src/bad.ts", "Deleted src/bad.ts"],
        "{reply}"
    );
    assert_eq!(metadata_syntax_line(&resp, "src/bad.ts"), None);
    assert!(aft.shutdown().success());
}

/// A broken new file moved to a new path: the notice follows the file to the
/// line that created its final path.
#[test]
fn apply_patch_add_broken_then_move_reports_under_moved_line() {
    let dir = tempfile::tempdir().unwrap();
    let mut aft = configured(dir.path());
    let patch = add_then(
        "src/bad.ts",
        BROKEN_TS,
        "*** Update File: src/bad.ts\n*** Move to: src/moved.ts\n@@\n export const first = 1;\n+export const extra = 1;\n",
    );

    let resp = tool_call(
        &mut aft,
        "patch-add-broken-move",
        "apply_patch",
        json!({ "patchText": patch }),
    );
    let reply = text(&resp);
    let notice = syntax_notice(4);
    let lines: Vec<&str> = reply.lines().collect();
    assert_eq!(
        lines,
        vec![
            "Created src/bad.ts",
            "Updated and moved src/bad.ts → src/moved.ts",
            notice.as_str()
        ],
        "{reply}"
    );
    assert_eq!(metadata_syntax_line(&resp, "src/moved.ts"), Some(json!(4)));
    assert_eq!(metadata_syntax_line(&resp, "src/bad.ts"), None);
    assert!(!dir.path().join("src/bad.ts").exists());
    assert!(aft.shutdown().success());
}
