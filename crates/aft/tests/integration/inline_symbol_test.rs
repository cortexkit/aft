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
        r#"{{"id":"cfg","command":"configure","harness":"opencode","project_root":{}}}"#,
        crate::helpers::json_string(&root)
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

/// Inline an exported TS function. After the v0.30.x parser fix that
/// included the `export` keyword in symbol ranges (commit 57d48d9), the
/// inline path was unable to descend from `export_statement` to the inner
/// `function_declaration` and returned a `symbol_not_found` error.
#[test]
fn inline_symbol_exported_function() {
    let tmp = tempfile::tempdir().expect("create temp dir");
    let file = tmp.path().join("exported.ts");
    std::fs::write(
        &file,
        "export function double(n: number): number {\n  return n * 2;\n}\n\nexport function useIt(x: number): number {\n  return double(x);\n}\n",
    )
    .expect("write test file");

    let root = tmp.path().display().to_string();
    let mut aft = AftProcess::spawn();
    configure(&mut aft, &root);

    let resp = aft.send(&format!(
        r#"{{"id":"1","command":"inline_symbol","file":{},"symbol":"double","call_site_line":6}}"#,
        crate::helpers::json_string(&file.display())
    ));

    assert_eq!(
        resp["success"], true,
        "inline of exported function should succeed: {:?}",
        resp
    );
    assert_eq!(resp["symbol"], "double");

    let content = std::fs::read_to_string(&file).expect("read file");
    assert!(
        !content.contains("double(x)"),
        "call should be replaced:\n{}",
        content
    );
    assert!(
        content.contains("x * 2"),
        "body should be substituted with argument:\n{}",
        content
    );

    aft.shutdown();
}

