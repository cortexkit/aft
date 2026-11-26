//! The retired-key configuration migration behind `aft doctor --fix` and the
//! automatic rewrite of the user config file.
//!
//! Ordinary loading refuses the names retired in 0.62 or earlier
//! ([`feature_config::REFUSED_RETIRED_PATHS`] and the prefixed `aft_*` host
//! names) with `removed_config_key`, and translates the more recently retired
//! `bridge.hang_threshold` and `views.idle_checkout_hours` in memory. This
//! module rewrites a config file so that it states the same intent with
//! canonical keys, whichever of those groups its retired names belong to, and
//! splices the changes into the original text so comments, formatting and
//! unrelated keys survive.
//! `doctor --fix` runs it for the user and project files on request, and also
//! deletes the removed data-layer keys (`views.enabled`,
//! `views.callgraph_copies`, `worktree.ram_overlay`);
//! [`auto_migrate_user_config`] runs it for the user file whenever AFT loads
//! that file, but leaves the removed data-layer keys in place. A project file
//! is shared through its repository and is never rewritten automatically.
//!
//! Every block is repaired: the base object and each embedded
//! `harnesses.<id>` object. A project file only ever gains disables for
//! unprotected tools; protected slots (`aft_safety` and the seven host tool
//! names) are dropped from what it materializes and reported, matching what
//! the resolver would ignore anyway.

use std::fs::{self, Permissions, TryLockError};
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde::Serialize;
use serde_json::{Map, Value};

use crate::feature_config::{self, INDEX_INPUTS, RETIRED_PATHS};
use crate::fs_lock::AdvisoryLockGuard;
use crate::jsonc_edit;

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

struct Document(String);

impl Document {
    fn value(&self) -> Result<Value, String> {
        parse_document(&self.0).map_err(|error| format!("parse document at /: {error}"))
    }

    fn set(&mut self, path: &[&str], value: &Value) -> Result<(), String> {
        let pointer = jsonc_edit::pointer(path);
        let current = self.value()?;
        let old = current.pointer(&pointer);
        if old == Some(value) {
            return Ok(());
        }
        if let (Some(Value::Array(old)), Value::Array(new)) = (old, value) {
            for (i, (old, new)) in old.iter().zip(new).enumerate() {
                if old != new {
                    subc_jsonc::set_element(&mut self.0, path, i, new)
                        .map_err(|error| format!("set_element at {pointer}/{i}: {error}"))?;
                }
            }
            for i in (new.len()..old.len()).rev() {
                subc_jsonc::remove_element(&mut self.0, path, i)
                    .map_err(|error| format!("remove_element at {pointer}/{i}: {error}"))?;
            }
            for element in new.iter().skip(old.len()) {
                subc_jsonc::push_element(&mut self.0, path, element)
                    .map_err(|error| format!("push_element at {pointer}: {error}"))?;
            }
        } else {
            subc_jsonc::set_member(&mut self.0, path, value)
                .map_err(|error| format!("set_member at {pointer}: {error}"))?;
        }
        // The editor addresses the first occurrence of a duplicate key; the
        // resolver uses the last. Refuse an edit that didn't reach its value.
        if self.value()?.pointer(&pointer) != Some(value) {
            return Err(format!("verify set at {pointer}: edited value is shadowed"));
        }
        Ok(())
    }

    fn remove(&mut self, path: &[&str]) -> Result<(), String> {
        let pointer = jsonc_edit::pointer(path);
        subc_jsonc::remove_member(&mut self.0, path)
            .map_err(|error| format!("remove_member at {pointer}: {error}"))?;
        if self.value()?.pointer(&pointer).is_some() {
            return Err(format!(
                "verify removal at {pointer}: a duplicate key remains"
            ));
        }
        Ok(())
    }

    fn remove_if_empty_object(&mut self, path: &[&str]) -> Result<(), String> {
        if self
            .value()?
            .pointer(&jsonc_edit::pointer(path))
            .and_then(Value::as_object)
            .is_some_and(Map::is_empty)
        {
            self.remove(path)?;
        }
        Ok(())
    }
}

fn parse_document(text: &str) -> Result<Value, String> {
    let strict = subc_jsonc::jsonc_to_json(text)?;
    if strict.trim().is_empty() {
        return Ok(Value::Object(Map::new()));
    }
    let value: Value = serde_json::from_str(&strict).map_err(|error| error.to_string())?;
    if !value.is_object() {
        return Err("the root value must be an object".to_string());
    }
    Ok(value)
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
    doc: &mut Document,
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
    Ok(())
}

