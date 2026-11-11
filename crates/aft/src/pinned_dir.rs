#[cfg(unix)]
use std::ffi::CString;
use std::ffi::{OsStr, OsString};
use std::fs::{self, File, OpenOptions};
#[cfg(all(test, windows))]
use std::io::Read;
use std::io::{self, Write};
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
use std::time::{Duration, SystemTime};

#[derive(Debug)]
pub struct PinnedDir {
    pub(crate) file: File,
    pub(crate) path: PathBuf,
    #[cfg(test)]
    pub(crate) cache_device_override: Option<u64>,
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
        let mut stack = vec![Self::open_with_access(Path::new("/"), true)?];
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
            match current.repair_child_with_access(&name, true) {
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
        #[allow(unused_mut)]
        let mut directory = stack.pop().expect("walk keeps the root");
        #[cfg(target_os = "linux")]
        {
            // Only ancestors use O_PATH. Upgrade the selected directory via
            // its descriptor, not its pathname, so reads/fsync keep working
            // without racing a replacement of any checked ancestor.
            directory.file = openat_file(
                directory.file.as_raw_fd(),
                &directory.path,
                OsStr::new("."),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                0,
            )?;
            validate_directory_handle(&directory.file)?;
        }
        Ok(directory)
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
        Self::open_with_access(path, false)
    }

    fn open_with_access(path: &Path, _traversal_only: bool) -> io::Result<Self> {
        #[cfg(test)]
        work_counts::record_open();
        #[cfg(unix)]
        let file = OpenOptions::new()
            .read(true)
            .custom_flags(if _traversal_only {
                crate::private_storage::ancestor_dir_flags()
            } else {
                libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC
            })
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
            &self.path,
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
        self.repair_child_with_access(name, false)
    }

