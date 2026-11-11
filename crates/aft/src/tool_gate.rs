//! Dispatch-time enforcement of `disabled_tools`.
//!
//! The plugins honour `disabled_tools` by not registering a disabled tool, but
//! any other consumer of the tool catalog (a subc route, a standalone
//! `tool_call`) could still call it. This module is the engine-side guarantee:
//! an agent tool call whose tool is disabled is refused with `tool_disabled`
//! before it translates, dispatches, asks for permission or takes a backup.
//!
//! `disabled_tools` is a connect-time setting, like the plugins' registration:
//! callers decide from the list resolved when the session connected (the
//! configure snapshot, or a subc route's bind snapshot). Editing `aft.jsonc`
//! mid-session changes nothing until the next connect, so the refusals always
//! agree with the tool descriptions the session was given.

use serde_json::{json, Value};

use crate::protocol::Response;

/// Error code of a refused call to a disabled tool.
pub const TOOL_DISABLED_CODE: &str = "tool_disabled";

/// The canonical `disabled_tools` name of the tool a call named `call_name`
/// reaches, or `None` when the name is not an agent tool.
///
/// Callers use the subc catalog's bare names (`delete`, `outline`,
/// `ast_search`, the host names `read`, `bash`, ...), sometimes with an
/// `aft_` prefix (`aft_inspect`, and the historical `aft_read` family listed
/// in [`crate::feature_config::LEGACY_TOOL_ALIASES`]). `disabled_tools`
/// spells the same tools with [`crate::feature_config::CANONICAL_TOOLS`]
/// names. The bare-to-canonical pairs below are the ones the plugins register
/// (for example OpenCode's `ast_grep_search` tool calls `ast_search`).
///
/// `powershell` is not a canonical tool but Pi registers it under its own
/// name, so that name disables it. `bash` and `powershell` are independent:
/// disabling one leaves the other available.
pub fn canonical_tool_name(call_name: &str) -> Option<&'static str> {
    let bare = call_name.strip_prefix("aft_").unwrap_or(call_name);
    Some(match bare {
        "read" => "read",
        "write" => "write",
        "edit" => "edit",
        "apply_patch" => "apply_patch",
        "grep" => "grep",
        "glob" => "glob",
        "bash" => "bash",
        "powershell" => "powershell",
        "bash_status" => "bash_status",
        "bash_kill" => "bash_kill",
        "bash_write" => "bash_write",
        "bash_watch" => "bash_watch",
        "ast_search" | "ast_grep_search" => "ast_grep_search",
        "ast_replace" | "ast_grep_replace" => "ast_grep_replace",
        "callgraph" => "aft_callgraph",
        "conflicts" => "aft_conflicts",
        "delete" => "aft_delete",
        "import" => "aft_import",
        "inspect" => "aft_inspect",
        "move" => "aft_move",
        "outline" => "aft_outline",
        "safety" => "aft_safety",
        "search" => "aft_search",
        "zoom" => "aft_zoom",
        _ => return None,
    })
}

/// The canonical name that gates this particular call, or `None` when the
/// call is never refused.
///
/// `bash_status`, `bash_kill` and `bash_write` are both agent tools and the
/// plugins' own background-task plumbing. Only the agent's call (catalog
/// spelling, see [`crate::host_tool_names::is_native_plumbing_call`]) is gated; the
/// plugins' native calls, and every other plumbing command such as
/// `bash_drain_completions`, always go through.
pub fn gating_name(call_name: &str, arguments: &Value) -> Option<&'static str> {
    let canonical = canonical_tool_name(call_name)?;
    let bare = call_name.strip_prefix("aft_").unwrap_or(call_name);
    if crate::host_tool_names::is_native_plumbing_call(bare, arguments) {
        return None;
    }
    Some(canonical)
}

/// Build the refusal for `call_name` if its tool is in `disabled`.
pub fn refusal(
    request_id: &str,
    call_name: &str,
    arguments: &Value,
    disabled: &[String],
) -> Option<Response> {
    let tool = gating_name(call_name, arguments)?;
    refusal_for_tool(request_id, call_name, tool, disabled)
}

/// v1 checks the requested name regardless of task_id/taskId argument spelling.
/// Legacy plugin task polling may bypass disables; v1 agent calls never do.
pub(crate) fn refusal_by_name(
    request_id: &str,
    call_name: &str,
    disabled: &[String],
) -> Option<Response> {
    refusal_for_tool(
        request_id,
        call_name,
        canonical_tool_name(call_name)?,
        disabled,
    )
}

fn refusal_for_tool(
    request_id: &str,
    call_name: &str,
    tool: &str,
    disabled: &[String],
) -> Option<Response> {
    if !disabled.iter().any(|name| name == tool) {
        return None;
    }
    let called_as = if call_name == tool {
        String::new()
    } else {
        format!(" (called as `{call_name}`)")
    };
    let message = format!(
        "{TOOL_DISABLED_CODE} {tool}: `{tool}`{called_as} is disabled by `disabled_tools` in aft.jsonc. \
         To enable it, remove \"{tool}\" from `disabled_tools` in ~/.config/cortexkit/aft.jsonc \
         (or the project's .cortexkit/aft.jsonc), or run `npx @cortexkit/aft setup`, then restart \
         the host so the session reconnects."
    );
    Some(Response::error_with_data(
        request_id,
        TOOL_DISABLED_CODE,
        message,
        json!({ "tool": tool }),
    ))
}

