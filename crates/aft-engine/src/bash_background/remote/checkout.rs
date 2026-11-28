use crate::exec_remote::{DirectoryKind, Error};
use std::path::{Path, PathBuf};

/// Resolve the workdir and find the same checkout the runner would copy.
#[doc(hidden)]
pub fn return_paths_workdir(workdir: &Path) -> (PathBuf, Option<PathBuf>) {
    let Ok(cwd) = workdir.canonicalize() else {
        return (workdir.to_path_buf(), None);
    };
    let checkout = checkout_containing(&cwd).map(Path::to_path_buf);
    (cwd, checkout)
}

pub(crate) fn worktree_root_containing(
    path: &Path,
    kind: DirectoryKind,
) -> Result<Option<PathBuf>, Error> {
    let path = kind.canonicalize(path)?;
    checkout_containing(&path)
        .map(|root| kind.canonicalize(root))
        .transpose()
}

fn checkout_containing(canonical_path: &Path) -> Option<&Path> {
    canonical_path.ancestors().find(|ancestor| {
        let git_marker = ancestor.join(".git");
        git_marker.is_file() || git_marker.is_dir()
    })
}
