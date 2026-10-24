#[cfg(unix)]
use std::ffi::CString;
use std::ffi::{OsStr, OsString};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
#[cfg(unix)]
use std::os::fd::{AsRawFd, FromRawFd, RawFd};
#[cfg(unix)]
use std::os::unix::ffi::{OsStrExt, OsStringExt};
#[cfg(unix)]
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
#[cfg(windows)]
use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
#[cfg(windows)]
use std::os::windows::io::{AsRawHandle, FromRawHandle};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::backup::hash_session;
use crate::bash_permissions::PermissionAsk;
use crate::db::bash_tasks::BashTaskRow;

use super::process::LiveDescendant;
use super::BgTaskStatus;

#[cfg(test)]
type SessionCreateGate = (std::sync::mpsc::Sender<()>, std::sync::mpsc::Receiver<()>);

#[cfg(test)]
static SESSION_CREATE_GATES: std::sync::OnceLock<
    std::sync::Mutex<std::collections::HashMap<PathBuf, SessionCreateGate>>,
> = std::sync::OnceLock::new();

#[cfg(test)]
fn install_session_create_gate_for_test(
    session_dir: &Path,
) -> (std::sync::mpsc::Receiver<()>, std::sync::mpsc::Sender<()>) {
    let (entered_tx, entered_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let previous = SESSION_CREATE_GATES
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(session_dir.to_path_buf(), (entered_tx, release_rx));
    assert!(previous.is_none(), "session create gate already installed");
    (entered_rx, release_tx)
}

#[cfg(test)]
fn wait_on_session_create_gate_for_test(session_dir: &Path) {
    let gate = SESSION_CREATE_GATES
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .remove(session_dir);
    if let Some((entered, release)) = gate {
        let _ = entered.send(());
        let _ = release.recv();
    }
}

pub const SCHEMA_VERSION: u32 = 7;
pub(crate) const CONTROL_DIR: &str = "control";
const IO_DIR: &str = "io";
pub(crate) const METADATA_FILE: &str = "metadata.json";
const TASK_LAYOUT_NOT_FOUND: &str = "background task layout not found";
pub const COMMAND_FILE: &str = "command.sh";
pub const WRAPPER_FILE: &str = "wrapper.sh";
pub const ENVIRONMENT_FILE: &str = "environment.bin";
pub const MANIFEST_FILE: &str = "manifest.blake3";
pub const SANDBOX_PROFILE_FILE: &str = "sandbox-profile.json";

// Directory-layout and legacy flat-layout names share one visibility list.
// The launch environment is deliberately absent: it can contain call secrets.
const READABLE_CONTROL_FILES: [(&str, &str); 5] = [
    (METADATA_FILE, "json"),
    (COMMAND_FILE, "sh"),
    (WRAPPER_FILE, "wrapper.sh"),
    (MANIFEST_FILE, "manifest"),
    (SANDBOX_PROFILE_FILE, "sandbox-profile.json"),
];

pub fn readable_task_file_names(layout: TaskLayout) -> Vec<String> {
    READABLE_CONTROL_FILES
        .iter()
        .map(|(directory, flat)| match layout {
            TaskLayout::Directory => format!("{CONTROL_DIR}/{directory}"),
            TaskLayout::Flat => flat.to_string(),
        })
        .chain(TaskArtifact::ALL.into_iter().map(|artifact| match layout {
            TaskLayout::Directory => format!("{IO_DIR}/{}", artifact.file_name()),
            TaskLayout::Flat => artifact.flat_extension().to_string(),
        }))
        .collect()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskLayout {
    Flat,
    Directory,
}

#[derive(Debug, Clone)]
pub struct TaskPaths {
    pub layout: TaskLayout,
    pub task_id: String,
    pub session_dir: PathBuf,
    /// Root directory for this task's persisted artifacts; legacy flat-layout tasks
    /// use the session directory instead of a per-task directory.
    pub dir: PathBuf,
    pub control_dir: PathBuf,
    pub io_dir: PathBuf,
    pub json: PathBuf,
    pub stdout: PathBuf,
    pub stderr: PathBuf,
    pub exit: PathBuf,
    pub pipeline_status: PathBuf,
    pub pty: PathBuf,
    pub sandbox_unavailable: PathBuf,
    pub command: PathBuf,
    pub wrapper: PathBuf,
    pub environment: PathBuf,
    pub manifest: PathBuf,
    pub sandbox_profile: PathBuf,
}

impl TaskPaths {
    fn directory(session_dir: PathBuf, task_id: &str) -> Self {
        let dir = session_dir.join(task_id);
        let control_dir = dir.join(CONTROL_DIR);
        let io_dir = dir.join(IO_DIR);
        Self {
            layout: TaskLayout::Directory,
            task_id: task_id.to_string(),
            session_dir,
            dir,
            json: control_dir.join(METADATA_FILE),
            stdout: io_dir.join(TaskArtifact::Stdout.file_name()),
            stderr: io_dir.join(TaskArtifact::Stderr.file_name()),
            exit: io_dir.join(TaskArtifact::Exit.file_name()),
            pipeline_status: io_dir.join(TaskArtifact::PipelineStatus.file_name()),
            pty: io_dir.join(TaskArtifact::Pty.file_name()),
            sandbox_unavailable: io_dir.join(TaskArtifact::SandboxUnavailable.file_name()),
            command: control_dir.join(COMMAND_FILE),
            wrapper: control_dir.join(WRAPPER_FILE),
            environment: control_dir.join(ENVIRONMENT_FILE),
            manifest: control_dir.join(MANIFEST_FILE),
            sandbox_profile: control_dir.join(SANDBOX_PROFILE_FILE),
            control_dir,
            io_dir,
        }
    }

    fn flat(session_dir: PathBuf, task_id: &str) -> Self {
        let prefix = |extension: &str| session_dir.join(format!("{task_id}.{extension}"));
        Self {
            layout: TaskLayout::Flat,
            task_id: task_id.to_string(),
            dir: session_dir.clone(),
            control_dir: session_dir.clone(),
            io_dir: session_dir.clone(),
            json: prefix("json"),
            stdout: prefix("stdout"),
            stderr: prefix("stderr"),
            exit: prefix("exit"),
            pipeline_status: prefix("pipeline-status"),
            pty: prefix("pty"),
            sandbox_unavailable: prefix("sandbox-unavailable"),
            command: prefix("sh"),
            wrapper: prefix("wrapper.sh"),
            environment: prefix("env"),
            manifest: prefix("manifest"),
            sandbox_profile: prefix("sandbox-profile.json"),
            session_dir,
        }
    }

    pub fn artifact_path(&self, artifact: TaskArtifact) -> &Path {
        match artifact {
            TaskArtifact::Stdout => &self.stdout,
            TaskArtifact::Stderr => &self.stderr,
            TaskArtifact::Exit => &self.exit,
            TaskArtifact::PipelineStatus => &self.pipeline_status,
            TaskArtifact::Pty => &self.pty,
            TaskArtifact::SandboxUnavailable => &self.sandbox_unavailable,
        }
    }

    /// Exact bundle files visible to authorized readers. The launch environment
    /// is intentionally absent even when the caller may inspect task records.
    pub(crate) fn readable_files(&self) -> impl Iterator<Item = PathBuf> + '_ {
        readable_task_file_names(self.layout)
            .into_iter()
            .map(|name| match self.layout {
                TaskLayout::Directory => self.dir.join(name),
                TaskLayout::Flat => self.session_dir.join(format!("{}.{name}", self.task_id)),
            })
    }

    fn artifact_name(&self, artifact: TaskArtifact) -> OsString {
        match self.layout {
            TaskLayout::Directory => OsString::from(artifact.file_name()),
            TaskLayout::Flat => {
                OsString::from(format!("{}.{}", self.task_id, artifact.flat_extension()))
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TaskArtifact {
    Stdout,
    Stderr,
    Exit,
    PipelineStatus,
    Pty,
    SandboxUnavailable,
}

impl TaskArtifact {
    pub const ALL: [Self; 6] = [
        Self::Stdout,
        Self::Stderr,
        Self::Exit,
        Self::PipelineStatus,
        Self::Pty,
        Self::SandboxUnavailable,
    ];

    pub fn file_name(self) -> &'static str {
        match self {
            Self::Stdout => "stdout",
            Self::Stderr => "stderr",
            Self::Exit => "exit",
            Self::PipelineStatus => "pipeline-status",
            Self::Pty => "pty",
            Self::SandboxUnavailable => "sandbox-unavailable",
        }
    }

    fn flat_extension(self) -> &'static str {
        match self {
            Self::Stdout => "stdout",
            Self::Stderr => "stderr",
            Self::Exit => "exit",
            Self::PipelineStatus => "pipeline-status",
            Self::Pty => "pty",
            Self::SandboxUnavailable => "sandbox-unavailable",
        }
    }
}

#[derive(Debug)]
pub struct PinnedDir {
    file: File,
    path: PathBuf,
    #[cfg(test)]
    cache_device_override: Option<u64>,
}

#[derive(Debug)]
pub(crate) struct PinnedEntry {
    file: File,
    path: PathBuf,
    #[cfg(test)]
    cache_device_override: Option<u64>,
}
impl PinnedEntry {
    pub(crate) fn identity(&self) -> io::Result<(u64, u64)> {
        file_identity(&self.file)
    }
    pub(crate) fn directory(&self) -> io::Result<PinnedDir> {
        validate_directory_handle(&self.file)?;
        Ok(PinnedDir {
            file: self.file.try_clone()?,
            path: self.path.clone(),
            #[cfg(test)]
            cache_device_override: self.cache_device_override,
        })
    }
}

/// A directory's identity without an open handle. Historical read permissions
/// reopen the directory and compare identities instead of pinning it forever.
#[derive(Debug, Clone)]
pub(crate) struct DirectorySnapshot {
    path: PathBuf,
    identity: DirectoryIdentity,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct DirectoryIdentity {
    #[cfg(unix)]
    device: u64,
    #[cfg(unix)]
    inode: u64,
    #[cfg(windows)]
    volume: u32,
    #[cfg(windows)]
    index: (u32, u32),
}

impl DirectorySnapshot {
    pub(crate) fn reopen(&self) -> io::Result<PinnedDir> {
        let current = PinnedDir::open(&self.path)?;
        if current.identity()? != self.identity {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "registered directory path identity changed",
            ));
        }
        Ok(current)
    }
}

impl PinnedDir {
    fn identity(&self) -> io::Result<DirectoryIdentity> {
        #[cfg(unix)]
        {
            let metadata = self.file.metadata()?;
            Ok(DirectoryIdentity {
                device: metadata.dev(),
                inode: metadata.ino(),
            })
        }
        #[cfg(windows)]
        {
            let info = windows_file_information(&self.file)?;
            Ok(DirectoryIdentity {
                volume: info.volume_serial_number,
                index: (info.file_index_high, info.file_index_low),
            })
        }
    }

    pub(crate) fn snapshot(&self) -> io::Result<DirectorySnapshot> {
        Ok(DirectorySnapshot {
            path: self.path.clone(),
            identity: self.identity()?,
        })
    }

    /// Pin a directory for a temporary probe without granting its configured
    /// spelling an exception in later repair walks. Only actual configured
    /// storage/cache roots should enter the process-wide resolution registry.
    pub(crate) fn pin_unregistered_root(path: &Path) -> io::Result<Self> {
        if !path.is_absolute() {
            return Err(io::Error::other(
                "root probe requires an absolute directory",
            ));
        }
        Self::repair_walk(&canonical_root_path(path)?)
    }

    /// Open an absolute directory one component at a time without following
    /// a symlink an unprivileged process could retarget.
    ///
    /// Two kinds of symlinked ancestor are accepted. A system symlink such as
    /// macOS `/var` or `/tmp` (owned by root, in a directory only root can
    /// rename it in) is resolved during the walk. A configured storage or
    /// cache root whose own path runs through a user symlink is resolved once
    /// by [`resolve_configured_root`] when AFT opens it; a later walk under
    /// that spelling opens the resolved directory and refuses if its device
    /// and inode are no longer the ones recorded then. Every component below
    /// such a root is still opened without following symlinks.
    pub(crate) fn repair_open_path(path: &Path) -> io::Result<Self> {
        if !path.is_absolute() {
            return Err(io::Error::other("repair requires an absolute directory"));
        }
        if let Some(root) = resolved_root_for(path) {
            // The configured spelling must still name the directory recorded
            // when the root was opened: same resolved path, same identity.
            let pinned = Self::repair_walk(&root.resolved)?;
            if fs::canonicalize(&root.configured).ok().as_ref() != Some(&root.resolved)
                || pinned.directory_identity()? != root.identity
            {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    format!(
                        "configured root {} no longer resolves to the directory it was opened as",
                        root.configured.display()
                    ),
                ));
            }
            let mut pinned = pinned;
            for component in path
                .strip_prefix(&root.configured)
                .map_err(io::Error::other)?
                .components()
            {
                pinned = pinned.repair_child(component.as_os_str())?;
            }
            return Ok(pinned);
        }
        Self::repair_walk(path)
    }

    #[cfg(not(unix))]
    fn repair_walk(path: &Path) -> io::Result<Self> {
        let base = path
            .ancestors()
            .last()
            .ok_or_else(|| io::Error::other("missing absolute directory root"))?;
        let mut pinned = Self::open(base)?;
        for component in path
            .strip_prefix(base)
            .map_err(io::Error::other)?
            .components()
        {
            pinned = pinned.repair_child(component.as_os_str())?;
        }
        Ok(pinned)
    }

    /// Walk from `/`, resolving only system symlinks; any other symlink fails.
    #[cfg(unix)]
    fn repair_walk(path: &Path) -> io::Result<Self> {
        use std::collections::VecDeque;
        // Bounded like the kernel's own symlink resolution (MAXSYMLINKS).
        const MAX_SYSTEM_SYMLINKS: usize = 32;
        let mut pending: VecDeque<OsString> = path
            .components()
            .filter(|component| !matches!(component, std::path::Component::RootDir))
            .map(|component| component.as_os_str().to_os_string())
            .collect();
        let mut stack = vec![Self::open(Path::new("/"))?];
        let mut followed = 0;
        while let Some(name) = pending.pop_front() {
            if name == "." || name.is_empty() {
                continue;
            }
            if name == ".." {
                if stack.len() > 1 {
                    stack.pop();
                }
                continue;
            }
            let current = stack.last().expect("walk keeps the root");
            match current.repair_child(&name) {
                Ok(child) => stack.push(child),
                Err(error) if repair_leaf_error(&error) => {
                    let Some(target) = current.system_symlink_target(&name)? else {
                        return Err(error);
                    };
                    followed += 1;
                    if followed > MAX_SYSTEM_SYMLINKS {
                        return Err(io::Error::from_raw_os_error(libc::ELOOP));
                    }
                    let target = Path::new(&target);
                    if target.is_absolute() {
                        stack.truncate(1);
                    }
                    for component in target
                        .components()
                        .filter(|component| !matches!(component, std::path::Component::RootDir))
                        .rev()
                    {
                        pending.push_front(component.as_os_str().to_os_string());
                    }
                }
                Err(error) => return Err(error),
            }
        }
        Ok(stack.pop().expect("walk keeps the root"))
    }

    /// The target of `name` when it is a symlink no unprivileged process can
    /// replace or retarget: the link is owned by root and sits in a directory
    /// owned by root that others cannot write, or that is sticky.
    #[cfg(unix)]
    fn system_symlink_target(&self, name: &OsStr) -> io::Result<Option<OsString>> {
        let c_name = os_cstring(name)?;
        let mut link = std::mem::MaybeUninit::<libc::stat>::uninit();
        if unsafe {
            libc::fstatat(
                self.file.as_raw_fd(),
                c_name.as_ptr(),
                link.as_mut_ptr(),
                libc::AT_SYMLINK_NOFOLLOW,
            )
        } != 0
        {
            return Err(io::Error::last_os_error());
        }
        let link = unsafe { link.assume_init() };
        if link.st_mode & libc::S_IFMT != libc::S_IFLNK {
            return Ok(None);
        }
        let parent = self.file.metadata()?;
        let parent_locked = parent.uid() == 0
            && (parent.mode() & 0o022 == 0 || parent.mode() & libc::S_ISVTX as u32 != 0);
        #[cfg(test)]
        // The walk resolves earlier system links, so the same fixture link may
        // be registered under /var while its pinned parent is under /private/var.
        // Match the link itself, without following its final component.
        let trusted = TRUSTED_TEST_SYMLINKS.with(|links| {
            links.borrow().iter().any(|path| {
                fs::symlink_metadata(path).is_ok_and(|registered| {
                    registered.file_type().is_symlink()
                        && registered.dev() == link.st_dev as u64
                        && registered.ino() == link.st_ino as u64
                })
            })
        }) || (link.st_uid == 0 && parent_locked);
        #[cfg(not(test))]
        let trusted = link.st_uid == 0 && parent_locked;
        if !trusted {
            return Ok(None);
        }
        let mut buffer = vec![0_u8; libc::PATH_MAX as usize];
        let length = unsafe {
            libc::readlinkat(
                self.file.as_raw_fd(),
                c_name.as_ptr(),
                buffer.as_mut_ptr().cast(),
                buffer.len(),
            )
        };
        if length < 0 {
            return Err(io::Error::last_os_error());
        }
        buffer.truncate(length as usize);
        Ok(Some(OsString::from_vec(buffer)))
    }
    pub fn open(path: &Path) -> io::Result<Self> {
        #[cfg(test)]
        work_counts::record_open();
        #[cfg(unix)]
        let file = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(path)?;
        #[cfg(windows)]
        let file = OpenOptions::new()
            .read(true)
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_BACKUP_SEMANTICS)
            .open(path)?;
        validate_directory_handle(&file)?;
        Ok(Self {
            file,
            path: path.to_path_buf(),
            #[cfg(test)]
            cache_device_override: crate::walk_boundary::simulated_volumes::device_of(path),
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub(crate) fn directory_identity(&self) -> io::Result<(u64, u64)> {
        #[cfg(unix)]
        {
            let metadata = self.file.metadata()?;
            Ok((metadata.dev(), metadata.ino()))
        }
        #[cfg(windows)]
        {
            let info = windows_file_information(&self.file)?;
            Ok((
                u64::from(info.volume_serial_number),
                (u64::from(info.file_index_high) << 32) | u64::from(info.file_index_low),
            ))
        }
    }
    pub(crate) fn cache_device(&self) -> io::Result<u64> {
        #[cfg(test)]
        if let Some(device) = self.cache_device_override {
            return Ok(device);
        }
        self.directory_identity().map(|identity| identity.0)
    }

    pub(crate) fn repair_entry_for_rename(&self, name: &OsStr) -> io::Result<PinnedEntry> {
        let mut components = Path::new(name).components();
        if !matches!(components.next(), Some(std::path::Component::Normal(_)))
            || components.next().is_some()
        {
            return Err(io::Error::other("rename requires one entry name"));
        }
        #[cfg(unix)]
        let file = openat_file(
            self.file.as_raw_fd(),
            name,
            libc::O_RDONLY | libc::O_NONBLOCK | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            0,
        )?;
        #[cfg(windows)]
        let file = windows_relative_repair_file(&self.file, name, RepairOpen::RenameEntry)?;
        if file.metadata()?.is_dir() {
            validate_directory_handle(&file)?;
        } else {
            validate_regular_handle(&file)?;
        }
        Ok(PinnedEntry {
            file,
            path: self.path.join(name),
            #[cfg(test)]
            cache_device_override: crate::walk_boundary::simulated_volumes::device_of(
                &self.path.join(name),
            )
            .or(self.cache_device_override),
        })
    }

    pub(crate) fn repair_sync(&self) -> io::Result<()> {
        #[cfg(unix)]
        {
            self.file.sync_all()?;
        }
        Ok(())
    }

    pub(crate) fn repair_write_durable(&self, name: &OsStr, contents: &[u8]) -> io::Result<()> {
        let mut file = self.repair_create_file(name)?;
        file.write_all(contents)?;
        file.sync_all()?;
        self.repair_sync()
    }

    pub(crate) fn repair_descendant(&self, relative: &Path, create: bool) -> io::Result<Self> {
        let mut directory = self.repair_clone()?;
        let mut budget = RepairBudget::new();
        for (depth, component) in relative.components().enumerate() {
            budget.enter(depth)?;
            budget.entry()?;
            let std::path::Component::Normal(name) = component else {
                return Err(io::Error::other("invalid cache directory component"));
            };
            let child = match directory.repair_child(name) {
                Ok(child) => child,
                Err(error) if create && error.kind() == io::ErrorKind::NotFound => {
                    match directory.repair_create_dir(name) {
                        Ok(child) => child,
                        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                            directory.repair_child(name)?
                        }
                        Err(error) => return Err(error),
                    }
                }
                Err(error) => return Err(error),
            };
            child.repair_check_boundary(self, &[])?;
            directory = child;
        }
        Ok(directory)
    }

    pub(crate) fn repair_validate_descendants(&self, relative: &Path) -> io::Result<()> {
        let mut directory = self.repair_clone()?;
        let mut components = relative.components().peekable();
        let mut budget = RepairBudget::new();
        let mut depth = 0;
        while let Some(component) = components.next() {
            budget.enter(depth)?;
            budget.entry()?;
            depth += 1;
            let std::path::Component::Normal(name) = component else {
                return Err(io::Error::other("invalid cache path component"));
            };
            match directory.repair_child(name) {
                Ok(child) => {
                    child.repair_check_boundary(self, &[])?;
                    directory = child;
                }
                Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
                Err(error)
                    if error.kind() == io::ErrorKind::NotADirectory
                        && components.peek().is_none() =>
                {
                    let file = directory.repair_read_file(name)?;
                    if file_mount_identity(&file)? != file_mount_identity(&self.file)? {
                        return Err(io::Error::other("cache file crosses a mount"));
                    }
                }
                Err(error) => return Err(error),
            }
        }
        Ok(())
    }

    pub(crate) fn repair_create_file(&self, name: &OsStr) -> io::Result<File> {
        let file = self.repair_coordination_file(name)?;
        if file_mount_identity(&file)? != file_mount_identity(&self.file)? {
            return Err(io::Error::other("cache file crosses a mount"));
        }
        file.set_len(0)?;
        Ok(file)
    }

    pub(crate) fn repair_replace_file(&self, name: &OsStr, contents: &[u8]) -> io::Result<()> {
        let temporary = random_temp_name()?;
        let result = (|| {
            let mut file = self.repair_new_file(&temporary)?;
            file.write_all(contents)?;
            #[cfg(unix)]
            self.rename_to(&temporary, self, name)?;
            #[cfg(windows)]
            windows_repair_rename(&self.file, &temporary, &self.file, name, true)?;
            Ok(())
        })();
        if result.is_err() {
            let _ = self.repair_remove_file(&temporary);
        }
        result
    }
    fn repair_mount_identity(&self) -> io::Result<(u64, Vec<u8>)> {
        #[cfg(test)]
        if let Some(mount) = REPAIR_MOUNTS.with(|ids| ids.borrow().get(self.path()).copied()) {
            return Ok((mount, Vec::new()));
        }
        file_mount_identity(&self.file)
    }

    pub(crate) fn repair_clone(&self) -> io::Result<Self> {
        Ok(Self {
            file: self.file.try_clone()?,
            path: self.path.clone(),
            #[cfg(test)]
            cache_device_override: self.cache_device_override,
        })
    }

    pub(crate) fn repair_child(&self, name: &OsStr) -> io::Result<Self> {
        let mut components = Path::new(name).components();
        if !matches!(components.next(), Some(std::path::Component::Normal(_)))
            || components.next().is_some()
        {
            return Err(io::Error::other("repair requires one directory entry name"));
        }
        #[cfg(all(unix, test))]
        if REPAIR_SYMLINK_ELOOP.with(|enabled| enabled.get()) {
            let name = os_cstring(name)?;
            let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
            if unsafe {
                libc::fstatat(
                    self.file.as_raw_fd(),
                    name.as_ptr(),
                    stat.as_mut_ptr(),
                    libc::AT_SYMLINK_NOFOLLOW,
                )
            } == 0
                && unsafe { stat.assume_init() }.st_mode & libc::S_IFMT == libc::S_IFLNK
            {
                return Err(io::Error::from_raw_os_error(libc::ELOOP));
            }
        }
        #[cfg(unix)]
        {
            self.open_dir_at(name)
        }
        #[cfg(windows)]
        {
            let file = windows_relative_repair_file(&self.file, name, RepairOpen::Directory)?;
            validate_directory_handle(&file)?;
            Ok(Self {
                file,
                path: self.path.join(name),
                #[cfg(test)]
                cache_device_override: crate::walk_boundary::simulated_volumes::device_of(
                    &self.path.join(name),
                )
                .or(self.cache_device_override),
            })
        }
    }
    pub(crate) fn repair_child_same_mount(&self, name: &OsStr) -> io::Result<Self> {
        let child = self.repair_child(name)?;
        if child.repair_mount_identity()? != self.repair_mount_identity()? {
            return Err(io::Error::other("repair refuses a nested mount"));
        }
        Ok(child)
    }

    pub(crate) fn repair_check_boundary(
        &self,
        root: &Self,
        protected: &[(u64, u64)],
    ) -> io::Result<()> {
        RepairPolicy {
            device: root.directory_identity()?.0,
            mount: root.repair_mount_identity()?,
            protected,
        }
        .check(self)
    }

    pub(crate) fn repair_coordination_file(&self, name: &OsStr) -> io::Result<File> {
        #[cfg(unix)]
        {
            let file = openat_file(
                self.file.as_raw_fd(),
                name,
                libc::O_RDWR
                    | libc::O_CREAT
                    | libc::O_NOFOLLOW
                    | libc::O_CLOEXEC
                    | libc::O_NONBLOCK,
                crate::private_storage::FILE_MODE as libc::mode_t,
            )?;
            validate_regular_handle(&file)?;
            clear_nonblocking(&file)?;
            Ok(file)
        }
        #[cfg(windows)]
        {
            let file = windows_relative_repair_file(&self.file, name, RepairOpen::Coordination)?;
            validate_regular_handle(&file)?;
            Ok(file)
        }
    }

    pub(crate) fn repair_create_dir(&self, name: &OsStr) -> io::Result<Self> {
        #[cfg(unix)]
        {
            self.create_dir_at(name)
        }
        #[cfg(windows)]
        {
            let file = windows_relative_repair_file(&self.file, name, RepairOpen::NewDirectory)?;
            validate_directory_handle(&file)?;
            Ok(Self {
                file,
                path: self.path.join(name),
                #[cfg(test)]
                cache_device_override: crate::walk_boundary::simulated_volumes::device_of(
                    &self.path.join(name),
                )
                .or(self.cache_device_override),
            })
        }
    }

    pub(crate) fn repair_new_file(&self, name: &OsStr) -> io::Result<File> {
        #[cfg(unix)]
        {
            self.open_new_file(name)
        }
        #[cfg(windows)]
        {
            let file = windows_relative_repair_file(&self.file, name, RepairOpen::NewFile)?;
            validate_regular_handle(&file)?;
            Ok(file)
        }
    }

    pub(crate) fn repair_read_file(&self, name: &OsStr) -> io::Result<File> {
        #[cfg(unix)]
        {
            self.open_file(name, false)
        }
        #[cfg(windows)]
        {
            let file = windows_relative_repair_file(&self.file, name, RepairOpen::ReadFile)?;
            validate_regular_handle(&file)?;
            Ok(file)
        }
    }

    pub(crate) fn repair_remove_file(&self, name: &OsStr) -> io::Result<()> {
        #[cfg(unix)]
        {
            self.remove_file(name)
        }
        #[cfg(windows)]
        {
            let file = windows_relative_repair_file(&self.file, name, RepairOpen::DeleteFile)?;
            windows_delete_pinned_handle(&file, false)
        }
    }

    pub(crate) fn repair_private_dir(&self) -> io::Result<()> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            self.file
                .set_permissions(fs::Permissions::from_mode(crate::private_storage::DIR_MODE))?;
        }
        Ok(())
    }

    /// Remove only the opened child, never resolve a replaced root pathname.
    /// Progress survives an error so repair can report partially removed data.
    pub(crate) fn repair_remove_child(
        &self,
        name: &OsStr,
        expected: (u64, u64),
        protected: &[(u64, u64)],
        removed: &mut usize,
    ) -> io::Result<()> {
        let mut warnings = Vec::new();
        let result = self.repair_remove_child_with_warnings(
            name,
            expected,
            protected,
            removed,
            &mut warnings,
        );
        for warning in warnings {
            crate::slog_warn!("{warning}");
        }
        result
    }

    pub(crate) fn repair_remove_child_with_warnings(
        &self,
        name: &OsStr,
        expected: (u64, u64),
        protected: &[(u64, u64)],
        removed: &mut usize,
        warnings: &mut Vec<String>,
    ) -> io::Result<()> {
        let child = self.repair_child(name)?;
        if child.directory_identity()? != expected {
            return Err(io::Error::other("repair child identity changed"));
        }
        let policy = RepairPolicy {
            device: self.directory_identity()?.0,
            mount: self.repair_mount_identity()?,
            protected,
        };
        policy.check(&child)?;
        repair_remove_contents(
            &child,
            &policy,
            removed,
            &mut RepairBudget::new(),
            0,
            warnings,
        )?;
        if let Some(reason) = self.repair_unlink_directory(name, &child)? {
            warnings.push(format!(
                "removed_with_warning: {}: {reason}",
                child.path().display()
            ));
        }
        *removed += 1;
        Ok(())
    }

    fn repair_unlink_directory(
        &self,
        name: &OsStr,
        child: &Self,
    ) -> io::Result<Option<&'static str>> {
        #[cfg(unix)]
        {
            if !child.same_identity(&self.open_dir_at(name)?)? {
                return Err(io::Error::other(
                    "repair directory identity changed before unlink",
                ));
            }
            let name = os_cstring(name)?;
            // The parent fd is pinned and the name buffer lives through unlinkat.
            // AT_REMOVEDIR removes the directory entry itself, not a symlink target.
            if unsafe { libc::unlinkat(self.file.as_raw_fd(), name.as_ptr(), libc::AT_REMOVEDIR) }
                != 0
            {
                return Err(io::Error::last_os_error());
            }
            Ok(None)
        }
        #[cfg(windows)]
        {
            let _ = name;
            match windows_delete_pinned_handle_outcome(&child.file, true)? {
                DeleteDispositionOutcome::Removed => Ok(None),
                DeleteDispositionOutcome::RemovedWithWarning(reason) => Ok(Some(reason)),
            }
        }
    }

    pub(crate) fn repair_names(&self, budget: &mut RepairBudget) -> io::Result<Vec<OsString>> {
        #[cfg(unix)]
        {
            self.unix_names(|| budget.entry())
        }
        #[cfg(windows)]
        {
            windows_repair_names(&self.file, budget)
        }
    }

    pub(crate) fn repair_directory_identities(
        &self,
        output: &mut Vec<(u64, u64)>,
        budget: &mut RepairBudget,
    ) -> io::Result<()> {
        self.repair_identities(output, budget, &mut std::collections::HashSet::new(), 0)
    }

    fn repair_identities(
        &self,
        output: &mut Vec<(u64, u64)>,
        budget: &mut RepairBudget,
        seen: &mut std::collections::HashSet<(u64, u64)>,
        depth: usize,
    ) -> io::Result<()> {
        budget.enter(depth)?;
        let id = self.directory_identity()?;
        if !seen.insert(id) {
            return Ok(());
        }
        output.push(id);
        let mount = self.repair_mount_identity()?;
        let names = self.repair_names(budget)?;
        #[cfg(test)]
        REPAIR_CENSUS_AFTER_NAMES.with(|hook| {
            if let Some(hook) = hook.borrow_mut().take() {
                hook(self.path());
            }
        });
        for name in names {
            match self.repair_child(&name) {
                Ok(child) => {
                    let child_id = child.directory_identity()?;
                    if child_id.0 == id.0 && child.repair_mount_identity()? == mount {
                        child.repair_identities(output, budget, seen, depth + 1)?;
                    } else {
                        // Without the mounted subtree's identities a bind alias
                        // elsewhere cannot be ruled out. Never use a partial census.
                        return Err(io::Error::other("durable identity census crosses a mount"));
                    }
                }
                Err(error) if repair_leaf_error(&error) => {}
                // Listed, then removed before it could be opened: an atomic
                // write's temporary file or an SQLite journal in live storage.
                // A deleted entry has no identity left to protect. (An entry
                // renamed into an already-listed directory is missed whether or
                // not this error is tolerated, so failing here adds no safety;
                // it only failed every census that met ordinary writes.)
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(error),
            }
        }
        Ok(())
    }

    pub(crate) fn repair_child_tree_bytes(
        &self,
        name: &OsStr,
        protected: &[(u64, u64)],
    ) -> io::Result<u64> {
        let policy = RepairPolicy {
            device: self.directory_identity()?.0,
            mount: self.repair_mount_identity()?,
            protected,
        };
        self.repair_child(name)?
            .repair_measure(&policy, &mut RepairBudget::new(), 0)
    }
    fn repair_measure(
        &self,
        policy: &RepairPolicy<'_>,
        budget: &mut RepairBudget,
        depth: usize,
    ) -> io::Result<u64> {
        budget.enter(depth)?;
        policy.check(self)?;
        let mut bytes = 0u64;
        for name in self.repair_names(budget)? {
            match self.repair_child(&name) {
                Ok(child) => {
                    bytes =
                        bytes.saturating_add(child.repair_measure(policy, budget, depth + 1)?);
                }
                Err(error) if repair_leaf_error(&error) => match self.repair_read_file(&name) {
                    Ok(file) => bytes = bytes.saturating_add(file.metadata()?.len()),
                    #[cfg(unix)]
                    Err(error) if error.raw_os_error() == Some(libc::ELOOP) => {}
                    Err(error) => return Err(error),
                },
                Err(error) => return Err(error),
            }
        }
        Ok(bytes)
    }

    fn modified(&self) -> io::Result<SystemTime> {
        #[cfg(test)]
        work_counts::record_stat();
        self.file.metadata()?.modified()
    }

    fn same_identity(&self, other: &Self) -> io::Result<bool> {
        #[cfg(unix)]
        {
            #[cfg(test)]
            {
                work_counts::record_stat();
                work_counts::record_stat();
            }
            let left = self.file.metadata()?;
            let right = other.file.metadata()?;
            Ok(left.dev() == right.dev() && left.ino() == right.ino())
        }
        #[cfg(windows)]
        {
            let left = windows_file_information(&self.file)?;
            let right = windows_file_information(&other.file)?;
            Ok(left.volume_serial_number == right.volume_serial_number
                && left.file_index_high == right.file_index_high
                && left.file_index_low == right.file_index_low)
        }
    }

    #[cfg(unix)]
    fn open_dir_at(&self, name: &OsStr) -> io::Result<Self> {
        #[cfg(test)]
        work_counts::record_open();
        let file = openat_file(
            self.file.as_raw_fd(),
            name,
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            0,
        )?;
        validate_directory_handle(&file)?;
        Ok(Self {
            file,
            path: self.path.join(name),
            #[cfg(test)]
            cache_device_override: crate::walk_boundary::simulated_volumes::device_of(
                &self.path.join(name),
            )
            .or(self.cache_device_override),
        })
    }

    #[cfg(windows)]
    fn open_dir_at(&self, name: &OsStr) -> io::Result<Self> {
        self.ensure_current_identity()?;
        let child = Self::open(&self.path.join(name))?;
        self.ensure_current_identity()?;
        Ok(child)
    }

    #[cfg(unix)]
    fn create_dir_at(&self, name: &OsStr) -> io::Result<Self> {
        let name = os_cstring(name)?;
        let result = unsafe {
            libc::mkdirat(
                self.file.as_raw_fd(),
                name.as_ptr(),
                crate::private_storage::DIR_MODE as libc::mode_t,
            )
        };
        if result != 0 {
            return Err(io::Error::last_os_error());
        }
        self.open_dir_at(OsStr::from_bytes(name.as_bytes()))
    }

    #[cfg(windows)]
    fn create_dir_at(&self, name: &OsStr) -> io::Result<Self> {
        self.ensure_current_identity()?;
        let path = self.path.join(name);
        fs::create_dir(&path)?;
        self.ensure_current_identity()?;
        Self::open(&path)
    }

    pub fn open_new_file(&self, name: &OsStr) -> io::Result<File> {
        #[cfg(unix)]
        let file = openat_file(
            self.file.as_raw_fd(),
            name,
            libc::O_RDWR | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            crate::private_storage::FILE_MODE as libc::mode_t,
        )?;
        #[cfg(windows)]
        self.ensure_current_identity()?;
        #[cfg(windows)]
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
            .open(self.path.join(name))?;
        validate_regular_handle(&file)?;
        #[cfg(windows)]
        self.ensure_current_identity()?;
        Ok(file)
    }

    pub fn open_file(&self, name: &OsStr, write: bool) -> io::Result<File> {
        #[cfg(test)]
        work_counts::record_open();
        #[cfg(unix)]
        let file = openat_file(
            self.file.as_raw_fd(),
            name,
            (if write {
                libc::O_RDWR
            } else {
                libc::O_RDONLY | libc::O_NONBLOCK
            }) | libc::O_NOFOLLOW
                | libc::O_CLOEXEC,
            0,
        )?;
        #[cfg(windows)]
        self.ensure_current_identity()?;
        #[cfg(windows)]
        let file = OpenOptions::new()
            .read(true)
            .write(write)
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
            .open(self.path.join(name))?;
        validate_regular_handle(&file)?;
        #[cfg(unix)]
        if !write {
            clear_nonblocking(&file)?;
        }
        #[cfg(windows)]
        self.ensure_current_identity()?;
        Ok(file)
    }

    #[cfg(unix)]
    fn unix_names(&self, mut admit: impl FnMut() -> io::Result<()>) -> io::Result<Vec<OsString>> {
        let dot = b".\0";
        let fresh = unsafe {
            libc::openat(
                self.file.as_raw_fd(),
                dot.as_ptr().cast(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC | libc::O_NOFOLLOW,
            )
        };
        if fresh < 0 {
            return Err(io::Error::last_os_error());
        }
        let directory = unsafe { libc::fdopendir(fresh) };
        if directory.is_null() {
            let error = io::Error::last_os_error();
            unsafe { libc::close(fresh) };
            return Err(error);
        }
        let mut names = Vec::new();
        loop {
            let errno = repair_errno_location();
            if errno.is_null() {
                unsafe { libc::closedir(directory) };
                return Err(io::Error::other(
                    "directory enumeration errno is unavailable",
                ));
            }
            unsafe { *errno = 0 };
            #[cfg(test)]
            let fail = REPAIR_READDIR_FAIL_AFTER
                .with(|limit| limit.get().is_some_and(|limit| names.len() >= limit));
            #[cfg(not(test))]
            let fail = false;
            let entry = if fail {
                unsafe { *errno = libc::EIO };
                std::ptr::null_mut()
            } else {
                unsafe { libc::readdir(directory) }
            };
            if entry.is_null() {
                let error = unsafe { *errno };
                if error != 0 {
                    unsafe { libc::closedir(directory) };
                    return Err(io::Error::from_raw_os_error(error));
                }
                break;
            }
            let bytes = unsafe {
                std::ffi::CStr::from_ptr((*entry).d_name.as_ptr())
                    .to_bytes()
                    .to_vec()
            };
            if bytes != b"." && bytes != b".." {
                if let Err(error) = admit() {
                    unsafe { libc::closedir(directory) };
                    return Err(error);
                }
                names.push(OsString::from_vec(bytes));
            }
        }
        unsafe { libc::closedir(directory) };
        Ok(names)
    }

    pub fn list_names(&self) -> io::Result<Vec<OsString>> {
        #[cfg(test)]
        work_counts::record_open();
        #[cfg(unix)]
        {
            self.unix_names(|| Ok(()))
        }
        #[cfg(windows)]
        {
            self.ensure_current_identity()?;
            let names = fs::read_dir(&self.path)?
                .map(|entry| entry.map(|entry| entry.file_name()))
                .collect::<io::Result<Vec<_>>>()?;
            self.ensure_current_identity()?;
            Ok(names)
        }
    }

    fn rename(&self, from: &OsStr, to: &OsStr) -> io::Result<()> {
        #[cfg(test)]
        work_counts::record_rename();
        self.rename_to(from, self, to)
    }

    fn rename_to(&self, from: &OsStr, target: &PinnedDir, to: &OsStr) -> io::Result<()> {
        #[cfg(unix)]
        {
            let from = os_cstring(from)?;
            let to = os_cstring(to)?;
            let result = unsafe {
                libc::renameat(
                    self.file.as_raw_fd(),
                    from.as_ptr(),
                    target.file.as_raw_fd(),
                    to.as_ptr(),
                )
            };
            if result != 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        }
        #[cfg(windows)]
        {
            self.ensure_current_identity()?;
            target.ensure_current_identity()?;
            fs::rename(self.path.join(from), target.path.join(to))?;
            self.ensure_current_identity()?;
            target.ensure_current_identity()
        }
    }

    pub(crate) fn repair_rename_entry_to(
        &self,
        from: &OsStr,
        source: &PinnedEntry,
        target: &PinnedDir,
        to: &OsStr,
    ) -> io::Result<()> {
        let expected = source.identity()?;
        if self.repair_entry_for_rename(from)?.identity()? != expected {
            return Err(io::Error::other(
                "retirement source identity changed before rename",
            ));
        }
        if file_mount_identity(&source.file)? != file_mount_identity(&self.file)? {
            return Err(io::Error::other("retirement source crosses a mount"));
        }
        #[cfg(test)]
        BEFORE_REPAIR_RENAME.with(|hook| {
            if let Some(hook) = hook.borrow_mut().take() {
                hook();
            }
        });
        #[cfg(unix)]
        {
            let from = os_cstring(from)?;
            let to = os_cstring(to)?;
            #[cfg(target_os = "linux")]
            let result = unsafe {
                libc::syscall(
                    libc::SYS_renameat2,
                    self.file.as_raw_fd(),
                    from.as_ptr(),
                    target.file.as_raw_fd(),
                    to.as_ptr(),
                    libc::RENAME_NOREPLACE,
                )
            };
            #[cfg(target_os = "macos")]
            let result = unsafe {
                libc::renameatx_np(
                    self.file.as_raw_fd(),
                    from.as_ptr(),
                    target.file.as_raw_fd(),
                    to.as_ptr(),
                    libc::RENAME_EXCL,
                )
            };
            #[cfg(not(any(target_os = "linux", target_os = "macos")))]
            return Err(io::Error::other(
                "handle-relative no-replace rename unavailable",
            ));
            #[cfg(any(target_os = "linux", target_os = "macos"))]
            if result != 0 {
                return Err(io::Error::last_os_error());
            }
        }
        #[cfg(windows)]
        windows_repair_rename_source(&source.file, &target.file, to, false)?;
        if target.repair_entry_for_rename(to)?.identity()? != expected {
            return Err(io::Error::other(
                "retirement source identity changed during rename; unverified retirement retained",
            ));
        }
        Ok(())
    }

    #[cfg(windows)]
    pub(crate) fn ensure_current_identity(&self) -> io::Result<()> {
        let current = Self::open(&self.path)?;
        if !self.same_identity(&current)? {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "pinned directory path identity changed",
            ));
        }
        Ok(())
    }

    fn remove_file(&self, name: &OsStr) -> io::Result<()> {
        #[cfg(unix)]
        {
            let name = os_cstring(name)?;
            let result = unsafe { libc::unlinkat(self.file.as_raw_fd(), name.as_ptr(), 0) };
            if result != 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        }
        #[cfg(windows)]
        {
            let file = self.open_file(name, false)?;
            windows_delete_pinned_handle(&file, false)?;
            self.ensure_current_identity()
        }
    }

    #[cfg(windows)]
    fn remove_self(&self) -> io::Result<()> {
        self.ensure_current_identity()?;
        windows_delete_pinned_handle(&self.file, true)
    }
}

