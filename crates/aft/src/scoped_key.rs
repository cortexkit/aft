//! Artifact identities for standing roots that point below a Git worktree.
//!
//! A scoped root must not reuse the whole-repository session key: doing so
//! would let a subtree warm or publish an artifact for files it does not own.

use std::collections::HashMap;
use std::fmt;
use std::path::{Component, Path, PathBuf};
use std::process::Command;

use sha2::{Digest, Sha256};

use crate::config::expand_index_root_path;

const SCOPED_V1_DOMAIN: &[u8] = b"scoped-v1";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScopedKeyError {
    EmptyRelativePath,
    RelativePathHasTrailingSeparator,
    RelativePathHasBackslash,
    RelativePathHasDotComponent,
    NonUnicodePath(PathBuf),
    NotInsideGitToplevel {
        target: PathBuf,
        git_toplevel: PathBuf,
    },
    ResolvePath {
        path: PathBuf,
        detail: String,
    },
    GitProbe {
        path: PathBuf,
        detail: String,
    },
    DuplicateArtifactKey {
        artifact_key: String,
        first_path: String,
        duplicate_path: String,
    },
}

impl fmt::Display for ScopedKeyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyRelativePath => write!(f, "scoped-v1 refuses an empty relative path"),
            Self::RelativePathHasTrailingSeparator => {
                write!(f, "scoped-v1 refuses a relative path with a trailing separator")
            }
            Self::RelativePathHasBackslash => {
                write!(f, "scoped-v1 refuses a logical relative path containing a backslash")
            }
            Self::RelativePathHasDotComponent => {
                write!(f, "scoped-v1 refuses a relative path containing . or ..")
            }
            Self::NonUnicodePath(path) => {
                write!(f, "standing root path is not valid Unicode: {}", path.display())
            }
            Self::NotInsideGitToplevel {
                target,
                git_toplevel,
            } => write!(
                f,
                "resolved target {} is not inside recorded git toplevel {}",
                target.display(),
                git_toplevel.display()
            ),
            Self::ResolvePath { path, detail } => {
                write!(f, "failed to resolve standing root {}: {detail}", path.display())
            }
            Self::GitProbe { path, detail } => {
                write!(f, "failed to determine git toplevel for {}: {detail}", path.display())
            }
            Self::DuplicateArtifactKey {
                artifact_key,
                first_path,
                duplicate_path,
            } => write!(
                f,
                "duplicate standing artifact key {artifact_key} for {first_path:?} and {duplicate_path:?}"
            ),
        }
    }
}

impl std::error::Error for ScopedKeyError {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StandingArtifactIdentity {
    GitToplevel {
        artifact_key: String,
    },
    GitSubtree {
        artifact_key: String,
        scoped_relative_path: String,
    },
    NonGit {
        artifact_key: String,
    },
}

impl StandingArtifactIdentity {
    pub fn artifact_key(&self) -> &str {
        match self {
            Self::GitToplevel { artifact_key }
            | Self::GitSubtree { artifact_key, .. }
            | Self::NonGit { artifact_key } => artifact_key,
        }
    }

    pub fn scoped_relative_path(&self) -> Option<&str> {
        match self {
            Self::GitSubtree {
                scoped_relative_path,
                ..
            } => Some(scoped_relative_path),
            Self::GitToplevel { .. } | Self::NonGit { .. } => None,
        }
    }
}

/// A resolved standing entry suitable for durable path pinning.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedStandingRoot {
    pub literal_path: String,
    pub resolved_target: String,
    pub resolved_git_toplevel: Option<String>,
    pub scoped_relative_path: Option<String>,
    pub artifact_key: String,
}

/// Derive the versioned subtree identity. `repo_identity` is the canonical,
/// sorted root-commit identity returned by the existing search-index helper;
/// this function never rebuilds or normalizes it.
pub fn scoped_v1_key(repo_identity: &str, rel_path_bytes: &[u8]) -> Result<String, ScopedKeyError> {
    validate_logical_relative_path_bytes(rel_path_bytes)?;

    let mut hasher = Sha256::new();
    hasher.update(SCOPED_V1_DOMAIN);
    hasher.update([0]);
    hasher.update(repo_identity.as_bytes());
    hasher.update([0]);
    hasher.update(rel_path_bytes);
    Ok(format!("{:x}", hasher.finalize()))
}