/// Basic inline: TS helper function call replaced with body.
#[test]
fn inline_symbol_basic_ts() {
    let (_tmp, root) = setup_inline_fixture();
    let mut aft = AftProcess::spawn();
    configure(&mut aft, &root);

    let file = format!("{}/sample.ts", root);

    // Inline `add` at line 10 (const result = add(x, y))
    let resp = aft.send(&format!(
        r#"{{"id":"1","command":"inline_symbol","file":{},"symbol":"add","call_site_line":11}}"#,
        crate::helpers::json_string(&file)
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

#[test]
fn inline_symbol_rejects_call_site_line_above_u32_range() {
    let (_tmp, root) = setup_inline_fixture();
    let mut aft = AftProcess::spawn();
    configure(&mut aft, &root);

    let file = format!("{root}/sample.ts");
    let original = std::fs::read_to_string(&file).expect("read fixture");
    let resp = aft.send(&format!(
        r#"{{"id":"line-overflow","command":"inline_symbol","file":{},"symbol":"add","call_site_line":4294967307}}"#,
        crate::helpers::json_string(&file)
    ));

    assert_eq!(resp["success"], false, "oversized line must fail: {resp:?}");
    assert_eq!(resp["code"], "invalid_request");
    assert_eq!(
        std::fs::read_to_string(&file).expect("read fixture after request"),
        original,
        "an oversized line must not wrap to line 11 and edit the file"
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
        r#"{{"id":"1","command":"inline_symbol","file":{},"symbol":"double","call_site_line":18}}"#,
        crate::helpers::json_string(&file)
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
        r#"{{"id":"1","command":"inline_symbol","file":{},"symbol":"add","call_site_line":10}}"#,
        crate::helpers::json_string(&file)
    ));

    assert_eq!(
        resp["success"], true,
        "python inline should succeed: {:?}",
        resp
    );
    assert_eq!(resp["symbol"], "add");

    // Verify call was replaced, at the call site's indentation: the body's
    // second line must not keep the function body's own indentation on top
    // of the call site's (`        result = total` is an IndentationError).
    // A Windows checkout gives the fixture CRLF endings, so the text is
    // compared with endings normalized and the endings are checked apart.
    let content = std::fs::read_to_string(&file).expect("read file");
    assert!(
        !content.contains("add(x, y)"),
        "call should be replaced:\n{}",
        content
    );
    assert!(
        content.replace("\r\n", "\n").contains(
            "def main():\n    x = 10\n    y = 20\n    total = x + y\n    result = total\n    print(result)\n"
        ),
        "inlined body should sit at the call site's indentation:\n{}",
        content
    );
    assert_consistent_line_endings(&content);

    aft.shutdown();
}

/// Every line ends the same way: all CRLF when the file has any CRLF, all
/// LF otherwise. An inline that wrote LF lines into a CRLF file would mix
/// them.
fn assert_consistent_line_endings(content: &str) {
    let crlf = content.matches("\r\n").count();
    let lf = content.matches('\n').count();
    assert!(
        crlf == 0 || crlf == lf,
        "line endings are mixed ({crlf} CRLF of {lf} lines): {content:?}"
    );
}

/// The same inline into a file with CRLF endings, written explicitly so the
/// case is covered on every platform: the inlined lines keep CRLF.
#[test]
fn inline_symbol_python_keeps_crlf_line_endings() {
    let source = "def add(a, b):\r\n    total = a + b\r\n    return total\r\n\r\ndef main():\r\n    x = 10\r\n    y = 20\r\n    result = add(x, y)\r\n    print(result)\r\n";
    let (resp, content) = inline_in_fresh_file("crlf.py", source, "add", 8);
    assert_eq!(
        resp["success"], true,
        "CRLF python inline should succeed: {resp:?}"
    );
    assert!(
        content.contains("    total = x + y\r\n    result = total\r\n    print(result)\r\n"),
        "inlined lines should keep CRLF: {content:?}"
    );
    assert_consistent_line_endings(&content);
}

/// A Python body with a nested block keeps its relative nesting when it is
/// inlined into a call site that is itself nested.
#[test]
fn inline_symbol_python_nested_body_keeps_relative_indentation() {
    let tmp = tempfile::tempdir().expect("create temp dir");
    let file = tmp.path().join("nested.py");
    std::fs::write(
        &file,
        "def pick(a):\n    if a > 1:\n        print(a)\n    return a\n\ndef main():\n    v = 5\n    if v:\n        r = pick(v)\n        print(r)\n",
    )
    .expect("write test file");
    let root = tmp.path().display().to_string();
    let mut aft = AftProcess::spawn();
    configure(&mut aft, &root);

    let resp = aft.send(&format!(
        r#"{{"id":"1","command":"inline_symbol","file":{},"symbol":"pick","call_site_line":9}}"#,
        crate::helpers::json_string(&file.display())
    ));
    assert_eq!(
        resp["success"], true,
        "nested python inline should succeed: {resp:?}"
    );
    let content = std::fs::read_to_string(&file).expect("read file");
    assert!(
        content.contains(
            "    if v:\n        if v > 1:\n            print(v)\n        r = v\n        print(r)\n"
        ),
        "inlined body should keep its nesting at the call site's depth:\n{content}"
    );

    aft.shutdown();
}

/// Inline `symbol` at 1-based `line` of a fresh file holding `source`, and
/// return the response and the file afterwards.
fn inline_in_fresh_file(
    name: &str,
    source: &str,
    symbol: &str,
    line: u32,
) -> (serde_json::Value, String) {
    let tmp = tempfile::tempdir().expect("create temp dir");
    let file = tmp.path().join(name);
    std::fs::write(&file, source).expect("write test file");
    let root = tmp.path().display().to_string();
    let mut aft = AftProcess::spawn();
    configure(&mut aft, &root);
    let resp = aft.send(&format!(
        r#"{{"id":"1","command":"inline_symbol","file":{},"symbol":"{}","call_site_line":{}}}"#,
        crate::helpers::json_string(&file.display()),
        symbol,
        line
    ));
    let content = std::fs::read_to_string(&file).expect("read file");
    aft.shutdown();
    (resp, content)
}

/// Removing the body's indentation must not remove indentation inside a
/// multi-line string: those spaces are part of the string's value, and the
/// result would still parse, so no syntax check would catch the change.
#[test]
fn inline_symbol_keeps_multiline_string_values() {
    let cases = [
        (
            "python triple-quoted string",
            "f.py",
            "def f():\n    return '''hello\n    world'''\n\nresult = f()\n",
            5,
            "result = '''hello\n    world'''\n",
        ),
        (
            "typescript template literal",
            "f.ts",
            "function f(): string {\n  return `a\n  b`;\n}\n\nconst r = f();\n",
            6,
            "const r = `a\n  b`;\n",
        ),
        (
            "python docstring in a nested body",
            "g.py",
            "def g():\n    \"\"\"Doc\n    more\n    \"\"\"\n    print(1)\n\ndef main():\n    if True:\n        g()\n",
            9,
            "    if True:\n        \"\"\"Doc\n    more\n    \"\"\"\n        print(1)\n",
        ),
    ];
    let mut failures = Vec::new();
    for (label, name, source, line, expected) in cases {
        let (resp, content) = inline_in_fresh_file(
            name,
            source,
            if name.starts_with('g') { "g" } else { "f" },
            line,
        );
        if resp["success"] != true || !content.contains(expected) {
            failures.push(format!(
                "{label}: expected the file to contain {expected:?}; response {resp:?}; file {content:?}"
            ));
        }
    }
    assert!(failures.is_empty(), "\n{}", failures.join("\n\n"));
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
        r#"{{"id":"1","command":"inline_symbol","file":{},"symbol":"multiReturn","call_site_line":9}}"#,
        crate::helpers::json_string(&file)
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
        r#"{{"id":"1","command":"inline_symbol","file":{},"symbol":"compute","call_site_line":9}}"#,
        crate::helpers::json_string(&file)
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
        r#"{{"id":"1","command":"inline_symbol","file":{},"symbol":"helper","call_site_line":6}}"#,
        crate::helpers::json_string(&file.display())
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
        r#"{{"id":"1","command":"inline_symbol","file":{},"symbol":"f","call_site_line":8}}"#,
        crate::helpers::json_string(&file.display())
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
        r#"{{"id":"multiline","command":"inline_symbol","file":{},"symbol":"helper","call_site_line":6}}"#,
        crate::helpers::json_string(&file.display())
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

#[test]
fn inline_symbol_preserves_nested_multiline_indentation() {
    let tmp = tempfile::tempdir().expect("temp dir");
    let file = tmp.path().join("nested_indent.ts");
    std::fs::write(
        &file,
        r#"function helper(items: number[]): void {
  for (const item of items) {
    if (item > 0) {
      console.log(item);
    }
  }
}

const values = [1, 2];

export function main() {
  helper(values);
}
"#,
    )
    .expect("write fixture");

    let mut aft = AftProcess::spawn();
    configure(&mut aft, &tmp.path().display().to_string());

    let resp = aft.send(&format!(
        r#"{{"id":"nested-indent","command":"inline_symbol","file":{},"symbol":"helper","call_site_line":12}}"#,
        crate::helpers::json_string(&file.display())
    ));
    assert_eq!(resp["success"], true, "inline should succeed: {resp:?}");

    let content = std::fs::read_to_string(&file).expect("read file");
    let expected =
        "  for (const item of values) {\n    if (item > 0) {\n      console.log(item);\n    }\n  }";
    assert!(
        content.contains(expected),
        "nested relative indentation should be preserved:\n{content}"
    );

    aft.shutdown();
}

#[test]
fn inline_symbol_standalone_return_expression_becomes_expression_statement() {
    let tmp = tempfile::tempdir().expect("temp dir");
    let file = tmp.path().join("standalone_return.ts");
    std::fs::write(
        &file,
        r#"function helper(): number {
  audit();
  return sideEffect();
}

function audit(): void {}
function sideEffect(): number { return 1; }

export function main() {
  helper();
}
"#,
    )
    .expect("write fixture");

    let mut aft = AftProcess::spawn();
    configure(&mut aft, &tmp.path().display().to_string());

    let resp = aft.send(&format!(
        r#"{{"id":"standalone-return","command":"inline_symbol","file":{},"symbol":"helper","call_site_line":10}}"#,
        crate::helpers::json_string(&file.display())
    ));
    assert_eq!(resp["success"], true, "inline should succeed: {resp:?}");

    let content = std::fs::read_to_string(&file).expect("read file");
    assert!(
        content.contains("  audit();\n  sideEffect();"),
        "return expression should be kept as an expression statement:\n{content}"
    );
    aft.shutdown();
}

#[test]
fn inline_symbol_maps_destructured_default_and_rest_parameters() {
    let tmp = tempfile::tempdir().expect("temp dir");
    let file = tmp.path().join("params.ts");
    std::fs::write(
        &file,
        r#"type Point = { x: number; y: number };

function describe({ x, y }: Point, [first, second]: number[], label = 'pt', ...rest: string[]): string {
  return `${label}:${x + y}:${first + second}:${rest.join(',')}`;
}

const point = { x: 1, y: 2 };
const pair = [3, 4];

export function main() {
  const result = describe(point, pair, 'p', 'a', 'b');
  console.log(result);
}
"#,
    )
    .expect("write fixture");

    let mut aft = AftProcess::spawn();
    configure(&mut aft, &tmp.path().display().to_string());

    let resp = aft.send(&format!(
        r#"{{"id":"params","command":"inline_symbol","file":{},"symbol":"describe","call_site_line":11}}"#,
        crate::helpers::json_string(&file.display())
    ));
    assert_eq!(resp["success"], true, "inline should succeed: {resp:?}");

    let content = std::fs::read_to_string(&file).expect("read file");
    assert!(
        content.contains("point.x + point.y"),
        "object destructuring bindings should map to variable properties:\n{content}"
    );
    assert!(
        content.contains("pair[0] + pair[1]"),
        "array destructuring bindings should map to indexed accesses:\n{content}"
    );
    assert!(
        content.contains("['a', 'b'].join(',')"),
        "rest parameter should map to an array literal of remaining args:\n{content}"
    );
    assert!(
        !content.contains("describe(point"),
        "call should be replaced:\n{content}"
    );

    aft.shutdown();
}

#[test]
fn inline_symbol_refuses_destructuring_from_non_variable_argument() {
    let tmp = tempfile::tempdir().expect("temp dir");
    let file = tmp.path().join("params_mismatch.ts");
    std::fs::write(
        &file,
        r#"type Point = { x: number; y: number };

function sumPoint({ x, y }: Point): number {
  return x + y;
}

export function main() {
  const result = sumPoint({ x: 1, y: 2 });
  console.log(result);
}
"#,
    )
    .expect("write fixture");

    let mut aft = AftProcess::spawn();
    configure(&mut aft, &tmp.path().display().to_string());

    let resp = aft.send(&format!(
        r#"{{"id":"params-mismatch","command":"inline_symbol","file":{},"symbol":"sumPoint","call_site_line":8}}"#,
        crate::helpers::json_string(&file.display())
    ));
    assert_eq!(resp["success"], false, "inline should fail: {resp:?}");
    assert_eq!(resp["code"], "param_mismatch", "wrong error: {resp:?}");

    aft.shutdown();
}

#[test]
fn inline_symbol_parenthesizes_binary_argument_to_preserve_precedence() {
    let tmp = tempfile::tempdir().expect("temp dir");
    let file = tmp.path().join("precedence.ts");
    std::fs::write(
        &file,
        r#"function twice(x: number): number {
  return x * 2;
}

export function main() {
  const result = twice(1 + 2);
}
"#,
    )
    .expect("write fixture");

    let mut aft = AftProcess::spawn();
    configure(&mut aft, &tmp.path().display().to_string());
    let resp = aft.send(&format!(
        r#"{{"id":"precedence","command":"inline_symbol","file":{},"symbol":"twice","call_site_line":6}}"#,
        crate::helpers::json_string(&file.display())
    ));
    assert_eq!(resp["success"], true, "inline should succeed: {resp:?}");

    let content = std::fs::read_to_string(&file).expect("read file");
    assert!(
        content.contains("const result = (1 + 2) * 2;"),
        "binary argument should retain its grouping:\n{content}"
    );
    assert!(
        !content.contains("const result = 1 + 2 * 2;"),
        "inline must not change operator precedence:\n{content}"
    );
    aft.shutdown();
}

