use std::fs;

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

use serde_json::json;

use super::helpers::AftProcess;

fn configure_with_backup_disabled(aft: &mut AftProcess, root: &std::path::Path) {
    let resp = aft.send(
        &json!({
            "id": "cfg",
            "command": "configure",
            "harness": "opencode",
            "project_root": root,
            "config": [{
                "tier": "user",
                "source": "test",
                "doc": r#"{ "backup": { "enabled": false } }"#,
            }],
        })
        .to_string(),
    );
    assert_eq!(resp["success"], true, "configure should succeed: {resp:?}");
}

#[test]
fn edit_match_glob_rolls_back_prior_files_when_later_write_fails() {
    let dir = crate::test_helpers::tempdir();
    let root = dir.path();
    let a = root.join("a.ts");
    let b = root.join("b.ts");
    let c = root.join("c.ts");

    fs::write(&a, "const a = \"OLD\";\n").unwrap();
    fs::write(&b, "const b = \"OLD\";\n").unwrap();
    fs::write(&c, "const c = \"OLD\";\n").unwrap();

    let mut readonly = fs::metadata(&b).unwrap().permissions();
    readonly.set_readonly(true);
    fs::set_permissions(&b, readonly).unwrap();

    let mut aft = AftProcess::spawn();
    configure_with_backup_disabled(&mut aft, root);
    let req = json!({
        "id": "glob-rollback-write-failure",
        "command": "edit_match",
        "file": format!("{}/*.ts", root.display()),
        "match": "OLD",
        "replacement": "NEW"
    });
    let resp = aft.send(&req.to_string());

    make_writable(&b);

    assert_eq!(resp["success"], false, "glob edit should fail: {resp:?}");
    assert_eq!(resp["code"], "write_error");
    assert_eq!(fs::read_to_string(&a).unwrap(), "const a = \"OLD\";\n");
    assert_eq!(fs::read_to_string(&b).unwrap(), "const b = \"OLD\";\n");
    assert_eq!(fs::read_to_string(&c).unwrap(), "const c = \"OLD\";\n");

    let status = aft.shutdown();
    assert!(status.success());
}

#[test]
fn edit_match_glob_rolls_back_when_any_file_becomes_syntax_invalid() {
    // Glob edit_match is atomic w.r.t. syntax: if any file ends up syntax-
    // invalid after the replacement, the whole batch rolls back to the
    // pre-edit checkpoint. Previously this code reported per-file
    // `syntax_valid: false` and left edits applied, which silently broke
    // the project. The new contract: agent gets a clear `syntax_invalid`
    // error and the working tree is unchanged.
    let dir = crate::test_helpers::tempdir();
    let root = dir.path();
    let a = root.join("a.ts");
    let b = root.join("b.ts");
    let c = root.join("c.ts");

    let original_a = "const a = 1; // TARGET\n";
    let original_b = "const b = \"TARGET\";\n";
    let original_c = "const c = TARGET;\n";
    fs::write(&a, original_a).unwrap();
    fs::write(&b, original_b).unwrap();
    fs::write(&c, original_c).unwrap();

    let mut aft = AftProcess::spawn();
    configure_with_backup_disabled(&mut aft, root);
    let req = json!({
        "id": "glob-rollback-syntax-failure",
        "command": "edit_match",
        "file": format!("{}/*.ts", root.display()),
        "match": "TARGET",
        "replacement": "{;"
    });
    let resp = aft.send(&req.to_string());

    assert_eq!(
        resp["success"], false,
        "glob edit must fail when any file becomes syntax-invalid: {resp:?}"
    );
    assert_eq!(resp["code"], "syntax_invalid", "wrong error code: {resp:?}");
    let msg = resp["message"].as_str().expect("message");
    assert!(
        msg.contains("rolled back"),
        "error message should mention rollback: {msg}"
    );

    // All three files must be unchanged from their original contents.
    assert_eq!(fs::read_to_string(&a).unwrap(), original_a);
    assert_eq!(fs::read_to_string(&b).unwrap(), original_b);
    assert_eq!(fs::read_to_string(&c).unwrap(), original_c);

    let undo = aft.send(&json!({"id": "undo-after-glob-rollback", "command": "undo"}).to_string());
    assert_eq!(
        undo["success"], false,
        "glob rollback should discard operation undo entries: {undo:?}"
    );
    assert_eq!(undo["code"], "backups_disabled");

    let status = aft.shutdown();
    assert!(status.success());
}

