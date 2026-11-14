//! Resolve a path the user named (a tool's `path` argument, which may start
//! with `~`) to the root a scan of it uses: the Git checkout containing it,
//! or a narrow plain folder when it is in no checkout.

use std::path::{Path, PathBuf};

#[derive(Debug)]
pub(crate) enum GitRootResolutionError {
    PathNotFound(PathBuf),
    NotAGitRoot,
    ProtectedScanRoot,
    Other(String),
}

pub(crate) fn resolve_git_root_from_user_path(
    project_root: &Path,
    raw_path: &str,
    scan_root_is_protected: impl Fn(&Path) -> bool,
) -> Result<PathBuf, GitRootResolutionError> {
    let account_home = crate::production_storage::resolve_account_home_dir();
    let environment_home = home_dir();
    resolve_git_root_from_user_path_with_homes(
        project_root,
        raw_path,
        account_home.as_deref(),
        environment_home.as_deref(),
        scan_root_is_protected,
    )
}

fn resolve_git_root_from_user_path_with_homes(
    project_root: &Path,
    raw_path: &str,
    account_home: Option<&Path>,
    environment_home: Option<&Path>,
    scan_root_is_protected: impl Fn(&Path) -> bool,
) -> Result<PathBuf, GitRootResolutionError> {
    let expanded = expand_tilde_with_home(raw_path, environment_home);
    let requested = if expanded.is_absolute() {
        expanded
    } else {
        project_root.join(expanded)
    };
    if !requested.exists() {
        return Err(GitRootResolutionError::PathNotFound(requested));
    }

    let existing = nearest_existing_parent(&requested)
        .ok_or_else(|| GitRootResolutionError::PathNotFound(requested.clone()))?;

    // `nearest_existing_parent` returns an `fs::canonicalize` result, which on
    // Windows is a verbatim (`\\?\C:\...`) path. Normalize before comparing the
    // requested directory with the configured root so aliases such as `.` and
    // symlinks remain local without requiring the workspace itself to be Git.
    let existing = crate::path_identity::canonicalize_normalized(&existing);
    if existing == crate::path_identity::canonicalize_normalized(project_root) {
        return Ok(project_root.to_path_buf());
    }

    let git_base = if existing.is_file() {
        existing.parent().unwrap_or(&existing).to_path_buf()
    } else {
        existing
    };
    match git_toplevel(&git_base) {
        Ok(root) => Ok(root),
        Err(error) if error == "not_a_git_root" => {
            let account_home = account_home.or(environment_home);
            // A scan must use the same narrow-root policy as a watcher, even
            // though it never starts a watcher or builds an index. Exact-file
            // authorization must not widen into a protected containing folder.
            if scan_root_is_protected(&git_base) {
                return Err(GitRootResolutionError::ProtectedScanRoot);
            }
            if (!crate::developer_tools::git_usable()
                && crate::git_probe::repository_marker_or_uncertainty_in_directory(&git_base))
                || plain_scan_root_is_too_broad(&git_base, account_home, environment_home)
            {
                return Err(GitRootResolutionError::NotAGitRoot);
            }
            Ok(git_base)
        }
        Err(error) => Err(GitRootResolutionError::Other(error)),
    }
}

fn plain_scan_root_is_too_broad(
    root: &Path,
    account_home: Option<&Path>,
    environment_home: Option<&Path>,
) -> bool {
    // HOME can differ from the OS account home, so a scan root must be narrow
    // relative to both homes when both are available.
    match (account_home, environment_home) {
        (None, None) => project_root_is_too_broad(root, false, None),
        (account_home, environment_home) => [account_home, environment_home]
            .into_iter()
            .flatten()
            .any(|home| project_root_is_too_broad(root, false, Some(home))),
    }
}

pub(crate) fn expand_tilde(raw: &str) -> PathBuf {
    let home = home_dir();
    expand_tilde_with_home(raw, home.as_deref())
}

fn expand_tilde_with_home(raw: &str, home: Option<&Path>) -> PathBuf {
    if raw == "~" {
        return home
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from(raw));
    }
    if let Some(rest) = raw.strip_prefix("~/") {
        if let Some(home) = home {
            return home.join(rest);
        }
    }
    PathBuf::from(raw)
}

fn home_dir() -> Option<PathBuf> {
    crate::environment::non_empty_os_var("HOME")
        .or_else(|| crate::environment::non_empty_os_var("USERPROFILE"))
        .map(PathBuf::from)
}

fn nearest_existing_parent(path: &Path) -> Option<PathBuf> {
    let mut current = path.to_path_buf();
    loop {
        if current.exists() {
            return std::fs::canonicalize(&current).ok().or(Some(current));
        }
        if !current.pop() {
            return None;
        }
    }
}

fn git_toplevel(base_dir: &Path) -> Result<PathBuf, String> {
    let stdout = match crate::git_probe::toplevel_in_directory(base_dir) {
        Ok(stdout) => stdout,
        Err(crate::git_probe::ProbeError::Spawn(error))
            if error.kind() == std::io::ErrorKind::NotFound
                && !crate::git_probe::repository_marker_or_uncertainty_in_directory(base_dir) =>
        {
            return Err("not_a_git_root".to_string());
        }
        Err(crate::git_probe::ProbeError::Spawn(error)) => {
            return Err(format!("failed to run git: {error}"));
        }
        Err(crate::git_probe::ProbeError::Failed { kind, output }) => {
            if kind == crate::git_probe::FailureKind::NotRepository {
                return Err("not_a_git_root".to_string());
            }
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(format!("git rev-parse failed: {}", stderr.trim()));
        }
    };

    let toplevel = String::from_utf8_lossy(&stdout).trim().to_string();
    if toplevel.is_empty() {
        return Err("git rev-parse returned an empty toplevel".to_string());
    }
    let toplevel = PathBuf::from(toplevel);
    Ok(std::fs::canonicalize(&toplevel).unwrap_or(toplevel))
}

pub(crate) fn project_root_is_too_broad(
    canonical_root: &Path,
    canonicalization_failed: bool,
    account_home: Option<&Path>,
) -> bool {
    let root_identity = crate::policy_path::policy_path(canonical_root);
    let account_home = account_home.filter(|home| home.is_dir());
    canonicalization_failed
        || crate::path_identity::project_root_volume(canonical_root, account_home)
            .map(|volume| crate::policy_path::policy_path(&volume))
            .is_none_or(|volume| {
                let home_identity = account_home
                    .map(crate::policy_path::policy_path)
                    .unwrap_or_else(|| volume.clone());
                crate::path_identity::project_root_is_narrow(
                    &root_identity,
                    &home_identity,
                    &volume,
                ) != Some(true)
            })
}
