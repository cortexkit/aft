//! Private build scratch directories, registered under the family writer lock.
//! Their own OS lock lets resolution continue without holding that writer lock.

use std::fs::{self, File};
use std::io;
use std::path::{Path, PathBuf};

const PREFIX: &str = "graph-build-";

#[derive(Debug)]
pub struct BuildWorkspace {
    lock: Option<File>,
    path: PathBuf,
}

impl BuildWorkspace {
    pub(super) fn create(root: &Path) -> io::Result<Self> {
        let path = root.join(format!("{PREFIX}{}", super::lease::fresh_id()));
        let builder = fs::DirBuilder::new();
        #[cfg(unix)]
        let builder = {
            use std::os::unix::fs::DirBuilderExt;
            let mut builder = builder;
            builder.mode(0o700);
            builder
        };
        builder.create(&path)?;
        let lock = crate::fs::create_new_private(&path.join("owner"))?;
        lock.lock()?;
        Ok(Self {
            lock: Some(lock),
            path,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for BuildWorkspace {
    fn drop(&mut self) {
        self.lock = None;
        let _ = fs::remove_dir_all(&self.path);
    }
}

/// Called only under the family writer lock, including during registration.
pub(super) fn sweep(path: &Path) -> io::Result<bool> {
    if !path
        .file_name()
        .is_some_and(|name| name.to_string_lossy().starts_with(PREFIX))
        || !fs::symlink_metadata(path)?.is_dir()
    {
        return Ok(false);
    }
    let owner = path.join("owner");
    let lock = match fs::OpenOptions::new().read(true).write(true).open(owner) {
        Ok(file) => Some(file),
        Err(e) if e.kind() == io::ErrorKind::NotFound => None,
        Err(e) => return Err(e),
    };
    if let Some(lock) = &lock {
        match lock.try_lock() {
            Ok(()) => {}
            Err(fs::TryLockError::WouldBlock) => return Ok(true),
            Err(fs::TryLockError::Error(e)) => return Err(e),
        }
    }
    drop(lock);
    fs::remove_dir_all(path)?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_workspace_sweeps_simulated_kill_but_preserves_live_builder() {
        let dir = tempfile::tempdir().unwrap();
        let family = crate::family::Family::open(
            dir.path(),
            crate::FamilyId::from_digest(crate::Digest128::of(b"workspace")),
        )
        .unwrap();
        let mut scratch = family
            .try_write()
            .unwrap()
            .unwrap()
            .create_build_workspace()
            .unwrap();
        let path = scratch.path().to_path_buf();
        fs::write(path.join("partial-fragment"), b"partial").unwrap();
        drop(family.try_write().unwrap().unwrap());
        assert!(path.join("partial-fragment").exists());
        // A killed process closes its lock without running directory cleanup.
        scratch.lock = None;
        drop(family.try_write().unwrap().unwrap());
        assert!(!path.exists());
    }
}
