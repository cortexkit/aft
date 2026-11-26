use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::path::Path;
use std::time::{Duration, Instant};

pub(super) fn kill(cgroup: &Path) -> io::Result<()> {
    kill_with(cgroup, |file| file.write_all(b"1"))
}

/// The processes in a cgroup and every cgroup below it, sorted. A cgroup that
/// is gone, or cannot be read, lists none.
fn member_pids(cgroup: &Path) -> Vec<u32> {
    let mut pids = Vec::new();
    let mut pending = vec![cgroup.to_path_buf()];
    while let Some(directory) = pending.pop() {
        if let Ok(text) = std::fs::read_to_string(directory.join("cgroup.procs")) {
            pids.extend(
                text.split_whitespace()
                    .filter_map(|pid| pid.parse::<u32>().ok()),
            );
        }
        for entry in std::fs::read_dir(&directory)
            .into_iter()
            .flatten()
            .flatten()
        {
            if entry.file_type().is_ok_and(|kind| kind.is_dir()) {
                pending.push(entry.path());
            }
        }
    }
    pids.sort_unstable();
    pids.dedup();
    pids
}

fn pidfd_open(pid: u32) -> Option<OwnedFd> {
    let pid = libc::pid_t::try_from(pid).ok()?;
    let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) };
    (fd >= 0).then(|| unsafe { OwnedFd::from_raw_fd(fd as i32) })
}

/// SIGTERM every process in `cgroup`, then wait the termination grace period,
/// ending early once the cgroup is empty. A listed pid can exit and be given
/// to an unrelated process before it is signalled, so each is signalled
/// through a pidfd, which names one process for good. A pid still listed after
/// its pidfd was opened belongs to that pidfd's process, if that process is
/// still alive when signalled; a dead one makes the signal fail harmlessly.
pub(super) fn terminate_members(cgroup: &Path, mut reap: impl FnMut()) {
    let opened: Vec<(u32, OwnedFd)> = member_pids(cgroup)
        .into_iter()
        .filter_map(|pid| pidfd_open(pid).map(|fd| (pid, fd)))
        .collect();
    let still_listed = member_pids(cgroup);
    for (pid, fd) in &opened {
        if still_listed.binary_search(pid).is_ok() {
            unsafe {
                libc::syscall(
                    libc::SYS_pidfd_send_signal,
                    fd.as_raw_fd(),
                    libc::SIGTERM,
                    std::ptr::null::<libc::siginfo_t>(),
                    0_u32,
                );
            }
        }
    }
    drop(opened);
    let started = Instant::now();
    while started.elapsed() < super::super::process::TERMINATE_GRACE {
        reap();
        if member_pids(cgroup).is_empty() {
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn open_kill_file(path: &Path) -> io::Result<File> {
    #[cfg(test)]
    if let Some(error) = OPEN_ERROR_FOR_TEST.with(|slot| slot.borrow_mut().take()) {
        return Err(error(path));
    }
    OpenOptions::new().write(true).open(path)
}

#[cfg(test)]
type OpenErrorForTest = Box<dyn FnOnce(&Path) -> io::Error>;

#[cfg(test)]
thread_local! {
    static OPEN_ERROR_FOR_TEST: std::cell::RefCell<Option<OpenErrorForTest>> = const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
pub(crate) fn with_scope_kill_open_error_for_test<T>(
    error: impl FnOnce(&Path) -> io::Error + 'static,
    run: impl FnOnce() -> T,
) -> T {
    struct Restore(Option<OpenErrorForTest>);
    impl Drop for Restore {
        fn drop(&mut self) {
            OPEN_ERROR_FOR_TEST.with(|slot| slot.replace(self.0.take()));
        }
    }
    let _restore = Restore(OPEN_ERROR_FOR_TEST.with(|slot| slot.replace(Some(Box::new(error)))));
    run()
}

fn kill_with(cgroup: &Path, write: impl FnOnce(&mut File) -> io::Result<()>) -> io::Result<()> {
    let existed_before = cgroup.try_exists();
    let (phase, result) = match open_kill_file(&cgroup.join("cgroup.kill")) {
        Ok(mut file) => ("write", write(&mut file)),
        Err(error) => ("open", Err(error)),
    };
    let Err(error) = result else { return Ok(()) };
    let exists_after = cgroup.try_exists();
    // The process-group kill can empty the scope before we reach cgroup.kill.
    // Linux can return ENOENT or ENODEV from either open or write while
    // systemd removes the scope. Accept neither without proof it is gone.
    if matches!(error.raw_os_error(), Some(libc::ENOENT | libc::ENODEV))
        && matches!(exists_after, Ok(false))
    {
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
        for errno in [libc::ENODEV, libc::ENOENT] {
            fs::create_dir(&scope).unwrap();
            fs::write(scope.join("cgroup.kill"), "0").unwrap();
            kill_with(&scope, |_| {
                fs::remove_dir_all(&scope).unwrap();
                Err(io::Error::from_raw_os_error(errno))
            })
            .unwrap();
        }
    }

    #[test]
    fn task_scope_kill_requires_directory_absence_for_collection() {
        let root = tempfile::tempdir().unwrap();
        fs::write(root.path().join("cgroup.kill"), "0").unwrap();
        for errno in [libc::ENODEV, libc::ENOENT] {
            let error = with_scope_kill_open_error_for_test(
                move |_| io::Error::from_raw_os_error(errno),
                || kill(root.path()).unwrap_err(),
            );
            assert_eq!(error.raw_os_error(), Some(errno));
            let error =
                kill_with(root.path(), |_| Err(io::Error::from_raw_os_error(errno))).unwrap_err();
            assert_eq!(error.raw_os_error(), Some(errno));
        }
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
