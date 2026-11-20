use super::{
    file_type, fstat_fd, open_directory_no_symlinks, open_relative_no_symlinks, OpenedRulePath,
};
use crate::cli::sandbox_launch::{paths_overlap, REMOTE_PROTECTED_GRANT};
use aft::sandbox_profile::SandboxProfile;
use std::os::unix::fs::MetadataExt;
use std::{
    collections::HashSet,
    ffi::OsStr,
    fs, io,
    os::fd::AsRawFd,
    path::{Path, PathBuf},
};

const MAX_ENTRIES: usize = 4096;

pub(super) fn prepare(
    profile: &SandboxProfile,
    folder: &Path,
) -> Result<Vec<(PathBuf, OpenedRulePath)>, String> {
    let controls = protected_paths(&profile.write_deny_nested)?;
    if profile
        .write_allow_roots()
        .iter()
        .any(|root| controls.iter().any(|deny| paths_overlap(root, deny)))
    {
        return Err(REMOTE_PROTECTED_GRANT.into());
    }
    let root = open_directory_no_symlinks(folder).map_err(|error| error.to_string())?;
    let mut grants = Vec::new();
    for (index, entry) in directory_entries(root.as_raw_fd())
        .map_err(|error| error.to_string())?
        .enumerate()
    {
        if index == MAX_ENTRIES {
            return Err(REMOTE_PROTECTED_GRANT.into());
        }
        let name = entry.map_err(|error| error.to_string())?.file_name();
        if name == OsStr::new(".git") || name == OsStr::new(".cortexkit") {
            continue;
        }
        let path = folder.join(&name);
        if controls.iter().any(|deny| paths_overlap(&path, deny)) {
            continue;
        }
        let fd = match open_relative_no_symlinks(root.as_raw_fd(), &name, false) {
            Ok(fd) => fd,
            Err(error)
                if error.kind() == io::ErrorKind::NotFound
                    || error.raw_os_error() == Some(libc::ELOOP) =>
            {
                continue
            }
            Err(error) => return Err(error.to_string()),
        };
        let stat = fstat_fd(&fd).map_err(|error| error.to_string())?;
        let is_dir = file_type(&stat) == libc::S_IFDIR;
        if !is_dir && !(file_type(&stat) == libc::S_IFREG && stat.st_nlink == 1) {
            continue;
        }
        if profile
            .write_deny
            .iter()
            .any(|deny| paths_overlap(&path, deny))
        {
            return Err(REMOTE_PROTECTED_GRANT.into());
        }
        grants.push((path, OpenedRulePath { fd, is_dir }));
    }
    profile
        .data_policy
        .validate_grants(
            grants.iter().map(|(path, _)| path.as_path()),
            &profile.data_policy.write_allow,
            &profile.writable_files,
        )
        .map_err(|error| error.to_string())?;
    Ok(grants)
}

fn directory_entries(fd: std::os::fd::RawFd) -> io::Result<fs::ReadDir> {
    // Enumerate the pinned directory. Child opens use the same descriptor and
    // O_NOFOLLOW; a swapped symlink cannot turn into a grant on its target.
    fs::read_dir(format!("/proc/self/fd/{fd}"))
}

fn protected_paths(denies: &[PathBuf]) -> Result<HashSet<PathBuf>, String> {
    let mut protected = HashSet::new();
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
                aft::sandbox_profile::resolve_env_deny_paths(path, "write_deny_nested")
                    .map_err(|error| error.to_string())?,
            );
        } else if metadata.is_file() && metadata.nlink() != 1 {
            // An alias can be anywhere under a writable child. Checking the
            // protected inodes avoids walking unrelated project subtrees.
            // Privileged bind mounts are outside this filesystem threat model.
            return Err(REMOTE_PROTECTED_GRANT.into());
        } else if metadata.is_dir() {
            let dir = open_directory_no_symlinks(&path).map_err(|error| error.to_string())?;
            for entry in directory_entries(dir.as_raw_fd()).map_err(|error| error.to_string())? {
                if pending.len() >= budget {
                    return Err(REMOTE_PROTECTED_GRANT.into());
                }
                pending.push(path.join(entry.map_err(|error| error.to_string())?.file_name()));
            }
        }
    }
    Ok(protected)
}
