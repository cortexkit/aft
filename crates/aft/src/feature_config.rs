//! Feature-based configuration policy shared by the resolver, the configure
//! handler and (later) the setup/doctor CLI.
//!
//! Configuration used to pick a tool surface level (`tool_surface`), a hoisting
//! mode (`hoist_builtin_tools`) and a set of index booleans. It now has one
//! registration rule — a tool is registered unless its canonical name is in
//! `disabled_tools` — plus three first-class index switches under `indexes`.
//! This module owns the literal tool inventory, the one-release migration
//! policy for the retired keys, the per-block translation of those keys into
//! canonical ones, validation of a resolved configuration object and the
//! migration-notice projection digest. The TypeScript plugins carry a mirror of
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
/// First minor release that ships the translation window.
pub const POLICY_INTRODUCED_MINOR: (u64, u64) = (0, 58);
/// First minor release that rejects the retired keys instead of translating.
pub const POLICY_REJECT_FROM_MINOR: (u64, u64) = (0, 59);

/// Retired top-level (or nested) config paths and the replacement named in
/// their rejection diagnostic.
pub const RETIRED_PATHS: [(&str, &str); 9] = [
    ("tool_surface", "disabled_tools"),
    ("hoist_builtin_tools", "disabled_tools"),
    ("enabled", "disabled_tools"),
    ("search_index", "indexes.trigram"),
    ("experimental_search_index", "indexes.trigram"),
    ("semantic_search", "indexes.semantic"),
    ("experimental_semantic_search", "indexes.semantic"),
    ("callgraph_store", "indexes.callgraph"),
    ("github.enabled", "github.read,github.write,github.shim"),
];

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

/// Whether the retired keys translate (inside the window) or reject.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PolicyPhase {
    Window,
    Rejecting,
}

/// Parse `major.minor[.patch...]`; patch and pre-release parts are ignored.
fn parse_minor(version: &str) -> Option<(u64, u64)> {
    let mut parts = version.trim().trim_start_matches('v').split('.');
    let major = parts.next()?.parse().ok()?;
    let minor_part = parts.next()?;
    let minor_digits: String = minor_part
        .chars()
        .take_while(char::is_ascii_digit)
        .collect();
    Some((major, minor_digits.parse().ok()?))
}

/// Policy phase for a package version (major/minor comparison only).
pub fn policy_phase_for_version(version: &str) -> PolicyPhase {
    match parse_minor(version) {
        Some(minor) if minor >= POLICY_REJECT_FROM_MINOR => PolicyPhase::Rejecting,
        _ => PolicyPhase::Window,
    }
}

/// Policy phase of the running binary.
pub fn current_policy_phase() -> PolicyPhase {
    policy_phase_for_version(env!("CARGO_PKG_VERSION"))
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
    /// Rejection diagnostics; any entry aborts the whole candidate load.
    pub errors: Vec<String>,
    pub warnings: Vec<TranslationWarning>,
    /// True when any retired key or alias was supplied (a migration notice applies).
    pub legacy_input: bool,
}

fn removed(old: &str, replacement: &str) -> String {
    format!("removed_config_key:{old}:use:{replacement}")
}