#[test]
fn edit_match_glob_succeeds_when_all_files_remain_syntax_valid() {
    // Companion to the rollback test: if the replacement keeps every file
    // syntax-valid, the batch commits normally and per-file results report
    // syntax_valid: true. This guards against an over-eager rollback that
    // would block legitimate batch edits.
    let dir = crate::test_helpers::tempdir();
    let root = dir.path();
    let a = root.join("a.ts");
    let b = root.join("b.ts");

    fs::write(&a, "const a = TARGET;\n").unwrap();
    fs::write(&b, "const b = TARGET;\n").unwrap();

    let mut aft = AftProcess::spawn();
    let req = json!({
        "id": "glob-syntax-clean",
        "command": "edit_match",
        "file": format!("{}/*.ts", root.display()),
        "match": "TARGET",
        "replacement": "42"
    });
    let resp = aft.send(&req.to_string());

    assert_eq!(resp["success"], true, "expected success: {resp:?}");
    let files = resp["files"].as_array().expect("files array");
    assert_eq!(files.len(), 2);
    for file in files {
        assert_eq!(file["syntax_valid"], true, "file should be valid: {file:?}");
    }
    assert_eq!(fs::read_to_string(&a).unwrap(), "const a = 42;\n");
    assert_eq!(fs::read_to_string(&b).unwrap(), "const b = 42;\n");

    let status = aft.shutdown();
    assert!(status.success());
}

#[test]
fn edit_match_omits_syntax_valid_when_language_unsupported() {
    let dir = crate::test_helpers::tempdir();
    let file = dir.path().join("notes.unusual_extension");
    fs::write(&file, "hello old world\n").unwrap();

    let mut aft = AftProcess::spawn();
    let req = json!({
        "id": "unsupported-syntax-valid",
        "command": "edit_match",
        "file": file,
        "match": "old",
        "replacement": "new"
    });
    let resp = aft.send(&req.to_string());

    assert_eq!(resp["success"], true, "expected edit success: {resp:?}");
    assert!(
        resp.get("syntax_valid").is_none(),
        "syntax_valid must be absent when validation could not run: {resp:?}"
    );
    assert_eq!(
        fs::read_to_string(dir.path().join("notes.unusual_extension")).unwrap(),
        "hello new world\n"
    );

    let status = aft.shutdown();
    assert!(status.success());
}

// Regression for #83: a fuzzy (rstrip) match must not drop the trailing
// newline of the matched range. The fuzzy line matcher includes the newline
// after the last matched line in its byte range; applying the replacement
// verbatim used to merge the last replaced line with the following line.
#[test]
fn edit_match_table_fuzzy_preserves_padding_but_exact_removes_requested_pipe() {
    let dir = crate::test_helpers::tempdir();
    let file = dir.path().join("table.md");
    fs::write(&file, "  | For  a task | b |  \n  | For  a task | b |  \n").unwrap();
    let mut aft = AftProcess::spawn();
    let req = json!({
        "id": "fuzzy-table", "command": "edit_match", "file": file,
        "match": "| For a task | b |", "replacement": "| Changed | b |",
        "replace_all": true
    });
    let resp = aft.send(&req.to_string());
    assert_eq!(resp["success"], true, "{resp:?}");
    assert_eq!(resp["fuzzy_match"]["pass"], 5);
    assert_eq!(
        fs::read_to_string(&file).unwrap(),
        "  | Changed | b |  \n  | Changed | b |  \n"
    );
    fs::write(&file, "  | For  a task | b |  \n").unwrap();
    let fuzzy_removal = aft.send(
        &json!({
            "id": "fuzzy-remove-pipe", "command": "edit_match", "file": file,
            "match": "| For a task | b |", "replacement": "Changed | b |"
        })
        .to_string(),
    );
    assert_eq!(fuzzy_removal["success"], true, "{fuzzy_removal:?}");
    assert_eq!(fs::read_to_string(&file).unwrap(), "  Changed | b |  \n");
    fs::write(&file, "| For a task | b |\n").unwrap();
    let exact = aft.send(
        &json!({
            "id": "exact-table", "command": "edit_match", "file": file,
            "match": "| For a task", "replacement": "Changed"
        })
        .to_string(),
    );
    assert_eq!(exact["success"], true, "{exact:?}");
    assert_eq!(fs::read_to_string(&file).unwrap(), "Changed | b |\n");
    assert!(aft.shutdown().success());
}

