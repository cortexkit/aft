//! Call-time enforcement of `disabled_tools`.
//!
//! The plugins honour `disabled_tools` by not registering a disabled tool, but
//! any other consumer of the tool catalog (a subc route, a standalone
//! `tool_call`) could still call it. This module is the engine-side guarantee:
//! an agent tool call whose tool is disabled is refused with `tool_disabled`
//! before it translates, dispatches, asks for permission or takes a backup.
//!
//! The disabled list is re-read from the same config files configure resolved,
//! on every call, so editing `aft.jsonc` mid-session takes effect on the next
//! call without a rebind. A resolved list is cached against the exact file
//! contents it came from, so an unchanged config costs two small file reads
//! and no re-resolution.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex};

use serde_json::{json, Value};

use crate::config_resolve::ConfigTier;
use crate::context::AppContext;
use crate::harness::Harness;
use crate::protocol::Response;

/// Error code of a refused call to a disabled tool.
pub const TOOL_DISABLED_CODE: &str = "tool_disabled";

/// Where a root's configuration came from: enough to re-read and re-resolve
/// the same tiers at call time.
#[derive(Debug, Clone)]
pub struct ToolGateSource {
    pub project_root: PathBuf,
    pub harness: Option<Harness>,
    /// The CortexKit user config file, when the caller named one.
    pub user_config_path: Option<PathBuf>,
    /// Tiers relayed by the plugin; used only for a tier with no file on disk,
    /// exactly as configure does.
    pub wire_tiers: Vec<ConfigTier>,
}

/// The canonical `disabled_tools` names that switch off a call made under
/// `call_name`. Empty means the call is not an agent tool and is never refused.
///
/// Callers use the subc catalog's bare names (`delete`, `outline`,
/// `ast_search`, the host names `read`, `bash`, ...), sometimes with an
/// `aft_` prefix (`aft_inspect`, and the historical `aft_read` family listed
/// in [`crate::feature_config::LEGACY_TOOL_ALIASES`]). `disabled_tools`
/// spells the same tools with [`crate::feature_config::CANONICAL_TOOLS`]
/// names. The bare-to-canonical pairs below are the ones the plugins register
/// (for example OpenCode's `ast_grep_search` tool calls `ast_search`).
///
/// `bash_status`, `bash_kill` and `bash_write` are both agent companions of
/// `bash` and the plugins' own background-task plumbing. An agent's call
/// (catalog spelling, see [`crate::subc::is_native_plumbing_call`]) is
/// refused when either the companion or `bash` is disabled; the plugins'
/// native calls are never refused, so their drain/ack and status polling keep
/// working. `powershell` is gated only by its own name, matching Pi, which
/// registers it independently of `bash`.
pub fn gating_names(call_name: &str, arguments: &Value) -> &'static [&'static str] {
    let bare = call_name.strip_prefix("aft_").unwrap_or(call_name);
    match bare {
        "read" => &["read"],
        "write" => &["write"],
        "edit" => &["edit"],
        "apply_patch" => &["apply_patch"],
        "grep" => &["grep"],
        "glob" => &["glob"],
        "bash" => &["bash"],
        "powershell" => &["powershell"],
        "bash_status" | "bash_kill" | "bash_write"
            if crate::subc::is_native_plumbing_call(bare, arguments) =>
        {
            &[]
        }
        "bash_status" => &["bash_status", "bash"],
        "bash_kill" => &["bash_kill", "bash"],
        "bash_write" => &["bash_write", "bash"],
        "bash_watch" => &["bash_watch", "bash"],
        "ast_search" | "ast_grep_search" => &["ast_grep_search"],
        "ast_replace" | "ast_grep_replace" => &["ast_grep_replace"],
        "callgraph" => &["aft_callgraph"],
        "conflicts" => &["aft_conflicts"],
        "delete" => &["aft_delete"],
        "import" => &["aft_import"],
        "inspect" => &["aft_inspect"],
        "move" => &["aft_move"],
        "outline" => &["aft_outline"],
        "safety" => &["aft_safety"],
        "search" => &["aft_search"],
        "zoom" => &["aft_zoom"],
        _ => &[],
    }
}

