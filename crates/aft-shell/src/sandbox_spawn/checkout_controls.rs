#![doc(hidden)]

use std::path::{Path, PathBuf};

#[cfg(unix)]
fn git_controls(dir: &Path) -> [PathBuf; 3] {
    ["config", "config.worktree", "hooks"].map(|name| dir.join(name))
}

fn aft_controls(root: &Path) -> [PathBuf; 2] {
    [
        root.join(".cortexkit/aft.jsonc"),
        root.join(".cortexkit/aft/filters"),
    ]
}

/// Build the same control-file denies for project and granted write roots,
/// including a linked worktree's common Git directory. Backends that cannot
/// enforce nested denies report those paths as unenforced.
#[cfg(unix)]
pub(super) fn checkout_control_denies(
    root: &Path,
    git: &super::GitPolicy,
) -> Result<Vec<PathBuf>, String> {
    let mut denies = Vec::new();
    if git.read_roots.is_empty() {
        denies.extend(git_controls(&root.join(".git")));
    } else {
        for dir in &git.read_roots {
            denies.extend(git_controls(dir));
        }
    }
    denies.extend(git.hooks.iter().cloned());
    denies.extend(
        git.configured_paths
            .iter()
            .filter(|(key, _)| key != "core.excludesFile")
            .map(|(_, path)| path.clone()),
    );
    denies.extend(super::resolve_missing_repository_hooks(
        root,
        git,
        &mut Vec::new(),
    )?);
    if let Some((git_dir, _)) = &git.linked {
        // Binding pointers must stay immutable too: redirecting one would
        // replace the operator-owned configuration used on the next spawn.
        denies.push(root.join(".git"));
        denies.extend(["commondir", "gitdir"].map(|name| git_dir.join(name)));
    }
    denies.extend(aft_controls(root));
    Ok(denies)
}

#[doc(hidden)]
pub fn remote_control_denies(root: &Path) -> Result<Vec<PathBuf>, String> {
    #[cfg(unix)]
    let mut denies = protected_paths(&checkout_control_denies(
        root,
        &super::resolve_git_policy(root)?,
    )?)?
    .into_iter()
    .collect::<Vec<_>>();
    #[cfg(not(unix))]
    let mut denies = aft_controls(root).into_iter().collect::<Vec<_>>();
    // Remote tools do not need to update Git metadata, even when .git is a
    // worktree pointer or is absent when the request arrives.
    denies.push(root.join(".git"));
    Ok(denies)
}

#[cfg(unix)]
pub const REMOTE_PROTECTED_GRANT: &str = "remote shell grant overlaps a protected path";

/// Check the protected inodes rather than walking unrelated project subtrees.
/// Privileged bind mounts are outside this filesystem threat model.
#[cfg(unix)]
pub fn protected_paths(denies: &[PathBuf]) -> Result<std::collections::HashSet<PathBuf>, String> {
    use std::{fs, io, os::unix::fs::MetadataExt};
    const MAX_ENTRIES: usize = 4096;
    let mut protected = std::collections::HashSet::new();
    let mut pending = denies.to_vec();
    let mut budget = MAX_ENTRIES;
    while let Some(path) = pending.pop() {
        budget = budget.checked_sub(1).ok_or(REMOTE_PROTECTED_GRANT)?;
        if !protected.insert(path.clone()) {
            continue;
        }
        let metadata = match fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error.to_string()),
        };
        if metadata.is_symlink() {
            pending.extend(
                crate::sandbox_profile::resolve_env_deny_paths(path, "write_deny_nested")
                    .map_err(|error| error.to_string())?,
            );
        } else if metadata.is_file() && metadata.nlink() != 1 {
            return Err(REMOTE_PROTECTED_GRANT.into());
        } else if metadata.is_dir() {
            let path = path.canonicalize().map_err(|error| error.to_string())?;
            let dir =
                crate::pinned_dir::PinnedDir::open(&path).map_err(|error| error.to_string())?;
            for entry in dir.unix_entries().map_err(|error| error.to_string())? {
                if pending.len() >= budget {
                    return Err(REMOTE_PROTECTED_GRANT.into());
                }
                let (name, _) = entry.map_err(|error| error.to_string())?;
                pending.push(path.join(name));
            }
        }
    }
    Ok(protected)
}
