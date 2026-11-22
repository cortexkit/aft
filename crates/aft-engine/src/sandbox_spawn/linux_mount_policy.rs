use std::path::{Path, PathBuf};

use super::{
    absolute_env_path, checkout_control_denies, git_worktree_paths, linked_git_file_grants,
    linked_git_grants, profile_candidate_exists, profile_candidate_is_dir,
    protect_worker_credential_services, resolve_git_policy, resolve_missing_repository_hooks,
    worker_jail_active, GitPolicy,
};
#[cfg(any(target_os = "macos", target_os = "linux"))]
use super::{
    build_slots,
    worker_reads::{grant_worker_pool_reads, worker_cargo_configs, worker_target_roots},
};
use crate::context::AppContext;
#[cfg(target_os = "linux")]
use crate::sandbox_profile::LinuxMountPolicy;
use crate::{
    config::SandboxMode,
    sandbox_profile::{resolved_env_deny_paths, SandboxProfile},
};

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
        // A recursive read-only bind also covers an allowed checkout below
        // it. Defer these broad denies until the writable carveouts exist.
        let writable = profile.write_allow_roots();
        let (nested, deferred_read_only): (Vec<_>, _) = profile
            .write_deny_nested
            .iter()
            .cloned()
            .partition(|deny| !writable.iter().any(|root| root.starts_with(deny)));
        LinuxMountPolicy {
            credential_masks: configured_shell_read_denies(ctx, home)?,
            read_masks: profile.read_deny.clone(),
            read_only: profile.write_deny.iter().cloned().chain(nested).collect(),
            deferred_read_only,
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

/// Build Standard's protected-path list without predicting the caches, sockets
/// or services a build will need. Each backend reports which denies it enforces.
pub(super) fn build_relaxed_profile(
    ctx: &AppContext,
    home: &Path,
    roots: &[PathBuf],
    git_policies: &[GitPolicy],
    session_store: &Path,
    task_io: &Path,
    temp: &Path,
) -> Result<SandboxProfile, String> {
    let mut policy = ctx
        .access_policy("")
        .map_err(|error| error.data.to_string())?;
    let mut read_deny = policy.absolute_floor.clone();
    let mut write_deny = read_deny.clone();
    write_deny.extend(standard_home_write_denies(home));
    let mut stores = policy.private_stores.clone();
    stores.deny.push(session_store.to_path_buf());
    stores.session_task_read_dir = Some(session_store.to_path_buf());
    stores.deny.extend([
        home.join(".local/share/cortexkit"),
        home.join(".local/state/cortexkit"),
        ctx.storage_dir(),
    ]);
    for key in ["XDG_DATA_HOME", "XDG_STATE_HOME"] {
        if let Some(base) = absolute_env_path(key) {
            stores
                .deny
                .extend(resolved_env_deny_paths(key, base.join("cortexkit")));
        }
    }
    stores.read_allow = roots.to_vec();
    stores.write_allow = roots.to_vec();
    for data in [
        Some(home.join(".local/share")),
        absolute_env_path("XDG_DATA_HOME"),
    ]
    .into_iter()
    .flatten()
    {
        // Workers already have their canonical checkout roots above.
        // Other workers' checkouts stay private; ancestor metadata does not
        // grant directory listings or access to their descendants.
        if !worker_jail_active() {
            stores
                .read_allow
                .push(data.join("cortexkit/alfonso/worktrees"));
        }
        stores.read_allow.push(data.join("cortexkit/bin"));
    }
    stores
        .read_allow
        .extend([temp.to_path_buf(), task_io.to_path_buf()]);
    stores
        .write_allow
        .extend([temp.to_path_buf(), task_io.to_path_buf()]);
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    if worker_jail_active() {
        grant_worker_pool_reads(home, roots, &mut stores, &mut write_deny);
    }
    // Select grants against the complete credential and store floors before
    // protecting their control files or carving out sibling checkout denies.
    policy.absolute_floor = read_deny.clone();
    policy.private_stores = stores.clone();
    // The read limit hides the worker pool outside the worker's own
    // checkout, and the build-slot launcher sits in that pool.
    #[cfg(target_os = "linux")]
    let mut slot_files = Vec::new();
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    if worker_jail_active() {
        let slots = build_slots::worker_build_slot_grants(home, roots, &policy);
        build_slots::add_build_slot_reads(&slots, &mut stores, &mut write_deny);
        #[cfg(target_os = "linux")]
        slot_files.extend(slots.files);
        policy.private_stores = stores.clone();
    }
    let write_grants = policy.shell_grants(crate::access_policy::AccessOperation::Write);
    let mut control_roots = roots.to_vec();
    control_roots.extend(write_grants.iter().cloned());
    control_roots.sort_unstable();
    control_roots.dedup();
    let control_git_policies = control_roots
        .iter()
        .map(|root| resolve_git_policy(root))
        .collect::<Result<Vec<_>, _>>()?;
    let mut writable = roots.to_vec();
    writable.extend(linked_git_grants(git_policies));
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    if worker_jail_active() {
        let targets = worker_target_roots(home, roots, &policy, &worker_cargo_configs(home, roots));
        stores.read_allow.extend(targets.iter().cloned());
        writable.extend(targets);
    }
    let files = linked_git_file_grants(git_policies);
    #[cfg(target_os = "linux")]
    let files = files.into_iter().chain(slot_files).collect::<Vec<_>>();
    stores.write_allow.extend(writable.iter().cloned());
    stores.write_allow.extend(files.iter().cloned());
    let mut checkout_denies = vec![home.join("Work")];
    for (root, git) in control_roots.iter().zip(&control_git_policies) {
        write_deny.extend(checkout_control_denies(root, git)?);
        stores.read_allow.extend(git.read_roots.iter().cloned());
        stores.read_allow.extend(git.hooks.iter().cloned());
        stores
            .read_allow
            .extend(git.configured_paths.iter().map(|(_, path)| path.clone()));
        stores
            .read_metadata_allow
            .extend(git.metadata_allow.iter().cloned());
        let mut metadata = Vec::new();
        for path in resolve_missing_repository_hooks(root, git, &mut metadata)? {
            if profile_candidate_is_dir(&path) {
                stores.read_allow.push(path);
            }
        }
        stores.read_metadata_allow.extend(metadata);
        for path in git_worktree_paths(root, git)? {
            if !roots.contains(&path) {
                checkout_denies.push(path);
            }
        }
    }
    let storage = ctx.storage_dir();
    for path in [
        crate::pinned_executable::shims_dir(&storage),
        crate::agent_child_env::managed_git_hooks_dir(&storage),
    ] {
        if profile_candidate_exists(&path) {
            stores.read_allow.push(path.clone());
        }
        write_deny.push(path);
    }
    if let Ok(binary) = crate::agent_child_env::shim_binary(&ctx.config()) {
        if profile_candidate_exists(&binary) {
            stores.read_allow.push(binary.clone());
        }
        write_deny.push(binary);
    }
    #[cfg(target_os = "linux")]
    if let Some(pin) = crate::pinned_executable::linux_task_pin().map_err(|e| e.to_string())? {
        // The inner launcher and the task's shim use this exact image. The
        // private cache that contains the image stays masked.
        stores.read_files.push(pin);
    }
    stores
        .read_allow
        .retain(|path| profile_candidate_exists(path));
    writable.extend(write_grants.iter().cloned());
    stores.write_allow.extend(write_grants);
    stores
        .read_allow
        .extend(policy.shell_grants(crate::access_policy::AccessOperation::Read));
    // A denied canary tells the gh shim not to fall back to the operator's gh.
    // Linux reports this fence as unenforced when bubblewrap is unavailable.
    let jailed = worker_jail_active();
    let mut worker_gh_fence = None;
    if jailed {
        let dirs = crate::worker_jail_canary::prepare_homes(home, true)?;
        read_deny.extend(dirs.iter().cloned());
        write_deny.extend(dirs.iter().cloned());
        worker_gh_fence = Some(crate::sandbox_profile::WorkerGhFence::Canary {
            paths: dirs
                .iter()
                .map(|dir| dir.join(crate::worker_jail_canary::CANARY_FILE))
                .collect(),
        });
    } else {
        stores
            .read_allow
            .extend(crate::worker_jail_canary::prepare_homes(home, false)?);
    }
    let mut profile = SandboxProfile {
        secret_proxy: false,
        v: crate::sandbox_profile::SANDBOX_PROFILE_VERSION,
        mode: SandboxMode::Standard,
        worker_jail: false,
        keychain_deny: Vec::new(),
        deny_prefixes: Vec::new(),
        data_policy: stores,
        writable_roots: writable,
        writable_files: files,
        write_deny,
        write_deny_nested: checkout_denies,
        read_allow: Vec::new(),
        read_allow_listed: Vec::new(),
        read_deny,
        socket_deny: Vec::new(),
        loopback_tcp_connect_deny: Vec::new(),
        cache_roots: Vec::new(),
        temp_dir: temp.to_path_buf(),
        linux_mounts: None,
        worker_gh_fence,
    };
    protect_worker_credential_services(&mut profile, home)?;
    Ok(profile)
}
