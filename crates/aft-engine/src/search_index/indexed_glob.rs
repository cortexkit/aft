//! Path admission shared by full and bounded listings of indexed files.

use std::path::{Path, PathBuf};

use super::{
    build_path_filters, canonicalize_for_search_membership, is_within_search_root, PathFilters,
};

pub(crate) struct IndexedGlobMatcher {
    filters: PathFilters,
    raw_root: PathBuf,
    canonical_root: PathBuf,
    file_scope: bool,
}

impl IndexedGlobMatcher {
    pub(crate) fn new(pattern: &str, search_root: &Path) -> Option<Self> {
        Some(Self {
            filters: build_path_filters(&[pattern.to_owned()], &[]).ok()?,
            raw_root: search_root.to_path_buf(),
            canonical_root: canonicalize_for_search_membership(search_root),
            file_scope: search_root.is_file(),
        })
    }

    pub(crate) fn matches(&self, path: &Path) -> bool {
        if path.as_os_str().is_empty() {
            return false;
        }
        // A stored Windows path can use the raw or canonical spelling, such
        // as a short filename alias. Match relative to the root spelling that
        // contains it, not relative to the project root.
        let root = if is_within_search_root(&self.raw_root, path) {
            self.raw_root.as_path()
        } else if is_within_search_root(&self.canonical_root, path) {
            self.canonical_root.as_path()
        } else {
            return false;
        };
        // A file scope matches its basename, not the empty path relative to itself.
        let base = if self.file_scope {
            root.parent().unwrap_or(root)
        } else {
            root
        };
        self.filters.matches(base, path)
    }
}
