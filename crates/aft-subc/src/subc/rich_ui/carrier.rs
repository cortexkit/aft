//! Closed carrier results: handler text, arguments and session sidecars never
//! participate in an error reply, including errors sent before tool dispatch.

use serde_json::{json, Value};

use super::{operations, schema};
use crate::protocol::Response;

pub(crate) const ANSWER_BYTES: usize = 1_048_576;

fn error_schema() -> Value {
    json!({"type":"record", "fields": {
        "success": {"type":"boolean"},
        "code": {"type":"string", "maxBytes":1024},
        "message": {"type":"string", "maxBytes":1024}
    }})
}

pub(crate) fn result_schema(entry: &Value) -> Value {
    if entry["kind"] == "page" {
        json!({"type":"record", "fields": {
            "items": {"type":"list", "items":entry["item"], "maxItems":entry["page"]["limitMax"]},
            "next": {"type":"string", "maxBytes":entry["page"]["cursorBytes"], "nullable":true}
        }})
    } else {
        entry["result"].clone()
    }
}

pub(crate) fn error_body(code: &str) -> Value {
    json!({
        "content": [{"type":"text", "text":code}], "isError":true,
        "structuredContent": {"success":false, "code":code, "message":operations::fixed_message(code)}
    })
}

/// Projection precedes validation and the compact answer cap. Errors bypass
/// the success schema and keep only the code plus a fixed, request-free text.
pub(crate) fn response_body(entry: &Value, response: &Response) -> Value {
    if !response.success {
        return error_body(response.data["code"].as_str().unwrap_or("unknown"));
    }
    let schema = result_schema(entry);
    let projected = if entry["kind"] == "page" {
        operations::project(&schema, &response.data)
    } else {
        operations::project_reply(entry, &response.data)
    };
    if schema::validate(&schema, &projected).is_err() {
        return error_body("result_schema_mismatch");
    }
    let limit = entry["limits"]["answerBytes"]
        .as_u64()
        .unwrap_or(ANSWER_BYTES as u64) as usize;
    if serde_json::to_vec(&projected).map_or(true, |bytes| bytes.len() > limit) {
        return error_body("answer_too_large");
    }
    json!({"content":[{"type":"text", "text":""}], "isError":false, "structuredContent":projected})
}

/// Check both the rich-UI schema and the exact carrier body, not the ordinary
/// tool-result/v1 conformance case (which adds fields to structuredContent).
pub(crate) fn validate_body(entry: Option<&Value>, body: &Value) -> Result<(), &'static str> {
    let fields = body.as_object().ok_or("body is not a record")?;
    if fields.len() != 3
        || !["content", "isError", "structuredContent"]
            .iter()
            .all(|key| fields.contains_key(*key))
    {
        return Err("unexpected body fields");
    }
    let is_error = body["isError"].as_bool().ok_or("isError is not boolean")?;
    let structured = &body["structuredContent"];
    let text = if is_error {
        schema::validate(&error_schema(), structured).map_err(|_| "invalid failure record")?;
        let code = structured["code"].as_str().ok_or("missing code")?;
        if structured["success"] != false
            || structured["message"] != operations::fixed_message(code)
        {
            return Err("unsafe failure record");
        }
        code
    } else {
        let entry = entry.ok_or("success requires an operation")?;
        schema::validate(&result_schema(entry), structured)
            .map_err(|_| "invalid success record")?;
        ""
    };
    if body["content"] != json!([{"type":"text", "text":text}]) {
        return Err("invalid content");
    }
    Ok(())
}

#[cfg(test)]
#[path = "carrier/tests.rs"]
mod tests;
