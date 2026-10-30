//! Short, private scratch paths leave room for pathname-based Unix sockets.

use std::ffi::CString;
use std::fs::{self, File, OpenOptions, ReadDir};
use std::io::{self, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, SystemTime};

#[cfg(target_os = "macos")]
const ROOT: &str = "/private/tmp/aft-w";
#[cfg(not(target_os = "macos"))]
const ROOT: &str = "/tmp/aft-w";
const SWEEP_BUDGET: usize = 128;
const STALE_AFTER: Duration = Duration::from_secs(24 * 60 * 60);

pub(super) fn root() -> &'static Path {
    Path::new(ROOT)
}

struct Parent {
    file: File,
    path: PathBuf,
}

impl Parent {
    fn open(path: &Path) -> io::Result<Self> {
        let base = open_dir(path.parent().unwrap())?;
        let name = CString::new(path.file_name().unwrap().as_encoded_bytes())?;
        if unsafe { libc::mkdirat(base.as_raw_fd(), name.as_ptr(), 0o700) } != 0 {
            let error = io::Error::last_os_error();
            if error.kind() != io::ErrorKind::AlreadyExists {
                return Err(error);
            }
        }
        // O_NOFOLLOW plus fstat checks the opened object, not a racy path stat.
        let file = open_dir_at(&base, &name)?;
        validate_private_dir(&file, unsafe { libc::geteuid() })?;
        Ok(Self {
            file,
            path: path.to_owned(),
        })
    }

    fn verify_identity(&self) -> io::Result<()> {
        let current = open_dir(&self.path)?.metadata()?;
        let held = self.file.metadata()?;
        if current.dev() != held.dev() || current.ino() != held.ino() {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "task temp parent changed",
            ));
        }
        Ok(())
    }

    fn allocate(&self, task_io: &Path) -> io::Result<PathBuf> {
        for _ in 0..32 {
            let mut random = [0u8; 6];
            getrandom::fill(&mut random).map_err(io::Error::other)?;
            let name = super::hex_bytes(&random);
            let c_name = CString::new(name.as_str())?;
            if unsafe { libc::mkdirat(self.file.as_raw_fd(), c_name.as_ptr(), 0o700) } != 0 {
                let error = io::Error::last_os_error();
                if error.kind() == io::ErrorKind::AlreadyExists {
                    continue;
                }
                return Err(error);
            }
            let child = open_dir_at(&self.file, &c_name)?;
            validate_private_dir(&child, unsafe { libc::geteuid() })?;
            self.verify_identity()?;
            let path = self.path.join(&name);
            // Keep the association outside the child-writable directory. A
            // crashed setup can be reclaimed after its IO bundle disappears.
            let result = (|| {
                let mut owner = OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .mode(0o600)
                    .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
                    .open(self.path.join(format!("{name}.owner")))?;
                owner.write_all(task_io.as_os_str().as_encoded_bytes())
            })();
            if let Err(error) = result {
                let _ = fs::remove_dir(&path);
                return Err(error);
            }
            return Ok(path);
        }
        Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "task temp collision limit",
        ))
    }

    fn remove(&self, name: &str) -> io::Result<()> {
        use crate::bash_background::persistence::{remove_directory_tree_at, PinnedDir};
        self.verify_identity()?;
        // The command's TMPDIR may hold directories it made read-only (Go's
        // module cache, tests that `chmod -w` a fixture). The pinned removal
        // restores owner access to those without following symlinks, where
        // `fs::remove_dir_all` stopped with "Permission denied" and left the
        // task bundle undeletable.
        let parent = PinnedDir::open(&self.path)?;
        let held = self.file.metadata()?;
        if parent.directory_identity()? != (held.dev(), held.ino()) {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "task temp parent changed",
            ));
        }
        match remove_directory_tree_at(&parent, std::ffi::OsStr::new(name)) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        remove_if_present(&self.path.join(format!("{name}.owner")))
    }
}

fn open_dir(path: &Path) -> io::Result<File> {
    OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)
}

fn open_dir_at(parent: &File, name: &CString) -> io::Result<File> {
    use std::os::fd::FromRawFd;
    let fd = unsafe {
        libc::openat(
            parent.as_raw_fd(),
            name.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { File::from_raw_fd(fd) })
}

fn validate_private_dir(file: &File, uid: u32) -> io::Result<()> {
    let metadata = file.metadata()?;
    if !metadata.is_dir() || metadata.uid() != uid || metadata.mode() & 0o777 != 0o700 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "task temp directory must be owned by the current user with mode 0700",
        ));
    }
    Ok(())
}

