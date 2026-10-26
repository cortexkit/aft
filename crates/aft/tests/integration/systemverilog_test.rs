//! Structural support needs no installed compiler or language server.
use std::fs;

use serde_json::{json, Value};

use super::helpers::AftProcess;

fn project() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    for (name, source) in [
        (
            "config.svh",
            include_str!("../fixtures/systemverilog/config.svh"),
        ),
        (
            "types_pkg.sv",
            include_str!("../fixtures/systemverilog/types_pkg.sv"),
        ),
        (
            "stream_if.sv",
            include_str!("../fixtures/systemverilog/stream_if.sv"),
        ),
        (
            "register_slice.sv",
            include_str!("../fixtures/systemverilog/register_slice.sv"),
        ),
        ("top.sv", include_str!("../fixtures/systemverilog/top.sv")),
    ] {
        fs::write(dir.path().join(name), source).unwrap();
    }
    dir
}

fn send(aft: &mut AftProcess, request: Value) -> Value {
    aft.send(&request.to_string())
}

#[test]
fn systemverilog_design_outline_and_scoped_zoom() {
    let dir = project();
    let mut aft = AftProcess::spawn();
    assert_eq!(aft.configure(dir.path())["success"], true);
    for (file, expected) in [
        ("types_pkg.sv", "types_pkg"),
        ("stream_if.sv", "stream_if"),
        ("register_slice.sv", "register_slice"),
        ("top.sv", "top"),
    ] {
        let result = send(
            &mut aft,
            json!({"id":file, "command":"outline", "file":file}),
        );
        assert_eq!(result["success"], true, "{result:#}");
        assert!(
            result["text"].as_str().unwrap().contains(expected),
            "{result:#}"
        );
    }
    let zoom = send(
        &mut aft,
        json!({"id":"zoom", "command":"zoom",
        "file":"types_pkg.sv", "symbol":"types_pkg::increment", "context_lines":0}),
    );
    assert_eq!(zoom["success"], true, "{zoom:#}");
    assert_eq!(zoom["kind"], "function");
    assert!(zoom["content"]
        .as_str()
        .unwrap()
        .contains("return value + 1'b1;"));
    assert!(!zoom["content"]
        .as_str()
        .unwrap()
        .contains("task automatic report"));

    // Identical written names in different packages select independently.
    fs::write(dir.path().join("scopes.sv"), "package a; function int same(); return 1; endfunction endpackage\npackage b; function int same(); return 2; endfunction endpackage\n").unwrap();
    let scoped = send(
        &mut aft,
        json!({"id":"scoped", "command":"zoom",
        "file":"scopes.sv", "symbol":"b::same", "context_lines":0}),
    );
    assert_eq!(scoped["success"], true, "{scoped:#}");
    assert!(scoped["content"].as_str().unwrap().contains("return 2;"));
    let search = send(
        &mut aft,
        json!({"id":"source-search", "command":"tool_call", "name":"grep",
        "arguments":{"pattern":"import types_pkg", "include":"*.sv"}}),
    );
    assert_eq!(search["success"], true, "{search:#}");
    let text = search["text"].as_str().unwrap();
    assert!(
        text.contains("top.sv") && text.contains("register_slice.sv"),
        "{text}"
    );
    let headers = send(
        &mut aft,
        json!({"id":"headers", "command":"tool_call", "name":"glob",
        "arguments":{"pattern":"*.svh"}}),
    );
    assert_eq!(headers["success"], true, "{headers:#}");
    assert!(
        headers["text"].as_str().unwrap().contains("config.svh"),
        "{headers:#}"
    );
    assert!(aft.shutdown().success());
}

