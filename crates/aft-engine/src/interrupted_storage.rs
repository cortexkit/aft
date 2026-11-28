//! Conservative liveness and path checks for interrupted storage writers.

use crate::bash_background::process::ProcessIdentity;
use crate::process_identity::ProcessLiveness;
use crate::walk_boundary::DeviceBoundary;
use serde::{Deserialize, Serialize};
use std::fs::{self, Metadata};
use std::io::{self, Read};
use std::path::Path;
use std::time::{Duration, SystemTime};

pub const ONE_HOUR: Duration = Duration::from_secs(3600);

pub fn schedule_sweeps(storage: &Path) {
    use std::collections::HashSet;
    use std::path::PathBuf;
    use std::sync::{Mutex, OnceLock};

    static SCHEDULED: OnceLock<Mutex<HashSet<PathBuf>>> = OnceLock::new();
    let scheduled = SCHEDULED.get_or_init(Default::default);
    let storage = storage.to_path_buf();
    let Ok(mut roots) = scheduled.lock() else {
        return;
    };
    if !roots.insert(storage.clone()) {
        return;
    }
    let worker_root = storage.clone();
    if std::thread::Builder::new()
        .name("aft-interrupted-storage".into())
        .spawn(move || {
            sweep_semantic_temps(&worker_root, SystemTime::now());
            crate::lsp::typescript_temp::sweep(&worker_root);
        })
        .is_err()
    {
        roots.remove(&storage);
    }
}

fn sweep_semantic_temps(storage: &Path, now: SystemTime) {
    let Some(boundary) = storage_child_dir(storage, "semantic") else {
        return;
    };
    let Ok(projects) = fs::read_dir(storage.join("semantic")) else {
        return;
    };
    for project in projects.flatten() {
        if !project
            .file_name()
            .to_str()
            .is_some_and(|key| hex_name(key, 16))
        {
            continue;
        }
        let dir = project.path();
        if !owned_metadata(&dir).is_ok_and(|meta| meta.is_dir())
            || !boundary.should_descend(&dir).unwrap_or(false)
        {
            continue;
        }
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        let mut lock = None;
        for entry in entries.flatten() {
            let path = entry.path();
            if !stale_semantic_temp(&path, &boundary, now) {
                continue;
            }
            // The old writer holds this lease through the final rename. Take
            // it before unlinking, and recheck the candidate after acquisition.
            if lock.is_none() {
                let lock_path = dir.join("semantic.persist.lock");
                match owned_metadata(&lock_path) {
                    Ok(meta)
                        if meta.is_file()
                            && boundary.should_descend(&lock_path).unwrap_or(false) => {}
                    Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                    _ => break,
                }
                let Ok(guard) = crate::fs_lock::try_acquire_once(&lock_path) else {
                    break;
                };
                lock = Some(guard);
            }
            if stale_semantic_temp(&path, &boundary, now) {
                let _ = fs::remove_file(path);
            }
        }
    }
}

fn stale_semantic_temp(path: &Path, boundary: &DeviceBoundary, now: SystemTime) -> bool {
    let Some((pid, stamp)) = path
        .file_name()
        .and_then(|name| name.to_str())
        .and_then(|name| name.strip_prefix("semantic.bin.tmp."))
        .and_then(|name| name.split_once('.'))
    else {
        return false;
    };
    let Some(pid) = (pid.bytes().all(|byte| byte.is_ascii_digit()))
        .then(|| pid.parse::<u32>().ok())
        .flatten()
        .filter(|pid| *pid > 0)
    else {
        return false;
    };
    if !stamp.bytes().all(|byte| byte.is_ascii_digit()) || stamp.parse::<u128>().is_err() {
        return false;
    }
    owned_metadata(path).is_ok_and(|meta| {
        meta.is_file()
            && meta
                .modified()
                .is_ok_and(|modified| old_enough(modified, now, ONE_HOUR))
    }) && boundary.should_descend(path).unwrap_or(false)
        && !crate::fs_lock::process_alive(pid)
}

/// Only artifacts known to predate the current boot can be reclaimed without
/// trustworthy child identity. Their paths are never reused by a new writer.
pub fn predates_boot(modified: SystemTime, boot: Option<SystemTime>) -> bool {
    boot.is_some_and(|boot| modified < boot)
}

pub fn old_enough(modified: SystemTime, now: SystemTime, age: Duration) -> bool {
    now.duration_since(modified)
        .is_ok_and(|elapsed| elapsed >= age)
}

pub fn owned_metadata(path: &Path) -> io::Result<Metadata> {
    let meta = fs::symlink_metadata(path)?;
    #[cfg(windows)]
    let redirected = {
        use std::os::windows::fs::MetadataExt;
        meta.file_attributes() & 0x400 != 0 // FILE_ATTRIBUTE_REPARSE_POINT
    };
    #[cfg(not(windows))]
    let redirected = meta.is_symlink();
    #[cfg(unix)]
    let owned = {
        use std::os::unix::fs::MetadataExt;
        meta.uid() == unsafe { libc::geteuid() }
    };
    #[cfg(not(unix))]
    let owned = true;
    if !owned || redirected {
        return Err(io::ErrorKind::PermissionDenied.into());
    }
    Ok(meta)
}

