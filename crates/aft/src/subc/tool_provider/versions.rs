//! Tool versions are independent of the legacy catalog's semantics counters.

use std::collections::BTreeMap;
use std::sync::LazyLock;

use cortexkit_role_tool_provider::{
    call::{SchemaPin, ToolCallRequest, ToolMajorPin, VersionedSchemaPin},
    catalog::{CatalogTool, ToolVersion},
    errors,
};
use serde_json::json;
use subc_protocol::ErrorBody;

use super::{legacy_file_path_digests, served_tools, CallerRole, CatalogPreset, ServedTool};

const PUBLISH_TOOL_VERSIONS: bool = false;
const INITIAL: ToolVersion = ToolVersion::new(1, 0, 0);
const VERSIONS: &[(&str, ToolVersion)] = &[
    ("apply_patch", INITIAL),
    ("ast_replace", INITIAL),
    ("ast_search", INITIAL),
    ("bash", INITIAL),
    ("bash_kill", INITIAL),
    ("bash_status", INITIAL),
    ("bash_watch", INITIAL),
    ("bash_write", INITIAL),
    ("callgraph", INITIAL),
    ("conflicts", INITIAL),
    ("delete", INITIAL),
    ("edit", INITIAL),
    ("glob", INITIAL),
    ("grep", INITIAL),
    ("import", INITIAL),
    ("inspect", INITIAL),
    ("move", INITIAL),
    ("outline", INITIAL),
    ("powershell", INITIAL),
    ("read", INITIAL),
    ("safety", INITIAL),
    ("search", INITIAL),
    ("status", INITIAL),
    ("write", INITIAL),
    ("zoom", INITIAL),
];

// This history is frozen independently of today's schemas and version table.
// A version bump must not silently remap a legacy caller to the new major.
static SHIPPED: LazyLock<BTreeMap<String, u64>> = LazyLock::new(|| {
    serde_json::from_str(include_str!("shipped_pins.json")).expect("shipped tool pin history")
});

#[cfg(feature = "test-hooks")]
static VERSION_PROBE: LazyLock<Option<bool>> =
    LazyLock::new(
        || match std::env::var("AFT_TEST_CONFORMANCE_TOOL_VERSIONS").as_deref() {
            Ok("1") => Some(true),
            Ok("0") => Some(false),
            _ => None,
        },
    );

pub(super) fn version(name: &str) -> ToolVersion {
    let version = VERSIONS
        .iter()
        .find(|(tool, _)| *tool == name)
        .expect("every served tool has a version")
        .1;
    #[cfg(feature = "test-hooks")]
    if *VERSION_PROBE == Some(true) && name == "read" {
        return ToolVersion::new(version.major, version.minor + 1, version.patch);
    }
    version
}

pub(super) fn prepare_catalog(tools: &mut [CatalogTool]) {
    #[cfg(feature = "test-hooks")]
    let publish = VERSION_PROBE.unwrap_or(PUBLISH_TOOL_VERSIONS);
    #[cfg(not(feature = "test-hooks"))]
    let publish = PUBLISH_TOOL_VERSIONS;
    if !publish {
        for tool in tools {
            tool.version = None;
        }
    }
}

pub(super) fn admit_pin(
    call: &ToolCallRequest,
    encoded: &str,
    role: CallerRole,
    served: &ServedTool,
    validator_tool: &CatalogTool,
    hashline_edit: bool,
) -> Result<(), ErrorBody> {
    let pin = VersionedSchemaPin::parse(encoded)
        .map_err(|error| errors::invalid_request("schema_pin", error.to_string()))?;
    let head_watch = (role == CallerRole::Head && call.name == "bash_watch")
        .then(|| &super::RUNNER_HEAD_TOOLS.bash_watch.catalog);
    let preferred = if role.is_worker() || hashline_edit {
        &served.catalog
    } else {
        served_tools(CatalogPreset::Head)
            .iter()
            .find(|entry| entry.catalog.name == call.name)
            .map_or(head_watch.unwrap_or(&served.catalog), |entry| {
                &entry.catalog
            })
    };
    let major = match &pin {
        VersionedSchemaPin::Tp2(pin) => pin.major,
        VersionedSchemaPin::Tp1(pin) => {
            // The head-only watch grammar is not a worker tool, and the
            // hashline edit grammar is only available on a hashline route.
            let other_grammar = (head_watch.is_none()
                && call.name == "bash_watch"
                && pin.schema_digest == super::RUNNER_HEAD_TOOLS.bash_watch.catalog.schema_digest)
                || (!hashline_edit
                    && call.name == "edit"
                    && pin.schema_digest
                        == super::RUNNER_HEAD_TOOLS.hashline_edit.catalog.schema_digest);
            match SHIPPED.get(encoded).filter(|_| !other_grammar) {
                Some(&major) => major,
                None => {
                    return Err(legacy_refusal(
                        pin,
                        preferred,
                        head_watch,
                        served,
                        validator_tool,
                    ))
                }
            }
        }
        _ => {
            return Err(errors::invalid_request(
                "schema_pin",
                "unknown schema pin scheme",
            ))
        }
    };
    let current = [
        head_watch,
        Some(preferred),
        Some(&served.catalog),
        served.with_runon.as_ref(),
        served.with_ref.as_ref(),
    ]
    .into_iter()
    .flatten()
    .find(|tool| tool.schema_digest == digest(&pin))
    .unwrap_or(preferred);
    if major != current.version.expect("served tool has a version").major {
        return Err(errors::tool_major_changed(
            &call.name,
            encoded,
            &current.schema_pin().encode().expect("current pin"),
        ));
    }
    Ok(())
}

