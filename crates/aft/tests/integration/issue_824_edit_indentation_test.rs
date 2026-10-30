//! Regression tests for issue #824: an `edit` could leave a Python file with
//! an IndentationError while reporting success.
//!
//! The `issue_824_*` edit tests drive the real agent path (`tool_call` with
//! the `edit` tool, the same request the OpenCode and Pi plugins send),
//! through both argument shapes that reach find/replace.
//!
//! Two diagnostics stay ignored. One prints the tree-sitter-python trees for
//! the broken results, to show why tree-sitter's `has_error()` alone accepts
//! them. The other runs edit syntax validation over an external list of valid
//! Python files to measure false rejections. Run them with
//! `cargo test -p agent-file-tools --test integration issue_824 -- --ignored`.

use std::fs;
use std::path::Path;

use serde_json::{json, Value};

use super::helpers::{user_config, AftProcess};

/// The file from the issue. `oldString` below drops one indentation level.
const CASE1_BEFORE: &str = "def f(x):\n    if x:\n        a = 1\n        b = 2\n    return x\n";
const CASE1_OLD: &str = "if x:\n    a = 1\n    b = 2";
const CASE1_NEW: &str = "if x:\n    a = 10\n    b = 20";
/// What AFT 0.62.0 and the current code write for case 1: only the first
/// replacement line lands at the file's indentation.
const CASE1_BROKEN: &str = "def f(x):\n    if x:\n    a = 10\n    b = 20\n    return x\n";
/// The replacement re-indented by the same amount the match was off by.
const CASE1_REINDENTED: &str =
    "def f(x):\n    if x:\n        a = 10\n        b = 20\n    return x\n";

/// An exact match whose replacement adds an over-indented line and then a
/// line back at the block's level.
const CASE2_BEFORE: &str = "def g():\n    if True:\n        return 2\n    return 3\n";
const CASE2_OLD: &str = "return 2";
const CASE2_NEW: &str = "return 2\n            y = 5\n        z = 6";
const CASE2_BROKEN: &str =
    "def g():\n    if True:\n        return 2\n            y = 5\n        z = 6\n    return 3\n";
/// The same edit adding a tab-indented line to a space-indented block.
const CASE2_TAB_NEW: &str = "return 2\n\ty = 5";
const CASE2_TAB_BROKEN: &str = "def g():\n    if True:\n        return 2\n\ty = 5\n    return 3\n";

/// TypeScript: `oldString` and `newString` written with no indentation.
const CASE3_BEFORE: &str = "export function f(x: number) {\n  if (x) {\n    const a = 1;\n    const b = 2;\n  }\n  return x;\n}\n";
const CASE3_OLD: &str = "const a = 1;\nconst b = 2;";
const CASE3_NEW: &str = "const a = 10;\nconst b = 20;";
const CASE3_COLUMN_ZERO: &str = "export function f(x: number) {\n  if (x) {\n    const a = 10;\nconst b = 20;\n  }\n  return x;\n}\n";
const CASE3_REINDENTED: &str = "export function f(x: number) {\n  if (x) {\n    const a = 10;\n    const b = 20;\n  }\n  return x;\n}\n";

/// The two argument shapes that reach find/replace: an `edits[]` item (the
/// shape the plugins' schema offers; translated to the `batch` command) and a
/// top-level `oldString` (translated to the `edit_match` command).
#[derive(Clone, Copy, Debug)]
enum Shape {
    EditsArray,
    TopLevel,
}

const SHAPES: [Shape; 2] = [Shape::EditsArray, Shape::TopLevel];

fn edit_arguments(shape: Shape, path: &str, old: &str, new: &str) -> Value {
    match shape {
        Shape::EditsArray => {
            json!({ "path": path, "edits": [{ "oldString": old, "newString": new }] })
        }
        Shape::TopLevel => json!({ "path": path, "oldString": old, "newString": new }),
    }
}

struct EditRun {
    reply: Value,
    after: String,
}

