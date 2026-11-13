//! Feature-based configuration policy shared by the resolver, the configure
//! handler and (later) the setup/doctor CLI.
//!
//! Configuration used to pick a tool surface level (`tool_surface`), a hoisting
//! mode (`hoist_builtin_tools`) and a set of index booleans. It now has one
//! registration rule — a tool is registered unless its canonical name is in
//! `disabled_tools` — plus three first-class index switches under `indexes`.
//! This module owns the literal tool inventory, the table of retired keys, the
//! per-block translation of those keys into canonical ones, validation of a
//! resolved configuration object and the migration-notice projection digest.
//!
//! Retired names in [`REFUSED_RETIRED_PATHS`] and the prefixed tool names in
//! [`LEGACY_TOOL_ALIASES`] are refused in user config with `removed_config_key`.
//! The translation below still covers those names,
//! because `aft doctor --fix` and the automatic rewrite of the user file
//! ([`crate::config_fix::auto_migrate_user_config`]) build their rewrite from
//! it; project config translates them with a notice. Names retired more
//! recently are translated in memory into their current equivalents on every
//! load, and the current keys then pass through the normal user/project trust
//! rules. The remaining data-layer rename ([`DATA_LAYER_RETIRED_PATHS`]) gets a notice until
//! [`DATA_LAYER_KEYS_REFUSED_FROM`] and are refused from that release on. A
//! project file is never written, only reported with a notice. The TypeScript plugins carry a mirror of
//! the same rules in `packages/aft-bridge/src/feature-config.ts`; the config
//! parity fixtures and the shared notice-projection fixtures keep them equal.

use std::collections::BTreeSet;

use serde_json::{Map, Value};
use sha2::{Digest, Sha256};

/// Every agent-visible tool name a registration adapter may publish, in
/// sorted order. Known-name checks use this complete inventory, never the
/// currently enabled subset.
pub const CANONICAL_TOOLS: [&str; 23] = [
    "aft_callgraph",
    "aft_conflicts",
    "aft_delete",
    "aft_import",
    "aft_inspect",
    "aft_move",
    "aft_outline",
    "aft_safety",
    "aft_search",
    "aft_zoom",
    "apply_patch",
    "ast_grep_replace",
    "ast_grep_search",
    "bash",
    "bash_kill",
    "bash_status",
    "bash_watch",
    "bash_write",
    "edit",
    "glob",
    "grep",
    "read",
    "write",
];

/// The seven host tool slots AFT takes over.
pub const HOST_TOOL_NAMES: [&str; 7] = [
    "apply_patch",
    "bash",
    "edit",
    "glob",
    "grep",
    "read",
    "write",
];

/// Disables applied when the user's base configuration makes no registration
/// choice at all: move and delete stay opt-in.
pub const DEFAULT_DISABLED_TOOLS: [&str; 2] = ["aft_delete", "aft_move"];

/// Historical prefixed tool names that disabled lists may still contain,
/// mapped to the canonical host name they now mean.
pub const LEGACY_TOOL_ALIASES: [(&str, &str); 7] = [
    ("aft_read", "read"),
    ("aft_write", "write"),
    ("aft_edit", "edit"),
    ("aft_apply_patch", "apply_patch"),
    ("aft_grep", "grep"),
    ("aft_glob", "glob"),
    ("aft_bash", "bash"),
];

/// Names removed by the historical `bash` registration gate (bash turned
/// off), expressed with canonical names.
pub const BASH_GATE_DISABLES: [&str; 5] = [
    "bash",
    "bash_kill",
    "bash_status",
    "bash_watch",
    "bash_write",
];

/// Identity of the migration policy that retires the keys below.
pub const MIGRATION_POLICY_ID: &str = "feature-config-v1";
/// First minor release that translates the retired keys.
pub const POLICY_INTRODUCED_MINOR: (u64, u64) = (0, 58);

pub const REMOVED_OPENCODE_PATHS: [(&str, &str); 2] = [
    ("opencode.server_url", "permission.assert"),
    ("opencode.server_password_env", "permission.assert"),
];

/// Keys the per-repository data store leaves with nothing to do. User config
/// refuses them; project config drops them with their existing no-effect notice.
/// Mirrors the TypeScript `REMOVED_DATA_LAYER_PATHS`.
pub const REMOVED_DATA_LAYER_PATHS: [&str; 3] = [
    "views.callgraph_copies",
    "views.enabled",
    "worktree.ram_overlay",
];

/// `views.idle_checkout_hours` and the key that replaces it. It is translated
/// like any retired key (and the user file rewritten) until
/// [`DATA_LAYER_KEYS_REFUSED_FROM`], then refused.
pub const RETIRED_IDLE_CHECKOUT_HOURS: (&str, &str) =
    ("views.idle_checkout_hours", "idle.checkout_hours");

/// First release, as (major, minor), that refuses the keys in
/// [`DATA_LAYER_RETIRED_PATHS`] with `removed_config_key`. Earlier releases
/// translate or keep them, with a notice. Mirrors the TypeScript
/// `DATA_LAYER_KEYS_REFUSED_FROM`.
pub const DATA_LAYER_KEYS_REFUSED_FROM: (u64, u64) = (0, 65);

/// The remaining version-gated data-layer rename. The removed no-effect keys
/// already follow [`REFUSED_RETIRED_PATHS`]' user-only refusal policy.
pub const DATA_LAYER_RETIRED_PATHS: [&str; 1] = ["views.idle_checkout_hours"];

/// The (major, minor) release a version string names, ignoring the patch and
/// any pre-release or build suffix.
fn parse_release(version: &str) -> Option<(u64, u64)> {
    let mut parts = version.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    Some((major, minor))
}

/// The (major, minor) release of this build, from the crate version.
pub fn running_release() -> (u64, u64) {
    parse_release(env!("CARGO_PKG_VERSION")).unwrap_or((0, 0))
}

/// `0.65.0` for [`DATA_LAYER_KEYS_REFUSED_FROM`], as notices print it.
fn refused_from_version() -> String {
    let (major, minor) = DATA_LAYER_KEYS_REFUSED_FROM;
    format!("{major}.{minor}.0")
}

/// Removed settings fail only in the base and first-level harness blocks.
pub fn removed_config_key_errors(map: &Map<String, Value>) -> Vec<String> {
    removed_config_key_errors_at(map, running_release())
}

/// [`removed_config_key_errors`] for one config file. Retired names in
/// [`REFUSED_RETIRED_PATHS`] and the prefixed tool names refuse
/// only the user file, which AFT rewrites itself. A project file belongs to
/// its repository, where a refusal would leave everyone who opens it without
/// AFT until a tracked file is edited, so those names are translated with a
/// notice there. Settings removed outright still refuse either file. Mirrors
/// the plugins' `removedConfigKeyErrors(..., refuseRetired)`.
pub fn removed_config_key_errors_for_file(
    map: &Map<String, Value>,
    user_file: bool,
) -> Vec<String> {
    removed_errors(map, running_release(), user_file)
}

/// [`removed_config_key_errors`] as the release `release` judges it: the
/// data-layer keys fail only from [`DATA_LAYER_KEYS_REFUSED_FROM`].
pub fn removed_config_key_errors_at(map: &Map<String, Value>, release: (u64, u64)) -> Vec<String> {
    removed_errors(map, release, true)
}

fn removed_errors(
    map: &Map<String, Value>,
    release: (u64, u64),
    refuse_retired: bool,
) -> Vec<String> {
    let mut errors = removed_block_key_errors(map, release, refuse_retired);
    if let Some(harnesses) = map.get("harnesses").and_then(Value::as_object) {
        for (name, block) in harnesses {
            if let Some(block) = block.as_object() {
                for error in removed_block_key_errors(block, release, refuse_retired) {
                    errors.push(error.replacen(
                        "removed_config_key:",
                        &format!("removed_config_key:harnesses.{name}."),
                        1,
                    ));
                }
            }
        }
    }
    errors
}

fn removed_block_key_errors(
    map: &Map<String, Value>,
    release: (u64, u64),
    refuse_retired: bool,
) -> Vec<String> {
    let mut errors = removed_opencode_key_errors(map);
    if refuse_retired {
        errors.extend(
            refused_retired_names(map).into_iter().map(|name| {
                format!("removed_config_key:{name}; run npx @cortexkit/aft doctor --fix")
            }),
        );
    }
    if refuse_retired && release >= DATA_LAYER_KEYS_REFUSED_FROM {
        for path in DATA_LAYER_RETIRED_PATHS {
            if has_path(map, path) {
                errors.push(format!(
                    "removed_config_key:{path}; run npx @cortexkit/aft doctor --fix"
                ));
            }
        }
    }
    errors
}

fn removed_opencode_key_errors(map: &Map<String, Value>) -> Vec<String> {
    let Some(opencode) = map.get("opencode").and_then(Value::as_object) else {
        return Vec::new();
    };
    let mut errors = Vec::new();
    for (path, _) in REMOVED_OPENCODE_PATHS {
        if opencode.contains_key(path.strip_prefix("opencode.").unwrap()) {
            errors.push(format!(
                "removed_config_key:{path}; run npx @cortexkit/aft doctor --fix"
            ));
        }
    }
    errors
}

/// Retired names refused in user config, in the order a
/// refusal lists them. A config that still sets one is refused with
/// `removed_config_key`; `doctor --fix` (or the automatic rewrite of the user
/// file) replaces it with its current equivalent or deletes it. Names retired
/// more recently (`bridge.hang_threshold`) are not
/// listed: they are still translated, with a notice. Mirrors the TypeScript
/// `REFUSED_RETIRED_PATHS`.
pub const REFUSED_RETIRED_PATHS: [&str; 23] = [
    "sandbox.enabled",
    "tool_surface",
    "hoist_builtin_tools",
    "enabled",
    "search_index",
    "experimental_search_index",
    "semantic_search",
    "experimental_semantic_search",
    "callgraph_store",
    "github.enabled",
    "gh_read",
    "gh_shim.enabled",
    "experimental_lsp_ty",
    "experimental_bash_rewrite",
    "experimental_bash_compress",
    "experimental_bash_background",
    "experimental.bash",
    "idle.lsp_ttl_minutes",
    "inspect.tier2_soft_deadline_ms",
    "inspect.max_drill_down_items",
    "views.callgraph_copies",
    "views.enabled",
    "worktree.ram_overlay",
];