fn legacy_refusal(
    pin: &SchemaPin,
    preferred: &CatalogTool,
    head_watch: Option<&CatalogTool>,
    served: &ServedTool,
    validator_tool: &CatalogTool,
) -> ErrorBody {
    let head_watch = head_watch.filter(|entry| entry.schema_digest == pin.schema_digest);
    if pin.schema_digest == validator_tool.schema_digest
        || pin.schema_digest == served.catalog.schema_digest
        || legacy_file_path_digests(&pin.tool).contains(&pin.schema_digest)
        || head_watch.is_some()
    {
        let semantics = head_watch.map_or_else(
            || {
                if pin.tool == "bash_watch" {
                    validator_tool.semantics
                } else {
                    preferred.semantics
                }
            },
            |entry| entry.semantics,
        );
        if pin.semantics != semantics {
            return ErrorBody::new(errors::TOOL_SEMANTICS_CHANGED, "semantics pin is stale")
                .with_detail(
                    json!({"tool": pin.tool, "expected": pin.semantics, "current": semantics}),
                );
        }
    }
    ErrorBody::new(errors::TOOL_SCHEMA_CHANGED, "schema pin is stale").with_detail(
        json!({"tool": pin.tool, "expected": pin.schema_digest, "current": validator_tool.schema_digest}),
    )
}

fn digest(pin: &VersionedSchemaPin) -> &str {
    match pin {
        VersionedSchemaPin::Tp1(pin) => &pin.schema_digest,
        VersionedSchemaPin::Tp2(pin) => &pin.schema_digest,
        _ => unreachable!("unrecognized versioned pin"),
    }
}

pub(super) fn takes_hashline_arm(call: &ToolCallRequest) -> bool {
    match call
        .schema_pin
        .as_deref()
        .and_then(|pin| VersionedSchemaPin::parse(pin).ok())
    {
        Some(VersionedSchemaPin::Tp1(pin)) => {
            pin.schema_digest == super::RUNNER_HEAD_TOOLS.hashline_edit.catalog.schema_digest
        }
        // A tp2 caller can have a digest from another minor. Its arguments
        // still identify which of the two edit grammars it uses.
        _ => call.arguments.get("patch").is_some(),
    }
}

pub(super) fn rename_pin(encoded: &str, from: &str, to: &str) -> Option<String> {
    let pin = VersionedSchemaPin::parse(encoded).ok()?;
    if pin.tool() != from {
        return None;
    }
    match pin {
        VersionedSchemaPin::Tp1(pin) => SchemaPin::new(to, pin.schema_digest, pin.semantics)
            .encode()
            .ok(),
        VersionedSchemaPin::Tp2(pin) => ToolMajorPin::new(to, pin.schema_digest, pin.major)
            .encode()
            .ok(),
        _ => None,
    }
}

pub(in crate::subc) fn route_call(
    call: &mut ToolCallRequest,
    executor: &super::super::Executor,
    identity: &super::super::RouteIdentity,
    confinement: Option<&super::super::remote_policy::CarriedConfinement>,
) -> Result<Option<String>, ErrorBody> {
    let wire_name = call.name.clone();
    if super::super::carrier_read_only::restricted(identity) {
        call.name = super::super::carrier_read_only::internal_name(&call.name).to_owned();
    }
    let sent_pin = super::route_head_call_name(call, identity.scope.is_some());
    super::super::admit_confined_call_name(executor, identity, &mut call.name, confinement)?;
    if wire_name == call.name {
        return Ok(sent_pin);
    }

    // Validate renamed tp2 calls against the name actually sent, before using
    // the internal name to select the tool's schema and run it.
    let original = sent_pin.as_deref().or(call.schema_pin.as_deref());
    if let Some(pin) = original.filter(|pin| is_tp2(pin)) {
        let mut wire = ToolCallRequest::new(wire_name, serde_json::Value::Null);
        wire.schema_pin = Some(pin.into());
        wire.call_key = call.call_key.clone();
        cortexkit_role_tool_provider::call::check_versioned_call(&wire)?;
        if sent_pin.is_none() {
            let normalized = rename_pin(pin, &wire.name, &call.name).expect("checked pin");
            return Ok(call.schema_pin.replace(normalized));
        }
    }
    Ok(sent_pin)
}

pub(in crate::subc) fn admit_routed_call(
    call: &ToolCallRequest,
    identity: &super::super::RouteIdentity,
    channel: u16,
    sent_pin: Option<&str>,
) -> Result<CallerRole, ErrorBody> {
    super::admit_on_route(
        call,
        identity.scope.is_some(),
        &identity.disabled_tools,
        crate::bash_background::powershell_available(),
        &identity.session,
        super::admission_trusted(identity, call),
        &identity.project_root,
        channel,
        super::head_catalog_key(identity).is_some(),
    )
    .map_err(|error| wire_refusal(error, sent_pin))
}

fn wire_refusal(error: ErrorBody, sent_pin: Option<&str>) -> ErrorBody {
    if error.code == errors::TOOL_MAJOR_CHANGED {
        if let Some((sent, pin)) =
            sent_pin.and_then(|sent| Some((sent, VersionedSchemaPin::parse(sent).ok()?)))
        {
            let detail = error.detail.as_ref().expect("major refusal detail");
            let tool = detail["tool"].as_str().expect("major refusal tool");
            let current = detail["current"].as_str().expect("major refusal pin");
            let current = rename_pin(current, tool, pin.tool()).expect("current wire pin");
            return errors::tool_major_changed(pin.tool(), sent, &current);
        }
    }
    error
}

pub(in crate::subc) fn record_pin(sent: Option<String>, checked: Option<String>) -> Option<String> {
    sent.filter(|pin| is_tp2(pin)).or(checked)
}

fn is_tp2(pin: &str) -> bool {
    pin.starts_with("tp2:")
}

#[cfg(test)]
mod tests;