/// Whether a rewrite also deletes the removed data-layer keys
/// ([`feature_config::REMOVED_DATA_LAYER_PATHS`]). `doctor --fix` deletes
/// them; the automatic rewrite keeps them so loading can refuse them until
/// the user explicitly runs `doctor --fix`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RemovedKeys {
    Delete,
    Keep,
}

/// Rewrite one block's retired keys to the canonical values ordinary loading
/// derives from them ([`feature_config::translate_document`]).
fn migrate_block(
    doc: &mut Document,
    prefix: &[String],
    raw: &Map<String, Value>,
    translated: &Map<String, Value>,
    tier: FixTier,
    removed_keys: RemovedKeys,
    notes: &mut Vec<String>,
) -> Result<(), String> {
    let label = block_label(prefix);
    let mut emptied = Vec::new();
    remove_retired_members(doc, prefix, raw, removed_keys, &mut emptied)?;
    let document_tier = if tier == FixTier::User {
        feature_config::DocumentTier::User
    } else {
        feature_config::DocumentTier::Project
    };
    let mut edits = feature_config::settings_shape::edits(raw, document_tier, prefix.is_empty());
    // Read replacements from the original values, then remove the old names
    // before writing their replacements into the staged text.
    edits.sort_by_key(|edit| edit.value.is_some());
    for edit in edits {
        let path: Vec<_> = prefix
            .iter()
            .chain(&edit.path)
            .map(String::as_str)
            .collect();
        match edit.value {
            Some(value) => doc.set(&path, &value)?,
            None => {
                doc.remove(&path)?;
            }
        }
    }
    if let Some(profile) = translated
        .get("sandbox")
        .and_then(|sandbox| sandbox.get("profile"))
    {
        if raw
            .get("sandbox")
            .and_then(|sandbox| sandbox.get("profile"))
            != Some(profile)
        {
            doc.set(&path_of(prefix, &["sandbox", "profile"]), profile)?;
        }
    }

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

    if let Some(value) = raw
        .get("views")
        .and_then(|views| views.get("idle_checkout_hours"))
    {
        if raw
            .get("idle")
            .and_then(|idle| idle.get("checkout_hours"))
            .is_none()
        {
            let hours = feature_config::retired_idle_checkout_hours(value);
            doc.set(
                &path_of(prefix, &["idle", "checkout_hours"]),
                &Value::from(hours),
            )?;
        }
    }

    if tier == FixTier::User && get_bool(raw, "bash", "runon_enabled") == Some(false) {
        doc.set(
            &path_of(prefix, &["remote_exec", "enabled"]),
            &Value::Bool(false),
        )?;
    }

    for container in emptied {
        doc.remove_if_empty_object(&path_of(prefix, &[container]))?;
    }
    Ok(())
}

fn remove_retired_members(
    doc: &mut Document,
    prefix: &[String],
    raw: &Map<String, Value>,
    removed_keys: RemovedKeys,
    emptied: &mut Vec<&'static str>,
) -> Result<(), String> {
    let removed_paths: &[&str] = match removed_keys {
        RemovedKeys::Delete => &feature_config::REMOVED_DATA_LAYER_PATHS,
        RemovedKeys::Keep => &[],
    };
    for (path, _) in RETIRED_PATHS
        .into_iter()
        .chain(feature_config::REMOVED_INSPECT_LSP_PATHS)
        .chain(feature_config::REMOVED_NO_EFFECT_PATHS)
        .chain(feature_config::REMOVED_OPENCODE_PATHS)
        .chain([feature_config::RETIRED_IDLE_CHECKOUT_HOURS])
        .chain([("bash.runon_enabled", "remote_exec.enabled")])
        .chain(removed_paths.iter().map(|path| (*path, "")))
        .chain(
            (removed_keys == RemovedKeys::Delete)
                .then_some((feature_config::CALLGRAPH_CHUNK_KEY, "")),
        )
    {
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
            // An empty bash object keeps its precedence over retired
            // experimental bash defaults after the old remote gate is gone.
            if segments.len() == 2 && path != "bash.runon_enabled" {
                emptied.push(segments[0]);
            }
        }
    }
    Ok(())
}

/// Migrate one config file's text the way `doctor --fix` does: retired keys
/// become their current equivalents and the removed data-layer keys are
/// deleted. `changed` is false (and `text` identical) when the file needs no
/// migration.
pub fn migrate_config_text(text: &str, tier: FixTier) -> Result<Migration, String> {
    migrate_config_text_with(text, tier, RemovedKeys::Delete)
}

/// The text the automatic rewrite of the user file
/// ([`auto_migrate_user_config`]) would leave: retired keys become their
/// current equivalents and the removed data-layer keys stay. Nothing is
/// written.
pub fn migrate_user_config_text(text: &str) -> Result<Migration, String> {
    migrate_config_text_with(text, FixTier::User, RemovedKeys::Keep)
}

