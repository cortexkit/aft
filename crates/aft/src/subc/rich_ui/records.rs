//! Immutable display bodies. None of these values are part of a tool reply.
use std::collections::BTreeMap;

use cortexkit_role_tool_display::{canonical_body, FieldKind, RecordSchema};
use serde_json::{json, Map, Value};

pub(crate) fn bash_schema() -> RecordSchema {
    RecordSchema::new("aft/bash/1", 1, BTreeMap::from([
        ("taskId".into(), FieldKind::Id),
        ("phase".into(), FieldKind::Code),
        ("exitCode".into(), FieldKind::Integer),
        ("outputBytes".into(), FieldKind::Integer),
        ("endReason".into(), FieldKind::Code),
        ("runDurationMs".into(), FieldKind::Integer),
        ("execution".into(), FieldKind::Code),
        ("target".into(), FieldKind::Code),
    ]))
}

pub(crate) fn files_schema() -> RecordSchema {
    let mut fields = BTreeMap::new();
    for name in ["fileCount", "modifiedCount", "createdCount", "deletedCount", "movedCount", "addedLines", "removedLines"] {
        fields.insert(name.into(), FieldKind::Integer);
    }
    fields.insert("path".into(), FieldKind::ProjectPath);
    RecordSchema::new("aft/files/1", 1, fields)
}

pub(crate) fn object(value: Value) -> Map<String, Value> {
    value.as_object().expect("display body is an object").clone()
}

fn size(value: &Value) -> usize {
    canonical_body(value.as_object().expect("display object")).map_or(usize::MAX, |v| v.len())
}

/// Budget the escaped canonical document, including omission counters, not just
/// the unescaped text. Security omissions must be made before this size cut.
pub(crate) fn cut_detail(mut body: Value, limit: usize) -> Option<Value> {
    if size(&body) <= limit { return Some(body); }
    let mut omitted_bytes = 0usize;
    let mut omitted_items = 0usize;
    body["truncated"] = json!(true);
    body["omittedItems"] = json!(0);
    body["omittedBytes"] = json!(0);
    if body.get("files").is_some() {
        let files = body["files"].as_array_mut()?;
        // Stable path/operation order is retained. Drop diff content before an
        // entry, so a large diff cannot hide the other changed files.
        for file in files.iter_mut().rev() {
            if let Some(diff) = file.as_object_mut()?.remove("diff") {
                omitted_bytes += diff.as_str()?.len();
                file["diffOmitted"] = json!(true);
            }
        }
        body["omittedBytes"] = json!(omitted_bytes);
        while size(&body) > limit {
            let file = body["files"].as_array_mut()?.pop()?;
            if let Some(diff) = file.get("diff").and_then(Value::as_str) { omitted_bytes += diff.len(); }
            omitted_items += 1;
            body["omittedItems"] = json!(omitted_items);
            body["omittedBytes"] = json!(omitted_bytes);
        }
    } else {
        let names = ["stderr", "stdout", "pty"];
        for name in names {
            while size(&body) > limit {
                let Some(text) = body.get(name).and_then(Value::as_str) else { break; };
                if text.is_empty() { break; }
                let mut keep = text.len().saturating_sub((size(&body) - limit).max(1));
                while !text.is_char_boundary(keep) { keep -= 1; }
                omitted_bytes += text.len() - keep;
                let kept = text[..keep].to_owned();
                body[name] = json!(kept);
                body["omittedBytes"] = json!(omitted_bytes);
            }
        }
    }
    (size(&body) <= limit).then_some(body)
}
