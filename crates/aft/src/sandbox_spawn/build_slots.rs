//! Read and lock grants for the operator-configured `prefrontal-build-slot`
//! rustc wrapper, which jailed workers run for every Rust build.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use super::{
    canonicalize_profile_candidate, cargo_build_settings, profile_candidate_is_file,
    worker_cargo_configs, WorkerJailGrants,
};

/// Build-slot grants for every `prefrontal-build-slot` launcher that a Cargo
/// configuration above the worker's checkouts names as its rustc wrapper.
pub(super) fn worker_build_slot_grants(
    home: &Path,
    project_roots: &[PathBuf],
    policy: &crate::access_policy::AccessPolicy,
) -> WorkerJailGrants {
    let mut grants = WorkerJailGrants::default();
    for (config_path, base) in worker_cargo_configs(home, project_roots) {
        let Some(wrapper) = cargo_build_settings(&config_path)
            .and_then(|build| build.rustc_wrapper)
            .map(PathBuf::from)
        else {
            continue;
        };
        let wrapper = if wrapper.is_absolute() {
            Some(wrapper)
        } else {
            base.map(|base| base.join(wrapper))
        };
        if let Some(wrapper) = wrapper {
            add_build_slot_grants(&wrapper, project_roots, policy, &mut grants);
        }
    }
    grants
}

/// Adds build-slot grants to a relaxed profile's data policy. The relaxed
/// profile has no separate jail grant lists: the launcher's directory and
/// locks become read exceptions to the denied data folder, the locks also
/// write exceptions, and the launcher itself stays write-denied.
#[cfg(any(target_os = "macos", test))]
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub(super) fn add_build_slot_reads(
    grants: &WorkerJailGrants,
    stores: &mut crate::sandbox_profile::SandboxDataPolicy,
    write_deny: &mut Vec<PathBuf>,
) {
    stores.read_allow.extend(
        grants
            .read_only
            .iter()
            .chain(&grants.read_dirs)
            .chain(&grants.files)
            .cloned(),
    );
    stores.write_allow.extend(grants.files.iter().cloned());
    write_deny.extend(grants.read_only.iter().cloned());
}

