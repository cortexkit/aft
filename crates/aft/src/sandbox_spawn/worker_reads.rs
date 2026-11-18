//! Which paths outside its own checkout a jailed worker may read: the exact
//! Cargo and container files above the checkout, shared build inputs under
//! `.rust-build-seeds`, and operator-configured build targets. Seed directories
//! are read-only; parent listings stay denied.

use std::path::{Path, PathBuf};

use super::{
    absolute_env_path, canonicalize_profile_candidate, cargo_build_settings,
    create_profile_candidate_dir, jail_target_allowed, profile_candidate_is_file,
};

const WORKER_ANCESTOR_CARGO_CONFIG_FILES: &[&str] = &[".cargo/config.toml", ".cargo/config"];
pub(super) const WORKER_CONTAINER_FILES: &[&str] = &[".cargo/alfonso-test-threads"];
pub(super) const WORKER_CONTAINER_DIRS: &[&str] = &[".rust-build-seeds"];

#[cfg(any(target_os = "macos", test))]
pub(super) fn worker_pool_read_files(home: &Path, roots: &[PathBuf]) -> Vec<PathBuf> {
    // Missing Cargo probes keep exact-file intent rather than becoming a
    // recursive directory grant if they appear before launch.
    let names = WORKER_ANCESTOR_CARGO_CONFIG_FILES
        .iter()
        .chain(WORKER_CONTAINER_FILES)
        .copied()
        .collect::<Vec<_>>();
    worker_pool_read_paths(home, roots, &names)
}

#[cfg(any(target_os = "macos", test))]
pub(super) fn worker_pool_read_dirs(home: &Path, roots: &[PathBuf]) -> Vec<PathBuf> {
    worker_pool_read_paths(home, roots, WORKER_CONTAINER_DIRS)
        .into_iter()
        .filter(|path| super::profile_candidate_is_dir(path))
        .collect()
}

#[cfg(any(target_os = "macos", test))]
fn worker_pool_read_paths(home: &Path, roots: &[PathBuf], names: &[&str]) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    for data in [
        Some(home.join(".local/share")),
        absolute_env_path("XDG_DATA_HOME"),
    ]
    .into_iter()
    .flatten()
    {
        let Ok(pool) = canonicalize_profile_candidate(&data.join("cortexkit/alfonso/worktrees"))
        else {
            continue;
        };
        for root in roots {
            for ancestor in root.ancestors().skip(1) {
                if !ancestor.starts_with(&pool) {
                    break;
                }
                paths.extend(names.iter().map(|name| ancestor.join(name)));
            }
        }
    }
    paths.sort_unstable();
    paths.dedup();
    paths
}

pub(super) fn worker_cargo_configs(
    home: &Path,
    project_roots: &[PathBuf],
) -> Vec<(PathBuf, Option<PathBuf>)> {
    // Cargo walks every ancestor, but only configurations outside the worker's
    // own checkout may authorize shared build locations.
    let cargo_home = absolute_env_path("CARGO_HOME").unwrap_or_else(|| home.join(".cargo"));
    let mut configs = vec![
        (
            cargo_home.join("config.toml"),
            cargo_home.parent().map(Path::to_path_buf),
        ),
        (
            cargo_home.join("config"),
            cargo_home.parent().map(Path::to_path_buf),
        ),
    ];
    for root in project_roots {
        for ancestor in root.ancestors().skip(1) {
            for name in WORKER_ANCESTOR_CARGO_CONFIG_FILES {
                configs.push((ancestor.join(name), Some(ancestor.to_path_buf())));
            }
        }
    }
    let mut seen = std::collections::BTreeSet::new();
    configs
        .into_iter()
        .filter_map(|(path, base)| {
            if !seen.insert(path.clone()) {
                return None;
            }
            let path = canonicalize_profile_candidate(&path).ok()?;
            (profile_candidate_is_file(&path)
                && !project_roots.iter().any(|root| path.starts_with(root)))
            .then_some((path, base))
        })
        .collect()
}

