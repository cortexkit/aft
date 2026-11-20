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

pub(crate) fn remote_control_denies(root: &Path) -> Result<Vec<PathBuf>, String> {
    #[cfg(unix)]
    let mut denies = checkout_control_denies(root, &super::resolve_git_policy(root)?)?;
    #[cfg(not(unix))]
    let mut denies = aft_controls(root).into_iter().collect::<Vec<_>>();
    // Remote tools do not need to update Git metadata, even when .git is a
    // worktree pointer or is absent when the request arrives.
    denies.push(root.join(".git"));
    Ok(denies)
}
