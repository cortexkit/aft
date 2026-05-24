//! Trust state for project-supplied TOML compression filters.
//!
//! Project filters at `<project>/.aft/filters/*.toml` are off by default
//! because a malicious repository could ship a filter that lies about output
//! (e.g. strips real test failures and replaces them with `tests: ok`).
//!
//! Trust is keyed by canonicalized project root path. Users opt in via
//! `aft doctor filters trust` (CLI) which calls into [`trust_project`] /
//! [`untrust_project`].
//!
//! The trust file lives at `<storage_dir>/trusted-filter-projects.json` so
//! it survives across bridge restarts and OpenCode restarts.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

const TRUST_FILE: &str = "trusted-filter-projects.json";

#[derive(Debug, Default, Serialize, Deserialize)]
struct TrustState {
    /// Canonicalized absolute project root paths the user has explicitly
    /// trusted to load `.aft/filters/*.toml`.
    trusted_projects: BTreeSet<String>,
}

/// Returns true when the project root is in the trusted set.
///
/// Returns false (untrusted) on any error — fail-closed by design.
pub fn is_project_trusted(storage_dir: Option<&Path>, project_root: &Path) -> bool {
    let Some(storage_dir) = storage_dir else {
        return false;
    };
    let Ok(canonical) = project_root.canonicalize() else {
        return false;
    };
    let Ok(state) = load(storage_dir) else {
        return false;
    };
    state
        .trusted_projects
        .contains(&canonical.display().to_string())
}

/// Add a project to the trusted set. Returns Ok if the file was updated (or
/// already trusted).
pub fn trust_project(storage_dir: &Path, project_root: &Path) -> Result<(), String> {
    let canonical = project_root
        .canonicalize()
        .map_err(|e| format!("canonicalize {}: {e}", project_root.display()))?;
    let mut state = load(storage_dir).unwrap_or_default();
    state
        .trusted_projects
        .insert(canonical.display().to_string());
    save(storage_dir, &state)
}

/// Remove a project from the trusted set. No-op if the project wasn't trusted.
pub fn untrust_project(storage_dir: &Path, project_root: &Path) -> Result<(), String> {
    let canonical = project_root
        .canonicalize()
        .map_err(|e| format!("canonicalize {}: {e}", project_root.display()))?;
    let mut state = load(storage_dir).unwrap_or_default();
    state
        .trusted_projects
        .remove(&canonical.display().to_string());
    save(storage_dir, &state)
}

/// List all trusted project paths (for `aft doctor filters trust --list`).
pub fn list_trusted(storage_dir: &Path) -> Vec<String> {
    load(storage_dir)
        .map(|state| state.trusted_projects.into_iter().collect())
        .unwrap_or_default()
}

fn trust_path(storage_dir: &Path) -> PathBuf {
    storage_dir.join(TRUST_FILE)
}

fn load(storage_dir: &Path) -> Result<TrustState, String> {
    let path = trust_path(storage_dir);
    let bytes = match fs::read(&path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok(TrustState::default());
        }
        Err(e) => return Err(format!("read {}: {e}", path.display())),
    };
    serde_json::from_slice(&bytes).map_err(|e| format!("parse {}: {e}", path.display()))
}

fn save(storage_dir: &Path, state: &TrustState) -> Result<(), String> {
    fs::create_dir_all(storage_dir)
        .map_err(|e| format!("create_dir_all {}: {e}", storage_dir.display()))?;
    let path = trust_path(storage_dir);
    let tmp_path = path.with_extension("json.tmp");
    let bytes =
        serde_json::to_vec_pretty(state).map_err(|e| format!("serialize trust state: {e}"))?;
    fs::write(&tmp_path, &bytes).map_err(|e| format!("write {}: {e}", tmp_path.display()))?;
    fs::rename(&tmp_path, &path)
        .map_err(|e| format!("rename {} → {}: {e}", tmp_path.display(), path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn untrusted_by_default() {
        let storage = tempdir().unwrap();
        let project = tempdir().unwrap();
        assert!(!is_project_trusted(Some(storage.path()), project.path()));
    }

    #[test]
    fn trust_then_check() {
        let storage = tempdir().unwrap();
        let project = tempdir().unwrap();
        trust_project(storage.path(), project.path()).unwrap();
        assert!(is_project_trusted(Some(storage.path()), project.path()));
    }

    #[test]
    fn untrust_removes_from_set() {
        let storage = tempdir().unwrap();
        let project = tempdir().unwrap();
        trust_project(storage.path(), project.path()).unwrap();
        untrust_project(storage.path(), project.path()).unwrap();
        assert!(!is_project_trusted(Some(storage.path()), project.path()));
    }

    #[test]
    fn list_returns_trusted_projects() {
        let storage = tempdir().unwrap();
        let p1 = tempdir().unwrap();
        let p2 = tempdir().unwrap();
        trust_project(storage.path(), p1.path()).unwrap();
        trust_project(storage.path(), p2.path()).unwrap();
        let listed = list_trusted(storage.path());
        assert_eq!(listed.len(), 2);
    }

    #[test]
    fn no_storage_dir_means_untrusted() {
        let project = tempdir().unwrap();
        assert!(!is_project_trusted(None, project.path()));
    }

    #[test]
    fn missing_trust_file_is_untrusted_not_error() {
        let storage = tempdir().unwrap();
        let project = tempdir().unwrap();
        // No prior trust calls; file doesn't exist.
        assert!(!is_project_trusted(Some(storage.path()), project.path()));
    }

    #[test]
    fn corrupt_trust_file_is_untrusted() {
        let storage = tempdir().unwrap();
        fs::write(trust_path(storage.path()), b"not valid json").unwrap();
        let project = tempdir().unwrap();
        assert!(!is_project_trusted(Some(storage.path()), project.path()));
    }
}
