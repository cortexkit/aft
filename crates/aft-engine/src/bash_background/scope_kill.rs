use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::path::Path;
use std::time::{Duration, Instant};

pub(super) fn kill(cgroup: &Path) -> io::Result<()> {
    kill_with(cgroup, |file| file.write_all(b"1"))
}

/// Fault seams for the scope listing and pidfds, for tests on this thread.
#[cfg(test)]
#[derive(Default)]
pub(crate) struct ListingFaults {
    /// Fail every `pidfd_open` with this errno (EMFILE: no descriptors left).
    pub pidfd_errno: Option<i32>,
    /// Fail walking a cgroup's child directories.
    pub walk_error: bool,
    /// Listings to return instead of reading `cgroup.procs`, in order; the
    /// last one repeats.
    pub listings: std::collections::VecDeque<Vec<u32>>,
}

#[cfg(test)]
thread_local! {
    static LISTING_FAULTS: std::cell::RefCell<ListingFaults> = std::cell::RefCell::new(ListingFaults::default());
}

#[cfg(test)]
pub(crate) fn with_listing_faults_for_test<T>(faults: ListingFaults, run: impl FnOnce() -> T) -> T {
    struct Restore;
    impl Drop for Restore {
        fn drop(&mut self) {
            LISTING_FAULTS.with(|slot| *slot.borrow_mut() = ListingFaults::default());
        }
    }
    LISTING_FAULTS.with(|slot| *slot.borrow_mut() = faults);
    let _restore = Restore;
    run()
}

/// The processes in a cgroup and every cgroup below it, sorted. A cgroup that
/// is gone lists none. Any listing or walk that cannot be read is an error:
/// a scope that cannot be listed is never taken for an empty one.
fn member_pids(cgroup: &Path) -> io::Result<Vec<u32>> {
    #[cfg(test)]
    if let Some(listing) = LISTING_FAULTS.with(|slot| {
        let mut faults = slot.borrow_mut();
        match faults.listings.len() {
            0 => None,
            1 => faults.listings.front().cloned(),
            _ => faults.listings.pop_front(),
        }
    }) {
        return Ok(listing);
    }
    let gone = |directory: &Path, error: &io::Error| {
        error.kind() == io::ErrorKind::NotFound && matches!(directory.try_exists(), Ok(false))
    };
    let mut pids = Vec::new();
    let mut pending = vec![cgroup.to_path_buf()];
    while let Some(directory) = pending.pop() {
        match std::fs::read_to_string(directory.join("cgroup.procs")) {
            Ok(text) => pids.extend(
                text.split_whitespace()
                    .filter_map(|pid| pid.parse::<u32>().ok()),
            ),
            // A cgroup removed while it was being listed has no processes.
            Err(error) if gone(&directory, &error) => continue,
            Err(error) => return Err(error),
        }
        #[cfg(test)]
        if LISTING_FAULTS.with(|slot| slot.borrow().walk_error) {
            return Err(io::Error::other("injected cgroup walk failure"));
        }
        let entries = match std::fs::read_dir(&directory) {
            Ok(entries) => entries,
            Err(error) if gone(&directory, &error) => continue,
            Err(error) => return Err(error),
        };
        for entry in entries {
            let entry = entry?;
            if entry.file_type()?.is_dir() {
                pending.push(entry.path());
            }
        }
    }
    pids.sort_unstable();
    pids.dedup();
    Ok(pids)
}

fn pidfd_open(pid: u32) -> io::Result<OwnedFd> {
    #[cfg(test)]
    if let Some(errno) = LISTING_FAULTS.with(|slot| slot.borrow().pidfd_errno) {
        return Err(io::Error::from_raw_os_error(errno));
    }
    let pid = libc::pid_t::try_from(pid).map_err(|_| io::Error::from_raw_os_error(libc::ESRCH))?;
    let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { OwnedFd::from_raw_fd(fd as i32) })
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
    *SUPPORTED.get_or_init(|| {
        !matches!(
            pidfd_open(std::process::id()),
            Err(error) if error.raw_os_error() == Some(libc::ENOSYS)
        )
    })
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

/// The most listings of a scope taken while waiting for two in a row to agree.
const MAX_LISTINGS: usize = 8;

