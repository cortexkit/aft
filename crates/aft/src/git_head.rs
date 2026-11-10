//! Where a checkout's Git `HEAD` lives and when it last changed, read from
//! the `.git` directory or `gitdir:` marker without running Git.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GitHeadMetadata {
    pub head_path: PathBuf,
    pub head_mtime: Option<SystemTime>,
    pub resolved_ref_path: Option<PathBuf>,
    pub resolved_ref_mtime: Option<SystemTime>,
}

impl GitHeadMetadata {
    pub fn matches_path(&self, path: &Path) -> bool {
        same_file_path(path, &self.head_path)
            || self
                .resolved_ref_path
                .as_deref()
                .is_some_and(|resolved| same_file_path(path, resolved))
    }

    pub fn watch_paths(&self) -> impl Iterator<Item = &Path> {
        std::iter::once(self.head_path.as_path()).chain(self.resolved_ref_path.as_deref())
    }
}

/// Find the checkout's `HEAD` file (following a linked worktree's `gitdir:`
/// marker) and the branch ref it names, with their modification times.
pub fn capture_git_head_metadata(
    repo_root: &Path,
    git_common_dir: Option<&Path>,
) -> std::io::Result<GitHeadMetadata> {
    let dot_git = repo_root.join(".git");
    let git_dir = if dot_git.is_dir() {
        canonical_or_original(dot_git)
    } else {
        let marker = fs::read_to_string(&dot_git)
            .map_err(|e| crate::io_accounting::at("reading Git directory marker", &dot_git, e))?;
        let relative = marker
            .trim()
            .strip_prefix("gitdir:")
            .map(str::trim)
            .ok_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("invalid gitdir marker at {}", dot_git.display()),
                )
            })?;
        let path = PathBuf::from(relative);
        canonical_or_original(if path.is_absolute() {
            path
        } else {
            repo_root.join(path)
        })
    };
    let head_path = canonical_or_original(git_dir.join("HEAD"));
    let head = fs::read_to_string(&head_path)
        .map_err(|e| crate::io_accounting::at("reading Git HEAD", &head_path, e))?;
    let resolved_ref_path = head
        .trim()
        .strip_prefix("ref:")
        .map(str::trim)
        .filter(|reference| !reference.is_empty())
        .map(|reference| canonical_or_original(git_common_dir.unwrap_or(&git_dir).join(reference)));
    Ok(GitHeadMetadata {
        head_mtime: modified_time(&head_path),
        resolved_ref_mtime: resolved_ref_path.as_deref().and_then(modified_time),
        head_path,
        resolved_ref_path,
    })
}

fn canonical_or_original(path: PathBuf) -> PathBuf {
    fs::canonicalize(&path).unwrap_or(path)
}

fn modified_time(path: &Path) -> Option<SystemTime> {
    fs::metadata(path)
        .and_then(|metadata| metadata.modified())
        .ok()
}

fn same_file_path(path: &Path, target: &Path) -> bool {
    path == target
        || fs::canonicalize(target)
            .map(|target| path == target)
            .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn directory_head_error_names_operation_and_path() {
        let root = tempfile::tempdir().unwrap();
        let head = root.path().join(".git").join("HEAD");
        fs::create_dir_all(&head).unwrap();
        let error = capture_git_head_metadata(root.path(), None)
            .unwrap_err()
            .to_string();
        assert!(error.contains("reading Git HEAD"), "{error}");
        // Windows reports the root in its verbatim `\\?\` spelling, so accept
        // the canonical form as well as the path the test built.
        let canonical = fs::canonicalize(&head).unwrap();
        assert!(
            error.contains(&head.display().to_string())
                || error.contains(&canonical.display().to_string()),
            "{error}"
        );
        assert!(head.is_dir());
    }
}
