//! Walking and deleting a directory tree for a recursive `delete_file`.
//!
//! A recursive delete first walks the tree once, without following symlinks
//! and without entering another filesystem, into a [`TreeManifest`]. Backups
//! are taken from that manifest, and the delete then removes exactly the
//! entries the manifest recorded, deepest first. An entry that appears after
//! the walk is never deleted: its directory is no longer empty, so removing
//! the directory fails and the delete stops as a reported partial delete.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

/// How much one `delete_file` call may still copy into the undo store for
/// recursive directory deletes. Shared by every directory in a batch, because
/// the whole call holds exclusive write access to the project root while it
/// copies backups.
#[derive(Debug, Clone, Copy)]
pub(crate) struct RecursiveDeleteBackupBudget {
    /// Entries still allowed. Every entry counts (directories, links and
    /// entries later refused as unsupported too): each backed-up entry costs
    /// its own synced metadata write, and the walk itself stays bounded.
    pub(crate) files_left: usize,
    /// Bytes still allowed, counting only files small enough to be copied,
    /// and each hard-linked file once.
    pub(crate) bytes_left: u64,
    /// The backup store's per-file limit; larger files are skipped by the
    /// store and so cost no copy.
    per_file_limit: Option<u64>,
    /// Whether backups are captured at all. With backups disabled by user
    /// config nothing is copied, so there is nothing to bound.
    pub(crate) enabled: bool,
}

impl RecursiveDeleteBackupBudget {
    pub(crate) fn new(
        policy: crate::backup::BackupPolicy,
        max_files: usize,
        max_bytes: u64,
    ) -> Self {
        Self {
            files_left: max_files,
            bytes_left: max_bytes,
            per_file_limit: policy.max_file_size,
            enabled: policy.enabled && policy.max_file_size != Some(0),
        }
    }
}

/// Which budget a recursive delete walk ran out of.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BudgetLimit {
    Files,
    Bytes,
}

/// A walk stopped because the tree needs more backup than the budget allows.
/// The counts are what the walk had seen when it stopped, so they are lower
/// bounds on the tree's real size.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct BudgetExceeded {
    pub(crate) limit: BudgetLimit,
    pub(crate) files_counted: usize,
    pub(crate) bytes_counted: u64,
}

#[derive(Debug)]
pub(crate) enum CollectError {
    Io(std::io::Error),
    OverBudget(BudgetExceeded),
}

impl From<std::io::Error> for CollectError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error)
    }
}

/// Kinds of entries a recursive delete records. Undo restores all of them
/// except sockets, which are deleted and reported as not restored.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum NodeKind {
    Directory,
    File {
        /// Filesystem identity, used to find paths that are hard links to
        /// the same file.
        dev: u64,
        ino: u64,
        nlink: u64,
    },
    Symlink,
    /// Deleted but never restored: a socket holds no data, and one recreated
    /// by undo would have no process listening on it.
    Socket,
    /// An unbacked leaf: unlink it without inspecting its content or target.
    Unbacked,
}

/// Entries a recursive delete refuses, because removing them would reach
/// beyond the tree or undo could not bring them back.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) enum UnsupportedKind {
    /// A mount point: removing it would delete another filesystem's contents.
    OtherFilesystem,
    /// Could be recreated, but is rare enough that it is not supported yet.
    #[cfg_attr(not(unix), allow(dead_code))]
    Fifo,
    /// Recreating one needs privileges the daemon does not have.
    #[cfg_attr(not(unix), allow(dead_code))]
    Device,
    /// The link target is not UTF-8, and the backup format stores it as text.
    SymlinkNonUtf8Target,
    /// Windows links need their file/directory type recorded, which the
    /// backup format does not do yet.
    WindowsSymlink,
    Other,
}

impl UnsupportedKind {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::OtherFilesystem => "other_filesystem",
            Self::Fifo => "fifo",
            Self::Device => "device",
            Self::SymlinkNonUtf8Target => "symlink_non_utf8_target",
            Self::WindowsSymlink => "windows_symlink",
            Self::Other => "other",
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct TreeEntry {
    pub(crate) path: PathBuf,
    pub(crate) kind: NodeKind,
    /// Device and inode of the entry itself (never a link's target) when the
    /// walk recorded it. On Unix every entry, directories and the root
    /// included, is deleted only while it is still this object; Windows has
    /// no portable identity here and binds children through handles instead.
    #[cfg_attr(not(unix), allow(dead_code))]
    pub(crate) identity: Option<(u64, u64)>,
}

/// The identity a recursive delete records for an entry, from metadata read
/// without following a final symlink.
#[cfg(unix)]
pub(crate) fn recorded_identity(metadata: &std::fs::Metadata) -> Option<(u64, u64)> {
    use std::os::unix::fs::MetadataExt;
    Some((metadata.dev(), metadata.ino()))
}

/// The identity a recursive delete records for an entry: none on platforms
/// without a portable device and inode.
#[cfg(not(unix))]
pub(crate) fn recorded_identity(_metadata: &std::fs::Metadata) -> Option<(u64, u64)> {
    None
}

/// Everything a recursive delete will back up and remove.
#[derive(Debug, Default)]
pub(crate) struct TreeManifest {
    /// Every recorded entry, each directory before its contents; the root
    /// directory is first.
    pub(crate) entries: Vec<TreeEntry>,
    pub(crate) unsupported: Vec<(PathBuf, UnsupportedKind)>,
    pub(crate) entries_counted: usize,
    pub(crate) bytes_counted: u64,
}

impl TreeManifest {
    /// Number of offending entries per kind, for the refusal log line.
    pub(crate) fn unsupported_counts(&self) -> Vec<(UnsupportedKind, usize)> {
        let mut counts: HashMap<UnsupportedKind, usize> = HashMap::new();
        for (_, kind) in &self.unsupported {
            *counts.entry(*kind).or_default() += 1;
        }
        let mut counts = counts.into_iter().collect::<Vec<_>>();
        counts.sort();
        counts
    }
}

/// Walk `root` (a real directory, not a symlink) into a manifest, stopping
/// the moment the tree needs more backup than `budget` allows. The walk must
/// stop at the cap rather than count the whole tree first: a tree large
/// enough to refuse can also be large enough that counting it is slow.
pub(crate) fn walk_tree(
    root: &Path,
    boundary: &crate::walk_boundary::DeviceBoundary,
    budget: &RecursiveDeleteBackupBudget,
) -> Result<TreeManifest, CollectError> {
    let mut walk = Walk {
        boundary,
        budget,
        manifest: TreeManifest::default(),
        seen_files: HashSet::new(),
    };
    walk.count_entry()?;
    let identity = recorded_identity(&std::fs::symlink_metadata(root)?);
    walk.manifest.entries.push(TreeEntry {
        path: root.to_path_buf(),
        kind: NodeKind::Directory,
        identity,
    });
    walk.walk_dir(root)?;
    Ok(walk.manifest)
}

struct Walk<'a> {
    boundary: &'a crate::walk_boundary::DeviceBoundary,
    budget: &'a RecursiveDeleteBackupBudget,
    manifest: TreeManifest,
    /// Files already counted toward the byte budget, so hard links to the
    /// same file are copied, and counted, once.
    seen_files: HashSet<(u64, u64)>,
}

