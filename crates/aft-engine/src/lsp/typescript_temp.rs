use super::registry::ServerKind;
use crate::interrupted_storage::{
    exact_liveness, hex_name, old_enough, owned_metadata, predates_boot, storage_child_dir,
    ArtifactOwner, ChildState, ONE_HOUR,
};
use crate::process_identity::ProcessLiveness::{Alive, Gone};
use crate::walk_boundary::DeviceBoundary;
use std::collections::HashMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

const DOMAIN: &str = "lsp-tmp";
const PREFIX: &str = "typescript-";
const OWNER: &str = ".aft-owner.json";
const LEGACY_AGE: Duration = Duration::from_secs(24 * 3600);

pub(crate) struct TypeScriptTempDir(PathBuf);

impl TypeScriptTempDir {
    pub(super) fn path(&self) -> &Path {
        &self.0
    }

    pub(super) fn record_server(&self, pid: u32) {
        ArtifactOwner::record_child(&self.0.join(OWNER), pid);
    }
}

impl Drop for TypeScriptTempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

pub(super) fn typescript_server_temp_dir(
    kind: &ServerKind,
    storage_root: &Path,
    env: &mut HashMap<String, String>,
) -> io::Result<Option<TypeScriptTempDir>> {
    if !matches!(kind, ServerKind::TypeScript(_)) {
        return Ok(None);
    }
    let parent = storage_root.join(DOMAIN);
    crate::private_storage::create_dir_all(&parent)?;
    let mut random = [0u8; 16];
    getrandom::fill(&mut random).map_err(|error| io::Error::other(error.to_string()))?;
    let path = parent.join(format!("{PREFIX}{:032x}", u128::from_le_bytes(random)));
    // A fresh private path keeps concurrent TypeScript servers from racing to
    // create the same per-UID directory at startup.
    crate::private_storage::create_dir(&path)?;
    let directory = TypeScriptTempDir(path);
    let _ = ArtifactOwner::current(ChildState::Pending).write(&directory.path().join(OWNER));
    let path = directory.path().to_str().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "TypeScript temp path is not UTF-8",
        )
    })?;
    for key in ["TMPDIR", "TMP", "TEMP"] {
        env.insert(key.into(), path.into());
    }
    Ok(Some(directory))
}

pub(crate) fn sweep(storage: &Path) {
    let kept = sweep_at(
        storage,
        SystemTime::now(),
        crate::process_identity::boot_time(),
    );
    if kept > 0 {
        crate::slog_info!(
            "TypeScript temporary directory cleanup kept {kept} unprovable orphan candidate(s)"
        );
    }
}

fn sweep_at(storage: &Path, now: SystemTime, boot: Option<SystemTime>) -> usize {
    let Some(boundary) = storage_child_dir(storage, DOMAIN) else {
        return 0;
    };
    let Ok(entries) = fs::read_dir(storage.join(DOMAIN)) else {
        return 0;
    };
    let mut unprovable = 0;
    for entry in entries.flatten() {
        let name = entry.file_name();
        if !name
            .to_str()
            .and_then(|name| name.strip_prefix(PREFIX))
            .is_some_and(|id| hex_name(id, 32))
        {
            continue;
        }
        let path = entry.path();
        let Ok(meta) = owned_metadata(&path) else {
            continue;
        };
        if !meta.is_dir() || !boundary.should_descend(&path).unwrap_or(false) {
            continue;
        }
        let Ok(modified) = meta.modified() else {
            continue;
        };
        if !old_enough(modified, now, ONE_HOUR) {
            continue;
        }
        let marker = ArtifactOwner::read(&path.join(OWNER));
        let owner = marker.as_ref().and_then(|marker| marker.owner.as_ref().map(exact_liveness));
        let server = match marker.map(|marker| marker.child) {
            Some(ChildState::Running(server)) => Some((server.pid, exact_liveness(&server))),
            _ => None,
        };
        if owner == Some(Alive) || server.is_some_and(|(_, state)| state == Alive) {
            continue;
        }
        let recorded_gone = owner == Some(Gone)
            && server.is_some_and(|(pid, state)| state == Gone && server_group_gone(pid));
        if !recorded_gone {
            if !old_enough(modified, now, LEGACY_AGE) {
                continue;
            }
            if !predates_boot(modified, boot) {
                unprovable += 1;
                continue;
            }
        }
        // Treat reparse points and nested mounts as foreign, even when the
        // owner and server have both exited.
        if tree_stays_inside_storage(&path, &boundary).unwrap_or(false) {
            let _ = fs::remove_dir_all(path);
        }
    }
    unprovable
}

