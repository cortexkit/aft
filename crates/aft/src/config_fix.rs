//! The retired-key configuration migration behind `aft doctor --fix` and the
//! automatic rewrite of the user config file.
//!
//! Ordinary loading translates the retired keys (`tool_surface`,
//! `hoist_builtin_tools`, top-level `enabled`, `search_index`,
//! `semantic_search`, `callgraph_store`, `github.enabled`, the GitHub enable
//! aliases `gh_read`/`gh_shim.enabled`, the prefixed `aft_*` host names,
//! `idle.lsp_ttl_minutes` and the two removed inspect keys) in memory and never
//! refuses them. This module rewrites a config file so that it states the same
//! intent with canonical keys, and splices the changes into the original text
//! so comments, formatting and unrelated keys survive. `doctor --fix` runs it
//! for the user and project files on request; [`auto_migrate_user_config`]
//! runs it for the user file whenever AFT loads that file. A project file is
//! shared through its repository and is never rewritten automatically.
//!
//! Every block is repaired: the base object and each embedded
//! `harnesses.<id>` object. A project file only ever gains disables for
//! unprotected tools; protected slots (`aft_safety` and the seven host tool
//! names) are dropped from what it materializes and reported, matching what
//! the resolver would ignore anyway.

use std::path::{Path, PathBuf};

use serde::Serialize;
use serde_json::{Map, Value};

use crate::feature_config::{self, INDEX_INPUTS, RETIRED_PATHS};
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
/// derives from them ([`feature_config::translate_document`]).
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

    if !raw.contains_key("bash") {
        if let Some(value) = translated.get("bash") {
            doc.set(&path_of(prefix, &["bash"]), value)?;
        }
    }
    let raw_lsp_ty = raw
        .get("experimental")
        .and_then(|experimental| experimental.get("lsp_ty"));
    let translated_lsp_ty = translated
        .get("experimental")
        .and_then(|experimental| experimental.get("lsp_ty"));
    if let (None, Some(value)) = (raw_lsp_ty, translated_lsp_ty) {
        doc.set(&path_of(prefix, &["experimental", "lsp_ty"]), value)?;
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

    if let Some(value) = raw.get("idle").and_then(|idle| idle.get("lsp_ttl_minutes")) {
        if raw
            .get("lsp")
            .and_then(|lsp| lsp.get("idle_minutes"))
            .is_none()
        {
            let minutes = feature_config::retired_lsp_idle_minutes(value);
            doc.set(
                &path_of(prefix, &["lsp", "idle_minutes"]),
                &Value::from(minutes),
            )?;
        }
    }

    for (path, _) in RETIRED_PATHS
        .into_iter()
        .chain(feature_config::REMOVED_INSPECT_LSP_PATHS)
    {
        if path == "experimental.bash"
            && translated
                .get("experimental")
                .and_then(|experimental| experimental.get("bash"))
                .is_some()
        {
            continue;
        }
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
    let document_tier = match tier {
        FixTier::User => feature_config::DocumentTier::User,
        FixTier::Project => feature_config::DocumentTier::Project,
    };
    feature_config::translate_document(&mut translated, document_tier);
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

/// What the automatic migration of the user config file did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UserConfigMigration {
    /// The file was rewritten to current keys; `backup` holds its previous text.
    Migrated {
        path: PathBuf,
        backup: PathBuf,
        notice: String,
    },
    /// The file still uses retired keys because it could not be rewritten.
    /// Loading translates them in memory, so nothing is refused.
    NotMigrated {
        path: PathBuf,
        reason: String,
        notice: String,
    },
}

impl UserConfigMigration {
    /// The user-facing notice for this outcome.
    pub fn notice(&self) -> &str {
        match self {
            Self::Migrated { notice, .. } | Self::NotMigrated { notice, .. } => notice,
        }
    }
}

/// How long another process's migration lock is honoured before it is
/// treated as left behind by a crash and removed.
const MIGRATION_LOCK_STALE: std::time::Duration = std::time::Duration::from_secs(30);
/// How long a loader waits for another process that is migrating the same
/// file before it gives up and translates in memory.
const MIGRATION_LOCK_WAIT: std::time::Duration = std::time::Duration::from_secs(2);

/// The retired-key translation of a config text, when it parses as an object
/// and uses at least one retired key.
fn retired_key_translation(text: &str) -> Option<feature_config::DocumentTranslation> {
    let value = serde_json::from_str::<Value>(&crate::jsonc::strip_jsonc(text)).ok()?;
    let Value::Object(mut map) = value else {
        return None;
    };
    let translation =
        feature_config::translate_document(&mut map, feature_config::DocumentTier::User);
    translation.legacy_input.then_some(translation)
}

/// Exclusive, cross-process right to rewrite one config file, held as a lock
/// file next to it and removed on drop.
struct MigrationLock {
    path: PathBuf,
}

impl Drop for MigrationLock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

enum LockAttempt {
    Acquired(MigrationLock),
    /// Another process holds the lock and did not release it in time.
    Busy,
    Failed(std::io::Error),
}

fn acquire_migration_lock(target: &Path) -> LockAttempt {
    let name = target
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "aft.jsonc".to_string());
    let path = target.with_file_name(format!(".{name}.migrate.lock"));
    let deadline = std::time::Instant::now() + MIGRATION_LOCK_WAIT;
    loop {
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(_) => return LockAttempt::Acquired(MigrationLock { path }),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                let stale = std::fs::metadata(&path)
                    .and_then(|metadata| metadata.modified())
                    .ok()
                    .and_then(|modified| modified.elapsed().ok())
                    .is_some_and(|age| age > MIGRATION_LOCK_STALE);
                if stale {
                    let _ = std::fs::remove_file(&path);
                    continue;
                }
                if std::time::Instant::now() >= deadline {
                    return LockAttempt::Busy;
                }
                std::thread::sleep(std::time::Duration::from_millis(25));
            }
            Err(error) => {
                // Windows refuses to create a file whose previous holder is
                // still deleting it (a delete-pending name) with
                // ERROR_ACCESS_DENIED rather than AlreadyExists, so a
                // concurrent migration releasing the lock looks like a
                // permission error. Retry it like a held lock until the wait
                // ends; a real permission problem still fails after that.
                if cfg!(windows)
                    && error.kind() == std::io::ErrorKind::PermissionDenied
                    && std::time::Instant::now() < deadline
                {
                    std::thread::sleep(std::time::Duration::from_millis(25));
                    continue;
                }
                return LockAttempt::Failed(error);
            }
        }
    }
}

