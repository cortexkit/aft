use super::*;

use crate::protocol::RawRequest;
use serde_json::json;
use std::path::Path;

pub const ORIGINAL: &str = "export const value = 1;\n";

#[derive(Clone, Copy, Debug)]
pub enum Tool {
    Edit,
    Write,
    Batch,
    Patch,
    Ast,
    Import,
    Append,
    Symbol,
}

pub fn request(tool: Tool, path: &Path, number: usize) -> RawRequest {
    let (command, params) = match tool {
        Tool::Edit => (
            "edit_match",
            json!({"file": path, "match": "value = 1", "replacement": format!("value = {number}")}),
        ),
        Tool::Write => (
            "write",
            json!({"file": path, "content": format!("export const value = {number};\n")}),
        ),
        Tool::Batch => (
            "batch",
            json!({"file": path, "edits": [{"line_start": 1, "line_end": 1, "content": format!("export const value = {number};")}]}),
        ),
        Tool::Patch => (
            "apply_patch",
            json!({"patch_text": format!("*** Begin Patch\n*** Update File: {}\n@@\n-export const value = 1;\n+export const value = {number};\n*** End Patch", path.display())}),
        ),
        Tool::Ast => (
            "ast_replace",
            json!({"paths": [path], "lang": "typescript", "pattern": "const value = 1;", "rewrite": format!("const value = {number};"), "dry_run": false}),
        ),
        Tool::Import => (
            "add_import",
            json!({"file": path, "module": format!("module-{number}"), "names": [format!("Thing{number}")]}),
        ),
        Tool::Append => (
            "edit_match",
            json!({"file": path, "op": "append", "append_content": format!("// writer {number}\n")}),
        ),
        Tool::Symbol => (
            "edit_symbol",
            json!({"file": path, "symbol": "value", "operation": "replace", "content": format!("value = {number}")}),
        ),
    };
    let mut value = params;
    value["id"] = json!(format!("writer-{number}"));
    value["command"] = json!(command);
    value["session_id"] = json!(format!("session-{number}"));
    serde_json::from_value(value).unwrap()
}

pub fn assert_winner(tool: Tool, path: &Path, number: usize) {
    let content = std::fs::read_to_string(path).unwrap();
    match tool {
        Tool::Import => {
            assert!(content.contains(&format!("module-{number}")), "{content}");
            assert!(content.contains(&format!("Thing{number}")), "{content}");
            assert!(
                !content.contains(&format!("module-{}", 5 - number)),
                "{content}"
            );
            assert!(content.ends_with(ORIGINAL), "{content}");
        }
        Tool::Append => assert_eq!(content, format!("{ORIGINAL}// writer {number}\n")),
        _ => assert_eq!(content, format!("export const value = {number};\n")),
    }
}

#[test]
fn stale_write_message_and_code() {
    let error = crate::error::AftError::FileChangedSinceRead {
        path: "src/example.ts".into(),
    };
    assert_eq!(
        error.to_error_json(),
        json!({
            "code": "file_changed_since_read",
            "message": "src/example.ts changed on disk after this call read it; nothing was written. Read the file again and retry."
        })
    );
}