fn file_identity(file: &File) -> io::Result<(u64, u64)> {
    #[cfg(unix)]
    {
        let metadata = file.metadata()?;
        Ok((metadata.dev(), metadata.ino()))
    }
    #[cfg(windows)]
    {
        let info = windows_file_information(file)?;
        Ok((
            u64::from(info.volume_serial_number),
            (u64::from(info.file_index_high) << 32) | u64::from(info.file_index_low),
        ))
    }
}

fn file_mount_identity(file: &File) -> io::Result<(u64, Vec<u8>)> {
    #[cfg(target_os = "linux")]
    {
        let mut stat = std::mem::MaybeUninit::<libc::statx>::zeroed();
        if unsafe {
            libc::statx(
                file.as_raw_fd(),
                c"".as_ptr(),
                libc::AT_EMPTY_PATH | libc::AT_SYMLINK_NOFOLLOW,
                libc::STATX_MNT_ID,
                stat.as_mut_ptr(),
            )
        } != 0
        {
            return Err(io::Error::last_os_error());
        }
        let stat = unsafe { stat.assume_init() };
        if stat.stx_mask & libc::STATX_MNT_ID == 0 {
            return Err(io::Error::other(
                "kernel cannot verify repair mount boundary",
            ));
        }
        Ok((stat.stx_mnt_id, Vec::new()))
    }
    #[cfg(target_os = "macos")]
    {
        let mut stat = std::mem::MaybeUninit::<libc::statfs>::zeroed();
        if unsafe { libc::fstatfs(file.as_raw_fd(), stat.as_mut_ptr()) } != 0 {
            return Err(io::Error::last_os_error());
        }
        let stat = unsafe { stat.assume_init() };
        let mount = unsafe { std::ffi::CStr::from_ptr(stat.f_mntonname.as_ptr()) };
        Ok((file.metadata()?.dev(), mount.to_bytes().to_vec()))
    }
    #[cfg(windows)]
    {
        Ok((
            u64::from(windows_file_information(file)?.volume_serial_number),
            Vec::new(),
        ))
    }
    #[cfg(all(unix, not(any(target_os = "linux", target_os = "macos"))))]
    {
        Ok((file.metadata()?.dev(), Vec::new()))
    }
}

#[derive(Debug)]
pub struct TaskDirs {
    pub session: Arc<PinnedDir>,
    pub task: Arc<PinnedDir>,
    pub control: Arc<PinnedDir>,
    pub io: Arc<PinnedDir>,
}

impl Clone for TaskDirs {
    fn clone(&self) -> Self {
        Self {
            session: Arc::clone(&self.session),
            task: Arc::clone(&self.task),
            control: Arc::clone(&self.control),
            io: Arc::clone(&self.io),
        }
    }
}

#[derive(Debug)]
pub struct ResolvedTask {
    pub paths: TaskPaths,
    pub dirs: TaskDirs,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum BgMode {
    #[default]
    Pipes,
    Pty,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PersistedCompletionPreview {
    pub output_preview: String,
    pub output_truncated: bool,
    pub bash_output_list_envelope: Option<crate::list_envelope::ListEnvelope>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PersistedTask {
    pub schema_version: u32,
    /// Storage namespace captured at spawn, independent of later route binds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub harness: Option<String>,
    pub task_id: String,
    pub session_id: String,
    pub command: String,
    /// Scanner-derived segment labels retained so completion rendering does not
    /// need to parse the command again.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pipeline_segments: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pipeline_status_unavailable: Option<String>,
    #[serde(default)]
    pub mode: BgMode,
    pub workdir: PathBuf,
    #[serde(default)]
    pub project_root: Option<PathBuf>,
    pub status: BgTaskStatus,
    pub started_at: u64,
    pub finished_at: Option<u64>,
    pub duration_ms: Option<u64>,
    pub timeout_ms: Option<u64>,
    /// Whether `timeout_ms` is AFT's default background limit rather than a
    /// caller's `timeout`. Only the default may be extended while a delegated
    /// worker waits on the task, and that must still hold after a restart
    /// reloads the task from this record. Absent (false) on records written
    /// before AFT recorded it; such a task is never extended.
    #[serde(default, skip_serializing_if = "is_false")]
    pub default_hard_kill: bool,
    /// Windows remote exit codes can exceed i32::MAX. Using i64 preserves all
    /// their bits and also stores signed local process exit codes unchanged.
    pub exit_code: Option<i64>,
    /// Why the exit marker file is absent; an exact known code may still be in `exit_code`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_marker_unavailable: Option<String>,
    pub child_pid: Option<u32>,
    pub pgid: Option<i32>,
    /// Process start time recorded for `child_pid`, in Unix milliseconds.
    /// Used to detect when the OS reuses that PID for a different process.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub child_start_time_ms: Option<u64>,
    /// `Some` records a completed Unix sample, including an empty group. `None`
    /// means sampling is unavailable (Windows and unsupported Unix targets).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub live_descendants: Option<Vec<LiveDescendant>>,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub live_descendants_omitted: usize,
    pub completion_delivered: bool,
    #[serde(default = "default_notify_on_completion")]
    pub notify_on_completion: bool,
    #[serde(default = "default_compressed")]
    pub compressed: bool,
    /// Whether the child environment held any completion secret, including
    /// AFT's memory-only gh-shim ticket.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completion_has_env_secrets: Option<bool>,
    /// Whether completion secrets include values other than AFT's revoked-on-
    /// restart gh-shim ticket. On older records this field is absent, so the
    /// completion preview remains withheld unless `completion_has_env_secrets`
    /// explicitly says the child environment held no secrets.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completion_has_non_ticket_env_secrets: Option<bool>,
    /// BLAKE3 digest of this task's AFT gh-shim ticket; the ticket itself is
    /// never persisted. Used only to redact that exact value from replay output.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completion_ticket_digest: Option<String>,
    /// Fully rendered, credential-masked notice text, saved before delivery so
    /// replay never needs the original environment values to reproduce it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completion_preview: Option<PersistedCompletionPreview>,
    #[serde(default)]
    pub pty_rows: Option<u16>,
    #[serde(default)]
    pub pty_cols: Option<u16>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub scanner_report: Vec<PermissionAsk>,
    #[serde(default)]
    pub sandbox_native: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sandbox_temp_dir: Option<PathBuf>,
    /// When the finished command's scratch directory (`sandbox_temp_dir`)
    /// may be deleted, in Unix milliseconds. Scratch is kept for a while after
    /// the command exits so the same session can still read files it wrote
    /// there. Stored in the record so a restarted daemon deletes it on time
    /// instead of either forgetting it or deleting it early. Absent on running
    /// tasks and on records written before AFT kept scratch after exit; those
    /// derive the time from `finished_at`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sandbox_temp_delete_at: Option<u64>,
    pub status_reason: Option<String>,
    /// Capture did not finish cleanly; independent of the command's exit status.
    #[serde(default, skip_serializing_if = "is_false")]
    pub output_incomplete: bool,
    /// Who started the task and the key they gave the call. Absent on records
    /// written before AFT recorded it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub call_key: Option<super::TaskCallKey>,
    /// The daemon's scope owner, reference and epoch at spawn. A reconnect
    /// must not relabel a running command as belonging to the new route.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<subc_protocol::scope::ScopeStamp>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) remote: Option<super::registry::remote::RemoteTask>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub execution_note: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) memory_policy: Option<super::memory_limit::MemoryPolicy>,
    /// Missing job-wide output ranges, independent of execution outcome.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub incomplete_output: Vec<(u64, u64)>,
    /// Caller-side removals, names only; never environment values.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub stripped_env_names: Vec<String>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub local_fallback_started: bool,
    /// The call that was waiting on this task was cancelled (client cancel,
    /// transport loss, restart) while the task kept running, and the session
    /// has not yet been told. Its next bash-family reply names the task once
    /// and clears this. Persisted so the notice survives an AFT restart.
    #[serde(default, skip_serializing_if = "is_false")]
    pub cancelled_call_unannounced: bool,
}

fn default_notify_on_completion() -> bool {
    true
}

fn default_compressed() -> bool {
    true
}

fn is_false(value: &bool) -> bool {
    !*value
}

fn is_zero(value: &usize) -> bool {
    *value == 0
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExitMarker {
    Code(i32),
    Killed,
}

impl PersistedTask {
    #[allow(clippy::too_many_arguments)]
    pub fn starting(
        task_id: String,
        session_id: String,
        command: String,
        workdir: PathBuf,
        project_root: Option<PathBuf>,
        timeout_ms: Option<u64>,
        notify_on_completion: bool,
        compressed: bool,
    ) -> Self {
        let call_key = Some(super::call_key_for_new_task(&task_id));
        Self {
            schema_version: SCHEMA_VERSION,
            harness: super::route_harness().map(|harness| harness.storage_segment()),
            task_id,
            session_id,
            command,
            pipeline_segments: Vec::new(),
            pipeline_status_unavailable: None,
            mode: BgMode::Pipes,
            workdir,
            project_root,
            status: BgTaskStatus::Starting,
            started_at: unix_millis(),
            finished_at: None,
            duration_ms: None,
            timeout_ms,
            default_hard_kill: false,
            exit_code: None,
            exit_marker_unavailable: None,
            child_pid: None,
            pgid: None,
            child_start_time_ms: None,
            live_descendants: None,
            live_descendants_omitted: 0,
            completion_delivered: !notify_on_completion,
            notify_on_completion,
            compressed,
            completion_has_env_secrets: None,
            completion_has_non_ticket_env_secrets: None,
            completion_ticket_digest: None,
            completion_preview: None,
            pty_rows: None,
            pty_cols: None,
            scanner_report: Vec::new(),
            sandbox_native: false,
            sandbox_temp_dir: None,
            sandbox_temp_delete_at: None,
            status_reason: None,
            output_incomplete: false,
            call_key,
            scope: super::registry::scope_for_spawn(),
            remote: None,
            execution_note: None,
            memory_policy: None,
            incomplete_output: Vec::new(),
            stripped_env_names: Vec::new(),
            local_fallback_started: false,
            cancelled_call_unannounced: false,
        }
    }

    pub fn is_terminal(&self) -> bool {
        self.status.is_terminal()
    }

    pub(crate) fn has_safe_completion_redaction_after_restart(&self) -> bool {
        if self.completion_has_non_ticket_env_secrets != Some(false) {
            return false;
        }
        match self.completion_has_env_secrets {
            Some(false) => true,
            Some(true) => self
                .completion_ticket_digest
                .as_deref()
                .is_some_and(|digest| {
                    digest.len() == 64 && digest.bytes().all(|byte| byte.is_ascii_hexdigit())
                }),
            None => false,
        }
    }

    pub fn mark_running(&mut self, child_pid: u32, pgid: i32) {
        self.status = BgTaskStatus::Running;
        self.child_pid = Some(child_pid);
        self.pgid = Some(pgid);
        self.child_start_time_ms = crate::root_cache::process_start_time_ms(child_pid);
    }

    pub fn mark_terminal(
        &mut self,
        status: BgTaskStatus,
        exit_code: Option<i64>,
        reason: Option<String>,
    ) {
        let finished_at = unix_millis();
        self.status = status;
        self.exit_code = exit_code;
        self.finished_at = Some(finished_at);
        self.duration_ms = Some(finished_at.saturating_sub(self.started_at));
        self.child_pid = None;
        self.status_reason = reason;
        self.completion_delivered = !self.notify_on_completion;
    }

    pub fn to_bash_task_row(
        &self,
        harness: &str,
        paths: &TaskPaths,
    ) -> Result<BashTaskRow, serde_json::Error> {
        let project_root = self.project_root.as_deref().unwrap_or(&self.workdir);
        let output_bytes = capture_output_bytes(&self.mode, paths);
        let stdout_path = match self.mode {
            BgMode::Pipes => Some(paths.stdout.display().to_string()),
            BgMode::Pty => Some(paths.pty.display().to_string()),
        };
        let stderr_path = match self.mode {
            BgMode::Pipes => Some(paths.stderr.display().to_string()),
            BgMode::Pty => None,
        };
        let mut metadata = self.clone();
        metadata.schema_version = SCHEMA_VERSION;
        Ok(BashTaskRow {
            harness: harness.to_string(),
            session_id: self.session_id.clone(),
            task_id: self.task_id.clone(),
            project_key: crate::path_identity::project_scope_key(project_root),
            command: self.command.clone(),
            cwd: self.workdir.display().to_string(),
            status: status_name(&self.status).to_string(),
            exit_code: self.exit_code,
            pid: self.child_pid.map(i64::from),
            pgid: self.pgid.map(i64::from),
            started_at: self.started_at as i64,
            completed_at: self.finished_at.map(|value| value as i64),
            stdout_path,
            stderr_path,
            compressed: self.compressed,
            timeout_ms: self.timeout_ms.map(|value| value as i64),
            completion_delivered: self.completion_delivered,
            output_bytes,
            metadata: serde_json::to_string(&metadata)?,
        })
    }
}

impl From<BashTaskRow> for PersistedTask {
    fn from(row: BashTaskRow) -> Self {
        if let Ok(mut task) = serde_json::from_str::<PersistedTask>(&row.metadata) {
            task.harness = Some(row.harness.clone());
            // A foreign registry acknowledges the row without rewriting its
            // metadata snapshot, so the durable column may be newer.
            task.completion_delivered |= row.completion_delivered;
            return task;
        }
        let status = match row.status.as_str() {
            "starting" => BgTaskStatus::Starting,
            "running" => BgTaskStatus::Running,
            "killing" => BgTaskStatus::Killing,
            "completed" => BgTaskStatus::Completed,
            "failed" => BgTaskStatus::Failed,
            "killed" => BgTaskStatus::Killed,
            "timed_out" => BgTaskStatus::TimedOut,
            "fate_unknown" => BgTaskStatus::FateUnknown,
            _ => BgTaskStatus::Failed,
        };
        let started_at = u64::try_from(row.started_at).unwrap_or_default();
        let finished_at = row.completed_at.and_then(|value| u64::try_from(value).ok());
        Self {
            schema_version: SCHEMA_VERSION,
            harness: Some(row.harness.clone()),
            task_id: row.task_id,
            session_id: row.session_id,
            command: row.command,
            pipeline_segments: Vec::new(),
            pipeline_status_unavailable: None,
            mode: BgMode::Pipes,
            workdir: PathBuf::from(row.cwd),
            project_root: None,
            status,
            started_at,
            finished_at,
            duration_ms: finished_at.map(|finished_at| finished_at.saturating_sub(started_at)),
            timeout_ms: row.timeout_ms.and_then(|value| u64::try_from(value).ok()),
            default_hard_kill: false,
            exit_code: row.exit_code,
            exit_marker_unavailable: None,
            child_pid: row.pid.and_then(|value| u32::try_from(value).ok()),
            pgid: row.pgid.and_then(|value| i32::try_from(value).ok()),
            child_start_time_ms: None,
            live_descendants: None,
            live_descendants_omitted: 0,
            completion_delivered: row.completion_delivered,
            notify_on_completion: !row.completion_delivered,
            compressed: row.compressed,
            completion_has_env_secrets: None,
            completion_has_non_ticket_env_secrets: None,
            completion_ticket_digest: None,
            completion_preview: None,
            pty_rows: None,
            pty_cols: None,
            scanner_report: Vec::new(),
            sandbox_native: false,
            sandbox_temp_dir: None,
            sandbox_temp_delete_at: None,
            status_reason: None,
            output_incomplete: false,
            call_key: None,
            scope: None,
            remote: None,
            execution_note: None,
            memory_policy: None,
            incomplete_output: Vec::new(),
            stripped_env_names: Vec::new(),
            local_fallback_started: false,
            cancelled_call_unannounced: false,
        }
    }
}

fn status_name(status: &BgTaskStatus) -> &'static str {
    match status {
        BgTaskStatus::Starting => "starting",
        BgTaskStatus::Running => "running",
        BgTaskStatus::Killing => "killing",
        BgTaskStatus::Completed => "completed",
        BgTaskStatus::Failed => "failed",
        BgTaskStatus::Killed => "killed",
        BgTaskStatus::TimedOut => "timed_out",
        BgTaskStatus::FateUnknown => "fate_unknown",
    }
}

fn capture_output_bytes(mode: &BgMode, paths: &TaskPaths) -> Option<i64> {
    let len = |artifact| {
        open_task_artifact(paths, artifact)
            .ok()
            .and_then(|file| file.len().ok())
    };
    match mode {
        BgMode::Pipes => match (len(TaskArtifact::Stdout), len(TaskArtifact::Stderr)) {
            (Some(stdout), Some(stderr)) => Some(stdout.saturating_add(stderr) as i64),
            (Some(bytes), None) | (None, Some(bytes)) => Some(bytes as i64),
            (None, None) => None,
        },
        BgMode::Pty => len(TaskArtifact::Pty).map(|bytes| bytes as i64),
    }
}

pub fn validate_task_id(task_id: &str) -> io::Result<()> {
    let bytes = task_id.as_bytes();
    if bytes.len() == 21
        && bytes.starts_with(b"bash-")
        && bytes[5..]
            .iter()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(byte))
    {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "background task id must match ^bash-[0-9a-f]{16}$",
        ))
    }
}