pub(super) fn worker_target_roots(
    home: &Path,
    project_roots: &[PathBuf],
    policy: &crate::access_policy::AccessPolicy,
    configs: &[(PathBuf, Option<PathBuf>)],
) -> Vec<PathBuf> {
    let mut targets = ["CARGO_TARGET_DIR", "CARGO_BUILD_TARGET_DIR"]
        .into_iter()
        .filter_map(absolute_env_path)
        .collect::<Vec<_>>();
    for (path, base) in configs {
        if let Some(target) = cargo_build_settings(path).and_then(|build| build.target_dir) {
            let target = PathBuf::from(target);
            match (&base, target.is_absolute()) {
                (_, true) => targets.push(target),
                (Some(base), false) => targets.push(base.join(target)),
                (None, false) => {}
            }
        }
    }

    let mut roots = Vec::new();
    for target in targets {
        let target = crate::inspect::job::normalize_path(&target);
        if !jail_target_allowed(&target, home, project_roots, policy) {
            log::warn!(
                "worker sandbox: ignoring configured cargo target {}: it would contain the home, a project root, or a protected path",
                target.display()
            );
            continue;
        }
        if create_profile_candidate_dir(&target) {
            roots.push(target);
        }
    }
    roots
}

/// Adds a macOS worker's read-only exceptions above its own checkout to a data
/// policy: the exact Cargo and container files, and the `.rust-build-seeds`
/// directories (prebuilt dependency outputs a new worktree's build copies from)
/// read recursively. Each path is also denied for writing. The links beside
/// the checkout get metadata reads only, so lookups can follow them (see
/// [`worker_container_links`]).
#[cfg(any(target_os = "macos", test))]
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub(super) fn grant_worker_pool_reads(
    home: &Path,
    roots: &[PathBuf],
    stores: &mut crate::sandbox_profile::SandboxDataPolicy,
    write_deny: &mut Vec<PathBuf>,
) {
    let files = worker_pool_read_files(home, roots);
    stores.read_files.extend(files.iter().cloned());
    write_deny.extend(files);
    let dirs = worker_pool_read_dirs(home, roots);
    stores.read_allow.extend(dirs.iter().cloned());
    write_deny.extend(dirs);
    stores
        .read_metadata_allow
        .extend(worker_container_links(home, roots));
}

/// How many entries of a container folder are examined for links. A
/// container holds one repository's worktrees and the links beside them, so
/// this bound only stops a pathological folder from slowing every launch.
#[cfg(any(target_os = "macos", test))]
const CONTAINER_ENTRY_LIMIT: usize = 4096;

/// The symbolic links in each checkout's container folder, such as the
/// sibling-repository links the worker system places beside every worktree
/// (`../subconscious`). The container is denied, so a lookup through a link
/// stops at the link itself unless its own metadata is readable. Granting the
/// link's metadata grants nothing else: the sandbox still checks the link's
/// target, so a link into another worker's checkout or a protected folder
/// stays unreadable.
#[cfg(any(target_os = "macos", test))]
fn worker_container_links(home: &Path, roots: &[PathBuf]) -> Vec<PathBuf> {
    let mut links = Vec::new();
    for data in [
        Some(home.join(".local/share")),
        absolute_env_path("XDG_DATA_HOME"),
    ]
    .into_iter()
    .flatten()
    {
        let Ok(pool) = canonicalize_profile_candidate(&data.join("cortexkit/alfonso/worktrees"))
        else {
            continue;
        };
        for root in roots {
            let Some(container) = root.parent().filter(|dir| dir.starts_with(&pool)) else {
                continue;
            };
            let Ok(entries) = std::fs::read_dir(container) else {
                continue;
            };
            links.extend(
                entries
                    .take(CONTAINER_ENTRY_LIMIT)
                    .filter_map(Result::ok)
                    .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_symlink()))
                    .map(|entry| entry.path()),
            );
        }
    }
    links.sort_unstable();
    links.dedup();
    links
}

