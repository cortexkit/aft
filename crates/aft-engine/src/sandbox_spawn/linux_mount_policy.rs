use std::path::{Path, PathBuf};

#[cfg(target_os = "linux")]
use super::{checkout_control_denies, resolve_git_policy, SandboxProfile};
use crate::context::AppContext;
#[cfg(target_os = "linux")]
use crate::{config::SandboxMode, sandbox_profile::LinuxMountPolicy};

pub(super) fn configured_shell_read_denies(
    ctx: &AppContext,
    home: &Path,
) -> Result<Vec<PathBuf>, String> {
    let mut denies = Vec::new();
    for path in &ctx.config().sandbox.read_deny {
        let path = crate::access_policy::expand_home(path, home);
        denies.extend(
            crate::sandbox_profile::resolve_env_deny_paths(path, "sandbox.read_deny")
                .map_err(|error| error.to_string())?,
        );
    }
    Ok(denies)
}

pub(super) fn standard_home_write_denies(home: &Path) -> [PathBuf; 7] {
    [
        ".zshrc",
        ".zprofile",
        ".bashrc",
        ".profile",
        ".gitconfig",
        ".config/git",
        "Library/LaunchAgents",
    ]
    .map(|path| home.join(path))
}

#[cfg(target_os = "linux")]
pub(super) fn permissive(
    profile: &mut SandboxProfile,
    ctx: &AppContext,
    home: &Path,
) -> Result<(), String> {
    let mounts = if profile.mode == SandboxMode::Credentials {
        LinuxMountPolicy {
            credential_masks: profile.read_deny.clone(),
            ..Default::default()
        }
    } else {
        LinuxMountPolicy {
            credential_masks: configured_shell_read_denies(ctx, home)?,
            read_masks: profile.read_deny.clone(),
            read_only: profile
                .write_deny
                .iter()
                .chain(&profile.write_deny_nested)
                .cloned()
                .collect(),
            ..Default::default()
        }
    };
    profile.linux_mounts = Some(mounts);
    Ok(())
}

#[cfg(target_os = "linux")]
pub(super) fn strict(
    profile: &mut SandboxProfile,
    ctx: &AppContext,
    home: &Path,
    env_home: &Path,
    project_roots: &[PathBuf],
    control_roots: &[PathBuf],
) -> Result<(), String> {
    let mut read_only = standard_home_write_denies(home).to_vec();
    let roots = project_roots
        .iter()
        .chain(control_roots)
        .collect::<std::collections::BTreeSet<_>>();
    for root in roots {
        read_only.extend(checkout_control_denies(root, &resolve_git_policy(root)?)?);
    }
    profile.linux_mounts = Some(LinuxMountPolicy {
        read_masks: profile.read_deny.clone(),
        credential_masks: configured_shell_read_denies(ctx, env_home)?,
        read_only,
        ..Default::default()
    });
    Ok(())
}
