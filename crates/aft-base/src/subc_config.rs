//! Subc-mode local config read (subc edge only).
//!
//! Config is read directly from the CortexKit user and project files: user
//! `~/.config/cortexkit/aft.jsonc` and project `<root>/.cortexkit/aft.jsonc`.
//! When the shared project file is absent, legacy per-harness project files
//! are read in place, never moved or rewritten.
//! There is NO wire-relayed config path, so a front (runner, `mcp:*`, or `fed:*`)
//! cannot push config over the connection. `config_resolve` then selects the
//! active bind's optional harness override from each file tier.
//!
//! Trust remains purely per-TIER after that selection: the user file is trusted
//! (the user's own disk), while privileged fields from the untrusted in-repo
//! project file, including its harness override, are dropped.

use crate::config_fix::UserConfigMigration;
use crate::config_resolve::ConfigTier;
use crate::harness::Harness;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};

mod approved;
mod bind_cache;
mod prepared;
pub use approved::ApprovedUser;
pub use bind_cache::BindConfigCache;
pub use prepared::{
    current_prepared_bind_config, read_bounded_config, with_prepared_bind_config,
    ConfigObservationError, PreparedBindConfig,
};

/// Legacy AFT config lookup, kept to find files written before AFT used the
/// daemon's shared config-home resolver.
pub fn user_config_path_from(
    xdg_config_home: Option<&OsStr>,
    home: Option<&OsStr>,
) -> Option<PathBuf> {
    let base = xdg_config_home
        .map(PathBuf::from)
        // An unset-but-empty `$XDG_CONFIG_HOME` ("") is not absolute → fall back
        // to `~/.config`, per the XDG Base Directory spec.
        .filter(|p| p.is_absolute())
        .or_else(|| home.map(|h| PathBuf::from(h).join(".config")))?;
    Some(base.join("cortexkit").join("aft.jsonc"))
}

fn user_config_paths() -> (PathBuf, Option<PathBuf>) {
    let canonical = PathBuf::from(cortexkit_store_types::resolve_config_home())
        .join("cortexkit")
        .join("aft.jsonc");
    let xdg = std::env::var_os("XDG_CONFIG_HOME");
    let home = crate::environment::non_empty_os_var("HOME")
        .or_else(|| crate::environment::non_empty_os_var("USERPROFILE"));
    let legacy =
        user_config_path_from(xdg.as_deref(), home.as_deref()).filter(|path| path != &canonical);
    (canonical, legacy)
}

fn missing(path: &Path) -> bool {
    matches!(std::fs::symlink_metadata(path), Err(error) if error.kind() == std::io::ErrorKind::NotFound)
}

fn selected_user_config_path(canonical: &Path, legacy: Option<&Path>) -> PathBuf {
    if let Some(legacy) = legacy.filter(|legacy| missing(canonical) && !missing(legacy)) {
        return legacy.to_path_buf();
    }
    canonical.to_path_buf()
}

/// Prefer the daemon's canonical path. Read a legacy path only while the
/// canonical file is absent, never when it is unreadable or invalid.
pub fn cortexkit_user_config_path() -> Option<PathBuf> {
    let (canonical, legacy) = user_config_paths();
    Some(selected_user_config_path(&canonical, legacy.as_deref()))
}

pub(crate) fn is_legacy_user_config_path(path: &Path) -> bool {
    user_config_paths().1.as_deref() == Some(path)
}

/// Try relocation once before startup passes the selected path to its readers.
pub fn startup_cortexkit_user_config_path() -> Option<PathBuf> {
    let (canonical, legacy) = user_config_paths();
    Some(relocate_user_config(
        &canonical,
        legacy.as_deref(),
        |path| std::fs::remove_file(path),
    ))
}