#[test]
fn edit_match_fuzzy_preserves_trailing_newline() {
    let dir = crate::test_helpers::tempdir();
    let file = dir.path().join("greet.js");
    // Trailing spaces after two lines force the rstrip fuzzy pass (pass 2),
    // since the clean `match` below won't match byte-for-byte.
    fs::write(
        &file,
        "function greet(name) {\n  const message = \"Hello, \" + name;   \n  console.log(message);   \n  return message;\n}\n",
    )
    .unwrap();

    let mut aft = AftProcess::spawn();
    let req = json!({
        "id": "fuzzy-trailing-newline",
        "command": "edit_match",
        "file": file,
        // No trailing spaces in the match (forces fuzzy) and no trailing newline
        // in the replacement (the bug condition).
        "match": "  const message = \"Hello, \" + name;\n  console.log(message);\n  return message;",
        "replacement": "  const msg = \"Hello, \" + name;\n  console.log(msg);\n  return msg;"
    });
    let resp = aft.send(&req.to_string());
    assert_eq!(resp["success"], true, "expected edit success: {resp:?}");

    let after = fs::read_to_string(&file).unwrap();
    // The newline before `}` must survive — `return msg;` and `}` stay on
    // separate lines instead of merging into `  return msg;}`.
    assert!(
        after.contains("  return msg;\n}"),
        "trailing newline before closing brace was dropped: {after:?}"
    );
    assert!(
        !after.contains("return msg;}"),
        "last replaced line merged with the next line: {after:?}"
    );

    let status = aft.shutdown();
    assert!(status.success());
}

#[test]
fn edit_match_replace_all_rejects_overlapping_fuzzy_matches() {
    let dir = crate::test_helpers::tempdir();
    let file = dir.path().join("over.js");
    // Trailing spaces defeat the exact (pass 1) matcher and force the fuzzy
    // line pass, which steps line-by-line. A 2-line needle over three identical
    // lines yields two OVERLAPPING matches (lines 0-1 and lines 1-2). Applying
    // them in reverse would silently corrupt the file, so replace_all must
    // reject with `overlapping_edits` instead.
    let original = "row \nrow \nrow \n";
    fs::write(&file, original).unwrap();

    let mut aft = AftProcess::spawn();
    let req = json!({
        "id": "replace-all-overlap",
        "command": "edit_match",
        "file": file,
        "match": "row\nrow",
        "replacement": "X\nX",
        "replace_all": true
    });
    let resp = aft.send(&req.to_string());
    assert_eq!(
        resp["success"], false,
        "overlapping replace_all must fail cleanly: {resp:?}"
    );
    assert_eq!(
        resp["code"], "overlapping_edits",
        "wrong error code: {resp:?}"
    );
    // File must be untouched — no partial/corrupt write.
    assert_eq!(
        fs::read_to_string(&file).unwrap(),
        original,
        "file was modified despite the overlap rejection"
    );

    let status = aft.shutdown();
    assert!(status.success());
}

