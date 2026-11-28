//! Compatibility with the exact catalog entry pinned on the sending computer.

use super::*;
use cortexkit_role_tool_provider::call::{ToolCallRequest, VersionedSchemaPin};
use std::sync::Arc;

fn spellings(tool: &CatalogTool, name: &str) -> bool {
    tool.name == name || crate::tool_gate::canonical_tool_name(&tool.name) == Some(name)
}

#[cfg(test)]
pub(in crate::subc) fn hashline_edit_pin() -> String {
    let tool = &RUNNER_HEAD_TOOLS.hashline_edit.catalog;
    SchemaPin::new(&tool.name, &tool.schema_digest, tool.semantics)
        .encode()
        .unwrap()
}

pub(in crate::subc) fn preset_serves(preset: CatalogPreset, name: &str) -> bool {
    served_tools(preset)
        .iter()
        .any(|tool| spellings(&tool.catalog, name))
        || (preset == CatalogPreset::Head && spellings(&RUNNER_HEAD_TOOLS.bash_watch.catalog, name))
}

pub(in crate::subc) fn transport(preset: CatalogPreset) -> (bool, Option<bool>) {
    // Read these declarations from the same catalog as the local provider. They
    // depend on the preset, not on the session's settings or tool arguments.
    static DECLARATIONS: [OnceLock<(bool, Option<bool>)>; 3] = [const { OnceLock::new() }; 3];
    *DECLARATIONS[preset as usize].get_or_init(|| {
        let (request, _) = validate_catalog_request(json!({"preset": preset.name()}))
            .expect("a built-in preset is valid");
        let answer = catalog_for_request(
            request,
            preset,
            &[],
            false,
            false,
            false,
            crate::config::DEFAULT_BASH_WORKER_WAIT_MAX_MS,
        )
        .expect("a built-in preset renders");
        let late_results = answer["capabilities"][session_capabilities::LATE_RESULTS] == true;
        let bash_yield = answer["tools"].as_array().and_then(|tools| {
            tools
                .iter()
                .find(|tool| tool["name"] == "bash")
                .map(|tool| {
                    tool["capabilities"].as_array().is_some_and(|tags| {
                        tags.iter()
                            .any(|tag| tag == super::super::call_yield::CALL_YIELD_TOOL_TAG)
                    })
                })
        });
        (late_results, bash_yield)
    })
}

pub(in crate::subc) fn parse(call: &ToolCallRequest) -> Result<VersionedSchemaPin, ErrorBody> {
    let encoded = call.schema_pin.as_ref().ok_or_else(|| {
        errors::invalid_request("schema_pin", "remote tool calls require a schema pin")
    })?;
    VersionedSchemaPin::parse(encoded)
        .map_err(|error| errors::invalid_request("schema_pin", error.to_string()))
}

pub(in crate::subc) fn validate(
    call: &ToolCallRequest,
    pin: &VersionedSchemaPin,
) -> Result<(), ErrorBody> {
    let matched = versions::current_pin_tools()
        .filter(|tool| spellings(tool, &call.name))
        .find(|tool| match pin {
            VersionedSchemaPin::Tp1(pin) => {
                pin.schema_digest == tool.schema_digest && pin.semantics == tool.semantics
            }
            VersionedSchemaPin::Tp2(pin) => {
                pin.tool == call.name
                    && pin.schema_digest == tool.schema_digest
                    && tool
                        .version
                        .is_some_and(|version| version.major == pin.major)
            }
            _ => false,
        })
        .ok_or_else(|| {
            ErrorBody::new(
                "remote_tool_versions_differ",
                "This tool differs between the AFT versions on the two computers.",
            )
        })?;
    let arguments = call
        .arguments
        .as_object()
        .ok_or_else(|| errors::invalid_request("arguments", "arguments must be an object"))?;
    let properties = matched.input_schema["properties"]
        .as_object()
        .expect("embedded tool schema");
    for key in arguments.keys() {
        let property = properties.get(key).ok_or_else(|| {
            errors::invalid_request(key, "argument is absent from the served schema")
        })?;
        if property["x-ck-audience"].as_str() == Some("host") {
            return Err(errors::invalid_request(key, "host-only argument"));
        }
    }
    // Remote admission runs off the frame loop. Cache exact validators by
    // structural digest; descriptions and behavior-only changes don't affect them.
    static VALIDATORS: LazyLock<Mutex<HashMap<String, Arc<jsonschema::Validator>>>> =
        LazyLock::new(|| Mutex::new(HashMap::new()));
    let validator = VALIDATORS
        .lock()
        .unwrap()
        .entry(matched.schema_digest.clone())
        .or_insert_with(|| {
            Arc::new(
                jsonschema::validator_for(&matched.input_schema).expect("embedded schema is valid"),
            )
        })
        .clone();
    if let Some(error) = validator.iter_errors(&call.arguments).next() {
        let field = error.instance_path.to_string();
        return Err(errors::invalid_request(
            if field.is_empty() {
                "arguments"
            } else {
                field.trim_start_matches('/')
            },
            error.to_string(),
        ));
    }
    Ok(())
}
