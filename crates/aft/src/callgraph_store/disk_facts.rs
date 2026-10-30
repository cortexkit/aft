//! Live-checkout implementation of the resolver's project-facts seam.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

use super::facts::{byte_path, path_bytes, DirEntry, EntryKind, ProjectFacts};

/// Canonical identities of resolver probes within one build/refresh snapshot.
/// Live identity checks deliberately bypass this memo, including source reads,
/// watcher paths, and the caller revalidation before Rust crate-root lookups.
#[derive(Default)]
pub(crate) struct CanonicalProbeMemo {
    paths: Mutex<(HashMap<PathBuf, PathBuf>, usize)>,
}

impl CanonicalProbeMemo {
    fn canonical(&self, path: PathBuf) -> PathBuf {
        if let Some(cached) = self.paths.lock().unwrap().0.get(&path) {
            return cached.clone();
        }
        let canonical = super::canonicalize_path(&path);
        // A redirected probe may traverse a symlink. Do not freeze its target:
        // retargeting an existing alias must still be observed within the pass.
        // The missing-path fallback can retain the spelling of a dangling
        // symlink. Inspect the missing suffix up to its first existing ancestor
        // as well, so a subsequently repaired alias is never served from cache.
        let regular_ancestor = path
            .ancestors()
            .find_map(|ancestor| match std::fs::symlink_metadata(ancestor) {
                Ok(metadata) => Some(!metadata.file_type().is_symlink()),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
                Err(_) => Some(false),
            })
            .unwrap_or(false);
        if canonical != path || !regular_ancestor {
            return canonical;
        }
        let weight = 128
            + path.as_os_str().as_encoded_bytes().len()
            + canonical.as_os_str().as_encoded_bytes().len();
        let mut paths = self.paths.lock().unwrap();
        // Stop admitting new probes at either bound; uncached probes still use
        // the identical live resolver, so capacity cannot change graph output.
        if paths.0.len() < 32_768
            && paths.1 + weight <= 4 * 1024 * 1024
            && !paths.0.contains_key(&path)
        {
            paths.1 += weight;
            paths.0.insert(path, canonical.clone());
        }
        canonical
    }
}

pub(crate) struct DiskFacts {
    pub project_root: PathBuf,
    symlinks: OnceLock<BTreeMap<Vec<u8>, Vec<u8>>>,
    canonical_probes: Option<Arc<CanonicalProbeMemo>>,
}

impl DiskFacts {
    pub fn new(project_root: &Path) -> Self {
        Self {
            project_root: project_root.to_path_buf(),
            symlinks: OnceLock::new(),
            canonical_probes: None,
        }
    }

    pub fn with_probe_memo(project_root: &Path, memo: Arc<CanonicalProbeMemo>) -> Self {
        Self {
            canonical_probes: Some(memo),
            ..Self::new(project_root)
        }
    }
}

impl ProjectFacts for DiskFacts {
    fn is_file(&self, rel: &[u8]) -> bool {
        self.project_root.join(byte_path(rel)).is_file()
    }
    fn is_dir(&self, rel: &[u8]) -> bool {
        self.project_root.join(byte_path(rel)).is_dir()
    }
    fn config_bytes(&self, rel: &[u8]) -> Option<Arc<[u8]>> {
        #[cfg(test)]
        super::work_counts::note_config_read(&self.project_root.join(byte_path(rel)));
        std::fs::read(self.project_root.join(byte_path(rel)))
            .ok()
            .map(Into::into)
    }
    fn symlink_target(&self, rel: &[u8]) -> Option<&[u8]> {
        // Targets are retained only if requested; normal disk identity uses the
        // operating system directly and never needs this auxiliary snapshot.
        self.symlinks
            .get_or_init(|| {
                let mut targets = BTreeMap::new();
                let mut stack = vec![Vec::new()];
                while let Some(dir) = stack.pop() {
                    for entry in self.list_dir(&dir) {
                        let child = byte_path(&dir).join(byte_path(&entry.name));
                        match entry.kind {
                            EntryKind::Directory => stack.push(path_bytes(&child)),
                            EntryKind::Symlink => {
                                if let Ok(target) =
                                    std::fs::read_link(self.project_root.join(&child))
                                {
                                    targets.insert(path_bytes(&child), path_bytes(&target));
                                }
                            }
                            EntryKind::Regular => {}
                        }
                    }
                }
                targets
            })
            .get(rel)
            .map(Vec::as_slice)
    }
    fn canonical(&self, rel: &[u8]) -> Option<Vec<u8>> {
        let path = self.project_root.join(byte_path(rel));
        let path = super::canonicalize_path(&path);
        Some(path_bytes(
            path.strip_prefix(&self.project_root).unwrap_or(&path),
        ))
    }
    fn canonical_probe(&self, rel: &[u8]) -> Option<Vec<u8>> {
        let Some(memo) = &self.canonical_probes else {
            return self.canonical(rel);
        };
        let path = memo.canonical(self.project_root.join(byte_path(rel)));
        Some(path_bytes(
            path.strip_prefix(&self.project_root).unwrap_or(&path),
        ))
    }
    fn canonical_path(&self, root: &Path, rel: &[u8]) -> Option<PathBuf> {
        let path = root.join(byte_path(rel));
        Some(super::canonicalize_path(&path))
    }
    fn list_dir(&self, rel: &[u8]) -> Vec<DirEntry> {
        let dir = self.project_root.join(byte_path(rel));
        #[cfg(test)]
        super::work_counts::note_directory_read(&dir);
        let Ok(boundary) = crate::walk_boundary::DeviceBoundary::for_root(&self.project_root)
        else {
            return Vec::new();
        };
        let Ok(entries) = std::fs::read_dir(&dir) else {
            return Vec::new();
        };
        entries
            .filter_map(Result::ok)
            .filter_map(|entry| {
                let file_type = entry.file_type().ok()?;
                let kind = if file_type.is_symlink() {
                    EntryKind::Symlink
                } else if file_type.is_dir() {
                    if !boundary.should_descend(&entry.path()).unwrap_or(false) {
                        return None;
                    }
                    EntryKind::Directory
                } else if file_type.is_file() {
                    EntryKind::Regular
                } else {
                    return None;
                };
                Some(DirEntry {
                    name: path_bytes(Path::new(&entry.file_name())),
                    kind,
                })
            })
            .collect()
    }
}
