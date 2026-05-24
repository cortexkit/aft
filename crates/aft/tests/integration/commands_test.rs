//! Integration tests for the outline command through the binary protocol.

use std::fs::{self, File};
use std::path::{Path, PathBuf};

use super::helpers::{fixture_path, AftProcess};

fn write_temp_file(root: &Path, relative: &str, content: &str) -> PathBuf {
    let path = root.join(relative);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).expect("create parent directories");
    }
    fs::write(&path, content).expect("write temp file");
    path
}

#[test]
fn read_allows_explicit_ranges_from_large_files() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("large.txt");
    let mut content = String::from("first\nsecond\nthird\n");
    content.push_str(&"x".repeat(51 * 1024 * 1024));
    fs::write(&path, content).expect("write large file");

    let mut aft = AftProcess::spawn();
    let resp = aft.send(
        &serde_json::json!({
            "id": "read-large-range",
            "command": "read",
            "file": path,
            "start_line": 1,
            "end_line": 2,
        })
        .to_string(),
    );

    assert_eq!(resp["success"], true, "read should succeed: {resp:?}");
    assert_eq!(resp["content"], "1: first\n2: second\n");
    assert_eq!(resp["complete"], false, "range read is partial: {resp:?}");
    assert_eq!(
        resp["truncated"], true,
        "range read should flag gap: {resp:?}"
    );
    assert_eq!(
        resp["total_lines"], 4,
        "total_lines must scan to EOF: {resp:?}"
    );

    assert!(aft.shutdown().success());
}

#[test]
fn read_range_reports_partial_and_accurate_total_lines() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("hundred.txt");
    let content = (1..=100)
        .map(|n| format!("line {n}"))
        .collect::<Vec<_>>()
        .join("\n");
    fs::write(&path, content).expect("write ranged fixture");

    let mut aft = AftProcess::spawn();
    let resp = aft.send(
        &serde_json::json!({
            "id": "read-range-partial",
            "command": "read",
            "file": path,
            "start_line": 1,
            "end_line": 10,
        })
        .to_string(),
    );

    assert_eq!(resp["success"], true, "read should succeed: {resp:?}");
    assert_eq!(resp["complete"], false, "range read is partial: {resp:?}");
    assert_eq!(
        resp["truncated"], true,
        "range read should flag gap: {resp:?}"
    );
    assert_eq!(
        resp["total_lines"], 100,
        "total_lines must be full file count: {resp:?}"
    );
    assert_eq!(
        resp["lines_read"], 10,
        "should return requested slice: {resp:?}"
    );

    assert!(aft.shutdown().success());
}

#[test]
fn read_directory_truncation_reports_partial() {
    let dir = tempfile::tempdir().unwrap();
    for i in 0..1001 {
        fs::write(dir.path().join(format!("entry-{i:04}.txt")), "x").expect("write entry");
    }

    let mut aft = AftProcess::spawn();
    let resp = aft.send(
        &serde_json::json!({
            "id": "read-dir-partial",
            "command": "read",
            "file": dir.path(),
        })
        .to_string(),
    );

    assert_eq!(
        resp["success"], true,
        "directory read should succeed: {resp:?}"
    );
    assert_eq!(
        resp["complete"], false,
        "truncated directory is partial: {resp:?}"
    );
    assert_eq!(
        resp["truncated"], true,
        "directory should flag truncation: {resp:?}"
    );
    assert_eq!(
        resp["total_entries"], 1001,
        "must include total entries: {resp:?}"
    );

    assert!(aft.shutdown().success());
}