pub fn session_tasks_dir(storage_dir: &Path, session_id: &str) -> PathBuf {
    let session_hash = hash_session(session_id);
    let direct = storage_dir.join("bash-tasks").join(&session_hash);
    if direct.exists() {
        return direct;
    }
    // Configure replays through a harness root. Live pre-namespace tasks must
    // retain their original paths until their detached writers have stopped.
    if storage_dir
        .file_name()
        .is_some_and(|name| name == "opencode" || name == "pi")
    {
        if let Some(root) = storage_dir.parent() {
            let legacy = root.join("bash-tasks").join(&session_hash);
            if legacy.exists() {
                return legacy;
            }
        }
    }
    let mut harness_matches = ["opencode", "pi"]
        .into_iter()
        .map(|harness| {
            storage_dir
                .join(harness)
                .join("bash-tasks")
                .join(&session_hash)
        })
        .filter(|path| path.exists())
        .collect::<Vec<_>>();
    if harness_matches.len() == 1 {
        return harness_matches.remove(0);
    }
    direct
}

pub fn task_paths(storage_dir: &Path, session_id: &str, task_id: &str) -> io::Result<TaskPaths> {
    validate_task_id(task_id)?;
    Ok(TaskPaths::flat(
        session_tasks_dir(storage_dir, session_id),
        task_id,
    ))
}

pub fn allocate_task_layout(storage_dir: &Path, session_id: &str) -> io::Result<ResolvedTask> {
    let session_dir = session_tasks_dir(storage_dir, session_id);
    let _session_lock = lock_session_directory(&session_dir)?;
    create_private_task_store(&session_dir)?;
    let session = Arc::new(PinnedDir::open(&session_dir)?);
    for _ in 0..32 {
        let task_id = random_task_id()?;
        match create_task_layout_from_session(Arc::clone(&session), &task_id) {
            Ok(task) => return Ok(task),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "failed to allocate unique background task id after 32 attempts",
    ))
}

pub fn create_task_layout(
    storage_dir: &Path,
    session_id: &str,
    task_id: &str,
) -> io::Result<ResolvedTask> {
    validate_task_id(task_id)?;
    let session_dir = session_tasks_dir(storage_dir, session_id);
    let _session_lock = lock_session_directory(&session_dir)?;
    create_private_task_store(&session_dir)?;
    create_task_layout_from_session(Arc::new(PinnedDir::open(&session_dir)?), task_id)
}

fn lock_session_directory(session_dir: &Path) -> io::Result<File> {
    lock_session_directories(&[session_dir])?
        .pop()
        .ok_or_else(|| io::Error::other("task session lock was not acquired"))
}

pub(crate) fn lock_session_directories(session_dirs: &[&Path]) -> io::Result<Vec<File>> {
    let mut roots = Vec::with_capacity(session_dirs.len());
    for session_dir in session_dirs {
        let task_root = session_dir
            .parent()
            .ok_or_else(|| io::Error::other("task session directory has no parent"))?;
        let storage_root = task_root
            .parent()
            .ok_or_else(|| io::Error::other("task root has no storage parent"))?;
        let lock_path = storage_root.join(".bash-task-session.lock");
        for path in [storage_root, task_root, *session_dir, lock_path.as_path()] {
            crate::production_storage::refuse_write(path)?;
        }
        roots.push((storage_root.to_path_buf(), task_root.to_path_buf()));
    }
    roots.sort();
    roots.dedup();
    for (storage_root, task_root) in &roots {
        crate::private_storage::open_dir(storage_root, task_root)?;
    }
    let mut storage_roots = roots
        .into_iter()
        .map(|(storage_root, _)| storage_root)
        .collect::<Vec<_>>();
    storage_roots.sort();
    storage_roots.dedup();
    storage_roots
        .into_iter()
        .map(|storage_root| {
            let lock = crate::private_storage::options()
                .create(true)
                .read(true)
                .write(true)
                .open(storage_root.join(".bash-task-session.lock"))?;
            lock.lock()?;
            Ok(lock)
        })
        .collect()
}

/// Remove a session directory only when no task or quarantine writer owns it.
/// The OS rmdir is the emptiness check and removal, so a racing entry survives.
#[cfg(test)]
pub(crate) fn remove_empty_session_directory(session_dir: &Path) -> io::Result<bool> {
    let _session_lock = lock_session_directory(session_dir)?;
    remove_empty_session_directory_locked(session_dir)
}

pub(crate) fn remove_empty_session_directory_locked(session_dir: &Path) -> io::Result<bool> {
    match fs::remove_dir(session_dir) {
        Ok(()) => Ok(true),
        Err(error)
            if matches!(
                error.kind(),
                io::ErrorKind::NotFound | io::ErrorKind::DirectoryNotEmpty
            ) =>
        {
            Ok(false)
        }
        Err(error) => Err(error),
    }
}

fn create_private_task_store(session_dir: &Path) -> io::Result<()> {
    crate::production_storage::refuse_write(session_dir)?;
    #[cfg(test)]
    task_io_fault_for_test(false)?;
    let root = session_dir.parent().and_then(Path::parent).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "task directory has no storage root",
        )
    })?;
    crate::private_storage::open_dir(root, session_dir)
}

fn create_task_layout_from_session(
    session: Arc<PinnedDir>,
    task_id: &str,
) -> io::Result<ResolvedTask> {
    validate_task_id(task_id)?;
    #[cfg(test)]
    wait_on_session_create_gate_for_test(&session.path);
    let task = session.create_dir_at(OsStr::new(task_id))?;
    let control = task.create_dir_at(OsStr::new(CONTROL_DIR))?;
    let io_dir = task.create_dir_at(OsStr::new(IO_DIR))?;
    let paths = TaskPaths::directory(session.path.clone(), task_id);
    Ok(ResolvedTask {
        paths,
        dirs: TaskDirs {
            session,
            task: Arc::new(task),
            control: Arc::new(control),
            io: Arc::new(io_dir),
        },
    })
}

pub fn resolve_task_layout(session_dir: &Path, task_id: &str) -> io::Result<ResolvedTask> {
    let task = resolve_uninitialized_task_layout(session_dir, task_id)?;
    let metadata = read_task_at(&task)?;
    if metadata.task_id != task_id {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "background task metadata identity mismatch",
        ));
    }
    Ok(task)
}

pub fn resolve_uninitialized_task_layout(
    session_dir: &Path,
    task_id: &str,
) -> io::Result<ResolvedTask> {
    validate_task_id(task_id)?;
    let session = Arc::new(PinnedDir::open(session_dir)?);
    let directory = session.open_dir_at(OsStr::new(task_id));
    let flat_name = OsString::from(format!("{task_id}.json"));
    let flat = open_metadata_through_replacement(&session, &flat_name);
    let has_directory = match &directory {
        Ok(_) => true,
        Err(error) if error.kind() == io::ErrorKind::NotFound => false,
        Err(error) => {
            return Err(io::Error::new(
                error.kind(),
                format!("invalid task directory: {error}"),
            ))
        }
    };
    let has_flat = match &flat {
        Ok(_) => true,
        Err(error) if error.kind() == io::ErrorKind::NotFound => false,
        Err(error) => {
            return Err(io::Error::new(
                error.kind(),
                format!("invalid flat task metadata: {error}"),
            ))
        }
    };
    if has_directory && has_flat {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "duplicate flat and directory background task layouts",
        ));
    }
    if has_directory {
        let task = directory.expect("directory result checked");
        let control = match task.open_dir_at(OsStr::new(CONTROL_DIR)) {
            Ok(control) => control,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Err(task_layout_not_found())
            }
            Err(error) => return Err(error),
        };
        let io_dir = match task.open_dir_at(OsStr::new(IO_DIR)) {
            Ok(io_dir) => io_dir,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Err(task_layout_not_found())
            }
            Err(error) => return Err(error),
        };
        let paths = TaskPaths::directory(session_dir.to_path_buf(), task_id);
        return Ok(ResolvedTask {
            paths,
            dirs: TaskDirs {
                session,
                task: Arc::new(task),
                control: Arc::new(control),
                io: Arc::new(io_dir),
            },
        });
    }
    if has_flat {
        let paths = TaskPaths::flat(session_dir.to_path_buf(), task_id);
        return Ok(ResolvedTask {
            paths,
            dirs: TaskDirs {
                session: Arc::clone(&session),
                task: Arc::clone(&session),
                control: Arc::clone(&session),
                io: session,
            },
        });
    }
    Err(io::Error::new(
        io::ErrorKind::NotFound,
        TASK_LAYOUT_NOT_FOUND,
    ))
}

pub fn resolve_task(
    storage_dir: &Path,
    session_id: &str,
    task_id: &str,
) -> io::Result<ResolvedTask> {
    resolve_task_layout(&session_tasks_dir(storage_dir, session_id), task_id)
}

pub fn discover_task_ids(session_dir: &Path) -> io::Result<(Vec<String>, Vec<OsString>)> {
    let session = PinnedDir::open(session_dir)?;
    let mut ids = std::collections::BTreeSet::new();
    let mut invalid = Vec::new();
    for name in session.list_names()? {
        let Some(text) = name.to_str() else {
            invalid.push(name);
            continue;
        };
        if validate_task_id(text).is_ok() {
            ids.insert(text.to_string());
            continue;
        }
        if let Some((task_id, _suffix)) = text.split_once('.') {
            if validate_task_id(task_id).is_ok() {
                ids.insert(task_id.to_string());
            } else if task_id.starts_with("bash-") {
                invalid.push(name);
            }
        } else if text.starts_with("bash-") {
            invalid.push(name);
        }
    }
    Ok((ids.into_iter().collect(), invalid))
}

pub fn uninitialized_layout_is_recent(
    session_dir: &Path,
    task_id: &str,
    grace: std::time::Duration,
) -> io::Result<bool> {
    // A spawn creates the task directory a few syscalls before `control/` and
    // the metadata exist, and the persisted-task GC runs concurrently with
    // spawns (in this process since it left the replay thread, and always from
    // sibling processes sharing the storage root). Mid-creation, the layout
    // resolver finds nothing to resolve; the directory's own mtime still says
    // how young it is, and a young directory must be skipped, not quarantined.
    let task = match resolve_uninitialized_task_layout(session_dir, task_id) {
        Ok(task) => task,
        Err(_) => {
            // The directory's age is an answer either way: young means a spawn
            // in progress, old means an abandoned layout that may be reclaimed.
            // Only a failing metadata probe (the directory is already gone)
            // propagates as an error.
            let modified = session_dir.join(task_id).metadata()?.modified()?;
            let age = SystemTime::now()
                .duration_since(modified)
                .unwrap_or_default();
            return Ok(age < grace);
        }
    };
    // Exit files are reserved empty at spawn. A nonempty marker means the
    // wrapper finished, so missing metadata cannot be the initial-write window.
    if task
        .dirs
        .io
        .open_file(&task.paths.artifact_name(TaskArtifact::Exit), false)
        .and_then(|file| file.metadata())
        .is_ok_and(|meta| meta.len() > 0)
    {
        return Ok(false);
    }
    let modified = match task.paths.layout {
        TaskLayout::Directory => task.dirs.control.modified()?,
        TaskLayout::Flat => task
            .dirs
            .session
            .open_file(&task.paths.artifact_name(TaskArtifact::Exit), false)
            .and_then(|file| file.metadata()?.modified())
            .or_else(|_| {
                task.dirs
                    .session
                    .open_file(OsStr::new(&format!("{task_id}.json")), false)
                    .and_then(|file| file.metadata()?.modified())
            })?,
    };
    Ok(SystemTime::now()
        .duration_since(modified)
        .unwrap_or_default()
        < grace)
}

pub fn quarantine_task_layout(
    storage_dir: &Path,
    session_dir: &Path,
    task_id: &str,
    reason: &str,
) -> io::Result<()> {
    let result = (|| -> io::Result<()> {
        validate_task_id(task_id)?;
        let quarantine_session = storage_dir.join("bash-tasks-quarantine").join(
            session_dir
                .file_name()
                .ok_or_else(|| io::Error::other("task session directory has no identity"))?,
        );
        let _session_locks = lock_session_directories(&[session_dir, &quarantine_session])?;
        let session = PinnedDir::open(session_dir)?;
        let names = session.list_names()?;
        let flat_prefix = format!("{task_id}.");
        let selected = names
            .into_iter()
            .filter(|name| {
                name == OsStr::new(task_id)
                    || name
                        .to_str()
                        .is_some_and(|name| name.starts_with(&flat_prefix))
            })
            .collect::<Vec<_>>();
        refuse_quarantine_of_newer_task(session_dir, &selected)?;
        quarantine_names(storage_dir, session_dir, &session, selected, reason)
    })();
    result.map_err(|error| {
        io::Error::new(
            error.kind(),
            format!(
                "failed to quarantine background task {task_id} from {}: {error}",
                session_dir.display()
            ),
        )
    })
}

pub fn quarantine_invalid_entry(
    storage_dir: &Path,
    session_dir: &Path,
    entry: &OsStr,
) -> io::Result<()> {
    let result = (|| -> io::Result<()> {
        let quarantine_session = storage_dir.join("bash-tasks-quarantine").join(
            session_dir
                .file_name()
                .ok_or_else(|| io::Error::other("task session directory has no identity"))?,
        );
        let _session_locks = lock_session_directories(&[session_dir, &quarantine_session])?;
        let session = PinnedDir::open(session_dir)?;
        refuse_quarantine_of_newer_task(session_dir, &[entry.to_os_string()])?;
        quarantine_names(
            storage_dir,
            session_dir,
            &session,
            vec![entry.to_os_string()],
            "invalid",
        )
    })();
    result.map_err(|error| {
        io::Error::new(
            error.kind(),
            format!(
                "failed to quarantine invalid background task entry {}: {error}",
                session_dir.join(entry).display()
            ),
        )
    })
}

/// Task metadata written by a newer build is not invalid; it is unreadable by
/// this build only. Replay, relaxed lookup and GC all quarantine through the
/// two functions above, so refusing here keeps such a task in place under its
/// original path whichever of them got here. Each entry is either a task
/// directory (`<id>/control/metadata.json`) or a flat-layout file
/// (`<id>.json`).
fn refuse_quarantine_of_newer_task(session_dir: &Path, names: &[OsString]) -> io::Result<()> {
    for name in names {
        let entry = session_dir.join(name);
        let metadata_path = if entry.is_dir() {
            entry.join(CONTROL_DIR).join(METADATA_FILE)
        } else if entry.extension() == Some(OsStr::new("json")) {
            entry
        } else {
            continue;
        };
        let version = fs::read(&metadata_path)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
            .and_then(|value| value.get("schema_version")?.as_u64());
        let Some(version) = version else {
            continue;
        };
        if let Some(refusal) = crate::persisted_format::UnsupportedPersistedFormat::check(
            crate::persisted_format::PersistedStore::BashTask,
            &metadata_path,
            version,
        ) {
            return Err(crate::persisted_format::refuse(refusal).into_io_error());
        }
    }
    Ok(())
}

fn quarantine_names(
    storage_dir: &Path,
    session_dir: &Path,
    session: &PinnedDir,
    names: Vec<OsString>,
    reason: &str,
) -> io::Result<()> {
    if names.is_empty() {
        return Ok(());
    }
    let session_hash = session_dir.file_name().ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidInput, "session dir has no identity")
    })?;
    let quarantine_path = storage_dir.join("bash-tasks-quarantine").join(session_hash);
    crate::private_storage::create_dir_all(&quarantine_path)?;
    let quarantine = PinnedDir::open(&quarantine_path)?;
    for name in names {
        let mut random = [0_u8; 8];
        getrandom::fill(&mut random).map_err(io::Error::other)?;
        let target = OsString::from(format!(
            "{}.{}-{}",
            name.to_string_lossy(),
            reason,
            hex_lower(&random)
        ));
        session.rename_to(&name, &quarantine, &target)?;
    }
    Ok(())
}

fn hex_lower(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// How many times an open of a task's metadata re-tries a concurrent
/// replacement before giving up. An attempt only loses if another rename lands
/// in the microseconds between this open and its link-count check, so a handful
/// of attempts outlasts even a writer republishing metadata in a loop —
/// provided the attempts actually sample different moments, which is what the
/// yield between them is for.
const METADATA_OPEN_ATTEMPTS: u32 = 8;

/// Open a task's metadata file by name, tolerating a concurrent atomic replace.
///
/// Metadata is republished by writing a temporary file and renaming it over the
/// old name (see `randomized_atomic_replace`), so a reader that opened the
/// previous file a moment earlier finds it at zero links and is told the
/// artifact was concurrently replaced (see the link-count semantics on
/// `validate_regular_handle`). The name already points at the replacement by
/// then, so re-opening it succeeds; the race is a single rename, not a
/// sustained condition.
///
/// This matters because callers use the layout resolver to decide whether a
/// task on disk is intact. Reporting a routine metadata write as a resolution
/// failure makes a healthy task look like a damaged layout, and callers that
/// quarantine damaged layouts — most destructively the persisted-task GC —
/// would then rename a live task's whole bundle away just because its metadata
/// was being written at that moment.
fn open_metadata_through_replacement(dir: &PinnedDir, name: &OsStr) -> io::Result<File> {
    retry_artifact_read(|| dir.open_file(name, false))
}

/// Retry a complete read attempt so validation never reuses an unlinked handle.
/// Other errors, including hard-link and symlink refusals, remain failures.
pub(crate) fn retry_artifact_read<T>(mut read: impl FnMut() -> io::Result<T>) -> io::Result<T> {
    let mut attempts = 0;
    loop {
        attempts += 1;
        match read() {
            Err(error)
                if attempts < METADATA_OPEN_ATTEMPTS
                    && error.kind() == io::ErrorKind::Interrupted
                    && error.to_string().contains(ARTIFACT_CONCURRENTLY_REPLACED) =>
            {
                // Retrying in a tight loop samples one instant several times:
                // every attempt can land inside the same scheduling quantum, so
                // eight of them are worth about one whenever the writer is
                // descheduled mid-rename -- exactly the case on a loaded
                // machine, which is the case this retry exists for. Yield first,
                // then sleep in growing steps, so the attempts span the rename
                // rather than racing it. The total stays under ~2ms, and losing
                // here is not cosmetic: a caller that reads this as a damaged
                // layout can quarantine a live task's whole bundle.
                if attempts <= 2 {
                    std::thread::yield_now();
                } else {
                    std::thread::sleep(Duration::from_micros(50 << (attempts - 3).min(5)));
                }
            }
            other => return other,
        }
    }
}

pub fn read_task(path: &Path) -> io::Result<PersistedTask> {
    retry_artifact_read(|| read_task_once(path))
}

fn read_task_once(path: &Path) -> io::Result<PersistedTask> {
    #[cfg(all(test, unix))]
    if let Some(error) = take_injected_read_task_replacement_error_for_test() {
        return Err(error);
    }
    let mut file = open_validated_path(path, false)?;
    read_task_file(&mut file, path)
}

#[cfg(all(test, unix))]
std::thread_local! {
    static READ_TASK_REPLACEMENT_FAILURES_FOR_TEST: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
}

#[cfg(all(test, unix))]
fn inject_read_task_replacement_failures_for_test(count: u32) {
    READ_TASK_REPLACEMENT_FAILURES_FOR_TEST.with(|remaining| remaining.set(count));
}

#[cfg(all(test, unix))]
fn take_injected_read_task_replacement_error_for_test() -> Option<io::Error> {
    READ_TASK_REPLACEMENT_FAILURES_FOR_TEST.with(|remaining| {
        let count = remaining.get();
        if count == 0 {
            None
        } else {
            remaining.set(count - 1);
            Some(io::Error::new(
                io::ErrorKind::Interrupted,
                ARTIFACT_CONCURRENTLY_REPLACED,
            ))
        }
    })
}

#[cfg(all(test, unix))]
fn injected_read_task_replacement_failures_for_test() -> u32 {
    READ_TASK_REPLACEMENT_FAILURES_FOR_TEST.with(std::cell::Cell::get)
}

/// Terminal status describes the leader's outcome, not the lifetime of its
/// descendants. Require the process group to have disappeared before deleting
/// scratch space; a cached `live_descendants` sample can already be out of date.
pub(crate) fn task_temp_quiescent(metadata: &PersistedTask) -> bool {
    if !metadata.is_terminal() {
        return false;
    }
    #[cfg(unix)]
    if let Some(pgid) = metadata.pgid {
        return pgid > 0
            && unsafe { libc::kill(-pgid, 0) } == -1
            && io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH);
    }
    // A spawn that failed before releasing a child has no process group.
    metadata.child_pid.is_none() && metadata.pgid.is_none()
}

pub fn read_task_at(task: &ResolvedTask) -> io::Result<PersistedTask> {
    let name = match task.paths.layout {
        TaskLayout::Directory => OsString::from(METADATA_FILE),
        TaskLayout::Flat => OsString::from(format!("{}.json", task.paths.task_id)),
    };
    let mut file = open_metadata_through_replacement(&task.dirs.control, &name)?;
    let metadata = read_task_file(&mut file, &task.dirs.control.path().join(&name))?;
    if metadata.task_id != task.paths.task_id {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "background task metadata identity does not match its layout name",
        ));
    }
    Ok(metadata)
}

fn read_task_file(file: &mut File, path: &Path) -> io::Result<PersistedTask> {
    #[cfg(test)]
    work_counts::record_read();
    file.seek(SeekFrom::Start(0))?;
    let mut content = String::new();
    file.read_to_string(&mut content)?;
    // The version is read before the full shape, so metadata written by a
    // newer build is refused by name (and kept out of quarantine) instead of
    // failing as an unparseable task.
    #[cfg(test)]
    work_counts::record_parse();
    let version = serde_json::from_str::<serde_json::Value>(&content)
        .ok()
        .and_then(|value| value.get("schema_version")?.as_u64());
    crate::persisted_format::gate(
        crate::persisted_format::PersistedStore::BashTask,
        path,
        path,
        version,
    )
    .map_err(crate::persisted_format::UnsupportedPersistedFormat::into_io_error)?;
    #[cfg(test)]
    work_counts::record_parse();
    let task: PersistedTask = serde_json::from_str(&content)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    if !matches!(task.schema_version, 2 | 3 | 4 | 5 | 6 | SCHEMA_VERSION) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "unsupported background task schema_version {} (expected 2, 3, 4, 5, 6, or {SCHEMA_VERSION})",
                task.schema_version
            ),
        ));
    }
    validate_task_id(&task.task_id)?;
    Ok(task)
}

pub fn write_task(path: &Path, task: &PersistedTask) -> io::Result<()> {
    crate::production_storage::refuse_write(path)?;
    validate_task_id(&task.task_id)?;
    if let Some(parent) = path.parent() {
        crate::private_storage::create_dir_all(parent)?;
    }
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let dir = PinnedDir::open(parent)?;
    let name = path
        .file_name()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "metadata path has no name"))?;
    write_task_in_dir(&dir, name, task, ReplaceSync::for_task(task))
}

pub fn write_task_at(task: &ResolvedTask, metadata: &PersistedTask) -> io::Result<()> {
    write_task_at_with(task, metadata, ReplaceSync::for_task(metadata))
}

/// Writes a remote task's output-progress checkpoint: a record whose only
/// change since the last durable write is the saved output cursor (sequence
/// number and byte lengths).
///
/// The new file's contents are synced before the rename, so a power loss can
/// never expose a partly written record under the metadata name. The
/// directory is not synced after the rename, so a power loss may leave the
/// previous record in place instead. That is still a correct checkpoint: each
/// saved cursor only ever covers output already synced to disk, and recovery
/// truncates the output files to whichever cursor it finds and asks the
/// runner to replay from there. The next state-change write syncs the
/// directory and makes any earlier rename durable.
pub fn write_task_checkpoint_at(task: &ResolvedTask, metadata: &PersistedTask) -> io::Result<()> {
    write_task_at_with(task, metadata, ReplaceSync::File)
}

fn write_task_at_with(
    task: &ResolvedTask,
    metadata: &PersistedTask,
    sync: ReplaceSync,
) -> io::Result<()> {
    if metadata.task_id != task.paths.task_id {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "refusing to write metadata under a different task identity",
        ));
    }
    let name = match task.paths.layout {
        TaskLayout::Directory => OsString::from(METADATA_FILE),
        TaskLayout::Flat => OsString::from(format!("{}.json", task.paths.task_id)),
    };
    write_task_in_dir(&task.dirs.control, &name, metadata, sync)
}

/// How much of an atomic file replacement is synced to disk.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ReplaceSync {
    /// Nothing is synced. The rename still protects readers from a partly
    /// written file after a process kill, but not after a power loss.
    None,
    /// The new file's contents are synced before the rename; the directory
    /// entry is not synced after it.
    File,
    /// The new file is synced before the rename and its directory after it,
    /// so the replacement itself survives a power loss.
    FileAndDirectory,
}

impl ReplaceSync {
    /// Remote tasks and local fallbacks of remote tasks resume from their
    /// metadata after a crash, so their records are written durably.
    fn for_task(task: &PersistedTask) -> Self {
        if task.remote.is_some() || task.local_fallback_started {
            Self::FileAndDirectory
        } else {
            Self::None
        }
    }
}

