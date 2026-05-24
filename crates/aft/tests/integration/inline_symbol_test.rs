//! Integration tests for `inline_symbol` through the binary protocol.
//!
//! Uses temp-dir isolation (copy fixtures, mutate copies, verify results)
//! to test the full inline pipeline: argument substitution, scope conflict
//! detection, multiple-return rejection, and error paths.

use crate::helpers::{fixture_path, AftProcess};

/// Copy the `tests/fixtures/inline_symbol/` directory into a temp dir.
/// Returns `(TempDir, root_path)`.
fn setup_inline_fixture() -> (tempfile::TempDir, String) {
    let fixtures = fixture_path("inline_symbol");
    let tmp = tempfile::tempdir().expect("create temp dir");

    for entry in std::fs::read_dir(&fixtures).expect("read fixtures dir") {
        let entry = entry.expect("read entry");
        let src = entry.path();
        if src.is_file() {
            let dst = tmp.path().join(entry.file_name());
            std::fs::copy(&src, &dst).expect("copy fixture file");
        }
    }

    let root = tmp.path().display().to_string();
    (tmp, root)
}

/// Helper: configure aft with the given project root and assert success.
fn configure(aft: &mut AftProcess, root: &str) {
    let resp = aft.send(&format!(
        r#"{{"id":"cfg","command":"configure","harness":"opencode","project_root":"{}"}}"#,
        root
    ));
    assert_eq!(
        resp["success"], true,
        "configure should succeed: {:?}",
        resp
    );
}

// ---------------------------------------------------------------------------
// Success path tests
// ---------------------------------------------------------------------------

/// Basic inline: TS helper function call replaced with body.
#[test]
fn inline_symbol_basic_ts() {
    let (_tmp, root) = setup_inline_fixture();
    let mut aft = AftProcess::spawn();
    configure(&mut aft, &root);

    let file = format!("{}/sample.ts", root);

    // Inline `add` at line 10 (const result = add(x, y))
    let resp = aft.send(&format!(
        r#"{{"id":"1","command":"inline_symbol","file":"{}","symbol":"add","call_site_line":11}}"#,
        file
    ));

    assert_eq!(resp["success"], true, "inline should succeed: {:?}", resp);
    assert_eq!(resp["symbol"], "add");
    assert_eq!(resp["call_context"], "assignment");
    assert!(
        resp["substitutions"].as_u64().unwrap() > 0,
        "should have substitutions"
    );

    // Verify call was replaced
    let content = std::fs::read_to_string(&file).expect("read file");
    assert!(
        !content.contains("add(x, y)"),
        "call should be replaced:\n{}",
        content
    );

    aft.shutdown();
}

/// Expression-body arrow function: implicit return inlined correctly.
#[test]
fn inline_symbol_expression_body() {
    let (_tmp, root) = setup_inline_fixture();
    let mut aft = AftProcess::spawn();
    configure(&mut aft, &root);

    let file = format!("{}/sample.ts", root);

    // Inline `double` at line 17 (const val = double(5)) — 0-indexed
    let resp = aft.send(&format!(
        r#"{{"id":"1","command":"inline_symbol","file":"{}","symbol":"double","call_site_line":18}}"#,
        file
    ));

    assert_eq!(
        resp["success"], true,
        "inline expression body should succeed: {:?}",
        resp
    );
    assert_eq!(resp["symbol"], "double");

    // Verify call was replaced
    let content = std::fs::read_to_string(&file).expect("read file");
    assert!(
        !content.contains("double(5)"),
        "call should be replaced:\n{}",
        content
    );

    aft.shutdown();
}

/// Python inline.
#[test]
fn inline_symbol_python() {
    let (_tmp, root) = setup_inline_fixture();
    let mut aft = AftProcess::spawn();
    configure(&mut aft, &root);

    let file = format!("{}/sample.py", root);

    // Inline `add` at line 9 (result = add(x, y)) — 0-indexed
    let resp = aft.send(&format!(
        r#"{{"id":"1","command":"inline_symbol","file":"{}","symbol":"add","call_site_line":10}}"#,
        file
    ));

    assert_eq!(
        resp["success"], true,
        "python inline should succeed: {:?}",
        resp
    );
    assert_eq!(resp["symbol"], "add");

    // Verify call was replaced
    let content = std::fs::read_to_string(&file).expect("read file");
    assert!(
        !content.contains("add(x, y)"),
        "call should be replaced:\n{}",
        content
    );

    aft.shutdown();
}

// ---------------------------------------------------------------------------
// Error path tests
// ---------------------------------------------------------------------------

/// Multiple-returns error: function with 2 returns rejected.
#[test]
fn inline_symbol_multiple_returns() {
    let (_tmp, root) = setup_inline_fixture();
    let mut aft = AftProcess::spawn();
    configure(&mut aft, &root);

    let file = format!("{}/sample_multi.ts", root);

    // multiReturn has 2 return statements
    let resp = aft.send(&format!(
        r#"{{"id":"1","command":"inline_symbol","file":"{}","symbol":"multiReturn","call_site_line":9}}"#,
        file
    ));

    assert_eq!(resp["success"], false, "should fail: {:?}", resp);
    assert_eq!(resp["code"], "multiple_returns");
    assert!(
        resp["return_count"].as_u64().unwrap() >= 2,
        "should report return count"
    );

    aft.shutdown();
}

