//! Operation checks that extend `carrier_read_only::admit`, never replace it.
//! The read-only gate must run first, so its refusals retain their own frames.

use serde_json::Value;
use subc_protocol::ErrorBody;

use super::{chunks, schema};

#[derive(Debug)]
pub(crate) struct Operation {
    pub(crate) entry: Value,
    pub(crate) arguments: Value,
    pub(crate) task: Option<String>,
}

impl Operation {
    /// Query translation runs after the policy route and viewed task have been
    /// resolved. Pages use their dedicated backend, not bash_status rendering.
    pub(crate) fn translate_query(
        &self,
        task_id: Option<&str>,
        watch_sync_max_ms: u64,
    ) -> Result<Value, ErrorBody> {
        let mut arguments = self.arguments.clone();
        match self.entry["id"].as_str() {
            Some("bash_status" | "bash_output") => {
                arguments = serde_json::json!({"taskId":task_id.ok_or_else(|| chunks::refusal("task_not_found"))?});
            }
            Some("bash_watch") => {
                let args = arguments.as_object_mut().expect("operation args validated");
                let regex = args
                    .remove("regex")
                    .and_then(|value| value.as_bool())
                    .unwrap_or(false);
                if regex {
                    let pattern = args.remove("pattern").expect("pattern validated");
                    args.insert("pattern".into(), serde_json::json!({"regex":pattern}));
                }
                let timeout = args
                    .get("timeoutMs")
                    .and_then(Value::as_u64)
                    .unwrap_or(20_000)
                    .min(watch_sync_max_ms);
                args.insert("timeoutMs".into(), timeout.into());
                args.insert(
                    "taskId".into(),
                    task_id
                        .ok_or_else(|| chunks::refusal("task_not_found"))?
                        .into(),
                );
            }
            _ if self.entry["kind"] == "page" => {
                return Err(chunks::refusal("operation_version_unsupported"))
            }
            _ => {}
        }
        let call = serde_json::from_value(serde_json::json!({
            "name":self.entry["target"]["tool"], "arguments":arguments
        }))
        .map_err(|_| chunks::refusal("args_invalid"))?;
        super::super::tool_provider::admit_arguments(
            &call,
            super::super::tool_provider::CallerRole::Head,
            false,
        )?;
        Ok(arguments)
    }
}

/// Parse and validate the operation and arguments before looking up the
/// session's policy route or consulting its database.
pub(crate) fn prepare(name: &str, arguments: &Value) -> Result<Operation, ErrorBody> {
    let manifest = chunks::running_manifest()
        .and_then(|(_, bytes)| serde_json::from_slice::<Value>(&bytes).ok());
    prepare_with_manifest(name, arguments, manifest.as_ref())
}

fn prepare_with_manifest(
    name: &str,
    arguments: &Value,
    manifest: Option<&Value>,
) -> Result<Operation, ErrorBody> {
    let mut arguments = arguments
        .as_object()
        .cloned()
        .ok_or_else(|| chunks::refusal("rich_ui_op_malformed"))?;
    let encoded = arguments.remove("rich_ui_op");
    let (id, version) = encoded
        .as_ref()
        .and_then(Value::as_str)
        .and_then(parse_operation)
        .ok_or_else(|| chunks::refusal("rich_ui_op_malformed"))?;
    let entry = manifest
        .and_then(|manifest| manifest["operations"].as_array())
        .and_then(|entries| {
            entries
                .iter()
                .find(|entry| entry["id"] == id && entry["version"] == version)
        })
        .ok_or_else(|| chunks::refusal("operation_version_unsupported"))?;
    let name = crate::tool_gate::canonical_tool_name(name)
        .map(super::super::carrier_read_only::internal_name)
        .unwrap_or(name);
    if entry["target"]["module"] != "aft" || entry["target"]["tool"] != name {
        return Err(chunks::refusal("operation_target_mismatch"));
    }
    if ["session", "session_id", "sessionId"]
        .iter()
        .any(|key| arguments.contains_key(*key))
    {
        return Err(chunks::refusal("bound_field_supplied"));
    }
    let task = match (entry["scope"].as_str(), arguments.remove("task")) {
        (Some("call"), Some(Value::String(task))) => Some(task),
        (Some("session"), None) => None,
        _ => return Err(chunks::refusal("args_invalid")),
    };
    let arguments = Value::Object(arguments);
    let limit = entry["limits"]["argsBytes"].as_u64().unwrap_or(8192) as usize;
    if serde_json::to_vec(&arguments).map_or(true, |bytes| bytes.len() > limit)
        || schema::validate(&entry["args"], &arguments).is_err()
        || remote_target(name, &arguments)
    {
        return Err(chunks::refusal("args_invalid"));
    }
    Ok(Operation {
        entry: entry.clone(),
        arguments,
        task,
    })
}

fn parse_operation(encoded: &str) -> Option<(&str, u64)> {
    let (id, version) = encoded.split_once('@')?;
    if id.is_empty()
        || !id
            .bytes()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'_')
        || version.is_empty()
        || !version.bytes().all(|c| c.is_ascii_digit())
    {
        return None;
    }
    let version = version.parse().ok()?;
    (version > 0).then_some((id, version))
}

fn remote_target(name: &str, arguments: &Value) -> bool {
    fn remote(value: &Value) -> bool {
        value.as_str().is_some_and(|target| {
            let target = target.trim_start().to_ascii_lowercase();
            ["https://", "http://", "issue://", "pr://"]
                .iter()
                .any(|prefix| target.starts_with(prefix))
        }) || value
            .as_array()
            .is_some_and(|targets| targets.iter().any(remote))
    }
    match name {
        "read" | "grep" | "glob" | "search" => arguments.get("path").is_some_and(remote),
        "outline" => arguments.get("target").is_some_and(remote),
        "zoom" => arguments["batch"]
            .as_array()
            .is_some_and(|batch| batch.iter().any(|item| remote(&item["target"]))),
        "callgraph" => ["path", "toPath"]
            .iter()
            .any(|key| arguments.get(*key).is_some_and(remote)),
        _ => false,
    }
}

/// Ordinary v1 calls keep their shell, confinement and call-mode precedence.
pub(crate) fn reject_non_carrier(arguments: &Value) -> Result<(), ErrorBody> {
    if arguments
        .as_object()
        .is_some_and(|args| args.contains_key("rich_ui_op"))
    {
        Err(ErrorBody::new(
            "rich_ui_op_not_allowed",
            "Rich UI operations are available only on the rich UI carrier route.",
        ))
    } else {
        Ok(())
    }
}

#[cfg(test)]
#[path = "admission/tests.rs"]
mod tests;