#[test]
fn edit_match_reflow_replaces_formatter_split() {
    let dir = crate::test_helpers::tempdir();
    let file = dir.path().join("split.js");
    let original =
        "function demo() {\n  const value = alpha +\n    beta +\n    gamma;\n  return value;\n}\n";
    fs::write(&file, original).unwrap();

    let mut aft = AftProcess::spawn();
    let req = json!({
        "id": "reflow-split",
        "command": "edit_match",
        "file": file,
        "match": "  const value = alpha + beta + gamma;",
        "replacement": "  const value = alpha + beta + delta;"
    });
    let resp = aft.send(&req.to_string());

    assert_eq!(resp["success"], true, "expected edit success: {resp:?}");
    assert_eq!(
        fs::read_to_string(dir.path().join("split.js")).unwrap(),
        "function demo() {\n  const value = alpha + beta + delta;\n  return value;\n}\n"
    );

    let status = aft.shutdown();
    assert!(status.success());
}

#[test]
fn edit_match_reflow_replaces_formatter_join() {
    let dir = crate::test_helpers::tempdir();
    let file = dir.path().join("join.js");
    let original = "function demo() {\n  const value = alpha + beta + gamma;\n}\n";
    fs::write(&file, original).unwrap();

    let mut aft = AftProcess::spawn();
    let req = json!({
        "id": "reflow-join",
        "command": "edit_match",
        "file": file,
        "match": "  const value = alpha +\n    beta +\n    gamma;",
        "replacement": "  const value = alpha +\n    beta +\n    delta;"
    });
    let resp = aft.send(&req.to_string());

    assert_eq!(resp["success"], true, "expected edit success: {resp:?}");
    assert_eq!(
        fs::read_to_string(dir.path().join("join.js")).unwrap(),
        "function demo() {\n  const value = alpha +\n    beta +\n    delta;\n}\n"
    );

    let status = aft.shutdown();
    assert!(status.success());
}

#[test]
fn edit_match_reflow_ambiguous_does_not_edit() {
    let dir = crate::test_helpers::tempdir();
    let file = dir.path().join("ambiguous.js");
    let original =
        "const value = alpha +\n  beta +\n  gamma;\n\nconst value = alpha +\n  beta +\n  gamma;\n";
    fs::write(&file, original).unwrap();

    let mut aft = AftProcess::spawn();
    let req = json!({
        "id": "reflow-ambiguous",
        "command": "edit_match",
        "file": file,
        "match": "const value = alpha + beta + gamma;",
        "replacement": "const value = alpha + beta + delta;"
    });
    let resp = aft.send(&req.to_string());

    assert_eq!(
        resp["success"], false,
        "ambiguous edit should fail: {resp:?}"
    );
    assert_eq!(
        resp["code"], "ambiguous_match",
        "wrong error code: {resp:?}"
    );
    assert_eq!(
        fs::read_to_string(dir.path().join("ambiguous.js")).unwrap(),
        original
    );

    let status = aft.shutdown();
    assert!(status.success());
}

#[test]
fn edit_match_near_miss_reports_candidate_and_divergence() {
    let dir = crate::test_helpers::tempdir();
    let file = dir.path().join("near_miss_detail.js");
    let original =
        "fn calculate() {\n    let first = 1;\n    let actual = 2;\n    let last = 3;\n}\n";
    fs::write(&file, original).unwrap();

    let mut aft = AftProcess::spawn();
    let req = json!({
        "id": "near-miss-detail",
        "command": "edit_match",
        "file": file,
        "match": "fn calculate() {\n    let first = 1;\n    let expected = 2;\n    let last = 3;\n}",
        "replacement": "fn calculate() {\n    let first = 1;\n    let changed = 2;\n    let last = 3;\n}"
    });
    let resp = aft.send(&req.to_string());

    assert_eq!(resp["success"], false, "near miss should fail: {resp:?}");
    assert_eq!(resp["code"], "match_not_found");
    let message = resp["message"].as_str().unwrap_or_default();
    assert!(message.starts_with("edit_match: 'fn calculate()"));
    assert!(message.contains("Nearest candidate at lines 1-5"));
    assert!(message.contains("- expected:     let expected = 2;"));
    assert!(message.contains("+ actual:     let actual = 2;"));
    assert_eq!(fs::read_to_string(&file).unwrap(), original);

    let status = aft.shutdown();
    assert!(status.success());
}

