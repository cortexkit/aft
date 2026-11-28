//! Translate catalog aliases without losing the pin that arrived on the wire.

use cortexkit_role_tool_provider::{
    call::{check_versioned_call, ToolCallRequest, VersionedSchemaPin},
    errors,
};
use subc_protocol::ErrorBody;

use crate::subc::{
    admit_confined_call_name, carrier_read_only, remote_policy::CarriedConfinement, Executor,
    RouteIdentity,
};

use super::super::{admission_trusted, admit_on_route, head_catalog_key, route_head_call_name};
use super::{rename_pin, CallerRole};

pub(in crate::subc) fn route_call(
    call: &mut ToolCallRequest,
    executor: &Executor,
    identity: &RouteIdentity,
    confinement: Option<&CarriedConfinement>,
) -> Result<Option<String>, ErrorBody> {
    let wire_name = call
        .schema_pin
        .as_deref()
        .is_some_and(is_tp2)
        .then(|| call.name.clone());
    if carrier_read_only::restricted(identity) {
        call.name = carrier_read_only::internal_name(&call.name).to_owned();
    }
    let sent_pin = route_head_call_name(call, identity.scope.is_some());
    admit_confined_call_name(executor, identity, &mut call.name, confinement)?;
    let Some(wire_name) = wire_name.filter(|name| name != &call.name) else {
        return Ok(sent_pin);
    };

    // A versioned pin must name the public tool before admission uses the
    // internal name to select the tool's schema and run it.
    let original = sent_pin.as_deref().or(call.schema_pin.as_deref());
    if let Some(pin) = original.filter(|pin| is_tp2(pin)) {
        let mut wire = ToolCallRequest::new(wire_name, serde_json::Value::Null);
        wire.schema_pin = Some(pin.into());
        wire.call_key = call.call_key.clone();
        check_versioned_call(&wire)?;
        if sent_pin.is_none() {
            let normalized = rename_pin(pin, &wire.name, &call.name).expect("checked pin");
            return Ok(call.schema_pin.replace(normalized));
        }
    }
    Ok(sent_pin)
}

pub(in crate::subc) fn admit_routed_call(
    call: &ToolCallRequest,
    identity: &RouteIdentity,
    channel: u16,
    sent_pin: Option<&str>,
) -> Result<CallerRole, ErrorBody> {
    admit_on_route(
        call,
        identity.scope.is_some(),
        &identity.disabled_tools,
        crate::bash_background::powershell_available(),
        &identity.session,
        admission_trusted(identity, call),
        &identity.project_root,
        channel,
        head_catalog_key(identity).is_some(),
    )
    .map_err(|error| wire_refusal(error, sent_pin))
}

pub(super) fn wire_refusal(error: ErrorBody, sent_pin: Option<&str>) -> ErrorBody {
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