fn relocate_user_config(
    canonical: &Path,
    legacy: Option<&Path>,
    remove_legacy: impl FnOnce(&Path) -> std::io::Result<()>,
) -> PathBuf {
    let source = selected_user_config_path(canonical, legacy);
    if source == canonical {
        return source;
    }
    match crate::config_fix::migrate_user_config_to(
        &source,
        canonical,
        CONFIG_READ_LIMIT,
        remove_legacy,
    ) {
        Ok(Some(outcome)) if !outcome.notice().is_empty() => {
            crate::slog_warn!("config user: {}", outcome.notice())
        }
        Err(error) if error.kind() != std::io::ErrorKind::NotFound => {
            crate::slog_warn!(
                "config user {}: read source at /: {error}",
                source.display()
            );
        }
        _ => {}
    }
    selected_user_config_path(canonical, legacy)
}

/// CortexKit project config: `<root>/.cortexkit/aft.jsonc`.
fn cortexkit_project_config_path(project_root: &Path) -> PathBuf {
    project_root.join(".cortexkit").join("aft.jsonc")
}

/// Prefer the shared file, then the active harness's legacy files, without writes.
/// Within a harness, aft.jsonc precedes aft.json. Without a plugin harness,
/// OpenCode is checked before Pi, matching the TypeScript fallback ordering.
fn project_config_candidates(project_root: &Path, harness: Option<&Harness>) -> Vec<PathBuf> {
    let mut candidates = vec![cortexkit_project_config_path(project_root)];
    let directories = if matches!(harness, Some(Harness::Pi)) {
        [".pi", ".opencode"]
    } else {
        [".opencode", ".pi"]
    };
    for directory in directories {
        for filename in ["aft.jsonc", "aft.json"] {
            candidates.push(project_root.join(directory).join(filename));
        }
    }
    candidates
}

pub fn project_config_read_path(project_root: &Path, harness: Option<&Harness>) -> PathBuf {
    project_config_candidates(project_root, harness)
        .into_iter()
        .find(|path| path.exists())
        .unwrap_or_else(|| cortexkit_project_config_path(project_root))
}

pub fn legacy_project_config_location_notice(project_root: &Path, path: &Path) -> Option<String> {
    let canonical = cortexkit_project_config_path(project_root);
    if path == canonical {
        return None;
    }
    Some(format!(
        "Legacy project config {} is read in place; move it to {} yourself, and use aft doctor --fix to update retired keys.",
        path.display(), canonical.display()
    ))
}

/// Largest configuration file the plain readers below accept, matching the
/// bound the prepared bind reader uses.
pub(crate) const CONFIG_READ_LIMIT: usize = 1024 * 1024;

/// Rewrite the user file's retired keys before it is read (see
/// [`crate::config_fix::auto_migrate_user_config`]) and log the outcome. A
/// file that cannot be rewritten is still read as it is: the resolver
/// translates its recently retired keys in memory and refuses the older ones.
fn migrate_user_file(user_path: &Path) -> Option<UserConfigMigration> {
    let outcome = crate::config_fix::auto_migrate_user_config(user_path)?;
    crate::slog_warn!("config user: {}", outcome.notice());
    Some(outcome)
}