fn migrate_config_text_with(
    text: &str,
    tier: FixTier,
    removed_keys: RemovedKeys,
) -> Result<Migration, String> {
    let mut doc = Document(text.to_string());
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
            removed_keys,
            &mut notes,
        )?;
    }

    let changed = doc.0 != text;
    Ok(Migration {
        text: doc.0,
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
/// A legacy project file is repaired in place when the shared file is absent;
/// explicit fix never relocates it or creates a shared project file.
pub fn fix_targets(user_config_path: Option<&Path>, cwd: &Path) -> Vec<(PathBuf, FixTier)> {
    let mut targets = Vec::new();
    if let Some(user) = user_config_path.filter(|path| path.is_file()) {
        targets.push((user.to_path_buf(), FixTier::User));
    }
    let project = crate::subc_config::project_config_read_path(cwd, None);
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
            let result = crate::config::open_config_file(path)
                .and_then(|file| std::io::read_to_string(file))
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
    /// Loading translates the recently retired ones in memory and refuses the
    /// names retired in 0.62 or earlier.
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

/// How long a loader waits for another process that is migrating the same
/// file before it gives up and reads the file as it is.
const MIGRATION_LOCK_WAIT: Duration = Duration::from_secs(2);
const MIGRATION_LOCK_RETRY: Duration = Duration::from_millis(25);

/// The retired-key translation of a config text, when it parses as an object
/// and uses at least one retired key.
fn retired_key_translation(text: &str) -> Option<feature_config::DocumentTranslation> {
    let value = parse_document(text).ok()?;
    let Value::Object(mut map) = value else {
        return None;
    };
    let mut translation =
        feature_config::translate_document(&mut map, feature_config::DocumentTier::User);
    // The automatic rewrite leaves these keys untouched. Its notice must name
    // only the keys it actually rewrites, not claim refused keys were migrated.
    translation.retired_keys.retain(|key| {
        !feature_config::REMOVED_DATA_LAYER_PATHS.iter().any(|path| {
            key == path
                || key
                    .strip_suffix(path)
                    .is_some_and(|prefix| prefix.ends_with('.'))
        })
    });
    translation.legacy_input = !translation.retired_keys.is_empty();
    translation.legacy_input.then_some(translation)
}

/// Whether loading refuses a config text for a retired or removed key
/// ([`feature_config::removed_config_key_errors`]).
fn names_refused_key(text: &str) -> bool {
    parse_document(text)
        .ok()
        .and_then(|value| {
            value
                .as_object()
                .map(|map| !feature_config::removed_config_key_errors(map).is_empty())
        })
        .unwrap_or(false)
}

enum LockAttempt {
    Acquired(AdvisoryLockGuard),
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
    let deadline = Instant::now() + MIGRATION_LOCK_WAIT;
    // Never unlink the lock file: waiters may already have it open, and a new
    // file at the same path would let two holders lock different files. The
    // OS releases the lock on exit; the file's age and contents don't matter.
    let file = loop {
        match fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
        {
            Ok(file) => break file,
            // Windows can deny access while a lock file's deletion is pending.
            // Retry within the deadline; a persistent denial still fails.
            Err(error)
                if cfg!(windows)
                    && error.kind() == io::ErrorKind::PermissionDenied
                    && Instant::now() < deadline =>
            {
                std::thread::sleep(MIGRATION_LOCK_RETRY);
            }
            Err(error) => return LockAttempt::Failed(error),
        }
    };
    #[cfg(test)]
    tests::migration_lock_hook(tests::LockStep::BeforeAttempt);
    loop {
        match file.try_lock() {
            Ok(()) => return LockAttempt::Acquired(AdvisoryLockGuard::from_locked_file(file)),
            Err(TryLockError::WouldBlock) => {
                #[cfg(test)]
                tests::migration_lock_hook(tests::LockStep::Contended);
                if Instant::now() >= deadline {
                    return LockAttempt::Busy;
                }
                std::thread::sleep(MIGRATION_LOCK_RETRY);
            }
            Err(TryLockError::Error(error)) => return LockAttempt::Failed(error),
        }
    }
}

/// The old bytes may contain credentials, even if the settings file is public.
fn write_backup(dir: &Path, text: &str) -> io::Result<PathBuf> {
    crate::production_storage::refuse_write(dir)?;
    crate::private_storage::create_dir_all(dir)?;
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos())
        .unwrap_or_default();
    let mut backup = tempfile::Builder::new()
        .prefix(&format!("aft.jsonc.bak-{stamp}-"))
        .tempfile_in(dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        backup
            .as_file()
            .set_permissions(Permissions::from_mode(0o600))?;
    }
    backup.write_all(text.as_bytes())?;
    crate::durability::sync_file(backup.as_file(), backup.path())?;
    crate::durability::sync_dir(dir)?;
    backup
        .keep()
        .map(|(_, path)| path)
        .map_err(|error| error.error)
}

fn publish_conversion(
    path: &Path,
    contents: &str,
    permissions: Permissions,
    relocating: bool,
) -> io::Result<()> {
    let parent = path
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let mut temp = tempfile::Builder::new()
        .prefix(".aft.jsonc.tmp-")
        .tempfile_in(parent)?;
    temp.write_all(contents.as_bytes())?;
    temp.as_file().set_permissions(permissions)?;
    crate::durability::sync_file(temp.as_file(), temp.path())?;
    // Complete fallible preparation before the rename commits new settings.
    crate::durability::sync_dir(parent)?;
    if relocating {
        // The desktop may have created the canonical settings since we checked.
        temp.persist_noclobber(path)
    } else {
        temp.persist(path)
    }
    .map(|_| ())
    .map_err(|error| error.error)
}

/// Rewrite retired user keys, retaining the old bytes in AFT's data folder.
pub fn auto_migrate_user_config(path: &Path) -> Option<UserConfigMigration> {
    auto_migrate_user_config_bounded(path, crate::subc_config::CONFIG_READ_LIMIT)
        .ok()
        .flatten()
}

pub(crate) fn auto_migrate_user_config_bounded(
    path: &Path,
    limit: usize,
) -> io::Result<Option<UserConfigMigration>> {
    if crate::subc_config::is_legacy_user_config_path(path) {
        // If startup couldn't relocate the legacy file, read it as-is. An
        // in-place rewrite would change it without reaching the canonical path.
        return Ok(None);
    }
    migrate_user_config_to(path, path, limit, |path| fs::remove_file(path))
}

pub(crate) fn migrate_user_config_to(
    source: &Path,
    destination: &Path,
    limit: usize,
    remove_legacy: impl FnOnce(&Path) -> io::Result<()>,
) -> io::Result<Option<UserConfigMigration>> {
    let mut failure = None;
    let result = auto_migrate_user_config_with_reader(
        source,
        destination,
        migration_backup_dir,
        publish_conversion,
        |path| {
            let read = (|| {
                let file = crate::config::open_config_file(path)?;
                crate::subc_config::read_bounded_config(file, limit)
            })();
            if let Err(error) = &read {
                failure = Some(io::Error::new(error.kind(), error.to_string()));
            }
            read
        },
        remove_legacy,
    );
    match failure {
        Some(error) => Err(error),
        None => Ok(result),
    }
}

fn migration_backup_dir() -> PathBuf {
    #[cfg(test)]
    if let Some(path) = TEST_BACKUP_DIR.with(|slot| slot.borrow().clone()) {
        return path;
    }
    crate::storage_path::storage_dir(None)
}

#[cfg(test)]
thread_local! {
    static TEST_BACKUP_DIR: std::cell::RefCell<Option<PathBuf>> = const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
pub(crate) fn with_backup_dir_for_test<T>(path: &Path, run: impl FnOnce() -> T) -> T {
    struct Restore(Option<PathBuf>);
    impl Drop for Restore {
        fn drop(&mut self) {
            TEST_BACKUP_DIR.with(|slot| slot.replace(self.0.take()));
        }
    }
    let _restore = Restore(TEST_BACKUP_DIR.with(|slot| slot.replace(Some(path.to_path_buf()))));
    run()
}

fn auto_migrate_user_config_with_reader(
    source: &Path,
    destination: &Path,
    backup_dir: impl FnOnce() -> PathBuf,
    write: impl FnOnce(&Path, &str, Permissions, bool) -> io::Result<()>,
    mut read: impl FnMut(&Path) -> io::Result<String>,
    remove_legacy: impl FnOnce(&Path) -> io::Result<()>,
) -> Option<UserConfigMigration> {
    let relocating = source != destination;
    let text = read(source).ok()?;
    let translation = retired_key_translation(&text);
    if translation.is_none() && !relocating {
        return None;
    }
    let not_migrated =
        |reason: String, translation: Option<&feature_config::DocumentTranslation>, text: &str| {
            // Loading refuses a file that still names a key retired in 0.62 or
            // earlier, and that refusal names the key and `doctor --fix`. A notice
            // saying AFT applied the current equivalents would be wrong for it.
            let notice = translation
                .filter(|_| !names_refused_key(text))
                .map_or_else(String::new, |translation| {
                    feature_config::user_config_not_migrated_notice(
                        &source.display().to_string(),
                        &reason,
                        translation,
                    )
                });
            if notice.is_empty() {
                crate::slog_warn!("config user {}: {reason}", source.display());
                return None;
            }
            Some(UserConfigMigration::NotMigrated {
                path: source.to_path_buf(),
                notice,
                reason,
            })
        };

    let actual = match fs::canonicalize(source) {
        Ok(path) => path,
        Err(error) => {
            return not_migrated(
                format!("resolve source at {}: {error}", source.display()),
                translation.as_ref(),
                &text,
            )
        }
    };
    let target = if relocating {
        destination
    } else {
        actual.as_path()
    };
    if actual.ends_with(Path::new(".cortexkit").join("aft.jsonc")) {
        return not_migrated(
            format!(
                "refused to replace a project settings file at {}",
                actual.display()
            ),
            translation.as_ref(),
            &text,
        );
    }
    if let Some(reason) = crate::production_storage::config_write_refusal(&actual)
        .or_else(|| crate::production_storage::config_write_refusal(target))
    {
        return not_migrated(reason, translation.as_ref(), &text);
    }
    let permissions = match fs::metadata(&actual) {
        Ok(metadata) if metadata.permissions().readonly() => {
            return not_migrated(
                "the file is read-only".to_string(),
                translation.as_ref(),
                &text,
            );
        }
        Ok(metadata) => metadata.permissions(),
        Err(error) => {
            return not_migrated(
                format!("inspect source at {}: {error}", actual.display()),
                translation.as_ref(),
                &text,
            )
        }
    };
    let _lock = match acquire_migration_lock(&actual) {
        LockAttempt::Acquired(lock) => lock,
        // Another loader is rewriting the file and will report it.
        LockAttempt::Busy => return None,
        LockAttempt::Failed(error) => {
            return not_migrated(
                format!("lock source at {}: {error}", actual.display()),
                translation.as_ref(),
                &text,
            );
        }
    };

    if relocating
        && !matches!(fs::symlink_metadata(target), Err(error) if error.kind() == io::ErrorKind::NotFound)
    {
        return None;
    }
    // Re-read under the lock: another process may have finished meanwhile.
    let text = match read(&actual) {
        Ok(text) => text,
        Err(error) => {
            return not_migrated(
                format!("read source at {}: {error}", actual.display()),
                translation.as_ref(),
                &text,
            )
        }
    };
    let translation = retired_key_translation(&text);
    if !relocating && translation.is_none() {
        return None;
    }
    let migration = match migrate_user_config_text(&text) {
        Ok(migration) if migration.changed || relocating => migration,
        Ok(_) => return None,
        Err(error) => return not_migrated(error, translation.as_ref(), &text),
    };
    let backup_dir = backup_dir();
    let backup = match write_backup(&backup_dir, &text) {
        Ok(backup) => backup,
        Err(error) => {
            return not_migrated(
                format!("write old copy at {}: {error}", backup_dir.display()),
                translation.as_ref(),
                &text,
            );
        }
    };
    let parent = target
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let publish = || {
        if relocating {
            fs::create_dir_all(parent)?;
        }
        write(target, &migration.text, permissions, relocating)
    };
    if let Err(error) = publish() {
        let cleanup = fs::remove_file(&backup);
        let mut reason = format!("publish settings at {}: {error}", target.display());
        if let Err(error) = cleanup {
            reason.push_str(&format!("; could not remove {}: {error}", backup.display()));
        }
        return not_migrated(reason, translation.as_ref(), &text);
    }
    // The rename has replaced the file, but its directory entry isn't durable
    // yet. Keep the backup if syncing fails, and don't remove a legacy source.
    if let Err(error) = crate::durability::sync_dir(parent) {
        return not_migrated(
            format!("publish settings at {}: {error}", target.display()),
            translation.as_ref(),
            &text,
        );
    }
    if relocating {
        if let Err(error) = remove_legacy(source) {
            crate::slog_warn!("config user: could not finish removing legacy settings {} after publishing {}: {error}", source.display(), target.display());
        }
    }
    let notice = translation
        .as_ref()
        .map_or_else(String::new, |translation| {
            feature_config::user_config_migrated_notice(
                &destination.display().to_string(),
                &backup.display().to_string(),
                translation,
            )
        });
    Some(UserConfigMigration::Migrated {
        path: destination.to_path_buf(),
        notice,
        backup,
    })
}

#[cfg(test)]
#[path = "config_fix_tests.rs"]
mod tests;
