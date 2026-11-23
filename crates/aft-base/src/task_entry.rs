/// Recursive readers must not bypass the explicit-path check by entering a
/// bundle through its directory. Do not follow links or credential payloads.
#[doc(hidden)]
pub fn safe_task_entry(path: &Path) -> bool {
    if task_directory(path).is_none() {
        return true;
    }
    if crate::project_content::is_sqlite_or_metadata(path) {
        return false;
    }
    if is_task_environment(path) {
        return false;
    }
    let Ok(metadata) = fs::symlink_metadata(path) else {
        return false;
    };
    if metadata.file_type().is_symlink() || (!metadata.is_file() && !metadata.is_dir()) {
        return false;
    }
    if metadata.is_file() {
        #[cfg(test)]
        crate::project_content::record_open(path);
        return path
            .parent()
            .and_then(|parent| PinnedDir::open(parent).ok())
            .is_some_and(|parent| {
                path.file_name()
                    .is_some_and(|name| parent.open_file(name, false).is_ok())
            });
    }
    true
}