#[test]
fn inline_symbol_wraps_ts_precedence_sensitive_arguments() {
    let tmp = tempfile::tempdir().expect("temp dir");
    let file = tmp.path().join("sensitive_arguments.ts");
    std::fs::write(
        &file,
        r#"function twice(x: number): number {
  return x * 2;
}

function invoke(fn: (n: number) => number): number {
  return fn(2);
}

async function getValue(): Promise<number> {
  return 3;
}

export async function main(flag: boolean) {
  const ternaryResult = twice(flag ? 1 : 2);
  const arrowResult = invoke((n: number) => n + 1);
  const awaitResult = twice(await getValue());
}
"#,
    )
    .expect("write fixture");

    let mut aft = AftProcess::spawn();
    configure(&mut aft, &tmp.path().display().to_string());
    for (id, symbol, line) in [
        ("ternary", "twice", 14),
        ("arrow", "invoke", 15),
        ("await", "twice", 16),
    ] {
        let resp = aft.send(&format!(
            r#"{{"id":"{id}","command":"inline_symbol","file":{},"symbol":"{symbol}","call_site_line":{line}}}"#,
            crate::helpers::json_string(&file.display())
        ));
        assert_eq!(
            resp["success"], true,
            "{id} inline should succeed: {resp:?}"
        );
    }

    let content = std::fs::read_to_string(&file).expect("read file");
    assert!(
        content.contains("const ternaryResult = (flag ? 1 : 2) * 2;"),
        "ternary argument should be wrapped:\n{content}"
    );
    assert!(
        content.contains("const arrowResult = ((n: number) => n + 1)(2);"),
        "arrow argument should be wrapped:\n{content}"
    );
    assert!(
        content.contains("const awaitResult = (await getValue()) * 2;"),
        "await argument should be wrapped:\n{content}"
    );
    aft.shutdown();
}

