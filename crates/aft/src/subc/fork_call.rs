//! Fork calls (the tool-provider/v1 `fork_id` marker). A runner's fork of a
//! head session, such as a voice call, may call only the tools AFT's catalog
//! marks `read_only`. Such a call must change no state the person, the head or
//! another session can see, and must never ask the person anything: whatever
//! would raise an approval card is refused instead.
//!
//! [`admit`] checks the marker and the tool before anything runs. While a
//! marked tool runs, an access question is answered by
//! [`crate::access_policy::AskRoute::Fork`], which refuses instead of asking,
//! and [`reply_refusal`] turns that refusal into the role's `ERROR` reply.

use cortexkit_role_tool_provider::{call, errors};
use serde_json::Value;
use subc_protocol::ErrorBody;

use crate::access_policy::AskRoute;

/// The tools AFT marks `read_only` in every catalog, by the bare names its
/// presets serve. The plugin spellings of the same tools (`aft_search`,
/// `ast_grep_search`, ...) are read-only too. Each of them only reads code or
/// git state. `inspect` is left out because it can start language servers,
/// and `safety` because it restores files.
pub(crate) const READ_ONLY_TOOLS: &[&str] = &[
    "read",
    "grep",
    "glob",
    "search",
    "outline",
    "zoom",
    "callgraph",
    "ast_search",
    "conflicts",
];

/// Whether the tool a call names, under any name a catalog or plugin gives
/// it, is one AFT marks read-only.
pub(super) fn read_only(name: &str) -> bool {
    crate::tool_gate::canonical_tool_name(name).is_some_and(|canonical| {
        READ_ONLY_TOOLS
            .iter()
            .any(|tool| crate::tool_gate::canonical_tool_name(tool) == Some(canonical))
    })
}

/// Admit a route request's fork marker before anything else runs.
///
/// Returns the tool a valid fork call names, spelled as the caller spelled
/// it (the name its refusals carry), or `None` when the request is not a fork
/// call. Only tool calls are checked; other operations are left as they were.
/// A malformed marker is refused `invalid_request {field: "fork_id"}`, and a
/// fork call to a tool AFT does not mark read-only is refused
/// `fork_call_refused {tool, reason: "not_read_only"}`.
pub(super) fn admit(envelope: &Value) -> Result<Option<String>, ErrorBody> {
    let name = match envelope.get("op").and_then(Value::as_str) {
        None | Some("tool.call") => envelope.get("name").and_then(Value::as_str),
        Some(_) => None,
    };
    let Some(name) = name.filter(|name| !super::tool_provider::recognized_operation(name)) else {
        return Ok(None);
    };
    if call::fork_id(envelope)?.is_none() {
        return Ok(None);
    }
    // Whether the call would ask the person is only known while it runs;
    // `ask_route` refuses it at that point instead.
    call::check_fork_call(envelope, name, read_only(name), false)?;
    Ok(Some(name.to_owned()))
}

/// The ask route a call runs under. A fork call on a route that could ask the
/// person (a plugin prompt, a reverse elicitation or the consent service)
/// gets [`AskRoute::Fork`], which refuses instead. Routes that never ask keep
/// their own answer, and every call without a marker keeps its route.
pub(super) fn ask_route(fork_tool: Option<&str>, route: AskRoute) -> AskRoute {
    match (fork_tool, &route) {
        (
            Some(tool),
            AskRoute::Plugin { .. } | AskRoute::Elicitation { .. } | AskRoute::Consent { .. },
        ) => AskRoute::Fork {
            tool: tool.to_owned(),
        },
        _ => route,
    }
}

/// The role `ERROR` for a tool response that [`AskRoute::Fork`] refused, or
/// `None` for any other response. The refusal is rebuilt with the role's own
/// constructor, so its bytes match what the contract pins.
pub(super) fn reply_refusal(response: &crate::protocol::Response) -> Option<ErrorBody> {
    if response.success
        || response.data.get("code").and_then(Value::as_str) != Some(errors::FORK_CALL_REFUSED)
    {
        return None;
    }
    let detail = response.data.get("detail")?;
    Some(errors::fork_call_refused(
        detail.get("tool")?.as_str()?,
        detail.get("reason")?.as_str()?,
    ))
}

#[cfg(test)]
#[path = "fork_call_tests.rs"]
mod tests;
