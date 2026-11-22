#![cfg(target_os = "linux")]

use super::SandboxProfile;
use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
pub(super) struct LauncherIdentity(u64, u64);

impl LauncherIdentity {
    pub(super) fn capture(path: &Path) -> Option<Self> {
        use std::os::unix::fs::MetadataExt;
        let metadata = path.symlink_metadata().ok()?;
        // A task can replace this name after launch. A private snapshot may
        // record only the direct regular file, never the task's symlink.
        metadata
            .is_file()
            .then(|| Self(metadata.dev(), metadata.ino()))
    }

    pub(super) fn validate_saved(expected: Option<Self>, path: &Path) -> Result<(), &'static str> {
        let expected = expected.ok_or("snapshot has no launcher identity")?;
        if Self::capture(path) == Some(expected) {
            Ok(())
        } else {
            Err("saved launcher identity no longer matches")
        }
    }
}

pub(super) fn finish_profile(
    mut profile: SandboxProfile,
    home: &Path,
    git_policies: &[super::GitPolicy],
    task_io: &Path,
) -> Result<SandboxProfile, String> {
    if let Some(pin) = crate::pinned_executable::linux_task_pin().map_err(|e| e.to_string())? {
        // Landlock rules are additive. Protect the entire cache from broad
        // grants and carve out only the current regular file for read/execute.
        let cache = pin
            .parent()
            .and_then(Path::parent)
            .expect("Linux pin stored under an image identity directory");
        profile.data_policy.deny.push(cache.to_path_buf());
        profile.data_policy.read_allow.push(pin);
        if let Some(mounts) = &mut profile.linux_mounts {
            mounts.task_executable_alias = Some(crate::pinned_executable::task_executable(task_io));
        }
    }

    let git_read_roots = git_policies
        .iter()
        .flat_map(|policy| policy.read_roots.iter().cloned())
        .collect::<Vec<_>>();
    let profile = super::build_linux_profile(profile, home, &git_read_roots)?
        .canonicalize_for_launch()
        .map_err(|error| error.to_string())?;
    super::validate_final_read_rules(&profile.read_allow, &profile.read_deny)?;
    assert!(
        super::validate_final_read_rules(&profile.read_allow, &profile.read_deny).is_ok(),
        "final Landlock read grants overlap a denied path"
    );
    Ok(profile)
}
