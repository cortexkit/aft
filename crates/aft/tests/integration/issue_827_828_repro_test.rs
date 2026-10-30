//! Regression tests for two reported tool defects, driven through the agent
//! tool path (`tool_call`), which is what the OpenCode plugin sends.
//!
//! Issue #828 (`aft_import`): comments stay with the import they describe when
//! organize reorders it (a trailing `# noqa` no longer splits a group), Python
//! groups follow isort's default order, and `remove` deletes the removed
//! statement's same-line comment with it.
//!
//! Issue #827 (`ast_grep_replace` / `ast_grep_search`): a dry run refuses a
//! rewrite that apply would refuse for invalid syntax, and a variadic capture
//! is displayed as its source text instead of its node texts joined by commas.

use std::fs;
use std::path::{Path, PathBuf};

use serde_json::{json, Value};

use super::helpers::AftProcess;

fn project(files: &[(&str, &str)]) -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().expect("create temp project");
    // Canonical root so fixture paths agree with the paths the tool reports.
    let root = fs::canonicalize(dir.path()).expect("canonical project root");
    for (name, body) in files {
        fs::write(root.join(name), body).expect("write fixture");
    }
    (dir, root)
}

fn configured(root: &Path) -> AftProcess {
    let mut aft = AftProcess::spawn();
    let resp = aft.configure(root);
    assert_eq!(resp["success"], true, "configure should succeed: {resp:?}");
    aft
}

fn tool_call(aft: &mut AftProcess, id: &str, name: &str, arguments: Value) -> Value {
    aft.send(
        &json!({ "id": id, "command": "tool_call", "name": name, "arguments": arguments })
            .to_string(),
    )
}

/// The agent-visible reply: rendered text on success, the message on failure.
fn reply_text(resp: &Value) -> String {
    resp["text"]
        .as_str()
        .or_else(|| resp["message"].as_str())
        .unwrap_or_default()
        .to_string()
}

fn line_index(content: &str, line: &str) -> usize {
    content
        .lines()
        .position(|candidate| candidate.trim_end() == line)
        .unwrap_or_else(|| panic!("missing line {line:?} in:\n{content}"))
}

// ---------------------------------------------------------------------------
// #828 aft_import
// ---------------------------------------------------------------------------

#[test]
fn organize_python_trailing_comment_does_not_split_stdlib_group() {
    let source = "import sys\nimport os\nimport zeta.side  # noqa\nimport abc\nfrom . import rel\nfrom collections import OrderedDict\nimport json\n\nprint(sys, os, abc, rel, OrderedDict, json)\n";
    let (_dir, root) = project(&[("case.py", source)]);
    let mut aft = configured(&root);

    let resp = tool_call(
        &mut aft,
        "org-py-noqa",
        "import",
        json!({ "op": "organize", "path": "case.py" }),
    );
    assert_eq!(resp["success"], true, "organize should succeed: {resp:?}");
    let content = fs::read_to_string(root.join("case.py")).unwrap();

    // The comment belongs to its import and must travel with it.
    assert!(
        content.contains("import zeta.side  # noqa\n"),
        "trailing comment must stay on its import:\n{content}"
    );
    // All five stdlib imports form one block: no other import line sits
    // between the first and the last of them.
    let stdlib = [
        "import abc",
        "import json",
        "import os",
        "import sys",
        "from collections import OrderedDict",
    ];
    let positions: Vec<usize> = stdlib.iter().map(|l| line_index(&content, l)).collect();
    let first = *positions.iter().min().unwrap();
    let last = *positions.iter().max().unwrap();
    let foreign = content
        .lines()
        .enumerate()
        .filter(|(i, _)| *i > first && *i < last)
        .filter(|(_, line)| !line.trim().is_empty() && !stdlib.contains(&line.trim_end()))
        .map(|(_, line)| line.to_string())
        .collect::<Vec<_>>();
    assert!(
        foreign.is_empty(),
        "stdlib imports were split by {foreign:?}:\n{content}"
    );

    aft.shutdown();
}