fn write_task_in_dir(
    dir: &PinnedDir,
    name: &OsStr,
    task: &PersistedTask,
    sync: ReplaceSync,
) -> io::Result<()> {
    crate::production_storage::refuse_write(&dir.path().join(name))?;
    #[cfg(test)]
    TASK_IO_FAULT.with(|slot| {
        if matches!(slot.get(), Some(TaskIoFault::RecordWriteNotFound)) {
            // Fail one record write without moving live output directories.
            // Keep "not found" in the storage diagnostic to exercise typed
            // task-absence classification, not just generic write failures.
            slot.set(None);
            Err(io::Error::new(
                io::ErrorKind::NotFound,
                "injected task record storage not found",
            ))
        } else {
            Ok(())
        }
    })?;
    #[cfg(test)]
    if task.status == BgTaskStatus::Running {
        task_io_fault_for_test(true)?;
    }
    #[cfg(test)]
    if task.status == BgTaskStatus::Starting {
        starting_write_delay_for_test();
    }
    // Every terminal transition is persisted through here, so this is the one
    // place that retires the task's gh shim ticket on completion, kill,
    // timeout and unknown fate alike. It runs before the write so a failed
    // write still leaves the ended task unable to speak.
    if task.is_terminal() {
        crate::gh_shim_ticket::revoke_task(&task.task_id);
    }
    let mut upgraded = task.clone();
    upgraded.schema_version = SCHEMA_VERSION;
    let content = serde_json::to_vec_pretty(&upgraded).map_err(io::Error::other)?;
    atomic_replace(dir, name, &content, sync)
}

#[cfg(test)]
#[derive(Clone, Copy)]
pub(crate) enum TaskIoFault {
    LayoutEnospc,
    RecordWriteNotFound,
    OutputHandlesEnospc,
    OutputHandlesMisleadingText,
    RunningEnospc,
    #[cfg(unix)]
    RunningDelay(std::time::Duration),
    RunningHook(fn() -> io::Result<()>),
    #[cfg(unix)]
    StartingHook(fn()),
    /// Delays the starting-record write, which happens before the spawn
    /// receipt commits, so a test can push that commit past the reply deadline.
    /// Only the Unix startup tests construct it.
    #[cfg_attr(not(unix), allow(dead_code))]
    StartingDelay(std::time::Duration),
}

#[cfg(test)]
thread_local! {
    static TASK_IO_FAULT: std::cell::Cell<Option<TaskIoFault>> = const { std::cell::Cell::new(None) };
}

#[cfg(test)]
pub(crate) fn with_task_io_fault<T>(fault: TaskIoFault, run: impl FnOnce() -> T) -> T {
    struct Restore(Option<TaskIoFault>);
    impl Drop for Restore {
        fn drop(&mut self) {
            TASK_IO_FAULT.with(|slot| slot.set(self.0));
        }
    }
    let previous = TASK_IO_FAULT.with(|slot| slot.replace(Some(fault)));
    let _restore = Restore(previous);
    run()
}

#[cfg(test)]
fn task_io_fault_for_test(running: bool) -> io::Result<()> {
    TASK_IO_FAULT.with(|slot| match slot.get() {
        Some(TaskIoFault::LayoutEnospc) if !running => Err(task_storage_full_for_test()),
        Some(TaskIoFault::RunningEnospc) if running => Err(task_storage_full_for_test()),
        #[cfg(unix)]
        Some(TaskIoFault::RunningDelay(delay)) if running => {
            std::thread::sleep(delay);
            Ok(())
        }
        Some(TaskIoFault::RunningHook(hook)) if running => hook(),
        _ => Ok(()),
    })
}

#[cfg(test)]
fn starting_write_delay_for_test() {
    #[cfg(unix)]
    if let Some(TaskIoFault::StartingHook(hook)) = TASK_IO_FAULT.with(std::cell::Cell::get) {
        hook();
    }
    if let Some(TaskIoFault::StartingDelay(delay)) = TASK_IO_FAULT.with(std::cell::Cell::get) {
        std::thread::sleep(delay);
    }
}

#[cfg(test)]
fn task_storage_full_for_test() -> io::Error {
    // OS code 28 is ENOSPC on Unix but a printer error on Windows. Inject the
    // portable error kind and a stable message so the test exercises disk-full
    // handling on every host without depending on localized system text.
    io::Error::new(io::ErrorKind::StorageFull, "No space left on device")
}

pub fn update_task_at<F>(task: &ResolvedTask, update: F) -> io::Result<PersistedTask>
where
    F: FnOnce(&mut PersistedTask),
{
    let mut metadata = read_task_at(task)?;
    let original_terminal = metadata.is_terminal();
    let original = metadata.clone();
    update(&mut metadata);
    metadata.schema_version = SCHEMA_VERSION;
    if original_terminal {
        let completion_delivered = metadata.completion_delivered;
        metadata = original;
        metadata.completion_delivered = completion_delivered;
        metadata.schema_version = SCHEMA_VERSION;
    }
    write_task_at(task, &metadata)?;
    Ok(metadata)
}

pub fn delete_task_bundle(paths: &TaskPaths) -> io::Result<()> {
    validate_task_id(&paths.task_id)?;
    crate::gh_shim_ticket::revoke_task(&paths.task_id);
    delete_task_bundle_at_root(paths)
}

/// Delete a task from the direct storage root or its harness-scoped legacy root.
pub fn delete_task_bundle_with_legacy_harness_root(
    paths: &TaskPaths,
    session_id: &str,
    harness: Option<&str>,
) -> io::Result<()> {
    validate_task_id(&paths.task_id)?;
    crate::gh_shim_ticket::revoke_task(&paths.task_id);
    #[cfg(unix)]
    if delete_task_tombstone(&paths.session_dir, &paths.task_id)? {
        return Ok(());
    }
    if task_layout_entry_exists(&paths.session_dir, &paths.task_id)? {
        return delete_task_bundle_at_root(paths);
    }

    let Some(legacy_session_dir) =
        legacy_harness_session_dir(&paths.session_dir, session_id, harness)
    else {
        return Err(task_layout_not_found());
    };
    if task_layout_entry_exists(&legacy_session_dir, &paths.task_id)? {
        let resolved = resolve_task_for_bundle_delete(&legacy_session_dir, paths)?;
        if resolved.paths.layout != paths.layout {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "background task layout changed before deletion",
            ));
        }
        return delete_resolved_task(&resolved);
    }
    #[cfg(unix)]
    if delete_task_tombstone(&legacy_session_dir, &paths.task_id)? {
        return Ok(());
    }
    Err(task_layout_not_found())
}

fn delete_task_bundle_at_root(paths: &TaskPaths) -> io::Result<()> {
    #[cfg(unix)]
    if delete_task_tombstone(&paths.session_dir, &paths.task_id)? {
        return Ok(());
    }
    #[cfg(windows)]
    if paths.layout == TaskLayout::Directory {
        return delete_windows_directory_bundle(paths);
    }
    let resolved = resolve_task_for_bundle_delete(&paths.session_dir, paths)?;
    if resolved.paths.layout != paths.layout {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "background task layout changed before deletion",
        ));
    }
    delete_resolved_task(&resolved)
}

fn resolve_task_for_bundle_delete(
    session_dir: &Path,
    paths: &TaskPaths,
) -> io::Result<ResolvedTask> {
    match resolve_task_layout(session_dir, &paths.task_id) {
        Ok(resolved) => Ok(resolved),
        Err(error) if is_task_layout_not_found(&error) => {
            if paths.layout == TaskLayout::Directory {
                let incomplete = TaskPaths::directory(session_dir.to_path_buf(), &paths.task_id);
                remove_empty_task_directory(&incomplete)?;
            }
            Err(error)
        }
        Err(error) => Err(error),
    }
}

fn remove_empty_task_directory(paths: &TaskPaths) -> io::Result<bool> {
    if paths.layout != TaskLayout::Directory {
        return Ok(false);
    }
    let _session_lock = lock_session_directory(&paths.session_dir)?;
    match fs::remove_dir(&paths.dir) {
        Ok(()) => Ok(true),
        Err(error)
            if matches!(
                error.kind(),
                io::ErrorKind::NotFound | io::ErrorKind::DirectoryNotEmpty
            ) =>
        {
            Ok(false)
        }
        Err(error) => Err(error),
    }
}

fn task_layout_entry_exists(session_dir: &Path, task_id: &str) -> io::Result<bool> {
    let session = match PinnedDir::open(session_dir) {
        Ok(session) => session,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error),
    };
    match session.open_dir_at(OsStr::new(task_id)) {
        Ok(_) => return Ok(true),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    let metadata = OsString::from(format!("{task_id}.json"));
    match open_metadata_through_replacement(&session, &metadata) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
}

#[cfg(unix)]
fn delete_task_tombstone(session_dir: &Path, task_id: &str) -> io::Result<bool> {
    validate_task_id(task_id)?;
    let session = match PinnedDir::open(session_dir) {
        Ok(session) => session,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error),
    };
    let Some(name) = find_task_tombstone(&session, task_id)? else {
        return Ok(false);
    };
    let task = match session.open_dir_at(&name) {
        Ok(task) => task,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error),
    };
    validate_and_cleanup_task_tombstone_metadata(&task, task_id)?;
    remove_optional_task_directory(&task, IO_DIR, remove_tree_contents)?;
    remove_optional_task_directory(&task, CONTROL_DIR, |control| {
        let mut names = control.list_names()?;
        names.sort_by_key(|name| name == OsStr::new(METADATA_FILE));
        for name in names {
            control.remove_file(&name)?;
        }
        Ok(())
    })?;

    let current = session.open_dir_at(&name)?;
    if !current.same_identity(&task)? {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "task tombstone identity changed before deletion",
        ));
    }
    let name = os_cstring(&name)?;
    let result =
        unsafe { libc::unlinkat(session.file.as_raw_fd(), name.as_ptr(), libc::AT_REMOVEDIR) };
    if result != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(true)
}

#[cfg(unix)]
fn find_task_tombstone(session: &PinnedDir, task_id: &str) -> io::Result<Option<OsString>> {
    let prefix = task_tombstone_prefix(task_id);
    let mut matches = session
        .list_names()?
        .into_iter()
        .filter(|name| name.to_str().is_some_and(|name| name.starts_with(&prefix)));
    let Some(name) = matches.next() else {
        return Ok(None);
    };
    if matches.next().is_some() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "multiple background task deletion tombstones found",
        ));
    }
    Ok(Some(name))
}

#[cfg(unix)]
fn validate_and_cleanup_task_tombstone_metadata(task: &PinnedDir, task_id: &str) -> io::Result<()> {
    let control = match task.open_dir_at(OsStr::new(CONTROL_DIR)) {
        Ok(control) => control,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    let mut file = match control.open_file(OsStr::new(METADATA_FILE), false) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    let metadata_path = control.path.join(METADATA_FILE);
    let metadata = read_task_file(&mut file, &metadata_path)?;
    if metadata.task_id != task_id {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "background task tombstone metadata identity mismatch",
        ));
    }
    if let Some(temp_dir) = &metadata.sandbox_temp_dir {
        if !task_temp_quiescent(&metadata) {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "task scratch still in use",
            ));
        }
        crate::sandbox_spawn::cleanup_task_temp_dir(temp_dir)?;
    }
    Ok(())
}

#[cfg(unix)]
fn remove_optional_task_directory(
    task: &PinnedDir,
    name: &str,
    remove_contents: impl FnOnce(&PinnedDir) -> io::Result<()>,
) -> io::Result<()> {
    match task.open_dir_at(OsStr::new(name)) {
        Ok(directory) => {
            remove_contents(&directory)?;
            remove_dir_entry(task, name)
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

fn legacy_harness_session_dir(
    session_dir: &Path,
    session_id: &str,
    harness: Option<&str>,
) -> Option<PathBuf> {
    let harness = harness?
        .parse::<crate::harness::Harness>()
        .ok()?
        .storage_segment();
    let session_hash = hash_session(session_id);
    if session_dir.file_name() != Some(OsStr::new(&session_hash)) {
        return None;
    }
    let task_root = session_dir.parent()?;
    if task_root.file_name() != Some(OsStr::new("bash-tasks")) {
        return None;
    }
    let storage_root = task_root.parent()?;
    Some(
        storage_root
            .join(harness)
            .join("bash-tasks")
            .join(session_hash),
    )
}

fn task_layout_not_found() -> io::Error {
    io::Error::new(io::ErrorKind::NotFound, TASK_LAYOUT_NOT_FOUND)
}

pub(crate) fn is_task_layout_not_found(error: &io::Error) -> bool {
    error.kind() == io::ErrorKind::NotFound && error.to_string() == TASK_LAYOUT_NOT_FOUND
}

#[cfg(windows)]
fn delete_windows_directory_bundle(paths: &TaskPaths) -> io::Result<()> {
    // A legacy disposition may finish removing IO/control directories only
    // after the previous sweep's pins close. Resume that partial layout rather
    // than requiring directories or metadata that cleanup already removed.
    let session = match PinnedDir::open(&paths.session_dir) {
        Ok(session) => session,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    match open_metadata_through_replacement(
        &session,
        &OsString::from(format!("{}.json", paths.task_id)),
    ) {
        Ok(_) => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "background task layout changed before deletion",
            ))
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    let Some(task) = open_optional_windows_directory(&session, OsStr::new(&paths.task_id))? else {
        return Ok(());
    };
    let io_dir = open_optional_windows_directory(&task, OsStr::new(IO_DIR))?;
    let control = open_optional_windows_directory(&task, OsStr::new(CONTROL_DIR))?;
    if let Some(control) = &control {
        match control.open_file(OsStr::new(METADATA_FILE), false) {
            Ok(mut file) => {
                let metadata = read_task_file(&mut file, &paths.json)?;
                if metadata.task_id != paths.task_id {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "background task metadata identity mismatch",
                    ));
                }
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound && io_dir.is_none() => {}
            Err(error) => return Err(error),
        }
    } else if io_dir.is_some() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "background task control directory is missing before IO cleanup",
        ));
    }
    remove_windows_directory_task(
        &session,
        &task,
        io_dir.as_ref(),
        control.as_ref(),
        remove_tree_contents,
    )
}

#[cfg(windows)]
fn open_optional_windows_directory(dir: &PinnedDir, name: &OsStr) -> io::Result<Option<PinnedDir>> {
    match dir.open_dir_at(name) {
        Ok(child) => Ok(Some(child)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

pub fn delete_resolved_task(task: &ResolvedTask) -> io::Result<()> {
    validate_task_id(&task.paths.task_id)?;
    match task.paths.layout {
        TaskLayout::Flat => {
            for path in task_bundle_files(&task.paths) {
                let Some(name) = path.file_name() else {
                    continue;
                };
                match task.dirs.session.remove_file(name) {
                    Ok(()) => {}
                    Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                    Err(error) => return Err(error),
                }
            }
            Ok(())
        }
        TaskLayout::Directory => remove_directory_task(task),
    }
}

fn remove_directory_task(task: &ResolvedTask) -> io::Result<()> {
    remove_directory_task_with_io_cleanup(task, remove_tree_contents)
}

fn remove_directory_task_with_io_cleanup(
    task: &ResolvedTask,
    remove_io_contents: impl FnOnce(&PinnedDir) -> io::Result<()>,
) -> io::Result<()> {
    let current = task
        .dirs
        .session
        .open_dir_at(OsStr::new(&task.paths.task_id))?;
    if !current.same_identity(&task.dirs.task)? {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "task directory identity changed before deletion",
        ));
    }
    #[cfg(windows)]
    return remove_windows_directory_task(
        &task.dirs.session,
        &task.dirs.task,
        Some(&task.dirs.io),
        Some(&task.dirs.control),
        remove_io_contents,
    );

    #[cfg(unix)]
    {
        // The short scratch directory is outside the bundle, so retire it
        // before removing the metadata that records its ownership.
        if let Ok(metadata) = read_task_at(task) {
            if let Some(temp_dir) = &metadata.sandbox_temp_dir {
                if !task_temp_quiescent(&metadata) {
                    return Err(io::Error::new(
                        io::ErrorKind::WouldBlock,
                        "task scratch still in use",
                    ));
                }
                crate::sandbox_spawn::cleanup_task_temp_dir(temp_dir)?;
            }
        }
        let tombstone = rename_task_to_tombstone(task)?;
        // Windows may refuse to delete an output file while the killed process
        // still has it open. Remove the child-writable IO tree before control
        // metadata so a partial failure leaves the task resolvable for the next
        // cleanup pass. Keep metadata until every other control file is gone too.
        remove_io_contents(&task.dirs.io)?;
        let mut control_names = task.dirs.control.list_names()?;
        control_names.sort_by_key(|name| name == OsStr::new(METADATA_FILE));
        for name in control_names {
            task.dirs.control.remove_file(&name)?;
        }
        remove_dir_entry(&task.dirs.task, IO_DIR)?;
        remove_dir_entry(&task.dirs.task, CONTROL_DIR)?;
        let name = os_cstring(&tombstone)?;
        let result = unsafe {
            libc::unlinkat(
                task.dirs.session.file.as_raw_fd(),
                name.as_ptr(),
                libc::AT_REMOVEDIR,
            )
        };
        if result != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
}

#[cfg(windows)]
fn remove_windows_directory_task(
    session: &PinnedDir,
    task: &PinnedDir,
    io_dir: Option<&PinnedDir>,
    control: Option<&PinnedDir>,
    remove_io_contents: impl FnOnce(&PinnedDir) -> io::Result<()>,
) -> io::Result<()> {
    session.ensure_current_identity()?;
    task.ensure_current_identity()?;
    // Keep control metadata until IO names are actually unlinked. Unsupported
    // POSIX deletion must leave the task registered, not claim pending removal
    // as success. A later sweep can resume after the retained handles close.
    if let Some(io_dir) = io_dir {
        remove_io_contents(io_dir)?;
        io_dir.remove_self()?;
    }
    if let Some(control) = control {
        let mut names = control.list_names()?;
        names.sort_by_key(|name| name == OsStr::new(METADATA_FILE));
        for name in names {
            control.remove_file(&name)?;
        }
        control.remove_self()?;
    }
    // Native Windows shell/PTY wrappers also live directly in the task root.
    // Remove those remaining entries through the same pinned, no-reparse walk
    // before unlinking the directory that contains them.
    remove_tree_contents(task)?;
    task.remove_self()?;
    session.ensure_current_identity()
}

#[cfg(test)]
thread_local! { static REPAIR_FAIL_AFTER: std::cell::Cell<Option<usize>> = const { std::cell::Cell::new(None) }; }

#[cfg(test)]
pub(crate) fn with_repair_failure_after_for_test<T>(entries: usize, work: impl FnOnce() -> T) -> T {
    struct Reset(Option<usize>);
    impl Drop for Reset {
        fn drop(&mut self) {
            REPAIR_FAIL_AFTER.with(|value| value.set(self.0));
        }
    }
    let _reset = Reset(REPAIR_FAIL_AFTER.with(|value| value.replace(Some(entries))));
    work()
}

#[cfg(test)]
thread_local! { pub(crate) static REPAIR_CENSUS_AFTER_NAMES: std::cell::RefCell<Option<Box<dyn FnOnce(&Path)>>> = const { std::cell::RefCell::new(None) }; }
#[cfg(test)]
thread_local! { pub(crate) static REPAIR_IDENTITIES: std::cell::RefCell<std::collections::HashMap<PathBuf, (u64,u64)>> = std::cell::RefCell::new(std::collections::HashMap::new()); }
#[cfg(test)]
thread_local! { pub(crate) static REPAIR_MOUNTS: std::cell::RefCell<std::collections::HashMap<PathBuf, u64>> = std::cell::RefCell::new(std::collections::HashMap::new()); }
#[cfg(test)]
thread_local! { pub(crate) static BEFORE_REPAIR_RENAME: std::cell::RefCell<Option<Box<dyn FnOnce()>>> = const { std::cell::RefCell::new(None) }; }
// Symlinks a test treats as system-owned, since tests cannot create
// root-owned links.
#[cfg(all(unix, test))]
thread_local! { pub(crate) static TRUSTED_TEST_SYMLINKS: std::cell::RefCell<std::collections::HashSet<PathBuf>> = std::cell::RefCell::new(Default::default()); }

/// A configured root whose spelling runs through a user symlink, resolved
/// once when AFT opened it.
#[derive(Clone, Debug)]
pub(crate) struct ResolvedRoot {
    pub(crate) configured: PathBuf,
    pub(crate) resolved: PathBuf,
    pub(crate) identity: (u64, u64),
}

fn resolved_roots() -> &'static std::sync::Mutex<Vec<ResolvedRoot>> {
    static ROOTS: std::sync::OnceLock<std::sync::Mutex<Vec<ResolvedRoot>>> =
        std::sync::OnceLock::new();
    ROOTS.get_or_init(Default::default)
}

/// The registered root with the longest configured spelling that `path` lies under.
fn resolved_root_for(path: &Path) -> Option<ResolvedRoot> {
    resolved_roots()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .iter()
        .filter(|root| path.starts_with(&root.configured))
        .max_by_key(|root| root.configured.components().count())
        .cloned()
}

/// `fs::canonicalize` for a storage or cache root whose resolved path is kept
/// and handed on (to SQLite, to cache keys, to path comparisons). On Windows
/// `fs::canonicalize` returns a verbatim `\\?\C:\...` path, a spelling no
/// other AFT path uses and under which Win32 takes `/` literally; the plain
/// DOS or UNC spelling is returned instead whenever one exists. Unix paths
/// are returned exactly as `fs::canonicalize` gives them.
pub(crate) fn canonical_root_path(path: &Path) -> io::Result<PathBuf> {
    let resolved = fs::canonicalize(path)?;
    #[cfg(windows)]
    let resolved = crate::windows_path::normalize_windows_path(&resolved);
    Ok(resolved)
}

/// Resolve a configured storage or cache root once, when AFT opens it, and
/// record the directory it names by device and inode. Later pinned walks
/// under the configured spelling open that directory and refuse if it has
/// been replaced (see [`PinnedDir::repair_open_path`]). Opening the root again
/// re-resolves it, so a deliberate change of where a symlink points takes
/// effect at the next configure, never in the middle of an operation.
/// Returns the resolved path; a root without symlinks resolves to itself.
pub(crate) fn resolve_configured_root(configured: &Path) -> io::Result<PathBuf> {
    if !configured.is_absolute() {
        return Err(io::Error::other("configured root must be absolute"));
    }
    #[cfg(not(unix))]
    {
        canonical_root_path(configured)
    }
    #[cfg(unix)]
    {
        let resolved = fs::canonicalize(configured)?;
        let mut roots = resolved_roots()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        roots.retain(|root| root.configured != configured);
        if resolved != configured {
            let identity = PinnedDir::repair_walk(&resolved)?.directory_identity()?;
            roots.push(ResolvedRoot {
                configured: configured.to_path_buf(),
                resolved: resolved.clone(),
                identity,
            });
        }
        Ok(resolved)
    }
}
#[cfg(all(unix, test))]
thread_local! { pub(crate) static REPAIR_READDIR_FAIL_AFTER: std::cell::Cell<Option<usize>> = const { std::cell::Cell::new(None) }; }
#[cfg(all(unix, test))]
thread_local! { pub(crate) static REPAIR_SYMLINK_ELOOP: std::cell::Cell<bool> = const { std::cell::Cell::new(false) }; }

/// Fail closed instead of allocating, recursing or holding repair locks without
/// a bound. A later attempt may finish after the oversized tree is inspected.
pub(crate) struct RepairBudget {
    remaining: usize,
    deadline: std::time::Instant,
}
impl RepairBudget {
    pub(crate) fn with_entries(entries: usize) -> Self {
        let mut budget = Self::new();
        budget.remaining = entries;
        budget
    }
    pub(crate) fn new() -> Self {
        Self {
            remaining: 1_000_000,
            deadline: std::time::Instant::now() + Duration::from_secs(60),
        }
    }
    pub(crate) fn enter(&self, depth: usize) -> io::Result<()> {
        if depth > 64 || std::time::Instant::now() >= self.deadline {
            return Err(io::Error::other("repair traversal limit exceeded"));
        }
        Ok(())
    }
    pub(crate) fn entry(&mut self) -> io::Result<()> {
        self.enter(0)?;
        self.remaining = self
            .remaining
            .checked_sub(1)
            .ok_or_else(|| io::Error::other("repair traversal limit exceeded"))?;
        Ok(())
    }
}

#[cfg(any(target_os = "linux", target_os = "android"))]
fn repair_errno_location() -> *mut libc::c_int {
    unsafe { libc::__errno_location() }
}
#[cfg(target_os = "macos")]
fn repair_errno_location() -> *mut libc::c_int {
    unsafe { libc::__error() }
}
#[cfg(all(
    unix,
    not(any(target_os = "linux", target_os = "android", target_os = "macos"))
))]
fn repair_errno_location() -> *mut libc::c_int {
    std::ptr::null_mut()
}

struct RepairPolicy<'a> {
    device: u64,
    mount: (u64, Vec<u8>),
    protected: &'a [(u64, u64)],
}
pub(crate) const REPAIR_WITNESS_LEAF: &str = ".aft-retirement.json";

#[cfg(unix)]
#[test]
fn repair_census_propagates_late_readdir_failure() {
    let temp = tempfile::tempdir().unwrap();
    for name in ["first", "second", "third"] {
        fs::create_dir(temp.path().join(name)).unwrap();
    }
    let pinned = PinnedDir::open(temp.path()).unwrap();
    REPAIR_READDIR_FAIL_AFTER.with(|limit| limit.set(Some(1)));
    let result = pinned.repair_directory_identities(&mut Vec::new(), &mut RepairBudget::new());
    REPAIR_READDIR_FAIL_AFTER.with(|limit| limit.set(None));
    assert_eq!(
        result
            .expect_err("a partial readdir must not become a complete census")
            .raw_os_error(),
        Some(libc::EIO)
    );
}

/// Live storage creates and removes files all the time (atomic-write
/// temporaries, SQLite journals). An entry removed between the census listing
/// it and opening it is simply gone: it is not protected and must not fail the
/// census, which would stop the retention reaper and doctor cleanup.
#[test]
fn repair_census_skips_an_entry_removed_after_listing() {
    let temp = tempfile::tempdir().unwrap();
    let kept = temp.path().join("kept");
    let vanishing = temp.path().join("vanishing");
    fs::create_dir(&kept).unwrap();
    fs::create_dir(&vanishing).unwrap();
    fs::write(temp.path().join("journal"), b"short-lived").unwrap();
    let kept_id = PinnedDir::open(&kept)
        .unwrap()
        .directory_identity()
        .unwrap();
    let listed = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
    let observed = listed.clone();
    let removed = (vanishing.clone(), temp.path().join("journal"));
    REPAIR_CENSUS_AFTER_NAMES.with(|hook| {
        *hook.borrow_mut() = Some(Box::new(move |directory: &Path| {
            observed.borrow_mut().push(directory.to_path_buf());
            fs::remove_dir(&removed.0).unwrap();
            fs::remove_file(&removed.1).unwrap();
        }));
    });
    let pinned = PinnedDir::open(temp.path()).unwrap();
    let mut identities = Vec::new();
    let result = pinned.repair_directory_identities(&mut identities, &mut RepairBudget::new());
    REPAIR_CENSUS_AFTER_NAMES.with(|hook| hook.borrow_mut().take());
    result.expect("an entry removed after listing must not fail the census");
    assert_eq!(*listed.borrow(), vec![temp.path().to_path_buf()]);
    assert!(!vanishing.exists());
    assert!(identities.contains(&kept_id), "{identities:?}");
}

#[cfg(target_os = "macos")]
#[test]
fn macos_repair_mount_identity_includes_the_native_mount_name() {
    let temp = tempfile::tempdir().unwrap();
    let pinned = PinnedDir::open(temp.path()).unwrap();
    let mount = pinned.repair_mount_identity().unwrap();
    assert!(
        !mount.1.is_empty(),
        "same-device mount boundaries need their statfs mount name"
    );
    assert!(Path::new(OsStr::from_bytes(&mount.1)).is_absolute());
}

#[test]
fn repair_name_enumeration_refuses_entry_budget() {
    let temp = tempfile::tempdir().unwrap();
    for number in 0..8 {
        fs::write(temp.path().join(number.to_string()), "keep").unwrap();
    }
    let pinned = PinnedDir::open(temp.path()).unwrap();
    let mut budget = RepairBudget::new();
    budget.remaining = 3;
    let error = pinned
        .repair_names(&mut budget)
        .expect_err("must not collect an unbounded directory");
    assert!(error.to_string().contains("traversal limit"), "{error}");
}

