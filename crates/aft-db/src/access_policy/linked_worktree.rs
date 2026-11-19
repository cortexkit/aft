//! Read authority for other checkouts of the same repository. The caller
//! enables it only for first-party routes with an authenticated head scope.

use std::cell::Cell;
use std::collections::HashMap;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

thread_local! {
    static HEAD_SCOPE: Cell<Option<bool>> = const { Cell::new(None) };
}

pub struct HeadScopeGuard(Option<bool>, std::marker::PhantomData<*const ()>);

pub fn head_scope_guard(head: bool) -> HeadScopeGuard {
    HeadScopeGuard(
        HEAD_SCOPE.with(|slot| slot.replace(Some(head && slot.get().unwrap_or(true)))),
        std::marker::PhantomData,
    )
}

impl Drop for HeadScopeGuard {
    fn drop(&mut self) {
        HEAD_SCOPE.with(|slot| slot.set(self.0));
    }
}

pub fn in_head_scope() -> bool {
    HEAD_SCOPE.with(|slot| slot.get() == Some(true))
}

#[derive(Debug, Default)]
pub struct ProjectCache(parking_lot::Mutex<HashMap<PathBuf, Project>>);

impl ProjectCache {
    pub fn for_root(&self, root: &Path, home: &Path) -> Project {
        self.0
            .lock()
            .entry(root.to_owned())
            .or_insert_with(|| {
                Project(Arc::new(ProjectData {
                    root: root.to_owned(),
                    home: home.to_owned(),
                    git: OnceLock::new(),
                }))
            })
            .clone()
    }
}

#[derive(Debug, Clone)]
pub struct Project(Arc<ProjectData>);

#[derive(Debug)]
struct ProjectData {
    root: PathBuf,
    home: PathBuf,
    git: OnceLock<Option<ProjectGit>>,
}

#[derive(Debug)]
struct ProjectGit {
    common: PathBuf,
    home: Option<PathBuf>,
}

impl Project {
    pub(super) fn admits(&self, path: &Path) -> bool {
        if self.0.git.get().is_some_and(Option::is_none) {
            return false;
        }
        let project = self.0.clone();
        aft_base::bounded_io::run(path, None, move |path| {
            let git = project.git.get_or_init(|| {
                let home = if project.home.as_os_str().is_empty() {
                    None
                } else {
                    Some(project.home.canonicalize().ok()?)
                };
                let common =
                    nearest_common_dir(&project.root.canonicalize().ok()?, home.as_deref())?;
                Some(ProjectGit { common, home })
            });
            let Some(git) = git.as_ref() else {
                return Ok(false);
            };
            let canonical = path.canonicalize()?;
            // Do not authorize a different path than the one already checked
            // against the protected paths, even if it is in the same checkout.
            if aft_base::path_identity::normalize_path(&canonical) != path {
                return Ok(false);
            }
            Ok(nearest_common_dir(&canonical, git.home.as_deref()).as_ref() == Some(&git.common))
        })
        .unwrap_or(false)
    }
}

fn nearest_common_dir(path: &Path, home: Option<&Path>) -> Option<PathBuf> {
    const MAX_ANCESTORS: usize = 64;
    let start = if path.metadata().ok()?.is_dir() {
        path
    } else {
        path.parent()?
    };
    for folder in start.ancestors().take(MAX_ANCESTORS) {
        if folder.parent().is_none() || home == Some(folder) {
            break;
        }
        let dot_git = folder.join(".git");
        match dot_git.symlink_metadata() {
            Ok(meta) if meta.is_dir() => return canonical_dir(&dot_git),
            Ok(meta) if meta.is_file() => return verified_linked_common_dir(&dot_git),
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            _ => return None,
        }
    }
    None
}

fn canonical_dir(path: &Path) -> Option<PathBuf> {
    let directory = path.canonicalize().ok()?;
    directory.is_dir().then_some(directory)
}

fn read_pointer(path: &Path) -> Option<String> {
    use aft_base::live_db_files::{is_live_sqlite_file_with_metadata, is_live_sqlite_handle};
    use aft_base::no_follow_file::open_no_follow;
    use aft_base::task_temp_reads::opened_location;

    const MAX_POINTER_BYTES: u64 = 16 * 1024;
    let metadata = path.symlink_metadata().ok()?;
    if !metadata.is_file() || is_live_sqlite_file_with_metadata(path, Some(&metadata)) {
        return None;
    }
    let mut file = open_no_follow(path.parent()?, path).ok()?;
    let metadata = file.metadata().ok()?;
    if !metadata.is_file()
        || is_live_sqlite_handle(&metadata)
        || aft_base::path_identity::normalize_path(&opened_location(&file).ok()?)
            != aft_base::path_identity::normalize_path(path)
    {
        return None;
    }
    let mut value = String::new();
    (&mut *file)
        .take(MAX_POINTER_BYTES + 1)
        .read_to_string(&mut value)
        .ok()?;
    if value.len() as u64 > MAX_POINTER_BYTES {
        return None;
    }
    let line = value.strip_suffix('\n').unwrap_or(&value);
    let line = line.strip_suffix('\r').unwrap_or(line);
    (!line.trim().is_empty() && !line.contains(['\n', '\r', '\0'])).then(|| line.to_owned())
}

/// A `.git` pointer alone is a claim anyone who can write the checkout can
/// make. Require the repository's own worktree registration to point back.
fn verified_linked_common_dir(dot_git: &Path) -> Option<PathBuf> {
    let pointer = read_pointer(dot_git)?;
    let gitdir = canonical_dir(&dot_git.parent()?.join(pointer.strip_prefix("gitdir: ")?))?;
    let common = canonical_dir(&gitdir.join(read_pointer(&gitdir.join("commondir"))?))?;
    if gitdir.parent()? != common.join("worktrees") {
        return None;
    }
    let back = gitdir.join(read_pointer(&gitdir.join("gitdir"))?);
    (back.canonicalize().ok()? == dot_git.canonicalize().ok()?).then_some(common)
}

/// Standalone plugins can read the registered main checkout without asking.
/// Only a common directory named `.git` identifies that checkout's root.
pub fn verified_linked_main_root(worktree: &Path, common: &Path) -> Option<PathBuf> {
    let common = canonical_dir(common)?;
    if common.file_name()? != ".git"
        || verified_linked_common_dir(&worktree.canonicalize().ok()?.join(".git"))? != common
    {
        return None;
    }
    Some(aft_base::path_identity::normalize_path(common.parent()?))
}
