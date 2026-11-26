//! Path admission shared by full and bounded listings of indexed files.

use std::path::{Path, PathBuf};

use ignore::WalkBuilder;

use super::{
    build_path_filters, canonicalize_for_search_membership, is_within_search_root,
    project_walk_builder_pruned, PathFilters,
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

/// The ordinary index walk, with coverage accounting for the unpublished corpus.
pub fn checked_project_walk_builder(
    root: &Path,
    coverage: crate::grep_executor::WalkCoverage,
) -> WalkBuilder {
    crate::io_accounting::WORK
        .full_walks
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let boundary = crate::walk_boundary::DeviceBoundary::for_root(root)
        .inspect_err(|error| coverage.record_error(root, error))
        .ok();
    project_walk_builder_pruned(root, checked_project_content_excludes, move |entry| {
        !entry.file_type().is_some_and(|kind| kind.is_dir())
            || coverage.allows_device(entry.path(), boundary.as_ref())
    })
}

fn checked_project_content_excludes(path: &Path, kind: Option<&std::fs::FileType>) -> bool {
    if crate::project_content::is_cortexkit_data_read_denied(path) {
        crate::data_layer::read_fence::capture().record_floor(path);
        return true;
    }
    crate::project_content::is_excluded_with_file_type(path, kind)
}
