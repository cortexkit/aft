//! Membership, configuration and identity inputs for the existing resolver.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use super::join::CallgraphBlob;
use crate::views::{Manifest, ManifestEntry, RelPath};

/// Manifest planes currently store their full blob keys as strings.
pub(crate) type BlobKey = String;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum EntryKind {
    Regular,
    Directory,
    Symlink,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct DirEntry {
    pub name: Vec<u8>,
    pub kind: EntryKind,
}

pub(crate) trait ProjectFacts {
    fn is_file(&self, rel: &[u8]) -> bool;
    fn is_dir(&self, rel: &[u8]) -> bool;
    fn config_bytes(&self, rel: &[u8]) -> Option<Arc<[u8]>>;
    fn symlink_target(&self, rel: &[u8]) -> Option<&[u8]>;
    fn canonical(&self, rel: &[u8]) -> Option<Vec<u8>>;
    fn list_dir(&self, rel: &[u8]) -> Vec<DirEntry>;
}

pub(crate) struct ManifestFacts<'m> {
    pub manifest: &'m Manifest,
    pub blobs: &'m dyn Fn(&BlobKey) -> Option<Arc<[u8]>>,
}

impl ManifestFacts<'_> {
    fn entry(&self, rel: &[u8]) -> Option<&ManifestEntry> {
        self.manifest.get(&RelPath::new(rel.to_vec()).ok()?)
    }
}

impl ProjectFacts for ManifestFacts<'_> {
    fn is_file(&self, rel: &[u8]) -> bool {
        matches!(self.entry(rel), Some(ManifestEntry::Regular { .. }))
    }

    fn is_dir(&self, rel: &[u8]) -> bool {
        let mut prefix = rel.to_vec();
        if !prefix.is_empty() {
            prefix.push(b'/');
        }
        self.manifest.entries().any(|(path, _)| {
            path.as_bytes().starts_with(&prefix) && path.as_bytes().len() > prefix.len()
        })
    }

    fn config_bytes(&self, rel: &[u8]) -> Option<Arc<[u8]>> {
        let path = self.canonical(rel)?;
        let ManifestEntry::Regular { planes, .. } = self.entry(&path)? else {
            return None;
        };
        let bytes = (self.blobs)(planes.callgraph.as_ref()?)?;
        match CallgraphBlob::from_bytes(&bytes).ok()? {
            CallgraphBlob::Config(config) if config.language == "config" => {
                Some(config.source.into())
            }
            _ => None,
        }
    }

    fn symlink_target(&self, rel: &[u8]) -> Option<&[u8]> {
        match self.entry(rel)? {
            ManifestEntry::Symlink { target_bytes } => Some(target_bytes.as_bytes()),
            _ => None,
        }
    }

    fn canonical(&self, rel: &[u8]) -> Option<Vec<u8>> {
        if rel.starts_with(b"/") {
            return None;
        }
        let mut pending = rel
            .split(|b| *b == b'/')
            .map(<[u8]>::to_vec)
            .collect::<std::collections::VecDeque<_>>();
        let mut components = Vec::<Vec<u8>>::new();
        let mut seen = BTreeSet::new();
        let mut followed = 0;
        while let Some(component) = pending.pop_front() {
            match component.as_slice() {
                b"" | b"." => continue,
                b".." => {
                    components.pop()?;
                    continue;
                }
                _ => components.push(component),
            }
            let path = components.join(&b'/');
            if let Some(target) = self.symlink_target(&path) {
                // Bound expanding cycles such as a -> a/a as well as exact cycles.
                followed += 1;
                if target.starts_with(b"/")
                    || followed > 40
                    || !seen.insert((path, pending.clone()))
                {
                    return None;
                }
                components.pop();
                for component in target.split(|b| *b == b'/').rev() {
                    pending.push_front(component.to_vec());
                }
            } else if !pending.is_empty() && !self.is_dir(&path) {
                return None;
            }
        }
        let path = components.join(&b'/');
        (self.entry(&path).is_some() || self.is_dir(&path)).then_some(path)
    }

    fn list_dir(&self, rel: &[u8]) -> Vec<DirEntry> {
        let mut prefix = rel.to_vec();
        if !prefix.is_empty() {
            prefix.push(b'/');
        }
        let mut entries = BTreeMap::new();
        for (path, entry) in self.manifest.entries() {
            let Some(tail) = path.as_bytes().strip_prefix(prefix.as_slice()) else {
                continue;
            };
            if tail.is_empty() {
                continue;
            }
            if let Some(slash) = tail.iter().position(|b| *b == b'/') {
                entries
                    .entry(tail[..slash].to_vec())
                    .or_insert(EntryKind::Directory);
            } else {
                let kind = match entry {
                    ManifestEntry::Regular { .. } => EntryKind::Regular,
                    ManifestEntry::Symlink { .. } => EntryKind::Symlink,
                    // A gitlink identifies a separate tree, not files in this view.
                    ManifestEntry::Gitlink { .. } | ManifestEntry::Synthetic { .. } => continue,
                };
                entries.insert(tail.to_vec(), kind);
            }
        }
        entries
            .into_iter()
            .map(|(name, kind)| DirEntry { name, kind })
            .collect()
    }
}

#[cfg(unix)]
pub(crate) fn path_bytes(path: &Path) -> Vec<u8> {
    use std::os::unix::ffi::OsStrExt;
    path.as_os_str().as_bytes().to_vec()
}
#[cfg(not(unix))]
pub(crate) fn path_bytes(path: &Path) -> Vec<u8> {
    path.to_string_lossy().replace('\\', "/").into_bytes()
}
#[cfg(unix)]
pub(crate) fn byte_path(bytes: &[u8]) -> PathBuf {
    use std::os::unix::ffi::OsStrExt;
    PathBuf::from(std::ffi::OsStr::from_bytes(bytes))
}
#[cfg(not(unix))]
pub(crate) fn byte_path(bytes: &[u8]) -> PathBuf {
    PathBuf::from(String::from_utf8_lossy(bytes).as_ref())
}

/// Adapts the resolver's existing absolute paths to project-relative byte keys.
pub(crate) struct FactPaths<'a> {
    pub root: &'a Path,
    pub facts: &'a dyn ProjectFacts,
}
impl FactPaths<'_> {
    fn rel(&self, path: &Path) -> Option<Vec<u8>> {
        Some(path_bytes(path.strip_prefix(self.root).unwrap_or(path)))
    }
    pub fn is_file(&self, path: &Path) -> bool {
        self.rel(path)
            .and_then(|rel| self.facts.canonical(&rel))
            .is_some_and(|rel| self.facts.is_file(&rel))
    }
    pub fn is_dir(&self, path: &Path) -> bool {
        self.rel(path)
            .and_then(|rel| self.facts.canonical(&rel))
            .is_some_and(|rel| self.facts.is_dir(&rel))
    }
    pub fn bytes(&self, path: &Path) -> Option<Arc<[u8]>> {
        self.facts.config_bytes(&self.rel(path)?)
    }
    pub fn canonical(&self, path: &Path) -> Option<PathBuf> {
        Some(
            self.root
                .join(byte_path(&self.facts.canonical(&self.rel(path)?)?)),
        )
    }
    pub fn list_dir(&self, path: &Path) -> Vec<DirEntry> {
        self.rel(path)
            .map(|rel| self.facts.list_dir(&rel))
            .unwrap_or_default()
    }
}
