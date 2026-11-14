//! Temporary adapter until graph and text builders share the store scratch API.
//! A registry lock protects directory registration; OS owner locks, not age or
//! process IDs, distinguish an abandoned build from an active one.

use std::fs::{self, File};
use std::io;
use std::path::{Path, PathBuf};

/// The graph writer only needs a private directory and its lifetime guard.
pub trait Scratch: std::fmt::Debug + Send {
    fn path(&self) -> &Path;
}

#[derive(Debug)]
pub struct LocalScratch {
    owner: Option<File>,
    _checkout: File,
    path: PathBuf,
}

fn directory(path: &Path) -> io::Result<()> {
    let mut builder = fs::DirBuilder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    match builder.create(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists && path.is_dir() => Ok(()),
        Err(e) => Err(e),
    }
}

fn lock_file(path: &Path) -> io::Result<File> {
    let mut options = fs::OpenOptions::new();
    options.read(true).write(true).create(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path)
}

fn registry(store: &Path) -> io::Result<(PathBuf, File)> {
    let root = store.join("graph-build-scratch");
    directory(&root)?;
    let registry = lock_file(&root.join("registry.lock"))?;
    registry.lock()?;
    Ok((root, registry))
}

fn sweep_locked(root: &Path) -> io::Result<()> {
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        if !entry.file_name().to_string_lossy().starts_with("build-")
            || !entry.file_type()?.is_dir()
        {
            continue;
        }
        let owner = entry.path().join("owner");
        let file = match fs::OpenOptions::new().read(true).write(true).open(&owner) {
            Ok(file) => Some(file),
            Err(e) if e.kind() == io::ErrorKind::NotFound => None,
            Err(e) => return Err(e),
        };
        if let Some(file) = &file {
            match file.try_lock() {
                Ok(()) => {}
                Err(fs::TryLockError::WouldBlock) => continue,
                Err(fs::TryLockError::Error(e)) => return Err(e),
            }
        }
        drop(file);
        match fs::remove_dir_all(entry.path()) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

impl LocalScratch {
    pub fn sweep(store: &Path) -> io::Result<()> {
        let (root, _registry) = registry(store)?;
        sweep_locked(&root)
    }

    pub fn create(store: &Path, checkout: &str) -> io::Result<Self> {
        let (root, _registry) = registry(store)?;
        sweep_locked(&root)?;
        // Checkout IDs are hashed rather than trusted as path components.
        let identity = crate::Digest128::of(checkout.as_bytes());
        let checkout = lock_file(&root.join(format!("checkout-{identity}.lock")))?;
        checkout.try_lock().map_err(|e| match e {
            fs::TryLockError::WouldBlock => io::Error::new(
                io::ErrorKind::WouldBlock,
                "a graph build already owns this checkout's scratch",
            ),
            fs::TryLockError::Error(e) => e,
        })?;
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(io::Error::other)?
            .as_nanos();
        let path = root.join(format!("build-{identity}-{}-{nonce}", std::process::id()));
        directory(&path)?;
        let mut scratch = Self {
            owner: None,
            _checkout: checkout,
            path,
        };
        let file = crate::fs::create_new_private(&scratch.path.join("owner"))?;
        file.lock()?;
        scratch.owner = Some(file);
        Ok(scratch)
    }
}

impl Scratch for LocalScratch {
    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for LocalScratch {
    fn drop(&mut self) {
        self.owner = None;
        let _ = fs::remove_dir_all(&self.path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn scratch_sweeps_simulated_kill_and_preserves_live_build() {
        let root = tempfile::tempdir().unwrap();
        let mut scratch = LocalScratch::create(root.path(), "checkout").unwrap();
        let path = scratch.path.clone();
        fs::write(path.join("partial"), b"partial").unwrap();
        LocalScratch::sweep(root.path()).unwrap();
        assert!(path.exists());
        assert!(LocalScratch::create(root.path(), "checkout").is_err());
        scratch.owner = None;
        LocalScratch::sweep(root.path()).unwrap();
        assert!(!path.exists());
    }
}
