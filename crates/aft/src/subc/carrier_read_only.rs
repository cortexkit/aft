//! Rich-UI route admission only narrows authority; it never grants permissions.

use serde_json::Value;
use subc_protocol::ErrorBody;

use super::RouteIdentity;
use crate::sandbox_spawn::AuthenticatedPrincipal;

pub(super) const ALLOWED_TOOLS: &[&str] = &[
    "read",
    "aft_outline",
    "aft_zoom",
    "aft_search",
    "grep",
    "glob",
    "aft_callgraph",
    "bash_status",
    "bash_watch",
];

const REFUSAL_MESSAGE: &str =
    "This route is read-only: <tool> is not available to the rich-UI provider.";
const TARGET_REFUSAL_MESSAGE: &str =
    "This route is read-only: <kind> targets are not available to the rich-UI provider.";

pub(super) fn restricted(identity: &RouteIdentity) -> bool {
    // A program may run using its head agent's sandbox identity. Check the
    // saved incoming run principal so borrowing authority cannot remove the
    // rich-UI restriction.
    let principal = identity
        .run_owner
        .as_ref()
        .map_or(&identity.spawn_principal, |owner| &owner.run_principal);
    matches!(principal, AuthenticatedPrincipal::RouteBind { principal_id: Some(id), .. }
        if id == "reserved:occipital")
}

pub(super) fn allowed(name: &str) -> bool {
    crate::tool_gate::canonical_tool_name(name)
        .is_some_and(|canonical| ALLOWED_TOOLS.contains(&canonical))
}

pub(super) fn observes_tasks(identity: &RouteIdentity, name: &str) -> bool {
    restricted(identity) && matches!(name, "bash_status" | "bash_watch")
}

pub(super) fn admit(identity: &RouteIdentity, envelope: &Value) -> Result<(), ErrorBody> {
    if !restricted(identity) {
        return Ok(());
    }
    let operation = envelope.get("op").and_then(Value::as_str);
    let name = match operation {
        Some("tool.call") | None => envelope.get("name").and_then(Value::as_str),
        Some(operation) => Some(operation),
    };
    // Discovery is read-only too. Every other operation, including cancellation
    // and result retrieval, must name a tool in the closed allowlist.
    if let Some(name) = name {
        if !matches!(name, "tool.catalog" | "role.describe") && !allowed(name) {
            return Err(ErrorBody::new(
                "carrier_read_only",
                REFUSAL_MESSAGE.replace("<tool>", name),
            ));
        }
        if let Some(arguments) = envelope.get("arguments").or_else(|| envelope.get("params")) {
            if let Some(kind) = blocked_target_kind(name, arguments) {
                return Err(ErrorBody::new(
                    "carrier_read_only",
                    TARGET_REFUSAL_MESSAGE.replace("<kind>", kind),
                ));
            }
        }
    }
    Ok(())
}

/// Extend `admit` with versioned operation admission without changing the
/// gate's allowlist, target checks or refusal frames. Call only after `admit`.
pub(super) fn admit_operation(
    identity: &RouteIdentity,
    name: &str,
    arguments: &Value,
) -> Result<Option<super::rich_ui::admission::Operation>, ErrorBody> {
    if super::occipital_carrier_route(identity) {
        super::rich_ui::admission::prepare(name, arguments).map(Some)
    } else {
        Ok(None)
    }
}

/// Check target arguments at admission, before even a cached remote document
/// can be returned. Reading a URL or GitHub item may fetch live network data.
fn blocked_target_kind(name: &str, arguments: &Value) -> Option<&'static str> {
    let tool = crate::tool_gate::canonical_tool_name(name)?;
    if !matches!(tool, "read" | "aft_outline" | "aft_zoom") {
        return None;
    }
    // Reuse dispatch's argument normalizer: it resolves path aliases and
    // supported zoom target forms, including JSON-encoded batches. Invalid
    // arguments cannot reach a fetch.
    let arguments =
        crate::subc_translate::normalize_path_arguments(internal_name(tool), arguments.clone())
            .ok()?;
    fn target_kind(tool: &str, value: &Value) -> Option<&'static str> {
        if let Some(target) = value.as_str() {
            let target = target.trim_start().to_ascii_lowercase();
            if target.starts_with("issue://") {
                return Some("issue://");
            }
            if target.starts_with("pr://") {
                return Some("pr://");
            }
            if tool != "read" && (target.starts_with("http://") || target.starts_with("https://")) {
                return Some("URL");
            }
        }
        value
            .as_array()?
            .iter()
            .find_map(|target| target_kind(tool, target))
    }
    for field in ["path", "target"] {
        if let Some(kind) = arguments
            .get(field)
            .and_then(|value| target_kind(tool, value))
        {
            return Some(kind);
        }
    }
    arguments.get("batch")?.as_array()?.iter().find_map(|item| {
        item.get("target")
            .and_then(|value| target_kind(tool, value))
    })
}

/// The public catalog spells navigation tools with `aft_`; the command core
/// and v1 schema validators use the corresponding bare names.
pub(super) fn internal_name(name: &str) -> &str {
    match name {
        "aft_outline" => "outline",
        "aft_zoom" => "zoom",
        "aft_search" => "search",
        "aft_callgraph" => "callgraph",
        _ => name,
    }
}