/// The refused retired names one block sets: the paths in
/// [`REFUSED_RETIRED_PATHS`], then each prefixed tool name
/// ([`LEGACY_TOOL_ALIASES`]) its `disabled_tools` list contains, once each and
/// by its bare name, as the retired-key notices list it.
fn refused_retired_names(map: &Map<String, Value>) -> Vec<String> {
    let mut names: Vec<String> = REFUSED_RETIRED_PATHS
        .iter()
        .filter(|path| has_path(map, path))
        .map(|path| (*path).to_string())
        .collect();
    if let Some(Value::Array(entries)) = map.get("disabled_tools") {
        for name in entries.iter().filter_map(Value::as_str) {
            if legacy_tool_alias(name).is_some() && !names.iter().any(|known| known == name) {
                names.push(name.to_string());
            }
        }
    }
    names
}

/// Retired top-level (or nested) config paths and their replacement.
pub const RETIRED_PATHS: [(&str, &str); 15] = [
    ("sandbox.enabled", "sandbox.profile"),
    ("tool_surface", "disabled_tools"),
    ("hoist_builtin_tools", "disabled_tools"),
    ("enabled", "disabled_tools"),
    ("search_index", "indexes.trigram"),
    ("experimental_search_index", "indexes.trigram"),
    ("semantic_search", "indexes.semantic"),
    ("experimental_semantic_search", "indexes.semantic"),
    ("callgraph_store", "indexes.callgraph"),
    ("github.enabled", "github.read,github.write,github.shim"),
    ("experimental_lsp_ty", "experimental.lsp_ty"),
    ("experimental_bash_rewrite", "bash.rewrite"),
    ("experimental_bash_compress", "bash.compress"),
    ("experimental_bash_background", "bash.background"),
    ("experimental.bash", "bash"),
];

/// Retired inspect/LSP paths and their replacement. `idle.lsp_ttl_minutes`
/// moves to `lsp.idle_minutes`; the two inspect keys never had an effect and
/// are dropped.
pub const REMOVED_INSPECT_LSP_PATHS: [(&str, &str); 3] = [
    ("idle.lsp_ttl_minutes", "lsp.idle_minutes"),
    (
        "inspect.tier2_soft_deadline_ms",
        "inspect.tier2_pass_timeout_ms",
    ),
    ("inspect.max_drill_down_items", "aft_inspect.topK"),
];

/// Retired paths that no longer do anything and have no replacement. They are
/// dropped on load and removed by `doctor --fix`, and the notice says so.
/// `bridge.hang_threshold` counted request timeouts toward restarting the
/// bridge; a timeout now fails only its own request, and the bridge's hang
/// watchdog judges a stuck child by its silence instead.
/// `inspect.tier2_idle_minutes` delayed the whole-repository analyses the
/// OpenCode plugin started when a session went idle; those analyses now run
/// only when `aft_inspect` is called.
pub const REMOVED_NO_EFFECT_PATHS: [(&str, &str); 2] = [
    ("bridge.hang_threshold", ""),
    ("inspect.tier2_idle_minutes", ""),
];

/// Notice sentence for a retired `bridge.hang_threshold`. Mirrors the
/// TypeScript `RETIRED_HANG_THRESHOLD_NOTE`.
pub const RETIRED_HANG_THRESHOLD_NOTE: &str = "bridge.hang_threshold no longer does anything: a timed-out request no longer restarts AFT, so the setting can be removed.";

/// Index leaf, its immediate legacy key and (when one exists) the older
/// experimental alias, in precedence order after the canonical leaf.
pub const INDEX_INPUTS: [(&str, &str, Option<&str>); 3] = [
    ("trigram", "search_index", Some("experimental_search_index")),
    (
        "semantic",
        "semantic_search",
        Some("experimental_semantic_search"),
    ),
    ("callgraph", "callgraph_store", None),
];

/// Whether a raw tool name is a protected slot a project config may not disable.
pub fn is_project_protected_tool(name: &str) -> bool {
    name == "aft_safety" || HOST_TOOL_NAMES.contains(&name)
}

/// Whether a name belongs to the complete canonical tool inventory.
pub fn is_known_tool(name: &str) -> bool {
    CANONICAL_TOOLS.contains(&name)
}

/// Canonical name for a historical prefixed alias, if `name` is one.
pub fn legacy_tool_alias(name: &str) -> Option<&'static str> {
    LEGACY_TOOL_ALIASES
        .iter()
        .find(|(alias, _)| *alias == name)
        .map(|(_, canonical)| *canonical)
}

/// Literal disabled set a legacy `tool_surface` value translates to. The sets
/// are the canonical inventory minus what that surface registered on OpenCode
/// with hoisting and every gate on.
pub fn surface_disables(surface: &str) -> Option<Vec<&'static str>> {
    match surface {
        "all" => Some(Vec::new()),
        "recommended" => Some(vec!["aft_callgraph", "aft_delete", "aft_move"]),
        "minimal" => Some(
            CANONICAL_TOOLS
                .iter()
                .copied()
                .filter(|name| !matches!(*name, "aft_outline" | "aft_zoom" | "aft_safety"))
                .collect(),
        ),
        _ => None,
    }
}

/// A non-fatal translation notice.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TranslationWarning {
    pub code: &'static str,
    pub key: String,
    pub message: String,
}

/// Result of translating one raw configuration document.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DocumentTranslation {
    pub warnings: Vec<TranslationWarning>,
    /// True when any retired key or alias was supplied (a migration notice applies).
    pub legacy_input: bool,
    /// Every retired key the document supplied, sorted and de-duplicated. A
    /// key inside a harness block carries its `harnesses.<id>.` prefix; a
    /// prefixed tool name inside `disabled_tools` is listed by that name.
    pub retired_keys: Vec<String>,
}

/// Record one supplied retired key of the block whose key prefix is `prefix`.
fn record_retired(out: &mut DocumentTranslation, prefix: &str, key: &str) {
    out.legacy_input = true;
    out.retired_keys.push(format!("{prefix}{key}"));
}

fn superseded(out: &mut DocumentTranslation, key: String, message: String) {
    out.warnings.push(TranslationWarning {
        code: "superseded_legacy_config",
        key,
        message,
    });
}

fn nested_bool(map: &Map<String, Value>, container: &str, leaf: &str) -> Option<bool> {
    map.get(container)?.as_object()?.get(leaf)?.as_bool()
}

fn has_path(map: &Map<String, Value>, path: &str) -> bool {
    match path.split_once('.') {
        Some((container, leaf)) => map
            .get(container)
            .and_then(Value::as_object)
            .is_some_and(|inner| inner.contains_key(leaf)),
        None => map.contains_key(path),
    }
}

/// Whether a false retained runtime gate (backup/inspect/bash) is supplied.
fn false_runtime_gates(map: &Map<String, Value>) -> Vec<&'static str> {
    let mut gates = Vec::new();
    if nested_bool(map, "backup", "enabled") == Some(false) {
        gates.push("backup.enabled");
    }
    if nested_bool(map, "inspect", "enabled") == Some(false) {
        gates.push("inspect.enabled");
    }
    match map.get("bash") {
        Some(Value::Bool(false)) => gates.push("bash"),
        Some(Value::Object(bash)) if bash.get("enabled") == Some(&Value::Bool(false)) => {
            gates.push("bash.enabled");
        }
        _ => {}
    }
    gates
}

/// Translate the GitHub enable aliases (`gh_read.enabled`, `gh_shim.enabled`)
/// of one block into `github.read` / `github.shim`, exactly as `doctor --fix`
/// rewrites them. Precedence: the canonical leaf, then the `false` that a
/// `github.enabled: false` in the same block sets for every leaf, then the
/// alias. The aliases are removed; `gh_shim.binary_path` is kept.
fn translate_github_aliases(
    map: &mut Map<String, Value>,
    prefix: &str,
    out: &mut DocumentTranslation,
) {
    let master_off = nested_bool(map, "github", "enabled") == Some(false);
    for (alias, leaf, retired_key) in [
        ("gh_read", "read", "gh_read"),
        ("gh_shim", "shim", "gh_shim.enabled"),
    ] {
        let supplied = if alias == "gh_read" {
            map.contains_key(alias)
        } else {
            map.get(alias)
                .and_then(Value::as_object)
                .is_some_and(|shim| shim.contains_key("enabled"))
        };
        if !supplied {
            continue;
        }
        record_retired(out, prefix, retired_key);
        let alias_value = map
            .get(alias)
            .and_then(|value| value.get("enabled"))
            .and_then(Value::as_bool);
        if let Some(value) = alias_value {
            match nested_bool(map, "github", leaf) {
                Some(canonical) => {
                    if canonical != value {
                        superseded(
                            out,
                            format!("{prefix}{retired_key}"),
                            format!("{alias}.enabled={value} is ignored because github.{leaf}={canonical} is set"),
                        );
                    }
                }
                None if master_off => {
                    if value {
                        superseded(
                            out,
                            format!("{prefix}{retired_key}"),
                            format!("{alias}.enabled=true is ignored because github.enabled=false switches github.{leaf} off"),
                        );
                    }
                }
                None => {
                    let github = map
                        .entry("github".to_string())
                        .or_insert_with(|| Value::Object(Map::new()));
                    if let Value::Object(github) = github {
                        github.insert(leaf.to_string(), Value::Bool(value));
                    }
                }
            }
        }
        if alias == "gh_read" {
            map.remove(alias);
        } else if let Some(Value::Object(shim)) = map.get_mut(alias) {
            shim.remove("enabled");
            if shim.is_empty() {
                map.remove(alias);
            }
        }
    }
}