/// Read the user + project config files into raw tiers. Pure over its path
/// inputs (no env, no fixed locations) so it is directly testable. Mirrors the
/// TS `readConfigTiers`: push `{tier, source, doc}` with the RAW file content as
/// `doc` (the resolver's `parse_tier` strips JSONC), skipping any missing or
/// unreadable file silently. The user file is first migrated off retired keys;
/// the project file is never written.
fn read_tiers_from(
    user_config_path: Option<&Path>,
    project_config_path: &Path,
) -> (Vec<ConfigTier>, Option<UserConfigMigration>) {
    let mut tiers = Vec::new();
    let mut migration = None;

    #[cfg(any(any(test, feature = "test-support"), feature = "test-timing-hooks"))]
    if let Some(delay_ms) = std::env::var("AFT_TEST_SUBC_CONFIG_READ_DELAY_MS")
        .ok()
        .and_then(|raw| raw.parse::<u64>().ok())
    {
        std::thread::sleep(std::time::Duration::from_millis(delay_ms));
    }

    if let Some(user_path) = user_config_path {
        migration = migrate_user_file(user_path);
        // Shared open: on Windows no reader of this file may block a tool that
        // replaces it while this handle lives.
        let read = crate::config::open_config_file(user_path)
            .and_then(|file| crate::subc_config::read_bounded_config(file, CONFIG_READ_LIMIT));
        if !matches!(&read, Err(error) if error.kind() == std::io::ErrorKind::NotFound) {
            let doc =
                read.unwrap_or_else(|error| format!("unreadable user configuration: {error}"));
            tiers.push(ConfigTier {
                tier: "user".to_string(),
                source: user_path.to_string_lossy().into_owned(),
                doc,
            });
        }
    }

    if let Ok(doc) = crate::config::open_config_file(project_config_path)
        .and_then(|file| crate::subc_config::read_bounded_config(file, CONFIG_READ_LIMIT))
    {
        tiers.push(ConfigTier {
            tier: "project".to_string(),
            source: project_config_path.to_string_lossy().into_owned(),
            doc,
        });
    }

    (tiers, migration)
}

/// Read the CortexKit config home (user) + project config for a subc bind. These
/// tiers are TRUSTED-LOCAL origin and keep their labels. `user_config_path` is
/// resolved once at the subc boundary (`cortexkit_user_config_path`) and passed
/// in, keeping this pure for testing.
pub fn read_local_cortexkit_config_tiers(
    user_config_path: Option<&Path>,
    project_root: &Path,
) -> Vec<ConfigTier> {
    read_local_cortexkit_config_tiers_with_migration(user_config_path, project_root).0
}

/// [`read_local_cortexkit_config_tiers`], also returning what the automatic
/// migration of the user file did, for callers that report it to the user.
pub fn read_local_cortexkit_config_tiers_with_migration(
    user_config_path: Option<&Path>,
    project_root: &Path,
) -> (Vec<ConfigTier>, Option<UserConfigMigration>) {
    read_local_cortexkit_config_tiers_for_harness(user_config_path, project_root, None)
}

/// The local tier reader with legacy-file precedence for the active harness.
pub fn read_local_cortexkit_config_tiers_for_harness(
    user_config_path: Option<&Path>,
    project_root: &Path,
    harness: Option<&Harness>,
) -> (Vec<ConfigTier>, Option<UserConfigMigration>) {
    let path = project_config_read_path(project_root, harness);
    if let Some(notice) = legacy_project_config_location_notice(project_root, &path) {
        crate::slog_warn!("{notice}");
    }
    read_tiers_from(user_config_path, &path)
}