/// Write `before` to `name` in a fresh project, run one `edit` tool call with
/// `validate_on_edit: "syntax"` (the issue's setting), and return the reply
/// and the file content afterwards.
fn run_edit(shape: Shape, name: &str, before: &str, old: &str, new: &str) -> EditRun {
    let dir = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(dir.path()).unwrap();
    let file = root.join(name);
    fs::write(&file, before).unwrap();

    let mut aft = AftProcess::spawn();
    let configured = aft.send(
        &json!({
            "id": "cfg",
            "command": "configure",
            "harness": "opencode",
            "project_root": root,
            "config": user_config(json!({ "validate_on_edit": "syntax", "format_on_edit": false })),
        })
        .to_string(),
    );
    assert_eq!(configured["success"], true, "{configured:?}");

    let reply = aft.send(
        &json!({
            "id": "edit",
            "command": "tool_call",
            "name": "edit",
            "arguments": edit_arguments(shape, name, old, new),
        })
        .to_string(),
    );
    let after = fs::read_to_string(&file).unwrap();
    assert!(aft.shutdown().success());
    EditRun { reply, after }
}

fn reply_text(reply: &Value) -> String {
    reply["text"]
        .as_str()
        .map(str::to_string)
        .unwrap_or_else(|| reply.to_string())
}

/// Fail with every shape's problem at once, so one run shows whether the
/// `batch` and `edit_match` routes both have the bug.
fn assert_no_failures(failures: Vec<String>) {
    assert!(failures.is_empty(), "\n{}", failures.join("\n\n"));
}

/// A fuzzy match whose `oldString` is one level too shallow must not leave a
/// file Python cannot parse while reporting success. Either the replacement is
/// re-indented to the file's level, or the edit is refused or rolled back and
/// the file is unchanged.
#[test]
fn issue_824_fuzzy_shallow_python_edit_never_leaves_invalid_file() {
    let mut failures = Vec::new();
    for shape in SHAPES {
        let run = run_edit(shape, "t.py", CASE1_BEFORE, CASE1_OLD, CASE1_NEW);
        if run.after == CASE1_BROKEN {
            failures.push(format!(
                "{shape:?}: the file was left with an IndentationError; reply: {:?}",
                reply_text(&run.reply)
            ));
        } else if run.after != CASE1_REINDENTED && run.after != CASE1_BEFORE {
            failures.push(format!(
                "{shape:?}: unexpected result {:?}; reply: {:?}",
                run.after,
                reply_text(&run.reply)
            ));
        }
    }
    assert_no_failures(failures);
}

/// The same edit lands at the file's indentation: every replacement line is
/// shifted by the four spaces `oldString` was missing, not just the first.
#[test]
fn issue_824_fuzzy_shallow_python_edit_is_reindented() {
    let mut failures = Vec::new();
    for shape in SHAPES {
        let run = run_edit(shape, "t.py", CASE1_BEFORE, CASE1_OLD, CASE1_NEW);
        if run.after != CASE1_REINDENTED || run.reply["success"] != true {
            failures.push(format!(
                "{shape:?}: expected the re-indented file, got {:?}; reply: {:?}",
                run.after, run.reply
            ));
        }
    }
    assert_no_failures(failures);
}

/// An `oldString` written one level too deep loses that level on every
/// replacement line.
#[test]
fn issue_824_fuzzy_deep_python_edit_is_dedented() {
    let before = "def f(x):\n    a = 1\n    return a\n";
    let old = "        a = 1\n        return a";
    let new = "        a = 1\n        if x:\n            a = 2\n        return a";
    let expected = "def f(x):\n    a = 1\n    if x:\n        a = 2\n    return a\n";
    let mut failures = Vec::new();
    for shape in SHAPES {
        let run = run_edit(shape, "t.py", before, old, new);
        if run.after != expected {
            failures.push(format!(
                "{shape:?}: expected {expected:?}, got {:?}; reply: {:?}",
                run.after, run.reply
            ));
        }
    }
    assert_no_failures(failures);
}

/// Indentation that differs in kind (tabs against spaces) or by different
/// amounts on different lines has no single correct shift. The edit is
/// refused with `ambiguous_indentation` and the file is left alone, in any
/// language.
#[test]
fn issue_824_ambiguous_indentation_is_refused() {
    let cases = [
        (
            "tabs against spaces",
            "t.py",
            "def f(x):\n\tif x:\n\t\ta = 1\n",
            "if x:\n    a = 1",
            "if x:\n    a = 2",
        ),
        (
            "flattened block",
            "t.ts",
            CASE3_BEFORE,
            "if (x) {\nconst a = 1;\nconst b = 2;\n}",
            "if (x) {\nconst a = 10;\n}",
        ),
    ];
    let mut failures = Vec::new();
    for shape in SHAPES {
        for (label, name, before, old, new) in cases {
            let run = run_edit(shape, name, before, old, new);
            if run.reply["success"] != false
                || run.reply["code"] != "ambiguous_indentation"
                || run.after != before
            {
                failures.push(format!(
                    "{shape:?} {label}: expected an ambiguous_indentation refusal, got {:?}; file {:?}",
                    run.reply, run.after
                ));
            }
        }
    }
    assert_no_failures(failures);
}

