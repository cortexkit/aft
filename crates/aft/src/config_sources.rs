//! Recorded configuration tiers and the paths that supplied them.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use crate::config::Config;
use crate::config_resolve::ConfigTier;

/// What one tier's text was when it was last applied, and where it lives.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TierSource {
    /// `user` or `project`.
    pub tier: &'static str,
    /// The config file for this tier, when known.
    pub path: Option<PathBuf>,
    /// The text applied last, or `None` when the tier was absent.
    pub text: Option<String>,
    /// Whether `text` was read from `path`. A tier relayed on the wire by an
    /// older plugin whose file does not exist is kept as it is.
    pub from_file: bool,
}

/// What the last configure applied for a root, so a reload can tell whether
/// the files changed and which changed keys are still waiting for a connect.
#[derive(Debug, Clone)]
pub struct ConfigSources {
    pub(crate) prepared_bind_snapshot_id: Option<u64>,
    pub(crate) approved_user: Option<crate::subc_config::ApprovedUser>,
    pub user: TierSource,
    pub project: TierSource,
    /// The resolver output of the last configure, before configure's own
    /// adjustments. Deferred keys are reported relative to it.
    pub connected: Config,
    /// Whether this root's context watches the user file itself. False in the
    /// daemon, where one process-level watch serves every root.
    pub owns_user_watch: bool,
}

impl ConfigSources {
    /// Record what configure resolved from. `tiers` is what configure passed to
    /// the resolver: a tier whose `source` names the tier's file was read from
    /// that file (the daemon's bind reads the files and relays them this way).
    pub fn from_configure(
        user_config_path: Option<PathBuf>,
        project_root: &Path,
        tiers: &[ConfigTier],
        connected: &Config,
    ) -> Self {
        let project_path =
            crate::subc_config::project_config_read_path(project_root, connected.harness.as_ref());
        Self::from_observed_paths(user_config_path, project_path, tiers, connected)
    }

    /// Record the source paths selected by a trusted local observation without
    /// probing them again. Reload must watch the same files that supplied tiers.
    pub(crate) fn from_observed_paths(
        user_config_path: Option<PathBuf>,
        project_path: PathBuf,
        tiers: &[ConfigTier],
        connected: &Config,
    ) -> Self {
        let owns_user_watch = user_config_path.is_some() && process_user_config_path().is_none();
        let user_path = user_config_path.or_else(process_user_config_path);
        let source_for = |tier: &'static str, path: Option<PathBuf>| {
            let used = tiers.iter().find(|candidate| candidate.tier == tier);
            let from_file = match (used, path.as_ref()) {
                (Some(used), Some(path)) => Path::new(&used.source) == path.as_path(),
                (None, _) => true,
                (Some(_), None) => false,
            };
            TierSource {
                tier,
                path,
                text: used.map(|used| used.doc.clone()),
                from_file,
            }
        };
        Self {
            prepared_bind_snapshot_id: None,
            approved_user: None,
            user: source_for("user", user_path),
            project: source_for("project", Some(project_path)),
            connected: connected.clone(),
            owns_user_watch,
        }
    }
}

static PROCESS_USER_CONFIG_PATH: OnceLock<PathBuf> = OnceLock::new();

/// The daemon watches the user config file once for the whole process and
/// records its path here, so root contexts do not each start a watch.
pub fn set_process_user_config_path(path: PathBuf) {
    let _ = PROCESS_USER_CONFIG_PATH.set(path);
}

fn process_user_config_path() -> Option<PathBuf> {
    PROCESS_USER_CONFIG_PATH.get().cloned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn observed_sources_keep_exact_paths_and_distinguish_relayed_text() {
        let user = PathBuf::from("user.jsonc");
        let project = PathBuf::from("project.jsonc");
        let config = Config::default();
        let tiers = vec![
            ConfigTier {
                tier: "user".into(),
                source: "user.jsonc".into(),
                doc: "{}".into(),
            },
            ConfigTier {
                tier: "project".into(),
                source: "wire".into(),
                doc: "{ }".into(),
            },
        ];
        let sources = ConfigSources::from_observed_paths(
            Some(user.clone()),
            project.clone(),
            &tiers,
            &config,
        );
        assert_eq!(sources.user.path, Some(user));
        assert_eq!(sources.project.path, Some(project));
        assert_eq!(sources.user.text.as_deref(), Some("{}"));
        assert_eq!(sources.project.text.as_deref(), Some("{ }"));
        assert!(sources.user.from_file);
        assert!(!sources.project.from_file);
        assert_eq!(sources.prepared_bind_snapshot_id, None);
    }

    #[test]
    fn absent_tiers_record_absent_text_as_file_backed() {
        let sources = ConfigSources::from_observed_paths(
            Some(PathBuf::from("user.jsonc")),
            PathBuf::from("project.jsonc"),
            &[],
            &Config::default(),
        );
        assert_eq!(sources.user.text, None);
        assert_eq!(sources.project.text, None);
        assert!(sources.user.from_file);
        assert!(sources.project.from_file);
    }
}