/// The `disabled_tools` list the module-wide subc catalog is filtered by,
/// read once when the module connects. Only the user file applies: the catalog
/// is shared by every route, so project files and per-harness overrides (which
/// differ per route) cannot shape it. A config the resolver rejects yields the
/// default list, the same one a missing file gives.
pub fn catalog_disabled_tools(user_config_path: Option<&Path>) -> Vec<String> {
    let tiers: Vec<ConfigTier> = user_config_path
        .and_then(|path| {
            migrate_user_file(path);
            let doc = crate::config::open_config_file(path)
                .and_then(|file| crate::subc_config::read_bounded_config(file, CONFIG_READ_LIMIT))
                .ok()?;
            Some(ConfigTier {
                tier: "user".to_string(),
                source: path.to_string_lossy().into_owned(),
                doc,
            })
        })
        .into_iter()
        .collect();
    let resolved = crate::config_resolve::resolve_config(&tiers);
    if !resolved.errors.is_empty() {
        log::warn!(
            "subc catalog: user config rejected ({}); filtering by the default disabled_tools",
            resolved.errors.join(", ")
        );
    }
    resolved.config.disabled_tools
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::config_fix::with_backup_dir_for_test;
    use crate::config_resolve::resolve_config_onto;

    // ---- path resolution (pure, no env mutation) ----

    #[test]
    fn user_path_prefers_absolute_xdg_config_home() {
        // The XDG base must be absolute ON THE HOST OS: `/xdg/cfg` is absolute on
        // Unix but NOT on Windows (which needs a drive letter), and the production
        // filter correctly ignores a non-absolute XDG per the XDG spec. Build the
        // expected path the same way production does so the separator matches.
        let xdg = if cfg!(windows) {
            r"C:\xdg\cfg"
        } else {
            "/xdg/cfg"
        };
        let home = if cfg!(windows) {
            r"C:\home\u"
        } else {
            "/home/u"
        };
        let path = user_config_path_from(Some(OsStr::new(xdg)), Some(OsStr::new(home)));
        let expected = PathBuf::from(xdg).join("cortexkit").join("aft.jsonc");
        assert_eq!(path, Some(expected));
    }

    #[test]
    fn user_path_falls_back_to_home_config_when_xdg_unset() {
        let path = user_config_path_from(None, Some(OsStr::new("/home/u")));
        assert_eq!(
            path,
            Some(PathBuf::from("/home/u/.config/cortexkit/aft.jsonc"))
        );
    }

    #[test]
    fn user_path_treats_empty_xdg_as_unset() {
        let path = user_config_path_from(Some(OsStr::new("")), Some(OsStr::new("/home/u")));
        assert_eq!(
            path,
            Some(PathBuf::from("/home/u/.config/cortexkit/aft.jsonc"))
        );
    }

    #[test]
    fn user_path_none_when_no_home_and_no_xdg() {
        assert_eq!(user_config_path_from(None, None), None);
    }

    #[test]
    fn settings_config_home_matches_daemon_across_platforms() {
        const EXPECTED: &str = "AFT_TEST_CANONICAL_SETTINGS_HOME";
        if let Some(expected) = std::env::var_os(EXPECTED) {
            assert_eq!(
                user_config_paths().0,
                PathBuf::from(expected).join("cortexkit/aft.jsonc")
            );
            return;
        }

        // Exercise the shared resolver in a child so changing its environment
        // does not affect settings reads from other tests.
        let dir = tempfile::tempdir().unwrap();
        let profile = dir.path().join("profile");
        let home = dir.path().join("home");
        let roaming = dir.path().join("roaming");
        let vars = ["XDG_CONFIG_HOME", "APPDATA", "USERPROFILE", "HOME"];
        let check = |values: [Option<&Path>; 4], expected: &Path| {
            let mut child = std::process::Command::new(std::env::current_exe().unwrap());
            child
                .current_dir(dir.path())
                .args([
                    "--exact",
                    "subc_config::tests::settings_config_home_matches_daemon_across_platforms",
                ])
                .env(EXPECTED, expected);
            for (name, value) in vars.into_iter().zip(values) {
                match value {
                    Some(value) => {
                        child.env(name, value);
                    }
                    None => {
                        child.env_remove(name);
                    }
                }
            }
            let output = child.output().unwrap();
            assert!(
                output.status.success(),
                "{}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            assert!(String::from_utf8_lossy(&output.stdout).contains("1 passed"));
        };
        check(
            [
                Some(Path::new("relative")),
                Some(&roaming),
                Some(&profile),
                Some(&home),
            ],
            Path::new("relative"),
        );
        #[cfg(windows)]
        {
            check(
                [None, Some(&roaming), Some(&profile), Some(&home)],
                &roaming,
            );
            check(
                [None, Some(Path::new("")), Some(&profile), Some(&home)],
                &profile.join("AppData/Roaming"),
            );
        }
        #[cfg(not(windows))]
        check(
            [None, Some(&roaming), Some(&profile), Some(&home)],
            &home.join(".config"),
        );
        check([None, None, None, Some(&home)], &home.join(".config"));
        check([None, None, None, None], Path::new(".config"));
    }

    #[cfg(windows)]
    #[test]
    fn windows_config_startup_moves_home_settings_to_appdata() {
        const FIXTURE: &str = "AFT_TEST_WINDOWS_SETTINGS_RELOCATION";
        const ORIGINAL: &str = "{\r\n  // keep this setting\r\n  \"bash\": true\r\n}\r\n";
        if let Some(root) = std::env::var_os(FIXTURE).map(PathBuf::from) {
            let legacy = root.join("home/.config/cortexkit/aft.jsonc");
            let canonical = root.join("roaming/cortexkit/aft.jsonc");
            let data = root.join("data");
            assert_eq!(cortexkit_user_config_path(), Some(legacy.clone()));
            assert_eq!(
                startup_cortexkit_user_config_path(),
                Some(canonical.clone())
            );
            assert!(
                missing(&legacy),
                "the production move must delete the old path"
            );
            let converted = std::fs::read_to_string(&canonical).unwrap();
            assert!(converted.contains("// keep this setting\r\n"));
            let strict = subc_jsonc::jsonc_to_json(&converted).unwrap();
            let settings: serde_json::Value = serde_json::from_str(&strict).unwrap();
            assert_eq!(settings["bash"]["enabled"], true);
            assert_eq!(cortexkit_user_config_path(), Some(canonical.clone()));
            assert_eq!(startup_cortexkit_user_config_path(), Some(canonical));
            let backups: Vec<_> = std::fs::read_dir(data)
                .unwrap()
                .map(|entry| entry.unwrap().path())
                .collect();
            assert_eq!(backups.len(), 1);
            assert_eq!(std::fs::read(&backups[0]).unwrap(), ORIGINAL.as_bytes());
            return;
        }

        // Exercise filesystem operations and path resolution with real child
        // environment variables; parallel tests keep their own environment.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let legacy = root.join("home/.config/cortexkit/aft.jsonc");
        std::fs::create_dir_all(legacy.parent().unwrap()).unwrap();
        std::fs::write(legacy, ORIGINAL).unwrap();
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "subc_config::tests::windows_config_startup_moves_home_settings_to_appdata",
            ])
            .env(FIXTURE, root)
            .env("HOME", root.join("home"))
            .env("APPDATA", root.join("roaming"))
            .env("AFT_STORAGE_DIR", root.join("data"))
            .env_remove("XDG_CONFIG_HOME")
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            output.status.success() && stdout.contains("1 passed"),
            "{stdout}\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    fn settings_relocation_uses_canonical_file_even_if_legacy_delete_fails() {
        let dir = tempfile::tempdir().unwrap();
        let old = dir.path().join("old/cortexkit/aft.jsonc");
        let canonical = dir.path().join("canonical/cortexkit/aft.jsonc");
        let data = dir.path().join("data");
        let original = r#"{"bash": true}"#;
        std::fs::create_dir_all(old.parent().unwrap()).unwrap();
        std::fs::write(&old, original).unwrap();
        let attempted = std::cell::Cell::new(0);
        let start = || {
            with_backup_dir_for_test(&data, || {
                relocate_user_config(&canonical, Some(&old), |path| {
                    assert_eq!(path, old);
                    attempted.set(attempted.get() + 1);
                    Err(std::io::Error::new(
                        std::io::ErrorKind::PermissionDenied,
                        "legacy delete denied",
                    ))
                })
            })
        };
        let selected = start();
        assert_eq!(selected, canonical);
        assert_eq!(std::fs::read_to_string(&old).unwrap(), original);
        let (tiers, migration) =
            read_tiers_from(Some(&selected), &dir.path().join("project.jsonc"));
        assert_eq!(tiers[0].source, canonical.to_string_lossy());
        assert!(tiers[0].doc.contains("\"enabled\": true"));
        assert_eq!(migration, None);
        assert_eq!(start(), canonical);
        assert_eq!(attempted.get(), 1);
        assert_eq!(std::fs::read_to_string(&canonical).unwrap(), tiers[0].doc);
        let backups: Vec<_> = std::fs::read_dir(data)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect();
        assert_eq!(backups.len(), 1);
        assert_eq!(std::fs::read_to_string(&backups[0]).unwrap(), original);
    }

    // ---- local file read ----

    #[test]
    fn reads_user_and_project_with_raw_jsonc_docs() {
        let dir = tempfile::tempdir().unwrap();
        let user = dir.path().join("user-aft.jsonc");
        let project = dir.path().join("project-aft.jsonc");
        // Comments preserved in the raw doc — the resolver strips JSONC.
        std::fs::write(&user, "{\n  // user\n  \"edit_mode\": \"hashline\"\n}").unwrap();
        std::fs::write(&project, "{ \"indexes\": { \"semantic\": false } }").unwrap();

        let (tiers, _) = read_tiers_from(Some(&user), &project);
        assert_eq!(tiers.len(), 2);
        assert_eq!(tiers[0].tier, "user");
        assert!(tiers[0].doc.contains("// user"));
        assert_eq!(tiers[1].tier, "project");
        assert_eq!(tiers[1].source, project.to_string_lossy());
    }

    /// Reading migrates the user file off retired keys and reads the new text;
    /// the project file is shared through its repository, so its bytes stay
    /// exactly as they were, and the names it still uses that were retired in
    /// 0.62 or earlier are translated with a notice rather than refused.
    #[test]
    fn reading_migrates_the_user_file_and_never_writes_the_project_file() {
        let dir = tempfile::tempdir().unwrap();
        let user = dir.path().join("user-aft.jsonc");
        let project_root = dir.path().join("repo");
        std::fs::create_dir_all(project_root.join(".cortexkit")).unwrap();
        let project = project_root.join(".cortexkit").join("aft.jsonc");
        let user_text = "{\n  // user\n  \"search_index\": false\n}\n";
        let project_text = "{ \"semantic_search\": false, \"hoist_builtin_tools\": false }";
        std::fs::write(&user, user_text).unwrap();
        std::fs::write(&project, project_text).unwrap();

        let (tiers, migration) = with_backup_dir_for_test(&dir.path().join("data"), || {
            read_local_cortexkit_config_tiers_with_migration(Some(&user), &project_root)
        });
        assert!(matches!(
            migration,
            Some(crate::config_fix::UserConfigMigration::Migrated { .. })
        ));
        assert_ne!(tiers[0].doc, user_text);
        assert!(
            tiers[0].doc.contains("\"trigram\": false"),
            "{}",
            tiers[0].doc
        );
        assert_eq!(std::fs::read_to_string(&project).unwrap(), project_text);
        assert_eq!(tiers[1].doc, project_text);

        let resolved = crate::config_resolve::resolve_config(&tiers);
        assert!(resolved.errors.is_empty(), "{:?}", resolved.errors);
        // The project's `semantic_search: false` applies as `indexes.semantic`.
        assert!(!resolved.config.indexes.semantic);
        let user_only = crate::config_resolve::resolve_config(&tiers[..1]);
        assert!(user_only.errors.is_empty(), "{:?}", user_only.errors);
        assert!(!user_only.config.indexes.trigram);

        let (_, again) =
            read_local_cortexkit_config_tiers_with_migration(Some(&user), &project_root);
        assert_eq!(again, None, "the second read finds nothing to migrate");
        assert_eq!(std::fs::read_to_string(&project).unwrap(), project_text);
    }

    #[test]
    fn experimental_keys_migrate_only_the_user_file_when_tiers_are_read() {
        let dir = tempfile::tempdir().unwrap();
        let user = dir.path().join("user-aft.jsonc");
        let project_root = dir.path().join("repo");
        std::fs::create_dir_all(project_root.join(".cortexkit")).unwrap();
        let project = project_root.join(".cortexkit/aft.jsonc");
        let user_text = r#"{"experimental_lsp_ty":true,"experimental_bash_rewrite":true,"experimental_bash_compress":true,"experimental_bash_background":true}"#;
        let project_text = "{\n // shared settings\n \"experimental_bash_compress\": false\n}\n";
        std::fs::write(&user, user_text).unwrap();
        std::fs::write(&project, project_text).unwrap();
        let (tiers, migration) = with_backup_dir_for_test(&dir.path().join("data"), || {
            read_local_cortexkit_config_tiers_with_migration(Some(&user), &project_root)
        });
        assert!(matches!(
            migration,
            Some(UserConfigMigration::Migrated { .. })
        ));
        let value: serde_json::Value = serde_json::from_str(&tiers[0].doc).unwrap();
        assert_eq!(value["experimental"]["lsp_ty"], true);
        assert_eq!(
            value["bash"],
            serde_json::json!({"rewrite":true,"compress":true,"background":true})
        );
        assert_eq!(std::fs::read_to_string(&project).unwrap(), project_text);
        assert_eq!(tiers[1].doc, project_text);
        let resolved = crate::config_resolve::resolve_config(&tiers);
        assert!(resolved.errors.is_empty(), "{:?}", resolved.errors);
        let user_only = crate::config_resolve::resolve_config(&tiers[..1]);
        assert!(user_only.errors.is_empty(), "{:?}", user_only.errors);
        assert!(user_only.config.experimental_lsp_ty);
        assert!(user_only.config.experimental_bash_compress);
        let user_after = std::fs::read_to_string(&user).unwrap();
        let (_, again) =
            read_local_cortexkit_config_tiers_with_migration(Some(&user), &project_root);
        assert_eq!(again, None);
        assert_eq!(std::fs::read_to_string(&user).unwrap(), user_after);
        assert_eq!(std::fs::read_to_string(&project).unwrap(), project_text);
    }

    #[test]
    fn missing_files_yield_no_tiers() {
        let dir = tempfile::tempdir().unwrap();
        let (tiers, _) = read_tiers_from(
            Some(&dir.path().join("nope-user.jsonc")),
            &dir.path().join("nope-project.jsonc"),
        );
        assert!(tiers.is_empty());
    }

    // ---- the security property: per-tier file trust ----
    // The active harness chooses only its matching override from each FILE tier.
    // The user file remains trusted; the project file is untrusted and its
    // privileged fields are dropped after that harness selection.

    const PRIVILEGED_DOC: &str = r#"{ "semantic": { "api_key_env": "SECRET_KEY" } }"#;

    #[test]
    fn user_file_privileged_field_is_trusted_project_file_is_dropped() {
        let dir = tempfile::tempdir().unwrap();
        let user = dir.path().join("user-aft.jsonc");
        std::fs::write(&user, PRIVILEGED_DOC).unwrap();

        // Project file (in-repo, untrusted) tries to set the same privileged field.
        let project_root = dir.path();
        let project_cfg_dir = project_root.join(".cortexkit");
        std::fs::create_dir_all(&project_cfg_dir).unwrap();
        std::fs::write(
            project_cfg_dir.join("aft.jsonc"),
            r#"{ "semantic": { "api_key_env": "PROJECT_INJECTED" } }"#,
        )
        .unwrap();

        let tiers = read_local_cortexkit_config_tiers(Some(&user), project_root);
        let mut base = Config::default();
        let dropped = resolve_config_onto(&tiers, &mut base);

        // The user FILE's privileged value is honored.
        assert_eq!(
            base.semantic.api_key_env.as_deref(),
            Some("SECRET_KEY"),
            "user-file privileged field must be trusted"
        );
        // The project FILE's attempt to override it is dropped.
        assert!(
            dropped.iter().any(|d| d.key == "semantic.api_key_env"),
            "project-file privileged field must be dropped by the resolver"
        );
    }
}
#[test]
fn legacy_project_paths_match_shared_typescript_fixtures_and_are_read_without_writes() {
    // The shared fixtures spell paths with `/`. Join them one component at a
    // time so the expected paths use the host separator, as the paths the
    // product builds and the notice prints do; `root.join("dir/file")` keeps
    // the `/` on Windows and its display would never appear in the notice.
    let fixture_path = |root: &Path, relative: &str| -> PathBuf {
        relative
            .split('/')
            .fold(root.to_path_buf(), |path, part| path.join(part))
    };
    let fixtures: serde_json::Value = serde_json::from_str(include_str!(
        "../../aft/tests/fixtures/config_locations/project_paths.json"
    ))
    .unwrap();
    for fixture in fixtures["cases"].as_array().unwrap() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let text = "{\n // committed legacy config\n \"experimental_bash_compress\": false\n}\n";
        let files = fixture["files"].as_array().unwrap();
        for file in files {
            let path = root.join(file.as_str().unwrap());
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
        }
        let harness = match fixture["harness"].as_str() {
            Some("opencode") => Some(Harness::Opencode),
            Some("pi") => Some(Harness::Pi),
            _ => None,
        };
        let path = project_config_read_path(root, harness.as_ref());
        assert_eq!(
            path,
            fixture_path(root, fixture["selected"].as_str().unwrap()),
            "{}",
            fixture["name"]
        );
        let (tiers, migration) =
            read_local_cortexkit_config_tiers_for_harness(None, root, harness.as_ref());
        assert!(migration.is_none());
        if files.is_empty() {
            assert!(tiers.is_empty());
        } else {
            assert_eq!(tiers.len(), 1);
            assert_eq!(tiers[0].tier, "project");
            assert_eq!(tiers[0].source, path.to_string_lossy());
            assert_eq!(tiers[0].doc, text);
            // A project file's long-retired names load with a notice.
            let resolved = crate::config_resolve::resolve_config(&tiers);
            assert!(
                resolved.errors.is_empty(),
                "{}: {:?}",
                fixture["name"],
                resolved.errors
            );
        }
        for file in files {
            assert_eq!(
                std::fs::read_to_string(root.join(file.as_str().unwrap())).unwrap(),
                text
            );
        }
        let canonical_exists = files
            .iter()
            .any(|file| file.as_str() == Some(".cortexkit/aft.jsonc"));
        let canonical = fixture_path(root, ".cortexkit/aft.jsonc");
        assert_eq!(canonical.exists(), canonical_exists);
        let notice = legacy_project_config_location_notice(root, &path);
        if path == canonical {
            assert!(notice.is_none());
        } else {
            let notice = notice.unwrap();
            assert!(notice.contains(&path.display().to_string()));
            assert!(notice.contains(&canonical.display().to_string()));
            assert!(notice.contains("doctor --fix"));
        }
    }
}

#[cfg(test)]
mod shared_config_open_tests {
    use super::*;

    #[test]
    fn config_read_handles_do_not_block_replacing_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("aft.jsonc");
        std::fs::write(&path, r#"{"indexes": {"trigram": false}}"#).unwrap();

        let file = crate::config::open_config_file(&path).unwrap();
        assert!(
            read_bounded_config(&file, CONFIG_READ_LIMIT)
                .unwrap()
                .contains("trigram"),
            "the shared open still reads the file"
        );

        let replacement = dir.path().join("replacement.jsonc");
        std::fs::write(&replacement, r#"{"indexes": {"semantic": false}}"#).unwrap();
        std::fs::rename(&replacement, &path).expect("replace the config while it is open");
        drop(file);
        assert!(std::fs::read_to_string(&path).unwrap().contains("semantic"));
    }
}