/// Remove `container.leaf` when present, dropping the container if that left
/// it empty. Returns the removed value.
fn take_nested(map: &mut Map<String, Value>, container: &str, leaf: &str) -> Option<Value> {
    let inner = map.get_mut(container)?.as_object_mut()?;
    let value = inner.remove(leaf)?;
    if inner.is_empty() {
        map.remove(container);
    }
    Some(value)
}

/// Largest integer a JSON number keeps exactly in JavaScript
/// (`Number.MAX_SAFE_INTEGER`).
const MAX_SAFE_INTEGER: i64 = 9_007_199_254_740_991;

/// The whole number a JSON value spells, the way JavaScript's
/// `Number.isSafeInteger` reads the parsed value: `12` and `12.0` are both 12,
/// while `12.5`, non-numbers and integers beyond the exactly representable
/// range are not whole numbers. Keeps the Rust and TypeScript translations
/// equal, since JavaScript cannot tell `12.0` from `12`.
fn safe_whole_number(value: &Value) -> Option<i64> {
    if let Some(number) = value.as_i64() {
        return (number.unsigned_abs() <= MAX_SAFE_INTEGER as u64).then_some(number);
    }
    let number = value.as_f64()?;
    (number.is_finite() && number.fract() == 0.0 && number.abs() <= MAX_SAFE_INTEGER as f64)
        .then_some(number as i64)
}

/// The `lsp.idle_minutes` value a retired `idle.lsp_ttl_minutes` value
/// becomes: a whole number clamped to the current range, or the default for
/// anything else. Shared by load-time translation and `doctor --fix`; mirrors
/// the TypeScript translation.
pub fn retired_lsp_idle_minutes(value: &Value) -> i64 {
    safe_whole_number(value)
        .unwrap_or(i64::from(crate::config::DEFAULT_LSP_IDLE_MINUTES))
        .clamp(
            i64::from(crate::config::MIN_LSP_IDLE_MINUTES),
            i64::from(crate::config::MAX_LSP_IDLE_MINUTES),
        )
}

/// Translate the retired inspect/LSP keys of one block, exactly as
/// `doctor --fix` rewrites them: `idle.lsp_ttl_minutes` becomes
/// `lsp.idle_minutes` (clamped to its range) unless that is already set, and
/// the two inspect keys that never had an effect are dropped.
fn translate_inspect_lsp_paths(
    map: &mut Map<String, Value>,
    prefix: &str,
    out: &mut DocumentTranslation,
) {
    if let Some(value) = take_nested(map, "idle", "lsp_ttl_minutes") {
        record_retired(out, prefix, "idle.lsp_ttl_minutes");
        if map
            .get("lsp")
            .and_then(|lsp| lsp.get("idle_minutes"))
            .is_some()
        {
            superseded(
                out,
                format!("{prefix}idle.lsp_ttl_minutes"),
                "idle.lsp_ttl_minutes is ignored because lsp.idle_minutes is set".to_string(),
            );
        } else {
            let minutes = retired_lsp_idle_minutes(&value);
            let lsp = map
                .entry("lsp".to_string())
                .or_insert_with(|| Value::Object(Map::new()));
            if let Value::Object(lsp) = lsp {
                lsp.insert("idle_minutes".to_string(), Value::from(minutes));
            }
        }
    }
    for leaf in ["tier2_soft_deadline_ms", "max_drill_down_items"] {
        if take_nested(map, "inspect", leaf).is_some() {
            record_retired(out, prefix, &format!("inspect.{leaf}"));
        }
    }
}

/// Drop the retired keys that no longer do anything
/// ([`REMOVED_NO_EFFECT_PATHS`]), exactly as `doctor --fix` removes them.
fn drop_no_effect_paths(map: &mut Map<String, Value>, prefix: &str, out: &mut DocumentTranslation) {
    for (path, _) in REMOVED_NO_EFFECT_PATHS {
        if let Some((container, leaf)) = path.split_once('.') {
            if take_nested(map, container, leaf).is_some() {
                record_retired(out, prefix, path);
            }
        }
    }
}

/// The `idle.checkout_hours` value a retired `views.idle_checkout_hours`
/// value becomes: a whole number clamped to the current range, or the default
/// for anything else. Shared by load-time translation and `doctor --fix`;
/// mirrors the TypeScript translation.
pub fn retired_idle_checkout_hours(value: &Value) -> i64 {
    safe_whole_number(value)
        .unwrap_or(i64::from(crate::config::DEFAULT_IDLE_CHECKOUT_HOURS))
        .clamp(
            i64::from(crate::config::MIN_IDLE_CHECKOUT_HOURS),
            i64::from(crate::config::MAX_IDLE_CHECKOUT_HOURS),
        )
}

/// Translate the keys the data-layer cutover retires in one block:
/// `views.idle_checkout_hours` becomes `idle.checkout_hours` (clamped to its
/// range) unless that is already set, exactly as `doctor --fix` rewrites it.
/// Removed no-effect keys are dropped before strict schema validation.
fn translate_data_layer_paths(
    map: &mut Map<String, Value>,
    prefix: &str,
    out: &mut DocumentTranslation,
) {
    let (retired, replacement) = RETIRED_IDLE_CHECKOUT_HOURS;
    if let Some(value) = take_nested(map, "views", "idle_checkout_hours") {
        record_retired(out, prefix, retired);
        if map
            .get("idle")
            .and_then(|idle| idle.get("checkout_hours"))
            .is_some()
        {
            superseded(
                out,
                format!("{prefix}{retired}"),
                format!("{retired} is ignored because {replacement} is set"),
            );
        } else {
            let hours = retired_idle_checkout_hours(&value);
            let idle = map
                .entry("idle".to_string())
                .or_insert_with(|| Value::Object(Map::new()));
            if let Value::Object(idle) = idle {
                idle.insert("checkout_hours".to_string(), Value::from(hours));
            }
        }
    }
    for path in REMOVED_DATA_LAYER_PATHS {
        let (container, leaf) = path.split_once('.').unwrap();
        if take_nested(map, container, leaf).is_some() {
            record_retired(out, prefix, path);
        }
    }
}

/// What a retired `enabled: false` no longer does. The translation only hides
/// tools; indexing continues, so a user who relied on it to keep AFT out of a
/// repository must also switch the indexes off. Mirrors the TypeScript
/// `RETIRED_ENABLED_FALSE_INDEXES_NOTE`.
pub const RETIRED_ENABLED_FALSE_INDEXES_NOTE: &str = "enabled: false no longer turns AFT off: it is translated to disabling every tool, but the trigram, semantic and callgraph indexes still build. To keep AFT from indexing this repository, also set indexes.trigram, indexes.semantic and indexes.callgraph to false.";

/// Translate flat experimental aliases to nested settings and move the
/// `experimental.bash` block to top-level `bash`. Graduating a block that sets
/// a feature flag explicitly sets its omitted flags to false; otherwise the
/// current default-on bash settings would silently enable them.
fn translate_experimental_paths(
    map: &mut Map<String, Value>,
    prefix: &str,
    out: &mut DocumentTranslation,
) {
    for (old_key, leaf) in [
        ("experimental_lsp_ty", "lsp_ty"),
        ("experimental_bash_rewrite", "rewrite"),
        ("experimental_bash_compress", "compress"),
        ("experimental_bash_background", "background"),
    ] {
        let Some(value) = map.remove(old_key) else {
            continue;
        };
        record_retired(out, prefix, old_key);
        let experimental = map
            .entry("experimental")
            .or_insert_with(|| Value::Object(Map::new()));
        if !experimental.is_object() {
            *experimental = Value::Object(Map::new());
        }
        let experimental = experimental.as_object_mut().unwrap();
        let destination = if leaf == "lsp_ty" {
            experimental
        } else {
            let bash = experimental
                .entry("bash")
                .or_insert_with(|| Value::Object(Map::new()));
            if !bash.is_object() {
                *bash = Value::Object(Map::new());
            }
            bash.as_object_mut().unwrap()
        };
        if destination.contains_key(leaf) {
            let path = if leaf == "lsp_ty" {
                leaf.to_string()
            } else {
                format!("bash.{leaf}")
            };
            superseded(
                out,
                old_key.to_string(),
                format!("{old_key} is ignored because experimental.{path} is already set"),
            );
        } else {
            destination.insert(leaf.to_string(), value);
        }
    }

    let Some(experimental) = map.get("experimental").and_then(Value::as_object) else {
        return;
    };
    let Some(legacy) = experimental.get("bash") else {
        return;
    };
    if let Some(legacy) = legacy.as_object() {
        if legacy.is_empty() {
            record_retired(out, prefix, "experimental.bash");
        }
        for leaf in legacy.keys() {
            record_retired(out, prefix, &format!("experimental.bash.{leaf}"));
        }
        if map.contains_key("bash") {
            superseded(
                out,
                "experimental.bash".to_string(),
                "experimental.bash is ignored because top-level \"bash\" is already set"
                    .to_string(),
            );
        } else {
            let mut bash = Map::new();
            // A block with only reminder settings never opted into the
            // feature flags, so they keep their current defaults instead of
            // being written as false.
            if ["rewrite", "compress", "background"]
                .iter()
                .any(|leaf| legacy.contains_key(*leaf))
            {
                for leaf in ["rewrite", "compress", "background"] {
                    bash.insert(
                        leaf.to_string(),
                        Value::Bool(legacy.get(leaf) == Some(&Value::Bool(true))),
                    );
                }
            }
            for leaf in [
                "long_running_reminder_enabled",
                "long_running_reminder_interval_ms",
            ] {
                if let Some(value) = legacy.get(leaf) {
                    bash.insert(leaf.to_string(), value.clone());
                }
            }
            if !bash.is_empty() {
                map.insert("bash".to_string(), Value::Object(bash));
            }
        }
    } else {
        record_retired(out, prefix, "experimental.bash");
    }
    let experimental = map
        .get_mut("experimental")
        .unwrap()
        .as_object_mut()
        .unwrap();
    experimental.remove("bash");
    if experimental.is_empty() {
        map.remove("experimental");
    }
}