/// Convert the two recorded canonical paths into the strict logical path used
/// by `scoped-v1`. It validates rather than repairs the recorded identity.
pub fn scoped_relative_path(
    resolved_target: &Path,
    resolved_git_toplevel: &Path,
) -> Result<String, ScopedKeyError> {
    let relative = resolved_target
        .strip_prefix(resolved_git_toplevel)
        .map_err(|_| ScopedKeyError::NotInsideGitToplevel {
            target: resolved_target.to_path_buf(),
            git_toplevel: resolved_git_toplevel.to_path_buf(),
        })?;
    if relative.as_os_str().is_empty() {
        return Err(ScopedKeyError::EmptyRelativePath);
    }

    let native = relative
        .to_str()
        .ok_or_else(|| ScopedKeyError::NonUnicodePath(relative.to_path_buf()))?;
    if native.ends_with('/') || native.ends_with('\\') {
        return Err(ScopedKeyError::RelativePathHasTrailingSeparator);
    }

    let mut components = Vec::new();
    for component in relative.components() {
        match component {
            Component::Normal(component) => {
                let component = component
                    .to_str()
                    .ok_or_else(|| ScopedKeyError::NonUnicodePath(relative.to_path_buf()))?;
                if component.contains('\\') || component.contains('/') {
                    return Err(ScopedKeyError::RelativePathHasBackslash);
                }
                components.push(component);
            }
            Component::CurDir | Component::ParentDir => {
                return Err(ScopedKeyError::RelativePathHasDotComponent)
            }
            Component::Prefix(_) | Component::RootDir => {
                return Err(ScopedKeyError::NotInsideGitToplevel {
                    target: resolved_target.to_path_buf(),
                    git_toplevel: resolved_git_toplevel.to_path_buf(),
                })
            }
        }
    }

    if components.is_empty() {
        return Err(ScopedKeyError::EmptyRelativePath);
    }
    let logical = components.join("/");
    validate_logical_relative_path_bytes(logical.as_bytes())?;
    Ok(logical)
}

/// Classify a resolved root without performing any implicit normalization or
/// storage mutation. Callers must reject duplicates before lifecycle admission.
pub fn classify_resolved_standing_root(
    resolved_target: &Path,
    resolved_git_toplevel: Option<&Path>,
) -> Result<StandingArtifactIdentity, ScopedKeyError> {
    let Some(git_toplevel) = resolved_git_toplevel else {
        return Ok(StandingArtifactIdentity::NonGit {
            artifact_key: crate::search_index::artifact_path_identity_key(resolved_target),
        });
    };

    if resolved_target == git_toplevel {
        // This deliberately calls the session derivation itself rather than a
        // sibling reimplementation, keeping top-level sharing byte-identical.
        return Ok(StandingArtifactIdentity::GitToplevel {
            artifact_key: crate::search_index::artifact_cache_key(resolved_target),
        });
    }

    let relative = scoped_relative_path(resolved_target, git_toplevel)?;
    let repo_identity = crate::search_index::canonical_git_root_commit_identity(git_toplevel)
        .map_err(|error| ScopedKeyError::GitProbe {
            path: git_toplevel.to_path_buf(),
            detail: error.to_string(),
        })?;
    let artifact_key = scoped_v1_key(&repo_identity, relative.as_bytes())?;
    Ok(StandingArtifactIdentity::GitSubtree {
        artifact_key,
        scoped_relative_path: relative,
    })
}

/// Resolve a configured spelling into the durable identity it records. The
/// literal spelling remains untouched; only the recorded target is canonical.
pub fn resolve_standing_root(literal_path: &str) -> Result<ResolvedStandingRoot, ScopedKeyError> {
    let home = crate::environment::non_empty_os_var("HOME")
        .or_else(|| crate::environment::non_empty_os_var("USERPROFILE"))
        .map(PathBuf::from);
    let expanded = expand_index_root_path(literal_path, home.as_deref()).map_err(|detail| {
        ScopedKeyError::ResolvePath {
            path: PathBuf::from(literal_path),
            detail,
        }
    })?;
    let target = std::fs::canonicalize(&expanded).map_err(|error| ScopedKeyError::ResolvePath {
        path: expanded,
        detail: error.to_string(),
    })?;
    let git_toplevel = find_git_toplevel(&target)?;
    let identity = classify_resolved_standing_root(&target, git_toplevel.as_deref())?;

    let resolved_target = unicode_path(&target)?;
    let resolved_git_toplevel = git_toplevel.as_deref().map(unicode_path).transpose()?;
    let scoped_relative_path = identity.scoped_relative_path().map(str::to_string);

    Ok(ResolvedStandingRoot {
        literal_path: literal_path.to_string(),
        resolved_target,
        resolved_git_toplevel,
        scoped_relative_path,
        artifact_key: identity.artifact_key().to_string(),
    })
}

