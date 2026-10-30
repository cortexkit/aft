//! Bash permission scanner for hoisted bash.
//!
//! Ports OpenCode's tree-sitter-based permission scan that walks the parsed
//! command tree to identify sub-commands that touch external directories or
//! match permission rules.

pub mod arity;
pub mod scan;
pub(crate) mod synthetic_load;

use crate::context::AppContext;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PermissionAsk {
    pub kind: PermissionKind,
    pub patterns: Vec<String>,
    pub always: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// Actual command targets, before an optional parent-folder scope is
    /// offered. Retry coverage must not depend on whether that probe succeeded.
    #[serde(skip)]
    pub targets: Vec<crate::access_policy::AccessGrant>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum PermissionKind {
    #[serde(rename = "external_directory")]
    ExternalDirectory,
    #[serde(rename = "bash")]
    Bash,
}

/// Scan a bash command and return the list of permission asks needed.
pub fn scan(command: &str, ctx: &AppContext) -> Vec<PermissionAsk> {
    scan::scan(command, ctx)
}

/// Returns true for scratch paths that should not create external-directory asks.
/// This only affects prompt decisions; hard project-root validation stays separate.
pub(crate) fn is_system_temp_path(path: &Path) -> bool {
    let resolved = resolve_with_existing_ancestors(path);
    system_temp_roots().into_iter().any(|root| {
        let root = normalize_path(&root);
        path_is_under(&root, &resolved)
    })
}

pub(crate) fn system_temp_roots() -> Vec<PathBuf> {
    let mut roots = Vec::new();
    roots.push(std::env::temp_dir());

    #[cfg(unix)]
    roots.extend([
        PathBuf::from("/tmp"),
        PathBuf::from("/var/tmp"),
        PathBuf::from("/private/tmp"),
        PathBuf::from("/private/var/tmp"),
    ]);

    #[cfg(target_os = "macos")]
    roots.extend([
        PathBuf::from("/var/folders"),
        PathBuf::from("/private/var/folders"),
    ]);

    let mut expanded = Vec::with_capacity(roots.len() * 2);
    for root in roots {
        expanded.push(root.clone());
        if let Ok(canonical) = std::fs::canonicalize(&root) {
            expanded.push(canonical);
        }
    }
    expanded
}

fn path_is_under(root: &Path, path: &Path) -> bool {
    path == root || path.starts_with(root)
}

fn resolve_with_existing_ancestors(path: &Path) -> PathBuf {
    if let Ok(canonical) = std::fs::canonicalize(path) {
        return normalize_path(&canonical);
    }

    let normalized = normalize_path(path);
    if !normalized.is_absolute() {
        return normalized;
    }

    let mut missing: Vec<PathBuf> = Vec::new();
    let mut current = normalized.clone();
    loop {
        if let Ok(real_parent) = std::fs::canonicalize(&current) {
            let mut resolved = real_parent;
            for part in missing.iter().rev() {
                resolved.push(part);
            }
            return normalize_path(&resolved);
        }

        let Some(parent) = current.parent() else {
            return normalized;
        };
        if parent == current {
            return normalized;
        }
        if let Some(name) = current.file_name() {
            missing.push(PathBuf::from(name));
        }
        current = parent.to_path_buf();
    }
}

fn normalize_path(path: &Path) -> PathBuf {
    crate::path_identity::lexical_normalize(path)
}

#[cfg(test)]
mod system_temp_path_tests {
    use super::is_system_temp_path;
    use std::path::Path;

    /// Unmatched leading `..` components must survive comparison normalization:
    /// dropping them makes a path that escapes upward look like a nearby child.
    #[test]
    fn comparison_adapter_preserves_relative_parent_components() {
        for (input, expected) in [("../../x", "../../x"), ("a/../../x", "../x")] {
            assert_eq!(
                super::normalize_path(std::path::Path::new(input)),
                std::path::PathBuf::from(expected),
                "{input}"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn unix_temp_roots_match_by_component() {
        for path in [
            "/tmp",
            "/tmp/aft-file.txt",
            "/var/tmp/aft-file.txt",
            "/private/tmp/aft-file.txt",
            "/private/var/tmp/aft-file.txt",
        ] {
            assert!(
                is_system_temp_path(Path::new(path)),
                "{path} should be temp"
            );
        }

        for path in ["/tmpfoo/x", "/Users/x", "/home/user/tmp/x"] {
            assert!(
                !is_system_temp_path(Path::new(path)),
                "{path} should not be temp"
            );
        }
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_var_folders_matches_tmpdir_tree() {
        for path in [
            "/var/folders/zz/aft/T/file.txt",
            "/private/var/folders/zz/aft/T/file.txt",
        ] {
            assert!(
                is_system_temp_path(Path::new(path)),
                "{path} should be temp"
            );
        }
    }

    #[test]
    fn process_temp_dir_and_children_are_exempt() {
        let temp_dir = std::env::temp_dir();
        assert!(is_system_temp_path(&temp_dir));
        assert!(is_system_temp_path(
            &temp_dir.join("aft-temp-exemption").join("file.txt")
        ));
    }

    #[test]
    fn relative_paths_are_not_temp_paths() {
        for path in ["tmp/x", "./tmp/x", "project/tmp/x"] {
            assert!(
                !is_system_temp_path(Path::new(path)),
                "{path} should not be temp"
            );
        }
    }

    #[cfg(windows)]
    #[test]
    fn windows_temp_dir_shape_is_exempt() {
        let temp_dir = std::env::temp_dir();
        assert!(is_system_temp_path(&temp_dir));
        assert!(is_system_temp_path(
            &temp_dir.join("aft-temp-exemption").join("file.txt")
        ));
    }
}
