use std::path::{Path, PathBuf};

pub(crate) fn git_info_exclude_for_target(target: &Path) -> Option<(PathBuf, PathBuf)> {
    for repository_root in target.ancestors() {
        let git_entry = repository_root.join(".git");
        let Ok(metadata) = crate::bounded_io::metadata(&git_entry) else {
            continue;
        };
        let git_dir = if metadata.is_dir() {
            git_entry
        } else if metadata.is_file() {
            let contents = crate::bounded_io::read_to_string(&git_entry).ok()?;
            let git_dir = contents.trim().strip_prefix("gitdir:")?.trim();
            let git_dir = PathBuf::from(git_dir);
            if git_dir.is_absolute() {
                git_dir
            } else {
                repository_root.join(git_dir)
            }
        } else {
            continue;
        };
        let git_dir = std::fs::canonicalize(&git_dir).unwrap_or(git_dir);
        let common_dir = git_dir.join("commondir");
        let git_common_dir = if let Ok(common) = crate::bounded_io::read_to_string(&common_dir) {
            let common = PathBuf::from(common.trim());
            if common.is_absolute() {
                common
            } else {
                git_dir.join(common)
            }
        } else {
            git_dir
        };
        return Some((
            repository_root.to_path_buf(),
            git_common_dir.join("info/exclude"),
        ));
    }
    None
}
