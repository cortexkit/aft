//! Versioned, closed carrier schemas. Projection selects only declared fields;
//! validation, rather than a default value, detects missing handler fields.

use serde_json::{json, Map, Value};

pub(crate) const RICH_UI_PROVIDER_VERSION: &str = "1.0";
pub(crate) const GENERIC_MESSAGE: &str = "The requested read could not be completed.";
pub(crate) const FIXED_MESSAGES: &[(&str, &str)] = &[
    (
        "rich_ui_op_malformed",
        "A versioned rich UI operation is required.",
    ),
    (
        "operation_version_unsupported",
        "This operation version is not published.",
    ),
    (
        "operation_target_mismatch",
        "The operation does not target this tool.",
    ),
    (
        "bound_field_supplied",
        "Session fields are supplied by the carrier binding.",
    ),
    ("args_invalid", "The operation arguments are invalid."),
    (
        "session_not_bound",
        "The carried session has no matching policy route.",
    ),
    (
        "session_route_ambiguous",
        "The carried session has conflicting policy routes.",
    ),
    (
        "session_preset_ambiguous",
        "The carried session has conflicting recorded roles.",
    ),
    (
        "unknown_tool",
        "This operation is not available to the session role.",
    ),
    (
        "tool_disabled",
        "This tool is disabled for the carried session.",
    ),
    (
        "task_not_found",
        "The viewed task was not found in the carried session.",
    ),
    ("database_unavailable", "The read database is unavailable."),
    ("database_busy", "The read database is busy."),
    (
        "digest_unknown",
        "The requested asset digest is not available.",
    ),
    (
        "chunk_index_out_of_range",
        "The requested chunk index is out of range.",
    ),
    (
        "principal_not_allowed",
        "This principal is not allowed to perform the read.",
    ),
    (
        "path_outside_root",
        "The path is outside the carried session project root.",
    ),
    (
        "path_outside_confined_root",
        "The path is outside the carried session confinement.",
    ),
    (
        "malformed_cursor",
        "The page cursor is invalid for this read.",
    ),
    (
        "result_schema_mismatch",
        "The read result does not match the published schema.",
    ),
    (
        "answer_too_large",
        "The read result exceeds the published answer limit.",
    ),
    (
        "unsupported_media",
        "This file type is not available as a rich UI text read.",
    ),
];

pub(crate) fn fixed_message(code: &str) -> &'static str {
    FIXED_MESSAGES
        .iter()
        .find(|(key, _)| *key == code)
        .map_or(GENERIC_MESSAGE, |(_, message)| *message)
}

fn string(max: usize) -> Value {
    json!({"type": "string", "maxBytes": max})
}
fn integer(min: i64, max: i64) -> Value {
    json!({"type": "integer", "min": min, "max": max})
}
fn boolean() -> Value {
    json!({"type": "boolean"})
}
fn optional(mut value: Value) -> Value {
    value["optional"] = json!(true);
    value
}
fn nullable(mut value: Value) -> Value {
    value["nullable"] = json!(true);
    value
}
fn list(items: Value, max: usize) -> Value {
    json!({"type": "list", "items": items, "maxItems": max})
}
fn record(fields: Value) -> Value {
    json!({"type": "record", "fields": fields})
}
fn enumeration(values: &[&str]) -> Value {
    json!({"type": "string", "maxBytes": 64, "enum": values})
}