/// An exact edit that inserts an over-indented line produces a file CPython
/// rejects with "unexpected indent". Syntax validation must roll it back, the
/// same way it rolls back other invalid edits. A line indented with a tab
/// where its block uses spaces (CPython's TabError) must roll back too.
#[test]
fn issue_824_exact_misindented_python_edit_is_rolled_back() {
    let mut failures = Vec::new();
    for shape in SHAPES {
        for (label, new, broken) in [
            ("unexpected indent", CASE2_NEW, CASE2_BROKEN),
            ("tab error", CASE2_TAB_NEW, CASE2_TAB_BROKEN),
        ] {
            let run = run_edit(shape, "t.py", CASE2_BEFORE, CASE2_OLD, new);
            if run.after == broken {
                failures.push(format!(
                    "{shape:?} {label}: the file was left invalid; syntax_valid={} rolled_back={}; reply: {:?}",
                    run.reply["syntax_valid"],
                    run.reply["rolled_back"],
                    reply_text(&run.reply)
                ));
            } else if run.after != CASE2_BEFORE || run.reply["rolled_back"] != true {
                failures.push(format!(
                    "{shape:?} {label}: expected a rollback, got {:?}; reply: {:?}",
                    run.after, run.reply
                ));
            }
        }
    }
    assert_no_failures(failures);
}

/// When a whitespace-insensitive pass was needed to find `oldString`, the
/// agent-facing reply says so in one line, and says the replacement was
/// re-indented. The reply text is what the plugins show the agent (rendered
/// by `subc_format::format_edit_summary`); it used to drop the structured
/// `fuzzy_match`/`fuzzy_matches` field.
#[test]
fn issue_824_fuzzy_edit_reply_text_reports_fuzzy_match() {
    const REINDENTED_NOTICE: &str = "oldString matched only after ignoring indentation and trailing whitespace; newString was re-indented to match the file.";
    let mut failures = Vec::new();
    for shape in SHAPES {
        for (name, before, old, new) in [
            ("t.py", CASE1_BEFORE, CASE1_OLD, CASE1_NEW),
            ("t.ts", CASE3_BEFORE, CASE3_OLD, CASE3_NEW),
        ] {
            let run = run_edit(shape, name, before, old, new);
            let structured = if run.reply.get("fuzzy_matches").is_some() {
                &run.reply["fuzzy_matches"][0]
            } else {
                &run.reply["fuzzy_match"]
            };
            assert_eq!(
                structured["pass"], 3,
                "{shape:?} {name}: the trimmed-line pass should be the one that matched: {:?}",
                run.reply
            );
            let text = reply_text(&run.reply);
            let expected = format!("Edited (+2/-2).\n{REINDENTED_NOTICE}");
            if text != expected {
                failures.push(format!(
                    "{shape:?} {name}: expected reply {expected:?}, got {text:?}; structured field: {structured}"
                ));
            }
        }
    }
    assert_no_failures(failures);
}

/// TypeScript stays valid at column 0, so this was a formatting complaint, not
/// a broken file: the second replacement line landed at column 0. It now gets
/// the file's indentation like every other shifted line.
#[test]
fn issue_824_fuzzy_unindented_ts_edit_keeps_file_indentation() {
    let mut failures = Vec::new();
    for shape in SHAPES {
        let run = run_edit(shape, "t.ts", CASE3_BEFORE, CASE3_OLD, CASE3_NEW);
        if run.after != CASE3_REINDENTED {
            let column_zero = run.after == CASE3_COLUMN_ZERO;
            failures.push(format!(
                "{shape:?}: second line written at column 0: {column_zero}; result {:?}",
                run.after
            ));
        }
    }
    assert_no_failures(failures);
}

// --- Probes -----------------------------------------------------------------

fn parse_python(source: &str) -> tree_sitter::Tree {
    let mut parser = tree_sitter::Parser::new();
    parser
        .set_language(&tree_sitter_python::LANGUAGE.into())
        .expect("load tree-sitter-python");
    parser.parse(source, None).expect("parse python")
}