/// Scope-conflict error: response includes conflicting variable names and suggestions.
#[test]
fn inline_symbol_scope_conflict() {
    let (_tmp, root) = setup_inline_fixture();
    let mut aft = AftProcess::spawn();
    configure(&mut aft, &root);

    let file = format!("{}/sample_conflict.ts", root);

    // compute() body declares `temp` and `result`, both exist at call site
    let resp = aft.send(&format!(
        r#"{{"id":"1","command":"inline_symbol","file":"{}","symbol":"compute","call_site_line":9}}"#,
        file
    ));

    assert_eq!(
        resp["success"], false,
        "should fail with scope_conflict: {:?}",
        resp
    );
    assert_eq!(resp["code"], "scope_conflict");

    let conflicting = resp["conflicting_names"]
        .as_array()
        .expect("conflicting_names array");
    assert!(
        !conflicting.is_empty(),
        "should report at least one conflicting name: {:?}",
        conflicting
    );

    let suggestions = resp["suggestions"].as_array().expect("suggestions array");
    assert!(
        !suggestions.is_empty(),
        "should include rename suggestions: {:?}",
        suggestions
    );
    // Each suggestion should have original and suggested fields
    for s in suggestions {
        assert!(
            s["original"].as_str().is_some(),
            "suggestion should have 'original': {:?}",
            s
        );
        assert!(
            s["suggested"].as_str().is_some(),
            "suggestion should have 'suggested': {:?}",
            s
        );
    }

    aft.shutdown();
}

/// Inline preserves the original line indentation. Without the leading-
/// whitespace expansion fix, the replacement text's indent prefix would be
/// inserted AFTER the original indent on the line, doubling it (e.g. a
/// 2-space-indented line became 4-space-indented).
#[test]
fn inline_symbol_preserves_call_site_indent() {
    let tmp = tempfile::tempdir().expect("temp dir");
    let file = tmp.path().join("indent.ts");
    std::fs::write(
        &file,
        "function helper(x: number): number {\n  return x * 2;\n}\n\nexport function main() {\n  const result = helper(5);\n  console.log(result);\n}\n",
    )
    .expect("write fixture");

    let mut aft = AftProcess::spawn();
    configure(&mut aft, &tmp.path().display().to_string());

    let resp = aft.send(&format!(
        r#"{{"id":"1","command":"inline_symbol","file":"{}","symbol":"helper","call_site_line":6}}"#,
        file.display()
    ));
    assert_eq!(resp["success"], true, "inline should succeed: {:?}", resp);

    let content = std::fs::read_to_string(&file).expect("read file");
    // The replacement line must keep its original 2-space indent. If the
    // bug regressed, this would be 4 spaces.
    assert!(
        content.contains("\n  const result = 5 * 2;\n"),
        "expected 2-space indent on inlined line, got:\n{}",
        content
    );
    assert!(
        !content.contains("    const result"),
        "indent should not be doubled, got:\n{}",
        content
    );

    aft.shutdown();
}

/// Parameter substitution should rewrite only references bound to the inlined
/// function's parameters. Nested arrow parameters that shadow the same name must
/// stay untouched.
#[test]
fn inline_symbol_does_not_substitute_shadowed_arrow_parameter() {
    let tmp = tempfile::tempdir().expect("temp dir");
    let file = tmp.path().join("shadowed_arrow.ts");
    std::fs::write(
        &file,
        "function f(x: number): number {\n  return x + items.map(x => x + 1)[0];\n}\n\nconst items = [1, 2];\n\nfunction main() {\n  const result = f(5);\n}\n",
    )
    .expect("write fixture");

    let mut aft = AftProcess::spawn();
    configure(&mut aft, &tmp.path().display().to_string());

    let resp = aft.send(&format!(
        r#"{{"id":"1","command":"inline_symbol","file":"{}","symbol":"f","call_site_line":8}}"#,
        file.display()
    ));
    assert_eq!(resp["success"], true, "inline should succeed: {:?}", resp);

    let content = std::fs::read_to_string(&file).expect("read file");
    let expected = "  const result = 5 + items.map(x => x + 1)[0];";
    assert!(
        content.contains(expected),
        "outer `x` should be substituted while nested arrow `x` remains:\n{}",
        content
    );
    assert!(
        !content.contains("items.map(5 => 5 + 1)"),
        "nested arrow parameter must not be substituted:\n{}",
        content
    );

    aft.shutdown();
}

/// `inline_symbol` should accept a target call_site_line that points at the
/// first line of a multiline call (e.g. `helper(\n  a,\n  b,\n)`), rather than
/// rejecting it as call_not_found.
#[test]
fn inline_symbol_matches_multiline_call_starting_on_target_line() {
    let tmp = tempfile::tempdir().expect("temp dir");
    let file = tmp.path().join("multiline.ts");
    std::fs::write(
        &file,
        "function helper(a: number, b: number): number {\n  return a + b;\n}\n\nexport function main() {\n  const result = helper(\n    1,\n    2,\n  );\n  console.log(result);\n}\n",
    )
    .expect("write fixture");

    let mut aft = AftProcess::spawn();
    configure(&mut aft, &tmp.path().display().to_string());

    let resp = aft.send(&format!(
        r#"{{"id":"multiline","command":"inline_symbol","file":"{}","symbol":"helper","call_site_line":6}}"#,
        file.display()
    ));
    assert_eq!(
        resp["success"], true,
        "inline should match multiline call by start line: {:?}",
        resp
    );

    let content = std::fs::read_to_string(&file).expect("read file");
    assert!(
        !content.contains("helper(\n"),
        "multiline call should be replaced:\n{}",
        content
    );
    assert!(
        content.contains("\n  const result = 1 + 2;\n"),
        "expected inlined expression, got:\n{}",
        content
    );

    aft.shutdown();
}
