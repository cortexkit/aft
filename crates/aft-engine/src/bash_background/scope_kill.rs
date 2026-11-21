use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::path::Path;

pub(super) fn kill(cgroup: &Path) -> io::Result<()> {
    kill_with(cgroup, |file| file.write_all(b"1"))
}

fn kill_with(cgroup: &Path, write: impl FnOnce(&mut File) -> io::Result<()>) -> io::Result<()> {
    let existed_before = cgroup.try_exists();
    let (phase, result, collection_errno) = match OpenOptions::new()
        .write(true)
        .open(cgroup.join("cgroup.kill"))
    {
        Ok(mut file) => ("write", write(&mut file), libc::ENODEV),
        Err(error) => ("open", Err(error), libc::ENOENT),
    };
    let Err(error) = result else { return Ok(()) };
    let exists_after = cgroup.try_exists();
    // The process-group kill can empty the scope first. If systemd removes the
    // scope while cgroup.kill is open, the write can fail with ENODEV.
    if error.raw_os_error() == Some(collection_errno) && matches!(exists_after, Ok(false)) {
        return Ok(());
    }
    log::warn!(
        "task scope kill {phase} failed for {}: {error} (errno={:?}, existed before={existed_before:?}, exists after={exists_after:?})",
        cgroup.display(),
        error.raw_os_error(),
    );
    Err(error)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn task_scope_kill_accepts_collection_between_open_and_write() {
        let root = tempfile::tempdir().unwrap();
        let scope = root.path().join("scope");
        fs::create_dir(&scope).unwrap();
        fs::write(scope.join("cgroup.kill"), "0").unwrap();
        kill_with(&scope, |_| {
            fs::remove_dir_all(&scope).unwrap();
            Err(io::Error::from_raw_os_error(libc::ENODEV))
        })
        .unwrap();
    }

    #[test]
    fn task_scope_kill_does_not_treat_enodev_alone_as_collection() {
        let root = tempfile::tempdir().unwrap();
        fs::write(root.path().join("cgroup.kill"), "0").unwrap();
        let error = kill_with(root.path(), |_| {
            Err(io::Error::from_raw_os_error(libc::ENODEV))
        })
        .unwrap_err();
        assert_eq!(error.raw_os_error(), Some(libc::ENODEV));
    }

    #[test]
    fn task_scope_kill_preserves_other_write_errors_even_if_scope_disappears() {
        let root = tempfile::tempdir().unwrap();
        let scope = root.path().join("scope");
        fs::create_dir(&scope).unwrap();
        fs::write(scope.join("cgroup.kill"), "0").unwrap();
        let error = kill_with(&scope, |_| {
            fs::remove_dir_all(&scope).unwrap();
            Err(io::Error::from_raw_os_error(libc::EACCES))
        })
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
    }
}