impl Walk<'_> {
    fn count_entry(&mut self) -> Result<(), CollectError> {
        self.manifest.entries_counted += 1;
        if self.budget.enabled && self.manifest.entries_counted > self.budget.files_left {
            return Err(self.over_budget(BudgetLimit::Files));
        }
        Ok(())
    }

    fn count_bytes(&mut self, len: u64) -> Result<(), CollectError> {
        if self.budget.per_file_limit.is_some_and(|limit| len > limit) {
            return Ok(());
        }
        self.manifest.bytes_counted = self.manifest.bytes_counted.saturating_add(len);
        if self.budget.enabled && self.manifest.bytes_counted > self.budget.bytes_left {
            return Err(self.over_budget(BudgetLimit::Bytes));
        }
        Ok(())
    }

    fn over_budget(&self, limit: BudgetLimit) -> CollectError {
        CollectError::OverBudget(BudgetExceeded {
            limit,
            files_counted: self.manifest.entries_counted,
            bytes_counted: self.manifest.bytes_counted,
        })
    }

    fn walk_dir(&mut self, dir: &Path) -> Result<(), CollectError> {
        for entry in std::fs::read_dir(dir)? {
            if crate::executor::current_job_cancelled() {
                return Err(std::io::Error::from(std::io::ErrorKind::Interrupted).into());
            }
            let entry = entry?;
            let path = entry.path();
            // Neither `DirEntry::file_type` nor `DirEntry::metadata` follows
            // symlinks, so a link is recorded as a link and never entered.
            let file_type = entry.file_type()?;
            self.count_entry()?;
            // `DirEntry::metadata` does not follow a symlink either.
            let identity = recorded_identity(&entry.metadata()?);
            if file_type.is_dir() {
                if self.boundary.should_descend(&path)? {
                    self.manifest.entries.push(TreeEntry {
                        path: path.clone(),
                        kind: NodeKind::Directory,
                        identity,
                    });
                    self.walk_dir(&path)?;
                } else {
                    self.manifest
                        .unsupported
                        .push((path, UnsupportedKind::OtherFilesystem));
                }
            } else if file_type.is_symlink() {
                match symlink_support(&path)? {
                    None => self.manifest.entries.push(TreeEntry {
                        path,
                        kind: NodeKind::Symlink,
                        identity,
                    }),
                    Some(kind) => self.manifest.unsupported.push((path, kind)),
                }
            } else if file_type.is_file() {
                let metadata = entry.metadata()?;
                let (dev, ino, nlink) = file_identity(&metadata);
                if self.seen_files.insert((dev, ino)) || nlink <= 1 {
                    self.count_bytes(metadata.len())?;
                }
                self.manifest.entries.push(TreeEntry {
                    path,
                    kind: NodeKind::File { dev, ino, nlink },
                    identity,
                });
            } else {
                match special_file_kind(&file_type) {
                    None => self.manifest.entries.push(TreeEntry {
                        path,
                        kind: NodeKind::Socket,
                        identity,
                    }),
                    Some(kind) => self.manifest.unsupported.push((path, kind)),
                }
            }
        }
        Ok(())
    }
}

/// Returns `None` when undo can recreate this symlink exactly; otherwise the
/// reason exact restoration is unsupported.
pub(crate) fn symlink_support(path: &Path) -> std::io::Result<Option<UnsupportedKind>> {
    if cfg!(windows) {
        return Ok(Some(UnsupportedKind::WindowsSymlink));
    }
    let target = std::fs::read_link(path)?;
    Ok(target
        .to_str()
        .is_none()
        .then_some(UnsupportedKind::SymlinkNonUtf8Target))
}

/// `None` for a socket (deleted, reported as not restored); otherwise the
/// reason a special file is refused.
pub(crate) fn special_file_kind(file_type: &std::fs::FileType) -> Option<UnsupportedKind> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::FileTypeExt;
        if file_type.is_socket() {
            return None;
        }
        if file_type.is_fifo() {
            return Some(UnsupportedKind::Fifo);
        }
        if file_type.is_block_device() || file_type.is_char_device() {
            return Some(UnsupportedKind::Device);
        }
    }
    #[cfg(not(unix))]
    let _ = file_type;
    Some(UnsupportedKind::Other)
}

#[cfg(unix)]
fn file_identity(metadata: &std::fs::Metadata) -> (u64, u64, u64) {
    use std::os::unix::fs::MetadataExt;
    (metadata.dev(), metadata.ino(), metadata.nlink())
}

#[cfg(not(unix))]
fn file_identity(_metadata: &std::fs::Metadata) -> (u64, u64, u64) {
    // No portable identity: every file is treated as its only link.
    (0, 0, 1)
}

/// How a recorded regular file is backed up.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum FileBackup {
    /// Back up the content. `detached` marks a hard-linked file that also
    /// has links outside the tree: undo restores it as an independent copy.
    Content { detached: bool },
    /// Record a hard link to another path in the tree whose content is
    /// backed up; undo relinks.
    LinkTo(PathBuf),
}