/// Reject shared artifact identities before a standing actor can admit work.
pub fn reject_duplicate_artifact_keys(
    entries: &[ResolvedStandingRoot],
) -> Result<(), ScopedKeyError> {
    let mut first_paths = HashMap::<&str, &str>::new();
    for entry in entries {
        if let Some(first_path) = first_paths.insert(&entry.artifact_key, &entry.literal_path) {
            return Err(ScopedKeyError::DuplicateArtifactKey {
                artifact_key: entry.artifact_key.clone(),
                first_path: first_path.to_string(),
                duplicate_path: entry.literal_path.clone(),
            });
        }
    }
    Ok(())
}

fn validate_logical_relative_path_bytes(bytes: &[u8]) -> Result<(), ScopedKeyError> {
    if bytes.is_empty() {
        return Err(ScopedKeyError::EmptyRelativePath);
    }
    let logical =
        std::str::from_utf8(bytes).map_err(|_| ScopedKeyError::RelativePathHasBackslash)?;
    if logical.ends_with('/') {
        return Err(ScopedKeyError::RelativePathHasTrailingSeparator);
    }
    if logical.contains('\\') {
        return Err(ScopedKeyError::RelativePathHasBackslash);
    }
    if logical
        .split('/')
        .any(|component| component.is_empty() || component == "." || component == "..")
    {
        return Err(ScopedKeyError::RelativePathHasDotComponent);
    }
    Ok(())
}

fn unicode_path(path: &Path) -> Result<String, ScopedKeyError> {
    path.to_str()
        .map(str::to_string)
        .ok_or_else(|| ScopedKeyError::NonUnicodePath(path.to_path_buf()))
}