#[test]
fn inline_symbol_keeps_atomic_arguments_bare() {
    let tmp = tempfile::tempdir().expect("temp dir");
    let file = tmp.path().join("atomic_arguments.ts");
    std::fs::write(
        &file,
        r#"function identity<T>(x: T): T {
  return x;
}

const a = { b: { c: 3 } };
function f(): number {
  return 4;
}

export function main(value: number) {
  const identifierResult = identity(value);
  const literalResult = identity(5);
  const memberResult = identity(a.b.c);
  const callResult = identity(f());
  const parenthesizedResult = identity((value + 1));
}
"#,
    )
    .expect("write fixture");

    let mut aft = AftProcess::spawn();
    configure(&mut aft, &tmp.path().display().to_string());
    for (id, line) in [
        ("identifier", 11),
        ("literal", 12),
        ("member", 13),
        ("call", 14),
        ("parenthesized", 15),
    ] {
        let resp = aft.send(&format!(
            r#"{{"id":"{id}","command":"inline_symbol","file":{},"symbol":"identity","call_site_line":{line}}}"#,
            crate::helpers::json_string(&file.display())
        ));
        assert_eq!(
            resp["success"], true,
            "{id} inline should succeed: {resp:?}"
        );
    }

    let content = std::fs::read_to_string(&file).expect("read file");
    for expected in [
        "const identifierResult = value;",
        "const literalResult = 5;",
        "const memberResult = a.b.c;",
        "const callResult = f();",
        "const parenthesizedResult = (value + 1);",
    ] {
        assert!(
            content.contains(expected),
            "atomic argument gained unnecessary parentheses; missing `{expected}`:\n{content}"
        );
    }
    assert!(
        !content.contains("((value + 1))"),
        "parenthesized argument should not be double-wrapped:\n{content}"
    );
    aft.shutdown();
}