pub(crate) fn operations() -> Vec<Value> {
    let text_result = || record(json!({"text": string(1_048_576)}));
    let mut entries = vec![
        query(
            "bash_status",
            "command.status",
            true,
            record(json!({})),
            record(json!({
                "running": boolean(), "exitCode": nullable(integer(-9007199254740991, 9007199254740991)),
            })),
        ),
        query(
            "bash_watch",
            "command.watch",
            true,
            record(json!({
                "pattern": string(1024), "regex": optional(boolean()),
                "timeoutMs": optional(integer(1, 20_000)),
            })),
            record(json!({
                "running": boolean(), "exitCode": nullable(integer(-9007199254740991, 9007199254740991)),
                "waited": record(json!({
                    "reason": enumeration(&["match", "exit", "timeout"]),
                    "limit_ms": integer(1, 20_000),
                })),
            })),
        ),
        query(
            "read",
            "files.read",
            false,
            record(json!({
                "path": string(8192), "startLine": optional(integer(1, 9007199254740991)),
                "endLine": optional(integer(1, 9007199254740991)),
                "offset": optional(integer(1, 9007199254740991)),
                "limit": optional(integer(1, 9007199254740991)),
            })),
            text_result(),
        ),
        query(
            "outline",
            "code.outline",
            false,
            record(json!({
                "target": string(8192), "files": optional(boolean()), "includeTests": optional(boolean()),
            })),
            text_result(),
        ),
        query(
            "zoom",
            "code.outline",
            false,
            record(json!({
                "batch": list(record(json!({"target": string(8192), "symbols": list(string(8192), 200)})), 200),
                "contextLines": optional(integer(1, 9007199254740991)), "callgraph": optional(boolean()),
            })),
            text_result(),
        ),
        query(
            "search",
            "code.search",
            false,
            record(json!({
                "query": optional(string(8192)), "pattern": optional(string(8192)),
                "includeTests": optional(boolean()), "offset": optional(integer(0, 100000)),
                "topK": optional(integer(1, 50)), "path": optional(string(8192)),
            })),
            text_result(),
        ),
        query(
            "grep",
            "code.search",
            false,
            record(json!({
                "pattern": string(8192), "include": optional(string(8192)),
                "path": optional(string(8192)), "offset": optional(integer(0, 100000)),
            })),
            text_result(),
        ),
        query(
            "glob",
            "code.search",
            false,
            record(json!({
                "pattern": string(8192), "path": optional(string(8192)),
            })),
            text_result(),
        ),
        query(
            "callgraph",
            "code.search",
            false,
            record(json!({
                "op": enumeration(&["callers", "impact", "call_tree", "trace_to", "trace_to_symbol", "trace_data"]),
                "path": string(8192), "symbol": string(8192),
                "depth": optional(integer(1, 9007199254740991)), "expression": optional(string(8192)),
                "includeTests": optional(boolean()), "includeUnresolved": optional(boolean()),
                "toPath": optional(string(8192)), "toSymbol": optional(string(8192)),
            })),
            text_result(),
        ),
    ];
    entries.push(json!({
        "id": "bash_output", "version": 1, "kind": "page", "class": "command.output",
        "target": {"module": "aft", "tool": "bash_status"}, "profiles": ["tool_page"],
        "scope": "call", "bound": {"task": "call"},
        "args": record(json!({"cursor": optional(string(1024)), "limit": optional(integer(1, 200))})),
        "item": record(json!({"stream": enumeration(&["stdout", "stderr", "pty"]),
            "text": string(8192), "bytes": integer(1, 8192),
            "continued": boolean(),
            "firstAvailableOffset": nullable(integer(0, 9007199254740991)),
            "prefixDropped": boolean()})),
        "limits": {"argsBytes": 8192, "timeoutMs": 30000},
        "page": {"limitMax": 200, "cursorBytes": 1024, "pageBytes": 262144, "itemKey": null, "total": false},
    }));
    entries
}

fn query(id: &str, class: &str, call: bool, args: Value, result: Value) -> Value {
    json!({"id": id, "version": 1, "kind": "query", "class": class,
        "target": {"module": "aft", "tool": id},
        "profiles": [if call { "tool_page" } else { "show_ui" }],
        "scope": if call { "call" } else { "session" },
        "bound": if call { json!({"task": "call"}) } else { json!({}) },
        "args": args, "result": result,
        "limits": {"argsBytes": 8192, "answerBytes": 1048576, "timeoutMs": 30000, "minRefreshSeconds": 2},
    })
}

/// Select fields recursively without inventing required values. The caller must
/// validate the candidate before it can leave in a carrier reply.
pub(crate) fn project(schema: &Value, source: &Value) -> Value {
    match schema["type"].as_str() {
        Some("record") if source.is_object() => {
            let mut result = Map::new();
            if let Some(fields) = schema["fields"].as_object() {
                for (key, field) in fields {
                    if let Some(value) = source.get(key) {
                        result.insert(key.clone(), project(field, value));
                    }
                }
            }
            Value::Object(result)
        }
        Some("list") if source.is_array() => Value::Array(
            source
                .as_array()
                .unwrap()
                .iter()
                .map(|value| project(&schema["items"], value))
                .collect(),
        ),
        _ => source.clone(),
    }
}