/// Validate one storage child before opening it. Never enumerate symlinked
/// domains, or cross to a mounted filesystem inside AFT's storage.
pub fn storage_child_dir(root: &Path, name: &str) -> Option<DeviceBoundary> {
    let child = root.join(name);
    let boundary = DeviceBoundary::for_root(root).ok()?;
    (owned_metadata(root).ok()?.is_dir()
        && owned_metadata(&child).ok()?.is_dir()
        && boundary.should_descend(&child).ok()?)
    .then_some(boundary)
}

pub(crate) fn hex_name(name: &str, len: usize) -> bool {
    name.len() == len
        && name
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

pub fn exact_liveness(identity: &ProcessIdentity) -> ProcessLiveness {
    if identity.pid == 0 || identity.started_at_ms == 0 {
        return ProcessLiveness::Unknown;
    }
    match ProcessIdentity::of(identity.pid) {
        Some(current) if current.same_process(identity) => ProcessLiveness::Alive,
        Some(_) => ProcessLiveness::Gone,
        None if !crate::fs_lock::process_alive(identity.pid) => ProcessLiveness::Gone,
        None => ProcessLiveness::Unknown,
    }
}

#[derive(Clone, Serialize, Deserialize)]
pub struct ArtifactOwner {
    pub owner: Option<ProcessIdentity>,
    pub child: ChildState,
}

#[derive(Clone, Serialize, Deserialize)]
pub enum ChildState {
    Pending,
    Running(ProcessIdentity),
    Finished,
}

impl ArtifactOwner {
    pub fn current(child: ChildState) -> Self {
        Self {
            owner: ProcessIdentity::of(std::process::id()),
            child,
        }
    }

    pub fn write(self, marker: &Path) -> io::Result<()> {
        crate::durability::atomic_write(
            marker,
            &serde_json::to_vec(&self)?,
            crate::durability::AtomicWriteDurability::File,
        )
    }

    pub fn read(marker: &Path) -> Option<Self> {
        const MAX_BYTES: u64 = 1024;
        let meta = owned_metadata(marker).ok()?;
        if !meta.is_file() || meta.len() > MAX_BYTES {
            return None;
        }
        let mut bytes = Vec::new();
        fs::File::open(marker)
            .ok()?
            .take(MAX_BYTES + 1)
            .read_to_end(&mut bytes)
            .ok()?;
        (bytes.len() as u64 <= MAX_BYTES)
            .then(|| serde_json::from_slice(&bytes).ok())
            .flatten()
    }

    pub fn record_child(marker: &Path, pid: u32) {
        if let Some(identity) = ProcessIdentity::of(pid) {
            let _ = Self::current(ChildState::Running(identity)).write(marker);
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn interrupted_run_cleanup_sweeps_only_old_unlocked_semantic_temps() {
        let storage = tempfile::tempdir().unwrap();
        let dir = storage.path().join("semantic/1234567890abcdef");
        fs::create_dir_all(&dir).unwrap();
        let mut child = std::process::Command::new("/bin/sh")
            .args(["-c", "exit 0"])
            .spawn()
            .unwrap();
        let dead = child.id();
        child.wait().unwrap();
        let now = SystemTime::now();
        let old = filetime::FileTime::from_system_time(now - ONE_HOUR - Duration::from_secs(5));
        let write = |name: &str, aged: bool| {
            let path = dir.join(name);
            fs::write(&path, b"fixture").unwrap();
            if aged {
                filetime::set_file_mtime(&path, old).unwrap();
            }
            path
        };
        let stale = write(&format!("semantic.bin.tmp.{dead}.1"), true);
        let fresh = write(&format!("semantic.bin.tmp.{dead}.2"), false);
        let live = write(&format!("semantic.bin.tmp.{}.3", std::process::id()), true);
        let malformed = write(&format!("semantic.bin.tmp.{dead}.4.bak"), true);
        let snapshot = write("semantic.bin", true);
        let link = dir.join(format!("semantic.bin.tmp.{dead}.5"));
        std::os::unix::fs::symlink(&snapshot, &link).unwrap();

        let lock = crate::fs_lock::try_acquire_once(&dir.join("semantic.persist.lock")).unwrap();
        sweep_semantic_temps(storage.path(), now);
        assert!(stale.exists(), "the persistence lease still holds");
        drop(lock);
        sweep_semantic_temps(storage.path(), now);
        assert!(!stale.exists());
        for path in [fresh, live, malformed, snapshot, link] {
            assert!(path.exists(), "{} was swept", path.display());
        }
    }
}