#[cfg(unix)]
#[test]
fn repair_walk_follows_only_symlinks_an_unprivileged_user_cannot_retarget() {
    let temp = tempfile::tempdir().unwrap();
    let base = fs::canonicalize(temp.path()).unwrap();
    let real = base.join("real");
    fs::create_dir_all(real.join("child")).unwrap();
    let link = base.join("link");
    std::os::unix::fs::symlink("real", &link).unwrap();
    // This link is owned by the test user, so the walk refuses it.
    let error = PinnedDir::repair_open_path(&link.join("child"))
        .expect_err("a user-owned symlink ancestor was followed");
    assert!(repair_leaf_error(&error), "{error}");
    // Treated as a root-owned system link (like macOS /var), it is resolved.
    TRUSTED_TEST_SYMLINKS.with(|links| links.borrow_mut().insert(link.clone()));
    let pinned = PinnedDir::repair_open_path(&link.join("child"));
    TRUSTED_TEST_SYMLINKS.with(|links| links.borrow_mut().clear());
    assert_eq!(
        pinned.unwrap().directory_identity().unwrap(),
        PinnedDir::open(&real.join("child"))
            .unwrap()
            .directory_identity()
            .unwrap()
    );
}

#[cfg(unix)]
#[test]
fn repair_walk_recognizes_a_trusted_link_beneath_a_system_alias() {
    let temp = tempfile::tempdir().unwrap();
    let base = fs::canonicalize(temp.path()).unwrap();
    let real = base.join("real");
    let volume = base.join("volume");
    fs::create_dir(&real).unwrap();
    fs::create_dir_all(volume.join("storage")).unwrap();
    let system_alias = base.join("system-var");
    std::os::unix::fs::symlink(&real, &system_alias).unwrap();
    let nested = system_alias.join("volume-alias");
    std::os::unix::fs::symlink(&volume, &nested).unwrap();
    // After resolving system-var, the walker sees real/volume-alias rather
    // than the spelling registered here, just as /var becomes /private/var.
    TRUSTED_TEST_SYMLINKS.with(|links| {
        links.borrow_mut().extend([system_alias, nested.clone()]);
    });
    let opened = PinnedDir::repair_open_path(&nested.join("storage"));
    TRUSTED_TEST_SYMLINKS.with(|links| links.borrow_mut().clear());
    assert_eq!(
        opened.unwrap().directory_identity().unwrap(),
        PinnedDir::open(&volume.join("storage"))
            .unwrap()
            .directory_identity()
            .unwrap()
    );
}

/// A resolved root is kept and handed on (to SQLite, cache keys and path
/// comparisons), so on Windows it must come back in the plain `C:\...`
/// spelling, never the verbatim `\\?\C:\...` form `fs::canonicalize` gives.
/// The temp folder may be spelled with an 8.3 name (`RUNNER~1`); the resolved
/// path must still name the same directory.
#[test]
fn resolved_root_keeps_the_plain_spelling() {
    let temp = tempfile::tempdir().unwrap();
    let resolved = canonical_root_path(temp.path()).unwrap();
    assert!(
        !resolved.to_string_lossy().starts_with(r"\\?\"),
        "resolved root kept a verbatim prefix: {}",
        resolved.display()
    );
    assert!(resolved.is_absolute());
    assert_eq!(
        fs::canonicalize(&resolved).unwrap(),
        fs::canonicalize(temp.path()).unwrap()
    );
    #[cfg(not(unix))]
    assert_eq!(resolve_configured_root(temp.path()).unwrap(), resolved);
}

/// Windows will not rename a directory while a file inside it is open: the
/// rename fails with ERROR_ACCESS_DENIED (os error 5) even though every handle
/// shares delete access. Publishing a staged cache folder must therefore close
/// the lock file it holds inside that folder before renaming it.
#[cfg(windows)]
#[test]
fn windows_directory_rename_waits_for_files_inside_to_close() {
    let temp = tempfile::tempdir().unwrap();
    let parent = PinnedDir::open(temp.path()).unwrap();
    let staging = OsStr::new(".staging");
    let published = OsStr::new("published");
    let folder = parent.repair_create_dir(staging).unwrap();
    let lock = folder
        .repair_coordination_file(OsStr::new("in-use.lock"))
        .unwrap();
    lock.try_lock_shared().unwrap();
    let source = parent.repair_entry_for_rename(staging).unwrap();
    let refused = parent
        .repair_rename_entry_to(staging, &source, &parent, published)
        .expect_err("rename with an open file inside");
    assert_eq!(refused.raw_os_error(), Some(5), "{refused}");
    drop(lock);
    parent
        .repair_rename_entry_to(staging, &source, &parent, published)
        .unwrap();
    assert!(temp.path().join("published").join("in-use.lock").is_file());
}

#[cfg(unix)]
#[test]
fn configured_root_is_resolved_once_and_refused_after_it_is_retargeted() {
    let temp = tempfile::tempdir().unwrap();
    let base = fs::canonicalize(temp.path()).unwrap();
    let (first, second) = (base.join("first"), base.join("second"));
    fs::create_dir_all(first.join("cache")).unwrap();
    fs::create_dir_all(second.join("cache")).unwrap();
    let configured = base.join("configured");
    std::os::unix::fs::symlink(&first, &configured).unwrap();
    assert_eq!(resolve_configured_root(&configured).unwrap(), first);
    let identity = |path: &Path| PinnedDir::open(path).unwrap().directory_identity().unwrap();
    assert_eq!(
        PinnedDir::repair_open_path(&configured.join("cache"))
            .unwrap()
            .directory_identity()
            .unwrap(),
        identity(&first.join("cache"))
    );
    fs::remove_file(&configured).unwrap();
    std::os::unix::fs::symlink(&second, &configured).unwrap();
    let error = PinnedDir::repair_open_path(&configured.join("cache"))
        .expect_err("a retargeted configured root was followed");
    assert!(error.to_string().contains("no longer resolves"), "{error}");
    // Opening the root again, as the next configure does, adopts the change.
    assert_eq!(resolve_configured_root(&configured).unwrap(), second);
    assert_eq!(
        PinnedDir::repair_open_path(&configured.join("cache"))
            .unwrap()
            .directory_identity()
            .unwrap(),
        identity(&second.join("cache"))
    );
}

#[test]
fn repair_traversal_refuses_expired_deadline() {
    let mut budget = RepairBudget::new();
    budget.deadline = std::time::Instant::now();
    let error = budget
        .enter(0)
        .expect_err("an expired walk must fail closed");
    assert!(error.to_string().contains("traversal limit"), "{error}");
}

impl RepairPolicy<'_> {
    fn check(&self, dir: &PinnedDir) -> io::Result<()> {
        let actual = dir.directory_identity()?;
        #[cfg(test)]
        let actual =
            REPAIR_IDENTITIES.with(|ids| ids.borrow().get(dir.path()).copied().unwrap_or(actual));
        if actual.0 != self.device
            || self.protected.contains(&actual)
            || dir.repair_mount_identity()? != self.mount
        {
            return Err(io::Error::other(
                "repair refuses a mounted or protected directory",
            ));
        }
        Ok(())
    }
}
fn repair_leaf_error(error: &io::Error) -> bool {
    #[cfg(unix)]
    if error.raw_os_error() == Some(libc::ELOOP) {
        return true;
    }
    error.kind() == io::ErrorKind::NotADirectory
}

fn repair_remove_contents(
    dir: &PinnedDir,
    policy: &RepairPolicy<'_>,
    removed: &mut usize,
    budget: &mut RepairBudget,
    depth: usize,
    warnings: &mut Vec<String>,
) -> io::Result<()> {
    budget.enter(depth)?;
    let mut names = dir.repair_names(budget)?;
    // Keep the retirement proof until its payload is gone so a crash retry can
    // validate the identity of every surviving top-level entry.
    names.sort_by(|left, right| {
        (left.as_os_str() == OsStr::new(REPAIR_WITNESS_LEAF))
            .cmp(&(right.as_os_str() == OsStr::new(REPAIR_WITNESS_LEAF)))
            .then_with(|| left.cmp(right))
    });
    for name in names {
        match dir.repair_child(&name) {
            Ok(child) => {
                policy.check(&child)?;
                repair_remove_contents(&child, policy, removed, budget, depth + 1, warnings)?;
                if let Some(reason) = dir.repair_unlink_directory(&name, &child)? {
                    warnings.push(format!(
                        "removed_with_warning: {}: {reason}",
                        child.path().display()
                    ));
                }
            }
            Err(error) if repair_leaf_error(&error) => {
                #[cfg(unix)]
                dir.remove_file(&name)?;
                #[cfg(windows)]
                {
                    let file =
                        windows_relative_repair_file(&dir.file, &name, RepairOpen::DeleteFile)?;
                    let outcome = windows_delete_pinned_handle_outcome(&file, false)?;
                    record_delete_outcome(outcome, &dir.path().join(&name), warnings);
                }
            }
            Err(error) => return Err(error),
        }
        *removed += 1;
        #[cfg(test)]
        if REPAIR_FAIL_AFTER.with(|value| value.get().is_some_and(|limit| *removed >= limit)) {
            return Err(io::Error::other("injected repair failure after removal"));
        }
    }
    Ok(())
}

fn remove_tree_contents(dir: &PinnedDir) -> io::Result<()> {
    for name in dir.list_names()? {
        match dir.open_dir_at(&name) {
            Ok(child) => {
                remove_tree_contents(&child)?;
                #[cfg(unix)]
                {
                    let name = os_cstring(&name)?;
                    let result = unsafe {
                        libc::unlinkat(dir.file.as_raw_fd(), name.as_ptr(), libc::AT_REMOVEDIR)
                    };
                    if result != 0 {
                        return Err(io::Error::last_os_error());
                    }
                }
                #[cfg(windows)]
                child.remove_self()?;
            }
            Err(error) if error.kind() == io::ErrorKind::NotADirectory => {
                dir.remove_file(&name)?;
            }
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

#[cfg(unix)]
fn task_tombstone_prefix(task_id: &str) -> String {
    format!(".aft-task-delete-{task_id}-")
}

#[cfg(unix)]
fn rename_task_to_tombstone(task: &ResolvedTask) -> io::Result<OsString> {
    for _ in 0..32 {
        let tombstone = OsString::from(format!(
            "{}{}",
            task_tombstone_prefix(&task.paths.task_id),
            random_temp_name()?.to_string_lossy()
        ));
        match task.dirs.session.open_dir_at(&tombstone) {
            Ok(_) => continue,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        task.dirs
            .session
            .rename(OsStr::new(&task.paths.task_id), &tombstone)?;
        let moved = task.dirs.session.open_dir_at(&tombstone)?;
        if !moved.same_identity(&task.dirs.task)? {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "task directory identity changed during deletion",
            ));
        }
        return Ok(tombstone);
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "failed to allocate randomized task deletion name",
    ))
}

#[cfg(unix)]
fn remove_dir_entry(task: &PinnedDir, child: &str) -> io::Result<()> {
    let child = os_cstring(OsStr::new(child))?;
    let result =
        unsafe { libc::unlinkat(task.file.as_raw_fd(), child.as_ptr(), libc::AT_REMOVEDIR) };
    if result != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

pub fn task_bundle_files(paths: &TaskPaths) -> Vec<PathBuf> {
    if paths.layout == TaskLayout::Directory {
        return vec![paths.dir.clone()];
    }
    vec![
        paths.json.clone(),
        paths.stdout.clone(),
        paths.stderr.clone(),
        paths.exit.clone(),
        paths.pipeline_status.clone(),
        paths.pty.clone(),
        paths.sandbox_unavailable.clone(),
        paths.command.clone(),
        paths.wrapper.clone(),
        paths.environment.clone(),
        paths.manifest.clone(),
        paths.sandbox_profile.clone(),
        paths.dir.join(format!("{}.ps1", paths.task_id)),
        paths.dir.join(format!("{}.bat", paths.task_id)),
    ]
}

pub fn write_kill_marker_if_absent(paths: &TaskPaths) -> io::Result<()> {
    // A concurrent replace (child exit write racing this kill marker) shows up
    // as a zero-link validated open; the replacement file is the child's real
    // exit marker, so re-opening resolves the race in either direction. Bounded
    // retries: the race is a single rename, not a sustained condition.
    let mut attempts = 0;
    loop {
        attempts += 1;
        // The exit file is opened writable so an empty one can be filled in
        // place; a read-only handle cannot be truncated (ftruncate fails with
        // EINVAL on macOS and Linux). Writing in place keeps the file the
        // child's inherited exit handle points at. If the path is replaced
        // after this open, the handle refers to the unlinked original, whose
        // zero link count makes validation report a concurrent replace, and
        // the retry below re-opens the replacement.
        let result = match open_task_artifact_for_write(paths, TaskArtifact::Exit) {
            Ok(file) if file.len()? > 0 => Ok(()),
            Ok(mut file) => file.replace_contents(b"killed"),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                let resolved = resolve_task_layout(&paths.session_dir, &paths.task_id)?;
                randomized_atomic_replace(
                    &resolved.dirs.io,
                    &resolved.paths.artifact_name(TaskArtifact::Exit),
                    b"killed",
                )
            }
            Err(error) => Err(error),
        };
        match result {
            Err(error)
                if attempts < 3
                    && error.kind() == io::ErrorKind::Interrupted
                    && error.to_string().contains(ARTIFACT_CONCURRENTLY_REPLACED) => {}
            other => return other,
        }
    }
}

pub fn read_exit_marker(paths: &TaskPaths) -> io::Result<Option<ExitMarker>> {
    let mut file = match open_task_artifact(paths, TaskArtifact::Exit) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    #[cfg(test)]
    work_counts::record_marker_read();
    let mut content = String::new();
    file.read_to_string(&mut content)?;
    let content = content.trim();
    if content.is_empty() {
        return Ok(None);
    }
    if content == "killed" {
        return Ok(Some(ExitMarker::Killed));
    }
    Ok(content.parse::<i32>().ok().map(ExitMarker::Code))
}

/// Publish a terminal marker through its validated I/O directory. When proof is
/// unavailable, remove only an empty placeholder, never an existing result.
pub(crate) fn record_terminal_artifact(
    paths: &TaskPaths,
    artifact: TaskArtifact,
    content: Option<&[u8]>,
) -> io::Result<()> {
    let resolved = resolve_task_layout(&paths.session_dir, &paths.task_id)?;
    let name = resolved.paths.artifact_name(artifact);
    if let Some(content) = content {
        return randomized_atomic_replace(&resolved.dirs.io, &name, content);
    }
    match resolved.dirs.io.open_file(&name, false) {
        Ok(file) if file.metadata()?.len() == 0 => {
            drop(file);
            resolved.dirs.io.remove_file(&name)
        }
        Ok(_) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

pub(crate) fn persist_terminal_exit(paths: &TaskPaths, metadata: &mut PersistedTask) {
    let (content, unavailable) = match metadata.exit_code {
        Some(code) if i32::try_from(code).is_ok() => (Some(format!("{code}\n")), None),
        Some(code) => (None, Some(format!(
            "exit code {code} is recorded in task metadata but exceeds the local exit marker's i32 range",
        ))),
        None if metadata.status == BgTaskStatus::Killed => (Some("killed\n".into()), None),
        None => (None, Some(metadata.status_reason.clone().unwrap_or_else(||
            "outcome unknown: no exit status was received".into()
        ))),
    };
    metadata.exit_marker_unavailable = match record_terminal_artifact(
        paths,
        TaskArtifact::Exit,
        content.as_deref().map(str::as_bytes),
    ) {
        Ok(()) => unavailable,
        Err(error) => Some(format!("exit marker persistence failed: {error}")),
    };
}

pub fn randomized_atomic_replace(dir: &PinnedDir, name: &OsStr, content: &[u8]) -> io::Result<()> {
    atomic_replace(dir, name, content, ReplaceSync::None)
}

pub(crate) fn sync_task_file(file: &File) -> io::Result<()> {
    #[cfg(test)]
    work_counts::record_sync();
    file.sync_all()
}

fn atomic_replace(
    dir: &PinnedDir,
    name: &OsStr,
    content: &[u8],
    sync: ReplaceSync,
) -> io::Result<()> {
    for _ in 0..32 {
        let temporary = random_temp_name()?;
        let mut file = match dir.open_new_file(&temporary) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        };
        let result = (|| {
            file.write_all(content)?;
            if sync != ReplaceSync::None {
                sync_task_file(&file)?;
            }
            // Atomic replacement protects readers after a daemon kill. Task
            // history is mirrored in a weaker database and is not a durable log.
            validate_regular_handle(&file)?;
            dir.rename(&temporary, name)?;
            if sync == ReplaceSync::FileAndDirectory {
                sync_task_file(&dir.file)?;
            }
            Ok(())
        })();
        if result.is_err() {
            let _ = dir.remove_file(&temporary);
        }
        return result;
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "failed to allocate a randomized atomic-write name",
    ))
}

pub fn create_control_file(dirs: &TaskDirs, name: &str, content: &[u8]) -> io::Result<File> {
    let mut file = dirs.control.open_new_file(OsStr::new(name))?;
    file.write_all(content)?;
    // These immutable payloads are verified and read through held handles
    // before spawning. They need cross-process visibility, not power-loss
    // durability: recovery never re-executes a task from its payload files.
    file.seek(SeekFrom::Start(0))?;
    validate_regular_handle(&file)?;
    Ok(file)
}

pub fn open_control_file(task: &ResolvedTask, name: &str) -> io::Result<File> {
    if name.is_empty() || name.contains('/') || name.contains('\\') || name == "." || name == ".." {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid control file name",
        ));
    }
    task.dirs.control.open_file(OsStr::new(name), false)
}

#[cfg(test)]
pub(crate) mod work_counts {
    use std::cell::Cell;

    #[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
    pub(crate) struct Counts {
        pub opens: usize,
        pub metadata_reads: usize,
        pub parses: usize,
        pub marker_reads: usize,
        pub stats: usize,
        pub gc_entries: usize,
        pub artifact_bytes: usize,
        pub artifact_opens: usize,
        pub syncs: usize,
        pub renames: usize,
    }

    thread_local! {
        static COUNTS: Cell<Counts> = Cell::new(Counts::default());
    }

    fn record(update: impl FnOnce(&mut Counts)) {
        COUNTS.with(|cell| {
            let mut counts = cell.get();
            update(&mut counts);
            cell.set(counts);
        });
    }

    pub(crate) fn record_open() {
        record(|counts| counts.opens += 1);
    }
    pub(crate) fn record_sync() {
        record(|counts| counts.syncs += 1);
    }
    pub(crate) fn record_rename() {
        record(|counts| counts.renames += 1);
    }
    pub(crate) fn record_bytes(bytes: usize) {
        record(|counts| counts.artifact_bytes += bytes);
    }
    pub(crate) fn record_artifact_open() {
        record(|counts| counts.artifact_opens += 1);
    }
    pub(crate) fn record_read() {
        record(|counts| counts.metadata_reads += 1);
    }
    pub(crate) fn record_marker_read() {
        record(|counts| counts.marker_reads += 1);
    }
    pub(crate) fn record_stat() {
        record(|counts| counts.stats += 1);
    }
    pub(crate) fn record_gc_entry() {
        record(|counts| counts.gc_entries += 1);
    }
    pub(crate) fn record_parse() {
        record(|counts| counts.parses += 1);
    }
    pub(crate) fn reset() {
        COUNTS.with(|cell| cell.set(Counts::default()));
    }
    pub(crate) fn get() -> Counts {
        COUNTS.with(Cell::get)
    }
}

#[derive(Debug)]
pub struct ValidatedArtifact {
    file: File,
}

impl ValidatedArtifact {
    fn new(file: File) -> io::Result<Self> {
        validate_regular_handle(&file)?;
        #[cfg(test)]
        work_counts::record_artifact_open();
        Ok(Self { file })
    }

    pub fn len(&self) -> io::Result<u64> {
        validate_regular_handle(&self.file)?;
        #[cfg(test)]
        work_counts::record_stat();
        Ok(self.file.metadata()?.len())
    }

    pub fn rewind(&mut self) -> io::Result<()> {
        self.file.seek(SeekFrom::Start(0)).map(|_| ())
    }

    pub fn tail(&mut self, max_bytes: usize) -> io::Result<(Vec<u8>, bool)> {
        let len = self.len()?;
        let read_len = len.min(max_bytes as u64);
        self.file
            .seek(SeekFrom::Start(len.saturating_sub(read_len)))?;
        let mut bytes = Vec::with_capacity(read_len as usize);
        Read::by_ref(&mut self.file)
            .take(read_len)
            .read_to_end(&mut bytes)?;
        #[cfg(test)]
        work_counts::record_bytes(bytes.len());
        Ok((bytes, len > max_bytes as u64))
    }

    pub fn read_range(&mut self, start: u64, len: u64) -> io::Result<Vec<u8>> {
        self.file.seek(SeekFrom::Start(start))?;
        let mut bytes = Vec::with_capacity(len.min(usize::MAX as u64) as usize);
        Read::by_ref(&mut self.file)
            .take(len)
            .read_to_end(&mut bytes)?;
        #[cfg(test)]
        work_counts::record_bytes(bytes.len());
        Ok(bytes)
    }

    pub fn read_all(&mut self) -> io::Result<Vec<u8>> {
        self.rewind()?;
        let mut bytes = Vec::new();
        self.file.read_to_end(&mut bytes)?;
        #[cfg(test)]
        work_counts::record_bytes(bytes.len());
        Ok(bytes)
    }

    /// Requires a handle from `open_task_artifact_for_write`; a read-only
    /// handle fails the truncate with EINVAL.
    pub fn replace_contents(&mut self, content: &[u8]) -> io::Result<()> {
        validate_regular_handle(&self.file)?;
        self.file.set_len(0)?;
        self.file.seek(SeekFrom::Start(0))?;
        self.file.write_all(content)?;
        Ok(())
    }

    pub fn try_clone_file(&self) -> io::Result<File> {
        validate_regular_handle(&self.file)?;
        self.file.try_clone()
    }
}

impl Read for ValidatedArtifact {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        self.file.read(buffer)
    }
}

impl Seek for ValidatedArtifact {
    fn seek(&mut self, position: SeekFrom) -> io::Result<u64> {
        self.file.seek(position)
    }
}

pub fn open_task_artifact(
    paths: &TaskPaths,
    artifact: TaskArtifact,
) -> io::Result<ValidatedArtifact> {
    retry_artifact_read(|| open_task_artifact_once(paths, artifact))
}

pub(super) fn open_task_artifact_once(
    paths: &TaskPaths,
    artifact: TaskArtifact,
) -> io::Result<ValidatedArtifact> {
    open_task_artifact_with_access(paths, artifact, false)
}

/// Open and sample the same artifact handle, reopening if either validation
/// races an atomic replacement. Callers calculate offsets only after this step.
pub(crate) fn sample_task_artifact(
    paths: &TaskPaths,
    artifact: TaskArtifact,
) -> io::Result<(ValidatedArtifact, u64)> {
    retry_artifact_read(|| {
        let file = open_task_artifact_once(paths, artifact)?;
        let len = file.len()?;
        Ok((file, len))
    })
}

/// Like `open_task_artifact`, but the handle can also rewrite the artifact.
fn open_task_artifact_for_write(
    paths: &TaskPaths,
    artifact: TaskArtifact,
) -> io::Result<ValidatedArtifact> {
    open_task_artifact_with_access(paths, artifact, true)
}

fn open_task_artifact_with_access(
    paths: &TaskPaths,
    artifact: TaskArtifact,
    write: bool,
) -> io::Result<ValidatedArtifact> {
    validate_task_id(&paths.task_id)?;
    let resolved = resolve_task_layout(&paths.session_dir, &paths.task_id)?;
    if resolved.paths.layout != paths.layout {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "background task layout identity changed",
        ));
    }
    // The resolver has already read the current metadata and checked its task
    // identity. Re-reading that same file adds no artifact-path validation;
    // the open below remains relative to the freshly pinned I/O directory.
    let file = resolved
        .dirs
        .io
        .open_file(&resolved.paths.artifact_name(artifact), write)?;
    ValidatedArtifact::new(file)
}

pub fn replace_artifact_with_tail(
    paths: &TaskPaths,
    artifact: TaskArtifact,
    retain_bytes: u64,
) -> io::Result<u64> {
    let (mut source, len) = sample_task_artifact(paths, artifact)?;
    if len <= retain_bytes {
        return Ok(0);
    }
    let mut tail = source.read_range(len.saturating_sub(retain_bytes), retain_bytes)?;
    align_tail_start(&mut tail);
    let resolved = resolve_task_layout(&paths.session_dir, &paths.task_id)?;
    randomized_atomic_replace(
        &resolved.dirs.io,
        &resolved.paths.artifact_name(artifact),
        &tail,
    )?;
    Ok(len.saturating_sub(tail.len() as u64))
}

#[derive(Debug)]
pub struct TaskIoHandles {
    pub dirs: TaskDirs,
    stdout: Option<File>,
    stderr: Option<File>,
    exit: File,
    pty: Option<File>,
    pipeline_status: Option<File>,
    sandbox_unavailable: File,
    write_counter: crate::write_ledger::Counter,
}

impl TaskIoHandles {
    /// Replay fallback retains the same validated task artifacts rather than
    /// creating a new task or replacing its output with another set of files.
    #[cfg(unix)]
    pub(crate) fn reopen(task: &ResolvedTask) -> io::Result<Self> {
        let open = |a: TaskArtifact| task.dirs.io.open_file(OsStr::new(a.file_name()), true);
        let mut stdout = open(TaskArtifact::Stdout)?;
        stdout.seek(SeekFrom::End(0))?;
        let mut stderr = open(TaskArtifact::Stderr)?;
        stderr.seek(SeekFrom::End(0))?;
        Ok(Self {
            dirs: task.dirs.clone(),
            stdout: Some(stdout),
            stderr: Some(stderr),
            exit: open(TaskArtifact::Exit)?,
            pipeline_status: Some(open(TaskArtifact::PipelineStatus)?),
            pty: None,
            sandbox_unavailable: open(TaskArtifact::SandboxUnavailable)?,
            write_counter: crate::write_ledger::register(
                crate::write_ledger::Domain::BashTaskIo,
                task.paths.dir.display().to_string(),
            ),
        })
    }
    pub fn create(
        task: &ResolvedTask,
        mode: BgMode,
        capture_pipeline_status: bool,
    ) -> io::Result<Self> {
        #[cfg(test)]
        match TASK_IO_FAULT.with(std::cell::Cell::get) {
            Some(TaskIoFault::OutputHandlesEnospc) => return Err(task_storage_full_for_test()),
            Some(TaskIoFault::OutputHandlesMisleadingText) => {
                return Err(io::Error::other(
                    "storage limit exceeded; startup deadline expired",
                ));
            }
            _ => {}
        }
        if task.paths.layout != TaskLayout::Directory {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "new task output handles require the directory layout",
            ));
        }
        let (stdout, stderr, pty) = match mode {
            BgMode::Pipes => (
                Some(
                    task.dirs
                        .io
                        .open_new_file(OsStr::new(TaskArtifact::Stdout.file_name()))?,
                ),
                Some(
                    task.dirs
                        .io
                        .open_new_file(OsStr::new(TaskArtifact::Stderr.file_name()))?,
                ),
                None,
            ),
            BgMode::Pty => (
                None,
                None,
                Some(
                    task.dirs
                        .io
                        .open_new_file(OsStr::new(TaskArtifact::Pty.file_name()))?,
                ),
            ),
        };
        Ok(Self {
            dirs: task.dirs.clone(),
            stdout,
            stderr,
            exit: task
                .dirs
                .io
                .open_new_file(OsStr::new(TaskArtifact::Exit.file_name()))?,
            pipeline_status: capture_pipeline_status
                .then(|| {
                    task.dirs
                        .io
                        .open_new_file(OsStr::new(TaskArtifact::PipelineStatus.file_name()))
                })
                .transpose()?,
            pty,
            sandbox_unavailable: task
                .dirs
                .io
                .open_new_file(OsStr::new(TaskArtifact::SandboxUnavailable.file_name()))?,
            write_counter: crate::write_ledger::register(
                crate::write_ledger::Domain::BashTaskIo,
                task.paths.dir.display().to_string(),
            ),
        })
    }

    pub fn clone_file(&self, artifact: TaskArtifact) -> io::Result<File> {
        let file = match artifact {
            TaskArtifact::Stdout => self.stdout.as_ref(),
            TaskArtifact::Stderr => self.stderr.as_ref(),
            TaskArtifact::Exit => Some(&self.exit),
            TaskArtifact::PipelineStatus => self.pipeline_status.as_ref(),
            TaskArtifact::Pty => self.pty.as_ref(),
            TaskArtifact::SandboxUnavailable => Some(&self.sandbox_unavailable),
        }
        .ok_or_else(|| {
            io::Error::new(io::ErrorKind::NotFound, "task artifact is not pre-opened")
        })?;
        validate_regular_handle(file)?;
        file.try_clone()
    }

    #[cfg(unix)]
    pub fn inheritable_file(&self, artifact: TaskArtifact) -> io::Result<File> {
        let file = self.clone_file(artifact)?;
        // Only the child's pre-exec allowlist may make a marker inheritable.
        // Clearing CLOEXEC here would expose it to unrelated concurrent spawns
        // in the daemon before the owning wrapper has even been launched.
        set_close_on_exec(file.as_raw_fd(), true)?;
        Ok(file)
    }

    pub fn write(&mut self, artifact: TaskArtifact, content: &[u8]) -> io::Result<()> {
        let file = match artifact {
            TaskArtifact::Stdout => self.stdout.as_mut(),
            TaskArtifact::Stderr => self.stderr.as_mut(),
            TaskArtifact::Exit => Some(&mut self.exit),
            TaskArtifact::PipelineStatus => self.pipeline_status.as_mut(),
            TaskArtifact::Pty => self.pty.as_mut(),
            TaskArtifact::SandboxUnavailable => Some(&mut self.sandbox_unavailable),
        }
        .ok_or_else(|| {
            io::Error::new(io::ErrorKind::NotFound, "task artifact is not pre-opened")
        })?;
        validate_regular_handle(file)?;
        file.set_len(0)?;
        file.seek(SeekFrom::Start(0))?;
        file.write_all(content)?;
        self.write_counter.credit_logical(content.len() as u64);
        Ok(())
    }

    pub fn artifact_len(&self, artifact: TaskArtifact) -> io::Result<u64> {
        let file = match artifact {
            TaskArtifact::Stdout => self.stdout.as_ref(),
            TaskArtifact::Stderr => self.stderr.as_ref(),
            TaskArtifact::Exit => Some(&self.exit),
            TaskArtifact::PipelineStatus => self.pipeline_status.as_ref(),
            TaskArtifact::Pty => self.pty.as_ref(),
            TaskArtifact::SandboxUnavailable => Some(&self.sandbox_unavailable),
        }
        .ok_or_else(|| {
            io::Error::new(io::ErrorKind::NotFound, "task artifact is not pre-opened")
        })?;
        validate_regular_handle(file)?;
        #[cfg(test)]
        work_counts::record_stat();
        Ok(file.metadata()?.len())
    }
}