fn contains_missing(node: tree_sitter::Node<'_>) -> bool {
    let mut cursor = node.walk();
    loop {
        if cursor.node().is_missing() {
            return true;
        }
        if cursor.goto_first_child() {
            continue;
        }
        loop {
            if cursor.goto_next_sibling() {
                break;
            }
            if !cursor.goto_parent() {
                return false;
            }
        }
    }
}

/// Prints the tree-sitter-python trees for the two broken results, and pins
/// why tree-sitter alone accepts them: neither tree holds an ERROR or a
/// MISSING node, so `has_error()` (which counts both) is false and edit
/// validation needs its own Python indentation check. Inputs that recover with
/// MISSING nodes show `has_error()` already rejects those.
#[test]
#[ignore = "issue #824 probe: prints parse trees"]
fn issue_824_probe_python_trees_for_broken_results() {
    for (label, source) in [("case1", CASE1_BROKEN), ("case2", CASE2_BROKEN)] {
        let tree = parse_python(source);
        let root = tree.root_node();
        println!("--- {label} ---\n{source}{}\n", root.to_sexp());
        assert!(!root.has_error(), "{label}: tree-sitter reports an error");
        assert!(
            !contains_missing(root),
            "{label}: tree holds a MISSING node"
        );
    }
    // Inputs whose recovery may insert MISSING nodes (Python first, then
    // TypeScript, whose grammar recovers that way more often). At least one
    // must hold a MISSING node and no ERROR node, and `has_error()` must still
    // be true for it: the check counts MISSING nodes regardless of language.
    let mut missing_without_error = 0;
    let typescript: tree_sitter::Language = tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into();
    let python: tree_sitter::Language = tree_sitter_python::LANGUAGE.into();
    for (language, source) in [
        (&python, "if x\n    pass\n"),
        (&python, "def f(a:\n    pass\n"),
        (&python, "x = [1, 2"),
        (&python, "x = (1 + 2\ny = 3\n"),
        (&python, "if x:\n    pass\nelse\n    pass\n"),
        (&python, "print(1\n"),
        (&typescript, "const x = foo(1;\n"),
        (&typescript, "function f() {\n  return 1;\n"),
        (&typescript, "if (x { y(); }\n"),
    ] {
        let mut parser = tree_sitter::Parser::new();
        parser.set_language(language).unwrap();
        let tree = parser.parse(source, None).unwrap();
        let root = tree.root_node();
        let sexp = root.to_sexp();
        println!(
            "--- {source:?} (has_error={}) ---\n{sexp}\n",
            root.has_error()
        );
        if contains_missing(root) && !sexp.contains("ERROR") {
            missing_without_error += 1;
            assert!(root.has_error(), "{source:?}: MISSING node not counted");
        }
    }
    assert!(
        missing_without_error > 0,
        "no input produced a MISSING-only tree"
    );

    // Indentation errors of the same family that tree-sitter also accepts.
    for (label, source) in [
        // A line dedented to a level no enclosing block uses
        // ("unindent does not match any outer indentation level").
        (
            "dedent_between_levels",
            "def h():\n    if x:\n        a = 1\n      b = 2\n",
        ),
        // A header whose body is only a comment ("expected an indented block").
        (
            "comment_only_body",
            "def k():\n    if x:\n        # todo\n    return 1\n",
        ),
    ] {
        let tree = parse_python(source);
        println!(
            "--- {label} (has_error={}) ---\n{source}{}\n",
            tree.root_node().has_error(),
            tree.root_node().to_sexp()
        );
    }
}

/// Runs edit syntax validation over every file named in the
/// newline-separated list at `AFT_ISSUE_824_CORPUS`. The list should hold only
/// files that CPython's own parser accepts, so any file validation rejects is
/// a false rejection that would roll back a good edit. The always-on, bounded
/// form of this check (every Python file in this repository) is the
/// `python_indent` unit test `accepts_every_python_file_in_the_repository`.
#[test]
#[ignore = "corpus measurement: needs AFT_ISSUE_824_CORPUS"]
fn issue_824_python_validation_has_no_false_rejections_on_corpus() {
    let list = std::env::var("AFT_ISSUE_824_CORPUS").expect("set AFT_ISSUE_824_CORPUS");
    let list = fs::read_to_string(list).unwrap();
    let mut checked = 0usize;
    let mut rejected = Vec::new();
    for path in list.lines().filter(|line| !line.trim().is_empty()) {
        let path = Path::new(path);
        let Ok(source) = fs::read_to_string(path) else {
            continue;
        };
        checked += 1;
        if aft::edit::validate_syntax_str(&source, path) != Some(true) {
            rejected.push(path.display().to_string());
        }
    }
    println!(
        "checked {checked} files; validation rejects {}",
        rejected.len()
    );
    assert!(checked > 0, "corpus list named no readable files");
    assert!(rejected.is_empty(), "false rejections: {rejected:?}");
}