/// Build the refusal for `call_name` if one of its gating names is in
/// `disabled`.
pub fn refusal_from_list(
    request_id: &str,
    call_name: &str,
    arguments: &Value,
    disabled: &[String],
) -> Option<Response> {
    let gates = gating_names(call_name, arguments);
    let tool = *gates.first()?;
    let disabled_entry = gates
        .iter()
        .find(|gate| disabled.iter().any(|name| name == *gate))?;
    let mut message = format!("Tool `{tool}` is disabled");
    if call_name != tool {
        message.push_str(&format!(" (called as `{call_name}`)"));
    }
    if *disabled_entry != tool {
        message.push_str(&format!(" because `{disabled_entry}` is disabled"));
    }
    message.push_str(&format!(
        ". To enable it, remove \"{disabled_entry}\" from `disabled_tools` in \
         ~/.config/cortexkit/aft.jsonc (or run `npx @cortexkit/aft setup`), then restart the host."
    ));
    Some(Response::error_with_data(
        request_id,
        TOOL_DISABLED_CODE,
        message,
        json!({ "tool": tool, "disabled_tool": disabled_entry }),
    ))
}

/// Refuse `call_name` if it is disabled for the root bound to `ctx`.
///
/// The list comes from re-reading the files the last configure resolved;
/// before any configure, or when the files no longer resolve, it falls back to
/// the configured (last good) list.
pub fn refusal_for_call(
    ctx: &AppContext,
    request_id: &str,
    call_name: &str,
    arguments: &Value,
) -> Option<Response> {
    if gating_names(call_name, arguments).is_empty() {
        return None;
    }
    let disabled = disabled_tools_now(ctx);
    refusal_from_list(request_id, call_name, arguments, &disabled)
}

/// Refuse `call_name` using a source the caller holds directly (a subc route
/// on its async side, which has no [`AppContext`]). Nothing is refused when
/// the files have never resolved.
pub fn refusal_for_source(
    source: &ToolGateSource,
    request_id: &str,
    call_name: &str,
    arguments: &Value,
) -> Option<Response> {
    if gating_names(call_name, arguments).is_empty() {
        return None;
    }
    let disabled = resolve_disabled_tools(source)?;
    refusal_from_list(request_id, call_name, arguments, &disabled)
}

/// The disabled list in force for `ctx` right now.
pub fn disabled_tools_now(ctx: &AppContext) -> Vec<String> {
    let config = ctx.config();
    if let Some(source) = ctx.tool_gate_source() {
        // A configure that failed after recording its source leaves the
        // previous root or harness published; only a matching source counts.
        if config.project_root.as_deref() == Some(source.project_root.as_path())
            && config.harness == source.harness
        {
            if let Some(disabled) = resolve_disabled_tools(&source) {
                return disabled;
            }
        }
    }
    config.disabled_tools.clone()
}

type SourceKey = (Option<PathBuf>, PathBuf, Option<Harness>);

struct CachedResolution {
    tiers: Vec<ConfigTier>,
    disabled: Vec<String>,
}

/// Last successful resolution per source. Bounded by the number of distinct
/// roots and harnesses this process serves; cleared if it ever grows past
/// [`MAX_CACHED_SOURCES`].
static RESOLVED: LazyLock<Mutex<HashMap<SourceKey, CachedResolution>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
const MAX_CACHED_SOURCES: usize = 256;

/// Re-read the source's tiers and resolve the disabled list. An unchanged set
/// of files answers from the cache. A set that no longer resolves (invalid
/// config) keeps the last good list for the source, or `None` if there is
/// none.
pub fn resolve_disabled_tools(source: &ToolGateSource) -> Option<Vec<String>> {
    let tiers = crate::subc_config::select_config_tiers(
        source.user_config_path.as_deref(),
        &source.project_root,
        &source.wire_tiers,
    );
    let key: SourceKey = (
        source.user_config_path.clone(),
        source.project_root.clone(),
        source.harness.clone(),
    );
    let mut cache = RESOLVED.lock().unwrap_or_else(|poison| poison.into_inner());
    if let Some(cached) = cache.get(&key) {
        if cached.tiers == tiers {
            return Some(cached.disabled.clone());
        }
    }
    let resolved =
        crate::config_resolve::resolve_config_for_harness(&tiers, source.harness.as_ref());
    if !resolved.errors.is_empty() {
        return cache.get(&key).map(|cached| cached.disabled.clone());
    }
    let disabled = resolved.config.disabled_tools;
    if cache.len() >= MAX_CACHED_SOURCES && !cache.contains_key(&key) {
        cache.clear();
    }
    cache.insert(
        key,
        CachedResolution {
            tiers,
            disabled: disabled.clone(),
        },
    );
    Some(disabled)
}

