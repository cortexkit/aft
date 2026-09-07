//! Subc-mode local config read (subc edge only).
//!
//! Config is read directly from the CortexKit user and project files: user
//! `~/.config/cortexkit/aft.jsonc` and project `<root>/.cortexkit/aft.jsonc`.
//! There is NO wire-relayed config path, so a front (runner, `mcp:*`, or `fed:*`)
//! cannot push config over the connection. `config_resolve` then selects the
//! active bind's optional harness override from each file tier.
//!
//! Trust remains purely per-TIER after that selection: the user file is trusted
//! (the user's own disk), while privileged fields from the untrusted in-repo
//! project file, including its harness override, are dropped.

use crate::config_resolve::ConfigTier;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};

/// CortexKit user config home: `$XDG_CONFIG_HOME/cortexkit/aft.jsonc`, falling
/// back to `~/.config/cortexkit/aft.jsonc`. Matches the shared CortexKit
/// convention (`~/.config/cortexkit/<module>.jsonc`) alongside `subc.jsonc` and
/// `mcp.jsonc`. Pure over its env inputs so it is testable without mutating
/// process-global env vars (which race under the parallel test runner).
///
/// This copy becomes the daemon's config-home callable when that API is available.
/// The call replaces these ordered daemon rungs: non-empty XDG_CONFIG_HOME;
/// Windows-only non-empty APPDATA; Windows-only non-empty USERPROFILE joined with
/// `AppData/Roaming`; non-empty HOME joined with `.config`; relative `.config`.
/// Last re-derived 2026-09-06 against subconscious
/// d5e09914b0791a66f2a5a00a9bb3422860ade95e: compare `(rung, guard)` pairs with
/// `subc-core/src/daemon_config.rs::default_config_path` and resolve
/// `DAEMON_CONFIG_RELATIVE_PATH` before editing this temporary copy.
pub(crate) fn user_config_path_from(
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

/// Resolve the production CortexKit user config path from the process env. This
/// is the only env-reading entry; it is called once at the subc boundary and the
/// resolved path is threaded down, so the per-bind composition stays pure (and
/// the integration tests inject a path instead of mutating env, which races).
pub fn cortexkit_user_config_path() -> Option<PathBuf> {
    let xdg = std::env::var_os("XDG_CONFIG_HOME");
    let home = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE"));
    user_config_path_from(xdg.as_deref(), home.as_deref())
}

/// CortexKit project config: `<root>/.cortexkit/aft.jsonc`.
fn cortexkit_project_config_path(project_root: &Path) -> PathBuf {
    project_root.join(".cortexkit").join("aft.jsonc")
}

/// Read the user + project config files into raw tiers. Pure over its path
/// inputs (no env, no fixed locations) so it is directly testable. Mirrors the
/// TS `readConfigTiers`: push `{tier, source, doc}` with the RAW file content as
/// `doc` (the resolver's `parse_tier` strips JSONC), skipping any missing or
/// unreadable file silently.
fn read_tiers_from(user_config_path: Option<&Path>, project_config_path: &Path) -> Vec<ConfigTier> {
    let mut tiers = Vec::new();

    #[cfg(debug_assertions)]
    if let Some(delay_ms) = std::env::var("AFT_TEST_SUBC_CONFIG_READ_DELAY_MS")
        .ok()
        .and_then(|raw| raw.parse::<u64>().ok())
    {
        std::thread::sleep(std::time::Duration::from_millis(delay_ms));
    }

    if let Some(user_path) = user_config_path {
        if let Ok(doc) = std::fs::read_to_string(user_path) {
            tiers.push(ConfigTier {
                tier: "user".to_string(),
                source: user_path.to_string_lossy().into_owned(),
                doc,
            });
        }
    }

    if let Ok(doc) = std::fs::read_to_string(project_config_path) {
        tiers.push(ConfigTier {
            tier: "project".to_string(),
            source: project_config_path.to_string_lossy().into_owned(),
            doc,
        });
    }

    tiers
}

/// Read the CortexKit config home (user) + project config for a subc bind. These
/// tiers are TRUSTED-LOCAL origin and keep their labels. `user_config_path` is
/// resolved once at the subc boundary (`cortexkit_user_config_path`) and passed
/// in, keeping this pure for testing.
pub fn read_local_cortexkit_config_tiers(
    user_config_path: Option<&Path>,
    project_root: &Path,
) -> Vec<ConfigTier> {
    read_tiers_from(
        user_config_path,
        &cortexkit_project_config_path(project_root),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
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

    // ---- local file read ----

    #[test]
    fn reads_user_and_project_with_raw_jsonc_docs() {
        let dir = tempfile::tempdir().unwrap();
        let user = dir.path().join("user-aft.jsonc");
        let project = dir.path().join("project-aft.jsonc");
        // Comments preserved in the raw doc — the resolver strips JSONC.
        std::fs::write(&user, "{\n  // user\n  \"search_index\": true\n}").unwrap();
        std::fs::write(&project, "{ \"semantic_search\": false }").unwrap();

        let tiers = read_tiers_from(Some(&user), &project);
        assert_eq!(tiers.len(), 2);
        assert_eq!(tiers[0].tier, "user");
        assert!(tiers[0].doc.contains("// user"));
        assert_eq!(tiers[1].tier, "project");
        assert_eq!(tiers[1].source, project.to_string_lossy());
    }

    #[test]
    fn missing_files_yield_no_tiers() {
        let dir = tempfile::tempdir().unwrap();
        let tiers = read_tiers_from(
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