/// Every name a jailed worker may read in its checkout's container folder:
/// the exact files, then the directories.
pub(super) fn worker_container_names() -> impl Iterator<Item = &'static str> {
    WORKER_CONTAINER_FILES
        .iter()
        .chain(WORKER_CONTAINER_DIRS)
        .copied()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn worker_pool_read_files_and_dirs_stop_at_the_canonical_pool() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().canonicalize().unwrap();
        let real = home.join("real-pool");
        let pool = home.join(".local/share/cortexkit/alfonso/worktrees");
        std::fs::create_dir_all(real.join("hash-a/own")).unwrap();
        std::fs::create_dir_all(pool.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(&real, &pool).unwrap();
        let own = pool.join("hash-a/own").canonicalize().unwrap();
        let outside = home.join("outside");
        std::fs::create_dir(&outside).unwrap();
        let roots = [own, outside];
        let files = worker_pool_read_files(&home, &roots);
        let mut expected = [
            ".cargo/config.toml",
            ".cargo/config",
            ".cargo/alfonso-test-threads",
            "hash-a/.cargo/config.toml",
            "hash-a/.cargo/config",
            "hash-a/.cargo/alfonso-test-threads",
        ]
        .map(|name| real.join(name))
        .to_vec();
        expected.sort_unstable();
        assert_eq!(files, expected);
        assert!(files.iter().all(|path| !path.exists()));
        assert!(worker_pool_read_dirs(&home, &roots).is_empty());
        let dirs = [
            real.join(".rust-build-seeds"),
            real.join("hash-a/.rust-build-seeds"),
        ];
        for dir in &dirs {
            std::fs::create_dir(dir).unwrap();
        }
        std::fs::create_dir_all(roots[0].join(".rust-build-seeds")).unwrap();
        std::fs::create_dir_all(roots[1].join(".rust-build-seeds")).unwrap();
        assert_eq!(worker_pool_read_dirs(&home, &roots), dirs);
        assert!(dirs.iter().all(|dir| !files.contains(dir)));
    }

    #[test]
    fn worker_container_links_lists_only_links_beside_pool_checkouts() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().canonicalize().unwrap();
        let pool = home.join(".local/share/cortexkit/alfonso/worktrees");
        let container = pool.join("hash-a");
        let own = container.join("own");
        std::fs::create_dir_all(&own).unwrap();
        std::fs::create_dir(container.join("other-worker")).unwrap();
        std::fs::write(container.join("plain-file"), "not a link").unwrap();
        let sibling = home.join("Work/sibling");
        std::fs::create_dir_all(&sibling).unwrap();
        std::os::unix::fs::symlink(&sibling, container.join("sibling")).unwrap();
        std::os::unix::fs::symlink(home.join("missing"), container.join("dangling")).unwrap();
        // A checkout outside the pool gets nothing, even with links beside it.
        let outside = home.join("outside/own");
        std::fs::create_dir_all(&outside).unwrap();
        std::os::unix::fs::symlink(&sibling, home.join("outside/sibling")).unwrap();
        assert_eq!(
            worker_container_links(&home, &[own, outside]),
            [container.join("dangling"), container.join("sibling")]
        );
    }

    #[test]
    fn worker_targets_inside_the_pool_require_operator_configuration() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().canonicalize().unwrap();
        let pool = home.join(".local/share/cortexkit/alfonso/worktrees");
        let own = pool.join("hash-a/own");
        let config = pool.join("hash-a/.cargo/config.toml");
        let target = pool.join("hash-a/configured-target");
        let sibling = pool.join("hash-a/sibling/target");
        for path in [&own, config.parent().unwrap(), &sibling] {
            std::fs::create_dir_all(path).unwrap();
        }
        std::fs::write(&config, "[build]\ntarget-dir = 'configured-target'\n").unwrap();
        std::fs::create_dir_all(own.join(".cargo")).unwrap();
        std::fs::write(
            own.join(".cargo/config.toml"),
            format!("[build]\ntarget-dir = {sibling:?}\n"),
        )
        .unwrap();
        let policy = crate::access_policy::AccessPolicy::from_config(
            &crate::config::Config::default(),
            &home.join("store"),
            vec![own.clone()],
            &home,
        );
        let configs = worker_cargo_configs(&home, &[own.clone()]);
        let targets = worker_target_roots(&home, &[own], &policy, &configs);
        assert!(targets.contains(&target));
        assert!(target.is_dir());
        assert!(!targets.iter().any(|grant| sibling.starts_with(grant)));
        assert!(!targets.iter().any(|grant| pool.starts_with(grant)));
    }
}