/// Translate the retired keys of one tier/harness block in place.
///
/// The false runtime gates (`backup.enabled`, `inspect.enabled`, `bash`,
/// `bash.enabled`) are current keys and only switch their behaviour off; they
/// never remove a tool registration. Only `disabled_tools` does that.
fn translate_block(
    map: &mut Map<String, Value>,
    // True only for the user file's base block, the one place the
    // absent-base default disables may be added.
    user_base: bool,
    block_label: &str,
    out: &mut DocumentTranslation,
) {
    let prefix = if block_label == "base" {
        String::new()
    } else {
        format!("{block_label}.")
    };
    translate_github_aliases(map, &prefix, out);
    translate_inspect_lsp_paths(map, &prefix, out);
    translate_data_layer_paths(map, &prefix, out);
    drop_no_effect_paths(map, &prefix, out);
    translate_experimental_paths(map, &prefix, out);
    if let Some(Value::Object(sandbox)) = map.get_mut("sandbox") {
        if let Some(enabled) = sandbox.remove("enabled") {
            record_retired(out, &prefix, "sandbox.enabled");
            match enabled.as_bool() {
                Some(enabled) => {
                    let profile = sandbox
                        .entry("profile")
                        .or_insert_with(|| Value::Object(Map::new()));
                    if let Value::Object(profile) = profile {
                        for (role, mode) in [
                            ("worker", if enabled { "strict" } else { "relaxed" }),
                            ("head", if enabled { "strict" } else { "off" }),
                        ] {
                            if profile.get(role).is_some_and(|value| value != mode) {
                                superseded(out, format!("{prefix}sandbox.enabled"), format!("sandbox.profile.{role} takes precedence over sandbox.enabled"));
                            }
                            profile
                                .entry(role)
                                .or_insert_with(|| Value::String(mode.to_string()));
                        }
                    }
                }
                None => out.warnings.push(TranslationWarning {
                    code: "invalid_legacy_config",
                    key: format!("{prefix}sandbox.enabled"),
                    message: "Ignoring non-boolean sandbox.enabled; use sandbox.profile"
                        .to_string(),
                }),
            }
        }
    }

    // Canonicalize the retired `aft_`-prefixed host tool names (for example
    // `aft_read` -> `read`) inside the disabled list.
    let mut explicit_list = None;
    if let Some(Value::Array(entries)) = map.get("disabled_tools") {
        if entries.iter().all(Value::is_string) {
            let mut canonical = Vec::with_capacity(entries.len());
            let mut aliases = Vec::new();
            for name in entries.iter().filter_map(Value::as_str) {
                match legacy_tool_alias(name) {
                    Some(host) => {
                        aliases.push(name.to_string());
                        canonical.push(host.to_string());
                    }
                    None => canonical.push(name.to_string()),
                }
            }
            for alias in aliases {
                record_retired(out, &prefix, &alias);
            }
            explicit_list = Some(canonical);
        }
    }

    // `experimental.bash` was already recorded, leaf by leaf, and removed by
    // `translate_experimental_paths`.
    for (path, _) in RETIRED_PATHS {
        if has_path(map, path) {
            record_retired(out, &prefix, path);
        }
    }
    let gates = false_runtime_gates(map);

    if let Some(list) = &explicit_list {
        map.insert(
            "disabled_tools".to_string(),
            Value::Array(list.iter().cloned().map(Value::String).collect()),
        );
    }

    // Index switches: canonical leaf, then immediate legacy name, then the
    // older experimental alias.
    for (leaf, legacy, experimental) in INDEX_INPUTS {
        let canonical = map
            .get("indexes")
            .and_then(Value::as_object)
            .and_then(|indexes| indexes.get(leaf))
            .and_then(Value::as_bool);
        let legacy_value = map.remove(legacy);
        let experimental_value = experimental.and_then(|key| map.remove(key));
        let mut chosen = None;
        for (key, value) in [
            (Some(legacy), legacy_value),
            (experimental, experimental_value),
        ] {
            let (Some(key), Some(value)) = (key, value) else {
                continue;
            };
            let Some(value) = value.as_bool() else {
                out.warnings.push(TranslationWarning {
                    code: "invalid_legacy_config",
                    key: key.to_string(),
                    message: format!("Ignoring non-boolean {key}; use indexes.{leaf}"),
                });
                continue;
            };
            if let Some(canonical) = canonical {
                if canonical != value {
                    superseded(
                        out,
                        key.to_string(),
                        format!(
                            "{key}={value} is ignored because indexes.{leaf}={canonical} is set"
                        ),
                    );
                }
            } else if let Some(previous) = chosen {
                if previous != value {
                    superseded(
                        out,
                        key.to_string(),
                        format!(
                            "{key}={value} is ignored because {legacy}={previous} takes precedence"
                        ),
                    );
                }
            } else {
                chosen = Some(value);
            }
        }
        if canonical.is_none() {
            if let Some(value) = chosen {
                let indexes = map
                    .entry("indexes".to_string())
                    .or_insert_with(|| Value::Object(Map::new()));
                if let Value::Object(indexes) = indexes {
                    indexes.insert(leaf.to_string(), Value::Bool(value));
                }
            }
        }
    }

    // github.enabled: false switches every leaf off unless the leaf is explicit.
    if let Some(Value::Object(github)) = map.get_mut("github") {
        if let Some(master) = github.remove("enabled") {
            if master == Value::Bool(false) {
                let mut notes = Vec::new();
                for leaf in ["read", "write", "shim"] {
                    match github.get(leaf) {
                        None => {
                            github.insert(leaf.to_string(), Value::Bool(false));
                        }
                        Some(Value::Bool(true)) => notes.push(format!(
                            "github.enabled=false is ignored for github.{leaf} because github.{leaf}=true is set"
                        )),
                        Some(_) => {}
                    }
                }
                for note in notes {
                    superseded(out, "github.enabled".to_string(), note);
                }
            }
        }
    }

    // Registration generators.
    let surface = map.remove("tool_surface");
    let hoist = map.remove("hoist_builtin_tools");
    let enabled = map.remove("enabled");
    let mut generated: BTreeSet<String> = BTreeSet::new();
    let mut explicit_surface = false;
    if let Some(surface) = surface {
        match surface.as_str().and_then(surface_disables) {
            Some(names) => {
                explicit_surface = true;
                generated.extend(names.into_iter().map(str::to_string));
            }
            None => out.warnings.push(TranslationWarning {
                code: "invalid_legacy_config",
                key: "tool_surface".to_string(),
                message: "Ignoring unknown tool_surface value; use disabled_tools".to_string(),
            }),
        }
    }
    if hoist == Some(Value::Bool(false)) {
        generated.extend(HOST_TOOL_NAMES.iter().map(|name| (*name).to_string()));
    }
    if enabled == Some(Value::Bool(false)) {
        generated.extend(CANONICAL_TOOLS.iter().map(|name| (*name).to_string()));
        out.warnings.push(TranslationWarning {
            code: "legacy_enabled_false_indexes_still_build",
            key: if block_label == "base" {
                "enabled".to_string()
            } else {
                format!("{block_label}.enabled")
            },
            message: RETIRED_ENABLED_FALSE_INDEXES_NOTE.to_string(),
        });
    }

    if !gates.is_empty() && explicit_list.is_none() {
        out.warnings.push(TranslationWarning {
            code: "legacy_runtime_gate_runtime_only",
            key: block_label.to_string(),
            message: format!(
                "{} now restricts runtime behavior only and no longer removes tool registrations; list tools in disabled_tools to unregister them",
                gates.join(", ")
            ),
        });
    }

    if explicit_list.is_some() {
        if explicit_surface || !generated.is_empty() {
            superseded(
                out,
                block_label.to_string(),
                "disabled_tools is set explicitly, so legacy tool_surface/hoist_builtin_tools/enabled do not change registration".to_string(),
            );
        }
        return;
    }

    let list = if user_base {
        if explicit_surface {
            Some(generated)
        } else if !generated.is_empty() {
            let mut with_default = generated;
            with_default.extend(
                DEFAULT_DISABLED_TOOLS
                    .iter()
                    .map(|name| (*name).to_string()),
            );
            Some(with_default)
        } else {
            None
        }
    } else {
        (!generated.is_empty()).then_some(generated)
    };
    if let Some(list) = list {
        map.insert(
            "disabled_tools".to_string(),
            Value::Array(list.into_iter().map(Value::String).collect()),
        );
    }
}

/// Which trust tier a raw document belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DocumentTier {
    User,
    Project,
}

/// Translate every block (base plus each `harnesses.<id>` object) of one raw
/// configuration document, in place.
///
/// Only the user file's base block receives the absent-base default
/// (`aft_move`/`aft_delete`) when its legacy keys generate disables: that
/// default applies once, at user-base resolution. A project file's base block
/// contributes only the names its own legacy keys imply, so a legacy key in a
/// repository can never re-disable tools the user enabled. The translated
/// values are current keys, so the resolver's project-tier rules apply to them
/// exactly as if the file had spelled them out.
pub fn translate_document(map: &mut Map<String, Value>, tier: DocumentTier) -> DocumentTranslation {
    let mut out = DocumentTranslation::default();
    translate_block(map, tier == DocumentTier::User, "base", &mut out);
    if let Some(Value::Object(harnesses)) = map.get_mut("harnesses") {
        for (name, block) in harnesses.iter_mut() {
            if let Value::Object(block) = block {
                translate_block(block, false, &format!("harnesses.{name}"), &mut out);
            }
        }
    }
    out.retired_keys.sort();
    out.retired_keys.dedup();
    out
}

/// Whether a translation carries the retired `enabled: false` note.
fn has_enabled_false_note(translation: &DocumentTranslation) -> bool {
    translation
        .warnings
        .iter()
        .any(|warning| warning.code == "legacy_enabled_false_indexes_still_build")
}