/// Retain only the operator-configured `prefrontal-build-slot` launcher's
/// directory: the launcher and its `rustc-lowprio-direct` companion take a
/// slot lock and compile inside the sandbox rather than asking a shared
/// server. The Cargo configuration and launcher directory must be outside
/// every model-writable worktree and inside a private store, where no jailed
/// shell can write.
///
/// The directory is read recursively rather than file by file: the launcher
/// reads its own metadata files (named by the launcher, such as
/// `prefrontal-build-slot.root`), and an exact list that drifts from the
/// launcher's layout fails every Rust build in a jailed worker. The sandbox
/// checks a symlink's target, so a link inside the directory grants nothing
/// outside it. Only the queue and slot locks are writable, file by file, so
/// the directory goes in `read_dirs`, not in the write-denied `read_only`.
pub(super) fn add_build_slot_grants(
    wrapper: &Path,
    project_roots: &[PathBuf],
    policy: &crate::access_policy::AccessPolicy,
    grants: &mut WorkerJailGrants,
) {
    let Ok(wrapper) = canonicalize_profile_candidate(wrapper) else {
        return;
    };
    if wrapper.file_name() != Some(OsStr::new("prefrontal-build-slot"))
        || !profile_candidate_is_file(&wrapper)
    {
        return;
    }
    let Some(dir) = wrapper.parent() else {
        return;
    };
    if project_roots
        .iter()
        .any(|root| root.starts_with(dir) || dir.starts_with(root))
        || !policy
            .private_stores
            .deny
            .iter()
            .any(|deny| dir.starts_with(deny) && dir != deny)
    {
        return;
    }
    let paths = [
        wrapper.clone(),
        dir.join("rustc-lowprio-direct"),
        dir.join("limit"),
    ];
    let Ok(paths) = paths
        .into_iter()
        .map(|path| path.canonicalize())
        .collect::<Result<Vec<_>, _>>()
    else {
        return;
    };
    if paths
        .iter()
        .any(|path| !profile_candidate_is_file(path) || !path.starts_with(dir))
    {
        return;
    }
    let Some(limit) = std::fs::read_to_string(dir.join("limit"))
        .ok()
        .and_then(|value| value.trim().parse::<u16>().ok())
        .filter(|limit| (1..=256).contains(limit))
    else {
        return;
    };
    // Landlock's single-file grants require existing inodes. Provision only
    // the configured queue and slot locks, never a writable directory grant.
    // Include both endpoints so zero-based and one-based slot numbering work.
    let locks = std::iter::once(dir.join("queue.lock"))
        .chain((0..=limit).map(|slot| dir.join(format!("{slot}.lock"))));
    let mut files = Vec::new();
    for path in locks {
        if std::fs::symlink_metadata(&path)
            .is_ok_and(|metadata| !metadata.is_file() || metadata.file_type().is_symlink())
        {
            return;
        }
        if std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .is_err()
        {
            return;
        }
        files.push(path);
    }
    grants.files.extend(files);
    // The wrapper's own path also goes in exactly: a strict sandbox empties
    // RUSTC_WRAPPER and restores only a `prefrontal-build-slot` path it finds
    // listed read-only (see the sandboxed child environment).
    grants.read_only.push(wrapper.clone());
    grants.read_dirs.push(dir.to_path_buf());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn worker_build_slot_grants_read_the_launcher_directory_and_reject_model_writable_launchers() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().canonicalize().unwrap();
        let store = home.join(".local/share/cortexkit");
        let project = store.join("alfonso/worktrees/container/own");
        // Prefrontal's layout: the launcher sits beside the worktree
        // containers and names its own metadata files after itself.
        let slots = store.join("alfonso/worktrees/.build-slots");
        for path in [&project, &slots] {
            std::fs::create_dir_all(path).unwrap();
        }
        for name in [
            "prefrontal-build-slot",
            "rustc-lowprio-direct",
            "prefrontal-build-slot.root",
            "prefrontal-build-slot.cache",
        ] {
            std::fs::write(slots.join(name), "operator-owned").unwrap();
        }
        std::fs::write(slots.join("limit"), "2\n").unwrap();
        let policy = crate::access_policy::AccessPolicy::from_config(
            &crate::config::Config::default(),
            &store.join("aft"),
            vec![project.clone()],
            &home,
        );
        let mut grants = WorkerJailGrants::default();
        add_build_slot_grants(
            &slots.join("prefrontal-build-slot"),
            &[project.clone()],
            &policy,
            &mut grants,
        );
        assert_eq!(
            grants.files,
            ["queue.lock", "0.lock", "1.lock", "2.lock"].map(|name| slots.join(name))
        );
        assert_eq!(grants.read_only, [slots.join("prefrontal-build-slot")]);
        assert_eq!(grants.read_dirs, [slots.clone()]);
        assert!(grants.roots.is_empty() && grants.caches.is_empty());
        let mut refused = WorkerJailGrants::default();
        add_build_slot_grants(
            &slots.join("prefrontal-build-slot"),
            &[slots.clone()],
            &policy,
            &mut refused,
        );
        assert_eq!(
            refused,
            WorkerJailGrants::default(),
            "a model-writable launcher must never be pinned"
        );
        #[cfg(unix)]
        {
            std::fs::remove_file(slots.join("rustc-lowprio-direct")).unwrap();
            std::fs::write(home.join("secret"), "secret").unwrap();
            std::os::unix::fs::symlink(home.join("secret"), slots.join("rustc-lowprio-direct"))
                .unwrap();
            add_build_slot_grants(
                &slots.join("prefrontal-build-slot"),
                &[project],
                &policy,
                &mut refused,
            );
            assert_eq!(
                refused,
                WorkerJailGrants::default(),
                "a companion that links outside the launcher directory must not be pinned"
            );
        }
    }
}
