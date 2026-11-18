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