#[test]
fn inline_symbol_wraps_every_use_of_non_atomic_argument() {
    let tmp = tempfile::tempdir().expect("temp dir");
    let file = tmp.path().join("multiple_uses.ts");
    std::fs::write(
        &file,
        r#"function addToItself(x: number): number {
  return x + x;
}

export function main() {
  const result = addToItself(1 + 2);
}
"#,
    )
    .expect("write fixture");

    let mut aft = AftProcess::spawn();
    configure(&mut aft, &tmp.path().display().to_string());
    let resp = aft.send(&format!(
        r#"{{"id":"multiple-uses","command":"inline_symbol","file":{},"symbol":"addToItself","call_site_line":6}}"#,
        crate::helpers::json_string(&file.display())
    ));
    assert_eq!(resp["success"], true, "inline should succeed: {resp:?}");

    let content = std::fs::read_to_string(&file).expect("read file");
    assert!(
        content.contains("const result = (1 + 2) + (1 + 2);"),
        "every substituted use should retain argument grouping:\n{content}"
    );
    aft.shutdown();
}

#[test]
fn inline_symbol_wraps_python_conditional_argument() {
    let tmp = tempfile::tempdir().expect("temp dir");
    let file = tmp.path().join("conditional.py");
    std::fs::write(
        &file,
        r#"def twice(x):
    return x * 2

flag = True
result = twice(1 if flag else 2)
"#,
    )
    .expect("write fixture");

    let mut aft = AftProcess::spawn();
    configure(&mut aft, &tmp.path().display().to_string());
    let resp = aft.send(&format!(
        r#"{{"id":"python-conditional","command":"inline_symbol","file":{},"symbol":"twice","call_site_line":5}}"#,
        crate::helpers::json_string(&file.display())
    ));
    assert_eq!(resp["success"], true, "inline should succeed: {resp:?}");

    let content = std::fs::read_to_string(&file).expect("read file");
    assert!(
        content.contains("result = (1 if flag else 2) * 2"),
        "Python conditional argument should be wrapped:\n{content}"
    );
    aft.shutdown();
}