/// Decide, for each regular file, whether to back up its content or record
/// it as a hard link to an earlier path with the same identity.
pub(crate) fn plan_file_backups(manifest: &TreeManifest) -> HashMap<PathBuf, FileBackup> {
    let mut groups: HashMap<(u64, u64), Vec<(&Path, u64)>> = HashMap::new();
    let mut plan = HashMap::new();
    for entry in &manifest.entries {
        if let NodeKind::File { dev, ino, nlink } = entry.kind {
            if nlink <= 1 {
                plan.insert(entry.path.clone(), FileBackup::Content { detached: false });
            } else {
                groups
                    .entry((dev, ino))
                    .or_default()
                    .push((&entry.path, nlink));
            }
        }
    }
    for members in groups.into_values() {
        let (first, nlink) = members[0];
        let detached = nlink > members.len() as u64;
        plan.insert(first.to_path_buf(), FileBackup::Content { detached });
        for (path, _) in &members[1..] {
            plan.insert(path.to_path_buf(), FileBackup::LinkTo(first.to_path_buf()));
        }
    }
    plan
}

/// Where and why deleting a recorded tree stopped early.
#[derive(Debug)]
pub(crate) struct DeleteStopped {
    pub(crate) path: PathBuf,
    pub(crate) reason: String,
    pub(crate) cancelled: bool,
    pub(crate) files_deleted: usize,
    pub(crate) directories_deleted: usize,
    /// An entry was no longer the object the walk recorded, so it and
    /// everything holding it were left in place.
    pub(crate) changed: bool,
    /// The tree's root could not be bound to an authorized location, so
    /// nothing was deleted: the response to return instead of a partial
    /// delete report.
    pub(crate) refusal: Option<crate::protocol::Response>,
}

/// Authorizes the located tree root and returns the identity the access
/// policy judged (the canonical parent plus the root's own name).
pub(crate) type AuthorizeRoot<'a> =
    &'a mut dyn FnMut(&Path) -> Result<PathBuf, crate::protocol::Response>;

/// The tree root's location changed between finding it and authorizing it.
#[cfg(any(unix, windows))]
fn root_changed(req_id: &str, root: &Path) -> DeleteStopped {
    refused(
        root,
        crate::protocol::Response::error(
            req_id,
            "path_changed",
            format!(
                "delete_file: '{}' changed while it was being deleted; nothing was deleted",
                root.display()
            ),
        ),
    )
}

fn refused(root: &Path, response: crate::protocol::Response) -> DeleteStopped {
    let mut error = stopped(root, "the tree root was not authorized");
    error.refusal = Some(response);
    error
}

/// Deletes are incremental, not an atomic commit: keep the job cancellable
/// through the last unlink. Explicit Cancel is read before every entry; root
/// abandonment (which probes disk) is checked every 64 entries instead of
/// adding a root stat to every leaf unlink. A syscall already in progress
/// cannot be interrupted by cooperative cancellation.
#[derive(Default)]
struct DeleteProgress {
    token: Option<crate::executor::JobCancellation>,
    checkpoints: usize,
    files: usize,
    directories: usize,
}

impl DeleteProgress {
    fn removed(&mut self, directory: bool) {
        if directory {
            self.directories += 1;
        } else {
            self.files += 1;
        }
        if let Some(token) = &self.token {
            token.report_progress();
        }
    }
    fn checkpoint(&mut self, path: &Path) -> Result<(), DeleteStopped> {
        #[cfg(test)]
        DELETE_OBSERVER.with(|observer| {
            if let Some(observer) = observer.borrow_mut().as_mut() {
                observer(self.files + self.directories);
            }
        });
        self.checkpoints += 1;
        if self.token.as_ref().is_some_and(|token| {
            token.cancel_already_requested()
                || (self.checkpoints % 64 == 1 && token.cancel_requested_before_commit())
        }) {
            let mut error = stopped(path, "request cancelled between entries");
            error.cancelled = true;
            return Err(error);
        }
        Ok(())
    }
}

#[cfg(test)]
thread_local! {
    pub(crate) static DELETE_OBSERVER: std::cell::RefCell<Option<Box<dyn FnMut(usize)>>> = Default::default();
}

/// Remove exactly the entries in `manifest`, deepest first. Each directory is
/// opened without following links, and entries are removed relative to that
/// open directory, so a directory swapped for a symlink during the delete is
/// never followed. A directory that is not empty once its recorded entries
/// are gone held something created after the walk: the delete stops there
/// and leaves it, and everything above it, in place.
///
/// The root is bound first, the same way a single-entry delete is: the
/// directory holding it (on Windows, the root itself) is opened and held,
/// its real location is passed to `authorize`, and the held object is
/// checked to still be the one the policy judged. Every removal then goes
/// through held handles. A refusal there deletes nothing and comes back in
/// [`DeleteStopped::refusal`]. The contract this implements is described on
/// [`crate::commands::delete_file::handle_delete_file`].
pub(crate) fn delete_recorded_tree(
    manifest: &TreeManifest,
    req_id: &str,
    authorize: AuthorizeRoot<'_>,
) -> Result<(), DeleteStopped> {
    let Some(root) = manifest.entries.first() else {
        return Ok(());
    };
    let mut progress = DeleteProgress {
        token: crate::executor::current_job_cancellation(),
        ..Default::default()
    };
    let mut children: HashMap<&Path, Vec<&TreeEntry>> = HashMap::new();
    for entry in &manifest.entries[1..] {
        progress.checkpoint(&entry.path)?;
        if let Some(parent) = entry.path.parent() {
            children.entry(parent).or_default().push(entry);
        }
    }
    #[cfg(test)]
    crate::commands::delete_file::delete_gate_for_test(&root.path);
    platform::delete_tree(root, &children, &mut progress, req_id, authorize).map_err(|mut error| {
        error.files_deleted = progress.files;
        error.directories_deleted = progress.directories;
        error
    })
}

fn stopped(path: &Path, reason: impl std::fmt::Display) -> DeleteStopped {
    DeleteStopped {
        path: path.to_path_buf(),
        reason: reason.to_string(),
        cancelled: false,
        files_deleted: 0,
        directories_deleted: 0,
        changed: false,
        refusal: None,
    }
}

