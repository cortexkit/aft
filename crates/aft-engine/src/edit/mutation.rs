//! In-process compare-and-write admission shared by every source mutation.

use crate::error::AftError;
use crate::file_identity::FileIdentity;
use parking_lot::{Condvar, Mutex};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum Key {
    Path(PathBuf),
    File(FileIdentity),
    Entry(FileIdentity, std::ffi::OsString),
}

static HELD: LazyLock<(Mutex<HashMap<Key, usize>>, Condvar)> =
    LazyLock::new(|| (Mutex::new(HashMap::new()), Condvar::new()));

/// Owns admission, not a mutex guard: no registry mutex is held during I/O.
pub(crate) struct FileLock(Vec<Key>);

impl Drop for FileLock {
    fn drop(&mut self) {
        let (held, wake) = &*HELD;
        let mut held = held.lock();
        for key in &self.0 {
            let count = held.get_mut(key).expect("file admission is held");
            *count -= 1;
            if *count == 0 {
                held.remove(key);
            }
        }
        wake.notify_all();
    }
}

fn resolved(path: &Path) -> PathBuf {
    if let Ok(path) = std::fs::canonicalize(path) {
        return path;
    }
    if let (Some(parent), Some(leaf)) = (path.parent(), path.file_name()) {
        return resolved(parent).join(leaf);
    }
    crate::path_identity::lexical_normalize(path)
}

/// Admit a sorted set in one registry turn, never holding one file while
/// waiting for another. Native identities also serialize hard-link aliases;
/// resolved names remain stable when an atomic rename changes the inode.
/// Parent identities protect new names reached through directory aliases too.
pub(crate) fn lock_files(paths: &[&Path]) -> Vec<FileLock> {
    let mut entries: Vec<_> = paths
        .iter()
        .enumerate()
        .map(|(index, path)| (resolved(path), *path, index))
        .collect();
    entries.sort_by(|a, b| a.0.cmp(&b.0));
    let keys: Vec<Vec<Key>> = entries
        .iter()
        .map(|(name, path, _)| {
            let mut keys = vec![Key::Path(name.clone())];
            if let (Some(parent), Some(leaf)) = (name.parent(), name.file_name()) {
                if let Ok((_, identity)) = crate::file_identity::metadata(parent) {
                    if identity != FileIdentity::Unavailable {
                        #[cfg(windows)]
                        let leaf = std::ffi::OsString::from(leaf.to_string_lossy().to_lowercase());
                        #[cfg(not(windows))]
                        let leaf = leaf.to_os_string();
                        keys.push(Key::Entry(identity, leaf));
                    }
                }
            }
            if let Ok((_, identity)) = crate::file_identity::metadata(path) {
                if identity != FileIdentity::Unavailable {
                    keys.push(Key::File(identity));
                }
            }
            keys
        })
        .collect();
    let (held, wake) = &*HELD;
    let mut held = held.lock();
    while keys.iter().flatten().any(|key| held.contains_key(key)) {
        wake.wait(&mut held);
    }
    for key in keys.iter().flatten() {
        *held.entry(key.clone()).or_default() += 1;
    }
    let mut locks: Vec<_> = entries
        .into_iter()
        .zip(keys)
        .map(|((_, _, index), keys)| (index, FileLock(keys)))
        .collect();
    locks.sort_by_key(|(index, _)| *index);
    locks.into_iter().map(|(_, guard)| guard).collect()
}

/// Strong stat evidence on Unix; identity, time and bytes on other platforms.
/// The bytes fallback also covers symlinks and filesystems without identities.
#[derive(Clone)]
pub(crate) struct Version {
    exists: bool,
    resolved: PathBuf,
    stat: Option<crate::backup::DiskFileVersion>,
    identity: Option<FileIdentity>,
    modified: Option<std::time::SystemTime>,
    bytes: Option<std::sync::Arc<[u8]>>,
}

impl Version {
    pub(crate) fn read(path: &Path) -> Self {
        let mut version = Self::existing(path, std::sync::Arc::from([]));
        match std::fs::read(path) {
            Ok(bytes) => {
                version.bytes = Some(bytes.into());
                version
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Self::missing(path),
            // Preserve the tool's ordinary per-file I/O failure handling. Stat
            // evidence still protects unreadable paths and directories.
            Err(_) => {
                version.bytes = None;
                version
            }
        }
    }

    pub(crate) fn set_bytes(&mut self, bytes: std::sync::Arc<[u8]>) {
        self.bytes = Some(bytes);
    }

    pub(crate) fn existing(path: &Path, bytes: std::sync::Arc<[u8]>) -> Self {
        let metadata = crate::file_identity::metadata(path).ok();
        Self {
            exists: true,
            resolved: resolved(path),
            stat: crate::backup::DiskFileVersion::of_path(path),
            identity: metadata.as_ref().map(|(_, identity)| *identity),
            modified: metadata.and_then(|(metadata, _)| metadata.modified().ok()),
            bytes: Some(bytes),
        }
    }

    pub(crate) fn missing(path: &Path) -> Self {
        Self {
            exists: false,
            resolved: resolved(path),
            stat: None,
            identity: None,
            modified: None,
            bytes: None,
        }
    }

    pub(crate) fn check(&self, path: &Path) -> Result<(), AftError> {
        let same = if self.exists {
            let metadata = crate::file_identity::metadata(path).ok();
            self.resolved == resolved(path)
                && self.stat == crate::backup::DiskFileVersion::of_path(path)
                && self.identity == metadata.as_ref().map(|(_, identity)| *identity)
                && self.modified == metadata.and_then(|(metadata, _)| metadata.modified().ok())
                && (self.stat.is_some()
                    || self.bytes.as_ref().is_none_or(|bytes| {
                        std::fs::read(path)
                            .is_ok_and(|current| current.as_slice() == bytes.as_ref())
                    }))
        } else {
            std::fs::symlink_metadata(path)
                .is_err_and(|error| error.kind() == std::io::ErrorKind::NotFound)
        };
        if same {
            Ok(())
        } else {
            Err(AftError::FileChangedSinceRead {
                path: path.display().to_string(),
            })
        }
    }
}

#[cfg(test)]
type BeforeCheck = std::sync::Arc<dyn Fn() + Send + Sync>;
#[cfg(test)]
static BEFORE_CHECK: LazyLock<Mutex<HashMap<PathBuf, BeforeCheck>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Hooks are keyed by the resolved path, so a hook set for a test's path
/// fires however a tool spells it (on Windows, `apply_patch` and the test can
/// differ by the `\\?\` prefix).
#[cfg(test)]
pub(crate) fn set_before_check(path: &Path, hook: Option<BeforeCheck>) {
    let mut hooks = BEFORE_CHECK.lock();
    if let Some(hook) = hook {
        hooks.insert(resolved(path), hook);
    } else {
        hooks.remove(&resolved(path));
    }
}

pub(crate) fn before_check(path: &Path) {
    #[cfg(test)]
    {
        let hook = BEFORE_CHECK.lock().get(&resolved(path)).cloned();
        if let Some(hook) = hook {
            hook();
        }
    }
    #[cfg(not(test))]
    let _ = path;
}