#[test]
fn systemverilog_public_ast_preview_apply_and_undo() {
    let dir = project();
    let path = dir.path().join("top.sv");
    let before = fs::read(&path).unwrap();
    let mut aft = AftProcess::spawn();
    assert_eq!(aft.configure(dir.path())["success"], true);
    let search = send(
        &mut aft,
        json!({"id":"search", "command":"tool_call",
        "name":"ast_search", "arguments":{"pattern":"increment($ARG)",
        "lang":"systemverilog", "paths":["top.sv"]}}),
    );
    assert_eq!(search["success"], true, "{search:#}");
    assert_eq!(search["total_matches"], 1, "{search:#}");
    for preview in [true, false] {
        let result = send(
            &mut aft,
            json!({"id":format!("replace-{preview}"), "command":"tool_call",
            "name":"ast_replace", "arguments":{"pattern":"increment($ARG)",
            "rewrite":"increment($ARG + 1'b1)", "lang":"verilog", "paths":["top.sv"], "dryRun":preview}}),
        );
        assert_eq!(result["success"], true, "{result:#}");
        if preview {
            assert_eq!(fs::read(&path).unwrap(), before);
        } else {
            assert!(fs::read_to_string(&path)
                .unwrap()
                .contains("increment(8'd41 + 1'b1)"));
        }
    }
    let undo = send(&mut aft, json!({"id":"undo", "command":"undo"}));
    assert_eq!(undo["success"], true, "{undo:#}");
    assert_eq!(fs::read(&path).unwrap(), before);

    let invalid = send(
        &mut aft,
        json!({"id":"invalid", "command":"ast_replace",
        "pattern":"increment($ARG)", "rewrite":"increment(", "lang":"systemverilog",
        "paths":["top.sv"], "dry_run":false}),
    );
    assert_eq!(invalid["success"], false, "{invalid:#}");
    assert_eq!(invalid["code"], "invalid_rewrite", "{invalid:#}");
    assert_eq!(fs::read(&path).unwrap(), before);
    assert!(aft.shutdown().success());
}

#[test]
fn systemverilog_scoped_symbol_edit_preserves_neighboring_routines() {
    let dir = project();
    let path = dir.path().join("types_pkg.sv");
    let before = fs::read(&path).unwrap();
    let mut aft = AftProcess::spawn();
    assert_eq!(aft.configure(dir.path())["success"], true);
    let edited = send(
        &mut aft,
        json!({"id":"symbol-edit", "command":"tool_call", "name":"edit",
        "arguments":{"path":"types_pkg.sv", "symbol":"types_pkg::increment",
        "content":"function automatic data_t increment(input data_t value);\n    return value + 2'd2;\n  endfunction"}}),
    );
    assert_eq!(edited["success"], true, "{edited:#}");
    assert_eq!(edited["syntax_valid"], true, "{edited:#}");
    let after = fs::read_to_string(&path).unwrap();
    assert!(after.contains("return value + 2'd2;"));
    assert!(after.contains("task automatic report"));
    assert!(after.contains("endpackage"));
    let undo = send(&mut aft, json!({"id":"undo-symbol", "command":"undo"}));
    assert_eq!(undo["success"], true, "{undo:#}");
    assert_eq!(fs::read(&path).unwrap(), before);
    assert!(aft.shutdown().success());
}

#[test]
fn systemverilog_text_edits_validate_syntax_and_undo_exactly() {
    let dir = project();
    let path = dir.path().join("register_slice.sv");
    let before = fs::read(&path).unwrap();
    let mut aft = AftProcess::spawn();
    assert_eq!(aft.configure(dir.path())["success"], true);
    let edit = send(
        &mut aft,
        json!({"id":"edit", "command":"tool_call", "name":"edit",
        "arguments":{"path":"register_slice.sv", "oldString":"logic full;", "newString":"logic full = 0;"}}),
    );
    assert_eq!(edit["success"], true, "{edit:#}");
    assert!(fs::read_to_string(&path)
        .unwrap()
        .contains("logic full = 0;"));
    let undo = send(
        &mut aft,
        json!({"id":"undo-edit", "command":"undo", "file":"register_slice.sv"}),
    );
    assert_eq!(undo["success"], true, "{undo:#}");
    assert_eq!(fs::read(&path).unwrap(), before);
    let broken = send(
        &mut aft,
        json!({"id":"broken", "command":"tool_call", "name":"edit",
        "arguments":{"path":"register_slice.sv", "oldString":"logic full;", "newString":"logic full = (;"}}),
    );
    assert_eq!(broken["syntax_valid"], false, "{broken:#}");
    assert_eq!(broken["rolled_back"], true, "{broken:#}");
    assert_eq!(fs::read(&path).unwrap(), before);

    let created = send(
        &mut aft,
        json!({"id":"new-broken", "command":"tool_call", "name":"write",
        "arguments":{"path":"broken.sv", "content":"module broken(\n"}}),
    );
    assert_eq!(created["success"], true, "{created:#}");
    assert_eq!(created["syntax_valid"], false, "{created:#}");
    assert!(
        created["text"].as_str().unwrap().contains("Syntax check:"),
        "{created:#}"
    );
    assert!(aft.shutdown().success());
}
