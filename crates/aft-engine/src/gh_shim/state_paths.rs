use super::{gh_shim_state_dir_from, trust_root, GH_SHIM_STATE_DIR_ENV};
use std::path::PathBuf;

#[derive(Clone, Debug)]
pub(super) struct StatePaths {
    pub(super) root: PathBuf,
    pub(super) manifest: PathBuf,
    pub(super) trust_root: PathBuf,
    pub(super) rung: PathBuf,
    pub(super) bypass_audit: PathBuf,
    pub(super) unexpected_gh_route_advertisers: PathBuf,
    pub(super) seam_state: PathBuf,
    pub(super) last_valid_manifest: PathBuf,
    pub(super) version_high_water: PathBuf,
    pub(super) manifests_dir: PathBuf,
    pub(super) last_probe: PathBuf,
    pub(super) daemon_trust: bool,
}

impl StatePaths {
    pub(super) fn for_daemon(mut self) -> Self {
        self.daemon_trust = true;
        self
    }

    pub(super) fn from_process() -> Self {
        Self::from_root(gh_shim_state_dir_from(
            crate::environment::non_empty_os_var(GH_SHIM_STATE_DIR_ENV).as_deref(),
            crate::environment::non_empty_os_var("XDG_STATE_HOME").as_deref(),
            crate::environment::non_empty_os_var("HOME").as_deref(),
        ))
    }

    pub(super) fn from_root(root: PathBuf) -> Self {
        crate::private_storage::tighten_root(&root);
        Self {
            manifest: root.join("gh-routing-manifest.json"),
            trust_root: root.join(trust_root::FILE_NAME),
            rung: root.join("rung-cache.json"),
            bypass_audit: root.join("operator-bypass.jsonl"),
            unexpected_gh_route_advertisers: root.join("unexpected-gh-route-advertisers.json"),
            seam_state: root.join("seam-state.json"),
            last_valid_manifest: root.join("last-valid-manifest.json"),
            version_high_water: root.join("manifest-version-high-water.json"),
            manifests_dir: root.join("manifests"),
            last_probe: root.join("last-probe.json"),
            daemon_trust: false,
            root,
        }
    }
}