fn find_git_toplevel(path: &Path) -> Result<Option<PathBuf>, ScopedKeyError> {
    let output = Command::new("git")
        .arg("-C")
        .arg(path)
        .args(["rev-parse", "--show-toplevel"])
        .output()
        .map_err(|error| ScopedKeyError::GitProbe {
            path: path.to_path_buf(),
            detail: format!("spawn failed: {error}"),
        })?;
    if output.status.success() {
        let output = String::from_utf8(output.stdout).map_err(|_| ScopedKeyError::GitProbe {
            path: path.to_path_buf(),
            detail: "git returned a non-Unicode toplevel".to_string(),
        })?;
        let toplevel = output.trim_end_matches(['\r', '\n']);
        return std::fs::canonicalize(toplevel).map(Some).map_err(|error| {
            ScopedKeyError::GitProbe {
                path: path.to_path_buf(),
                detail: error.to_string(),
            }
        });
    }

    let stderr = String::from_utf8_lossy(&output.stderr);
    if stderr.contains("not a git repository") {
        return Ok(None);
    }
    Err(ScopedKeyError::GitProbe {
        path: path.to_path_buf(),
        detail: format!("exit {:?}: {}", output.status.code(), stderr.trim()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scoped_v1_has_domain_separation_and_stable_bytes() {
        let scoped = scoped_v1_key("root-a\nroot-b", "src/é.rs".as_bytes()).unwrap();
        assert_eq!(
            scoped,
            "11f4480f5f5374290464d5a82c2e08153ca75de53b5e71a9c3c6e645b57cd4a4"
        );
        assert_ne!(
            scoped,
            crate::search_index::artifact_key_from_git_identity("root-a\nroot-b")
        );
    }

    #[test]
    fn scoped_v1_preserves_unicode_case_and_never_normalizes() {
        let composed = scoped_v1_key("roots", "Src/é.rs".as_bytes()).unwrap();
        let decomposed = scoped_v1_key("roots", "Src/e\u{301}.rs".as_bytes()).unwrap();
        let lower = scoped_v1_key("roots", "src/é.rs".as_bytes()).unwrap();
        assert_ne!(composed, decomposed);
        assert_ne!(composed, lower);
    }

    #[test]
    fn scoped_v1_rejects_unsafe_logical_paths() {
        for path in ["", "src/", "src\\lib.rs", "./src", "src/../lib"] {
            assert!(
                scoped_v1_key("roots", path.as_bytes()).is_err(),
                "accepted {path:?}"
            );
        }
    }

    #[test]
    fn relative_path_uses_forward_slashes_without_case_folding() {
        let relative =
            scoped_relative_path(Path::new("/repo/Src/Lib"), Path::new("/repo")).unwrap();
        assert_eq!(relative, "Src/Lib");
    }

    #[cfg(windows)]
    #[test]
    fn windows_native_separators_match_logical_forward_slashes() {
        let relative =
            scoped_relative_path(Path::new(r"C:\repo\src\lib"), Path::new(r"C:\repo")).unwrap();
        assert_eq!(relative, "src/lib");
        assert_eq!(
            scoped_v1_key("roots", relative.as_bytes()).unwrap(),
            scoped_v1_key("roots", b"src/lib").unwrap()
        );
    }

    #[test]
    fn subtree_key_never_equals_or_prewarms_the_repository_session_key() {
        let Some(repo) = initialized_git_repo() else {
            eprintln!("skipping: git is not available");
            return;
        };
        let root = std::fs::canonicalize(repo.path()).unwrap();
        let subtree = root.join("src");
        assert!(crate::search_index::artifact_cache_key_memoized_only(&root).is_none());

        let subtree_identity = classify_resolved_standing_root(&subtree, Some(&root)).unwrap();
        assert!(matches!(
            subtree_identity,
            StandingArtifactIdentity::GitSubtree { .. }
        ));
        assert!(
            crate::search_index::artifact_cache_key_memoized_only(&root).is_none(),
            "subtree derivation must not pre-warm the repository session key"
        );

        let session_key = crate::search_index::artifact_cache_key(&root);
        assert_ne!(subtree_identity.artifact_key(), session_key);
        let top_level = classify_resolved_standing_root(&root, Some(&root)).unwrap();
        assert_eq!(top_level.artifact_key(), session_key);
    }

    #[test]
    fn non_git_roots_use_the_existing_path_scope_key() {
        let root = tempfile::tempdir().unwrap();
        let identity = classify_resolved_standing_root(root.path(), None).unwrap();
        assert_eq!(
            identity.artifact_key(),
            crate::search_index::artifact_path_identity_key(root.path())
        );
    }

    #[test]
    fn same_repo_worktree_and_logical_subtree_share_scoped_v1_key() {
        let Some(repo) = initialized_git_repo() else {
            eprintln!("skipping: git is not available");
            return;
        };
        let worktree = repo.path().join("linked-worktree");
        let status = Command::new("git")
            .arg("-C")
            .arg(repo.path())
            .args(["worktree", "add", "--detach"])
            .arg(&worktree)
            .status();
        let Ok(status) = status else {
            eprintln!("skipping: git worktree is unavailable");
            return;
        };
        if !status.success() {
            eprintln!("skipping: git worktree add failed");
            return;
        }

        let main_root = std::fs::canonicalize(repo.path()).unwrap();
        let linked_root = std::fs::canonicalize(&worktree).unwrap();
        let main =
            classify_resolved_standing_root(&main_root.join("src"), Some(&main_root)).unwrap();
        let linked =
            classify_resolved_standing_root(&linked_root.join("src"), Some(&linked_root)).unwrap();
        assert_eq!(main.artifact_key(), linked.artifact_key());
    }

    #[cfg(unix)]
    #[test]
    fn symlink_retargeting_resolves_to_a_different_pinned_identity() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().unwrap();
        let first = temp.path().join("first");
        let second = temp.path().join("second");
        std::fs::create_dir_all(&first).unwrap();
        std::fs::create_dir_all(&second).unwrap();
        let link = temp.path().join("root");
        symlink(&first, &link).unwrap();
        let original = resolve_standing_root(link.to_str().unwrap()).unwrap();
        std::fs::remove_file(&link).unwrap();
        symlink(&second, &link).unwrap();
        let retargeted = resolve_standing_root(link.to_str().unwrap()).unwrap();
        assert_ne!(original.resolved_target, retargeted.resolved_target);
    }

    #[test]
    fn duplicate_artifact_keys_are_refused_before_admission() {
        let entries = vec![
            ResolvedStandingRoot {
                literal_path: "/one".to_string(),
                resolved_target: "/one".to_string(),
                resolved_git_toplevel: None,
                scoped_relative_path: None,
                artifact_key: "same".to_string(),
            },
            ResolvedStandingRoot {
                literal_path: "/two".to_string(),
                resolved_target: "/two".to_string(),
                resolved_git_toplevel: None,
                scoped_relative_path: None,
                artifact_key: "same".to_string(),
            },
        ];
        assert!(matches!(
            reject_duplicate_artifact_keys(&entries),
            Err(ScopedKeyError::DuplicateArtifactKey { .. })
        ));
    }

    fn initialized_git_repo() -> Option<tempfile::TempDir> {
        let repo = tempfile::tempdir().ok()?;
        let run = |args: &[&str]| {
            Command::new("git")
                .arg("-C")
                .arg(repo.path())
                .args(args)
                .status()
                .ok()
                .is_some_and(|status| status.success())
        };
        if !run(&["init", "-q"]) {
            return None;
        }
        std::fs::create_dir_all(repo.path().join("src")).ok()?;
        std::fs::write(repo.path().join("src/lib.rs"), "pub fn f() {}\n").ok()?;
        if !run(&["add", "."])
            || !run(&[
                "-c",
                "user.name=AFT test",
                "-c",
                "user.email=aft@example.test",
                "commit",
                "-qm",
                "init",
            ])
        {
            return None;
        }
        Some(repo)
    }
}