/// Source for a subc route: the route's project root and harness, the user
/// config file the module was started with, and no wire tiers (subc never
/// takes config over the wire).
pub fn subc_route_source(
    user_config_path: Option<&Path>,
    project_root: &Path,
    harness: &str,
) -> ToolGateSource {
    ToolGateSource {
        project_root: project_root.to_path_buf(),
        harness: harness.parse::<Harness>().ok(),
        user_config_path: user_config_path.map(Path::to_path_buf),
        wire_tiers: Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn disabled(names: &[&str]) -> Vec<String> {
        names.iter().map(|name| (*name).to_string()).collect()
    }

    #[test]
    fn every_catalog_and_legacy_name_maps_to_a_canonical_tool() {
        let args = json!({});
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
            ("ast_replace", "ast_grep_replace"),
            ("outline", "aft_outline"),
            ("zoom", "aft_zoom"),
            ("search", "aft_search"),
            ("safety", "aft_safety"),
            ("import", "aft_import"),
            ("callgraph", "aft_callgraph"),
            ("conflicts", "aft_conflicts"),
        ] {
            assert_eq!(
                gating_names(call, &args).first(),
                Some(&canonical),
                "{call}"
            );
            assert!(
                crate::feature_config::is_known_tool(canonical),
                "{canonical} must be a canonical tool name"
            );
        }
        // Every legacy alias the config accepts resolves the same way here.
        for (alias, canonical) in crate::feature_config::LEGACY_TOOL_ALIASES {
            assert_eq!(gating_names(alias, &args).first(), Some(&canonical));
        }
    }

    #[test]
    fn plumbing_is_never_gated() {
        for name in [
            "bash_drain_completions",
            "bash_ack_completions",
            "undo_preview",
            "checkpoint_paths",
            "hashline_preflight",
            "inspect_tier2_run",
            "status",
        ] {
            assert!(gating_names(name, &json!({})).is_empty(), "{name}");
        }
        // Native snake_case companion calls are the plugins' plumbing.
        assert!(gating_names("bash_status", &json!({ "task_id": "t" })).is_empty());
        // The catalog's camelCase spelling is an agent call that follows bash.
        assert_eq!(
            gating_names("bash_status", &json!({ "taskId": "t" })),
            &["bash_status", "bash"]
        );
    }

    #[test]
    fn companion_refusal_names_the_disabled_bash_entry() {
        let refusal = refusal_from_list(
            "r1",
            "bash_kill",
            &json!({ "taskId": "t" }),
            &disabled(&["bash"]),
        )
        .expect("agent companion follows bash");
        assert!(!refusal.success);
        assert_eq!(refusal.data["code"], TOOL_DISABLED_CODE);
        assert_eq!(refusal.data["tool"], "bash_kill");
        assert_eq!(refusal.data["disabled_tool"], "bash");
        let message = refusal.data["message"].as_str().unwrap();
        assert!(message.contains("remove \"bash\" from `disabled_tools`"));
        assert!(message.contains("~/.config/cortexkit/aft.jsonc"));
        assert!(message.contains("npx @cortexkit/aft setup"));
        assert!(message.contains("restart the host"));
    }

    #[test]
    fn resolution_follows_file_edits_and_keeps_last_good_on_invalid_config() {
        let dir = tempfile::tempdir().unwrap();
        let user = dir.path().join("aft.jsonc");
        let root = dir.path().join("project");
        std::fs::create_dir_all(&root).unwrap();
        let source = ToolGateSource {
            project_root: root,
            harness: None,
            user_config_path: Some(user.clone()),
            wire_tiers: Vec::new(),
        };

        // No user file behaves like `{}`: move and delete stay off.
        assert_eq!(
            resolve_disabled_tools(&source).unwrap(),
            disabled(&["aft_delete", "aft_move"])
        );
        std::fs::write(&user, r#"{ "disabled_tools": ["read"] }"#).unwrap();
        assert_eq!(
            resolve_disabled_tools(&source).unwrap(),
            disabled(&["read"])
        );
        std::fs::write(&user, r#"{ "disabled_tools": [] }"#).unwrap();
        assert!(resolve_disabled_tools(&source).unwrap().is_empty());
        // A retired key is rejected at every version; the last good list holds.
        std::fs::write(&user, r#"{ "gh_read": true, "disabled_tools": ["bash"] }"#).unwrap();
        let after_invalid = resolve_disabled_tools(&source).unwrap();
        assert!(after_invalid.is_empty(), "{after_invalid:?}");
    }
}