/// Whether a catalog entry named `catalog_name` stays advertised under
/// `disabled`.
pub fn catalog_keeps(catalog_name: &str, disabled: &[String]) -> bool {
    canonical_tool_name(catalog_name).is_none_or(|tool| !disabled.iter().any(|name| name == tool))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn disabled(names: &[&str]) -> Vec<String> {
        names.iter().map(|name| (*name).to_string()).collect()
    }

    #[test]
    fn every_catalog_and_legacy_name_maps_to_a_canonical_tool() {
        for (call, canonical) in [
            ("read", "read"),
            ("aft_read", "read"),
            ("write", "write"),
            ("aft_write", "write"),
            ("edit", "edit"),
            ("aft_edit", "edit"),
            ("apply_patch", "apply_patch"),
            ("aft_apply_patch", "apply_patch"),
            ("grep", "grep"),
            ("aft_grep", "grep"),
            ("glob", "glob"),
            ("aft_glob", "glob"),
            ("bash", "bash"),
            ("aft_bash", "bash"),
            ("delete", "aft_delete"),
            ("aft_delete", "aft_delete"),
            ("move", "aft_move"),
            ("aft_move", "aft_move"),
            ("inspect", "aft_inspect"),
            ("aft_inspect", "aft_inspect"),
            ("ast_search", "ast_grep_search"),
            ("ast_grep_search", "ast_grep_search"),
            ("ast_replace", "ast_grep_replace"),
            ("outline", "aft_outline"),
            ("zoom", "aft_zoom"),
            ("search", "aft_search"),
            ("safety", "aft_safety"),
            ("import", "aft_import"),
            ("callgraph", "aft_callgraph"),
            ("conflicts", "aft_conflicts"),
            ("bash_status", "bash_status"),
            ("bash_kill", "bash_kill"),
            ("bash_write", "bash_write"),
        ] {
            assert_eq!(canonical_tool_name(call), Some(canonical), "{call}");
            assert!(
                crate::feature_config::is_known_tool(canonical),
                "{canonical} must be a canonical tool name"
            );
        }
        // Every legacy alias the config accepts resolves the same way here.
        for (alias, canonical) in crate::feature_config::LEGACY_TOOL_ALIASES {
            assert_eq!(canonical_tool_name(alias), Some(canonical));
        }
        // Every canonical tool is reachable by its own name.
        for canonical in crate::feature_config::CANONICAL_TOOLS {
            assert_eq!(canonical_tool_name(canonical), Some(canonical));
        }
    }

    #[test]
    fn plumbing_is_never_gated() {
        for name in [
            "bash_drain_completions",
            "bash_ack_completions",
            "undo_preview",
            "checkpoint_paths",
            "safety_purge_paths",
            "hashline_preflight",
            "inspect_tier2_run",
            "status",
        ] {
            assert_eq!(gating_name(name, &json!({})), None, "{name}");
        }
        // Native snake_case companion calls are the plugins' plumbing.
        assert_eq!(gating_name("bash_status", &json!({ "task_id": "t" })), None);
        // The catalog-style `taskId` argument marks an agent call; the plugins'
        // native plumbing uses `task_id`.
        assert_eq!(
            gating_name("bash_status", &json!({ "taskId": "t" })),
            Some("bash_status")
        );
    }

    #[test]
    fn bash_and_powershell_are_independent() {
        let args = json!({ "command": "true" });
        assert!(refusal("r", "powershell", &args, &disabled(&["bash"])).is_none());
        assert!(refusal("r", "bash", &args, &disabled(&["powershell"])).is_none());
        assert!(refusal("r", "powershell", &args, &disabled(&["powershell"])).is_some());
        // A companion is refused by its own name only.
        assert!(refusal(
            "r",
            "bash_kill",
            &json!({ "taskId": "t" }),
            &disabled(&["bash"])
        )
        .is_none());
        assert!(catalog_keeps("powershell", &disabled(&["bash"])));
        assert!(!catalog_keeps("bash", &disabled(&["bash"])));
    }

    #[test]
    fn refusal_is_named_and_says_how_to_enable() {
        let refusal = refusal(
            "r1",
            "aft_delete",
            &json!({ "files": ["x"] }),
            &disabled(&["aft_delete"]),
        )
        .expect("disabled tool is refused");
        assert!(!refusal.success);
        assert_eq!(refusal.data["code"], TOOL_DISABLED_CODE);
        assert_eq!(refusal.data["tool"], "aft_delete");
        let message = refusal.data["message"].as_str().unwrap();
        assert!(
            message.starts_with("tool_disabled aft_delete:"),
            "{message}"
        );
        assert!(message.contains("aft.jsonc"));
        assert!(message.contains("remove \"aft_delete\" from `disabled_tools`"));
        assert!(message.contains("npx @cortexkit/aft setup"));
    }

    #[test]
    fn catalog_filter_drops_disabled_names_only() {
        let off = disabled(&["aft_delete", "read"]);
        assert!(!catalog_keeps("delete", &off));
        assert!(!catalog_keeps("read", &off));
        assert!(catalog_keeps("move", &off));
        assert!(catalog_keeps("status", &off));
    }
}