/// Re-indenting a loose match must not change the value of a multi-line
/// string literal that `newString` opens: the lines inside it keep exactly
/// the text the agent wrote, while code lines around it are shifted. Covers
/// Python triple-quoted strings, TypeScript template literals, Rust raw
/// strings and bash heredocs.
#[test]
fn issue_824_reindent_leaves_multiline_string_contents_alone() {
    let cases = [
        (
            "python triple-quoted string",
            "t.py",
            "def f():\n    a = 1\n    b = 2\n    return b\n",
            "a = 1\nb = 2",
            "a = \"\"\"\nhello\n\"\"\"\nb = 2",
            "def f():\n    a = \"\"\"\nhello\n\"\"\"\n    b = 2\n    return b\n",
        ),
        (
            "typescript template literal",
            "t.ts",
            "function f() {\n  const a = 1;\n  return a;\n}\n",
            "const a = 1;\nreturn a;",
            "const a = `x\ny`;\nreturn a;",
            "function f() {\n  const a = `x\ny`;\n  return a;\n}\n",
        ),
        (
            "rust raw string",
            "t.rs",
            "fn f() -> &'static str {\n    let a = \"\";\n    a\n}\n",
            "let a = \"\";\na",
            "let a = r#\"x\ny\"#;\na",
            "fn f() -> &'static str {\n    let a = r#\"x\ny\"#;\n    a\n}\n",
        ),
        (
            "bash heredoc",
            "t.sh",
            "f() {\n  echo a\n  echo b\n}\n",
            "echo a\necho b",
            "cat <<EOF\nhi\nEOF\necho b",
            "f() {\n  cat <<EOF\nhi\nEOF\n  echo b\n}\n",
        ),
    ];
    let mut failures = Vec::new();
    for shape in SHAPES {
        for (label, name, before, old, new, expected) in cases {
            let run = run_edit(shape, name, before, old, new);
            if run.after != expected || run.reply["success"] != true {
                failures.push(format!(
                    "{shape:?} {label}: expected {expected:?}, got {:?}; reply: {:?}",
                    run.after,
                    reply_text(&run.reply)
                ));
            }
        }
    }
    assert_no_failures(failures);
}

/// A loose match that falls inside a multi-line string the file already
/// has gives no way to tell whether the agent meant the string's text as
/// written or at the file's depth, so the edit is refused instead of
/// changing the string either way.
#[test]
fn issue_824_loose_match_inside_existing_multiline_string_is_refused() {
    let before = "def f():\n    s = \"\"\"\n        one\n        two\n    \"\"\"\n    return s\n";
    let mut failures = Vec::new();
    for shape in SHAPES {
        let run = run_edit(shape, "t.py", before, "one\ntwo", "one\ntwo\nthree");
        if run.reply["code"] != "ambiguous_indentation" || run.after != before {
            failures.push(format!(
                "{shape:?}: expected an ambiguous_indentation refusal, got {:?}; file {:?}",
                reply_text(&run.reply),
                run.after
            ));
        }
    }
    assert_no_failures(failures);
}

/// Python never continues a comment onto the next line, even when the
/// comment ends in a backslash, so an indented statement after such a
/// comment is an "unexpected indent" and the edit must roll back.
#[test]
fn issue_824_comment_ending_in_backslash_does_not_hide_indentation_error() {
    let before = "a = 1\nb = 2\n";
    let mut failures = Vec::new();
    for shape in SHAPES {
        let run = run_edit(shape, "t.py", before, "a = 1", "a = 1  # note \\\n  x = 3");
        if run.after != before || run.reply["rolled_back"] != true {
            failures.push(format!(
                "{shape:?}: expected a rollback, got {:?}; reply: {:?}",
                run.after,
                reply_text(&run.reply)
            ));
        }
    }
    assert_no_failures(failures);
}
