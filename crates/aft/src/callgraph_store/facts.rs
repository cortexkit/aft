//! Membership, configuration and identity inputs for the existing resolver.

use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Immutable parse payloads are identified by their full content keys.
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
    /// Whether the resolver should record which configuration fields it reads.
    fn records_config_facts(&self) -> bool {
        false
    }
    /// Whether every checkout read is recorded with its answer (the data
    /// layer's recorder), so a reader can record the key of a repository-wide
    /// structure in place of the reads that build it.
    fn records_answers(&self) -> bool {
        false
    }
    fn config_fact(&self, _rel: &[u8], _name: &str) {}
    fn memo_start(&self, _path: &Path, _kind: &str, _name: &str) {}
    fn memo_finish(&self, _path: &Path, _kind: &str, _name: &str) {}
    fn memo_replay(&self, _path: &Path, _kind: &str, _name: &str) {}
    /// Records that the caller depends on a repository-wide structure,
    /// named by its domain key, instead of on that structure's inputs.
    fn record_domain(&self, _key: &str) {}
    /// Stops recording consultations until the matching
    /// [`ProjectFacts::resume_recording`]; for building a repository-wide
    /// structure whose readers record its domain key instead.
    fn pause_recording(&self) {}
    fn resume_recording(&self) {}
    fn workspace_package(&self, _root: &Path, _name: &str) -> Option<Option<PathBuf>> {
        None
    }
    fn remember_workspace_package(&self, _root: &Path, _name: &str, _value: Option<PathBuf>) {}
    fn workspace_members(&self, _root: &Path) -> Option<Arc<Vec<PathBuf>>> {
        None
    }
    fn remember_workspace_members(&self, _root: &Path, _value: Arc<Vec<PathBuf>>) {}
    fn is_file(&self, rel: &[u8]) -> bool;
    fn is_dir(&self, rel: &[u8]) -> bool;
    fn config_bytes(&self, rel: &[u8]) -> Option<Arc<[u8]>>;
    fn attributed_config_bytes(&self, rel: &[u8]) -> Option<Arc<[u8]>> {
        self.config_bytes(rel)
    }
    fn symlink_target(&self, rel: &[u8]) -> Option<&[u8]>;
    fn canonical(&self, rel: &[u8]) -> Option<Vec<u8>>;
    /// Probe identities may be shared within a resolver snapshot. Explicit
    /// canonical-path requests remain live so identity revalidation is not memoized.
    fn canonical_probe(&self, rel: &[u8]) -> Option<Vec<u8>> {
        self.canonical(rel)
    }
    fn canonical_path(&self, root: &Path, rel: &[u8]) -> Option<PathBuf> {
        Some(root.join(byte_path(&self.canonical(rel)?)))
    }
    fn list_dir(&self, rel: &[u8]) -> Vec<DirEntry>;
}

/// Stands for "no project facts": per-file extraction binds one file's parse
/// on its own, before it belongs to any checkout. It is not an empty checkout.
/// Every question answers that nothing exists, and no directory visit, read
/// or consultation is recorded.
pub(crate) struct NoProjectFacts;

impl ProjectFacts for NoProjectFacts {
    fn is_file(&self, _rel: &[u8]) -> bool {
        false
    }

    fn is_dir(&self, _rel: &[u8]) -> bool {
        false
    }

    fn config_bytes(&self, _rel: &[u8]) -> Option<Arc<[u8]>> {
        None
    }

    fn symlink_target(&self, _rel: &[u8]) -> Option<&[u8]> {
        None
    }

    fn canonical(&self, _rel: &[u8]) -> Option<Vec<u8>> {
        None
    }

    fn list_dir(&self, _rel: &[u8]) -> Vec<DirEntry> {
        Vec::new()
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
    /// Test the recording capability before allocating relative path bytes, so
    /// the disk resolver's default hooks do no work and allocate nothing.
    pub fn config_fact(&self, path: &Path, name: &str) {
        if self.facts.records_config_facts() {
            if let Some(rel) = self.rel(path) {
                self.facts.config_fact(&rel, name);
            }
        }
    }
    pub fn config_field(&self, dir: &Path, input: &str, name: &str) {
        if self.facts.records_config_facts() {
            self.config_fact(&dir.join(input), name);
        }
    }
    fn rel(&self, path: &Path) -> Option<Vec<u8>> {
        Some(path_bytes(path.strip_prefix(self.root).unwrap_or(path)))
    }
    pub fn is_file(&self, path: &Path) -> bool {
        self.rel(path)
            .and_then(|rel| self.facts.canonical_probe(&rel))
            .is_some_and(|rel| self.facts.is_file(&rel))
    }
    pub fn is_dir(&self, path: &Path) -> bool {
        self.rel(path)
            .and_then(|rel| self.facts.canonical_probe(&rel))
            .is_some_and(|rel| self.facts.is_dir(&rel))
    }
    /// Opaque reads remain distinguishable from the field-instrumented readers.
    pub fn attributed_bytes(&self, path: &Path) -> Option<Arc<[u8]>> {
        self.facts.attributed_config_bytes(&self.rel(path)?)
    }
    pub fn canonical(&self, path: &Path) -> Option<PathBuf> {
        self.facts.canonical_path(self.root, &self.rel(path)?)
    }
    pub fn list_dir(&self, path: &Path) -> Vec<DirEntry> {
        self.rel(path)
            .map(|rel| self.facts.list_dir(&rel))
            .unwrap_or_default()
    }
}