/// Retired keys that no longer do anything, each with the notice sentence that
/// says so. Nothing is applied for them, so a notice never counts them among
/// the keys whose "current equivalents" were applied. Kept in key order;
/// mirrors the TypeScript `NO_EFFECT_RETIRED_NOTES`.
pub const NO_EFFECT_RETIRED_NOTES: [(&str, &str); 4] = [
    ("bridge.hang_threshold", RETIRED_HANG_THRESHOLD_NOTE),
    (
        "inspect.max_drill_down_items",
        "inspect.max_drill_down_items never had an effect, so the setting can be removed.",
    ),
    (
        "inspect.tier2_idle_minutes",
        "inspect.tier2_idle_minutes no longer does anything: AFT runs whole-repository analyses only when aft_inspect is called, so the setting can be removed.",
    ),
    (
        "inspect.tier2_soft_deadline_ms",
        "inspect.tier2_soft_deadline_ms never had an effect, so the setting can be removed.",
    ),
];

/// The no-effect sentence for a retired key, which may carry a
/// `harnesses.<id>.` prefix.
fn no_effect_note(key: &str) -> Option<&'static str> {
    NO_EFFECT_RETIRED_NOTES
        .iter()
        .chain(REMOVED_DATA_LAYER_NOTES.iter())
        .find(|(path, _)| names_path(key, path))
        .map(|(_, note)| *note)
}

/// Whether `key`, which may carry a `harnesses.<id>.` prefix, is `path`.
fn names_path(key: &str, path: &str) -> bool {
    key == path
        || key
            .strip_suffix(path)
            .is_some_and(|prefix| prefix.ends_with('.'))
}

/// Why each removed data-layer key no longer does anything, in
/// [`REMOVED_DATA_LAYER_PATHS`] order. Mirrors the TypeScript
/// `REMOVED_DATA_LAYER_NOTES`.
pub const REMOVED_DATA_LAYER_NOTES: [(&str, &str); 3] = [
    (
        "views.callgraph_copies",
        "views.callgraph_copies is gone because no checkout borrows another checkout's call graph any more.",
    ),
    (
        "views.enabled",
        "views.enabled is gone because AFT now keeps one index store per repository and always uses it.",
    ),
    (
        "worktree.ram_overlay",
        "worktree.ram_overlay is gone because every worktree now has its own index state, so there is no borrowed index to patch in memory.",
    ),
];

/// The sentence saying a file that still names `views.idle_checkout_hours`
/// will be refused. Mirrors the TypeScript `retiredIdleCheckoutHoursNote`.
pub fn retired_idle_checkout_hours_note() -> String {
    let (retired, replacement) = RETIRED_IDLE_CHECKOUT_HOURS;
    format!(
        "{retired} is now {replacement}; from {} AFT refuses a config file that still sets {retired}.",
        refused_from_version()
    )
}

/// Build a retired-key notice. `with_equivalents` renders the sentence about
/// keys whose current equivalents were applied, given those keys joined by
/// ", "; it is used only when there are such keys. The sentences for keys
/// that no longer do anything follow, once each. A file whose only retired
/// keys have no effect gets just those sentences. `still_in_file` is true
/// when the file keeps the retired keys (it was not rewritten), so the notice
/// warns about keys a later release refuses.
fn retired_keys_notice(
    translation: &DocumentTranslation,
    still_in_file: bool,
    with_equivalents: impl FnOnce(&str) -> String,
) -> String {
    let mut equivalents = Vec::new();
    let mut notes: Vec<&str> = Vec::new();
    for key in &translation.retired_keys {
        match no_effect_note(key) {
            Some(note) => {
                if !notes.contains(&note) {
                    notes.push(note);
                }
            }
            None => equivalents.push(key.as_str()),
        }
    }
    notes.sort_by_key(|note| {
        NO_EFFECT_RETIRED_NOTES
            .iter()
            .chain(REMOVED_DATA_LAYER_NOTES.iter())
            .position(|(_, known)| known == note)
    });
    let mut parts = Vec::new();
    if !equivalents.is_empty() {
        parts.push(with_equivalents(&equivalents.join(", ")));
        if has_enabled_false_note(translation) {
            parts.push(RETIRED_ENABLED_FALSE_INDEXES_NOTE.to_string());
        }
        if still_in_file
            && equivalents
                .iter()
                .any(|key| names_path(key, RETIRED_IDLE_CHECKOUT_HOURS.0))
        {
            parts.push(retired_idle_checkout_hours_note());
        }
    }
    parts.extend(notes.into_iter().map(str::to_string));
    parts.join(" ")
}

/// Notice for a project config file whose retired keys were translated in
/// memory. The file itself is never written: it is shared through the repository. Mirrors the
/// TypeScript `legacyConfigNoticeMessage`.
pub fn project_retired_keys_notice(path: &str, translation: &DocumentTranslation) -> String {
    retired_keys_notice(translation, true, |keys| {
        format!(
            "{path} uses retired keys ({keys}); AFT applied their current equivalents, with the same limits a project config has for those keys. Run `npx @cortexkit/aft doctor --fix` to update the file."
        )
    })
}

/// Notice for the user config file after AFT rewrote its retired keys. It
/// names only the keys the rewrite replaced. Removed data-layer keys remain
/// in the file for the loader to refuse until an explicit doctor fix removes them.
pub fn user_config_migrated_notice(
    path: &str,
    backup: &str,
    translation: &DocumentTranslation,
) -> String {
    retired_keys_notice(translation, false, |keys| {
        format!(
            "AFT updated {path}: its retired keys ({keys}) now use their current equivalents. The previous file is saved as {backup}."
        )
    })
}

/// Notice for the user config file when AFT could not rewrite it and applied
/// the current equivalents in memory instead.
pub fn user_config_not_migrated_notice(
    path: &str,
    reason: &str,
    translation: &DocumentTranslation,
) -> String {
    retired_keys_notice(translation, true, |keys| {
        format!(
            "{path} uses retired keys ({keys}); AFT applied their current equivalents but could not update the file ({reason}). Run `npx @cortexkit/aft doctor --fix` to update it."
        )
    })
}

/// Sorted, de-duplicated disabled list.
pub fn normalize_tool_list<I, S>(names: I) -> Vec<String>
where
    I: IntoIterator<Item = S>,
    S: Into<String>,
{
    names
        .into_iter()
        .map(Into::into)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

/// Distinct unknown names in a resolved disabled list, sorted.
pub fn unknown_disabled_tools(disabled: &[String]) -> Vec<String> {
    normalize_tool_list(disabled.iter().filter(|name| !is_known_tool(name)).cloned())
}

/// Validate a resolved configuration object before anything consumes it.
///
/// A resolved configuration must carry the registration choice and every index
/// switch explicitly: a missing field must never be silently replaced by a
/// serde or historical default. Missing containers are reported instead of
/// their children. Errors are sorted by path.
pub fn validate_resolved_config(value: &Value) -> Result<(), Vec<String>> {
    let mut errors = Vec::new();
    let Some(object) = value.as_object() else {
        return Err(vec!["invalid_resolved_config:type:$".to_string()]);
    };
    match object.get("disabled_tools") {
        None => errors.push("invalid_resolved_config:missing:disabled_tools".to_string()),
        Some(Value::Array(entries)) if entries.iter().all(Value::is_string) => {}
        Some(_) => errors.push("invalid_resolved_config:type:disabled_tools".to_string()),
    }
    match object.get("indexes") {
        None => errors.push("invalid_resolved_config:missing:indexes".to_string()),
        Some(Value::Object(indexes)) => {
            for leaf in ["callgraph", "semantic", "trigram"] {
                match indexes.get(leaf) {
                    None => errors.push(format!("invalid_resolved_config:missing:indexes.{leaf}")),
                    Some(Value::Bool(_)) => {}
                    Some(_) => errors.push(format!("invalid_resolved_config:type:indexes.{leaf}")),
                }
            }
        }
        Some(_) => errors.push("invalid_resolved_config:type:indexes".to_string()),
    }
    errors.sort_by(|left, right| {
        let path = |error: &String| error.rsplit(':').next().unwrap_or_default().to_string();
        path(left).cmp(&path(right))
    });
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors)
    }
}

/// Explicit absent marker used by the notice projection.
fn absent() -> Value {
    let mut marker = Map::new();
    marker.insert("absent".to_string(), Value::Bool(true));
    Value::Object(marker)
}