/// `path` is no longer the object the walk recorded.
#[cfg(any(unix, windows))]
fn replaced(path: &Path) -> DeleteStopped {
    let mut error = stopped(
        path,
        "it was replaced after the delete started, so it and what holds it were left in place",
    );
    error.changed = true;
    error
}

#[cfg(unix)]
mod platform {
    use super::{
        refused, root_changed, stopped, AuthorizeRoot, DeleteProgress, DeleteStopped, NodeKind,
        TreeEntry,
    };
    #[cfg(test)]
    use crate::commands::delete_file::UnlinkStage;
    use std::collections::HashMap;
    use std::ffi::{CString, OsStr};
    use std::io;
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    use std::os::unix::ffi::OsStrExt;
    use std::path::Path;

    fn c_name(name: &OsStr) -> io::Result<CString> {
        CString::new(name.as_bytes()).map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))
    }

    /// Open `name` under `parent` as a directory. With `nofollow`, fail if it
    /// is a symlink.
    fn open_dir(parent: libc::c_int, name: &CString, nofollow: bool) -> io::Result<OwnedFd> {
        let mut flags = libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC;
        if nofollow {
            flags |= libc::O_NOFOLLOW;
        }
        // SAFETY: `name` is a valid NUL-terminated string and `parent` is an
        // open directory descriptor or AT_FDCWD.
        let fd = unsafe { libc::openat(parent, name.as_ptr(), flags) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: `fd` was just returned by openat and is owned here.
        Ok(unsafe { OwnedFd::from_raw_fd(fd) })
    }

    fn unlink(parent: &OwnedFd, name: &CString, directory: bool) -> io::Result<()> {
        #[cfg(test)]
        crate::commands::delete_file::count_unlink_for_test();
        let flags = if directory { libc::AT_REMOVEDIR } else { 0 };
        // SAFETY: valid descriptor and NUL-terminated name.
        if unsafe { libc::unlinkat(parent.as_raw_fd(), name.as_ptr(), flags) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    /// The entry's current type and identity, without following a symlink.
    fn stat_entry(parent: &OwnedFd, name: &CString) -> io::Result<libc::stat> {
        // SAFETY: zeroed stat is a valid out-parameter for fstatat.
        let mut stat: libc::stat = unsafe { std::mem::zeroed() };
        // SAFETY: valid descriptor, NUL-terminated name, writable stat.
        let result = unsafe {
            libc::fstatat(
                parent.as_raw_fd(),
                name.as_ptr(),
                &mut stat,
                libc::AT_SYMLINK_NOFOLLOW,
            )
        };
        if result != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(stat)
    }

    /// Whether the entry on disk is still the one the walk recorded: the same
    /// type and, where the walk recorded one, the same device and inode.
    fn matches_record(stat: &libc::stat, entry: &TreeEntry) -> bool {
        let format = stat.st_mode & libc::S_IFMT;
        let same_kind = match entry.kind {
            NodeKind::File { dev, ino, .. } => {
                format == libc::S_IFREG && stat.st_dev as u64 == dev && stat.st_ino as u64 == ino
            }
            NodeKind::Symlink => format == libc::S_IFLNK,
            NodeKind::Socket => format == libc::S_IFSOCK,
            NodeKind::Directory => format == libc::S_IFDIR,
            NodeKind::Unbacked => format != libc::S_IFDIR,
        };
        same_kind && same_identity(stat, entry)
    }

    fn same_identity(stat: &libc::stat, entry: &TreeEntry) -> bool {
        entry
            .identity
            .is_none_or(|(dev, ino)| stat.st_dev as u64 == dev && stat.st_ino as u64 == ino)
    }

    fn fstat(fd: &OwnedFd) -> io::Result<libc::stat> {
        let mut stat: libc::stat = unsafe { std::mem::zeroed() };
        // SAFETY: live descriptor and a writable stat out-parameter.
        if unsafe { libc::fstat(fd.as_raw_fd(), &mut stat) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(stat)
    }

    pub(super) fn delete_tree(
        root_entry: &TreeEntry,
        children: &HashMap<&Path, Vec<&TreeEntry>>,
        progress: &mut DeleteProgress,
        req_id: &str,
        authorize: AuthorizeRoot<'_>,
    ) -> Result<(), DeleteStopped> {
        use std::os::unix::fs::MetadataExt;
        let root = root_entry.path.as_path();
        progress.checkpoint(root)?;
        let (Some(parent), Some(root_name)) = (root.parent(), root.file_name()) else {
            return Err(stopped(root, "cannot delete a filesystem root"));
        };
        let parent = if parent.as_os_str().is_empty() {
            Path::new(".")
        } else {
            parent
        };
        let parent_name = c_name(parent.as_os_str()).map_err(|e| stopped(parent, e))?;
        #[cfg(test)]
        crate::commands::delete_file::unlink_seam(UnlinkStage::BeforePin);
        // The parent may legitimately be reached through a symlink (such as
        // /tmp on macOS), so it is opened by path and then bound below; the
        // tree itself, from its root down, is never entered through one.
        let parent_fd =
            open_dir(libc::AT_FDCWD, &parent_name, false).map_err(|e| stopped(parent, e))?;
        // Bind the held parent to an authorized location, as a single-entry
        // delete does: find where it is, authorize the root there, then check
        // that the policy judged that directory and that it is still the one
        // held open.
        let pinned = fstat(&parent_fd).map_err(|e| stopped(parent, e))?;
        let held = |path: &Path| {
            std::fs::metadata(path).is_ok_and(|current| {
                current.dev() == pinned.st_dev as u64 && current.ino() == pinned.st_ino as u64
            })
        };
        let located = std::fs::canonicalize(parent).map_err(|_| root_changed(req_id, root))?;
        if !held(&located) {
            return Err(root_changed(req_id, root));
        }
        #[cfg(test)]
        crate::commands::delete_file::unlink_seam(UnlinkStage::BeforeAuthorize);
        let judged =
            authorize(&located.join(root_name)).map_err(|response| refused(root, response))?;
        #[cfg(test)]
        crate::commands::delete_file::unlink_seam(UnlinkStage::AfterAuthorize);
        if judged.parent() != Some(located.as_path()) || !held(&located) {
            return Err(root_changed(req_id, root));
        }
        #[cfg(test)]
        crate::commands::delete_file::unlink_seam(UnlinkStage::BeforeUnlink);
        // The held parent may since have been moved, or its tree swapped for
        // another under the same name. Only the root the walk recorded (and
        // backed up) is deleted: anything else is refused before any unlink.
        let name = c_name(root_name).map_err(|e| stopped(root, e))?;
        let root_fd = open_dir(parent_fd.as_raw_fd(), &name, true).map_err(|e| stopped(root, e))?;
        let opened = fstat(&root_fd).map_err(|e| stopped(root, e))?;
        if !same_identity(&opened, root_entry) {
            return Err(root_changed(req_id, root));
        }
        delete_contents(&root_fd, root, children, progress)?;
        drop(root_fd);
        progress.checkpoint(root)?;
        if remove_recorded_dir(&parent_fd, &name, root_entry)? {
            progress.removed(true);
        }
        Ok(())
    }

    /// Remove the empty directory `name` under `parent` only if it is still
    /// the recorded directory. The check and the removal are two system
    /// calls; within the held parent nothing narrower exists.
    /// Returns whether a directory was removed.
    fn remove_recorded_dir(
        parent: &OwnedFd,
        name: &CString,
        entry: &TreeEntry,
    ) -> Result<bool, DeleteStopped> {
        match stat_entry(parent, name) {
            Ok(stat) if !matches_record(&stat, entry) => return Err(super::replaced(&entry.path)),
            Ok(_) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(false),
            Err(e) => return Err(stopped(&entry.path, e)),
        }
        match unlink(parent, name, true) {
            Ok(()) => Ok(true),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(stopped(&entry.path, not_empty_reason(e))),
        }
    }

    fn not_empty_reason(error: io::Error) -> String {
        if error.raw_os_error() == Some(libc::ENOTEMPTY)
            || error.raw_os_error() == Some(libc::EEXIST)
        {
            "it contains an entry created after the delete started, which has no backup".to_string()
        } else {
            error.to_string()
        }
    }

    /// Remove the recorded entries under the held directory `dir_fd`. Each
    /// entry, unbacked leaves included, is removed only while it is still the
    /// recorded object (type and, where recorded, device and inode); a
    /// directory is also checked once opened, so a swap between the check and
    /// the open is never entered. The first entry that is not the recorded
    /// object stops the delete there.
    fn delete_contents(
        dir_fd: &OwnedFd,
        dir: &Path,
        children: &HashMap<&Path, Vec<&TreeEntry>>,
        progress: &mut DeleteProgress,
    ) -> Result<(), DeleteStopped> {
        for entry in children.get(dir).into_iter().flatten() {
            progress.checkpoint(&entry.path)?;
            let Some(file_name) = entry.path.file_name() else {
                continue;
            };
            let name = c_name(file_name).map_err(|e| stopped(&entry.path, e))?;
            let stat = match stat_entry(dir_fd, &name) {
                Ok(stat) => stat,
                // Already gone: nothing left to remove.
                Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
                Err(e) => return Err(stopped(&entry.path, e)),
            };
            if !matches_record(&stat, entry) {
                return Err(super::replaced(&entry.path));
            }
            if entry.kind == NodeKind::Directory {
                let child_fd = match open_dir(dir_fd.as_raw_fd(), &name, true) {
                    Ok(fd) => fd,
                    Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
                    Err(e) => return Err(stopped(&entry.path, e)),
                };
                let child_stat = fstat(&child_fd).map_err(|e| stopped(&entry.path, e))?;
                if !same_identity(&child_stat, entry) {
                    return Err(super::replaced(&entry.path));
                }
                // Recheck the opened directory's device as well as preflight:
                // a mount introduced during the walk must never be entered.
                let parent_stat = fstat(dir_fd).map_err(|e| stopped(&entry.path, e))?;
                if child_stat.st_dev != parent_stat.st_dev {
                    return Err(stopped(
                        &entry.path,
                        "directory now belongs to another filesystem",
                    ));
                }
                delete_contents(&child_fd, &entry.path, children, progress)?;
                drop(child_fd);
                progress.checkpoint(&entry.path)?;
                if remove_recorded_dir(dir_fd, &name, entry)? {
                    progress.removed(true);
                }
            } else {
                // unlinkat does not follow a leaf symlink, and without
                // AT_REMOVEDIR refuses a directory swapped in since the check.
                match unlink(dir_fd, &name, false) {
                    Ok(()) => progress.removed(false),
                    Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                    Err(e) => return Err(stopped(&entry.path, e)),
                }
            }
        }
        Ok(())
    }
}

/// Windows: every entry is opened by handle without following a final link
/// (`FILE_FLAG_OPEN_REPARSE_POINT`) and removed by setting a delete
/// disposition on that handle. A tree's entries are opened relative to their
/// parent's handle, never by path. Nothing is removed by path.
#[cfg(windows)]
pub(crate) mod win {
    use std::fs::File;
    use std::io;
    use std::path::{Path, PathBuf};

    const DELETE: u32 = 0x0001_0000;
    const FILE_READ_ATTRIBUTES: u32 = 0x0000_0080;
    const FILE_SHARE_ALL: u32 = 0x0000_0007;
    const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
    const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
    const FILE_DISPOSITION_INFO: i32 = 4;
    const FILE_DISPOSITION_INFO_EX: i32 = 21;
    const FILE_DISPOSITION_FLAG_DELETE: u32 = 0x0000_0001;
    const FILE_DISPOSITION_FLAG_POSIX_SEMANTICS: u32 = 0x0000_0002;

    #[link(name = "kernel32")]
    extern "system" {
        fn SetFileInformationByHandle(
            file: std::os::windows::io::RawHandle,
            class: i32,
            information: *mut std::ffi::c_void,
            size: u32,
        ) -> i32;
    }

    const SYNCHRONIZE: u32 = 0x0010_0000;
    const FILE_SYNCHRONOUS_IO_NONALERT: u32 = 0x0000_0020;
    const FILE_OPEN_FOR_BACKUP_INTENT: u32 = 0x0000_4000;
    const FILE_OPEN_REPARSE_POINT: u32 = 0x0020_0000;

    #[repr(C)]
    struct UnicodeString {
        length: u16,
        maximum_length: u16,
        buffer: *mut u16,
    }

    #[repr(C)]
    struct ObjectAttributes {
        length: u32,
        root_directory: std::os::windows::io::RawHandle,
        object_name: *mut UnicodeString,
        attributes: u32,
        security_descriptor: *mut std::ffi::c_void,
        security_quality_of_service: *mut std::ffi::c_void,
    }

    #[repr(C)]
    struct IoStatusBlock {
        status_or_pointer: usize,
        information: usize,
    }

    #[link(name = "ntdll")]
    extern "system" {
        fn NtOpenFile(
            handle: *mut std::os::windows::io::RawHandle,
            access: u32,
            object: *mut ObjectAttributes,
            io_status: *mut IoStatusBlock,
            share: u32,
            options: u32,
        ) -> i32;
        fn RtlNtStatusToDosError(status: i32) -> u32;
    }

    /// Open the entry `name` inside the open directory `parent`, resolved by
    /// the kernel relative to that handle, never through a path. A final
    /// link is opened itself (`FILE_OPEN_REPARSE_POINT`), with the access a
    /// delete needs; directories open too (`FILE_OPEN_FOR_BACKUP_INTENT`).
    pub(crate) fn open_child(parent: &File, name: &std::ffi::OsStr) -> io::Result<File> {
        use std::os::windows::ffi::OsStrExt;
        use std::os::windows::io::{AsRawHandle, FromRawHandle};
        let mut wide: Vec<u16> = name.encode_wide().collect();
        if wide.is_empty()
            || wide
                .iter()
                .any(|&unit| unit == u16::from(b'\\') || unit == u16::from(b'/'))
        {
            return Err(io::Error::from(io::ErrorKind::InvalidInput));
        }
        let bytes = u16::try_from(wide.len() * 2)
            .map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))?;
        let mut unicode = UnicodeString {
            length: bytes,
            maximum_length: bytes,
            buffer: wide.as_mut_ptr(),
        };
        let mut object = ObjectAttributes {
            length: std::mem::size_of::<ObjectAttributes>() as u32,
            root_directory: parent.as_raw_handle(),
            object_name: &mut unicode,
            attributes: 0,
            security_descriptor: std::ptr::null_mut(),
            security_quality_of_service: std::ptr::null_mut(),
        };
        let mut handle = std::ptr::null_mut();
        let mut io_status = std::mem::MaybeUninit::<IoStatusBlock>::uninit();
        // SAFETY: every pointer refers to a live local for the duration of
        // the call; `parent` stays open, and `name` is a single component.
        let status = unsafe {
            NtOpenFile(
                &mut handle,
                DELETE | FILE_READ_ATTRIBUTES | SYNCHRONIZE,
                &mut object,
                io_status.as_mut_ptr(),
                FILE_SHARE_ALL,
                FILE_SYNCHRONOUS_IO_NONALERT
                    | FILE_OPEN_FOR_BACKUP_INTENT
                    | FILE_OPEN_REPARSE_POINT,
            )
        };
        if status < 0 {
            // SAFETY: plain status conversion.
            return Err(io::Error::from_raw_os_error(
                unsafe { RtlNtStatusToDosError(status) } as i32,
            ));
        }
        // SAFETY: NtOpenFile succeeded and returned an owned handle.
        Ok(unsafe { File::from_raw_handle(handle) })
    }

    /// Open the entry at `path` itself, never a link's target, with the
    /// access a delete needs. Directories open too (`BACKUP_SEMANTICS`).
    pub(crate) fn open_entry(path: &Path) -> io::Result<File> {
        use std::os::windows::fs::OpenOptionsExt;
        std::fs::OpenOptions::new()
            .access_mode(DELETE | FILE_READ_ATTRIBUTES)
            .share_mode(FILE_SHARE_ALL)
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_BACKUP_SEMANTICS)
            .open(path)
    }

    /// Where the open entry is now, in the form the access policy uses for
    /// its identities (no `\\?\` prefix). With the link itself opened, this
    /// names the link entry, not what it points at.
    pub(crate) fn location(entry: &File) -> io::Result<PathBuf> {
        crate::commands::file_read_exact::opened_location(entry)
            .map(|path| crate::inspect::job::normalize_path(&path))
    }

    /// Delete the open entry through its handle: POSIX semantics where the
    /// volume supports them (the name goes away now), otherwise
    /// delete-on-close (the name goes away when the handle is dropped).
    ///
    /// The disposition applies to the object the handle holds, wherever it is
    /// now. That object was the one authorized; if it was renamed since the
    /// last check, deleting it is still within the delete contract (objects
    /// are authorized, not pathnames at the instant of unlink).
    pub(crate) fn dispose(entry: &File) -> io::Result<()> {
        use std::os::windows::io::AsRawHandle;
        let handle = entry.as_raw_handle();
        let mut flags = FILE_DISPOSITION_FLAG_DELETE | FILE_DISPOSITION_FLAG_POSIX_SEMANTICS;
        // SAFETY: `entry` is open for the call and the buffer matches the
        // size given for FILE_DISPOSITION_INFO_EX.
        let posix = unsafe {
            SetFileInformationByHandle(
                handle,
                FILE_DISPOSITION_INFO_EX,
                (&mut flags as *mut u32).cast(),
                std::mem::size_of_val(&flags) as u32,
            )
        };
        if posix != 0 {
            return Ok(());
        }
        let error = io::Error::last_os_error();
        // ERROR_NOT_SUPPORTED or ERROR_INVALID_PARAMETER: no POSIX delete on
        // this volume. Anything else (access, sharing, not empty) is real.
        if !matches!(error.raw_os_error(), Some(50 | 87)) {
            return Err(error);
        }
        let mut delete_file = 1_u8;
        // SAFETY: as above; FILE_DISPOSITION_INFO is a single BOOLEAN.
        let legacy = unsafe {
            SetFileInformationByHandle(
                handle,
                FILE_DISPOSITION_INFO,
                (&mut delete_file as *mut u8).cast(),
                std::mem::size_of_val(&delete_file) as u32,
            )
        };
        if legacy == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
}

#[cfg(windows)]
mod platform {
    use super::win::{dispose, location, open_child, open_entry};
    use super::{
        refused, replaced, root_changed, stopped, AuthorizeRoot, DeleteProgress, DeleteStopped,
        NodeKind, TreeEntry,
    };
    #[cfg(test)]
    use crate::commands::delete_file::UnlinkStage;
    use std::collections::HashMap;
    use std::fs::File;
    use std::path::Path;

    /// The root is opened by handle, its real location authorized, and the
    /// handle checked to still sit at the judged location. Each entry below is
    /// then opened relative to its parent's handle and removed through its own
    /// handle; directories once empty.
    pub(super) fn delete_tree(
        root_entry: &TreeEntry,
        children: &HashMap<&Path, Vec<&TreeEntry>>,
        progress: &mut DeleteProgress,
        req_id: &str,
        authorize: AuthorizeRoot<'_>,
    ) -> Result<(), DeleteStopped> {
        let root = root_entry.path.as_path();
        progress.checkpoint(root)?;
        #[cfg(test)]
        crate::commands::delete_file::unlink_seam(UnlinkStage::BeforePin);
        let root_handle = open_entry(root).map_err(|e| stopped(root, e))?;
        let metadata = root_handle.metadata().map_err(|e| stopped(root, e))?;
        if !metadata.is_dir() {
            return Err(root_changed(req_id, root));
        }
        let located = location(&root_handle).map_err(|_| root_changed(req_id, root))?;
        #[cfg(test)]
        crate::commands::delete_file::unlink_seam(UnlinkStage::BeforeAuthorize);
        let judged = authorize(&located).map_err(|response| refused(root, response))?;
        #[cfg(test)]
        crate::commands::delete_file::unlink_seam(UnlinkStage::AfterAuthorize);
        if crate::inspect::job::normalize_path(&judged) != located
            || location(&root_handle).ok().as_deref() != Some(located.as_path())
        {
            return Err(root_changed(req_id, root));
        }
        #[cfg(test)]
        crate::commands::delete_file::unlink_seam(UnlinkStage::BeforeUnlink);
        delete_contents(&root_handle, root, children, progress)?;
        progress.checkpoint(root)?;
        #[cfg(test)]
        crate::commands::delete_file::count_unlink_for_test();
        dispose(&root_handle).map_err(|e| stopped(root, not_empty_reason(e)))?;
        drop(root_handle);
        progress.removed(true);
        Ok(())
    }

    fn not_empty_reason(error: std::io::Error) -> String {
        // ERROR_DIR_NOT_EMPTY
        if error.raw_os_error() == Some(145) {
            "it contains an entry created after the delete started, which has no backup".to_string()
        } else {
            error.to_string()
        }
    }

    /// Remove the entries recorded under `recorded` through `dir`, the open
    /// handle of that directory. Each child is opened relative to `dir`, so a
    /// path swapped elsewhere in the tree cannot redirect it.
    fn delete_contents(
        dir: &File,
        recorded: &Path,
        children: &HashMap<&Path, Vec<&TreeEntry>>,
        progress: &mut DeleteProgress,
    ) -> Result<(), DeleteStopped> {
        for entry in children.get(recorded).into_iter().flatten() {
            progress.checkpoint(&entry.path)?;
            let Some(name) = entry.path.file_name() else {
                continue;
            };
            let handle = match open_child(dir, name) {
                Ok(handle) => handle,
                // Already gone: nothing left to remove.
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                Err(e) => return Err(stopped(&entry.path, e)),
            };
            let metadata = handle.metadata().map_err(|e| stopped(&entry.path, e))?;
            let is_link = metadata.file_type().is_symlink();
            match entry.kind {
                NodeKind::Directory if metadata.is_dir() && !is_link => {
                    delete_contents(&handle, &entry.path, children, progress)?;
                    progress.checkpoint(&entry.path)?;
                    #[cfg(test)]
                    crate::commands::delete_file::count_unlink_for_test();
                    dispose(&handle).map_err(|e| stopped(&entry.path, not_empty_reason(e)))?;
                    progress.removed(true);
                }
                NodeKind::Directory => return Err(replaced(&entry.path)),
                _ if metadata.is_dir() && !is_link => return Err(replaced(&entry.path)),
                _ => {
                    #[cfg(test)]
                    crate::commands::delete_file::count_unlink_for_test();
                    dispose(&handle).map_err(|e| stopped(&entry.path, e))?;
                    progress.removed(false);
                }
            }
        }
        Ok(())
    }
}

/// Targets that are neither Unix nor Windows have no handle-bound delete, so
/// a recursive delete is refused rather than done by path.
#[cfg(not(any(unix, windows)))]
mod platform {
    use super::{refused, AuthorizeRoot, DeleteProgress, DeleteStopped, TreeEntry};
    use std::collections::HashMap;
    use std::path::Path;

    pub(super) fn delete_tree(
        root_entry: &TreeEntry,
        _children: &HashMap<&Path, Vec<&TreeEntry>>,
        _progress: &mut DeleteProgress,
        req_id: &str,
        _authorize: AuthorizeRoot<'_>,
    ) -> Result<(), DeleteStopped> {
        let root = root_entry.path.as_path();
        Err(refused(
            root,
            crate::protocol::Response::error(
                req_id,
                "delete_unsupported",
                format!(
                    "delete_file: '{}' cannot be deleted safely on this platform",
                    root.display()
                ),
            ),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backup::BackupPolicy;
    use crate::walk_boundary::DeviceBoundary;

    fn write_tree(root: &Path, files: usize, bytes_each: usize) {
        for index in 0..files {
            let dir = root.join(format!("d{}", index / 50));
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join(format!("f{index}")), vec![b'x'; bytes_each]).unwrap();
        }
    }

    fn walk(
        root: &Path,
        policy: BackupPolicy,
        max_files: usize,
        max_bytes: u64,
    ) -> Result<TreeManifest, CollectError> {
        let boundary = DeviceBoundary::for_root(root).unwrap();
        let budget = RecursiveDeleteBackupBudget::new(policy, max_files, max_bytes);
        walk_tree(root, &boundary, &budget)
    }

    fn files(manifest: &TreeManifest) -> usize {
        manifest
            .entries
            .iter()
            .filter(|entry| matches!(entry.kind, NodeKind::File { .. }))
            .count()
    }

    #[test]
    fn entry_budget_stops_the_walk_one_past_the_cap() {
        let dir = tempfile::tempdir().unwrap();
        write_tree(dir.path(), 1_500, 8);

        let Err(CollectError::OverBudget(exceeded)) =
            walk(dir.path(), BackupPolicy::default(), 1_000, u64::MAX)
        else {
            panic!("a 1,500-file tree must exceed a 1,000-entry budget");
        };
        assert_eq!(exceeded.limit, BudgetLimit::Files);
        // Counting the whole tree before comparing would report 1,500 or more.
        assert_eq!(exceeded.files_counted, 1_001);
    }

    #[test]
    fn byte_budget_counts_only_files_the_store_would_copy() {
        let dir = tempfile::tempdir().unwrap();
        write_tree(dir.path(), 200, 1_024);

        let Err(CollectError::OverBudget(exceeded)) =
            walk(dir.path(), BackupPolicy::default(), usize::MAX, 100 * 1_024)
        else {
            panic!("200 KiB of files must exceed a 100 KiB budget");
        };
        assert_eq!(exceeded.limit, BudgetLimit::Bytes);
        assert_eq!(exceeded.bytes_counted, 101 * 1_024);

        // With a per-file limit below every file's size the store copies
        // nothing, so the same tree fits a byte budget of zero.
        let small_files_only = BackupPolicy {
            max_file_size: Some(512),
            ..BackupPolicy::default()
        };
        let manifest = walk(dir.path(), small_files_only, usize::MAX, 0).unwrap();
        assert_eq!(files(&manifest), 200);
        assert_eq!(manifest.bytes_counted, 0);
    }

    #[test]
    fn tree_within_budget_records_every_file_and_directory() {
        let dir = tempfile::tempdir().unwrap();
        write_tree(dir.path(), 120, 16);

        // 120 files in 3 directories, plus the root.
        let manifest = walk(dir.path(), BackupPolicy::default(), 124, 120 * 16).unwrap();
        assert_eq!(files(&manifest), 120);
        assert_eq!(manifest.entries_counted, 124);
        assert_eq!(manifest.entries[0].path, dir.path());
        assert_eq!(manifest.bytes_counted, 120 * 16);
    }

    #[test]
    fn disabled_backups_leave_the_walk_unbounded() {
        let dir = tempfile::tempdir().unwrap();
        write_tree(dir.path(), 30, 16);

        let disabled = BackupPolicy {
            enabled: false,
            ..BackupPolicy::default()
        };
        let manifest = walk(dir.path(), disabled, 1, 1).unwrap();
        assert_eq!(files(&manifest), 30);
    }

    #[cfg(unix)]
    #[test]
    fn hard_links_count_their_bytes_once_and_link_to_the_first_path() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a"), vec![b'x'; 100]).unwrap();
        std::fs::hard_link(dir.path().join("a"), dir.path().join("b")).unwrap();

        let manifest = walk(dir.path(), BackupPolicy::default(), 10, 100).unwrap();
        assert_eq!(manifest.bytes_counted, 100);
        let plan = plan_file_backups(&manifest);
        let contents = plan
            .values()
            .filter(|backup| **backup == FileBackup::Content { detached: false })
            .count();
        let links = plan
            .values()
            .filter(|backup| matches!(backup, FileBackup::LinkTo(_)))
            .count();
        assert_eq!((contents, links), (1, 1), "{plan:?}");
    }

    #[cfg(unix)]
    #[test]
    fn a_hard_link_with_a_link_outside_the_tree_is_detached() {
        let outside = tempfile::tempdir().unwrap();
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("a"), "x").unwrap();
        std::fs::hard_link(outside.path().join("a"), dir.path().join("a")).unwrap();

        let manifest = walk(dir.path(), BackupPolicy::default(), 10, 100).unwrap();
        let plan = plan_file_backups(&manifest);
        assert_eq!(
            plan.get(&dir.path().join("a")),
            Some(&FileBackup::Content { detached: true })
        );
    }

    /// An entry created after the walk has no backup, so the delete must not
    /// remove it: it stops at the directory holding it, leaves that directory
    /// and the root in place, and still removes what it recorded elsewhere.
    #[test]
    fn an_entry_created_after_the_walk_stops_the_delete() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("tree");
        std::fs::create_dir_all(root.join("a")).unwrap();
        std::fs::create_dir_all(root.join("b")).unwrap();
        std::fs::write(root.join("a/recorded.txt"), "recorded").unwrap();
        std::fs::write(root.join("b/recorded.txt"), "recorded").unwrap();

        let manifest = walk(&root, BackupPolicy::default(), 100, 1_000).unwrap();
        std::fs::write(root.join("a/late.txt"), "created after the walk").unwrap();

        let stopped = delete_recorded_tree(&manifest, "test", &mut |root| Ok(root.to_path_buf()))
            .unwrap_err();
        assert_eq!(stopped.path, root.join("a"), "{stopped:?}");
        assert_eq!(
            std::fs::read_to_string(root.join("a/late.txt")).unwrap(),
            "created after the walk"
        );
        assert!(!root.join("a/recorded.txt").exists());
    }

    #[cfg(unix)]
    #[test]
    fn deleting_a_recorded_tree_never_follows_a_symlink() {
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("keep.txt"), "outside").unwrap();
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("tree");
        std::fs::create_dir_all(&root).unwrap();
        std::os::unix::fs::symlink(outside.path(), root.join("link")).unwrap();

        let manifest = walk(&root, BackupPolicy::default(), 100, 1_000).unwrap();
        delete_recorded_tree(&manifest, "test", &mut |root| Ok(root.to_path_buf())).unwrap();
        assert!(!root.exists());
        assert_eq!(
            std::fs::read_to_string(outside.path().join("keep.txt")).unwrap(),
            "outside"
        );
    }
}