#[test]
fn test_outline_typescript_nested_structure() {
    let mut aft = AftProcess::spawn();
    let file = fixture_path("sample.ts");

    let resp = aft.send(&format!(
        r#"{{"id":"ol-1","command":"outline","file":"{}"}}"#,
        file.display()
    ));

    assert_eq!(resp["id"], "ol-1");
    assert_eq!(resp["success"], true, "outline should succeed");

    let text = resp["text"]
        .as_str()
        .expect("text field should be a string");

    // UserService should be at top level (no "." prefix in its line)
    let user_service_line = text
        .lines()
        .find(|l| l.contains("UserService"))
        .expect("UserService should be in outline");
    assert!(
        !user_service_line.contains('.'),
        "UserService should be at top level (no '.' prefix), got: {:?}",
        user_service_line
    );

    // getUser should be a nested member (its line has a "." prefix)
    let get_user_line = text
        .lines()
        .find(|l| l.contains("getUser"))
        .expect("getUser should be in outline");
    assert!(
        get_user_line.contains('.'),
        "getUser should be a nested member (has '.' prefix), got: {:?}",
        get_user_line
    );

    // addUser should be a nested member (its line has a "." prefix)
    let add_user_line = text
        .lines()
        .find(|l| l.contains("addUser"))
        .expect("addUser should be in outline");
    assert!(
        add_user_line.contains('.'),
        "addUser should be a nested member (has '.' prefix), got: {:?}",
        add_user_line
    );

    // Verify all expected symbol kind abbreviations are present
    assert!(text.contains(" fn "), "should have fn (function) kind");
    assert!(text.contains(" cls "), "should have cls (class) kind");
    assert!(text.contains(" ifc "), "should have ifc (interface) kind");
    assert!(text.contains(" enum "), "should have enum kind");
    assert!(
        text.contains(" type "),
        "should have type (type_alias) kind"
    );

    // greet should be exported: its line's first non-space chars are "E "
    let greet_line = text
        .lines()
        .find(|l| l.contains("greet") && !l.contains("UserService"))
        .expect("greet line should be in outline");
    assert!(
        greet_line.trim_start().starts_with("E "),
        "greet should be exported, got: {:?}",
        greet_line
    );

    // internalHelper should not be exported: its line's first non-space chars are "- "
    let internal_line = text
        .lines()
        .find(|l| l.contains("internalHelper"))
        .expect("internalHelper line should be in outline");
    assert!(
        internal_line.trim_start().starts_with("- "),
        "internalHelper should not be exported, got: {:?}",
        internal_line
    );

    let status = aft.shutdown();
    assert!(status.success());
}

#[test]
fn test_outline_python_multi_level_nesting() {
    let mut aft = AftProcess::spawn();
    let file = fixture_path("sample.py");

    let resp = aft.send(&format!(
        r#"{{"id":"ol-py","command":"outline","file":"{}"}}"#,
        file.display()
    ));

    assert_eq!(resp["success"], true, "outline should succeed for Python");

    let text = resp["text"]
        .as_str()
        .expect("text field should be a string");

    // OuterClass should be present at top level (2-space indent: starts with "  E" or "  -")
    let outer_class_line = text
        .lines()
        .find(|l| l.contains("OuterClass"))
        .expect("OuterClass should be in outline");
    assert!(
        outer_class_line.starts_with("  E") || outer_class_line.starts_with("  -"),
        "OuterClass should be at top level (2-space indent), got: {:?}",
        outer_class_line
    );

    // InnerClass should be nested, NOT at top level
    let inner_class_line = text
        .lines()
        .find(|l| l.contains("InnerClass"))
        .expect("InnerClass should be in outline");
    assert!(
        !(inner_class_line.starts_with("  E") || inner_class_line.starts_with("  -")),
        "InnerClass should be nested, not at top level, got: {:?}",
        inner_class_line
    );

    // inner_method should be nested, NOT at top level
    let inner_method_line = text
        .lines()
        .find(|l| l.contains("inner_method"))
        .expect("inner_method should be in outline");
    assert!(
        !(inner_method_line.starts_with("  E") || inner_method_line.starts_with("  -")),
        "inner_method should be nested, not at top level, got: {:?}",
        inner_method_line
    );

    // outer_method should be present as a member of OuterClass
    assert!(
        text.contains("outer_method"),
        "outer_method should be in outline"
    );

    let status = aft.shutdown();
    assert!(status.success());
}

