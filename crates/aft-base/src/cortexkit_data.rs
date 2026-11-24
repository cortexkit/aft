//! Path-only rules shared by file tools and project-content admission.

use std::path::Path;

pub const HOME_DATA_DIR: &str = ".local/share/cortexkit";
pub const WORKTREES: &str = "alfonso/worktrees";

#[derive(Clone, Copy)]
pub enum Floor {
    Closed(&'static str),
    ReadOnly(&'static str),
    Database,
    Credential,
}

/// Classify an entry under one resolved data root. The worktrees store project
/// code, so their filenames are subject only to the surrounding project rules.
pub fn floor(root: &Path, path: &Path, write: bool) -> Option<Floor> {
    use crate::policy_path::contains_resolved;

    if !contains_resolved(path, root) {
        return None;
    }
    let module = path.components().nth(root.components().count())?;
    let module = module.as_os_str().to_str().unwrap_or_default();
    let same = |name: &str| {
        if cfg!(any(windows, target_os = "macos")) {
            module.eq_ignore_ascii_case(name)
        } else {
            module == name
        }
    };
    if same("alfonso") && contains_resolved(path, &root.join(WORKTREES)) {
        return None;
    }
    if let Some(folder) = ["run", "signing"].into_iter().find(|folder| same(folder)) {
        return Some(Floor::Closed(folder));
    }

    let name = path.file_name()?.to_string_lossy();
    let folded = name.to_ascii_lowercase();
    let database_name = if cfg!(any(windows, target_os = "macos")) {
        &folded
    } else {
        name.as_ref()
    };
    let stem = ["-wal", "-shm", "-journal"]
        .into_iter()
        .find_map(|suffix| database_name.strip_suffix(suffix))
        .unwrap_or(database_name);
    if [".db", ".sqlite", ".sqlite3", ".db3"]
        .iter()
        .any(|ext| stem.ends_with(ext))
        || [".db.", ".sqlite.", ".sqlite3."]
            .iter()
            .any(|infix| stem.contains(infix))
    {
        return Some(Floor::Database);
    }
    if ["token", "secret", "credential"]
        .iter()
        .any(|word| folded.contains(word))
        || [".pem", ".p12", ".key", ".jwt", ".nk", ".creds"]
            .iter()
            .any(|ext| folded.ends_with(ext))
    {
        return Some(Floor::Credential);
    }

    if write {
        return ["nats", "nats-hub", "ckbus"]
            .into_iter()
            .find(|folder| same(folder))
            .map(Floor::ReadOnly);
    }
    None
}