fn project_block(block: &Map<String, Value>) -> Value {
    let mut projected = Map::new();

    let mut legacy_paths = Map::new();
    // Retired keys with no replacement are projected too: otherwise adding one
    // to a file whose notice was already acknowledged would keep the old
    // digest and the saved record would hide the new notice. The data-layer
    // keys are projected for the same reason.
    for path in RETIRED_PATHS
        .into_iter()
        .chain(REMOVED_NO_EFFECT_PATHS)
        .map(|(path, _)| path)
        .chain(DATA_LAYER_RETIRED_PATHS)
        .chain(REMOVED_DATA_LAYER_PATHS)
    {
        let value = match path.split_once('.') {
            Some((container, leaf)) => block
                .get(container)
                .and_then(Value::as_object)
                .and_then(|inner| inner.get(leaf)),
            None => block.get(path),
        };
        if let Some(value) = value {
            legacy_paths.insert(path.to_string(), value.clone());
        }
    }
    projected.insert("legacy_paths".to_string(), Value::Object(legacy_paths));

    let mut gates = Map::new();
    if let Some(value) = block
        .get("backup")
        .and_then(Value::as_object)
        .and_then(|backup| backup.get("enabled"))
    {
        gates.insert("backup.enabled".to_string(), value.clone());
    }
    if let Some(value) = block
        .get("inspect")
        .and_then(Value::as_object)
        .and_then(|inspect| inspect.get("enabled"))
    {
        gates.insert("inspect.enabled".to_string(), value.clone());
    }
    match block.get("bash") {
        Some(Value::Bool(value)) => {
            gates.insert("bash".to_string(), Value::Bool(*value));
        }
        Some(Value::Object(bash)) => {
            if let Some(value) = bash.get("enabled") {
                gates.insert("bash.enabled".to_string(), value.clone());
            }
        }
        _ => {}
    }
    projected.insert("runtime_gates".to_string(), Value::Object(gates));

    let (aliases, disabled) = match block.get("disabled_tools") {
        Some(Value::Array(entries)) => {
            let names: Vec<String> = entries
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect();
            let aliases = normalize_tool_list(
                names
                    .iter()
                    .filter(|name| legacy_tool_alias(name).is_some())
                    .cloned(),
            );
            (
                aliases,
                Value::Array(
                    normalize_tool_list(names)
                        .into_iter()
                        .map(Value::String)
                        .collect(),
                ),
            )
        }
        Some(other) => (Vec::new(), other.clone()),
        None => (Vec::new(), absent()),
    };
    projected.insert(
        "legacy_disabled_entries".to_string(),
        Value::Array(aliases.into_iter().map(Value::String).collect()),
    );
    projected.insert("disabled_tools".to_string(), disabled);

    let mut github = Map::new();
    for leaf in ["read", "shim", "write"] {
        let value = block
            .get("github")
            .and_then(Value::as_object)
            .and_then(|github| github.get(leaf))
            .cloned()
            .unwrap_or_else(absent);
        github.insert(leaf.to_string(), value);
    }
    projected.insert("github".to_string(), Value::Object(github));

    let mut indexes = Map::new();
    for (leaf, legacy, experimental) in INDEX_INPUTS {
        let mut inputs = Map::new();
        inputs.insert(
            "canonical".to_string(),
            block
                .get("indexes")
                .and_then(Value::as_object)
                .and_then(|indexes| indexes.get(leaf))
                .cloned()
                .unwrap_or_else(absent),
        );
        inputs.insert(
            legacy.to_string(),
            block.get(legacy).cloned().unwrap_or_else(absent),
        );
        if let Some(experimental) = experimental {
            inputs.insert(
                experimental.to_string(),
                block.get(experimental).cloned().unwrap_or_else(absent),
            );
        }
        indexes.insert(leaf.to_string(), Value::Object(inputs));
    }
    projected.insert("indexes".to_string(), Value::Object(indexes));
    Value::Object(projected)
}

/// Build the `notice_projection_v1` object for one raw (untranslated) config
/// document. Only inputs that influence translation are projected, so
/// comments, formatting, key order and unrelated settings never change it.
pub fn notice_projection(raw: Option<&Map<String, Value>>) -> Value {
    let mut blocks = Map::new();
    if let Some(raw) = raw {
        blocks.insert("base".to_string(), project_block(raw));
        if let Some(Value::Object(harnesses)) = raw.get("harnesses") {
            for (name, block) in harnesses {
                if let Value::Object(block) = block {
                    blocks.insert(format!("harnesses.{name}"), project_block(block));
                }
            }
        }
    }
    let mut projection = Map::new();
    projection.insert(
        "projection".to_string(),
        Value::String("notice_projection_v1".to_string()),
    );
    projection.insert("file_absent".to_string(), Value::Bool(raw.is_none()));
    projection.insert("blocks".to_string(), Value::Object(blocks));
    Value::Object(projection)
}

/// Compact JSON with object keys sorted at every level.
pub fn canonical_json(value: &Value) -> String {
    match value {
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            let body = keys
                .into_iter()
                .map(|key| {
                    format!(
                        "{}:{}",
                        serde_json::to_string(key).unwrap_or_default(),
                        canonical_json(&map[key])
                    )
                })
                .collect::<Vec<_>>()
                .join(",");
            format!("{{{body}}}")
        }
        Value::Array(items) => format!(
            "[{}]",
            items
                .iter()
                .map(canonical_json)
                .collect::<Vec<_>>()
                .join(",")
        ),
        other => serde_json::to_string(other).unwrap_or_default(),
    }
}