#[test]
fn organize_python_sorts_group_by_module_not_raw_text() {
    let source = "import json\nfrom collections import OrderedDict\nimport abc\n\nprint(json, OrderedDict, abc)\n";
    let (_dir, root) = project(&[("sorting.py", source)]);
    let mut aft = configured(&root);

    let resp = tool_call(
        &mut aft,
        "org-py-sort",
        "import",
        json!({ "op": "organize", "path": "sorting.py" }),
    );
    assert_eq!(resp["success"], true, "organize should succeed: {resp:?}");
    let content = fs::read_to_string(root.join("sorting.py")).unwrap();

    // isort's default order: plain `import` lines by module, then `from`
    // lines by module.
    let abc = line_index(&content, "import abc");
    let collections = line_index(&content, "from collections import OrderedDict");
    let json_line = line_index(&content, "import json");
    assert!(
        abc < json_line && json_line < collections,
        "stdlib group is not in isort's default order:\n{content}"
    );

    aft.shutdown();
}

#[test]
fn organize_ts_leading_comment_stays_with_its_import() {
    let source = "import \"./z-side\";\nimport {z} from \"./z\";\n// about y\nimport {y} from \"./y\";\nimport fs from \"fs\";\nimport \"reflect-metadata\";\nimport {x} from \"./x\";\nimport path from \"path\";\n\nexport const all = [z, y, fs, x, path];\n";
    let (_dir, root) = project(&[("case.ts", source)]);
    let mut aft = configured(&root);

    let resp = tool_call(
        &mut aft,
        "org-ts-comment",
        "import",
        json!({ "op": "organize", "path": "case.ts" }),
    );
    assert_eq!(resp["success"], true, "organize should succeed: {resp:?}");
    let content = fs::read_to_string(root.join("case.ts")).unwrap();

    let comment = line_index(&content, "// about y");
    let y_import = line_index(&content, "import { y } from \"./y\";");
    assert_eq!(
        comment + 1,
        y_import,
        "`// about y` must stay directly above the ./y import:\n{content}"
    );
    // Side-effect imports stay ordering barriers: nothing crosses them.
    let side = line_index(&content, "import \"reflect-metadata\";");
    assert!(
        line_index(&content, "import fs from \"fs\";") < side,
        "{content}"
    );
    assert!(
        side < line_index(&content, "import { x } from \"./x\";"),
        "{content}"
    );

    aft.shutdown();
}

#[test]
fn remove_takes_the_trailing_same_line_comment_with_the_import() {
    let ts = "// about y\nimport { y } from \"./y\"; // trailing\nimport { w } from \"./w\";\n\nexport const all = [w];\n";
    let py = "import os  # noqa\nimport sys\n\nprint(sys)\n";
    let (_dir, root) = project(&[("case.ts", ts), ("case.py", py)]);
    let mut aft = configured(&root);

    let resp = tool_call(
        &mut aft,
        "rm-ts",
        "import",
        json!({ "op": "remove", "path": "case.ts", "module": "./y" }),
    );
    assert_eq!(resp["success"], true, "remove should succeed: {resp:?}");
    let content = fs::read_to_string(root.join("case.ts")).unwrap();
    assert!(
        !content.contains("// trailing"),
        "the removed import's trailing comment was left behind:\n{content}"
    );
    assert!(
        content.contains("import { w } from \"./w\";\n"),
        "{content}"
    );

    let resp = tool_call(
        &mut aft,
        "rm-py",
        "import",
        json!({ "op": "remove", "path": "case.py", "module": "os" }),
    );
    assert_eq!(resp["success"], true, "remove should succeed: {resp:?}");
    let content = fs::read_to_string(root.join("case.py")).unwrap();
    assert!(
        !content.contains("# noqa"),
        "the removed import's trailing comment was left behind:\n{content}"
    );
    assert!(content.starts_with("import sys\n"), "{content}");

    aft.shutdown();
}

/// Runs organize on one file and returns the organize reply and new contents.
fn organize_file(name: &str, source: &str) -> (Value, String) {
    let (_dir, root) = project(&[(name, source)]);
    let mut aft = configured(&root);
    let resp = tool_call(
        &mut aft,
        "organize",
        "import",
        json!({ "op": "organize", "path": name }),
    );
    assert_eq!(resp["success"], true, "organize should succeed: {resp:?}");
    let content = fs::read_to_string(root.join(name)).unwrap();
    aft.shutdown();
    (resp, content)
}

#[test]
fn organize_keeps_a_comment_inside_a_named_import_list() {
    let (_, content) = organize_file(
        "inner.ts",
        "import { b, // why b\n  a } from \"./m\";\nimport z from \"z\";\n\nexport const all = [a, b, z];\n",
    );
    assert_eq!(
        content,
        "import z from \"z\";\n\nimport { b, // why b\n  a } from \"./m\";\n\nexport const all = [a, b, z];\n",
        "a statement with a comment inside is kept verbatim and still sorted"
    );
}