fn remove_if_present(path: &Path) -> io::Result<()> {
    match fs::remove_file(path) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        result => result,
    }
}

fn task_name(name: &str) -> bool {
    name.len() == 12
        && name
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
}

pub(super) fn is_managed(path: &Path) -> bool {
    path.parent() == Some(Path::new(ROOT))
        && path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(task_name)
}

pub(super) fn create(task_io: &Path) -> io::Result<PathBuf> {
    static CURSOR: OnceLock<Mutex<Option<ReadDir>>> = OnceLock::new();
    let parent = Parent::open(Path::new(ROOT))?;
    if let Ok(mut cursor) = CURSOR.get_or_init(|| Mutex::new(None)).lock() {
        // A failed maintenance pass must not prevent a fresh task from starting.
        let _ = sweep_stale(&parent, &mut cursor, SystemTime::now());
    }
    parent.allocate(task_io)
}

pub(super) fn remove(path: &Path) -> io::Result<()> {
    if !is_managed(path) {
        return Ok(());
    }
    Parent::open(Path::new(ROOT))?.remove(path.file_name().unwrap().to_str().unwrap())
}

fn sweep_stale(
    parent: &Parent,
    cursor: &mut Option<ReadDir>,
    now: SystemTime,
) -> io::Result<usize> {
    use std::os::unix::ffi::OsStringExt;
    parent.verify_identity()?;
    if cursor.is_none() {
        *cursor = Some(fs::read_dir(&parent.path)?);
    }
    let mut inspected = 0;
    // Advance the live iterator, rather than collecting the entire shared root
    // before applying the budget. Retained entries are revisited next cycle.
    for _ in 0..SWEEP_BUDGET {
        let Some(entry) = cursor.as_mut().unwrap().next() else {
            *cursor = None;
            break;
        };
        inspected += 1;
        let Ok(entry) = entry else { continue };
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        let task = name.strip_suffix(".owner").unwrap_or(name);
        if !task_name(task) {
            continue;
        }
        let Ok(metadata) = fs::symlink_metadata(entry.path()) else {
            continue;
        };
        if metadata.file_type().is_symlink()
            || metadata.uid() != unsafe { libc::geteuid() }
            || now.duration_since(metadata.modified()?).unwrap_or_default() < STALE_AFTER
        {
            continue;
        }
        let owner_path = parent.path.join(format!("{task}.owner"));
        let owner = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC)
            .open(&owner_path);
        let stale = match owner {
            Ok(mut owner) => {
                if !owner.metadata()?.is_file() {
                    continue;
                }
                let mut bytes = Vec::new();
                owner.read_to_end(&mut bytes)?;
                let io_path = PathBuf::from(std::ffi::OsString::from_vec(bytes));
                // A crash can leave the newly created owner file empty.
                if !io_path.is_absolute()
                    || matches!(fs::symlink_metadata(&io_path), Err(error) if error.kind() == io::ErrorKind::NotFound)
                {
                    true
                } else {
                    // Retained task records keep their IO bundle for days after exit.
                    // Use the validated task-record reader, and keep unknown
                    // records or terminal leaders with surviving descendants.
                    let record = io_path
                        .parent()
                        .map(|task| task.join("control/metadata.json"));
                    record
                        .and_then(|path| crate::bash_background::persistence::read_task(&path).ok())
                        .is_some_and(|metadata| {
                            // A long-running command can exit with already-old
                            // scratch. Give terminal output enrichment time to
                            // finish before another process's stale sweep reclaims it.
                            let finished = SystemTime::UNIX_EPOCH
                                + Duration::from_millis(
                                    metadata.finished_at.unwrap_or(metadata.started_at),
                                );
                            metadata.sandbox_temp_dir.as_deref()
                                == Some(parent.path.join(task).as_path())
                                && now.duration_since(finished).unwrap_or_default() >= STALE_AFTER
                                && crate::bash_background::persistence::task_temp_quiescent(
                                    &metadata,
                                )
                        })
                }
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => true,
            Err(_) => false,
        };
        if stale {
            let _ = parent.remove(task);
        }
    }
    Ok(inspected)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::{symlink, PermissionsExt};

    #[test]
    fn sandbox_task_temp_parent_refuses_symlinks_modes_and_foreign_owners() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("aft-w");
        symlink(root.path(), &path).unwrap();
        assert!(Parent::open(&path).is_err());
        fs::remove_file(&path).unwrap();
        let parent = Parent::open(&path).unwrap();
        assert_eq!(parent.file.metadata().unwrap().mode() & 0o777, 0o700);
        assert!(
            validate_private_dir(&parent.file, unsafe { libc::geteuid() }.wrapping_add(1)).is_err()
        );
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        assert!(Parent::open(&path).is_err());
    }

    #[test]
    fn sandbox_task_temp_creation_is_private_unique_and_cleanup_removes_owner() {
        let root = tempfile::tempdir().unwrap();
        let parent = Parent::open(&root.path().join("aft-w")).unwrap();
        let first = parent.allocate(root.path()).unwrap();
        let second = parent.allocate(root.path()).unwrap();
        assert_ne!(first, second);
        assert_eq!(fs::metadata(&first).unwrap().mode() & 0o777, 0o700);
        fs::write(first.join("scratch"), "data").unwrap();
        parent
            .remove(first.file_name().unwrap().to_str().unwrap())
            .unwrap();
        assert!(!first.exists());
        assert!(second.is_dir());
        parent
            .remove(second.file_name().unwrap().to_str().unwrap())
            .unwrap();
        assert_eq!(fs::read_dir(&parent.path).unwrap().count(), 0);
    }

    #[test]
    fn sandbox_task_temp_cleanup_removes_read_only_trees_without_following_links() {
        let root = tempfile::tempdir().unwrap();
        let parent = Parent::open(&root.path().join("aft-w")).unwrap();
        let scratch = parent.allocate(root.path()).unwrap();
        let outside = root.path().join("outside");
        fs::create_dir(&outside).unwrap();
        fs::write(outside.join("keep"), "outside").unwrap();
        fs::set_permissions(&outside, fs::Permissions::from_mode(0o500)).unwrap();
        // Go's module cache marks every directory read-only; a test that
        // `chmod 000`s a fixture and crashes leaves a directory with no
        // owner access at all.
        let module = scratch.join("go/pkg/mod/example.com/mod@v1");
        fs::create_dir_all(&module).unwrap();
        fs::write(module.join("go.mod"), "module example.com/mod").unwrap();
        symlink(&outside, module.join("outside-link")).unwrap();
        let fixture = scratch.join("fixture");
        fs::create_dir(&fixture).unwrap();
        fs::write(fixture.join("data"), "data").unwrap();
        for directory in module.ancestors().take(5) {
            fs::set_permissions(directory, fs::Permissions::from_mode(0o555)).unwrap();
        }
        fs::set_permissions(&fixture, fs::Permissions::from_mode(0o000)).unwrap();

        parent
            .remove(scratch.file_name().unwrap().to_str().unwrap())
            .unwrap();

        assert!(!scratch.exists());
        assert!(!scratch.with_extension("owner").exists());
        assert_eq!(fs::read_to_string(outside.join("keep")).unwrap(), "outside");
        assert_eq!(
            fs::metadata(&outside).unwrap().mode() & 0o7777,
            0o500,
            "a symlink target's permissions were changed"
        );
        fs::set_permissions(&outside, fs::Permissions::from_mode(0o700)).unwrap();
    }

    #[test]
    fn sandbox_task_temp_sweep_keeps_just_finished_old_scratch() {
        use crate::bash_background::{persistence, BgTaskStatus};
        let root = tempfile::tempdir().unwrap();
        let parent = Parent::open(&root.path().join("aft-w")).unwrap();
        let paths =
            persistence::create_task_layout(root.path(), "session", "bash-0000000000000001")
                .unwrap()
                .paths;
        fs::create_dir_all(&paths.io_dir).unwrap();
        let scratch = parent.allocate(&paths.io_dir).unwrap();
        let now = SystemTime::now();
        let old = filetime::FileTime::from_system_time(now - STALE_AFTER - Duration::from_secs(1));
        filetime::set_file_mtime(&scratch, old).unwrap();
        filetime::set_file_mtime(scratch.with_extension("owner"), old).unwrap();
        let mut metadata = persistence::PersistedTask::starting(
            paths.task_id.clone(),
            "session".into(),
            "true".into(),
            root.path().into(),
            None,
            None,
            false,
            false,
        );
        metadata.sandbox_temp_dir = Some(scratch.clone());
        metadata.mark_terminal(BgTaskStatus::Completed, Some(0), None);
        persistence::write_task(&paths.json, &metadata).unwrap();
        let mut cursor = None;
        sweep_stale(&parent, &mut cursor, SystemTime::now()).unwrap();
        assert!(
            scratch.is_dir(),
            "just-finished task lost its launch sandbox"
        );
        cursor = None;
        sweep_stale(
            &parent,
            &mut cursor,
            SystemTime::now() + STALE_AFTER + Duration::from_secs(1),
        )
        .unwrap();
        assert!(!scratch.exists(), "stale terminal task scratch was leaked");
        assert!(!scratch.with_extension("owner").exists());
    }

    #[test]
    fn sandbox_task_temp_sweep_reclaims_terminal_records_from_older_cards() {
        use crate::bash_background::{persistence, BgTaskStatus};
        let root = tempfile::tempdir().unwrap();
        let parent = Parent::open(&root.path().join("aft-w")).unwrap();
        let mut kept = Vec::new();
        for (number, status) in [
            BgTaskStatus::Completed,
            BgTaskStatus::Failed,
            BgTaskStatus::TimedOut,
            BgTaskStatus::Killed,
            BgTaskStatus::Running,
            BgTaskStatus::Completed,
            BgTaskStatus::Completed,
        ]
        .into_iter()
        .enumerate()
        {
            let paths = persistence::create_task_layout(
                root.path(),
                "older-card",
                &format!("bash-{number:016x}"),
            )
            .unwrap()
            .paths;
            fs::create_dir_all(&paths.io_dir).unwrap();
            let path = parent.allocate(&paths.io_dir).unwrap();
            let mut metadata = persistence::PersistedTask::starting(
                paths.task_id.clone(),
                "older-card".into(),
                "true".into(),
                root.path().into(),
                None,
                None,
                false,
                false,
            );
            metadata.status = status;
            metadata.sandbox_temp_dir = Some(path.clone());
            if number == 5 {
                metadata.child_pid = Some(std::process::id());
                metadata.pgid = Some(unsafe { libc::getpgrp() });
                metadata.live_descendants = Some(Vec::new());
            }
            persistence::write_task(&paths.json, &metadata).unwrap();
            if number == 6 {
                fs::write(&paths.json, b"unreadable record").unwrap();
            }
            kept.push((path, number < 4));
        }
        let mut cursor = None;
        sweep_stale(
            &parent,
            &mut cursor,
            SystemTime::now() + STALE_AFTER + Duration::from_secs(1),
        )
        .unwrap();
        for (path, terminal) in kept {
            assert_eq!(
                path.exists(),
                !terminal,
                "terminal={terminal}: {}",
                path.display()
            );
        }
    }

    #[test]
    fn sandbox_task_temp_sweep_is_bounded_resumable_and_preserves_live_bundles() {
        let root = tempfile::tempdir().unwrap();
        let parent = Parent::open(&root.path().join("aft-w")).unwrap();
        let live = parent.allocate(root.path()).unwrap();
        let missing = root.path().join("missing-io");
        for _ in 0..SWEEP_BUDGET + 2 {
            parent.allocate(&missing).unwrap();
        }
        let orphan = parent.path.join("000000000000");
        fs::create_dir(&orphan).unwrap();
        fs::set_permissions(&orphan, fs::Permissions::from_mode(0o700)).unwrap();
        let torn = parent.allocate(&missing).unwrap();
        fs::write(torn.with_extension("owner"), b"").unwrap();
        let fresh = SystemTime::now();
        let mut cursor = None;
        assert_eq!(
            sweep_stale(&parent, &mut cursor, fresh).unwrap(),
            SWEEP_BUDGET
        );
        assert!(orphan.exists());
        cursor = None;
        let future = fresh + STALE_AFTER + Duration::from_secs(1);
        assert_eq!(
            sweep_stale(&parent, &mut cursor, future).unwrap(),
            SWEEP_BUDGET
        );
        assert!(cursor.is_some());
        while cursor.is_some() {
            assert!(sweep_stale(&parent, &mut cursor, future).unwrap() <= SWEEP_BUDGET);
        }
        assert!(live.is_dir());
        assert!(!orphan.exists());
        assert!(!torn.exists());
        assert_eq!(fs::read_dir(&parent.path).unwrap().count(), 2);
    }
}