/// SHA-256 hex digest of the canonical projection.
pub fn notice_digest(projection: &Value) -> String {
    let digest = Sha256::digest(canonical_json(projection).as_bytes());
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn translate(doc: Value) -> (Value, DocumentTranslation) {
        let Value::Object(mut map) = doc else {
            panic!("object")
        };
        let out = translate_document(&mut map, DocumentTier::User);
        (Value::Object(map), out)
    }

    fn translate_project(doc: Value) -> Value {
        let Value::Object(mut map) = doc else {
            panic!("object")
        };
        translate_document(&mut map, DocumentTier::Project);
        Value::Object(map)
    }

    #[test]
    fn removed_config_key_errors_match_shared_rust_typescript_fixtures() {
        let fixture: Value = serde_json::from_str(include_str!(
            "../tests/fixtures/feature_config/removed_config_key_errors.json"
        ))
        .unwrap();
        for case in fixture["cases"].as_array().unwrap() {
            let doc = case["doc"].as_object().unwrap();
            // A case may name the release that judges it; the data-layer
            // keys are refused only from DATA_LAYER_KEYS_REFUSED_FROM.
            let errors = match case.get("release").and_then(Value::as_str) {
                Some(release) => removed_config_key_errors_at(doc, parse_release(release).unwrap()),
                None => removed_config_key_errors(doc),
            };
            let expected = case["errors"]
                .as_array()
                .unwrap()
                .iter()
                .map(|error| error.as_str().unwrap())
                .collect::<Vec<_>>();
            assert_eq!(errors, expected, "fixture {}", case["name"]);
        }
    }

    #[test]
    fn legacy_enabled_false_warns_that_indexes_still_build() {
        let (doc, out) = translate(json!({"enabled": false}));
        assert!(
            doc.get("indexes").is_none(),
            "indexes stay at their defaults"
        );
        let warning = out
            .warnings
            .iter()
            .find(|warning| warning.code == "legacy_enabled_false_indexes_still_build")
            .expect("enabled:false notice");
        assert_eq!(warning.key, "enabled");
        assert!(warning.message.contains("indexes still build"));
        assert!(warning.message.contains("indexes.trigram"));
    }

    /// A project block contributes only what its own legacy keys imply; the
    /// move/delete default belongs to the user base alone.
    #[test]
    fn project_base_blocks_never_receive_the_default_disables() {
        assert_eq!(
            translate_project(json!({"hoist_builtin_tools": false}))["disabled_tools"],
            json!([
                "apply_patch",
                "bash",
                "edit",
                "glob",
                "grep",
                "read",
                "write"
            ])
        );
        assert_eq!(translate_project(json!({"tool_surface": "all"})), json!({}));
        assert_eq!(
            translate_project(json!({"tool_surface": "recommended"}))["disabled_tools"],
            json!(["aft_callgraph", "aft_delete", "aft_move"])
        );
    }

    /// A false runtime gate only switches its behaviour off: it never removes
    /// a registration, alone or beside a retired key, and the translation
    /// warns `legacy_runtime_gate_runtime_only` so the user knows to list the
    /// tool in `disabled_tools`.
    #[test]
    fn false_runtime_gates_never_generate_disables() {
        for doc in [
            json!({"backup": {"enabled": false}}),
            json!({"inspect": {"enabled": false}}),
            json!({"bash": false}),
            json!({"bash": {"enabled": false}}),
        ] {
            let (value, out) = translate(doc.clone());
            assert!(value.get("disabled_tools").is_none(), "{doc}");
            assert!(!out.legacy_input, "{doc}");
            assert!(
                out.warnings
                    .iter()
                    .any(|warning| warning.code == "legacy_runtime_gate_runtime_only"),
                "{doc}"
            );
            assert!(translate_project(doc.clone())
                .get("disabled_tools")
                .is_none());
        }
        let (value, _) = translate(json!({"bash": false, "hoist_builtin_tools": false}));
        assert_eq!(
            value["disabled_tools"],
            json!([
                "aft_delete",
                "aft_move",
                "apply_patch",
                "bash",
                "edit",
                "glob",
                "grep",
                "read",
                "write"
            ]),
            "only the retired hoist key generates disables"
        );
    }

    #[test]
    fn policy_constants_match_the_shared_artifact() {
        let path = std::path::Path::new(crate::test_support::manifest_dir())
            .join("../../spec/feature-config/migration-policy.json");
        let Ok(text) = std::fs::read_to_string(path) else {
            return;
        };
        let artifact: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(artifact["id"], MIGRATION_POLICY_ID);
        assert_eq!(artifact["introduced_minor"], "0.58");
        assert!(
            artifact.get("reject_from_minor").is_none(),
            "retired keys are never rejected"
        );
        for (path, replacement) in RETIRED_PATHS {
            assert_eq!(artifact["paths"][path], replacement, "{path}");
        }
        for (alias, canonical) in LEGACY_TOOL_ALIASES {
            assert_eq!(artifact["disabled_list_names"][alias], canonical);
        }
    }

    #[test]
    fn surfaces_translate_to_literal_sets() {
        let (all, _) = translate(json!({"tool_surface": "all"}));
        assert_eq!(all, json!({"disabled_tools": []}));
        let (recommended, _) = translate(json!({"tool_surface": "recommended"}));
        assert_eq!(
            recommended["disabled_tools"],
            json!(["aft_callgraph", "aft_delete", "aft_move"])
        );
        let (minimal, _) = translate(json!({"tool_surface": "minimal"}));
        assert_eq!(minimal["disabled_tools"].as_array().unwrap().len(), 20);
    }

    #[test]
    fn retired_base_key_unions_with_default_and_explicit_empty_wins() {
        let (hoist, out) = translate(json!({"hoist_builtin_tools": false}));
        assert_eq!(
            hoist["disabled_tools"],
            json!([
                "aft_delete",
                "aft_move",
                "apply_patch",
                "bash",
                "edit",
                "glob",
                "grep",
                "read",
                "write"
            ])
        );
        assert!(out.legacy_input);
        assert_eq!(out.retired_keys, vec!["hoist_builtin_tools"]);
        let (explicit, _) = translate(json!({"hoist_builtin_tools": false, "disabled_tools": []}));
        assert_eq!(explicit["disabled_tools"], json!([]));
    }

    /// Every retired key translates; none is ever refused, and the translation
    /// names each one (harness keys with their block prefix).
    #[test]
    fn retired_keys_translate_and_are_listed() {
        let (value, out) = translate(json!({
            "disabled_tools": ["aft_glob"],
            "search_index": true,
            "harnesses": {"pi": {"semantic_search": false}}
        }));
        assert_eq!(
            value,
            json!({
                "disabled_tools": ["glob"],
                "indexes": {"trigram": true},
                "harnesses": {"pi": {"indexes": {"semantic": false}}}
            })
        );
        assert!(out.legacy_input);
        assert_eq!(
            out.retired_keys,
            vec!["aft_glob", "harnesses.pi.semantic_search", "search_index"]
        );
    }

    #[test]
    fn index_precedence_is_canonical_then_legacy_then_experimental() {
        let (value, _) =
            translate(json!({"experimental_search_index": false, "search_index": true}));
        assert_eq!(value, json!({"indexes": {"trigram": true}}));
        let (value, out) =
            translate(json!({"indexes": {"semantic": false}, "semantic_search": true}));
        assert_eq!(value, json!({"indexes": {"semantic": false}}));
        assert_eq!(out.warnings[0].code, "superseded_legacy_config");
    }

    /// The GitHub enable aliases translate to the canonical leaves with the
    /// same precedence `doctor --fix` uses: canonical leaf, then a
    /// `github.enabled: false` in the same block, then the alias.
    #[test]
    fn retired_github_aliases_translate_like_doctor_fix() {
        let (value, out) = translate(json!({
            "gh_read": {"enabled": true},
            "gh_shim": {"enabled": false, "binary_path": "/opt/aft"}
        }));
        assert_eq!(
            value,
            json!({
                "github": {"read": true, "shim": false},
                "gh_shim": {"binary_path": "/opt/aft"}
            })
        );
        assert_eq!(out.retired_keys, vec!["gh_read", "gh_shim.enabled"]);

        let (value, out) =
            translate(json!({"gh_read": {"enabled": true}, "github": {"read": false}}));
        assert_eq!(value, json!({"github": {"read": false}}));
        assert_eq!(out.warnings[0].code, "superseded_legacy_config");

        let (value, _) = translate(json!({
            "harnesses": {"pi": {"gh_shim": {"enabled": true}, "github": {"enabled": false}}}
        }));
        assert_eq!(
            value,
            json!({"harnesses": {"pi": {"github": {"read": false, "write": false, "shim": false}}}})
        );

        let (value, out) = translate(json!({"gh_shim": {"binary_path": "/opt/aft"}}));
        assert_eq!(value, json!({"gh_shim": {"binary_path": "/opt/aft"}}));
        assert!(!out.legacy_input);
    }

    #[test]
    fn retired_inspect_and_lsp_keys_translate_like_doctor_fix() {
        let (value, out) = translate(json!({
            "idle": {"lsp_ttl_minutes": 3, "root_ttl_minutes": 20},
            "inspect": {"tier2_soft_deadline_ms": 50, "max_drill_down_items": 20}
        }));
        assert_eq!(
            value,
            json!({"idle": {"root_ttl_minutes": 20}, "lsp": {"idle_minutes": 5}})
        );
        assert_eq!(
            out.retired_keys,
            vec![
                "idle.lsp_ttl_minutes",
                "inspect.max_drill_down_items",
                "inspect.tier2_soft_deadline_ms"
            ]
        );
        let (value, _) = translate(json!({"idle": {"lsp_ttl_minutes": "x"}}));
        assert_eq!(value, json!({"lsp": {"idle_minutes": 60}}));
        // A whole number written as a float is that number, as JavaScript
        // (and so the TypeScript translation) reads it; anything else is the
        // default.
        for (text, minutes) in [
            ("12.0", 12),
            ("12", 12),
            ("1e3", 1000),
            ("12.5", 60),
            ("1e20", 60),
            ("9007199254740993", 60),
        ] {
            let doc: Value =
                serde_json::from_str(&format!(r#"{{"idle": {{"lsp_ttl_minutes": {text}}}}}"#))
                    .unwrap();
            let (value, _) = translate(doc);
            assert_eq!(value, json!({"lsp": {"idle_minutes": minutes}}), "{text}");
        }
        let (value, out) = translate(json!({
            "idle": {"lsp_ttl_minutes": 10},
            "lsp": {"idle_minutes": "never"}
        }));
        assert_eq!(value, json!({"lsp": {"idle_minutes": "never"}}));
        assert_eq!(out.warnings[0].code, "superseded_legacy_config");
    }

    #[test]
    fn github_master_false_fills_absent_leaves() {
        let (value, _) = translate(json!({"github": {"enabled": false, "shim": true}}));
        assert_eq!(
            value,
            json!({"github": {"shim": true, "read": false, "write": false}})
        );
    }

    #[test]
    fn notices_name_the_file_and_the_retired_keys() {
        let (_, out) = translate(json!({"search_index": false, "hoist_builtin_tools": true}));
        let project = project_retired_keys_notice("/repo/.cortexkit/aft.jsonc", &out);
        assert!(project.starts_with(
            "/repo/.cortexkit/aft.jsonc uses retired keys (hoist_builtin_tools, search_index); AFT applied their current equivalents"
        ));
        assert!(project.contains("npx @cortexkit/aft doctor --fix"));
        let migrated = user_config_migrated_notice("/u/aft.jsonc", "/u/aft.jsonc.bak-1", &out);
        assert!(migrated.contains("/u/aft.jsonc.bak-1"));
        let (_, out) = translate(json!({"enabled": false}));
        assert!(
            user_config_not_migrated_notice("/u/aft.jsonc", "read-only", &out)
                .ends_with(RETIRED_ENABLED_FALSE_INDEXES_NOTE)
        );
    }

    #[test]
    fn retired_hang_threshold_is_dropped_and_the_notice_says_it_does_nothing() {
        let (value, out) = translate(json!({
            "bridge": {"hang_threshold": 3, "request_timeout_ms": 45000},
            "harnesses": {"pi": {"bridge": {"hang_threshold": 5}}}
        }));
        assert_eq!(
            value,
            json!({"bridge": {"request_timeout_ms": 45000}, "harnesses": {"pi": {}}})
        );
        assert!(out.legacy_input);
        assert_eq!(
            out.retired_keys,
            vec![
                "bridge.hang_threshold",
                "harnesses.pi.bridge.hang_threshold"
            ]
        );
        // Nothing is applied for a key that no longer does anything, so a
        // file whose only retired keys are such keys gets just their sentence.
        for notice in [
            project_retired_keys_notice("/repo/.cortexkit/aft.jsonc", &out),
            user_config_migrated_notice("/u/aft.jsonc", "/u/aft.jsonc.bak-1", &out),
            user_config_not_migrated_notice("/u/aft.jsonc", "read-only", &out),
        ] {
            assert_eq!(notice, RETIRED_HANG_THRESHOLD_NOTE);
        }
        let (_, out) = translate(json!({
            "inspect": {"tier2_soft_deadline_ms": 50, "max_drill_down_items": 20}
        }));
        assert_eq!(
            project_retired_keys_notice("/p", &out),
            "inspect.max_drill_down_items never had an effect, so the setting can be removed. inspect.tier2_soft_deadline_ms never had an effect, so the setting can be removed."
        );

        // Mixed with keys that have equivalents: the generic sentence names
        // only those keys, and the no-effect sentence follows.
        let (_, out) = translate(json!({
            "search_index": false,
            "bridge": {"hang_threshold": 3},
            "harnesses": {"pi": {"bridge": {"hang_threshold": 5}}}
        }));
        assert_eq!(
            project_retired_keys_notice("/repo/.cortexkit/aft.jsonc", &out),
            format!("/repo/.cortexkit/aft.jsonc uses retired keys (search_index); AFT applied their current equivalents, with the same limits a project config has for those keys. Run `npx @cortexkit/aft doctor --fix` to update the file. {RETIRED_HANG_THRESHOLD_NOTE}")
        );
        assert_eq!(
            user_config_migrated_notice("/u/aft.jsonc", "/u/aft.jsonc.bak-1", &out),
            format!("AFT updated /u/aft.jsonc: its retired keys (search_index) now use their current equivalents. The previous file is saved as /u/aft.jsonc.bak-1. {RETIRED_HANG_THRESHOLD_NOTE}")
        );
        assert_eq!(
            user_config_not_migrated_notice("/u/aft.jsonc", "read-only", &out),
            format!("/u/aft.jsonc uses retired keys (search_index); AFT applied their current equivalents but could not update the file (read-only). Run `npx @cortexkit/aft doctor --fix` to update it. {RETIRED_HANG_THRESHOLD_NOTE}")
        );
        let (_, out) = translate(json!({"enabled": false, "bridge": {"hang_threshold": 3}}));
        assert!(project_retired_keys_notice("/p", &out).ends_with(&format!(
            "{RETIRED_ENABLED_FALSE_INDEXES_NOTE} {RETIRED_HANG_THRESHOLD_NOTE}"
        )));

        // Only the retired leaf goes; an emptied bridge block goes with it.
        let (value, _) = translate(json!({"bridge": {"hang_threshold": 2}}));
        assert_eq!(value, json!({}));
        let (_, out) = translate(json!({"search_index": false}));
        assert!(!project_retired_keys_notice("/p", &out).contains("hang_threshold"));
    }
    #[test]
    fn resolved_validation_reports_sorted_paths_and_containers_only() {
        assert_eq!(
            validate_resolved_config(&json!({})).unwrap_err(),
            vec![
                "invalid_resolved_config:missing:disabled_tools",
                "invalid_resolved_config:missing:indexes"
            ]
        );
        assert_eq!(
            validate_resolved_config(&json!({
                "disabled_tools": "x",
                "indexes": {"trigram": 1, "semantic": true}
            }))
            .unwrap_err(),
            vec![
                "invalid_resolved_config:type:disabled_tools",
                "invalid_resolved_config:missing:indexes.callgraph",
                "invalid_resolved_config:type:indexes.trigram"
            ]
        );
        assert!(validate_resolved_config(&json!({
            "disabled_tools": [],
            "indexes": {"trigram": true, "semantic": false, "callgraph": true}
        }))
        .is_ok());
    }

    #[test]
    fn adding_a_no_effect_key_changes_the_notice_identity_and_nothing_else_does() {
        let digest = |doc: Value| notice_digest(&notice_projection(doc.as_object()));
        let acknowledged = digest(json!({"search_index": false}));
        // Nothing a notice depends on changed: same identity, no repeat.
        assert_eq!(acknowledged, digest(json!({"search_index": false})));
        assert_eq!(
            acknowledged,
            digest(json!({"search_index": false, "bridge": {"request_timeout_ms": 45000}}))
        );
        // Adding the retired key, in the base or a harness block, is a new
        // identity, so a saved record for the old one cannot hide it.
        let added = json!({"search_index": false, "bridge": {"hang_threshold": 3}});
        assert_ne!(acknowledged, digest(added.clone()));
        assert_ne!(
            acknowledged,
            digest(
                json!({"search_index": false, "harnesses": {"pi": {"bridge": {"hang_threshold": 5}}}})
            )
        );
        // The notice for the new identity names it.
        let Value::Object(mut map) = added else {
            panic!("object")
        };
        let out = translate_document(&mut map, DocumentTier::Project);
        assert_eq!(
            project_retired_keys_notice("/p", &out),
            format!("/p uses retired keys (search_index); AFT applied their current equivalents, with the same limits a project config has for those keys. Run `npx @cortexkit/aft doctor --fix` to update the file. {RETIRED_HANG_THRESHOLD_NOTE}")
        );
    }

    #[test]
    fn notice_digests_match_the_shared_fixtures() {
        let path = std::path::Path::new(crate::test_support::manifest_dir())
            .join("tests/fixtures/feature_config/notice_projection.json");
        let fixtures: Value =
            serde_json::from_str(&std::fs::read_to_string(path).expect("fixture")).unwrap();
        for case in fixtures["cases"].as_array().unwrap() {
            let doc = case.get("doc").and_then(Value::as_object);
            let projection = notice_projection(doc);
            assert_eq!(
                notice_digest(&projection),
                case["digest"].as_str().unwrap(),
                "{}",
                case["name"]
            );
        }
    }

    #[test]
    fn release_parsing_reads_major_and_minor_only() {
        assert_eq!(parse_release("0.65.0"), Some((0, 65)));
        assert_eq!(parse_release("0.64.12-beta.1"), Some((0, 64)));
        assert_eq!(parse_release("1.2"), Some((1, 2)));
        assert_eq!(parse_release("latest"), None);
        assert_eq!(
            running_release(),
            parse_release(env!("CARGO_PKG_VERSION")).unwrap()
        );
    }

    /// The idle-checkout rename keeps its existing version-gated policy.
    #[test]
    fn each_data_layer_key_is_refused_only_from_the_refusing_release() {
        assert_eq!(DATA_LAYER_KEYS_REFUSED_FROM, (0, 65));
        for path in DATA_LAYER_RETIRED_PATHS {
            let (container, leaf) = path.split_once('.').unwrap();
            let base = json!({ container: { leaf: 1 } });
            let harness = json!({ "harnesses": { "pi": { container: { leaf: 1 } } } });
            for doc in [&base, &harness] {
                let doc = doc.as_object().unwrap();
                assert!(
                    removed_config_key_errors_at(doc, (0, 64)).is_empty(),
                    "{path}"
                );
            }
            assert_eq!(
                removed_config_key_errors_at(base.as_object().unwrap(), (0, 65)),
                vec![format!(
                    "removed_config_key:{path}; run npx @cortexkit/aft doctor --fix"
                )]
            );
            assert_eq!(
                removed_config_key_errors_at(harness.as_object().unwrap(), (0, 65)),
                vec![format!(
                    "removed_config_key:harnesses.pi.{path}; run npx @cortexkit/aft doctor --fix"
                )]
            );
            assert_eq!(
                removed_config_key_errors_at(base.as_object().unwrap(), (1, 0)).len(),
                1,
                "{path}"
            );
            for doc in [&base, &harness] {
                assert!(removed_errors(doc.as_object().unwrap(), (0, 65), false).is_empty());
            }
        }
    }

    #[test]
    fn removed_data_layer_keys_refuse_only_user_files_including_harness_blocks() {
        for path in REMOVED_DATA_LAYER_PATHS {
            let (container, leaf) = path.split_once('.').unwrap();
            for (doc, prefix) in [
                (json!({container: {leaf: false}}), ""),
                (
                    json!({"harnesses": {"pi": {container: {leaf: false}}}}),
                    "harnesses.pi.",
                ),
            ] {
                let map = doc.as_object().unwrap();
                assert_eq!(
                    removed_errors(map, (0, 65), true),
                    vec![format!(
                        "removed_config_key:{prefix}{path}; run npx @cortexkit/aft doctor --fix"
                    )]
                );
                assert!(removed_errors(map, (0, 65), false).is_empty(), "{path}");
            }
        }
        assert!(removed_errors(
            json!({"inspect": {"tier2_idle_minutes": 5}})
                .as_object()
                .unwrap(),
            (0, 65),
            true
        )
        .is_empty());
    }

    /// `views.idle_checkout_hours` moves to `idle.checkout_hours`, clamped to
    /// its range, unless that is already set; it is listed as a retired key
    /// and so triggers the user file's automatic rewrite.
    #[test]
    fn retired_views_idle_checkout_hours_translates_like_doctor_fix() {
        let (value, out) = translate(json!({
            "views": {"idle_checkout_hours": 48},
            "idle": {"root_ttl_minutes": 20},
            "harnesses": {"pi": {"views": {"idle_checkout_hours": 0, "enabled": true}}}
        }));
        assert_eq!(
            value,
            json!({
                "idle": {"root_ttl_minutes": 20, "checkout_hours": 48},
                "harnesses": {"pi": {"idle": {"checkout_hours": 1}}}
            })
        );
        assert!(out.legacy_input);
        assert_eq!(
            out.retired_keys,
            vec![
                "harnesses.pi.views.enabled",
                "harnesses.pi.views.idle_checkout_hours",
                "views.idle_checkout_hours"
            ]
        );

        for (raw, hours) in [
            (json!(9000), 8760),
            (json!(12.0), 12),
            (json!(12.5), 168),
            (json!("never"), 168),
        ] {
            assert_eq!(retired_idle_checkout_hours(&raw), hours, "{raw}");
        }

        let (value, out) = translate(json!({
            "views": {"idle_checkout_hours": 2},
            "idle": {"checkout_hours": 5}
        }));
        assert_eq!(value, json!({"idle": {"checkout_hours": 5}}));
        assert_eq!(out.warnings[0].code, "superseded_legacy_config");
        assert_eq!(
            out.warnings[0].message,
            "views.idle_checkout_hours is ignored because idle.checkout_hours is set"
        );
    }

    /// Project translation removes the obsolete settings before strict parsing.
    #[test]
    fn removed_data_layer_keys_are_dropped_and_listed_as_retired() {
        let (value, out) = translate(json!({
            "views": {"enabled": true, "callgraph_copies": 2},
            "worktree": {"ram_overlay": false},
            "harnesses": {"opencode": {"worktree": {"ram_overlay": true}}}
        }));
        assert_eq!(value, json!({"harnesses": {"opencode": {}}}));
        assert!(out.legacy_input);
        assert_eq!(
            out.retired_keys,
            vec![
                "harnesses.opencode.worktree.ram_overlay",
                "views.callgraph_copies",
                "views.enabled",
                "worktree.ram_overlay"
            ]
        );
    }

    #[test]
    fn data_layer_project_notices_reuse_only_existing_no_effect_sentences() {
        let (_, removed) = translate(json!({
            "views": {"enabled": true, "callgraph_copies": 2},
            "worktree": {"ram_overlay": false}
        }));
        assert_eq!(project_retired_keys_notice("/p", &removed),
            "views.callgraph_copies is gone because no checkout borrows another checkout's call graph any more. views.enabled is gone because AFT now keeps one index store per repository and always uses it. worktree.ram_overlay is gone because every worktree now has its own index state, so there is no borrowed index to patch in memory.");
        let (_, one) = translate(json!({"harnesses": {"pi": {"worktree": {"ram_overlay": true}}}}));
        assert_eq!(
            project_retired_keys_notice("/p", &one),
            REMOVED_DATA_LAYER_NOTES[2].1
        );
        assert_eq!(
            project_retired_keys_notice("/p", &DocumentTranslation::default()),
            ""
        );
        let (_, both) = translate(json!({"views": {"idle_checkout_hours": 24, "enabled": true}}));
        assert_eq!(project_retired_keys_notice("/p", &both),
            format!("/p uses retired keys (views.idle_checkout_hours); AFT applied their current equivalents, with the same limits a project config has for those keys. Run `npx @cortexkit/aft doctor --fix` to update the file. {} {}", retired_idle_checkout_hours_note(), REMOVED_DATA_LAYER_NOTES[1].1));
    }

    /// Adding a data-layer key to a file whose notice was acknowledged is a
    /// new notice identity, so the new notice is not hidden.
    #[test]
    fn adding_a_data_layer_key_changes_the_notice_identity() {
        let digest = |doc: Value| notice_digest(&notice_projection(doc.as_object()));
        let acknowledged = digest(json!({"search_index": false}));
        for path in DATA_LAYER_RETIRED_PATHS {
            let (container, leaf) = path.split_once('.').unwrap();
            assert_ne!(
                acknowledged,
                digest(json!({"search_index": false, container: { leaf: 1 }})),
                "{path}"
            );
        }
    }
}
