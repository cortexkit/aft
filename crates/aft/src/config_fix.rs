//! The `aft doctor --fix` configuration migration.
//!
//! Ordinary loading translates the keys retired by the switch to
//! `disabled_tools`/`indexes` (`tool_surface`, `hoist_builtin_tools`,
//! top-level `enabled`, `search_index`, `semantic_search`, `callgraph_store`,
//! `github.enabled` and the prefixed `aft_*` host names) during the migration
//! window ([`PolicyPhase::Window`], the releases that still accept them) and
//! rejects them afterwards; it rejects the already-retired GitHub enable
//! aliases (`gh_read`, `gh_shim.enabled`) at every version. This module is the
//! only reader allowed to repair them. It
//! rewrites each consumed config file so that it states the same intent with
//! canonical keys, and splices the changes into the original text so comments,
//! formatting and unrelated keys survive.
//!
//! Every block is repaired: the base object and each embedded
//! `harnesses.<id>` object. A project file only ever gains disables for
//! unprotected tools; protected slots (`aft_safety` and the seven host tool
//! names) are dropped from what it materializes and reported, matching what
//! the resolver would ignore anyway.

use std::path::{Path, PathBuf};

use serde::Serialize;
use serde_json::{Map, Value};

use crate::feature_config::{self, PolicyPhase, INDEX_INPUTS, RETIRED_PATHS};
use crate::jsonc_edit::{self, JsoncDocument};

/// Which trust tier a config file belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum FixTier {
    User,
    Project,
}

/// The rewritten text of one file and what the rewrite did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Migration {
    pub text: String,
    pub changed: bool,
    /// Human-readable notes: conflicts resolved by precedence and ignored
    /// protected project disables.
    pub notes: Vec<String>,
}

fn block_label(prefix: &[String]) -> String {
    if prefix.is_empty() {
        "base".to_string()
    } else {
        prefix.join(".")
    }
}

fn path_of<'a>(prefix: &'a [String], tail: &[&'a str]) -> Vec<&'a str> {
    prefix
        .iter()
        .map(String::as_str)
        .chain(tail.iter().copied())
        .collect()
}

fn lookup<'a>(value: &'a Value, prefix: &[String]) -> Option<&'a Map<String, Value>> {
    prefix
        .iter()
        .try_fold(value, |node, key| node.get(key))?
        .as_object()
}

fn get_bool(block: &Map<String, Value>, container: &str, leaf: &str) -> Option<bool> {
    block.get(container)?.as_object()?.get(leaf)?.as_bool()
}

/// Repair the already-retired GitHub enable aliases of one block. Precedence:
/// canonical leaf, then the value `github.enabled:false` generates, then the
/// alias. The aliases are removed; `gh_shim.binary_path` is kept.
fn repair_github_aliases(
    doc: &mut JsoncDocument,
    prefix: &[String],
    notes: &mut Vec<String>,
) -> Result<(), String> {
    let value = doc.value()?;
    let Some(block) = lookup(&value, prefix) else {
        return Ok(());
    };
    let label = block_label(prefix);
    let master_off = get_bool(block, "github", "enabled") == Some(false);
    let aliases = [
        (
            "gh_read",
            "read",
            block
                .get("gh_read")
                .and_then(|v| v.get("enabled"))
                .and_then(Value::as_bool),
        ),
        (
            "gh_shim",
            "shim",
            block
                .get("gh_shim")
                .and_then(|v| v.get("enabled"))
                .and_then(Value::as_bool),
        ),
    ];
    for (alias, leaf, alias_value) in aliases {
        let canonical = get_bool(block, "github", leaf);
        if let Some(alias_value) = alias_value {
            if let Some(canonical) = canonical {
                if canonical != alias_value {
                    notes.push(format!(
                        "{label}: {alias}.enabled={alias_value} ignored because github.{leaf}={canonical} is set"
                    ));
                }
            } else if master_off {
                if alias_value {
                    notes.push(format!(
                        "{label}: {alias}.enabled=true ignored because github.enabled=false switches github.{leaf} off"
                    ));
                }
            } else {
                doc.set(
                    &path_of(prefix, &["github", leaf]),
                    &Value::Bool(alias_value),
                )?;
            }
        }
    }
    if block.contains_key("gh_read") {
        doc.remove(&path_of(prefix, &["gh_read"]))?;
    }
    if block
        .get("gh_shim")
        .and_then(Value::as_object)
        .is_some_and(|shim| shim.contains_key("enabled"))
    {
        doc.remove(&path_of(prefix, &["gh_shim", "enabled"]))?;
        doc.remove_if_empty_object(&path_of(prefix, &["gh_shim"]))?;
    }
    Ok(())
}

