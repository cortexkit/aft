use super::{ChildEnvironment, SandboxProfile};
use std::path::Path;

pub(super) fn task_environment(
    _profile: &SandboxProfile,
    launcher: &Path,
    environment: &mut ChildEnvironment,
) {
    #[cfg(target_os = "linux")]
    let task_launcher = _profile
        .temp_dir
        .parent()
        .map(crate::pinned_executable::task_executable);
    #[cfg(target_os = "linux")]
    let launcher = task_launcher.as_deref().unwrap_or(launcher);
    crate::pinned_executable::task_environment(launcher, environment);
}

#[cfg(target_os = "linux")]
pub(super) fn finish_linux_profile(
    mut profile: SandboxProfile,
    home: &Path,
    git_policies: &[super::GitPolicy],
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