pub fn repin_task_io(paths: &TaskPaths) -> io::Result<TaskDirs> {
    let resolved = resolve_task_layout(&paths.session_dir, &paths.task_id)?;
    let metadata = read_task_at(&resolved)?;
    if metadata.task_id != paths.task_id {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "background task metadata identity mismatch",
        ));
    }
    Ok(resolved.dirs)
}

pub fn unix_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}

fn random_task_id() -> io::Result<String> {
    let mut bytes = [0_u8; 8];
    getrandom::fill(&mut bytes).map_err(io::Error::other)?;
    Ok(format!(
        "bash-{}",
        bytes
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>()
    ))
}

fn random_temp_name() -> io::Result<OsString> {
    let mut bytes = [0_u8; 16];
    getrandom::fill(&mut bytes).map_err(io::Error::other)?;
    Ok(OsString::from(format!(
        ".aft-tmp-{}",
        bytes
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>()
    )))
}

#[cfg(test)]
pub(crate) fn open_unregistered_artifact(path: &Path) -> io::Result<ValidatedArtifact> {
    ValidatedArtifact::new(open_validated_path(path, false)?)
}

#[cfg(test)]
pub(crate) fn replace_unregistered_with_tail(path: &Path, retain_bytes: u64) -> io::Result<u64> {
    let mut source = open_unregistered_artifact(path)?;
    let len = source.len()?;
    if len <= retain_bytes {
        return Ok(0);
    }
    let mut tail = source.read_range(len.saturating_sub(retain_bytes), retain_bytes)?;
    align_tail_start(&mut tail);
    let parent = PinnedDir::open(path.parent().unwrap_or_else(|| Path::new(".")))?;
    let name = path
        .file_name()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "path has no file name"))?;
    randomized_atomic_replace(&parent, name, &tail)?;
    Ok(len.saturating_sub(tail.len() as u64))
}

fn align_tail_start(bytes: &mut Vec<u8>) {
    let prefix = bytes
        .iter()
        .take_while(|byte| **byte & 0xc0 == 0x80)
        .count();
    if prefix > 0 {
        bytes.drain(..prefix);
    }
}

#[cfg(unix)]
fn clear_nonblocking(file: &File) -> io::Result<()> {
    let flags = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GETFL) };
    if flags == -1 {
        return Err(io::Error::last_os_error());
    }
    if flags & libc::O_NONBLOCK != 0 {
        let result =
            unsafe { libc::fcntl(file.as_raw_fd(), libc::F_SETFL, flags & !libc::O_NONBLOCK) };
        if result == -1 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

fn open_validated_path(path: &Path, write: bool) -> io::Result<File> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let name = path
        .file_name()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "path has no file name"))?;
    PinnedDir::open(parent)?.open_file(name, write)
}

#[cfg(unix)]
fn openat_file(dirfd: RawFd, name: &OsStr, flags: i32, mode: libc::mode_t) -> io::Result<File> {
    let name = os_cstring(name)?;
    let fd = unsafe { libc::openat(dirfd, name.as_ptr(), flags, libc::c_uint::from(mode)) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { File::from_raw_fd(fd) })
}

#[cfg(unix)]
fn os_cstring(value: &OsStr) -> io::Result<CString> {
    CString::new(value.as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path contains a NUL byte"))
}

fn validate_directory_handle(file: &File) -> io::Result<()> {
    // Reject reparse points before classifying a non-directory as a file that
    // recursive cleanup may unlink. Junctions must never enter that fallback.
    #[cfg(windows)]
    validate_windows_handle(file, true)?;
    #[cfg(test)]
    work_counts::record_stat();
    let metadata = file.metadata()?;
    if !metadata.is_dir() {
        #[cfg(unix)]
        let kind = io::ErrorKind::InvalidData;
        // CreateFile's BACKUP_SEMANTICS permits opening directories, but unlike
        // Unix O_DIRECTORY it also opens regular files. Match O_DIRECTORY's
        // error so remove_tree_contents can unlink ordinary IO artifacts.
        #[cfg(windows)]
        let kind = io::ErrorKind::NotADirectory;
        return Err(io::Error::new(
            kind,
            "expected a non-reparse directory handle",
        ));
    }
    Ok(())
}

fn validate_regular_handle(file: &File) -> io::Result<()> {
    #[cfg(all(test, unix))]
    BEFORE_ARTIFACT_VALIDATION.with(|slot| {
        if let Some(mut hook) = slot.take() {
            hook(file);
            slot.replace(Some(hook));
        }
    });
    #[cfg(test)]
    work_counts::record_stat();
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "task artifact is not a regular file",
        ));
    }
    // Link-count semantics: >1 means the artifact is aliased somewhere else on
    // disk (tamper suspicion, refuse); exactly 0 means the file was unlinked or
    // atomically replaced AFTER we opened it — on Windows the child's own
    // temp+rename exit write races a daemon kill-marker write and leaves the
    // superseded handle at zero links. That is a benign concurrent replace, not
    // an attack; report it distinctly so callers can re-open the replacement.
    #[cfg(unix)]
    match metadata.nlink() {
        1 => {}
        0 => {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                ARTIFACT_CONCURRENTLY_REPLACED,
            ));
        }
        _ => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "task artifact has multiple hard links",
            ));
        }
    }
    #[cfg(windows)]
    validate_windows_handle(file, false)?;
    Ok(())
}

/// Marker message for a validated-open that lost a race against an atomic
/// replacement of the same artifact (see link-count semantics above).
pub(crate) const ARTIFACT_CONCURRENTLY_REPLACED: &str = "task artifact was concurrently replaced";

#[cfg(all(test, unix))]
std::thread_local! {
    static BEFORE_ARTIFACT_VALIDATION: std::cell::RefCell<Option<Box<dyn FnMut(&File)>>> = const { std::cell::RefCell::new(None) };
}

#[cfg(unix)]
pub fn set_close_on_exec(fd: RawFd, enabled: bool) -> io::Result<()> {
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
    if flags < 0 {
        return Err(io::Error::last_os_error());
    }
    let flags = if enabled {
        flags | libc::FD_CLOEXEC
    } else {
        flags & !libc::FD_CLOEXEC
    };
    if unsafe { libc::fcntl(fd, libc::F_SETFD, flags) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(windows)]
const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
#[cfg(windows)]
const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
#[cfg(windows)]
const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0000_0400;
#[cfg(windows)]
const FILE_TYPE_DISK: u32 = 0x0001;
#[cfg(windows)]
const HANDLE_FLAG_INHERIT: u32 = 0x0000_0001;
#[cfg(windows)]
const GENERIC_READ: u32 = 0x8000_0000;
#[cfg(windows)]
const DELETE_ACCESS: u32 = 0x0001_0000;
#[cfg(windows)]
const SYNCHRONIZE_ACCESS: u32 = 0x0010_0000;
#[cfg(windows)]
const FILE_SHARE_READ_WRITE_DELETE: u32 = 0x0000_0007;
#[cfg(windows)]
const FILE_DISPOSITION_INFO: i32 = 4;
#[cfg(windows)]
const FILE_DISPOSITION_INFO_EX: i32 = 21;
#[cfg(windows)]
const FILE_DISPOSITION_FLAG_DELETE: u32 = 0x0000_0001;
#[cfg(windows)]
const FILE_DISPOSITION_FLAG_POSIX_SEMANTICS: u32 = 0x0000_0002;
#[cfg(windows)]
const FILE_DIRECTORY_FILE: u32 = 0x0000_0001;
#[cfg(windows)]
const FILE_SYNCHRONOUS_IO_NONALERT: u32 = 0x0000_0020;
#[cfg(windows)]
const FILE_OPEN_FOR_BACKUP_INTENT: u32 = 0x0000_4000;
#[cfg(windows)]
const FILE_OPEN_REPARSE_POINT: u32 = 0x0020_0000;

#[cfg(any(windows, test))]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum WindowsDeleteDisposition {
    Posix,
    Legacy,
}

#[cfg(any(windows, test))]
#[derive(Debug, PartialEq, Eq)]
enum DeleteDispositionOutcome {
    Removed,
    RemovedWithWarning(&'static str),
}

#[cfg(any(windows, test))]
fn record_delete_outcome(
    outcome: DeleteDispositionOutcome,
    path: &Path,
    warnings: &mut Vec<String>,
) {
    if let DeleteDispositionOutcome::RemovedWithWarning(reason) = outcome {
        warnings.push(format!(
            "removed_with_warning: {}: {reason}",
            path.display()
        ));
    }
}

#[cfg(any(windows, test))]
fn windows_delete_with_fallback(
    mut set_disposition: impl FnMut(WindowsDeleteDisposition) -> io::Result<()>,
) -> io::Result<DeleteDispositionOutcome> {
    match set_disposition(WindowsDeleteDisposition::Posix) {
        Ok(()) => Ok(DeleteDispositionOutcome::Removed),
        // ERROR_NOT_SUPPORTED / ERROR_INVALID_PARAMETER are capability failures,
        // not permission, sharing, or non-empty-directory errors.
        Err(error) if matches!(error.raw_os_error(), Some(50 | 87)) => {
            set_disposition(WindowsDeleteDisposition::Legacy)?;
            Ok(DeleteDispositionOutcome::RemovedWithWarning(
                "POSIX delete unsupported on this volume; entry pending until handles close",
            ))
        }
        Err(error) => Err(error),
    }
}

#[test]
fn legacy_delete_disposition_is_reported_as_removed_with_warning() {
    let mut calls = Vec::new();
    let result = windows_delete_with_fallback(|disposition| {
        calls.push(disposition);
        match disposition {
            WindowsDeleteDisposition::Posix => Err(io::Error::from_raw_os_error(50)),
            WindowsDeleteDisposition::Legacy => Ok(()),
        }
    });
    assert_eq!(
        calls,
        [
            WindowsDeleteDisposition::Posix,
            WindowsDeleteDisposition::Legacy
        ]
    );
    let mut warnings = Vec::new();
    let mut removed = 0;
    record_delete_outcome(
        result.expect("legacy disposition accepted deletion"),
        Path::new("cache/payload"),
        &mut warnings,
    );
    removed += 1;
    assert_eq!(removed, 1);
    assert_eq!(warnings.len(), 1);
    assert!(warnings[0].starts_with("removed_with_warning:"));
    assert!(warnings[0].contains("pending until handles close"));
}

#[cfg(windows)]
#[derive(Clone, Copy)]
enum RepairOpen {
    Directory,
    ReadFile,
    DeleteFile,
    Coordination,
    NewDirectory,
    NewFile,
    RenameEntry,
}

#[cfg(windows)]
fn windows_relative_repair_file(
    parent: &File,
    leaf: &OsStr,
    operation: RepairOpen,
) -> io::Result<File> {
    use std::os::windows::ffi::OsStrExt;
    let mut wide = leaf.encode_wide().collect::<Vec<_>>();
    if wide.contains(&0) {
        return Err(io::Error::other("NUL in repair entry name"));
    }
    let bytes = u16::try_from(
        wide.len()
            .checked_mul(2)
            .ok_or_else(|| io::Error::other("entry name overflow"))?,
    )
    .map_err(io::Error::other)?;
    let mut name = WindowsUnicodeString {
        length: bytes,
        maximum_length: bytes,
        buffer: wide.as_mut_ptr(),
    };
    let mut object = WindowsObjectAttributes {
        length: std::mem::size_of::<WindowsObjectAttributes>() as u32,
        root_directory: parent.as_raw_handle(),
        object_name: &mut name,
        attributes: 0x40,
        security_descriptor: std::ptr::null_mut(),
        security_quality_of_service: std::ptr::null_mut(),
    };
    let mut handle = std::ptr::null_mut();
    let mut io_status = std::mem::MaybeUninit::<WindowsIoStatusBlock>::uninit();
    let directory = matches!(operation, RepairOpen::Directory | RepairOpen::NewDirectory);
    let create = matches!(operation, RepairOpen::Coordination);
    let new = matches!(operation, RepairOpen::NewDirectory | RepairOpen::NewFile);
    let access = GENERIC_READ
        | SYNCHRONIZE_ACCESS
        | if create || new {
            0x4000_0000
        } else if matches!(operation, RepairOpen::DeleteFile | RepairOpen::RenameEntry) {
            DELETE_ACCESS
        } else {
            0
        };
    let options = FILE_SYNCHRONOUS_IO_NONALERT
        | FILE_OPEN_FOR_BACKUP_INTENT
        | FILE_OPEN_REPARSE_POINT
        | if directory {
            FILE_DIRECTORY_FILE
        } else if matches!(operation, RepairOpen::RenameEntry) {
            0
        } else {
            0x40
        };
    // The name and output buffers outlive this synchronous call. RootDirectory
    // names the pinned parent handle, so no ancestor pathname is resolved.
    let status = unsafe {
        NtCreateFile(
            &mut handle,
            access,
            &mut object,
            io_status.as_mut_ptr(),
            std::ptr::null_mut(),
            if directory { 0x10 } else { 0x80 },
            FILE_SHARE_READ_WRITE_DELETE,
            if create {
                3
            } else if new {
                2
            } else {
                1
            },
            options,
            std::ptr::null_mut(),
            0,
        )
    };
    if status < 0 {
        return Err(io::Error::from_raw_os_error(
            unsafe { RtlNtStatusToDosError(status) } as i32,
        ));
    }
    // A successful NT create returned an owned, non-inheritable file handle.
    Ok(unsafe { File::from_raw_handle(handle) })
}

#[cfg(windows)]
fn windows_repair_rename(
    parent: &File,
    from: &OsStr,
    target: &File,
    to: &OsStr,
    replace: bool,
) -> io::Result<()> {
    let source = windows_relative_repair_file(parent, from, RepairOpen::RenameEntry)?;
    windows_repair_rename_source(&source, target, to, replace)
}

#[cfg(windows)]
fn windows_repair_rename_source(
    source: &File,
    target: &File,
    to: &OsStr,
    replace: bool,
) -> io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    #[repr(C)]
    struct RenameInfo {
        replace: u8,
        root: *mut std::ffi::c_void,
        length: u32,
        name: [u16; 1],
    }
    #[link(name = "ntdll")]
    unsafe extern "system" {
        fn NtSetInformationFile(
            handle: *mut std::ffi::c_void,
            status: *mut WindowsIoStatusBlock,
            information: *mut std::ffi::c_void,
            length: u32,
            class: u32,
        ) -> i32;
    }
    validate_windows_handle(source, source.metadata()?.is_dir())?;
    let name = to.encode_wide().collect::<Vec<_>>();
    let offset = std::mem::offset_of!(RenameInfo, name);
    let length = offset
        .checked_add(
            name.len()
                .checked_mul(2)
                .ok_or_else(|| io::Error::other("rename name overflow"))?,
        )
        .ok_or_else(|| io::Error::other("rename buffer overflow"))?;
    let mut buffer = vec![
        0usize;
        length
            .max(std::mem::size_of::<RenameInfo>())
            .div_ceil(std::mem::size_of::<usize>())
    ];
    let info = buffer.as_mut_ptr().cast::<RenameInfo>();
    // FileRenameInformation resolves the destination name against the pinned
    // target handle. Source and destination ancestors are never re-resolved.
    let status = unsafe {
        (*info).replace = u8::from(replace);
        (*info).root = target.as_raw_handle();
        (*info).length = u32::try_from(name.len() * 2).map_err(io::Error::other)?;
        std::ptr::copy_nonoverlapping(
            name.as_ptr(),
            buffer.as_mut_ptr().cast::<u8>().add(offset).cast::<u16>(),
            name.len(),
        );
        let mut status = std::mem::MaybeUninit::<WindowsIoStatusBlock>::uninit();
        NtSetInformationFile(
            source.as_raw_handle(),
            status.as_mut_ptr(),
            info.cast(),
            u32::try_from(length).map_err(io::Error::other)?,
            10,
        )
    };
    if status < 0 {
        return Err(io::Error::from_raw_os_error(
            unsafe { RtlNtStatusToDosError(status) } as i32,
        ));
    }
    Ok(())
}

#[cfg(any(windows, test))]
fn parse_repair_directory_names(bytes: &[u8]) -> io::Result<Vec<Vec<u16>>> {
    let mut result = Vec::new();
    let mut offset = 0usize;
    loop {
        let header_end = offset
            .checked_add(12)
            .ok_or_else(|| io::Error::other("directory header overflow"))?;
        let header = bytes
            .get(offset..header_end)
            .ok_or_else(|| io::Error::other("short directory entry header"))?;
        let next = u32::from_le_bytes(header[0..4].try_into().unwrap()) as usize;
        let length = u32::from_le_bytes(header[8..12].try_into().unwrap()) as usize;
        if length % 2 != 0 {
            return Err(io::Error::other("unaligned directory entry name"));
        }
        let end = offset
            .checked_add(12)
            .and_then(|start| start.checked_add(length))
            .ok_or_else(|| io::Error::other("directory entry overflow"))?;
        let name = bytes
            .get(offset + 12..end)
            .ok_or_else(|| io::Error::other("short directory entry name"))?
            .chunks_exact(2)
            .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
            .collect::<Vec<_>>();
        if name != [46] && name != [46, 46] {
            result.push(name);
        }
        if next == 0 {
            return Ok(result);
        }
        if next < 12 + length {
            return Err(io::Error::other("overlapping directory entry"));
        }
        offset = offset
            .checked_add(next)
            .ok_or_else(|| io::Error::other("directory entry offset overflow"))?;
    }
}

#[cfg(test)]
#[test]
fn repair_directory_records_reject_invalid_lengths_and_offsets() {
    fn record(name: &str, next: u32) -> Vec<u8> {
        let name = name.encode_utf16().collect::<Vec<_>>();
        let mut bytes = vec![0u8; 12];
        bytes[0..4].copy_from_slice(&next.to_le_bytes());
        bytes[8..12].copy_from_slice(&((name.len() * 2) as u32).to_le_bytes());
        for unit in name {
            bytes.extend_from_slice(&unit.to_le_bytes());
        }
        bytes
    }
    let mut bytes = record("a", 16);
    bytes.resize(16, 0);
    bytes.extend(record("b", 0));
    assert_eq!(
        parse_repair_directory_names(&bytes).unwrap(),
        vec![vec![97], vec![98]]
    );
    assert!(parse_repair_directory_names(&[0; 11]).is_err());
    bytes[0..4].copy_from_slice(&13u32.to_le_bytes());
    assert!(parse_repair_directory_names(&bytes).is_err());
    bytes[8..12].copy_from_slice(&u32::MAX.to_le_bytes());
    assert!(parse_repair_directory_names(&bytes).is_err());
    assert!(parse_repair_directory_names(&record(".", 0))
        .unwrap()
        .is_empty());
    let mut offset = record("", u32::MAX);
    offset[8..12].copy_from_slice(&0u32.to_le_bytes());
    assert!(parse_repair_directory_names(&offset).is_err());
}

#[cfg(windows)]
fn windows_repair_names(file: &File, budget: &mut RepairBudget) -> io::Result<Vec<OsString>> {
    use std::os::windows::ffi::OsStringExt;
    let mut result = Vec::new();
    let mut restart = 1u8;
    loop {
        let mut bytes = vec![0u8; 64 * 1024];
        let mut io_status = std::mem::MaybeUninit::<WindowsIoStatusBlock>::uninit();
        // FileNamesInformation is a sequence of length-checked UTF-16 records.
        // The directory handle is synchronous and all buffers remain alive.
        let status = unsafe {
            NtQueryDirectoryFile(
                file.as_raw_handle(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                io_status.as_mut_ptr(),
                bytes.as_mut_ptr().cast(),
                bytes.len() as u32,
                12,
                0,
                std::ptr::null_mut(),
                restart,
            )
        };
        if status as u32 == 0x8000_0006 {
            return Ok(result);
        }
        if status != 0 {
            return Err(io::Error::from_raw_os_error(
                unsafe { RtlNtStatusToDosError(status) } as i32,
            ));
        }
        // Success initializes IO_STATUS_BLOCK and its byte count.
        let filled = unsafe { io_status.assume_init() }.information;
        if filled == 0 {
            return Ok(result);
        }
        let records = parse_repair_directory_names(
            bytes
                .get(..filled)
                .ok_or_else(|| io::Error::other("invalid directory byte count"))?,
        )?;
        for name in records {
            budget.entry()?;
            result.push(OsString::from_wide(&name));
        }
        restart = 0;
    }
}

#[cfg(windows)]
fn windows_reopen_directory_for_delete(file: &File) -> io::Result<File> {
    // ReOpenFile rejects directories with STATUS_FILE_IS_A_DIRECTORY, mapped
    // to Win32 Access Denied, even when BACKUP_SEMANTICS is requested. An empty
    // NT-relative name reopens the pinned object without resolving its path.
    let mut empty_name = 0_u16;
    let mut name = WindowsUnicodeString {
        length: 0,
        maximum_length: 2,
        buffer: &mut empty_name,
    };
    let mut object = WindowsObjectAttributes {
        length: std::mem::size_of::<WindowsObjectAttributes>() as u32,
        root_directory: file.as_raw_handle(),
        object_name: &mut name,
        attributes: 0,
        security_descriptor: std::ptr::null_mut(),
        security_quality_of_service: std::ptr::null_mut(),
    };
    let mut handle = std::ptr::null_mut();
    let mut io_status = std::mem::MaybeUninit::<WindowsIoStatusBlock>::uninit();
    let status = unsafe {
        NtOpenFile(
            &mut handle,
            GENERIC_READ | DELETE_ACCESS | SYNCHRONIZE_ACCESS,
            &mut object,
            io_status.as_mut_ptr(),
            FILE_SHARE_READ_WRITE_DELETE,
            FILE_DIRECTORY_FILE
                | FILE_SYNCHRONOUS_IO_NONALERT
                | FILE_OPEN_FOR_BACKUP_INTENT
                | FILE_OPEN_REPARSE_POINT,
        )
    };
    if status < 0 {
        return Err(io::Error::from_raw_os_error(
            unsafe { RtlNtStatusToDosError(status) } as i32,
        ));
    }
    let reopened = unsafe { File::from_raw_handle(handle) };
    let pinned_identity = windows_file_information(file)?;
    let reopened_identity = windows_file_information(&reopened)?;
    if pinned_identity.volume_serial_number != reopened_identity.volume_serial_number
        || pinned_identity.file_index_high != reopened_identity.file_index_high
        || pinned_identity.file_index_low != reopened_identity.file_index_low
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "delete handle does not refer to the pinned task directory",
        ));
    }
    Ok(reopened)
}

#[cfg(windows)]
fn windows_delete_pinned_handle(file: &File, directory: bool) -> io::Result<()> {
    let outcome = windows_delete_pinned_handle_outcome(file, directory)?;
    let mut warnings = Vec::new();
    record_delete_outcome(outcome, Path::new("pinned artifact"), &mut warnings);
    for warning in warnings {
        crate::slog_warn!("{warning}");
    }
    Ok(())
}

#[cfg(windows)]
fn windows_delete_pinned_handle_outcome(
    file: &File,
    directory: bool,
) -> io::Result<DeleteDispositionOutcome> {
    // Reopen the pinned object, not a path that could be swapped. A distinct
    // DELETE file object is necessary: POSIX disposition unlinks the name when
    // that handle closes, even if the original pins remain open.
    let deletion = if directory {
        windows_reopen_directory_for_delete(file)?
    } else {
        let handle = unsafe {
            ReOpenFile(
                file.as_raw_handle(),
                GENERIC_READ | DELETE_ACCESS,
                FILE_SHARE_READ_WRITE_DELETE,
                FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_BACKUP_SEMANTICS,
            )
        };
        if handle as isize == -1 {
            return Err(io::Error::last_os_error());
        }
        unsafe { File::from_raw_handle(handle) }
    };
    if directory {
        validate_directory_handle(&deletion)?;
    } else {
        validate_regular_handle(&deletion)?;
    }
    let result = windows_delete_with_fallback(|disposition| {
        let success = match disposition {
            WindowsDeleteDisposition::Posix => {
                let mut flags =
                    FILE_DISPOSITION_FLAG_DELETE | FILE_DISPOSITION_FLAG_POSIX_SEMANTICS;
                unsafe {
                    SetFileInformationByHandle(
                        deletion.as_raw_handle(),
                        FILE_DISPOSITION_INFO_EX,
                        (&mut flags as *mut u32).cast(),
                        std::mem::size_of_val(&flags) as u32,
                    )
                }
            }
            WindowsDeleteDisposition::Legacy => {
                let mut delete_file = 1_u8; // FILE_DISPOSITION_INFO::DeleteFile (BOOLEAN)
                unsafe {
                    SetFileInformationByHandle(
                        deletion.as_raw_handle(),
                        FILE_DISPOSITION_INFO,
                        (&mut delete_file as *mut u8).cast(),
                        std::mem::size_of_val(&delete_file) as u32,
                    )
                }
            }
        };
        if success == 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    });
    drop(deletion);
    result
}

#[cfg(windows)]
fn windows_file_information(file: &File) -> io::Result<ByHandleFileInformation> {
    let mut information = std::mem::MaybeUninit::<ByHandleFileInformation>::zeroed();
    if unsafe { GetFileInformationByHandle(file.as_raw_handle(), information.as_mut_ptr()) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { information.assume_init() })
}

#[cfg(windows)]
fn validate_windows_handle(file: &File, directory: bool) -> io::Result<()> {
    if file.metadata()?.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "task path is a reparse point",
        ));
    }
    let handle = file.as_raw_handle();
    let file_type = unsafe { GetFileType(handle) };
    if file_type != FILE_TYPE_DISK {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "task artifact is not a regular disk file",
        ));
    }
    let information = windows_file_information(file)?;
    if !directory && information.number_of_links == 0 {
        // Zero links = this handle points at a file that was unlinked or
        // rename-replaced after open (e.g. the child's temp+rename exit write
        // racing a daemon kill-marker write). Benign; caller may re-open.
        return Err(io::Error::new(
            io::ErrorKind::Interrupted,
            ARTIFACT_CONCURRENTLY_REPLACED,
        ));
    }
    if !directory && information.number_of_links > 1 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "task artifact has multiple hard links",
        ));
    }
    if unsafe { SetHandleInformation(handle, HANDLE_FLAG_INHERIT, 0) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let mut flags = 0_u32;
    if unsafe { GetHandleInformation(handle, &mut flags) } == 0 {
        return Err(io::Error::last_os_error());
    }
    if flags & HANDLE_FLAG_INHERIT != 0 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "validated task handles must not be inherited",
        ));
    }
    Ok(())
}