    fn repair_child_with_access(&self, name: &OsStr, _traversal_only: bool) -> io::Result<Self> {
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
            self.open_dir_at_with_flags(
                name,
                if _traversal_only {
                    crate::private_storage::ancestor_dir_flags()
                } else {
                    libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC
                },
            )
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
            regular_files_only: false,
        }
        .check(self)
    }

    pub(crate) fn repair_coordination_file(&self, name: &OsStr) -> io::Result<File> {
        #[cfg(unix)]
        {
            let file = openat_file(
                self.file.as_raw_fd(),
                &self.path,
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

    pub(crate) fn repair_remove_child_with_warnings(
        &self,
        name: &OsStr,
        expected: (u64, u64),
        protected: &[(u64, u64)],
        removed: &mut usize,
        warnings: &mut Vec<String>,
    ) -> io::Result<()> {
        self.repair_remove_child_with_budget(
            name,
            expected,
            protected,
            removed,
            warnings,
            &mut RepairBudget::new(),
        )
    }

    pub(crate) fn repair_remove_child_with_budget(
        &self,
        name: &OsStr,
        expected: (u64, u64),
        protected: &[(u64, u64)],
        removed: &mut usize,
        warnings: &mut Vec<String>,
        budget: &mut RepairBudget,
    ) -> io::Result<()> {
        self.repair_remove_child_with_policy(
            name, expected, protected, false, removed, warnings, budget,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn repair_remove_child_with_policy(
        &self,
        name: &OsStr,
        expected: (u64, u64),
        protected: &[(u64, u64)],
        regular_files_only: bool,
        removed: &mut usize,
        warnings: &mut Vec<String>,
        budget: &mut RepairBudget,
    ) -> io::Result<()> {
        budget.enter(0)?;
        let child = self.repair_child(name)?;
        if child.directory_identity()? != expected {
            return Err(io::Error::other("repair child identity changed"));
        }
        let policy = RepairPolicy {
            device: self.directory_identity()?.0,
            mount: self.repair_mount_identity()?,
            protected,
            regular_files_only,
        };
        policy.check(&child)?;
        repair_remove_contents(&child, &policy, removed, budget, 0, warnings)?;
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

    /// Read-only identity census. Every durable descendant is needed because a
    /// cache being removed may contain an alias of a nested durable directory.
    pub(crate) fn directory_identities(
        &self,
        output: &mut Vec<(u64, u64)>,
        budget: &mut RepairBudget,
        examined: &mut IdentityCensus,
    ) -> io::Result<()> {
        self.collect_directory_identities(
            output,
            budget,
            examined,
            &mut std::collections::HashSet::new(),
            0,
        )
    }

    fn collect_directory_identities(
        &self,
        output: &mut Vec<(u64, u64)>,
        budget: &mut RepairBudget,
        examined: &mut IdentityCensus,
        seen: &mut std::collections::HashSet<(u64, u64)>,
        depth: usize,
    ) -> io::Result<()> {
        budget.enter(depth)?;
        let id = self.directory_identity()?;
        if !seen.insert(id) {
            return Ok(());
        }
        output.push(id);
        examined.directories += 1;
        let mount = self.repair_mount_identity()?;
        #[cfg(unix)]
        let mut entries = self.unix_entries()?;
        #[cfg(windows)]
        let mut entries = self
            .repair_names(budget)?
            .into_iter()
            .map(|name| Ok::<_, io::Error>((name, true)));
        loop {
            // Check before advancing the iterator, not after collecting a whole
            // directory. Also check while processing leaves, not just recursion.
            budget.enter(depth)?;
            #[cfg(unix)]
            budget.entry()?;
            let Some(entry) = entries.next() else { break };
            let (name, may_be_directory) = entry?;
            examined.entries += 1;
            #[cfg(test)]
            REPAIR_CENSUS_AFTER_NAMES.with(|hook| {
                if let Some(hook) = hook.borrow_mut().take() {
                    hook(self.path());
                }
            });
            // dirent types avoid opening every task artifact as a directory.
            // Unknown types must still be probed, and opens remain no-follow.
            if !may_be_directory {
                continue;
            }
            examined.directory_probes += 1;
            match self.repair_child(&name) {
                Ok(child) => {
                    let child_id = child.directory_identity()?;
                    if child_id.0 == id.0 && child.repair_mount_identity()? == mount {
                        child.collect_directory_identities(
                            output,
                            budget,
                            examined,
                            seen,
                            depth + 1,
                        )?;
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
            regular_files_only: false,
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

    /// The pinned directory's open handle, for `*at` calls relative to it.
    #[cfg(unix)]
    pub(crate) fn as_raw_fd(&self) -> std::os::fd::RawFd {
        self.file.as_raw_fd()
    }

    pub(crate) fn modified(&self) -> io::Result<SystemTime> {
        #[cfg(test)]
        work_counts::record_stat();
        self.file.metadata()?.modified()
    }

    pub(crate) fn same_identity(&self, other: &Self) -> io::Result<bool> {
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
    pub(crate) fn open_dir_at(&self, name: &OsStr) -> io::Result<Self> {
        self.open_dir_at_with_flags(
            name,
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    }

    #[cfg(unix)]
    fn open_dir_at_with_flags(&self, name: &OsStr, flags: libc::c_int) -> io::Result<Self> {
        #[cfg(test)]
        work_counts::record_open();
        let file = openat_file(self.file.as_raw_fd(), &self.path, name, flags, 0)?;
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
    pub(crate) fn open_dir_at(&self, name: &OsStr) -> io::Result<Self> {
        self.ensure_current_identity()?;
        let child = Self::open(&self.path.join(name))?;
        self.ensure_current_identity()?;
        Ok(child)
    }

    #[cfg(unix)]
    pub(crate) fn create_dir_at(&self, name: &OsStr) -> io::Result<Self> {
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
    pub(crate) fn create_dir_at(&self, name: &OsStr) -> io::Result<Self> {
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
            &self.path,
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
            &self.path,
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
        let mut entries = self.unix_entries()?;
        let mut names = Vec::new();
        loop {
            admit()?;
            let Some(entry) = entries.next() else { break };
            names.push(entry?.0);
        }
        Ok(names)
    }

    #[cfg(unix)]
    fn unix_entries(&self) -> io::Result<DirectoryEntries> {
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
        Ok(DirectoryEntries {
            directory,
            #[cfg(test)]
            emitted: 0,
        })
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

    pub(crate) fn rename(&self, from: &OsStr, to: &OsStr) -> io::Result<()> {
        #[cfg(test)]
        work_counts::record_rename();
        self.rename_to(from, self, to)
    }

    pub(crate) fn rename_to(&self, from: &OsStr, target: &PinnedDir, to: &OsStr) -> io::Result<()> {
        #[cfg(unix)]
        {
            let from = os_cstring(from)?;
            let to = os_cstring(to)?;
            #[cfg(test)]
            let started = std::time::Instant::now();
            let result = unsafe {
                libc::renameat(
                    self.file.as_raw_fd(),
                    from.as_ptr(),
                    target.file.as_raw_fd(),
                    to.as_ptr(),
                )
            };
            #[cfg(test)]
            work_counts::record_rename_time(started.elapsed());
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

    pub(crate) fn remove_file(&self, name: &OsStr) -> io::Result<()> {
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
    pub(crate) fn remove_self(&self) -> io::Result<()> {
        self.ensure_current_identity()?;
        windows_delete_pinned_handle(&self.file, true)
    }

    /// Fail with [`NonRegularEntryRetained`] unless `name` is a regular file,
    /// checked without following a symlink.
    #[cfg(unix)]
    fn repair_require_regular_file(&self, name: &OsStr) -> io::Result<()> {
        let c_name = os_cstring(name)?;
        let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
        if unsafe {
            libc::fstatat(
                self.file.as_raw_fd(),
                c_name.as_ptr(),
                stat.as_mut_ptr(),
                libc::AT_SYMLINK_NOFOLLOW,
            )
        } != 0
        {
            return Err(io::Error::last_os_error());
        }
        if unsafe { stat.assume_init() }.st_mode & libc::S_IFMT != libc::S_IFREG {
            return Err(io::Error::other(NonRegularEntryRetained(
                self.path.join(name),
            )));
        }
        Ok(())
    }

    /// Give the owner read, write and search permission on this opened
    /// directory so its entries can be listed and unlinked. Only a directory
    /// owned by the current user is changed; for any other the later unlink
    /// reports the permission error.
    #[cfg(unix)]
    fn restore_owner_access(&self) -> io::Result<()> {
        let metadata = self.file.metadata()?;
        let mode = metadata.mode() & 0o7777;
        if mode & 0o700 == 0o700 || metadata.uid() != unsafe { libc::geteuid() } {
            return Ok(());
        }
        // fchmod changes the inode this handle pinned, never a path.
        if unsafe { libc::fchmod(self.file.as_raw_fd(), (mode | 0o700) as libc::mode_t) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    /// Restore owner access to the child directory `name`, which could not be
    /// opened because its owner lacks read or search permission. Returns the
    /// identity of the directory that was changed. Returns `denied` unchanged
    /// when the child belongs to another user or this platform has no way to
    /// change it without following a symlink, and a `NotADirectory` error when
    /// the entry is not a directory, so the caller unlinks it as a leaf.
    #[cfg(unix)]
    fn restore_child_directory_access(
        &self,
        name: &OsStr,
        denied: io::Error,
    ) -> io::Result<(u64, u64)> {
        let c_name = os_cstring(name)?;
        let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
        if unsafe {
            libc::fstatat(
                self.file.as_raw_fd(),
                c_name.as_ptr(),
                stat.as_mut_ptr(),
                libc::AT_SYMLINK_NOFOLLOW,
            )
        } != 0
        {
            return Err(io::Error::last_os_error());
        }
        let stat = unsafe { stat.assume_init() };
        if stat.st_mode & libc::S_IFMT != libc::S_IFDIR {
            return Err(io::Error::new(
                io::ErrorKind::NotADirectory,
                "entry is not a directory",
            ));
        }
        if stat.st_uid != unsafe { libc::geteuid() } {
            return Err(denied);
        }
        // A mount point's root can be locked like any directory; never change
        // permissions on another filesystem (see `open_dir_for_removal`).
        if removal_device(&self.path.join(name), stat.st_dev as u64)
            != removal_device(&self.path, self.directory_identity()?.0)
        {
            return Err(io::Error::other(ForeignFilesystemRetained(
                self.path.join(name),
            )));
        }
        let identity = (stat.st_dev as u64, stat.st_ino as u64);
        let mode = (stat.st_mode as u32 & 0o7777) | 0o700;
        #[cfg(target_os = "macos")]
        {
            // With AT_SYMLINK_NOFOLLOW Darwin changes a symlink itself, so an
            // entry swapped for a link after the stat above still cannot
            // redirect the change outside the tree.
            if unsafe {
                libc::fchmodat(
                    self.file.as_raw_fd(),
                    c_name.as_ptr(),
                    mode as libc::mode_t,
                    libc::AT_SYMLINK_NOFOLLOW,
                )
            } != 0
            {
                return Err(io::Error::last_os_error());
            }
            Ok(identity)
        }
        #[cfg(any(target_os = "linux", target_os = "android"))]
        {
            // Linux's fchmodat cannot refuse symlinks, and fchmod does not
            // accept the O_PATH handle that opening a directory without read
            // permission requires. Pin the directory with O_PATH and
            // O_NOFOLLOW, check it is the one stat'ed above, then change it
            // through /proc/self/fd, which names exactly that pinned inode.
            let fd = unsafe {
                libc::openat(
                    self.file.as_raw_fd(),
                    c_name.as_ptr(),
                    libc::O_PATH | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                )
            };
            if fd < 0 {
                return Err(io::Error::last_os_error());
            }
            let pinned = unsafe { File::from_raw_fd(fd) };
            let metadata = pinned.metadata()?;
            if (metadata.dev(), metadata.ino()) != identity {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "directory changed while its permissions were restored for removal",
                ));
            }
            let proc_path = CString::new(format!("/proc/self/fd/{fd}"))?;
            if unsafe { libc::chmod(proc_path.as_ptr(), mode as libc::mode_t) } != 0 {
                // Without /proc there is no symlink-safe way to change it.
                return Err(denied);
            }
            drop(pinned);
            Ok(identity)
        }
        #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "android")))]
        {
            let _ = (identity, mode);
            Err(denied)
        }
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
        // Call statx through syscall(2), not glibc's wrapper: the wrapper only
        // exists from glibc 2.28, and the Linux release binary is linked
        // against an older glibc so it runs on older distributions. A kernel
        // without statx fails with ENOSYS, which refuses the repair like any
        // other unverifiable mount boundary.
        if unsafe {
            libc::syscall(
                libc::SYS_statx,
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
#[cfg(all(unix, test))]
type OpenatFileHook = Box<dyn FnMut(RawFd, &OsStr, i32) -> Option<io::Result<File>>>;
#[cfg(all(unix, test))]
thread_local! { static OPENAT_FILE_HOOK: std::cell::RefCell<Option<OpenatFileHook>> = const { std::cell::RefCell::new(None) }; }

#[cfg(all(unix, test))]
pub(crate) fn with_openat_file_hook<T>(hook: OpenatFileHook, action: impl FnOnce() -> T) -> T {
    struct Reset(Option<OpenatFileHook>);
    impl Drop for Reset {
        fn drop(&mut self) {
            OPENAT_FILE_HOOK.with(|slot| *slot.borrow_mut() = self.0.take());
        }
    }
    let _reset = Reset(OPENAT_FILE_HOOK.with(|slot| slot.borrow_mut().replace(hook)));
    action()
}

/// Fail closed instead of allocating, recursing or holding repair locks without
/// a bound. A later attempt may finish after the oversized tree is inspected.
pub(crate) struct RepairBudget {
    remaining: usize,
    deadline: std::time::Instant,
    cancelled: Option<std::sync::Arc<dyn Fn() -> bool + Send + Sync>>,
}

#[derive(Clone, Debug, Default, serde::Serialize)]
pub(crate) struct IdentityCensus {
    pub entries: usize,
    pub directories: usize,
    pub directory_probes: usize,
}

/// Owns a fresh stream opened relative to a pinned directory, so traversal does
/// not depend on a pathname staying attached to the same directory.
#[cfg(unix)]
struct DirectoryEntries {
    directory: *mut libc::DIR,
    #[cfg(test)]
    emitted: usize,
}

#[cfg(unix)]
impl Drop for DirectoryEntries {
    fn drop(&mut self) {
        unsafe { libc::closedir(self.directory) };
    }
}

#[cfg(unix)]
impl Iterator for DirectoryEntries {
    type Item = io::Result<(OsString, bool)>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            let errno = repair_errno_location();
            if errno.is_null() {
                return Some(Err(io::Error::other(
                    "directory enumeration errno is unavailable",
                )));
            }
            unsafe { *errno = 0 };
            #[cfg(test)]
            let fail = REPAIR_READDIR_FAIL_AFTER
                .with(|limit| limit.get().is_some_and(|limit| self.emitted >= limit));
            #[cfg(not(test))]
            let fail = false;
            let entry = if fail {
                unsafe { *errno = libc::EIO };
                std::ptr::null_mut()
            } else {
                unsafe { libc::readdir(self.directory) }
            };
            if entry.is_null() {
                let error = unsafe { *errno };
                return (error != 0).then(|| Err(io::Error::from_raw_os_error(error)));
            }
            let bytes = unsafe { std::ffi::CStr::from_ptr((*entry).d_name.as_ptr()).to_bytes() };
            if bytes == b"." || bytes == b".." {
                continue;
            }
            #[cfg(test)]
            {
                self.emitted += 1;
            }
            let kind = unsafe { (*entry).d_type };
            return Some(Ok((
                OsString::from_vec(bytes.to_vec()),
                kind == libc::DT_DIR || kind == libc::DT_UNKNOWN,
            )));
        }
    }
}

impl RepairBudget {
    pub(crate) fn new() -> Self {
        Self {
            remaining: 1_000_000,
            deadline: std::time::Instant::now() + Duration::from_secs(60),
            cancelled: None,
        }
    }
    pub(crate) fn enter(&self, depth: usize) -> io::Result<()> {
        if self.cancelled.as_ref().is_some_and(|cancelled| cancelled()) {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "repair cancelled",
            ));
        }
        if depth > 64 {
            // Not a budget stop: a deeper tree is just as deep next pass, so
            // reporting it as progress to continue would retry it forever.
            return Err(io::Error::other(
                "repair traversal limit exceeded: more than 64 nested directories",
            ));
        }
        if std::time::Instant::now() >= self.deadline {
            return Err(io::Error::other(RepairLimitExceeded));
        }
        Ok(())
    }
    pub(crate) fn entry(&mut self) -> io::Result<()> {
        self.enter(0)?;
        self.remaining = self
            .remaining
            .checked_sub(1)
            .ok_or_else(|| io::Error::other(RepairLimitExceeded))?;
        Ok(())
    }
}

/// A bounded repair walk ran out of entries, depth or time. The walk stopped
/// cleanly; anything it already removed stays removed.
#[derive(Debug)]
pub(crate) struct RepairLimitExceeded;

impl std::fmt::Display for RepairLimitExceeded {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("repair traversal limit exceeded")
    }
}

impl std::error::Error for RepairLimitExceeded {}

/// A tree removed without validating it first met an entry that is neither a
/// directory nor a regular file (a symlink, socket or device node). It is
/// kept, untouched, together with everything not yet removed.
#[derive(Debug)]
// Only Unix walks construct it; Windows stops at the same entries through
// its validated delete handles.
#[cfg_attr(windows, allow(dead_code))]
pub(crate) struct NonRegularEntryRetained(pub(crate) PathBuf);

impl std::fmt::Display for NonRegularEntryRetained {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "non-regular entry retained: {}", self.0.display())
    }
}

impl std::error::Error for NonRegularEntryRetained {}

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
    /// Stop at a symlink or special file instead of unlinking it. Used when a
    /// tree is removed without a complete validation walk first.
    regular_files_only: bool,
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
    let result = pinned.directory_identities(
        &mut Vec::new(),
        &mut RepairBudget::new(),
        &mut IdentityCensus::default(),
    );
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
    let result = pinned.directory_identities(
        &mut identities,
        &mut RepairBudget::new(),
        &mut IdentityCensus::default(),
    );
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
        budget.enter(depth)?;
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
                {
                    if policy.regular_files_only {
                        dir.repair_require_regular_file(&name)?;
                    }
                    dir.remove_file(&name)?;
                }
                #[cfg(windows)]
                {
                    // The delete handle is validated as a regular, non-reparse
                    // file, so a Windows walk always stops at links and
                    // special files, whatever the policy asks.
                    let _ = policy.regular_files_only;
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

/// Remove everything below `dir` without following symlinks or junctions.
///
/// Commands can leave directories that their owner cannot write inside the
/// trees AFT deletes: Go's module cache is read-only by design, and test
/// suites `chmod -w` fixture folders and may exit before restoring them.
/// Unlinking an entry needs write and search permission on the directory that
/// holds it, so on Unix the walk gives the owner back full access to each
/// directory before emptying it. That change is made only through handles
/// opened without following links (see [`open_dir_for_removal`]): a symlink
/// inside the tree is removed as a link and never lets the walk change
/// permissions on, or delete, anything outside the tree. On Windows the
/// read-only attribute of each deleted file is cleared through its pinned
/// handle instead (see `windows_delete_pinned_handle_outcome`).
pub(crate) fn remove_tree_contents(dir: &PinnedDir) -> io::Result<()> {
    #[cfg(unix)]
    dir.restore_owner_access()
        .map_err(|error| removal_error(error, dir.path()))?;
    for name in dir.list_names()? {
        match open_dir_for_removal(dir, &name) {
            Ok(child) => {
                remove_tree_contents(&child)?;
                #[cfg(unix)]
                {
                    let c_name = os_cstring(&name)?;
                    let result = unsafe {
                        libc::unlinkat(dir.file.as_raw_fd(), c_name.as_ptr(), libc::AT_REMOVEDIR)
                    };
                    if result != 0 {
                        return Err(removal_error(
                            io::Error::last_os_error(),
                            &dir.path().join(&name),
                        ));
                    }
                }
                #[cfg(windows)]
                child.remove_self()?;
            }
            // A symlink (ELOOP on macOS, ENOTDIR on Linux) or any other
            // non-directory is unlinked itself; its target is never opened.
            Err(error) if repair_leaf_error(&error) => {
                dir.remove_file(&name)
                    .map_err(|error| removal_error(error, &dir.path().join(&name)))?;
            }
            Err(error) => return Err(removal_error(error, &dir.path().join(&name))),
        }
    }
    Ok(())
}

/// Name the entry a permission failure is about, so a bundle that still
/// cannot be removed reports which file or directory blocked it. Other
/// errors keep their original text, which callers and tests match on.
pub(crate) fn removal_error(error: io::Error, path: &Path) -> io::Error {
    if error.kind() != io::ErrorKind::PermissionDenied {
        return error;
    }
    io::Error::new(error.kind(), format!("{}: {error}", path.display()))
}

/// Open a child directory for removal. On Unix a directory whose owner lacks
/// read or search permission cannot be opened, so its owner access is
/// restored first, without following a symlink, and the reopened directory
/// must be the one whose permissions were changed.
///
/// The child must be on the same filesystem as `dir`. A command can leave a
/// mount (sshfs, rclone, macFUSE, a disk image attached under its TMPDIR)
/// inside the trees AFT deletes; descending into it would empty that volume.
/// Such a child fails with [`ForeignFilesystemRetained`], which is not a leaf
/// error, so the walk stops and keeps the whole tree for the owner to unmount.
pub(crate) fn open_dir_for_removal(dir: &PinnedDir, name: &OsStr) -> io::Result<PinnedDir> {
    let child = match dir.open_dir_at(name) {
        #[cfg(unix)]
        Err(error) if error.kind() == io::ErrorKind::PermissionDenied => {
            let restored = dir.restore_child_directory_access(name, error)?;
            let child = dir.open_dir_at(name)?;
            if child.directory_identity()? != restored {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "directory changed while its permissions were restored for removal",
                ));
            }
            child
        }
        result => result?,
    };
    if removal_device(child.path(), child.directory_identity()?.0)
        != removal_device(dir.path(), dir.directory_identity()?.0)
        || child.repair_mount_identity()? != dir.repair_mount_identity()?
    {
        return Err(io::Error::other(ForeignFilesystemRetained(
            child.path().to_path_buf(),
        )));
    }
    Ok(child)
}

/// The device a removal walk compares to stay on one filesystem. Tests cannot
/// mount a filesystem, so they inject a different device per path through
/// `REPAIR_IDENTITIES`, the same seam the repair policy uses.
fn removal_device(path: &Path, actual: u64) -> u64 {
    #[cfg(test)]
    if let Some(identity) = REPAIR_IDENTITIES.with(|ids| ids.borrow().get(path).copied()) {
        return identity.0;
    }
    #[cfg(not(test))]
    let _ = path;
    actual
}

/// A removal walk met a directory on another filesystem (a mount point) and
/// kept it, with everything below it and the tree that contains it.
#[derive(Debug)]
pub(crate) struct ForeignFilesystemRetained(pub(crate) PathBuf);

impl std::fmt::Display for ForeignFilesystemRetained {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}: directory is on another filesystem (a mount point); kept, unmount it so \
             the rest can be removed",
            self.0.display()
        )
    }
}

impl std::error::Error for ForeignFilesystemRetained {}

/// Remove the directory `name` inside `parent` and everything below it,
/// restoring owner access to read-only directories on the way (see
/// [`remove_tree_contents`]). The directory must belong to the current user:
/// AFT only force-removes trees it created for its own tasks.
#[cfg(unix)]
pub(crate) fn remove_directory_tree_at(parent: &PinnedDir, name: &OsStr) -> io::Result<()> {
    let child = open_dir_for_removal(parent, name)?;
    if child.file.metadata()?.uid() != unsafe { libc::geteuid() } {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "{}: directory to remove belongs to another user",
                child.path().display()
            ),
        ));
    }
    remove_tree_contents(&child)?;
    if !child.same_identity(&parent.open_dir_at(name)?)? {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "directory identity changed before removal",
        ));
    }
    let c_name = os_cstring(name)?;
    if unsafe { libc::unlinkat(parent.file.as_raw_fd(), c_name.as_ptr(), libc::AT_REMOVEDIR) } != 0
    {
        return Err(removal_error(io::Error::last_os_error(), child.path()));
    }
    Ok(())
}

#[cfg(test)]
pub(crate) mod work_counts {
    use std::cell::Cell;

    #[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
    pub(crate) struct Counts {
        pub opens: usize,
        pub metadata_opens: usize,
        pub metadata_writes: usize,
        pub metadata_reads: usize,
        pub parses: usize,
        pub marker_reads: usize,
        pub stats: usize,
        pub gc_entries: usize,
        pub gc_deadlines: usize,
        pub gc_teardown_opens: usize,
        pub gc_teardown_metadata_reads: usize,
        pub gc_teardown_stats: usize,
        pub artifact_bytes: usize,
        pub artifact_opens: usize,
        pub syncs: usize,
        pub renames: usize,
        pub rename_nanos: u128,
        pub rename_max_nanos: u128,
        pub sync_nanos: u128,
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
    pub(crate) fn record_metadata_open() {
        record(|counts| counts.metadata_opens += 1);
    }
    pub(crate) fn record_metadata_write() {
        record(|counts| counts.metadata_writes += 1);
    }
    pub(crate) fn record_sync() {
        record(|counts| counts.syncs += 1);
    }
    pub(crate) fn record_rename() {
        record(|counts| counts.renames += 1);
    }
    #[cfg(unix)]
    pub(crate) fn record_rename_time(elapsed: std::time::Duration) {
        record(|counts| {
            counts.rename_nanos += elapsed.as_nanos();
            counts.rename_max_nanos = counts.rename_max_nanos.max(elapsed.as_nanos());
        });
    }
    pub(crate) fn record_sync_time(elapsed: std::time::Duration) {
        record(|counts| counts.sync_nanos += elapsed.as_nanos());
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
    pub(crate) fn record_gc_deadline() {
        record(|counts| counts.gc_deadlines += 1);
    }
    pub(crate) fn record_gc_teardown_since(before: Counts) {
        record(|counts| {
            counts.gc_teardown_opens += counts.opens - before.opens;
            counts.gc_teardown_metadata_reads += counts.metadata_reads - before.metadata_reads;
            counts.gc_teardown_stats += counts.stats - before.stats;
        });
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

pub(crate) fn random_temp_name() -> io::Result<OsString> {
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

#[cfg(unix)]
#[derive(Debug)]
pub(crate) struct CoordinationCreateContended {
    path: PathBuf,
    attempts: usize,
}

#[cfg(unix)]
impl std::fmt::Display for CoordinationCreateContended {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "{}: {}",
            crate::cache_location::CACHE_DIR_BUSY,
            crate::cache_location::CACHE_DIR_BUSY_MESSAGE
        )
    }
}

#[cfg(unix)]
impl std::error::Error for CoordinationCreateContended {}

#[cfg(all(unix, test))]
impl CoordinationCreateContended {
    pub(crate) fn path(&self) -> &Path {
        &self.path
    }
}

#[cfg(unix)]
fn openat_file(
    dirfd: RawFd,
    parent: &Path,
    name: &OsStr,
    flags: i32,
    mode: libc::mode_t,
) -> io::Result<File> {
    // On macOS 27.0.1, two fresh-entry runs of concurrent openat(O_CREAT)
    // returned ENOENT in 597/800 and 569/800 attempts even though the winning
    // creator's file was present afterward. Preexisting-entry and absolute-path
    // open controls each failed 0/800 times. Without O_NOFOLLOW, fresh-entry
    // failures were 472/800. Reopen through the same
    // pinned parent, retaining every safety flag. Alternate a bounded number
    // of creates and noncreating opens if the winner has not published yet.
    // Exclusive creation must keep its create-new contract, never adopt a rival.
    let retry_create = flags & libc::O_CREAT != 0 && flags & libc::O_EXCL == 0;
    let attempts = if retry_create { 6 } else { 1 };
    for attempt in 0..attempts {
        let attempt_flags = if attempt % 2 == 0 {
            flags
        } else {
            flags & !libc::O_CREAT
        };
        match openat_file_once(dirfd, name, attempt_flags, mode) {
            Err(error) if retry_create && error.kind() == io::ErrorKind::NotFound => continue,
            result => return result,
        }
    }
    let contention = CoordinationCreateContended {
        path: parent.join(name),
        attempts,
    };
    // The agent-facing text names no path; the operator log keeps it.
    log::debug!(
        "cache coordination create stayed contended: path={} attempts={}",
        contention.path.display(),
        contention.attempts
    );
    Err(io::Error::new(io::ErrorKind::WouldBlock, contention))
}

#[cfg(unix)]
fn openat_file_once(
    dirfd: RawFd,
    name: &OsStr,
    flags: i32,
    mode: libc::mode_t,
) -> io::Result<File> {
    #[cfg(test)]
    if let Some(result) = OPENAT_FILE_HOOK.with(|slot| {
        slot.borrow_mut()
            .as_mut()
            .and_then(|hook| hook(dirfd, name, flags))
    }) {
        return result;
    }
    let name = os_cstring(name)?;
    let fd = unsafe { libc::openat(dirfd, name.as_ptr(), flags, libc::c_uint::from(mode)) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { File::from_raw_fd(fd) })
}

#[cfg(unix)]
pub(crate) fn os_cstring(value: &OsStr) -> io::Result<CString> {
    CString::new(value.as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path contains a NUL byte"))
}

pub(crate) fn validate_directory_handle(file: &File) -> io::Result<()> {
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

pub(crate) fn validate_regular_handle(file: &File) -> io::Result<()> {
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
    pub(crate) static BEFORE_ARTIFACT_VALIDATION: std::cell::RefCell<Option<Box<dyn FnMut(&File)>>> = const { std::cell::RefCell::new(None) };
}

#[cfg(all(test, unix))]
pub(crate) fn with_artifact_validation_hook_for_test<T>(
    hook: impl FnMut(&File) + 'static,
    read: impl FnOnce() -> T,
) -> T {
    struct Restore(Option<Box<dyn FnMut(&File)>>);
    impl Drop for Restore {
        fn drop(&mut self) {
            BEFORE_ARTIFACT_VALIDATION.with(|slot| slot.replace(self.0.take()));
        }
    }
    let _restore =
        Restore(BEFORE_ARTIFACT_VALIDATION.with(|slot| slot.replace(Some(Box::new(hook)))));
    read()
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
pub(crate) const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
#[cfg(windows)]
pub(crate) const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
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
const FILE_WRITE_ATTRIBUTES: u32 = 0x0000_0100;
#[cfg(windows)]
const FILE_ATTRIBUTE_READONLY: u32 = 0x0000_0001;
#[cfg(windows)]
const FILE_ATTRIBUTE_NORMAL: u32 = 0x0000_0080;
#[cfg(windows)]
const FILE_BASIC_INFO_CLASS: i32 = 0;
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
pub(crate) enum RepairOpen {
    Directory,
    ReadFile,
    DeleteFile,
    Coordination,
    NewDirectory,
    NewFile,
    RenameEntry,
}

#[cfg(windows)]
pub(crate) fn windows_relative_repair_file(
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
        budget.enter(0)?;
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
pub(crate) fn windows_delete_pinned_handle(file: &File, directory: bool) -> io::Result<()> {
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
    // Windows refuses to delete a file that carries the read-only attribute
    // (commands and tools such as `attrib +r` or Go's module cache set it).
    // Clear it through the same pinned handle that is about to be deleted, so
    // only this file changes and no path is resolved again.
    let readonly = !directory
        && windows_file_information(file)?.file_attributes & FILE_ATTRIBUTE_READONLY != 0;
    let deletion = if directory {
        windows_reopen_directory_for_delete(file)?
    } else {
        let handle = unsafe {
            ReOpenFile(
                file.as_raw_handle(),
                GENERIC_READ | DELETE_ACCESS | if readonly { FILE_WRITE_ATTRIBUTES } else { 0 },
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
    if readonly {
        windows_clear_readonly(&deletion)?;
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

/// Layout of the Win32 `FILE_BASIC_INFO` structure. Zero timestamps mean
/// "leave unchanged" when it is written with `SetFileInformationByHandle`.
#[cfg(windows)]
#[repr(C)]
struct WindowsFileBasicInfo {
    creation_time: i64,
    last_access_time: i64,
    last_write_time: i64,
    change_time: i64,
    file_attributes: u32,
}

#[cfg(windows)]
fn windows_clear_readonly(file: &File) -> io::Result<()> {
    let attributes = windows_file_information(file)?.file_attributes & !FILE_ATTRIBUTE_READONLY;
    let mut info = WindowsFileBasicInfo {
        creation_time: 0,
        last_access_time: 0,
        last_write_time: 0,
        change_time: 0,
        // Zero attributes would also mean "leave unchanged"; NORMAL is the
        // documented value for a file with no other attributes.
        file_attributes: if attributes == 0 {
            FILE_ATTRIBUTE_NORMAL
        } else {
            attributes
        },
    };
    if unsafe {
        SetFileInformationByHandle(
            file.as_raw_handle(),
            FILE_BASIC_INFO_CLASS,
            (&mut info as *mut WindowsFileBasicInfo).cast(),
            std::mem::size_of::<WindowsFileBasicInfo>() as u32,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(())
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
    use super::*;

    #[cfg(target_os = "macos")]
    #[test]
    #[ignore = "native Darwin syscall probe; run explicitly with --ignored --nocapture"]
    fn darwin_openat_concurrent_first_create_probe() {
        // Run on macOS: nice -n 19 cargo test -p agent-file-tools --lib
        // darwin_openat_concurrent_first_create_probe -- --ignored --nocapture
        use super::*;
        use std::sync::{Arc, Barrier};

        let temp = tempfile::tempdir().unwrap();
        let mut counts = Vec::new();
        for mode in ["raw-missing", "raw-existing", "protected-missing"] {
            let mut missing = 0;
            for round in 0..100 {
                let parent = temp.path().join(format!("{mode}-{round}"));
                fs::create_dir(&parent).unwrap();
                if mode == "raw-existing" {
                    File::create(parent.join(".aft-in-use.lock")).unwrap();
                }
                let directory = Arc::new(File::open(&parent).unwrap());
                let barrier = Arc::new(Barrier::new(8));
                missing += std::thread::scope(|scope| {
                    let handles = (0..8)
                        .map(|_| {
                            let directory = Arc::clone(&directory);
                            let barrier = Arc::clone(&barrier);
                            let parent = &parent;
                            scope.spawn(move || {
                                let flags = libc::O_RDWR
                                    | libc::O_CREAT
                                    | libc::O_NOFOLLOW
                                    | libc::O_CLOEXEC
                                    | libc::O_NONBLOCK;
                                barrier.wait();
                                let result = if mode == "protected-missing" {
                                    openat_file(
                                        directory.as_raw_fd(),
                                        parent,
                                        OsStr::new(".aft-in-use.lock"),
                                        flags,
                                        0o600,
                                    )
                                } else {
                                    let fd = unsafe {
                                        libc::openat(
                                            directory.as_raw_fd(),
                                            c".aft-in-use.lock".as_ptr(),
                                            flags,
                                            0o600 as libc::c_uint,
                                        )
                                    };
                                    if fd < 0 {
                                        Err(io::Error::last_os_error())
                                    } else {
                                        Ok(unsafe { File::from_raw_fd(fd) })
                                    }
                                };
                                barrier.wait();
                                match result {
                                    Ok(_) => 0,
                                    Err(error) => {
                                        assert_ne!(mode, "protected-missing", "{error}");
                                        assert_eq!(
                                            error.kind(),
                                            io::ErrorKind::NotFound,
                                            "{error}"
                                        );
                                        assert!(parent.join(".aft-in-use.lock").is_file());
                                        1
                                    }
                                }
                            })
                        })
                        .collect::<Vec<_>>();
                    handles
                        .into_iter()
                        .map(|handle| handle.join().unwrap())
                        .sum::<usize>()
                });
            }
            eprintln!("{mode}: {missing}/800 ENOENT results");
            counts.push(missing);
        }
        assert_eq!(counts[1], 0, "preexisting-file control");
        assert_eq!(counts[2], 0, "bounded create/reopen helper");
    }

    #[cfg(unix)]
    #[test]
    fn openat_create_retry_preserves_exclusive_creation_and_other_errors() {
        use super::*;
        use std::sync::{Arc, Mutex};

        let temp = tempfile::tempdir().unwrap();
        let parent = PinnedDir::repair_open_path(temp.path()).unwrap();
        for (exclusive, kind) in [
            (true, io::ErrorKind::NotFound),
            (false, io::ErrorKind::PermissionDenied),
        ] {
            let calls = Arc::new(Mutex::new(0));
            let observed = Arc::clone(&calls);
            let error = with_openat_file_hook(
                Box::new(move |_, _, _| {
                    *observed.lock().unwrap() += 1;
                    Some(Err(io::Error::from(kind)))
                }),
                || {
                    if exclusive {
                        parent.open_new_file(OsStr::new("exclusive"))
                    } else {
                        parent.repair_coordination_file(OsStr::new("denied"))
                    }
                },
            )
            .unwrap_err();
            assert_eq!(error.kind(), kind);
            assert_eq!(*calls.lock().unwrap(), 1);
        }
    }

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
}