/// Rewrite one block's retired keys to the canonical values ordinary loading
/// derives from them while it still accepts them ([`PolicyPhase::Window`]).
fn migrate_block(
    doc: &mut JsoncDocument,
    prefix: &[String],
    raw: &Map<String, Value>,
    translated: &Map<String, Value>,
    tier: FixTier,
    notes: &mut Vec<String>,
) -> Result<(), String> {
    let label = block_label(prefix);

    // Registration: materialize the translated list when it differs from
    // what the block spells out (aliases canonicalized, generated disables).
    if let Some(Value::Array(list)) = translated.get("disabled_tools") {
        let wanted: Vec<String> = list
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_string)
            .collect();
        let current: Option<Vec<String>> =
            raw.get("disabled_tools")
                .and_then(Value::as_array)
                .map(|entries| {
                    entries
                        .iter()
                        .filter_map(Value::as_str)
                        .map(str::to_string)
                        .collect()
                });
        if current.as_ref() != Some(&wanted) {
            let mut kept = Vec::new();
            for name in wanted {
                if tier == FixTier::Project && feature_config::is_project_protected_tool(&name) {
                    notes.push(format!(
                        "{label}: ignored protected disable {name}; a project config cannot disable it"
                    ));
                } else if !kept.contains(&name) {
                    kept.push(name);
                }
            }
            doc.set(
                &path_of(prefix, &["disabled_tools"]),
                &Value::Array(kept.into_iter().map(Value::String).collect()),
            )?;
        }
    }

    for (leaf, _, _) in INDEX_INPUTS {
        let raw_has = raw
            .get("indexes")
            .and_then(Value::as_object)
            .is_some_and(|indexes| indexes.contains_key(leaf));
        if let (false, Some(value)) = (raw_has, get_bool(translated, "indexes", leaf)) {
            doc.set(&path_of(prefix, &["indexes", leaf]), &Value::Bool(value))?;
        }
    }

    for leaf in ["read", "write", "shim"] {
        let raw_has = raw
            .get("github")
            .and_then(Value::as_object)
            .is_some_and(|github| github.contains_key(leaf));
        if let (false, Some(value)) = (raw_has, get_bool(translated, "github", leaf)) {
            doc.set(&path_of(prefix, &["github", leaf]), &Value::Bool(value))?;
        }
    }

    for (path, _) in RETIRED_PATHS {
        let segments: Vec<&str> = path.split('.').collect();
        let present = match segments.as_slice() {
            [key] => raw.contains_key(*key),
            [container, leaf] => raw
                .get(*container)
                .and_then(Value::as_object)
                .is_some_and(|inner| inner.contains_key(*leaf)),
            _ => false,
        };
        if present {
            doc.remove(&path_of(prefix, &segments))?;
            if segments.len() == 2 {
                doc.remove_if_empty_object(&path_of(prefix, &segments[..1]))?;
            }
        }
    }
    Ok(())
}

/// Migrate one config file's text. `changed` is false (and `text` identical)
/// when the file needs no migration.
pub fn migrate_config_text(text: &str, tier: FixTier) -> Result<Migration, String> {
    let mut doc = JsoncDocument::parse(text)?;
    let mut notes = Vec::new();

    let harness_names: Vec<String> = doc
        .value()?
        .get("harnesses")
        .and_then(Value::as_object)
        .map(|harnesses| {
            harnesses
                .iter()
                .filter(|(_, block)| block.is_object())
                .map(|(name, _)| name.clone())
                .collect()
        })
        .unwrap_or_default();
    let mut prefixes: Vec<Vec<String>> = vec![Vec::new()];
    prefixes.extend(
        harness_names
            .iter()
            .map(|name| vec!["harnesses".to_string(), name.clone()]),
    );

    for prefix in &prefixes {
        repair_github_aliases(&mut doc, prefix, &mut notes)?;
    }

    let raw = doc.value()?;
    let mut translated = raw.as_object().cloned().unwrap_or_default();
    let translation = feature_config::translate_document(&mut translated, PolicyPhase::Window);
    if !translation.errors.is_empty() {
        return Err(translation.errors.join("\n"));
    }
    let translated = Value::Object(translated);
    for prefix in &prefixes {
        let (Some(raw_block), Some(translated_block)) =
            (lookup(&raw, prefix), lookup(&translated, prefix))
        else {
            continue;
        };
        migrate_block(
            &mut doc,
            prefix,
            raw_block,
            translated_block,
            tier,
            &mut notes,
        )?;
    }

    let changed = doc.text() != text;
    Ok(Migration {
        text: doc.text().to_string(),
        changed,
        notes,
    })
}

/// Outcome for one file of a fix run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FixOutcome {
    pub path: PathBuf,
    pub tier: FixTier,
    /// `rewritten`, `unchanged` or `failed`.
    pub status: &'static str,
    pub notes: Vec<String>,
    pub error: Option<String>,
}

/// Files a fix run may touch for an invocation in `cwd`: the existing user
/// file and, when present, the project file at the invocation directory.
/// These are exactly the files ordinary loading consumes; legacy per-harness
/// locations are not loaded and so are not repaired.
pub fn fix_targets(user_config_path: Option<&Path>, cwd: &Path) -> Vec<(PathBuf, FixTier)> {
    let mut targets = Vec::new();
    if let Some(user) = user_config_path.filter(|path| path.is_file()) {
        targets.push((user.to_path_buf(), FixTier::User));
    }
    let project = crate::setup_plan::project_config_path(cwd);
    if project.is_file() {
        targets.push((project, FixTier::Project));
    }
    targets
}

/// Repair every target independently. A failure leaves that file unchanged
/// and does not stop the others.
pub fn fix_files(targets: &[(PathBuf, FixTier)]) -> Vec<FixOutcome> {
    targets
        .iter()
        .map(|(path, tier)| {
            let result = std::fs::read_to_string(path)
                .map_err(|error| format!("could not read: {error}"))
                .and_then(|text| {
                    let migration = migrate_config_text(&text, *tier)?;
                    if migration.changed {
                        jsonc_edit::write_atomic(path, &migration.text)
                            .map_err(|error| format!("could not write: {error}"))?;
                    }
                    Ok(migration)
                });
            match result {
                Ok(migration) => FixOutcome {
                    path: path.clone(),
                    tier: *tier,
                    status: if migration.changed {
                        "rewritten"
                    } else {
                        "unchanged"
                    },
                    notes: migration.notes,
                    error: None,
                },
                Err(error) => FixOutcome {
                    path: path.clone(),
                    tier: *tier,
                    status: "failed",
                    notes: Vec::new(),
                    error: Some(error),
                },
            }
        })
        .collect()
}

#[cfg(test)]
#[path = "config_fix_tests.rs"]
mod tests;