#[cfg(windows)]
#[repr(C)]
struct ByHandleFileInformation {
    file_attributes: u32,
    creation_time: [u32; 2],
    last_access_time: [u32; 2],
    last_write_time: [u32; 2],
    volume_serial_number: u32,
    file_size_high: u32,
    file_size_low: u32,
    number_of_links: u32,
    file_index_high: u32,
    file_index_low: u32,
}

#[cfg(windows)]
#[repr(C)]
struct WindowsUnicodeString {
    length: u16,
    maximum_length: u16,
    buffer: *mut u16,
}

#[cfg(windows)]
#[repr(C)]
struct WindowsObjectAttributes {
    length: u32,
    root_directory: std::os::windows::io::RawHandle,
    object_name: *mut WindowsUnicodeString,
    attributes: u32,
    security_descriptor: *mut std::ffi::c_void,
    security_quality_of_service: *mut std::ffi::c_void,
}

#[cfg(windows)]
#[repr(C)]
struct WindowsIoStatusBlock {
    // The first member is an opaque union of NTSTATUS and a pointer.
    status_or_pointer: usize,
    information: usize,
}

#[cfg(windows)]
#[link(name = "ntdll")]
extern "system" {
    fn NtCreateFile(
        handle: *mut std::os::windows::io::RawHandle,
        access: u32,
        object: *mut WindowsObjectAttributes,
        io_status: *mut WindowsIoStatusBlock,
        allocation_size: *mut i64,
        attributes: u32,
        share: u32,
        disposition: u32,
        options: u32,
        ea: *mut std::ffi::c_void,
        ea_length: u32,
    ) -> i32;
    fn NtQueryDirectoryFile(
        file: std::os::windows::io::RawHandle,
        event: std::os::windows::io::RawHandle,
        apc: *mut std::ffi::c_void,
        context: *mut std::ffi::c_void,
        io_status: *mut WindowsIoStatusBlock,
        buffer: *mut std::ffi::c_void,
        length: u32,
        class: u32,
        single: u8,
        name: *mut WindowsUnicodeString,
        restart: u8,
    ) -> i32;
    fn NtOpenFile(
        handle: *mut std::os::windows::io::RawHandle,
        access: u32,
        object: *mut WindowsObjectAttributes,
        io_status: *mut WindowsIoStatusBlock,
        share: u32,
        options: u32,
    ) -> i32;
    fn RtlNtStatusToDosError(status: i32) -> u32;
}

#[cfg(windows)]
#[link(name = "kernel32")]
extern "system" {
    fn ReOpenFile(
        original: std::os::windows::io::RawHandle,
        access: u32,
        share: u32,
        flags: u32,
    ) -> std::os::windows::io::RawHandle;
    fn SetFileInformationByHandle(
        file: std::os::windows::io::RawHandle,
        class: i32,
        information: *mut std::ffi::c_void,
        size: u32,
    ) -> i32;
    fn GetFileType(file: std::os::windows::io::RawHandle) -> u32;
    fn GetFileInformationByHandle(
        file: std::os::windows::io::RawHandle,
        information: *mut ByHandleFileInformation,
    ) -> i32;
    fn SetHandleInformation(object: std::os::windows::io::RawHandle, mask: u32, flags: u32) -> i32;
    fn GetHandleInformation(object: std::os::windows::io::RawHandle, flags: *mut u32) -> i32;
}

#[cfg(test)]
mod tests {
    #[cfg(unix)]
    #[test]
    fn refused_task_layout_creation_leaves_storage_root_untouched() {
        use std::os::unix::fs::PermissionsExt;

        let fixture = tempfile::tempdir().unwrap();
        for (name, allocate) in [("create", false), ("allocate", true)] {
            let storage = fixture.path().join(name);
            std::fs::create_dir(&storage).unwrap();
            std::fs::set_permissions(&storage, std::fs::Permissions::from_mode(0o755)).unwrap();
            let before_mode = std::fs::metadata(&storage).unwrap().permissions().mode() & 0o777;
            let before_entries = std::fs::read_dir(&storage).unwrap().count();

            let error = crate::production_storage::with_test_account(&storage, false, || {
                let result = if allocate {
                    super::allocate_task_layout(&storage, "session")
                } else {
                    super::create_task_layout(&storage, "session", "bash-0000000000000040")
                };
                result.unwrap_err()
            });

            assert!(
                error.to_string().contains(crate::production_storage::CODE),
                "protected task write was not refused: {error}"
            );
            assert_eq!(
                std::fs::metadata(&storage).unwrap().permissions().mode() & 0o777,
                before_mode,
                "refused task creation changed storage permissions"
            );
            assert_eq!(
                std::fs::read_dir(&storage).unwrap().count(),
                before_entries,
                "refused task creation added a storage entry"
            );
            assert!(!storage.join("bash-tasks").exists());
            assert!(!storage.join(".bash-task-session.lock").exists());
        }
    }

    #[cfg(unix)]
    #[test]
    fn empty_task_directory_is_classified_and_removed_on_delete() {
        let storage = tempfile::tempdir().unwrap();
        let task =
            super::create_task_layout(storage.path(), "empty-layout", "bash-0000000000000010")
                .unwrap();
        let paths = task.paths.clone();
        drop(task);
        std::fs::remove_dir(&paths.control_dir).unwrap();
        std::fs::remove_dir(&paths.io_dir).unwrap();

        let error = super::resolve_uninitialized_task_layout(&paths.session_dir, &paths.task_id)
            .unwrap_err();
        assert!(
            super::is_task_layout_not_found(&error),
            "missing required layout directories were not classified: {error}"
        );
        let error = super::delete_task_bundle(&paths).unwrap_err();
        assert!(super::is_task_layout_not_found(&error));
        assert!(!paths.dir.exists(), "empty task directory was left behind");
    }

    #[test]
    fn task_session_rmdir_waits_for_creator_with_an_open_session_directory() {
        let storage = tempfile::tempdir().unwrap();
        let initial =
            super::create_task_layout(storage.path(), "rmdir-race", "bash-0000000000000001")
                .unwrap();
        let initial_paths = initial.paths.clone();
        let session_dir = initial_paths.session_dir.clone();
        drop(initial);
        std::fs::remove_dir_all(&initial_paths.dir).unwrap();

        let (creator_entered, release_creator) =
            super::install_session_create_gate_for_test(&session_dir);
        std::thread::scope(|scope| {
            let creator = scope.spawn(|| {
                super::create_task_layout(storage.path(), "rmdir-race", "bash-0000000000000002")
            });
            creator_entered
                .recv_timeout(std::time::Duration::from_secs(5))
                .expect("creator did not open the session directory");

            let (remove_started_tx, remove_started_rx) = std::sync::mpsc::channel();
            let (removed_tx, removed_rx) = std::sync::mpsc::channel();
            let remove_session_dir = session_dir.clone();
            let remover = scope.spawn(move || {
                remove_started_tx.send(()).unwrap();
                removed_tx
                    .send(super::remove_empty_session_directory(&remove_session_dir))
                    .unwrap();
            });
            remove_started_rx
                .recv_timeout(std::time::Duration::from_secs(5))
                .unwrap();
            assert!(
                removed_rx
                    .recv_timeout(std::time::Duration::from_millis(100))
                    .is_err(),
                "session removal passed a task creator holding the shared gate"
            );

            release_creator.send(()).unwrap();
            let created = creator.join().unwrap().unwrap();
            remover.join().unwrap();
            let removed = removed_rx.recv().unwrap().unwrap();
            assert!(!removed, "a session containing a new task was removed");
            assert!(created.paths.dir.is_dir());
        });
        assert!(session_dir.join("bash-0000000000000002").is_dir());
    }

    #[test]
    fn parallel_session_layout_creators_keep_their_own_gates() {
        let storage = tempfile::tempdir().unwrap();
        let session_a = super::session_tasks_dir(storage.path(), "parallel-gate-a");
        let session_b = super::session_tasks_dir(storage.path(), "parallel-gate-b");
        let (entered_a, release_a) = super::install_session_create_gate_for_test(&session_a);
        let (entered_b, release_b) = super::install_session_create_gate_for_test(&session_b);

        std::thread::scope(|scope| {
            let creator_a = scope.spawn(|| {
                super::create_task_layout(
                    storage.path(),
                    "parallel-gate-a",
                    "bash-0000000000000030",
                )
            });
            entered_a
                .recv_timeout(std::time::Duration::from_secs(5))
                .expect("session A creator lost its gate");

            let creator_b = scope.spawn(|| {
                super::create_task_layout(
                    storage.path(),
                    "parallel-gate-b",
                    "bash-0000000000000031",
                )
            });
            release_a.send(()).unwrap();
            let created_a = creator_a.join().unwrap().unwrap();
            entered_b
                .recv_timeout(std::time::Duration::from_secs(5))
                .expect("session B creator lost its gate");
            release_b.send(()).unwrap();
            let created_b = creator_b.join().unwrap().unwrap();
            assert!(created_a.paths.dir.is_dir());
            assert!(created_b.paths.dir.is_dir());
        });
    }

    // Freeze the scope-blind version-7 task shape rather than deserializing
    // through PersistedTask, whose new scope field would hide rollback behavior.
    // Serde's unknown-field handling lets that reader ignore the added stamp.
    #[derive(Serialize, Deserialize)]
    struct ScopeBlindV7Task {
        schema_version: u32,
        harness: Option<String>,
        task_id: String,
        session_id: String,
        command: String,
        #[serde(default)]
        pipeline_segments: Vec<String>,
        pipeline_status_unavailable: Option<String>,
        #[serde(default)]
        mode: BgMode,
        workdir: PathBuf,
        project_root: Option<PathBuf>,
        status: BgTaskStatus,
        started_at: u64,
        finished_at: Option<u64>,
        duration_ms: Option<u64>,
        timeout_ms: Option<u64>,
        #[serde(default)]
        default_hard_kill: bool,
        exit_code: Option<i64>,
        child_pid: Option<u32>,
        pgid: Option<i32>,
        live_descendants: Option<Vec<LiveDescendant>>,
        #[serde(default)]
        live_descendants_omitted: usize,
        completion_delivered: bool,
        #[serde(default = "default_notify_on_completion")]
        notify_on_completion: bool,
        #[serde(default = "default_compressed")]
        compressed: bool,
        pty_rows: Option<u16>,
        pty_cols: Option<u16>,
        #[serde(default)]
        scanner_report: Vec<PermissionAsk>,
        #[serde(default)]
        sandbox_native: bool,
        sandbox_temp_dir: Option<PathBuf>,
        status_reason: Option<String>,
        #[serde(default)]
        output_incomplete: bool,
        call_key: Option<crate::bash_background::TaskCallKey>,
        remote: Option<crate::bash_background::registry::remote::RemoteTask>,
        execution_note: Option<String>,
        #[serde(default)]
        incomplete_output: Vec<(u64, u64)>,
        #[serde(default)]
        stripped_env_names: Vec<String>,
        #[serde(default)]
        local_fallback_started: bool,
        #[serde(default)]
        cancelled_call_unannounced: bool,
    }

    fn read_scope_blind_v7(bytes: &[u8]) -> Result<ScopeBlindV7Task, String> {
        let value: serde_json::Value = serde_json::from_slice(bytes).map_err(|e| e.to_string())?;
        let version = value["schema_version"].as_u64().ok_or("missing version")?;
        if !(2..=7).contains(&version) {
            return Err(format!(
                "unsupported background task schema_version {version}"
            ));
        }
        let task: ScopeBlindV7Task = serde_json::from_slice(bytes).map_err(|e| e.to_string())?;
        validate_task_id(&task.task_id).map_err(|e| e.to_string())?;
        Ok(task)
    }

    #[test]
    fn stamped_v7_is_readable_by_scope_blind_v7_reader() {
        let directory = tempfile::tempdir().unwrap();
        let (resolved, mut task) = counted_task(directory.path());
        task.scope = Some(subc_protocol::scope::ScopeStamp::new(
            subc_protocol::Principal::Direct,
            "scope",
            1,
            subc_protocol::scope::ScopeKind::Head,
        ));
        write_task_at(&resolved, &task).unwrap();
        let bytes = std::fs::read(&resolved.paths.json).unwrap();
        let written: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(written["schema_version"], 7);
        assert_eq!(written["scope"]["ref"], "scope");
        let old = read_scope_blind_v7(&bytes).expect("scope-blind reader accepts stamped v7");
        assert_eq!(old.task_id, task.task_id);
        assert_eq!(old.session_id, task.session_id);
        assert_eq!(old.command, task.command);
        assert_eq!(old.status, task.status);
        assert!(serde_json::to_value(old).unwrap().get("scope").is_none());
        assert_eq!(read_task_at(&resolved).unwrap().scope, task.scope);
    }

    #[test]
    fn scope_stamp_task_versions_and_unstamped_v7_are_compatible() {
        let directory = tempfile::tempdir().unwrap();
        let (resolved, mut task) = counted_task(directory.path());
        assert_eq!(SCHEMA_VERSION, 7);
        let stamp = subc_protocol::scope::ScopeStamp::new(
            subc_protocol::Principal::Reserved {
                module_id: "owner".into(),
            },
            "scope",
            1,
            subc_protocol::scope::ScopeKind::Head,
        )
        .with_owner_authorized(true);
        task.scope = Some(stamp.clone());
        task.mark_terminal(
            BgTaskStatus::Killed,
            Some(137),
            Some("scope revoked".into()),
        );
        write_task_at(&resolved, &task).unwrap();
        assert_eq!(read_task_at(&resolved).unwrap().scope, Some(stamp.clone()));
        assert_eq!(
            read_task_at(&resolved).unwrap().status_reason.as_deref(),
            Some("scope revoked")
        );
        let mut value = serde_json::to_value(&task).unwrap();
        // Version-7 background-task files also predate scope tracking.
        // A file without scope must remain readable and must not gain one.
        value.as_object_mut().unwrap().remove("scope");
        std::fs::write(&resolved.paths.json, serde_json::to_vec(&value).unwrap()).unwrap();
        assert!(read_task_at(&resolved).unwrap().scope.is_none());
        value["schema_version"] = serde_json::json!(6);
        std::fs::write(&resolved.paths.json, serde_json::to_vec(&value).unwrap()).unwrap();
        let old = read_task_at(&resolved).unwrap();
        assert_eq!(old.schema_version, 6);
        write_task_at(&resolved, &old).unwrap();
        assert_eq!(read_task_at(&resolved).unwrap().schema_version, 7);
        value["schema_version"] = serde_json::json!(SCHEMA_VERSION + 1);
        std::fs::write(&resolved.paths.json, serde_json::to_vec(&value).unwrap()).unwrap();
        let error = read_task_at(&resolved).unwrap_err().to_string();
        assert!(error.contains("storage_requires_newer_reader"), "{error}");
        assert!(error.contains("7"), "{error}");
    }
    #[test]
    fn durability_bash_record_payload_count() {
        let storage = tempfile::tempdir().unwrap();
        let task =
            create_task_layout(storage.path(), "durability", "bash-0000000000000601").unwrap();
        crate::durability::take();
        for name in [
            "command.sh",
            "wrapper.sh",
            "environment.bin",
            "manifest.blake3",
        ] {
            create_control_file(&task.dirs, name, b"payload").unwrap();
        }
        for state in ["starting", "running", "exited", "completed"] {
            randomized_atomic_replace(
                &task.dirs.control,
                OsStr::new("metadata.json"),
                state.as_bytes(),
            )
            .unwrap();
        }
        let events = crate::durability::take();
        assert_eq!(crate::durability::sync_count(&events), 0, "{events:?}");
        assert_eq!(
            fs::read(task.dirs.control.path.join("metadata.json")).unwrap(),
            b"completed"
        );
    }
    use super::*;

    #[test]
    fn windows_delete_unsupported_uses_legacy_and_reports_removed_with_warning() {
        for code in [50, 87] {
            let mut calls = Vec::new();
            let outcome = windows_delete_with_fallback(|disposition| {
                calls.push(disposition);
                match disposition {
                    WindowsDeleteDisposition::Posix => Err(io::Error::from_raw_os_error(code)),
                    WindowsDeleteDisposition::Legacy => Ok(()),
                }
            })
            .unwrap();
            assert_eq!(
                calls,
                [
                    WindowsDeleteDisposition::Posix,
                    WindowsDeleteDisposition::Legacy
                ]
            );
            assert_eq!(
                outcome,
                DeleteDispositionOutcome::RemovedWithWarning(
                    "POSIX delete unsupported on this volume; entry pending until handles close"
                )
            );
        }
    }

    #[test]
    fn windows_delete_other_errors_never_fall_back() {
        // Access denied, sharing violation, directory not empty, and a synthetic
        // error must not be mistaken for unsupported POSIX semantics.
        for code in [Some(5), Some(32), Some(145), None] {
            let mut calls = Vec::new();
            let error = windows_delete_with_fallback(|disposition| {
                calls.push(disposition);
                match code {
                    Some(code) => Err(io::Error::from_raw_os_error(code)),
                    None => Err(io::Error::other("injected failure")),
                }
            })
            .unwrap_err();
            assert_eq!(calls, [WindowsDeleteDisposition::Posix]);
            assert_eq!(error.raw_os_error(), code);
            if code.is_none() {
                assert_eq!(error.to_string(), "injected failure");
            }
        }
    }

    #[test]
    fn windows_delete_legacy_failure_is_propagated() {
        let mut calls = Vec::new();
        let error = windows_delete_with_fallback(|disposition| {
            calls.push(disposition);
            match disposition {
                WindowsDeleteDisposition::Posix => Err(io::Error::from_raw_os_error(87)),
                WindowsDeleteDisposition::Legacy => Err(io::Error::from_raw_os_error(5)),
            }
        })
        .unwrap_err();
        assert_eq!(
            calls,
            [
                WindowsDeleteDisposition::Posix,
                WindowsDeleteDisposition::Legacy
            ]
        );
        assert_eq!(error.raw_os_error(), Some(5));
    }

    #[test]
    fn windows_delete_posix_success_never_falls_back() {
        let mut calls = Vec::new();
        windows_delete_with_fallback(|disposition| {
            calls.push(disposition);
            Ok(())
        })
        .unwrap();
        assert_eq!(calls, [WindowsDeleteDisposition::Posix]);
    }

    fn valid_id(suffix: u64) -> String {
        format!("bash-{suffix:016x}")
    }

    fn counted_task(storage: &Path) -> (ResolvedTask, PersistedTask) {
        let task = create_task_layout(storage, "counted", &valid_id(42)).unwrap();
        let metadata = PersistedTask::starting(
            task.paths.task_id.clone(),
            "counted".into(),
            "printf 'a realistic long build command'; ".repeat(128),
            storage.into(),
            None,
            None,
            true,
            false,
        );
        write_task_at(&task, &metadata).unwrap();
        (task, metadata)
    }

    #[test]
    fn artifact_open_reads_metadata_once() {
        let storage = tempfile::tempdir().unwrap();
        let (task, _) = counted_task(storage.path());
        let mut handles = TaskIoHandles::create(&task, BgMode::Pipes, false).unwrap();
        let output = b"build output\n".repeat(10_000);
        handles.write(TaskArtifact::Stdout, &output).unwrap();
        work_counts::reset();
        let mut file = open_task_artifact(&task.paths, TaskArtifact::Stdout).unwrap();
        assert_eq!(file.read_all().unwrap(), output);
        let counts = work_counts::get();
        eprintln!("artifact open work: {counts:?}");
        assert_eq!(counts.metadata_reads, 1);
        assert_eq!(counts.parses, 2);
        #[cfg(unix)]
        assert_eq!(counts.opens, 7);
    }

    #[test]
    fn terminal_exit_persistence_reports_missing_proof_wide_codes_and_write_failures() {
        for code in [None, Some(7), Some(i64::from(u32::MAX))] {
            let dir = tempfile::tempdir().unwrap();
            let (task, mut metadata) = counted_task(dir.path());
            fs::write(&task.paths.exit, "").unwrap();
            metadata.mark_terminal(
                if code.is_some() {
                    BgTaskStatus::Failed
                } else {
                    BgTaskStatus::FateUnknown
                },
                code,
                Some("terminal proof lost after restart".into()),
            );
            persist_terminal_exit(&task.paths, &mut metadata);
            assert_eq!(metadata.exit_code, code);
            assert_eq!(metadata.schema_version, SCHEMA_VERSION);
            if code == Some(7) {
                assert_eq!(
                    read_exit_marker(&task.paths).unwrap(),
                    Some(ExitMarker::Code(7))
                );
                assert!(metadata.exit_marker_unavailable.is_none());
            } else {
                assert!(!task.paths.exit.exists());
                assert!(metadata.exit_marker_unavailable.is_some());
            }
            write_task_at(&task, &metadata).unwrap();
            assert_eq!(
                read_task_at(&task).unwrap().exit_marker_unavailable,
                metadata.exit_marker_unavailable
            );
            let mut old = serde_json::to_value(&metadata).unwrap();
            old.as_object_mut()
                .unwrap()
                .remove("exit_marker_unavailable");
            assert!(serde_json::from_value::<PersistedTask>(old)
                .unwrap()
                .exit_marker_unavailable
                .is_none());
        }
        let dir = tempfile::tempdir().unwrap();
        let (task, mut metadata) = counted_task(dir.path());
        fs::create_dir(&task.paths.exit).unwrap();
        metadata.mark_terminal(BgTaskStatus::Completed, Some(0), None);
        persist_terminal_exit(&task.paths, &mut metadata);
        assert_eq!(metadata.exit_code, Some(0));
        assert!(metadata
            .exit_marker_unavailable
            .unwrap()
            .contains("persistence failed"));
    }

    #[test]
    fn missing_exit_marker_reads_metadata_once() {
        let storage = tempfile::tempdir().unwrap();
        let (task, _) = counted_task(storage.path());
        work_counts::reset();
        assert_eq!(read_exit_marker(&task.paths).unwrap(), None);
        let counts = work_counts::get();
        eprintln!("missing marker work: {counts:?}");
        assert_eq!(counts.metadata_reads, 1);
        assert_eq!(counts.parses, 2);
    }

    #[cfg(unix)]
    #[test]
    fn task_lifecycle_round_trips_payloads_and_metadata() {
        let storage = tempfile::tempdir().unwrap();
        let (task, mut metadata) = counted_task(storage.path());
        work_counts::reset();
        // Task metadata uses atomic replacement without a disk flush (the
        // durability design treats bash task history as rebuildable; no
        // flush helper remains in this module), and payload files are only
        // consumed through verified handles.
        write_task_at(&task, &metadata).unwrap();
        for (name, bytes) in [
            (COMMAND_FILE, metadata.command.as_bytes()),
            (WRAPPER_FILE, super::super::process::PAYLOAD_WRAPPER),
            (ENVIRONMENT_FILE, b"PATH=/usr/bin".as_slice()),
            (MANIFEST_FILE, b"verified digest".as_slice()),
        ] {
            let mut held = create_control_file(&task.dirs, name, bytes).unwrap();
            let mut actual = Vec::new();
            held.read_to_end(&mut actual).unwrap();
            assert_eq!(actual, bytes);
        }
        metadata.status = super::super::BgTaskStatus::Running;
        write_task_at(&task, &metadata).unwrap();
        metadata.status = super::super::BgTaskStatus::Completed;
        write_task_at(&task, &metadata).unwrap();
        let counts = work_counts::get();
        eprintln!("task lifecycle work: {counts:?}");
        assert!(read_task_at(&task).unwrap().is_terminal());
    }