#[test]
fn organize_moves_the_last_imports_same_line_comment_with_it() {
    let (_, content) = organize_file(
        "last.ts",
        "import { b } from \"./b\";\nimport { a } from \"./a\"; // about a\n\nexport const all = [a, b];\n",
    );
    assert_eq!(
        content,
        "import { a } from \"./a\"; // about a\nimport { b } from \"./b\";\n\nexport const all = [a, b];\n"
    );
}

#[test]
fn organize_keeps_the_comments_of_a_removed_duplicate() {
    let (resp, content) = organize_file(
        "dup.ts",
        "import { a } from \"./a\";\nimport { a } from \"./a\"; // dup\nimport { b } from \"./b\";\n\nexport const all = [a, b];\n",
    );
    assert_eq!(resp["removed_duplicates"], 1, "{resp:?}");
    assert_eq!(
        content,
        "import { a } from \"./a\"; // dup\nimport { b } from \"./b\";\n\nexport const all = [a, b];\n"
    );
}

#[test]
fn organize_rust_merge_keeps_the_same_line_comment() {
    let (_, content) = organize_file(
        "merge.rs",
        "use std::path::PathBuf; // pb\nuse std::path::Path;\n\nfn main() {}\n",
    );
    assert_eq!(
        content,
        "use std::path::{Path, PathBuf}; // pb\n\nfn main() {}\n"
    );
}

#[test]
fn organize_keeps_a_section_comment_followed_by_a_blank_line_as_a_boundary() {
    let source =
        "import B from \"b\";\n// section\n\nimport A from \"a\";\n\nexport const all = [A, B];\n";
    let (_, content) = organize_file("section.ts", source);
    assert_eq!(
        content, source,
        "a comment separated from the next import by a blank line still splits the sort"
    );
}

// ---------------------------------------------------------------------------
// #827 ast_grep_replace / ast_grep_search
// ---------------------------------------------------------------------------

#[test]
fn ast_replace_dry_run_refuses_rewrite_that_apply_would_refuse() {
    let original = "foo(1, 2);\nfoo(3, 4);\n";
    let (_dir, root) = project(&[("dry.ts", original)]);
    let mut aft = configured(&root);
    let args = |dry_run: bool| {
        json!({
            "pattern": "foo($A, $B)",
            "rewrite": "bar($B, $A",
            "lang": "typescript",
            "paths": ["dry.ts"],
            "dryRun": dry_run,
        })
    };

    // Control: apply refuses this rewrite today.
    let applied = tool_call(&mut aft, "apply", "ast_replace", args(false));
    assert_eq!(applied["success"], false, "apply must refuse: {applied:?}");
    assert!(
        reply_text(&applied).contains("syntax validation"),
        "{applied:?}"
    );

    let preview = tool_call(&mut aft, "preview", "ast_replace", args(true));
    let text = reply_text(&preview);
    assert!(
        text.contains("syntax validation"),
        "the preview must report the same syntax refusal as apply, got success={} text:\n{text}",
        preview["success"]
    );
    assert!(
        !text.contains("Would replace"),
        "the preview must not promise a replacement apply refuses:\n{text}"
    );
    assert_eq!(preview["success"], false, "{preview:?}");
    assert_eq!(preview["code"], "invalid_rewrite", "{preview:?}");
    let file = fs::canonicalize(root.join("dry.ts")).unwrap();
    assert_eq!(
        text,
        format!(
            "ast_replace: dry run: rewritten code failed syntax validation in {}; apply would refuse this rewrite (no files were written)",
            file.display()
        ),
        "the preview must name the refused file"
    );
    assert_eq!(fs::read_to_string(root.join("dry.ts")).unwrap(), original);

    aft.shutdown();
}

#[test]
fn ast_search_renders_variadic_capture_as_source_text() {
    let (_dir, root) = project(&[("search.ts", "foo(1, 2);\n")]);
    let mut aft = configured(&root);

    let resp = tool_call(
        &mut aft,
        "search",
        "ast_search",
        json!({ "pattern": "foo($$$A)", "lang": "typescript", "paths": ["search.ts"] }),
    );
    assert_eq!(resp["success"], true, "search should succeed: {resp:?}");
    let text = reply_text(&resp);
    assert!(
        text.lines().any(|line| line.trim() == "$A: 1, 2"),
        "variadic capture should show its source text:\n{text}"
    );

    aft.shutdown();
}

