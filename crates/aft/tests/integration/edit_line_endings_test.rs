use std::fs;
use std::path::Path;

use serde_json::{json, Value};

use super::helpers::{user_config, AftProcess};

fn start_aft(root: &Path) -> AftProcess {
    let mut aft = AftProcess::spawn();
    let reply = aft.send(
        &json!({
            "id": "config",
            "command": "configure",
            "harness": "opencode",
            "project_root": root,
            "config": user_config(json!({ "format_on_edit": false })),
        })
        .to_string(),
    );
    assert_eq!(reply["success"], true, "{reply:?}");
    aft
}

fn assert_edit(request: Value, name: &str, before: &[u8], after: &[u8]) {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join(name);
    fs::write(&file, before).unwrap();
    let mut aft = start_aft(dir.path());
    let reply = aft.send(&request.to_string());
    assert_eq!(reply["success"], true, "{reply:?}");
    assert!(aft.shutdown().success());
    assert_eq!(fs::read(file).unwrap(), after);
}

fn match_edit(replacement: &str) -> Value {
    json!({
        "id": "edit", "command": "edit_match", "file": "notes.txt",
        "match": "bravo", "replacement": replacement,
    })
}

#[test]
fn match_preview_and_write_keep_crlf_without_changing_unmatched_bytes() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("notes.txt");
    let before = "alpha\rvalue\r\nbravo\r\ncharlie\nend\r\n";
    let after = "alpha\rvalue\r\nBRAVO\rinside\r\nagain\r\ncharlie\nend\r\n";
    fs::write(&file, before).unwrap();
    let mut aft = start_aft(dir.path());
    let mut request = match_edit("BRAVO\rinside\nagain");
    request["preview"] = json!(true);
    request["include_diff_content"] = json!(true);
    let reply = aft.send(&request.to_string());
    assert_eq!(reply["success"], true, "{reply:?}");
    assert_eq!(fs::read(&file).unwrap(), before.as_bytes());
    request["preview"] = json!(false);
    let written = aft.send(&request.to_string());
    assert_eq!(written["success"], true, "{written:?}");
    assert!(aft.shutdown().success());
    assert_eq!(
        (reply["diff"]["after"].as_str(), fs::read(file).unwrap()),
        (Some(after), after.as_bytes().to_vec()),
    );
}

#[test]
fn match_keeps_an_already_crlf_replacement() {
    assert_edit(
        match_edit("BRAVO\r\nagain"),
        "notes.txt",
        b"alpha\r\nbravo\r\ncharlie\r\n",
        b"alpha\r\nBRAVO\r\nagain\r\ncharlie\r\n",
    );
}

#[test]
fn match_keeps_lf_file_endings() {
    assert_edit(
        match_edit("BRAVO\nagain"),
        "notes.txt",
        b"alpha\nbravo\ncharlie\n",
        b"alpha\nBRAVO\nagain\ncharlie\n",
    );
}

#[test]
fn match_uses_lf_when_endings_are_tied() {
    assert_edit(
        match_edit("BRAVO\r\nagain"),
        "notes.txt",
        b"alpha\r\nbravo\ncharlie",
        b"alpha\r\nBRAVO\nagain\ncharlie",
    );
}

fn batch_edit(edits: Value) -> Value {
    json!({
        "id": "edit", "command": "tool_call", "name": "edit",
        "arguments": { "path": "notes.txt", "edits": edits },
    })
}

#[test]
fn batch_matches_keep_crlf() {
    assert_edit(
        batch_edit(json!([
            { "oldString": "bravo", "newString": "BRAVO\nsecond" },
            { "oldString": "delta", "newString": "DELTA\r\nfourth" },
        ])),
        "notes.txt",
        b"alpha\r\nbravo\r\ncharlie\r\ndelta\r\n",
        b"alpha\r\nBRAVO\r\nsecond\r\ncharlie\r\nDELTA\r\nfourth\r\n",
    );
}

#[test]
fn batch_line_ranges_keep_crlf() {
    assert_edit(
        batch_edit(json!([
            { "startLine": 2, "endLine": 2, "content": "BRAVO\nsecond" },
            { "startLine": 4, "endLine": 4, "content": "DELTA\r\nfourth\r\n" },
        ])),
        "notes.txt",
        b"alpha\r\nbravo\r\ncharlie\r\ndelta\r\n",
        b"alpha\r\nBRAVO\r\nsecond\r\ncharlie\r\nDELTA\r\nfourth\r\n",
    );
}

const SYMBOL_SOURCE: &[u8] = b"function greet() {\r\n  return 'hi';\r\n}\r\nconst keep = 1;\r\n";

fn symbol_edit(operation: &str, content: &str) -> Value {
    json!({
        "id": "edit", "command": "edit_symbol", "file": "greet.js",
        "symbol": "greet", "operation": operation, "content": content,
    })
}

#[test]
fn symbol_replace_keeps_crlf() {
    assert_edit(
        symbol_edit("replace", "function greet() {\n  return 'bye';\n}"),
        "greet.js",
        SYMBOL_SOURCE,
        b"function greet() {\r\n  return 'bye';\r\n}\r\nconst keep = 1;\r\n",
    );
}

#[test]
fn symbol_insert_before_keeps_crlf() {
    assert_edit(
        symbol_edit("insert_before", "// comment\r\nconst added = 2;"),
        "greet.js",
        SYMBOL_SOURCE,
        b"// comment\r\nconst added = 2;\r\nfunction greet() {\r\n  return 'hi';\r\n}\r\nconst keep = 1;\r\n",
    );
}

#[test]
fn symbol_insert_after_keeps_crlf() {
    assert_edit(
        symbol_edit("insert_after", "// comment\nconst added = 2;"),
        "greet.js",
        SYMBOL_SOURCE,
        b"function greet() {\r\n  return 'hi';\r\n}\r\n// comment\r\nconst added = 2;\r\nconst keep = 1;\r\n",
    );
}