/// Write `text` to a new sibling backup file `<name>.bak-<unix seconds>`
/// (with a counter when that name is taken) carrying `target`'s permissions.
fn write_backup(target: &Path, text: &str) -> std::io::Result<PathBuf> {
    use std::io::Write as _;
    let name = target
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "aft.jsonc".to_string());
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or_default();
    let mut attempt = 0u32;
    loop {
        let candidate = if attempt == 0 {
            target.with_file_name(format!("{name}.bak-{stamp}"))
        } else {
            target.with_file_name(format!("{name}.bak-{stamp}-{attempt}"))
        };
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&candidate)
        {
            Ok(mut file) => {
                let written = (|| {
                    file.write_all(text.as_bytes())?;
                    file.sync_all()?;
                    if let Ok(metadata) = std::fs::metadata(target) {
                        std::fs::set_permissions(&candidate, metadata.permissions())?;
                    }
                    Ok(())
                })();
                return match written {
                    Ok(()) => Ok(candidate),
                    Err(error) => {
                        let _ = std::fs::remove_file(&candidate);
                        Err(error)
                    }
                };
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists && attempt < 100 => {
                attempt += 1;
            }
            Err(error) => return Err(error),
        }
    }
}

/// Rewrite the user config file at `path` to current keys when it uses
/// retired ones, with the same comment-preserving migration `doctor --fix`
/// performs.
///
/// Returns `None` when there is nothing to report: the file is missing,
/// unreadable, does not parse (ordinary loading reports that), uses no retired
/// key, or another process migrated it meanwhile. The rewrite is atomic (a
/// sibling temporary file renamed over the target, keeping its permissions),
/// keeps the previous text in a `<name>.bak-<unix seconds>` file beside it,
/// and is serialized across processes by a lock file, so only one of several
/// loaders rewrites the file and reports it. A symlinked file is rewritten at
/// its target so the link survives. A read-only file, a debug build pointed at
/// the account's own config directory (see
/// [`crate::production_storage::config_write_refusal`]), or any failure leaves
/// the file as it was and returns [`UserConfigMigration::NotMigrated`]; the
/// caller's in-memory translation still applies its current equivalents.
pub fn auto_migrate_user_config(path: &Path) -> Option<UserConfigMigration> {
    let text = std::fs::read_to_string(path).ok()?;
    let translation = retired_key_translation(&text)?;
    let display = path.display().to_string();
    let not_migrated = |reason: String, translation: &feature_config::DocumentTranslation| {
        Some(UserConfigMigration::NotMigrated {
            path: path.to_path_buf(),
            notice: feature_config::user_config_not_migrated_notice(&display, &reason, translation),
            reason,
        })
    };

    let target = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    if let Some(reason) = crate::production_storage::config_write_refusal(&target) {
        return not_migrated(reason, &translation);
    }
    match std::fs::metadata(&target) {
        Ok(metadata) if metadata.permissions().readonly() => {
            return not_migrated("the file is read-only".to_string(), &translation);
        }
        Ok(_) => {}
        Err(error) => return not_migrated(format!("could not inspect it: {error}"), &translation),
    }
    let _lock = match acquire_migration_lock(&target) {
        LockAttempt::Acquired(lock) => lock,
        // Another loader is rewriting the file and will report it.
        LockAttempt::Busy => return None,
        LockAttempt::Failed(error) => {
            return not_migrated(format!("could not lock it: {error}"), &translation);
        }
    };

    // Re-read under the lock: another process may have finished meanwhile.
    let text = match std::fs::read_to_string(&target) {
        Ok(text) => text,
        Err(error) => return not_migrated(format!("could not read it: {error}"), &translation),
    };
    let translation = retired_key_translation(&text)?;
    let migration = match migrate_config_text(&text, FixTier::User) {
        Ok(migration) if migration.changed => migration,
        Ok(_) => return None,
        Err(error) => return not_migrated(error, &translation),
    };
    let backup = match write_backup(&target, &text) {
        Ok(backup) => backup,
        Err(error) => {
            return not_migrated(format!("could not write a backup: {error}"), &translation);
        }
    };
    if let Err(error) = jsonc_edit::write_atomic(&target, &migration.text) {
        let _ = std::fs::remove_file(&backup);
        return not_migrated(format!("could not write it: {error}"), &translation);
    }
    Some(UserConfigMigration::Migrated {
        path: path.to_path_buf(),
        notice: feature_config::user_config_migrated_notice(
            &display,
            &backup.display().to_string(),
            &translation,
        ),
        backup,
    })
}

#[cfg(test)]
#[path = "config_fix_tests.rs"]
mod tests;