#[test]
fn edit_match_diagnostics_flow_through_tool_call_ndjson() {
    let dir = crate::test_helpers::tempdir();
    let near_file = dir.path().join("tool_call_near.rs");
    let ambiguous_file = dir.path().join("tool_call_ambiguous.rs");
    fs::write(
        &near_file,
        "fn calculate() {\n    let first = 1;\n    let actual = 2;\n    let last = 3;\n}\n",
    )
    .unwrap();
    fs::write(&ambiguous_file, "same\nother\nsame\n").unwrap();

    let mut aft = AftProcess::spawn();
    let near_resp = aft.send(
        &json!({
            "id": "tool-call-near",
            "command": "tool_call",
            "name": "edit_match",
            "arguments": {
                "file": near_file,
                "match": "fn calculate() {\n    let first = 1;\n    let expected = 2;\n    let last = 3;\n}",
                "replacement": "unused"
            }
        })
        .to_string(),
    );
    assert_eq!(
        near_resp["success"], false,
        "near tool_call should fail: {near_resp:?}"
    );
    assert!(near_resp["message"]
        .as_str()
        .unwrap_or_default()
        .contains("First divergence:\n- expected:"));
    assert!(near_resp["text"].is_string());

    let ambiguous_resp = aft.send(
        &json!({
            "id": "tool-call-ambiguous",
            "command": "tool_call",
            "name": "edit_match",
            "arguments": {
                "file": ambiguous_file,
                "match": "same",
                "replacement": "changed"
            }
        })
        .to_string(),
    );
    assert_eq!(
        ambiguous_resp["success"], false,
        "ambiguous tool_call should fail: {ambiguous_resp:?}"
    );
    let ambiguous_message = ambiguous_resp["message"].as_str().unwrap_or_default();
    assert!(ambiguous_message.contains("2 occurrences: #1 at line 1, #2 at line 3"));
    assert!(ambiguous_resp["text"].is_string());

    let status = aft.shutdown();
    assert!(status.success());
}

#[test]
fn edit_match_reflow_near_miss_does_not_edit() {
    let dir = crate::test_helpers::tempdir();
    let file = dir.path().join("near_miss.js");
    let original = "const value = alpha +\n  beta +\n  gamma;\n";
    fs::write(&file, original).unwrap();

    let mut aft = AftProcess::spawn();
    let req = json!({
        "id": "reflow-near-miss",
        "command": "edit_match",
        "file": file,
        "match": "const value = alpha + beta + delta;",
        "replacement": "const value = alpha + beta + epsilon;"
    });
    let resp = aft.send(&req.to_string());

    assert_eq!(resp["success"], false, "near miss should fail: {resp:?}");
    assert_eq!(
        resp["code"], "match_not_found",
        "wrong error code: {resp:?}"
    );
    let message = resp["message"].as_str().unwrap_or_default();
    assert!(message.starts_with("edit_match: 'const value = alpha + beta + delta;' not found in "));
    assert!(message.contains("file has 3 lines"));
    assert!(!message.contains("Nearest candidate"));
    assert_eq!(
        fs::read_to_string(dir.path().join("near_miss.js")).unwrap(),
        original
    );

    let status = aft.shutdown();
    assert!(status.success());
}

#[cfg(unix)]
fn make_writable(path: &std::path::Path) {
    let mut permissions = fs::metadata(path).unwrap().permissions();
    permissions.set_mode(0o644);
    fs::set_permissions(path, permissions).unwrap();
}

#[cfg(windows)]
fn make_writable(path: &std::path::Path) {
    let mut permissions = fs::metadata(path).unwrap().permissions();
    // On Windows this clears the read-only file attribute, which is exactly the
    // intent here; the clippy lint targets the Unix world-writable footgun that
    // does not apply to this windows-only helper.
    #[allow(clippy::permissions_set_readonly_false)]
    permissions.set_readonly(false);
    fs::set_permissions(path, permissions).unwrap();
}