/// Signal every process in `cgroup` that has not exited, and return the
/// pidfds of the ones still running; signal 0 only finds them. A listed pid
/// can exit and be given to an unrelated process before it is signalled, so
/// each is signalled through a pidfd, which names one process for good. A pid
/// still listed after its pidfd was opened belongs to that pidfd's process,
/// if that process is still alive when signalled; a dead one makes the signal
/// fail harmlessly. The scope is listed again until two listings agree, so a
/// process that appears meanwhile is opened and signalled too.
///
/// It fails closed: an unreadable listing, a pidfd that cannot be opened for
/// any reason but the process being gone (EMFILE included), or listings that
/// never settle are errors, never an empty scope.
fn signal_members(cgroup: &Path, signal: i32) -> io::Result<Vec<OwnedFd>> {
    let mut opened = std::collections::BTreeMap::<u32, OwnedFd>::new();
    let mut signalled = std::collections::BTreeSet::<u32>::new();
    let mut previous = member_pids(cgroup)?;
    for _ in 0..MAX_LISTINGS {
        for &pid in &previous {
            if opened.contains_key(&pid) {
                continue;
            }
            match pidfd_open(pid) {
                Ok(fd) => {
                    opened.insert(pid, fd);
                }
                Err(error) if error.raw_os_error() == Some(libc::ESRCH) => {}
                Err(error) => return Err(error),
            }
        }
        let current = member_pids(cgroup)?;
        for pid in &current {
            let Some(fd) = opened.get(pid) else { continue };
            if signal != 0 && !pidfd_exited(fd) && signalled.insert(*pid) {
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
        }
        if current == previous {
            return Ok(opened
                .into_iter()
                .filter(|(pid, fd)| current.binary_search(pid).is_ok() && !pidfd_exited(fd))
                .map(|(_, fd)| fd)
                .collect());
        }
        previous = current;
    }
    Err(io::Error::other("task scope membership kept changing"))
}

/// SIGTERM every process in `cgroup`, then wait the termination grace period,
/// ending early once none is left running. `before` runs right before the
/// SIGTERM. Returns false, having sent nothing, when this kernel has no
/// pidfds; the caller then signals the process group instead.
pub(super) fn terminate_members(
    cgroup: &Path,
    before: impl FnOnce(),
    mut reap: impl FnMut(),
) -> bool {
    if !pidfds_supported() {
        return false;
    }
    before();
    // A failed listing is settled by the caller writing to `cgroup.kill` and
    // checking that the scope is empty.
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
/// Returns whether the scope was confirmed empty; false whenever that cannot
/// be shown. Without pidfds the process group was already terminated, so an
/// empty listing (a scope folder that lingers) is the only confirmation.
pub(super) fn kill_members(cgroup: &Path) -> bool {
    if !pidfds_supported() {
        return member_pids(cgroup).is_ok_and(|pids| pids.is_empty());
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

/// Whether a process may still be running in `cgroup` or below it. A listing
/// that cannot be read counts as possibly running.
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

#[cfg(test)]
mod membership_tests {
    use super::*;

    fn sleeper() -> std::process::Child {
        std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .unwrap()
    }

    /// A process that joins the scope between two listings is opened and
    /// signalled too: the scope is listed again until two listings agree.
    #[test]
    fn task_scope_term_reaches_a_member_that_appears_between_listings() {
        let dir = tempfile::tempdir().unwrap();
        let (mut first, mut late) = (sleeper(), sleeper());
        let faults = ListingFaults {
            listings: [
                vec![first.id()],
                vec![first.id(), late.id()],
                vec![first.id(), late.id()],
            ]
            .into(),
            ..Default::default()
        };
        with_listing_faults_for_test(faults, || {
            let _ = signal_members(dir.path(), libc::SIGTERM).unwrap();
        });
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut stopped = (false, false);
        while Instant::now() < deadline && !(stopped.0 && stopped.1) {
            stopped.0 |= first.try_wait().unwrap().is_some();
            stopped.1 |= late.try_wait().unwrap().is_some();
            std::thread::sleep(Duration::from_millis(10));
        }
        let _ = (first.kill(), late.kill(), first.wait(), late.wait());
        assert_eq!(stopped, (true, true), "a member was left unsignalled");
    }
}