#[cfg(unix)]
fn server_group_gone(pid: u32) -> bool {
    // The TypeScript wrapper can exit before its tsserver descendants. The
    // launcher has made the wrapper's PID the process group ID with setsid.
    let Ok(pgid) = i32::try_from(pid) else {
        return false;
    };
    pgid > 0
        && unsafe { libc::kill(-pgid, 0) } != 0
        && io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
}

#[cfg(not(unix))]
fn server_group_gone(_pid: u32) -> bool {
    // Windows does not put the wrapper and its descendants in a job object.
    // Wrapper exit alone cannot justify deleting a descendant's temp files.
    false
}

fn tree_stays_inside_storage(path: &Path, boundary: &DeviceBoundary) -> io::Result<bool> {
    let mut dirs = vec![path.to_path_buf()];
    while let Some(dir) = dirs.pop() {
        for entry in fs::read_dir(dir)? {
            let path = entry?.path();
            let meta = owned_metadata(&path)?;
            if !boundary.should_descend(&path)? {
                return Ok(false);
            }
            if meta.is_dir() {
                dirs.push(path);
            } else if !meta.is_file() {
                return Ok(false);
            }
        }
    }
    Ok(true)
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::bash_background::process::ProcessIdentity;

    #[test]
    fn interrupted_run_cleanup_keeps_live_and_unprovable_typescript_dirs() {
        let storage = tempfile::tempdir().unwrap();
        let parent = storage.path().join(DOMAIN);
        fs::create_dir_all(&parent).unwrap();
        let mut child = std::process::Command::new("/bin/sh")
            .args(["-c", "exit 0"])
            .spawn()
            .unwrap();
        let dead = ProcessIdentity::from_epoch(child.id(), 1);
        child.wait().unwrap();
        let live = ProcessIdentity::of(std::process::id()).unwrap();
        let now = SystemTime::now();
        let boot = now - LEGACY_AGE - ONE_HOUR;
        let create = |id: u32, marker: Option<ArtifactOwner>, modified: SystemTime| {
            let dir = parent.join(format!("{PREFIX}{id:032x}"));
            fs::create_dir(&dir).unwrap();
            fs::write(dir.join("server-data"), "fixture").unwrap();
            if let Some(marker) = marker {
                marker.write(&dir.join(OWNER)).unwrap();
            }
            filetime::set_file_mtime(&dir, filetime::FileTime::from_system_time(modified)).unwrap();
            dir
        };
        let recorded = |owner, child| {
            Some(ArtifactOwner {
                owner: Some(owner),
                child,
            })
        };
        let dead_pair = recorded(dead.clone(), ChildState::Running(dead.clone()));
        let orphan = create(1, dead_pair.clone(), now - 2 * ONE_HOUR);
        let live_owner = create(
            2,
            recorded(live.clone(), ChildState::Running(dead.clone())),
            now - 2 * ONE_HOUR,
        );
        let live_server = create(
            3,
            recorded(dead.clone(), ChildState::Running(live)),
            now - 2 * ONE_HOUR,
        );
        let pending = create(
            4,
            recorded(dead, ChildState::Pending),
            boot + Duration::from_secs(1),
        );
        let older_than_boot = create(5, None, boot - Duration::from_secs(1));
        let fresh = create(6, dead_pair.clone(), now);
        let unrelated = parent.join("typescript-not-a-generated-name");
        fs::create_dir(&unrelated).unwrap();
        let link = create(7, dead_pair, now - 2 * ONE_HOUR);
        std::os::unix::fs::symlink(&unrelated, link.join("foreign")).unwrap();
        filetime::set_file_mtime(
            &link,
            filetime::FileTime::from_system_time(now - 2 * ONE_HOUR),
        )
        .unwrap();

        assert_eq!(sweep_at(storage.path(), now, Some(boot)), 1);
        assert!(!orphan.exists());
        assert!(!older_than_boot.exists());
        for path in [live_owner, live_server, pending, fresh, unrelated, link] {
            assert!(path.is_dir(), "{} was swept", path.display());
        }
    }
}
