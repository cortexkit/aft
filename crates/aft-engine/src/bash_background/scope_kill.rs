use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::path::Path;
use std::time::{Duration, Instant};

pub(super) fn kill(cgroup: &Path) -> io::Result<()> {
    kill_with(cgroup, |file| file.write_all(b"1"))
}

/// The processes in a cgroup and every cgroup below it, sorted. A cgroup that
/// is gone lists none; one whose listing cannot be read is an error.
fn member_pids(cgroup: &Path) -> io::Result<Vec<u32>> {
    let mut pids = Vec::new();
    let mut pending = vec![cgroup.to_path_buf()];
    while let Some(directory) = pending.pop() {
        match std::fs::read_to_string(directory.join("cgroup.procs")) {
            Ok(text) => pids.extend(
                text.split_whitespace()
                    .filter_map(|pid| pid.parse::<u32>().ok()),
            ),
            // A cgroup removed while it was being listed has no processes.
            Err(error)
                if error.kind() == io::ErrorKind::NotFound
                    && matches!(directory.try_exists(), Ok(false)) => {}
            Err(error) => return Err(error),
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
    Ok(pids)
}

fn pidfd_open(pid: u32) -> Option<OwnedFd> {
    let pid = libc::pid_t::try_from(pid).ok()?;
    let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) };
    (fd >= 0).then(|| unsafe { OwnedFd::from_raw_fd(fd as i32) })
}

#[cfg(test)]
thread_local! {
    static PIDFDS_UNAVAILABLE_FOR_TEST: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Make this thread behave as if the kernel had no pidfds (before Linux 5.3).
#[cfg(test)]
pub(crate) fn with_pidfds_unavailable_for_test<T>(run: impl FnOnce() -> T) -> T {
    struct Restore;
    impl Drop for Restore {
        fn drop(&mut self) {
            PIDFDS_UNAVAILABLE_FOR_TEST.with(|slot| slot.set(false));
        }
    }
    PIDFDS_UNAVAILABLE_FOR_TEST.with(|slot| slot.set(true));
    let _restore = Restore;
    run()
}

/// Whether this kernel can open pidfds (Linux 5.3 and later).
fn pidfds_supported() -> bool {
    #[cfg(test)]
    if PIDFDS_UNAVAILABLE_FOR_TEST.with(std::cell::Cell::get) {
        return false;
    }
    static SUPPORTED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *SUPPORTED.get_or_init(|| pidfd_open(std::process::id()).is_some())
}

/// Whether the process behind a pidfd has exited (zombies included).
fn pidfd_exited(fd: &OwnedFd) -> bool {
    let mut poll = libc::pollfd {
        fd: fd.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    (unsafe { libc::poll(&mut poll, 1, 0) }) > 0
}

/// Signal every listed process in `cgroup` that has not exited, and return
/// the pidfds signalled; signal 0 only finds them. A listed pid can exit and
/// be given to an unrelated process before it is signalled, so each is
/// signalled through a pidfd, which names one process for good. A pid still
/// listed after its pidfd was opened belongs to that pidfd's process, if that
/// process is still alive when signalled; a dead one makes the signal fail
/// harmlessly. An unreadable listing is an error.
fn signal_members(cgroup: &Path, signal: i32) -> io::Result<Vec<OwnedFd>> {
    let opened: Vec<(u32, OwnedFd)> = member_pids(cgroup)?
        .into_iter()
        .filter_map(|pid| pidfd_open(pid).map(|fd| (pid, fd)))
        .collect();
    let still_listed = member_pids(cgroup)?;
    let mut signalled = Vec::new();
    for (pid, fd) in opened {
        if still_listed.binary_search(&pid).is_ok() && !pidfd_exited(&fd) {
            if signal != 0 {
                unsafe {
                    libc::syscall(
                        libc::SYS_pidfd_send_signal,
                        fd.as_raw_fd(),
                        signal,
                        std::ptr::null::<libc::siginfo_t>(),
                        0_u32,
                    );
                }
            }
            signalled.push(fd);
        }
    }
    Ok(signalled)
}

/// SIGTERM every process in `cgroup`, then wait the termination grace period,
/// ending early once none is left running. Returns false, having sent
/// nothing, when this kernel has no pidfds; the caller then signals the
/// process group instead.
pub(super) fn terminate_members(cgroup: &Path, mut reap: impl FnMut()) -> bool {
    if !pidfds_supported() {
        return false;
    }
    if signal_members(cgroup, libc::SIGTERM).is_err() {
        return true;
    }
    let started = Instant::now();
    while started.elapsed() < super::super::process::TERMINATE_GRACE {
        reap();
        if signal_members(cgroup, 0).map_or(true, |live| live.is_empty()) {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    true
}

/// SIGKILL every process in `cgroup` through pidfds, for a kernel without
/// `cgroup.kill` (before Linux 5.14) or a `cgroup.kill` write that failed.
/// Returns whether every process found has exited; false when the listing
/// cannot be read or there are no pidfds. A process started after a listing
/// is caught by the next round; a few rounds are tried.
pub(super) fn kill_members(cgroup: &Path) -> bool {
    if !pidfds_supported() {
        return false;
    }
    for _ in 0..3 {
        let Ok(signalled) = signal_members(cgroup, libc::SIGKILL) else {
            return false;
        };
        if signalled.is_empty() {
            return true;
        }
        let deadline = Instant::now() + Duration::from_secs(1);
        while !signalled.iter().all(pidfd_exited) {
            if Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }
    signal_members(cgroup, 0).is_ok_and(|live| live.is_empty())
}

/// Whether a process may still be running in `cgroup` or below it. An
/// unreadable listing counts as possibly running.
pub(super) fn has_members(cgroup: &Path) -> bool {
    if pidfds_supported() {
        return signal_members(cgroup, 0).map_or(true, |live| !live.is_empty());
    }
    member_pids(cgroup).map_or(true, |pids| !pids.is_empty())
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