#[test]
fn test_outline_missing_file() {
    let mut aft = AftProcess::spawn();

    let resp =
        aft.send(r#"{"id":"ol-miss","command":"outline","file":"/nonexistent/path/to/file.ts"}"#);

    assert_eq!(resp["success"], false);
    assert_eq!(resp["code"], "file_not_found");
    assert!(resp["message"].as_str().unwrap().contains("file not found"));

    // Process should still be alive
    let resp = aft.send(r#"{"id":"alive","command":"ping"}"#);
    assert_eq!(resp["success"], true);

    let status = aft.shutdown();
    assert!(status.success());
}

#[test]
fn test_outline_missing_param() {
    let mut aft = AftProcess::spawn();

    let resp = aft.send(r#"{"id":"ol-nop","command":"outline"}"#);

    assert_eq!(resp["success"], false);
    assert_eq!(resp["code"], "invalid_request");
    assert!(resp["message"].as_str().unwrap().contains("file"));

    let status = aft.shutdown();
    assert!(status.success());
}

// ============================================================================
// Zoom command integration tests
// ============================================================================

#[test]
fn test_zoom_success_with_annotations() {
    let mut aft = AftProcess::spawn();
    let file = fixture_path("calls.ts");

    let resp = aft.send(&format!(
        r#"{{"id":"z-1","command":"zoom","file":"{}","symbol":"compute"}}"#,
        file.display()
    ));

    assert_eq!(resp["id"], "z-1");
    assert_eq!(resp["success"], true, "zoom should succeed: {:?}", resp);
    assert_eq!(resp["name"], "compute");
    assert_eq!(resp["kind"], "function");

    // Content should contain the function body
    let content = resp["content"].as_str().expect("content string");
    assert!(
        content.contains("function compute"),
        "content should have function declaration: {}",
        content
    );
    assert!(
        content.contains("helper(a)"),
        "content should contain call to helper: {}",
        content
    );

    // Range should be present
    assert!(resp["range"]["start_line"].is_number());
    assert!(resp["range"]["end_line"].is_number());

    // Annotations
    let calls_out = resp["annotations"]["calls_out"]
        .as_array()
        .expect("calls_out array");
    let out_names: Vec<&str> = calls_out
        .iter()
        .map(|c| c["name"].as_str().unwrap())
        .collect();
    assert!(
        out_names.contains(&"helper"),
        "compute should call helper: {:?}",
        out_names
    );

    let called_by = resp["annotations"]["called_by"]
        .as_array()
        .expect("called_by array");
    let by_names: Vec<&str> = called_by
        .iter()
        .map(|c| c["name"].as_str().unwrap())
        .collect();
    assert!(
        by_names.contains(&"orchestrate"),
        "orchestrate should call compute: {:?}",
        by_names
    );

    // Each CallRef should have a line number
    for cr in calls_out {
        assert!(cr["line"].is_number(), "CallRef should have line: {:?}", cr);
        assert!(
            cr["line"].as_u64().unwrap_or(0) >= 1,
            "calls_out line should be 1-based: {:?}",
            cr
        );
    }
    for cr in called_by {
        assert!(cr["line"].is_number(), "CallRef should have line: {:?}", cr);
        assert!(
            cr["line"].as_u64().unwrap_or(0) >= 1,
            "called_by line should be 1-based: {:?}",
            cr
        );
    }

    let helper_call = calls_out
        .iter()
        .find(|cr| cr["name"] == "helper")
        .expect("compute should call helper");
    assert_eq!(helper_call["line"], 8, "helper call should be 1-based");

    let orchestrate_caller = called_by
        .iter()
        .find(|cr| cr["name"] == "orchestrate")
        .expect("orchestrate should call compute");
    assert_eq!(
        orchestrate_caller["line"], 13,
        "caller annotation should be 1-based"
    );

    // Context lines
    let ctx_before = resp["context_before"]
        .as_array()
        .expect("context_before array");
    let ctx_after = resp["context_after"]
        .as_array()
        .expect("context_after array");
    assert!(ctx_before.len() <= 3, "default context_lines is 3");
    assert!(ctx_after.len() <= 3, "default context_lines is 3");

    let status = aft.shutdown();
    assert!(status.success());
}

#[test]
fn test_read_range_after_binary_sample_rewind() {
    let temp_dir = tempfile::tempdir().expect("create temp dir");
    let file = temp_dir.path().join("sample.txt");
    let mut content = "a".repeat(4096);
    content.push_str("\nline-two\nline-three\n");
    std::fs::write(&file, content).expect("write sample");

    let mut aft = AftProcess::spawn();
    let cfg = aft.configure(temp_dir.path());
    assert_eq!(cfg["success"], true, "configure should succeed: {:?}", cfg);

    let resp = aft.send(&format!(
        r#"{{"id":"read-range-rewind","command":"read","file":"{}","start_line":2,"end_line":3}}"#,
        file.display()
    ));

    assert_eq!(resp["success"], true, "read should succeed: {resp:?}");
    assert_eq!(resp["lines_read"], 2);
    let content = resp["content"].as_str().expect("content string");
    assert!(content.contains("2: line-two"), "content: {content}");
    assert!(content.contains("3: line-three"), "content: {content}");

    let status = aft.shutdown();
    assert!(status.success());
}

#[test]
fn test_read_binary_detection_after_single_open_refactor() {
    let temp_dir = tempfile::tempdir().expect("create temp dir");
    let file = temp_dir.path().join("binary.bin");
    std::fs::write(&file, [b'a', b'b', 0, b'c']).expect("write binary");

    let mut aft = AftProcess::spawn();
    let cfg = aft.configure(temp_dir.path());
    assert_eq!(cfg["success"], true, "configure should succeed: {:?}", cfg);

    let resp = aft.send(&format!(
        r#"{{"id":"read-binary-single-open","command":"read","file":"{}","start_line":1,"end_line":1}}"#,
        file.display()
    ));

    assert_eq!(
        resp["success"], true,
        "binary read should succeed: {resp:?}"
    );
    assert_eq!(resp["binary"], true);
    assert!(resp["message"]
        .as_str()
        .is_some_and(|message| message.contains("Binary file")));

    let status = aft.shutdown();
    assert!(status.success());
}

#[test]
fn test_read_rejects_files_larger_than_50mb() {
    let temp_dir = tempfile::tempdir().expect("create temp dir");
    let file = temp_dir.path().join("large.txt");
    let handle = File::create(&file).expect("create large file");
    handle
        .set_len((50 * 1024 * 1024 + 1) as u64)
        .expect("set sparse file length");

    let mut aft = AftProcess::spawn();
    let cfg = aft.configure(temp_dir.path());
    assert_eq!(cfg["success"], true, "configure should succeed: {:?}", cfg);

    let resp = aft.send(&format!(
        r#"{{"id":"read-large","command":"read","file":"{}"}}"#,
        file.display()
    ));

    assert_eq!(resp["success"], false, "read should fail: {:?}", resp);
    assert_eq!(resp["code"], "invalid_request");
    assert!(resp["message"]
        .as_str()
        .expect("message")
        .contains("Use start_line/end_line to read sections"));

    let status = aft.shutdown();
    assert!(status.success());
}

#[test]
fn test_read_handles_inverted_line_range() {
    // Regression for audit #1: read with start_line > end_line previously
    // panicked at `lines[start_idx..end_idx]` because end_idx was clamped
    // independently of start_idx. Must now return success with zero lines.
    let temp_dir = tempfile::tempdir().expect("create temp dir");
    let file = temp_dir.path().join("sample.txt");
    std::fs::write(&file, "one\ntwo\nthree\nfour\nfive\n").expect("write sample");

    let mut aft = AftProcess::spawn();
    let cfg = aft.configure(temp_dir.path());
    assert_eq!(cfg["success"], true, "configure should succeed: {:?}", cfg);

    let resp = aft.send(&format!(
        r#"{{"id":"read-inv","command":"read","file":"{}","start_line":8,"end_line":3}}"#,
        file.display()
    ));

    assert_eq!(
        resp["success"], true,
        "inverted range should not crash: {:?}",
        resp
    );
    assert_eq!(resp["lines_read"], 0);

    // Process must still be alive (would be dead on the old panic path).
    let ping = aft.send(r#"{"id":"alive","command":"ping"}"#);
    assert_eq!(ping["success"], true);

    let status = aft.shutdown();
    assert!(status.success());
}

#[test]
fn test_read_directory_caps_entries_at_one_thousand() {
    let temp_dir = tempfile::tempdir().expect("create temp dir");
    for index in 0..1005 {
        std::fs::write(temp_dir.path().join(format!("entry_{index:04}.txt")), "x")
            .expect("write directory entry");
    }

    let mut aft = AftProcess::spawn();
    let cfg = aft.configure(temp_dir.path());
    assert_eq!(cfg["success"], true, "configure should succeed: {cfg:?}");

    let resp = aft.send(&format!(
        r#"{{"id":"read-dir-cap","command":"read","file":"{}"}}"#,
        temp_dir.path().display()
    ));

    assert_eq!(
        resp["success"], true,
        "read directory should succeed: {resp:?}"
    );
    assert_eq!(resp["total_entries"], 1005);
    let entries = resp["entries"].as_array().expect("entries array");
    assert_eq!(entries.len(), 1001);
    assert_eq!(entries[0], "entry_0000.txt");
    assert_eq!(entries[999], "entry_0999.txt");
    assert_eq!(
        entries[1000],
        "\n... and 5 more entries (truncated, showing first 1000)"
    );

    let status = aft.shutdown();
    assert!(status.success());
}

#[test]
fn test_zoom_symbol_not_found() {
    let mut aft = AftProcess::spawn();
    let file = fixture_path("calls.ts");

    let resp = aft.send(&format!(
        r#"{{"id":"z-nf","command":"zoom","file":"{}","symbol":"nonexistent_fn"}}"#,
        file.display()
    ));

    assert_eq!(resp["success"], false);
    assert_eq!(resp["code"], "symbol_not_found");
    assert!(resp["message"].as_str().unwrap().contains("nonexistent_fn"));

    // Process should still be alive
    let resp = aft.send(r#"{"id":"alive","command":"ping"}"#);
    assert_eq!(resp["success"], true);

    let status = aft.shutdown();
    assert!(status.success());
}

#[test]
fn test_zoom_code_symbol_lookup_keeps_hash_prefix_exact() {
    let dir = tempfile::tempdir().unwrap();
    let file = write_temp_file(
        dir.path(),
        "lib.rs",
        r#"#[derive(Debug)]
pub struct Thing;
"#,
    );

    let mut aft = AftProcess::spawn();
    aft.configure(dir.path());

    let resp = aft.send(&format!(
        r##"{{"id":"z-rust-hash","command":"zoom","file":"{}","symbol":"#[derive(Debug)]"}}"##,
        file.display()
    ));

    assert_eq!(
        resp["success"], false,
        "attribute text should not be normalized as a code symbol: {resp:?}"
    );
    assert_eq!(resp["code"], "symbol_not_found");
    assert!(resp["message"]
        .as_str()
        .unwrap()
        .contains("#[derive(Debug)]"));

    let status = aft.shutdown();
    assert!(status.success());
}

#[test]
fn test_zoom_context_lines_param() {
    let mut aft = AftProcess::spawn();
    let file = fixture_path("calls.ts");

    // Use context_lines=1
    let resp = aft.send(&format!(
        r#"{{"id":"z-cl","command":"zoom","file":"{}","symbol":"compute","context_lines":1}}"#,
        file.display()
    ));

    assert_eq!(resp["success"], true);

    let ctx_before = resp["context_before"].as_array().unwrap();
    let ctx_after = resp["context_after"].as_array().unwrap();
    assert!(
        ctx_before.len() <= 1,
        "context_before should be ≤1 with context_lines=1: {:?}",
        ctx_before
    );
    assert!(
        ctx_after.len() <= 1,
        "context_after should be ≤1 with context_lines=1: {:?}",
        ctx_after
    );

    let status = aft.shutdown();
    assert!(status.success());
}

#[test]
fn test_zoom_empty_annotations_arrays() {
    let mut aft = AftProcess::spawn();
    let file = fixture_path("calls.ts");

    // `unused` has no known callers and calls no known symbols
    let resp = aft.send(&format!(
        r#"{{"id":"z-empty","command":"zoom","file":"{}","symbol":"unused"}}"#,
        file.display()
    ));

    assert_eq!(resp["success"], true);

    // called_by must be empty array, not null
    let called_by = resp["annotations"]["called_by"]
        .as_array()
        .expect("called_by should be array, not null");
    assert!(called_by.is_empty());

    // calls_out must be an array (possibly empty depending on known symbols)
    let calls_out = resp["annotations"]["calls_out"]
        .as_array()
        .expect("calls_out should be array, not null");
    // It's fine if calls_out has items — what matters is it's an array
    let _ = calls_out;

    let status = aft.shutdown();
    assert!(status.success());
}

#[test]
fn test_zoom_supports_c_symbols() {
    let dir = tempfile::tempdir().expect("create temp dir");
    let file = write_temp_file(
        dir.path(),
        "src/sample.c",
        "int compute(int value) {\n    return value + 1;\n}\n",
    );

    let mut aft = AftProcess::spawn();
    let cfg = aft.configure(dir.path());
    assert_eq!(cfg["success"], true, "configure should succeed: {cfg:?}");

    let resp = aft.send(&format!(
        r#"{{"id":"zoom-c","command":"zoom","file":"{}","symbol":"compute"}}"#,
        file.display()
    ));

    assert_eq!(resp["success"], true, "zoom should succeed: {resp:?}");
    let content = resp["content"].as_str().expect("content string");
    assert!(content.contains("int compute(int value)"));
    assert!(content.contains("return value + 1;"));

    let status = aft.shutdown();
    assert!(status.success());
}

#[test]
fn test_zoom_supports_cpp_symbols() {
    let dir = tempfile::tempdir().expect("create temp dir");
    let file = write_temp_file(
        dir.path(),
        "src/sample.cpp",
        "class Worker {\npublic:\n    void run() {}\n};\n",
    );

    let mut aft = AftProcess::spawn();
    let cfg = aft.configure(dir.path());
    assert_eq!(cfg["success"], true, "configure should succeed: {cfg:?}");

    let resp = aft.send(&format!(
        r#"{{"id":"zoom-cpp","command":"zoom","file":"{}","symbol":"Worker"}}"#,
        file.display()
    ));

    assert_eq!(resp["success"], true, "zoom should succeed: {resp:?}");
    let content = resp["content"].as_str().expect("content string");
    assert!(content.contains("class Worker"));
    assert!(content.contains("void run()"));

    let status = aft.shutdown();
    assert!(status.success());
}

#[test]
fn test_zoom_supports_zig_symbols() {
    let dir = tempfile::tempdir().expect("create temp dir");
    let file = write_temp_file(
        dir.path(),
        "src/sample.zig",
        "fn greet(name: []const u8) void {\n    _ = name;\n}\n",
    );

    let mut aft = AftProcess::spawn();
    let cfg = aft.configure(dir.path());
    assert_eq!(cfg["success"], true, "configure should succeed: {cfg:?}");

    let resp = aft.send(&format!(
        r#"{{"id":"zoom-zig","command":"zoom","file":"{}","symbol":"greet"}}"#,
        file.display()
    ));

    assert_eq!(resp["success"], true, "zoom should succeed: {resp:?}");
    let content = resp["content"].as_str().expect("content string");
    assert!(content.contains("fn greet(name: []const u8) void"));
    assert!(content.contains("_ = name;"));

    let status = aft.shutdown();
    assert!(status.success());
}

#[test]
fn test_zoom_supports_csharp_symbols() {
    let dir = tempfile::tempdir().expect("create temp dir");
    let file = write_temp_file(
        dir.path(),
        "src/Sample.cs",
        "public class Worker\n{\n    public void Run()\n    {\n    }\n}\n",
    );

    let mut aft = AftProcess::spawn();
    let cfg = aft.configure(dir.path());
    assert_eq!(cfg["success"], true, "configure should succeed: {cfg:?}");

    let resp = aft.send(&format!(
        r#"{{"id":"zoom-csharp","command":"zoom","file":"{}","symbol":"Worker"}}"#,
        file.display()
    ));

    assert_eq!(resp["success"], true, "zoom should succeed: {resp:?}");
    let content = resp["content"].as_str().expect("content string");
    assert!(content.contains("public class Worker"));
    assert!(content.contains("public void Run()"));

    let status = aft.shutdown();
    assert!(status.success());
}