#[test]
fn inline_symbol_rejects_qualified_call_with_same_named_free_function() {
    let tmp = tempfile::tempdir().expect("temp dir");
    let file = tmp.path().join("qualified_collision.ts");
    let original = "function helper(x: number): number {\n  return x + 1;\n}\n\nconst obj = { helper(x: number) { return x * 2; } };\nconst result = obj.helper(3);\n";
    std::fs::write(&file, original).expect("write fixture");

    let mut aft = AftProcess::spawn();
    configure(&mut aft, &tmp.path().display().to_string());

    let resp = aft.send(&format!(
        r#"{{"id":"qualified-collision","command":"inline_symbol","file":{},"symbol":"helper","call_site_line":6}}"#,
        crate::helpers::json_string(&file.display())
    ));

    assert_eq!(
        resp["success"], false,
        "qualified call must reject: {resp:?}"
    );
    assert_eq!(resp["code"], "ambiguous_call_target");
    let message = resp["message"].as_str().expect("error message");
    assert!(
        message.contains("line 6"),
        "missing call-site line: {message}"
    );
    assert!(
        message.contains("helper (function at line 1)"),
        "missing free-function candidate: {message}"
    );
    assert!(
        message.contains("obj::helper (method at line 5)"),
        "missing object-method candidate: {message}"
    );
    assert_eq!(
        std::fs::read_to_string(&file).expect("read fixture"),
        original,
        "rejected inline must not mutate the file"
    );

    aft.shutdown();
}

