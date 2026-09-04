//! Live-checkout implementation of the resolver's project-facts seam.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

use super::facts::{byte_path, path_bytes, DirEntry, EntryKind, ProjectFacts};

pub(crate) struct DiskFacts {
    pub project_root: PathBuf,
    symlinks: OnceLock<BTreeMap<Vec<u8>, Vec<u8>>>,
}

impl DiskFacts {
    pub fn new(project_root: &Path) -> Self {
        Self {
            project_root: project_root.to_path_buf(),
            symlinks: OnceLock::new(),
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
    fn list_dir(&self, rel: &[u8]) -> Vec<DirEntry> {
        let dir = self.project_root.join(byte_path(rel));
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