/// The tool-call dispatcher has already supplied formatted `text` for session
/// reads. Shell snapshots instead need their snake-case fields translated.
pub(crate) fn project_reply(operation: &Value, source: &Value) -> Value {
    let mut candidate = source.clone();
    if matches!(operation["id"].as_str(), Some("bash_status" | "bash_watch")) {
        let mut fields = Map::new();
        if let Some(status) = source.get("status").and_then(|value| {
            serde_json::from_value::<crate::bash_background::BgTaskStatus>(value.clone()).ok()
        }) {
            fields.insert("running".into(), json!(!status.is_terminal()));
        }
        if let Some(exit) = source.get("exit_code") {
            fields.insert("exitCode".into(), exit.clone());
        }
        if operation["id"] == "bash_watch" {
            if let Some(waited) = source.get("waited") {
                let reason = match waited.get("reason").and_then(Value::as_str) {
                    Some("matched") => "match",
                    Some("exited") => "exit",
                    Some("timeout") => "timeout",
                    // Leave an invalid candidate for result validation instead
                    // of forwarding an unknown handler value or inventing one.
                    _ => return Value::Null,
                };
                let mut waited = waited.clone();
                waited["reason"] = json!(reason);
                fields.insert("waited".into(), waited);
            }
        }
        candidate = Value::Object(fields);
    }
    project(&operation["result"], &candidate)
}

/// v1 carries text only, not attachments or binary navigation. Reject media
/// before the handler runs so a summary can never masquerade as file content.
pub(crate) fn media_refusal(operation: &str, kind: &str) -> Option<&'static str> {
    (matches!(operation, "read" | "outline" | "zoom") && matches!(kind, "image" | "pdf" | "binary"))
        .then_some("unsupported_media")
}

/// The host may display a running placeholder, but AFT persists only terminal
/// records. The record schema is separate from the manifest's closed view shape.
pub(crate) fn view_record_schema() -> Value {
    record(json!({
        "schema": enumeration(&["aft.bash.view"]),
        "version": integer(1, 1),
        "task_id": nullable(string(1024)),
        "schema_pin": nullable(string(1024)),
        "phase": enumeration(&["running", "finished"]),
        "exitCode": nullable(integer(-9007199254740991, 9007199254740991)),
        "durationMs": nullable(integer(0, 9007199254740991)),
        "outputBytes": nullable(integer(0, 9007199254740991)),
        "endReason": nullable(enumeration(&["completed", "failed", "killed", "timed_out", "fate_unknown"])),
        "templates": record(json!({"row": string(71), "page": string(71)})),
    }))
}

pub(crate) fn valid_template_digest(value: &str) -> bool {
    value.len() == 71
        && value.starts_with("sha256:")
        && value.as_bytes()[7..]
            .iter()
            .all(|byte| matches!(*byte, b'0'..=b'9' | b'a'..=b'f'))
}

/// Schema validation cannot express SHA-256 spelling or phase-dependent nulls.
/// Keep those checks here so stored records and host placeholders agree without
/// adding patterns or unions to the phone's bounded schema language.
pub(crate) fn validate_view_record(
    value: &Value,
    emitted_by_aft: bool,
) -> Result<(), &'static str> {
    const CODE: &str = "result_schema_mismatch";
    super::schema::validate(&view_record_schema(), value).map_err(|_| CODE)?;
    for key in ["row", "page"] {
        if !value["templates"][key]
            .as_str()
            .is_some_and(valid_template_digest)
        {
            return Err(CODE);
        }
    }
    if emitted_by_aft
        && (value["phase"] != "finished"
            || ["task_id", "schema_pin", "endReason"]
                .iter()
                .any(|key| value[*key].is_null()))
    {
        return Err(CODE);
    }
    if value["phase"] == "running"
        && [
            "task_id",
            "schema_pin",
            "endReason",
            "exitCode",
            "durationMs",
            "outputBytes",
        ]
        .iter()
        .any(|key| !value[*key].is_null())
    {
        return Err(CODE);
    }
    Ok(())
}
