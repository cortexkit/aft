//! Shared project-walk ignore policy without request-local state.

use std::path::Path;

/// Whether a traversal may open ignore files for the current request.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IgnoreFileAccess {
    Allowed,
    Fenced,
}

/// True when `entry` is a directory below the walk root that is itself a
/// nested linked worktree of this repository, which the walk must prune.
/// Shared by every walker built on the project walk; the depth check keeps
/// an explicitly named root (depth 0) admitted.
pub fn is_nested_worktree_entry(
    entry: &ignore::DirEntry,
    worktrees: &crate::project_content::NestedWorktreeBoundary,
) -> bool {
    entry.depth() > 0
        && entry
            .file_type()
            .as_ref()
            .is_some_and(std::fs::FileType::is_dir)
        && worktrees.is_nested_worktree_root(entry.path())
}

/// Apply the ignore rules every AFT project traversal honours to `builder`.
///
/// This is the one definition the project walkers share (search index build
/// and refresh, semantic collection, grep/glob fallback, call graph, inspect
/// scans, views membership, LSP applicability and ignore-file discovery), so
/// a folder is indexed the same way whether or not it is a git repository:
///
/// - `.gitignore` files at the walk root, below it, and in its ancestors;
/// - `.aftignore` files, layered on top with higher precedence;
/// - Git's global excludes file (`core.excludesFile`);
/// - `.git/info/exclude`, which only exists inside a repository.
///
/// The `ignore` crate applies the git-family sources only inside a repository
/// unless `require_git` is off. Inside a git or jj work tree the crate default
/// is kept, so git roots walk exactly as they always have (including where a
/// nested repository starts its own rule scope). In a folder that is not a
/// repository it is switched off so `.gitignore` files and the global excludes
/// file still apply, matching the shared matcher that filters watcher events
/// (the shared ignore-input matcher), which reads both regardless of
/// git. `.git/info/exclude` has nothing to contribute there.
///
/// Call it after `standard_filters`, which would otherwise reset these flags.
pub fn apply_project_ignore_rules<'a>(
    builder: &'a mut ignore::WalkBuilder,
    walk_root: &Path,
    access: IgnoreFileAccess,
) -> &'a mut ignore::WalkBuilder {
    if access == IgnoreFileAccess::Fenced {
        // Ignore files can themselves be symlinks to another checkout. A
        // confined walk must not read those files, even to filter its results.
        return builder
            .ignore(false)
            .git_ignore(false)
            .git_global(false)
            .git_exclude(false)
            .parents(false)
            .require_git(false);
    }
    builder
        .git_ignore(true)
        .git_global(true)
        .git_exclude(true)
        .add_custom_ignore_filename(".aftignore")
        .require_git(is_inside_vcs_work_tree(walk_root))
}

/// Whether `path` or one of its ancestors holds `.git` (a directory, or the
/// file a linked worktree or submodule uses) or `.jj`. This is the same test
/// the `ignore` crate uses to decide that git rules apply, run on the same
/// canonical ancestors it inspects, so keeping `require_git` on exactly when
/// this is true leaves the crate's own decision unchanged for git roots.
fn is_inside_vcs_work_tree(path: &Path) -> bool {
    let absolute = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    absolute
        .ancestors()
        .any(|dir| dir.join(".git").exists() || dir.join(".jj").exists())
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use std::ffi::CString;
    use std::io::Read;
    use std::os::fd::FromRawFd;
    use std::os::unix::ffi::OsStrExt;

    #[test]
    fn fenced_walk_does_not_open_ignore_files() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join(".git")).unwrap();
        std::fs::write(root.path().join("visible.rs"), "fn visible() {}\n").unwrap();
        // Kernel open notifications distinguish not reading an ignore file from
        // reading it and then disregarding its rules.
        let fd = unsafe { libc::inotify_init1(libc::IN_NONBLOCK | libc::IN_CLOEXEC) };
        assert!(fd >= 0, "{}", std::io::Error::last_os_error());
        let mut events = unsafe { std::fs::File::from_raw_fd(fd) };
        for name in [".ignore", ".gitignore", ".aftignore"] {
            let path = root.path().join(name);
            std::fs::write(&path, "visible.rs\n").unwrap();
            let path = CString::new(path.as_os_str().as_bytes()).unwrap();
            let watch = unsafe { libc::inotify_add_watch(fd, path.as_ptr(), libc::IN_OPEN) };
            assert!(watch >= 0, "{}", std::io::Error::last_os_error());
        }
        let mut builder = ignore::WalkBuilder::new(root.path());
        let paths = apply_project_ignore_rules(&mut builder, root.path(), IgnoreFileAccess::Fenced)
            .build()
            .map(|entry| entry.unwrap().into_path())
            .collect::<Vec<_>>();
        assert!(paths.contains(&root.path().join("visible.rs")));
        let mut buffer = [0_u8; 1024];
        assert_eq!(
            events.read(&mut buffer).unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock
        );
        // Check the observer itself: a deliberate open must produce an event.
        drop(std::fs::File::open(root.path().join(".gitignore")).unwrap());
        assert!(events.read(&mut buffer).unwrap() > 0);
    }
}
