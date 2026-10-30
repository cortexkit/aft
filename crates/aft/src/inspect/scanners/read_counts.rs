use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

/// Read counters are enabled explicitly for fixture roots, not for ordinary
/// debug-daemon scans. Counting one root must not collect paths from others.
#[derive(Default)]
pub(super) struct FileReadCounts {
    roots: OnceLock<Mutex<BTreeMap<PathBuf, BTreeMap<PathBuf, usize>>>>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registered_roots_count_equivalent_path_spellings_only() {
        let fixture = tempfile::tempdir().unwrap();
        let base = crate::inspect::job::canonicalize_normalized(fixture.path());
        let root = base.join("tracked");
        std::fs::create_dir(&root).unwrap();
        let counts = FileReadCounts::new();
        counts.reset(root.clone());
        counts.record(&base.join("spelling/../tracked/file.rs"));
        counts.record(&base.join("untracked/file.rs"));
        assert_eq!(counts.count(&root), 1);
    }

    #[cfg(unix)]
    #[test]
    fn registered_roots_count_symlinked_reads() {
        let fixture = tempfile::tempdir().unwrap();
        let root = fixture.path().join("tracked");
        std::fs::create_dir(&root).unwrap();
        std::fs::write(root.join("file.rs"), "fn f() {}\n").unwrap();
        let alias = fixture.path().join("alias");
        std::os::unix::fs::symlink(&root, &alias).unwrap();
        let root = crate::inspect::job::canonicalize_normalized(&root);
        let counts = FileReadCounts::new();
        counts.reset(root.clone());
        counts.record(&alias.join("file.rs"));
        assert_eq!(counts.count(&root), 1);
    }
}

impl FileReadCounts {
    pub(super) const fn new() -> Self {
        Self {
            roots: OnceLock::new(),
        }
    }

    pub(super) fn record(&self, path: &Path) {
        let Some(roots) = self.roots.get() else {
            return;
        };
        // Match canonical fixture roots even when a read uses a symlinked
        // temporary directory or a Windows short name. Unregistered scans
        // return above without doing this filesystem lookup.
        let path = crate::inspect::job::canonicalize_normalized(path);
        if let Ok(mut roots) = roots.lock() {
            for (root, counts) in roots.iter_mut() {
                if path.starts_with(root) {
                    *counts.entry(path.to_path_buf()).or_default() += 1;
                }
            }
        }
    }

    pub(super) fn reset(&self, root: PathBuf) {
        if let Ok(mut roots) = self.roots.get_or_init(Default::default).lock() {
            roots.insert(root, BTreeMap::new());
        }
    }

    pub(super) fn count(&self, root: &Path) -> usize {
        self.roots
            .get()
            .and_then(|roots| roots.lock().ok())
            .and_then(|roots| roots.get(root).map(|counts| counts.values().sum()))
            .unwrap_or_default()
    }
}