/// Reject the already-retired GitHub enable aliases. These are errors at every
/// version; only `doctor --fix` may repair them.
fn reject_retired_github_aliases(map: &Map<String, Value>, errors: &mut Vec<String>) {
    if map.contains_key("gh_read") {
        errors.push(removed("gh_read", "github.read"));
    }
    if let Some(Value::Object(shim)) = map.get("gh_shim") {
        if shim.contains_key("enabled") {
            errors.push(removed("gh_shim", "github.shim"));
        }
    }
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

/// Translate or reject the retired keys of one tier/harness block in place.
/// What a retired `enabled: false` no longer does. The translation only hides
/// tools; indexing continues, so a user who relied on it to keep AFT out of a
/// repository must also switch the indexes off. Mirrors the TypeScript
/// `RETIRED_ENABLED_FALSE_INDEXES_NOTE`.
pub const RETIRED_ENABLED_FALSE_INDEXES_NOTE: &str = "enabled: false no longer turns AFT off: it is translated to disabling every tool, but the trigram, semantic and callgraph indexes still build. To keep AFT from indexing this repository, also set indexes.trigram, indexes.semantic and indexes.callgraph to false.";

fn translate_block(
    map: &mut Map<String, Value>,
    is_base: bool,
    phase: PolicyPhase,
    block_label: &str,
    out: &mut DocumentTranslation,
) {
    reject_retired_github_aliases(map, &mut out.errors);

    // Canonicalize (or reject) historical prefixed names inside disabled lists.
    let mut explicit_list = None;
    if let Some(Value::Array(entries)) = map.get("disabled_tools") {
        if entries.iter().all(Value::is_string) {
            let mut canonical = Vec::with_capacity(entries.len());
            for name in entries.iter().filter_map(Value::as_str) {
                match legacy_tool_alias(name) {
                    Some(host) => {
                        out.legacy_input = true;
                        if phase == PolicyPhase::Rejecting {
                            out.errors.push(removed(name, host));
                        }
                        canonical.push(host.to_string());
                    }
                    None => canonical.push(name.to_string()),
                }
            }
            explicit_list = Some(canonical);
        }
    }

    let supplied_paths: Vec<&str> = RETIRED_PATHS
        .iter()
        .map(|(path, _)| *path)
        .filter(|path| has_path(map, path))
        .collect();
    if !supplied_paths.is_empty() {
        out.legacy_input = true;
    }
    let gates = false_runtime_gates(map);

    if phase == PolicyPhase::Rejecting {
        for path in &supplied_paths {
            let replacement = RETIRED_PATHS
                .iter()
                .find(|(old, _)| old == path)
                .map(|(_, replacement)| *replacement)
                .unwrap_or("disabled_tools");
            out.errors.push(removed(path, replacement));
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
        return;
    }

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
                    out.warnings.push(TranslationWarning {
                        code: "superseded_legacy_config",
                        key: key.to_string(),
                        message: format!(
                            "{key}={value} is ignored because indexes.{leaf}={canonical} is set"
                        ),
                    });
                }
            } else if let Some(previous) = chosen {
                if previous != value {
                    out.warnings.push(TranslationWarning {
                        code: "superseded_legacy_config",
                        key: key.to_string(),
                        message: format!(
                            "{key}={value} is ignored because {legacy}={previous} takes precedence"
                        ),
                    });
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
                for leaf in ["read", "write", "shim"] {
                    match github.get(leaf) {
                        None => {
                            github.insert(leaf.to_string(), Value::Bool(false));
                        }
                        Some(Value::Bool(true)) => out.warnings.push(TranslationWarning {
                            code: "superseded_legacy_config",
                            key: "github.enabled".to_string(),
                            message: format!(
                                "github.enabled=false is ignored for github.{leaf} because github.{leaf}=true is set"
                            ),
                        }),
                        Some(_) => {}
                    }
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
    for gate in &gates {
        match *gate {
            "backup.enabled" => {
                generated.insert("aft_safety".to_string());
            }
            "inspect.enabled" => {
                generated.insert("aft_inspect".to_string());
            }
            _ => generated.extend(BASH_GATE_DISABLES.iter().map(|name| (*name).to_string())),
        }
    }

    if explicit_list.is_some() {
        if explicit_surface || !generated.is_empty() {
            out.warnings.push(TranslationWarning {
                code: "superseded_legacy_config",
                key: block_label.to_string(),
                message: "disabled_tools is set explicitly, so legacy tool_surface/hoist_builtin_tools/enabled and runtime gates do not change registration".to_string(),
            });
        }
        return;
    }

    let list = if is_base {
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
    if !gates.is_empty() {
        out.warnings.push(TranslationWarning {
            code: "legacy_runtime_gate_requires_fix",
            key: block_label.to_string(),
            message: format!(
                "{} still removes tool registrations during this release only; run `aft doctor --fix` to record the choice in disabled_tools",
                gates.join(", ")
            ),
        });
    }
}

/// Translate or reject every block (base plus each `harnesses.<id>` object)
/// of one raw configuration document, in place.
pub fn translate_document(map: &mut Map<String, Value>, phase: PolicyPhase) -> DocumentTranslation {
    let mut out = DocumentTranslation::default();
    translate_block(map, true, phase, "base", &mut out);
    if let Some(Value::Object(harnesses)) = map.get_mut("harnesses") {
        for (name, block) in harnesses.iter_mut() {
            if let Value::Object(block) = block {
                translate_block(block, false, phase, &format!("harnesses.{name}"), &mut out);
            }
        }
    }
    out.errors.sort();
    out.errors.dedup();
    out
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
    for (path, _) in RETIRED_PATHS {
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

    #[test]
    fn legacy_enabled_false_warns_that_indexes_still_build() {
        let (doc, out) = translate(json!({"enabled": false}), PolicyPhase::Window);
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
    use super::*;
    use serde_json::json;

    fn translate(doc: Value, phase: PolicyPhase) -> (Value, DocumentTranslation) {
        let Value::Object(mut map) = doc else {
            panic!("object")
        };
        let out = translate_document(&mut map, phase);
        (Value::Object(map), out)
    }

    #[test]
    fn policy_versions_compare_major_minor_only() {
        assert_eq!(policy_phase_for_version("0.57.2"), PolicyPhase::Window);
        assert_eq!(policy_phase_for_version("0.58.9"), PolicyPhase::Window);
        assert_eq!(policy_phase_for_version("0.59.0"), PolicyPhase::Rejecting);
        assert_eq!(
            policy_phase_for_version("0.59.0-rc.1"),
            PolicyPhase::Rejecting
        );
        assert_eq!(policy_phase_for_version("1.0.0"), PolicyPhase::Rejecting);
    }

    #[test]
    fn policy_constants_match_the_shared_artifact() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../spec/feature-config/migration-policy.json");
        let Ok(text) = std::fs::read_to_string(path) else {
            return;
        };
        let artifact: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(artifact["id"], MIGRATION_POLICY_ID);
        assert_eq!(artifact["introduced_minor"], "0.58");
        assert_eq!(artifact["reject_from_minor"], "0.59");
        for (path, replacement) in RETIRED_PATHS {
            assert_eq!(artifact["paths"][path], replacement, "{path}");
        }
        for (alias, canonical) in LEGACY_TOOL_ALIASES {
            assert_eq!(artifact["disabled_list_names"][alias], canonical);
        }
        let text = text.to_string();
        assert!(!artifact["paths"]
            .as_object()
            .unwrap()
            .keys()
            .any(|key| key.starts_with("gh_")));
        assert!(!text.contains("\"gh_read\":"));
    }

    #[test]
    fn surfaces_translate_to_literal_sets() {
        let (all, _) = translate(json!({"tool_surface": "all"}), PolicyPhase::Window);
        assert_eq!(all, json!({"disabled_tools": []}));
        let (recommended, _) =
            translate(json!({"tool_surface": "recommended"}), PolicyPhase::Window);
        assert_eq!(
            recommended["disabled_tools"],
            json!(["aft_callgraph", "aft_delete", "aft_move"])
        );
        let (minimal, _) = translate(json!({"tool_surface": "minimal"}), PolicyPhase::Window);
        assert_eq!(minimal["disabled_tools"].as_array().unwrap().len(), 20);
    }

    #[test]
    fn gate_only_base_unions_with_default_and_explicit_empty_wins() {
        let (hoist, out) = translate(json!({"hoist_builtin_tools": false}), PolicyPhase::Window);
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
        let (backup, out) = translate(json!({"backup": {"enabled": false}}), PolicyPhase::Window);
        assert_eq!(
            backup["disabled_tools"],
            json!(["aft_delete", "aft_move", "aft_safety"])
        );
        assert!(out
            .warnings
            .iter()
            .any(|warning| warning.code == "legacy_runtime_gate_requires_fix"));
        let (explicit, _) = translate(
            json!({"backup": {"enabled": false}, "hoist_builtin_tools": false, "disabled_tools": []}),
            PolicyPhase::Window,
        );
        assert_eq!(explicit["disabled_tools"], json!([]));
    }

    #[test]
    fn aliases_translate_in_window_and_reject_after() {
        let (value, out) = translate(json!({"disabled_tools": ["aft_glob"]}), PolicyPhase::Window);
        assert_eq!(value["disabled_tools"], json!(["glob"]));
        assert!(out.errors.is_empty());
        let (_, out) = translate(
            json!({"disabled_tools": ["aft_glob"], "search_index": true}),
            PolicyPhase::Rejecting,
        );
        assert_eq!(
            out.errors,
            vec![
                "removed_config_key:aft_glob:use:glob".to_string(),
                "removed_config_key:search_index:use:indexes.trigram".to_string()
            ]
        );
    }

    #[test]
    fn index_precedence_is_canonical_then_legacy_then_experimental() {
        let (value, _) = translate(
            json!({"experimental_search_index": false, "search_index": true}),
            PolicyPhase::Window,
        );
        assert_eq!(value, json!({"indexes": {"trigram": true}}));
        let (value, out) = translate(
            json!({"indexes": {"semantic": false}, "semantic_search": true}),
            PolicyPhase::Window,
        );
        assert_eq!(value, json!({"indexes": {"semantic": false}}));
        assert_eq!(out.warnings[0].code, "superseded_legacy_config");
    }

    #[test]
    fn retired_github_aliases_reject_in_every_phase() {
        for phase in [PolicyPhase::Window, PolicyPhase::Rejecting] {
            let (_, out) = translate(
                json!({"gh_read": {"enabled": true}, "github": {"read": true}}),
                phase,
            );
            assert_eq!(
                out.errors,
                vec!["removed_config_key:gh_read:use:github.read"]
            );
            let (_, out) = translate(
                json!({"harnesses": {"pi": {"gh_shim": {"enabled": false}}}}),
                phase,
            );
            assert_eq!(
                out.errors,
                vec!["removed_config_key:gh_shim:use:github.shim"]
            );
        }
        let (_, out) = translate(
            json!({"gh_shim": {"binary_path": "/opt/aft"}}),
            PolicyPhase::Window,
        );
        assert!(out.errors.is_empty());
    }

    #[test]
    fn github_master_false_fills_absent_leaves() {
        let (value, _) = translate(
            json!({"github": {"enabled": false, "shim": true}}),
            PolicyPhase::Window,
        );
        assert_eq!(
            value,
            json!({"github": {"shim": true, "read": false, "write": false}})
        );
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
    fn notice_digests_match_the_shared_fixtures() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
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
}
