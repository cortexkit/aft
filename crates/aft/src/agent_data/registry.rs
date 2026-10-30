use std::path::{Component, Path, PathBuf};

use cortexkit_project_identity::{EnumerateReply, Presence, ResolveReply, RootKey, RootRecord};
use serde::{Deserialize, Serialize};

/// The registry, not a Git remote or a pathname suffix, assigns these keys.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PortablePath {
    pub project_id: String,
    pub root_key: RootKey,
    pub relative_path: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PathDrop {
    MachineLocalPath,
    RootUnknown,
    AmbiguousRoot,
    UnsafePath,
}

impl PathDrop {
    pub fn reason(self) -> &'static str {
        match self {
            Self::MachineLocalPath => "machine_local_path",
            Self::RootUnknown => "root_unknown",
            Self::AmbiguousRoot => "ambiguous_root",
            Self::UnsafePath => "unsafe_path",
        }
    }
}

fn safe_relative(path: &str) -> bool {
    !path.is_empty()
        && !path.contains(['\\', ':', '\0'])
        && path
            .split('/')
            .all(|part| !part.is_empty() && part != "." && part != "..")
        && Path::new(path)
            .components()
            .all(|part| matches!(part, Component::Normal(_)))
}

fn no_links(root: &Path, relative: &str) -> Result<PathBuf, PathDrop> {
    if !safe_relative(relative) || !root.is_absolute() {
        return Err(PathDrop::UnsafePath);
    }
    let canonical = root.canonicalize().map_err(|_| PathDrop::RootUnknown)?;
    if canonical != root {
        return Err(PathDrop::UnsafePath);
    }
    let mut path = root.to_path_buf();
    for part in relative.split('/') {
        path.push(part);
        match std::fs::symlink_metadata(&path) {
            Ok(meta) if meta.file_type().is_symlink() => return Err(PathDrop::UnsafePath),
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => return Err(PathDrop::UnsafePath),
        }
    }
    Ok(path)
}

fn usable(record: &RootRecord) -> bool {
    record.root_key.is_some()
        && record.root_key_mismatch != Some(true)
        && matches!(
            record.identity,
            cortexkit_project_identity::RootIdentity::Bound
                | cortexkit_project_identity::RootIdentity::Unbound
        )
}

pub fn export_path(path: &Path, reply: &ResolveReply) -> Result<PortablePath, PathDrop> {
    if reply.gone || reply.project_id.is_empty() {
        return Err(PathDrop::MachineLocalPath);
    }
    let Presence::Present(records) = &reply.root_records else {
        return Err(PathDrop::MachineLocalPath);
    };
    let mut selected = None;
    for record in records.iter().filter(|record| usable(record)) {
        let root = Path::new(&record.root);
        let Ok(relative) = path.strip_prefix(root) else {
            continue;
        };
        let Some(relative) = relative.to_str() else {
            return Err(PathDrop::UnsafePath);
        };
        let relative = relative.replace(std::path::MAIN_SEPARATOR, "/");
        no_links(root, &relative)?;
        if selected.is_some() {
            return Err(PathDrop::AmbiguousRoot);
        }
        selected = Some(PortablePath {
            project_id: reply.project_id.clone(),
            root_key: record.root_key.clone().ok_or(PathDrop::MachineLocalPath)?,
            relative_path: relative,
        });
    }
    selected.ok_or(PathDrop::MachineLocalPath)
}

pub fn import_path(key: &PortablePath, reply: &EnumerateReply) -> Result<PathBuf, PathDrop> {
    if !safe_relative(&key.relative_path) {
        return Err(PathDrop::UnsafePath);
    }
    let mut only = None;
    let mut containing = None;
    let mut root_count = 0;
    for project in reply
        .projects
        .iter()
        .filter(|project| project.project_id == key.project_id)
    {
        for record in project
            .root_records
            .iter()
            .flatten()
            .filter(|record| usable(record) && record.root_key.as_ref() == Some(&key.root_key))
        {
            let path = no_links(Path::new(&record.root), &key.relative_path)?;
            root_count += 1;
            only = Some(path.clone());
            if path.try_exists().map_err(|_| PathDrop::UnsafePath)? {
                if containing.is_some() {
                    return Err(PathDrop::AmbiguousRoot);
                }
                containing = Some(path);
            }
        }
    }
    containing
        .or_else(|| (root_count == 1).then_some(only).flatten())
        .ok_or(PathDrop::RootUnknown)
}
