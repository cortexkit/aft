//! Route legacy native requests through the context's plugin approval scope.

use crate::access_policy::{
    ask_route_guard, current_ask_route, finish_plugin_response, plugin, AskRoute,
};

/// Legacy stdin requests lack the session and actor identity authenticated by
/// the daemon. Their private process context and configured harness supply an
/// approval route; requests nested inside daemon dispatch keep its authenticated route.
pub fn with_native_route(
    mut request: crate::protocol::RawRequest,
    ctx: &crate::context::AppContext,
    run: impl FnOnce(
        crate::protocol::RawRequest,
        &crate::context::AppContext,
    ) -> crate::protocol::Response,
) -> crate::protocol::Response {
    let preview = request
        .params
        .get("preview")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false)
        || ["hashline_preflight", "undo_preview", "checkpoint_paths"]
            .contains(&request.command.as_str());
    match current_ask_route() {
        AskRoute::Plugin { scope, once } => {
            ctx.mark_standalone_plugin_host();
            strip_native_approval_metadata(&mut request);
            return finish_plugin_response(&scope, &once, preview, run(request, ctx));
        }
        AskRoute::Lowered => {}
        _ => return run(request, ctx),
    }
    let plugin = matches!(
        ctx.config().harness,
        Some(crate::harness::Harness::Opencode | crate::harness::Harness::Pi)
    );
    if !plugin {
        return run(request, ctx);
    }
    ctx.mark_standalone_plugin_host();
    let args = request
        .params
        .get("arguments")
        .or_else(|| request.params.get("args"))
        .unwrap_or(&request.params);
    let tool = native_tool_name(&request);
    let mut scope = ctx
        .standalone_approval_scope(request.session())
        .for_tool(tool, args);
    let ids: Vec<String> = args
        .get("permissions_granted")
        .and_then(serde_json::Value::as_array)
        .map(|ids| {
            ids.iter()
                .filter_map(serde_json::Value::as_str)
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default();
    let once = plugin::claim(&mut scope, &ids);
    strip_native_approval_metadata(&mut request);
    let _route = ask_route_guard(Some(AskRoute::Plugin {
        scope: scope.clone(),
        once: once.clone(),
    }));
    finish_plugin_response(&scope, &once, preview, run(request, ctx))
}

/// The tool a native request runs: the nested `name` of a `tool_call`,
/// otherwise the command itself.
fn native_tool_name(request: &crate::protocol::RawRequest) -> &str {
    if request.command == "tool_call" {
        request
            .params
            .get("name")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("")
    } else {
        &request.command
    }
}

/// Remove `permissions_granted` before a plugin host's native request runs,
/// after its outside-access tokens were claimed.
///
/// On the native wire `permissions_granted` only ever holds the host's own
/// approvals: both plugins drop the agent's `permissions_granted` from the
/// tool arguments before adding the approvals they granted
/// (`withHostPermissionGrants` and `withOutsidePermissions` in
/// `packages/opencode-plugin/src/tools/outside-permissions.ts` and
/// `packages/pi-plugin/src/tools/_shared.ts`, each destructuring the field
/// away as `_untrusted`). A shell reads its approved command patterns and
/// escalation grants from that field, so it is kept when the tool that
/// runs is bash or powershell, called directly or nested in a `tool_call`.
/// Every other tool has no use for it, so it is removed and a forged value
/// can never reach a handler.
fn strip_native_approval_metadata(request: &mut crate::protocol::RawRequest) {
    if matches!(native_tool_name(request), "bash" | "powershell") {
        return;
    }
    let params = &mut request.params;
    let arguments = if params.get("arguments").is_some() {
        params.get_mut("arguments")
    } else if params.get("args").is_some() {
        params.get_mut("args")
    } else {
        Some(params)
    };
    if let Some(args) = arguments.and_then(serde_json::Value::as_object_mut) {
        args.remove("permissions_granted");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn approval_metadata_only_survives_for_shell_requests() {
        for (command, name, retained) in [
            ("read", "", false),
            ("bash", "", true),
            ("tool_call", "read", false),
            ("tool_call", "powershell", true),
        ] {
            let mut request = crate::protocol::RawRequest {
                id: "route".into(),
                command: command.into(),
                lsp_hints: None,
                session_id: None,
                params: serde_json::json!({"name": name, "arguments": {"permissions_granted": ["host-token"]}}),
            };
            strip_native_approval_metadata(&mut request);
            assert_eq!(
                request.params["arguments"]
                    .get("permissions_granted")
                    .is_some(),
                retained
            );
        }
    }
}