// ---------------------------------------------------------------------------
// Line endings: a line comment ends at `\r` as well as `\n`
// ---------------------------------------------------------------------------

/// Runs remove on one file and returns the remove reply and new contents.
fn remove_from_file(name: &str, source: &str, module: &str) -> (Value, String) {
    let (_dir, root) = project(&[(name, source)]);
    let mut aft = configured(&root);
    let resp = tool_call(
        &mut aft,
        "remove",
        "import",
        json!({ "op": "remove", "path": name, "module": module }),
    );
    let content = fs::read_to_string(root.join(name)).unwrap();
    aft.shutdown();
    (resp, content)
}

#[test]
fn remove_with_cr_only_line_endings_deletes_only_the_import_line() {
    let (resp, content) = remove_from_file(
        "cr.ts",
        "import { y } from \"./y\"; // y\rimport { w } from \"./w\";\rexport const all = [w];\r",
        "./y",
    );
    assert_eq!(resp["success"], true, "remove should succeed: {resp:?}");
    assert_eq!(
        content, "import { w } from \"./w\";\rexport const all = [w];\r",
        "only the removed import's line may go; the lines after it must stay"
    );
}

#[test]
fn remove_with_crlf_line_endings_deletes_the_import_line_and_its_comment() {
    let (resp, content) = remove_from_file(
        "crlf.ts",
        "import { y } from \"./y\"; // y\r\nimport { w } from \"./w\";\r\n\r\nexport const all = [w];\r\n",
        "./y",
    );
    assert_eq!(resp["success"], true, "remove should succeed: {resp:?}");
    assert_eq!(
        content,
        "import { w } from \"./w\";\r\n\r\nexport const all = [w];\r\n"
    );

    let (resp, content) = remove_from_file(
        "crlf.py",
        "import os  # noqa\r\nimport sys\r\n\r\nprint(sys)\r\n",
        "os",
    );
    assert_eq!(resp["success"], true, "remove should succeed: {resp:?}");
    assert_eq!(content, "import sys\r\n\r\nprint(sys)\r\n");
}

#[test]
fn organize_with_cr_only_line_endings_keeps_code_after_the_last_import() {
    let (_, content) = organize_file(
        "cr_organize.ts",
        "import { b } from \"./b\";\rimport { a } from \"./a\"; // about a\rexport const all = [a, b];\r",
    );
    // The organized block is joined with `\n`; what matters is that the code
    // after the last import stays after the imports and is not moved into
    // the block as part of the last import's comment.
    assert!(
        content.starts_with("import { a } from \"./a\"; // about a\n"),
        "{content:?}"
    );
    assert!(
        content.ends_with("import { b } from \"./b\";\rexport const all = [a, b];\r"),
        "{content:?}"
    );
}

// ---------------------------------------------------------------------------
// Lua long comments with level markers (`--[=[ ... ]=]`)
// ---------------------------------------------------------------------------

#[test]
fn remove_lua_keeps_code_after_a_leveled_long_comment_on_the_import_line() {
    let (resp, content) = remove_from_file(
        "leveled.lua",
        "local a = require(\"a\") --[==[ note ]==] local z = 1\nprint(z)\n",
        "a",
    );
    assert_eq!(resp["success"], true, "remove should succeed: {resp:?}");
    assert!(!content.contains("require(\"a\")"), "{content:?}");
    assert!(
        content.contains("--[==[ note ]==] local z = 1\nprint(z)\n"),
        "code after the long comment must stay: {content:?}"
    );
}

#[test]
fn remove_lua_takes_a_same_line_leveled_long_comment_with_the_import() {
    let (resp, content) = remove_from_file(
        "leveled_tail.lua",
        "local a = require(\"a\") --[=[ about a ]=]\nprint(1)\n",
        "a",
    );
    assert_eq!(resp["success"], true, "remove should succeed: {resp:?}");
    assert_eq!(content, "print(1)\n");
}

#[test]
fn organize_lua_accepts_a_multi_line_leveled_long_comment_between_imports() {
    let source =
        "local b = require(\"b\") --[==[ about\nb ]==]\nlocal a = require(\"a\")\n\nprint(a, b)\n";
    let (_, content) = organize_file("leveled_gap.lua", source);
    assert_eq!(
        content, source,
        "a long comment spanning lines stays where it is and splits the sort"
    );
}
