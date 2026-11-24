use super::{fs, project_walk_builder, Path, SearchIndex, DEFAULT_MAX_FILE_SIZE};

pub fn build(root: &Path) -> SearchIndex {
    build_with_limit(root, DEFAULT_MAX_FILE_SIZE)
}

pub fn build_with_limit(root: &Path, max_file_size: u64) -> SearchIndex {
    let root = fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    let mut index = SearchIndex::empty_ready_for_root(&root);
    index.max_file_size = max_file_size;
    for entry in project_walk_builder(&root).build().flatten() {
        if entry.file_type().is_some_and(|kind| kind.is_file()) {
            index.update_file(entry.path());
        }
    }
    index
}