#[test]
fn inline_symbol_bare_call_selects_free_function_despite_same_named_method() {
    let tmp = tempfile::tempdir().expect("temp dir");
    let file = tmp.path().join("bare_collision.ts");
    std::fs::write(
        &file,
        "function helper(x: number): number {\n  return x + 1;\n}\n\nconst obj = { helper(x: number) { return x * 2; } };\nconst result = helper(5);\n",
    )
    .expect("write fixture");

    let mut aft = AftProcess::spawn();
    configure(&mut aft, &tmp.path().display().to_string());

    let resp = aft.send(&format!(
        r#"{{"id":"bare-collision","command":"inline_symbol","file":{},"symbol":"helper","call_site_line":6}}"#,
        crate::helpers::json_string(&file.display())
    ));

    assert_eq!(resp["success"], true, "bare call should inline: {resp:?}");
    let content = std::fs::read_to_string(&file).expect("read fixture");
    assert!(
        content.contains("const result = 5 + 1;"),
        "bare call should use the free-function body:\n{content}"
    );
    assert!(
        content.contains("return x * 2;"),
        "same-named object method must remain unchanged:\n{content}"
    );

    aft.shutdown();
}

#[test]
fn inline_symbol_rejects_python_attribute_call_with_same_named_function() {
    let tmp = tempfile::tempdir().expect("temp dir");
    let file = tmp.path().join("qualified_collision.py");
    let original = "def helper(x):\n    return x + 1\n\nclass Obj:\n    def helper(self, x):\n        return x * 2\n\nobj = Obj()\nresult = obj.helper(3)\n";
    std::fs::write(&file, original).expect("write fixture");

    let mut aft = AftProcess::spawn();
    configure(&mut aft, &tmp.path().display().to_string());

    let resp = aft.send(&format!(
        r#"{{"id":"python-qualified-collision","command":"inline_symbol","file":{},"symbol":"helper","call_site_line":9}}"#,
        crate::helpers::json_string(&file.display())
    ));

    assert_eq!(
        resp["success"], false,
        "attribute call must reject: {resp:?}"
    );
    assert_eq!(resp["code"], "ambiguous_call_target");
    let candidates = resp["candidates"].as_array().expect("candidate array");
    assert!(
        candidates
            .iter()
            .any(|candidate| candidate["kind"] == "function" && candidate["line"] == 1),
        "missing Python free-function candidate: {candidates:?}"
    );
    assert!(
        candidates
            .iter()
            .any(|candidate| candidate["kind"] == "method" && candidate["line"] == 5),
        "missing Python method candidate: {candidates:?}"
    );
    assert_eq!(
        std::fs::read_to_string(&file).expect("read fixture"),
        original,
        "rejected inline must not mutate the file"
    );

    aft.shutdown();
}

#[test]
fn inline_symbol_preserves_unambiguous_method_inlining() {
    let tmp = tempfile::tempdir().expect("temp dir");
    let file = tmp.path().join("method_only.ts");
    std::fs::write(
        &file,
        "class Calculator {\n  helper(x: number): number {\n    return x * 2;\n  }\n}\nconst calculator = new Calculator();\nconst result = calculator.helper(3);\n",
    )
    .expect("write fixture");

    let mut aft = AftProcess::spawn();
    configure(&mut aft, &tmp.path().display().to_string());

    let resp = aft.send(&format!(
        r#"{{"id":"method-only","command":"inline_symbol","file":{},"symbol":"helper","call_site_line":7}}"#,
        crate::helpers::json_string(&file.display())
    ));

    assert_eq!(
        resp["success"], true,
        "unambiguous method call should keep working: {resp:?}"
    );
    let content = std::fs::read_to_string(&file).expect("read fixture");
    assert!(
        content.contains("const result = 3 * 2;"),
        "method body should be inlined:\n{content}"
    );

    aft.shutdown();
}