    #[cfg(unix)]
    #[test]
    fn read_task_retries_injected_errors_up_to_attempt_limit() {
        let storage = tempfile::tempdir().unwrap();
        let (task, metadata) = counted_task(storage.path());
        let expected_task_id = metadata.task_id.clone();

        inject_read_task_replacement_failures_for_test(3);
        let read = read_task(&task.paths.json).unwrap();
        assert_eq!(read.task_id, expected_task_id);
        assert_eq!(injected_read_task_replacement_failures_for_test(), 0);

        inject_read_task_replacement_failures_for_test(METADATA_OPEN_ATTEMPTS);
        let error = read_task(&task.paths.json).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::Interrupted);
        assert_eq!(error.to_string(), ARTIFACT_CONCURRENTLY_REPLACED);
        assert_eq!(injected_read_task_replacement_failures_for_test(), 0);
    }

    #[cfg(unix)]
    fn with_replacements<T>(
        path: &Path,
        validation: u32,
        replacements: u32,
        content: Vec<u8>,
        read: impl FnOnce() -> io::Result<T>,
    ) -> (io::Result<T>, u32) {
        struct Restore;
        impl Drop for Restore {
            fn drop(&mut self) {
                BEFORE_ARTIFACT_VALIDATION.with(|slot| slot.replace(None));
            }
        }
        let path = path.to_path_buf();
        let replaced = std::rc::Rc::new(std::cell::Cell::new(0));
        let count = replaced.clone();
        let mut seen = 0;
        BEFORE_ARTIFACT_VALIDATION.with(|slot| {
            assert!(slot.borrow().is_none());
            slot.replace(Some(Box::new(move |file| {
                let opened = file.metadata().unwrap();
                let current = std::fs::metadata(&path).unwrap();
                if (opened.dev(), opened.ino()) != (current.dev(), current.ino()) {
                    return;
                }
                seen += 1;
                if seen == validation && count.get() < replacements {
                    let parent = PinnedDir::open(path.parent().unwrap()).unwrap();
                    randomized_atomic_replace(&parent, path.file_name().unwrap(), &content)
                        .unwrap();
                    count.set(count.get() + 1);
                    seen = 0;
                }
            })));
        });
        let _restore = Restore;
        (read(), replaced.get())
    }

    #[cfg(unix)]
    #[test]
    fn read_task_reopens_atomically_replaced_metadata() {
        let storage = tempfile::tempdir().unwrap();
        let (task, mut metadata) = counted_task(storage.path());
        metadata.command = "replacement metadata".to_string();
        let (read, replaced) = with_replacements(
            &task.paths.json,
            1,
            1,
            serde_json::to_vec(&metadata).unwrap(),
            || read_task(&task.paths.json),
        );
        assert_eq!(
            replaced, 1,
            "the rename must land after open, before validation"
        );
        assert_eq!(read.unwrap().command, "replacement metadata");
    }

    #[cfg(unix)]
    #[test]
    fn artifact_reads_reopen_after_atomic_replacement() {
        let storage = tempfile::tempdir().unwrap();
        let (task, _) = counted_task(storage.path());
        TaskIoHandles::create(&task, BgMode::Pipes, false).unwrap();
        // The first two checks validate the open; the third samples the length.
        for validation in 1..=3 {
            randomized_atomic_replace(
                &task.dirs.io,
                &task.paths.artifact_name(TaskArtifact::Stdout),
                b"original output",
            )
            .unwrap();
            let (read, replaced) = with_replacements(
                &task.paths.stdout,
                validation,
                1,
                b"replacement output".to_vec(),
                || {
                    let (mut file, len) = sample_task_artifact(&task.paths, TaskArtifact::Stdout)?;
                    file.read_range(0, len)
                },
            );
            assert_eq!(replaced, 1, "validation {validation} must observe a rename");
            assert_eq!(read.unwrap(), b"replacement output");
        }
        for validation in 1..=2 {
            randomized_atomic_replace(
                &task.dirs.io,
                &task.paths.artifact_name(TaskArtifact::Stdout),
                b"original output",
            )
            .unwrap();
            let (read, replaced) = with_replacements(
                &task.paths.stdout,
                validation,
                1,
                b"reopened output".to_vec(),
                || open_task_artifact(&task.paths, TaskArtifact::Stdout)?.read_all(),
            );
            assert_eq!(replaced, 1);
            assert_eq!(read.unwrap(), b"reopened output");
        }
    }

    #[cfg(unix)]
    #[test]
    fn buffer_reads_reopen_after_atomic_replacement() {
        use super::super::buffer::{BgBuffer, StreamKind};

        let storage = tempfile::tempdir().unwrap();
        let (task, _) = counted_task(storage.path());
        TaskIoHandles::create(&task, BgMode::Pipes, false).unwrap();
        let buffer = BgBuffer::registered(&task.paths, BgMode::Pipes);
        for validation in 1..=3 {
            for operation in ["bounded", "tail", "empty_tail", "len", "notice"] {
                randomized_atomic_replace(
                    &task.dirs.io,
                    &task.paths.artifact_name(TaskArtifact::Stdout),
                    b"original output",
                )
                .unwrap();
                let (read, replaced) = with_replacements(
                    &task.paths.stdout,
                    validation,
                    1,
                    b"replacement output".to_vec(),
                    || {
                        let text = match operation {
                            "bounded" => buffer.read_stream_bounded(StreamKind::Stdout, 64).text,
                            "tail" => buffer.read_tail(64).0,
                            "empty_tail" => {
                                let (text, truncated) = buffer.read_tail(0);
                                assert!(text.is_empty());
                                assert!(truncated);
                                return Ok(());
                            }
                            "len" => {
                                assert_eq!(buffer.stream_len(StreamKind::Stdout), 18);
                                return Ok(());
                            }
                            "notice" => buffer.read_complete_for_notice().unwrap().0,
                            _ => unreachable!(),
                        };
                        assert_eq!(text, "replacement output", "{operation}");
                        Ok(())
                    },
                );
                assert_eq!(replaced, 1, "{operation} validation {validation}");
                read.unwrap();
            }
        }
    }

    #[cfg(unix)]
    #[test]
    fn artifact_read_replacement_retries_are_bounded() {
        let storage = tempfile::tempdir().unwrap();
        let (task, metadata) = counted_task(storage.path());
        TaskIoHandles::create(&task, BgMode::Pipes, false).unwrap();
        for validation in 1..=3 {
            let (read, replaced) = with_replacements(
                &task.paths.stdout,
                validation,
                METADATA_OPEN_ATTEMPTS + 1,
                b"repeated replacement".to_vec(),
                || sample_task_artifact(&task.paths, TaskArtifact::Stdout),
            );
            let error = read.unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::Interrupted);
            assert_eq!(error.to_string(), ARTIFACT_CONCURRENTLY_REPLACED);
            assert_eq!(replaced, METADATA_OPEN_ATTEMPTS);
        }
        let (read, replaced) = with_replacements(
            &task.paths.json,
            1,
            METADATA_OPEN_ATTEMPTS + 1,
            serde_json::to_vec(&metadata).unwrap(),
            || read_task(&task.paths.json),
        );
        assert_eq!(read.unwrap_err().kind(), io::ErrorKind::Interrupted);
        assert_eq!(replaced, METADATA_OPEN_ATTEMPTS);
    }

    #[test]
    fn artifact_read_retry_preserves_other_errors() {
        for (kind, message) in [
            (io::ErrorKind::Interrupted, "unrelated interruption"),
            (
                io::ErrorKind::InvalidData,
                "task artifact has multiple hard links",
            ),
            (
                io::ErrorKind::PermissionDenied,
                ARTIFACT_CONCURRENTLY_REPLACED,
            ),
            (io::ErrorKind::NotFound, "task removed"),
        ] {
            let mut calls = 0;
            let read = retry_artifact_read::<()>(|| {
                calls += 1;
                Err(io::Error::new(kind, message))
            });
            assert_eq!(read.unwrap_err().kind(), kind);
            assert_eq!(calls, 1);
        }
    }

    /// Manual check: race real metadata readers against atomic replacement writes.
    #[cfg(unix)]
    #[test]
    #[ignore = "manual real-race stress check"]
    fn read_task_replacement_retry_handles_concurrent_metadata_writes() {
        let storage = tempfile::tempdir().unwrap();
        let (task, metadata) = counted_task(storage.path());
        let path = task.paths.json.clone();
        let writing = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
        let writer_writing = writing.clone();
        let writer_path = path.clone();
        let started = std::sync::Arc::new(std::sync::Barrier::new(2));
        let writer_started = started.clone();
        let writer = std::thread::spawn(move || {
            write_task(&writer_path, &metadata).unwrap();
            writer_started.wait();
            while writer_writing.load(std::sync::atomic::Ordering::Relaxed) {
                write_task(&writer_path, &metadata).unwrap();
                std::thread::yield_now();
            }
        });
        started.wait();

        let mut replacement_error = None;
        let mut other_error = None;
        let read_deadline = std::time::Instant::now() + Duration::from_secs(10);
        let mut plain_reads = 0;
        while replacement_error.is_none() && std::time::Instant::now() < read_deadline {
            plain_reads += 1;
            if let Err(error) = read_task_once(&path) {
                if error.kind() == io::ErrorKind::Interrupted
                    && error.to_string().contains(ARTIFACT_CONCURRENTLY_REPLACED)
                {
                    replacement_error.get_or_insert_with(|| error.to_string());
                } else {
                    other_error.get_or_insert_with(|| error.to_string());
                }
            }
        }

        let mut retry_error = None;
        for _ in 0..plain_reads.max(3_000) {
            if let Err(error) = read_task(&path) {
                retry_error.get_or_insert_with(|| error.to_string());
            }
        }

        writing.store(false, std::sync::atomic::Ordering::Relaxed);
        writer.join().unwrap();
        assert!(
            other_error.is_none(),
            "unexpected read error: {other_error:?}"
        );
        assert!(
            replacement_error.is_some(),
            "single-attempt read never observed a transient metadata replacement"
        );
        assert!(
            retry_error.is_none(),
            "retrying metadata read failed: {retry_error:?}"
        );
        eprintln!("single-attempt read observed {replacement_error:?}; read_task succeeded");
    }

    #[test]
    fn task_id_validation_is_exact() {
        assert!(validate_task_id("bash-0123456789abcdef").is_ok());
        for invalid in [
            "bash-0123456789abcde",
            "bash-0123456789abcdef0",
            "bash-0123456789ABCDEf",
            "bash-0123456789abcdeg",
            "../bash-0123456789abcdef",
        ] {
            assert!(validate_task_id(invalid).is_err(), "accepted {invalid}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn marker_clones_stay_close_on_exec_until_the_child_allowlist_remaps_them() {
        let temp = tempfile::tempdir().unwrap();
        let task = create_task_layout(temp.path(), "marker-clones", &valid_id(1)).unwrap();
        let handles = TaskIoHandles::create(&task, BgMode::Pipes, true).unwrap();
        let markers = [
            TaskArtifact::Exit,
            TaskArtifact::SandboxUnavailable,
            TaskArtifact::PipelineStatus,
        ]
        .map(|artifact| handles.inheritable_file(artifact).unwrap());
        for marker in &markers {
            let flags = unsafe { libc::fcntl(marker.as_raw_fd(), libc::F_GETFD) };
            assert!(
                flags >= 0 && flags & libc::FD_CLOEXEC != 0,
                "marker clone is inheritable in the parent"
            );
        }
        let output = std::process::Command::new("/bin/bash")
            .args(["-c", "if [ ! -e /dev/fd/1 ] || [ ! -e /dev/fd/2 ]; then echo 'descriptor lookup cannot see open stdio' >&2; exit 1; fi; for fd in \"$@\"; do if [ -e \"/dev/fd/$fd\" ]; then echo \"leaked marker $fd\" >&2; exit 1; fi; done", "marker-probe"])
            .args(markers.iter().map(|marker| marker.as_raw_fd().to_string()))
            .output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    fn new_layout_separates_control_and_io() {
        let storage = tempfile::tempdir().unwrap();
        let task = create_task_layout(storage.path(), "session", &valid_id(1)).unwrap();
        assert_eq!(
            task.paths.json.parent(),
            Some(task.paths.control_dir.as_path())
        );
        assert_eq!(
            task.paths.stdout.parent(),
            Some(task.paths.io_dir.as_path())
        );
        assert_ne!(task.paths.control_dir, task.paths.io_dir);
    }

    #[cfg(unix)]
    #[test]
    fn task_layout_directories_are_private() {
        use std::os::unix::fs::PermissionsExt;

        let storage = tempfile::tempdir().unwrap();
        let task = create_task_layout(storage.path(), "session", &valid_id(5)).unwrap();
        for path in [
            &task.paths.session_dir,
            &task.paths.dir,
            &task.paths.control_dir,
            &task.paths.io_dir,
        ] {
            let mode = fs::metadata(path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o700, "unexpected permissions for {}", path.display());
        }
        let bash_tasks = task.paths.session_dir.parent().unwrap();
        let mode = fs::metadata(bash_tasks).unwrap().permissions().mode() & 0o777;
        assert_eq!(
            mode,
            0o700,
            "unexpected permissions for {}",
            bash_tasks.display()
        );
    }

    #[test]
    fn resolver_refuses_duplicate_layouts() {
        let storage = tempfile::tempdir().unwrap();
        let task = create_task_layout(storage.path(), "session", &valid_id(2)).unwrap();
        let flat = task
            .paths
            .session_dir
            .join(format!("{}.json", task.paths.task_id));
        fs::write(flat, b"{}").unwrap();
        let error = resolve_task_layout(&task.paths.session_dir, &task.paths.task_id).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn resolver_rejects_metadata_identity_mismatch() {
        let storage = tempfile::tempdir().unwrap();
        let task = create_task_layout(storage.path(), "session", &valid_id(30)).unwrap();
        let metadata = PersistedTask::starting(
            valid_id(31),
            "session".into(),
            "true".into(),
            storage.path().into(),
            None,
            None,
            true,
            false,
        );
        fs::write(&task.paths.json, serde_json::to_vec(&metadata).unwrap()).unwrap();
        let error = resolve_task_layout(&task.paths.session_dir, &task.paths.task_id).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    }

    // A task directory swapped underneath the daemon must never let deletion
    // touch the impostor's control content. The two platforms enforce this the
    // same guarantee through different mechanisms, so each is asserted against
    // its real mechanism rather than a shared code path.
    //
    // Unix: POSIX permits renaming a directory while a fd is held open on it, so
    // the swap succeeds on disk and `remove_directory_task`'s `same_identity`
    // check is what refuses the deletion.
    #[cfg(unix)]
    #[test]
    fn deletion_refuses_replaced_task_directory_without_touching_victim() {
        let storage = tempfile::tempdir().unwrap();
        let first = create_task_layout(storage.path(), "session", &valid_id(40)).unwrap();
        let second = create_task_layout(storage.path(), "session", &valid_id(41)).unwrap();
        let victim = second.paths.control_dir.join("victim");
        fs::write(&victim, b"victim-bytes").unwrap();
        let moved_first = first.paths.session_dir.join("moved-first");
        fs::rename(&first.paths.dir, &moved_first).unwrap();
        fs::rename(&second.paths.dir, &first.paths.dir).unwrap();

        assert!(delete_resolved_task(&first).is_err());
        assert_eq!(
            fs::read(first.paths.control_dir.join("victim")).unwrap(),
            b"victim-bytes"
        );
    }

    // Windows: the daemon's retained `PinnedDir` handle on the task directory
    // makes the OS refuse to rename it (Access denied), so the swap cannot occur
    // at all while the daemon is live — the impostor's content is never reachable
    // for deletion. Assert that structural refusal directly.
    #[cfg(windows)]
    #[test]
    fn deletion_refuses_replaced_task_directory_without_touching_victim() {
        let storage = tempfile::tempdir().unwrap();
        let first = create_task_layout(storage.path(), "session", &valid_id(40)).unwrap();
        let second = create_task_layout(storage.path(), "session", &valid_id(41)).unwrap();
        let victim = second.paths.control_dir.join("victim");
        fs::write(&victim, b"victim-bytes").unwrap();

        // The daemon still holds `first`'s pinned directory handles, so moving
        // its task directory out of the way is refused by the OS.
        let moved_first = first.paths.session_dir.join("moved-first");
        let refusal = fs::rename(&first.paths.dir, &moved_first)
            .expect_err("open pinned-dir handle must block the task-dir rename on Windows");
        assert_eq!(refusal.kind(), io::ErrorKind::PermissionDenied);

        // The victim's control content is untouched because the swap never happened.
        assert_eq!(fs::read(&victim).unwrap(), b"victim-bytes");
    }

    #[cfg(windows)]
    #[test]
    fn directory_handle_validation_reports_not_a_directory_for_regular_files() {
        let storage = tempfile::tempdir().unwrap();
        let dir = PinnedDir::open(storage.path()).unwrap();
        fs::write(storage.path().join("output.bin"), b"output").unwrap();

        let error = dir
            .open_dir_at(OsStr::new("output.bin"))
            .err()
            .expect("a regular file must not become a pinned directory");
        assert_eq!(error.kind(), io::ErrorKind::NotADirectory);
        remove_tree_contents(&dir).unwrap();
        assert!(!storage.path().join("output.bin").exists());
    }

    #[cfg(windows)]
    #[test]
    fn windows_delete_handle_reopens_directory_without_following_renamed_path() {
        let storage = tempfile::tempdir().unwrap();
        let original = storage.path().join("original");
        let moved = storage.path().join("moved");
        fs::create_dir(&original).unwrap();
        let dir = PinnedDir::open(&original).unwrap();
        fs::rename(&original, &moved).unwrap();
        fs::create_dir(&original).unwrap();
        let victim = original.join("victim");
        fs::write(&victim, b"victim-bytes").unwrap();

        windows_delete_pinned_handle(&dir.file, true).unwrap();

        assert!(
            !moved.exists(),
            "the pinned directory must be unlinked immediately"
        );
        assert_eq!(fs::read(&victim).unwrap(), b"victim-bytes");
        // The original pin remains live even though its name has disappeared.
        assert!(dir.file.metadata().unwrap().is_dir());
    }

    #[cfg(windows)]
    #[test]
    fn windows_delete_handle_unlinks_plain_file_with_retained_reader() {
        let storage = tempfile::tempdir().unwrap();
        let path = storage.path().join("output");
        fs::write(&path, b"retained-output").unwrap();
        let mut reader = File::open(&path).unwrap();

        windows_delete_pinned_handle(&reader, false).unwrap();

        assert!(!path.exists());
        let mut output = Vec::new();
        reader.read_to_end(&mut output).unwrap();
        assert_eq!(output, b"retained-output");
    }

    #[cfg(windows)]
    #[test]
    fn windows_bundle_deletion_unlinks_names_while_pins_remain_open() {
        let storage = tempfile::tempdir().unwrap();
        let (task, _) = counted_task(storage.path());
        let handles = TaskIoHandles::create(&task, BgMode::Pipes, false).unwrap();
        fs::write(&task.paths.stdout, b"retained-output").unwrap();
        let mut stdout = handles.clone_file(TaskArtifact::Stdout).unwrap();
        let nested = task.dirs.io.create_dir_at(OsStr::new("nested")).unwrap();
        fs::write(nested.path().join("output"), b"nested-output").unwrap();

        delete_task_bundle(&task.paths).unwrap();

        assert!(!task.paths.dir.exists());
        assert!(!nested.path().exists());
        assert!(task.dirs.session.list_names().unwrap().is_empty());
        stdout.rewind().unwrap();
        let mut output = Vec::new();
        stdout.read_to_end(&mut output).unwrap();
        assert_eq!(output, b"retained-output");
        // Keep all original pins and IO handles live through the assertions.
        drop((stdout, nested, handles, task));
    }

    #[cfg(windows)]
    #[test]
    fn windows_bundle_deletion_removes_native_wrapper_at_task_root() {
        let storage = tempfile::tempdir().unwrap();
        let (task, _) = counted_task(storage.path());
        let wrapper = task.paths.dir.join("metadata.ps1");
        fs::write(&wrapper, b"exit 0").unwrap();

        delete_task_bundle(&task.paths).unwrap();

        assert!(!wrapper.exists());
        assert!(!task.paths.dir.exists());
        assert!(task.dirs.session.list_names().unwrap().is_empty());
    }

    #[cfg(windows)]
    #[test]
    fn windows_bundle_deletion_resumes_after_io_or_control_disappears() {
        for remove_control in [false, true] {
            let storage = tempfile::tempdir().unwrap();
            let (task, _) = counted_task(storage.path());
            task.dirs.io.remove_self().unwrap();
            if remove_control {
                remove_tree_contents(&task.dirs.control).unwrap();
                task.dirs.control.remove_self().unwrap();
            }

            delete_task_bundle(&task.paths).unwrap();

            assert!(!task.paths.dir.exists());
            assert!(task.dirs.session.list_names().unwrap().is_empty());
            // A completed legacy deletion is also safe to resume when its
            // final task entry vanished after the previous sweep's pins closed.
            delete_task_bundle(&task.paths).unwrap();
        }
    }

    #[cfg(windows)]
    #[test]
    fn directory_handle_validation_refuses_junctions() {
        let storage = tempfile::tempdir().unwrap();
        let io_path = storage.path().join("io");
        let target = storage.path().join("outside-task");
        fs::create_dir(&io_path).unwrap();
        fs::create_dir(&target).unwrap();
        let victim = target.join("victim");
        fs::write(&victim, b"victim-bytes").unwrap();
        let junction = io_path.join("junction");
        // Directory junctions do not require the symbolic-link privilege.
        let output = std::process::Command::new("cmd.exe")
            .args(["/D", "/C", "mklink", "/J"])
            .arg(&junction)
            .arg(&target)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "mklink /J failed: {} {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );

        let file = OpenOptions::new()
            .read(true)
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_BACKUP_SEMANTICS)
            .open(&junction)
            .unwrap();
        assert_ne!(
            file.metadata().unwrap().file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT,
            0,
            "the handle must refer to the junction itself, not its target"
        );
        let error = validate_directory_handle(&file).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert_eq!(error.to_string(), "task path is a reparse point");
        drop(file);

        let dir = PinnedDir::open(&io_path).unwrap();
        let error = remove_tree_contents(&dir).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert_eq!(error.to_string(), "task path is a reparse point");
        assert_eq!(fs::read(&victim).unwrap(), b"victim-bytes");
        fs::remove_dir(&junction).unwrap();
    }

    #[test]
    #[cfg(unix)]
    fn directory_cleanup_removes_short_sandbox_temp() {
        let storage = tempfile::tempdir().unwrap();
        let (task, mut metadata) = counted_task(storage.path());
        let temp = crate::sandbox_spawn::create_task_temp_dir(task.dirs.io.path()).unwrap();
        fs::write(temp.join("scratch"), "payload").unwrap();
        metadata.sandbox_temp_dir = Some(temp.clone());
        write_task_at(&task, &metadata).unwrap();
        assert_eq!(
            delete_resolved_task(&task).unwrap_err().kind(),
            io::ErrorKind::WouldBlock
        );
        assert!(
            temp.exists(),
            "an unfinished task must keep its scratch directory"
        );
        metadata.mark_terminal(BgTaskStatus::Completed, Some(0), None);
        write_task_at(&task, &metadata).unwrap();
        delete_resolved_task(&task).unwrap();
        assert!(!temp.exists());
        assert!(!temp.with_extension("owner").exists());
        crate::sandbox_spawn::cleanup_task_temp_dir(&temp).unwrap();
        crate::sandbox_spawn::cleanup_task_temp_dir(
            &task.dirs.io.path().join(".sandbox-tmp-already-removed"),
        )
        .unwrap();
    }

    #[test]
    fn directory_cleanup_preserves_metadata_when_io_removal_fails() {
        let storage = tempfile::tempdir().unwrap();
        let (task, metadata) = counted_task(storage.path());
        write_task_at(&task, &metadata).unwrap();
        fs::write(&task.paths.stdout, b"output held by a child").unwrap();

        let error = remove_directory_task_with_io_cleanup(&task, |io_dir| {
            io_dir.remove_file(OsStr::new(TaskArtifact::Stdout.file_name()))?;
            Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "injected failure after partial IO cleanup",
            ))
        })
        .expect_err("an injected IO cleanup failure must abort bundle deletion");

        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        assert!(
            task.dirs
                .control
                .list_names()
                .unwrap()
                .iter()
                .any(|name| name == OsStr::new(METADATA_FILE)),
            "task metadata must remain available so a later sweep can resolve and retry deletion"
        );
        assert_eq!(read_task_at(&task).unwrap().task_id, task.paths.task_id);
    }

    #[cfg(unix)]
    #[test]
    fn directory_cleanup_resumes_tombstone_after_io_removal_failure() {
        let storage = tempfile::tempdir().unwrap();
        let (task, metadata) = counted_task(storage.path());
        write_task_at(&task, &metadata).unwrap();
        fs::write(&task.paths.stdout, b"output held by a child").unwrap();
        let original = task.paths.dir.clone();
        let session = task.paths.session_dir.clone();

        let error = remove_directory_task_with_io_cleanup(&task, |_| {
            Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "injected failure after the task rename",
            ))
        })
        .expect_err("the injected IO cleanup failure must abort the first deletion");
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        assert!(
            !original.exists(),
            "the failed deletion has renamed the task"
        );

        delete_task_bundle_with_legacy_harness_root(&task.paths, "counted", None)
            .expect("a later cleanup pass must resume deletion from the renamed task");

        assert_eq!(fs::read_dir(session).unwrap().count(), 0);
    }

    #[cfg(unix)]
    #[test]
    fn task_tombstone_cleanup_refuses_symlinks() {
        use std::os::unix::fs::symlink;

        let storage = tempfile::tempdir().unwrap();
        let session_id = "session";
        let session = session_tasks_dir(storage.path(), session_id);
        fs::create_dir_all(&session).unwrap();
        let task_id = valid_id(43);
        let paths = TaskPaths::directory(session.clone(), &task_id);
        let victim = storage.path().join("victim");
        fs::create_dir_all(&victim).unwrap();
        fs::write(victim.join("keep"), b"untouched").unwrap();
        let tombstone = session.join(format!("{}test", task_tombstone_prefix(&task_id)));
        symlink(&victim, &tombstone).unwrap();

        assert!(delete_task_bundle_with_legacy_harness_root(&paths, session_id, None).is_err());
        assert_eq!(fs::read(victim.join("keep")).unwrap(), b"untouched");
        assert!(fs::symlink_metadata(&tombstone)
            .unwrap()
            .file_type()
            .is_symlink());
    }

    #[cfg(unix)]
    #[test]
    fn legacy_harness_root_fallback_refuses_a_task_symlink() {
        use std::os::unix::fs::symlink;

        let storage = tempfile::tempdir().unwrap();
        let session_id = "session";
        let task_id = valid_id(33);
        let paths = task_paths(storage.path(), session_id, &task_id).unwrap();
        fs::create_dir_all(&paths.session_dir).unwrap();
        let legacy_session = storage
            .path()
            .join("pi")
            .join("bash-tasks")
            .join(hash_session(session_id));
        fs::create_dir_all(&legacy_session).unwrap();
        let victim = storage.path().join("victim");
        fs::create_dir_all(&victim).unwrap();
        fs::write(victim.join("keep"), b"untouched").unwrap();
        let task_link = legacy_session.join(&task_id);
        symlink(&victim, &task_link).unwrap();

        assert!(
            delete_task_bundle_with_legacy_harness_root(&paths, session_id, Some("pi")).is_err()
        );
        assert_eq!(fs::read(victim.join("keep")).unwrap(), b"untouched");
        assert!(fs::symlink_metadata(&task_link)
            .unwrap()
            .file_type()
            .is_symlink());
    }

    #[test]
    fn legacy_flat_layout_is_readable_and_deleted_as_a_bundle() {
        let storage = tempfile::tempdir().unwrap();
        let task_id = valid_id(32);
        let paths = task_paths(storage.path(), "session", &task_id).unwrap();
        fs::create_dir_all(&paths.session_dir).unwrap();
        let metadata = PersistedTask::starting(
            task_id.clone(),
            "session".into(),
            "true".into(),
            storage.path().into(),
            None,
            None,
            true,
            false,
        );
        write_task(&paths.json, &metadata).unwrap();
        fs::write(&paths.stdout, b"legacy").unwrap();
        fs::write(&paths.stderr, b"").unwrap();
        assert_eq!(
            resolve_task_layout(&paths.session_dir, &task_id)
                .unwrap()
                .paths
                .layout,
            TaskLayout::Flat
        );
        assert_eq!(
            open_task_artifact(&paths, TaskArtifact::Stdout)
                .unwrap()
                .read_all()
                .unwrap(),
            b"legacy"
        );
        delete_task_bundle(&paths).unwrap();
        assert!(!paths.json.exists());
        assert!(!paths.stdout.exists());
    }

    #[cfg(unix)]
    #[test]
    fn live_output_creation_and_later_writes_refuse_link_attacks() {
        use std::os::unix::fs::symlink;

        let storage = tempfile::tempdir().unwrap();
        let task = create_task_layout(storage.path(), "session", &valid_id(33)).unwrap();
        let metadata = PersistedTask::starting(
            task.paths.task_id.clone(),
            "session".into(),
            "true".into(),
            storage.path().into(),
            None,
            None,
            true,
            false,
        );
        write_task_at(&task, &metadata).unwrap();
        let victim = storage.path().join("victim");
        fs::write(&victim, b"victim-bytes").unwrap();

        symlink(&victim, &task.paths.stdout).unwrap();
        assert!(TaskIoHandles::create(&task, BgMode::Pipes, false).is_err());
        assert_eq!(fs::read(&victim).unwrap(), b"victim-bytes");
        fs::remove_file(&task.paths.stdout).unwrap();

        let mut handles = TaskIoHandles::create(&task, BgMode::Pipes, false).unwrap();
        fs::hard_link(&task.paths.stdout, task.paths.io_dir.join("linked-stdout")).unwrap();
        assert!(handles
            .write(TaskArtifact::Stdout, b"daemon-write")
            .is_err());
        assert_eq!(fs::read(&victim).unwrap(), b"victim-bytes");

        fs::remove_file(&task.paths.stdout).unwrap();
        symlink(&victim, &task.paths.stdout).unwrap();
        assert!(replace_artifact_with_tail(&task.paths, TaskArtifact::Stdout, 1).is_err());
        assert_eq!(fs::read(&victim).unwrap(), b"victim-bytes");
    }

    #[test]
    fn registered_artifact_consumers_do_not_reopen_paths_directly() {
        let rust_sources = [
            include_str!("buffer.rs"),
            include_str!("registry.rs"),
            include_str!("process.rs"),
            include_str!("pty_process.rs"),
            include_str!("watches.rs"),
            include_str!("watchdog.rs"),
            include_str!("../commands/bash_status.rs"),
        ];
        for source in rust_sources {
            // A Windows checkout can have CRLF line endings. Without this the
            // split marker never matches there, so the guard would scan test
            // modules on Windows but not elsewhere. Scan the same production
            // text on every platform.
            let source = source.replace("\r\n", "\n");
            let production = source
                .split("#[cfg(test)]\nmod tests")
                .next()
                .unwrap_or(&source);
            for forbidden in [
                "File::open(&task.paths",
                "fs::read(&task.paths",
                "fs::read_to_string(&task.paths",
                "File::open(path)?",
            ] {
                assert!(
                    !production.contains(forbidden),
                    "registered artifact consumer contains raw path read: {forbidden}"
                );
            }
        }
        for source in [
            include_str!("../../../../packages/opencode-plugin/src/tools/bash.ts"),
            include_str!("../../../../packages/opencode-plugin/src/tools/bash_watch.ts"),
            include_str!("../../../../packages/pi-plugin/src/tools/bash.ts"),
        ] {
            for forbidden in [
                "fs.readFile(outputPath)",
                "fs.readFile(details.output_path)",
                "fs.open(outputPath",
            ] {
                assert!(
                    !source.contains(forbidden),
                    "plugin artifact consumer contains raw path read: {forbidden}"
                );
            }
        }
    }

    #[cfg(unix)]
    #[test]
    fn validated_artifact_refuses_symlink_hardlink_and_fifo() {
        use std::os::unix::fs::symlink;

        let storage = tempfile::tempdir().unwrap();
        let task = create_task_layout(storage.path(), "session", &valid_id(3)).unwrap();
        let metadata = PersistedTask::starting(
            task.paths.task_id.clone(),
            "session".into(),
            "true".into(),
            storage.path().into(),
            None,
            None,
            true,
            false,
        );
        write_task_at(&task, &metadata).unwrap();
        let canary = storage.path().join("canary");
        fs::write(&canary, b"secret").unwrap();

        symlink(&canary, &task.paths.stdout).unwrap();
        assert!(open_task_artifact(&task.paths, TaskArtifact::Stdout).is_err());
        fs::remove_file(&task.paths.stdout).unwrap();

        fs::hard_link(&canary, &task.paths.stdout).unwrap();
        assert!(open_task_artifact(&task.paths, TaskArtifact::Stdout).is_err());
        fs::remove_file(&task.paths.stdout).unwrap();

        let path = CString::new(task.paths.stdout.as_os_str().as_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(path.as_ptr(), 0o600) }, 0);
        assert!(open_task_artifact(&task.paths, TaskArtifact::Stdout).is_err());
    }
}

#[cfg(test)]
mod readable_name_tests {
    use super::*;

    #[test]
    fn shell_read_names_match_file_tool_readable_files() {
        for paths in [
            TaskPaths::directory(PathBuf::from("/session"), "bash-1111111111111111"),
            TaskPaths::flat(PathBuf::from("/session"), "bash-1111111111111111"),
        ] {
            let actual = paths
                .readable_files()
                .collect::<std::collections::BTreeSet<_>>();
            let expected = [
                &paths.json,
                &paths.command,
                &paths.wrapper,
                &paths.manifest,
                &paths.sandbox_profile,
            ]
            .into_iter()
            .cloned()
            .chain(
                TaskArtifact::ALL
                    .into_iter()
                    .map(|artifact| paths.artifact_path(artifact).to_path_buf()),
            )
            .collect();
            assert_eq!(actual, expected);
            assert!(!actual.contains(&paths.environment));
        }
    }
}
