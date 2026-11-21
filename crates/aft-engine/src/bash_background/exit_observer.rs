//! Kernel exit observation without taking ownership of the child handle.
use std::time::Duration;
#[cfg(unix)]
use std::time::Instant;

/// Dropping a foreground wait stops its observer, even if the command continues
/// in the background. The timeout below bounds cancellation, not exit detection:
/// the kernel wait returns immediately when the process exits.
#[doc(hidden)]
pub struct ForegroundExitObserver(std::sync::Arc<std::sync::atomic::AtomicBool>);

impl Drop for ForegroundExitObserver {
    fn drop(&mut self) {
        self.0.store(true, std::sync::atomic::Ordering::Release);
    }
}

pub(crate) fn observe_foreground_exit(
    pid: u32,
    wake: impl FnOnce() + Send + 'static,
) -> std::io::Result<ForegroundExitObserver> {
    let monitor = ProcessExit::open(pid)?;
    let cancelled = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let stopping = cancelled.clone();
    std::thread::Builder::new()
        .name("aft-foreground-exit".into())
        .spawn(move || {
            while !stopping.load(std::sync::atomic::Ordering::Acquire) {
                match monitor.wait(Duration::from_millis(100)) {
                    Ok(false) => continue,
                    Ok(true) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(error) => {
                        log::debug!(
                            "foreground exit observer failed; using poll fallback: {error}"
                        );
                    }
                }
                wake();
                break;
            }
        })?;
    Ok(ForegroundExitObserver(cancelled))
}

#[cfg(unix)]
#[derive(Default)]
pub(crate) struct AdoptedExit {
    initialized: bool,
    exited: bool,
    #[cfg(unix)]
    monitor: Option<ProcessExit>,
    fallback: MarkerBackoff,
}

#[cfg(unix)]
impl AdoptedExit {
    pub(crate) fn marker_due(&mut self, pid: Option<u32>, now: Instant) -> bool {
        if self.exited {
            return true;
        }
        #[cfg(unix)]
        {
            if !self.initialized {
                self.initialized = true;
                self.monitor = pid.and_then(|pid| ProcessExit::open(pid).ok());
            }
            if let Some(monitor) = &self.monitor {
                match monitor.exited() {
                    Ok(exited) => {
                        self.exited = exited;
                        return exited;
                    }
                    Err(_) => self.monitor = None,
                }
            }
        }
        #[cfg(not(unix))]
        let _ = pid;
        self.fallback.due(now)
    }
}

#[cfg(unix)]
#[derive(Default)]
pub(crate) struct MarkerBackoff {
    next: Option<Instant>,
    interval: Option<Duration>,
}

#[cfg(unix)]
impl MarkerBackoff {
    pub(crate) fn due(&mut self, now: Instant) -> bool {
        if self.next.is_some_and(|next| now < next) {
            return false;
        }
        let interval = self.interval.unwrap_or(Duration::from_millis(500));
        self.next = Some(now + interval);
        self.interval = Some((interval * 2).min(Duration::from_secs(5)));
        true
    }
}

#[cfg(unix)]
struct ProcessExit(std::os::fd::OwnedFd);

#[cfg(windows)]
struct ProcessExit(std::os::windows::io::OwnedHandle);

#[cfg(unix)]
impl ProcessExit {
    #[cfg(target_os = "linux")]
    fn open(pid: u32) -> std::io::Result<Self> {
        use std::os::fd::FromRawFd;
        let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid as libc::pid_t, 0) };
        if fd < 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(Self(unsafe {
            std::os::fd::OwnedFd::from_raw_fd(fd as i32)
        }))
    }

    #[cfg(any(
        target_os = "macos",
        target_os = "freebsd",
        target_os = "openbsd",
        target_os = "netbsd",
        target_os = "dragonfly"
    ))]
    fn open(pid: u32) -> std::io::Result<Self> {
        use std::os::fd::{AsRawFd, FromRawFd};
        let fd = unsafe { libc::kqueue() };
        if fd < 0 {
            return Err(std::io::Error::last_os_error());
        }
        let monitor = Self(unsafe { std::os::fd::OwnedFd::from_raw_fd(fd) });
        // kqueue does not set close-on-exec: detached commands must not inherit it.
        if unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) } < 0 {
            return Err(std::io::Error::last_os_error());
        }
        let mut event: libc::kevent = unsafe { std::mem::zeroed() };
        event.ident = pid as _;
        event.filter = libc::EVFILT_PROC;
        event.flags = libc::EV_ADD | libc::EV_ONESHOT;
        event.fflags = libc::NOTE_EXIT;
        let result = unsafe {
            libc::kevent(
                monitor.0.as_raw_fd(),
                &event,
                1,
                std::ptr::null_mut(),
                0,
                std::ptr::null(),
            )
        };
        if result < 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(monitor)
    }

    #[cfg(not(any(
        target_os = "linux",
        target_os = "macos",
        target_os = "freebsd",
        target_os = "openbsd",
        target_os = "netbsd",
        target_os = "dragonfly"
    )))]
    fn open(_pid: u32) -> std::io::Result<Self> {
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "no kernel exit observer",
        ))
    }

    #[cfg(target_os = "linux")]
    fn exited(&self) -> std::io::Result<bool> {
        self.wait(Duration::ZERO)
    }

    #[cfg(target_os = "linux")]
    fn wait(&self, timeout: Duration) -> std::io::Result<bool> {
        use std::os::fd::AsRawFd;
        let mut poll = libc::pollfd {
            fd: self.0.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        let result = unsafe { libc::poll(&mut poll, 1, timeout.as_millis() as i32) };
        if result < 0 {
            return Err(std::io::Error::last_os_error());
        }
        if poll.revents & (libc::POLLERR | libc::POLLNVAL) != 0 {
            return Err(std::io::Error::other("invalid pidfd"));
        }
        Ok(poll.revents & (libc::POLLIN | libc::POLLHUP) != 0)
    }

    #[cfg(any(
        target_os = "macos",
        target_os = "freebsd",
        target_os = "openbsd",
        target_os = "netbsd",
        target_os = "dragonfly"
    ))]
    fn exited(&self) -> std::io::Result<bool> {
        self.wait(Duration::ZERO)
    }

    #[cfg(any(
        target_os = "macos",
        target_os = "freebsd",
        target_os = "openbsd",
        target_os = "netbsd",
        target_os = "dragonfly"
    ))]
    fn wait(&self, duration: Duration) -> std::io::Result<bool> {
        use std::os::fd::AsRawFd;
        let mut event: libc::kevent = unsafe { std::mem::zeroed() };
        let timeout = libc::timespec {
            tv_sec: duration.as_secs() as _,
            tv_nsec: duration.subsec_nanos() as _,
        };
        let result = unsafe {
            libc::kevent(
                self.0.as_raw_fd(),
                std::ptr::null(),
                0,
                &mut event,
                1,
                &timeout,
            )
        };
        if result < 0 {
            return Err(std::io::Error::last_os_error());
        }
        if result > 0 && event.flags & libc::EV_ERROR != 0 {
            return Err(std::io::Error::from_raw_os_error(event.data as i32));
        }
        Ok(result > 0 && event.fflags & libc::NOTE_EXIT != 0)
    }

    #[cfg(not(any(
        target_os = "linux",
        target_os = "macos",
        target_os = "freebsd",
        target_os = "openbsd",
        target_os = "netbsd",
        target_os = "dragonfly"
    )))]
    fn exited(&self) -> std::io::Result<bool> {
        Ok(false)
    }

    #[cfg(not(any(
        target_os = "linux",
        target_os = "macos",
        target_os = "freebsd",
        target_os = "openbsd",
        target_os = "netbsd",
        target_os = "dragonfly"
    )))]
    fn wait(&self, _timeout: Duration) -> std::io::Result<bool> {
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "no kernel exit observer",
        ))
    }
}

#[cfg(windows)]
impl ProcessExit {
    fn open(pid: u32) -> std::io::Result<Self> {
        use std::os::windows::io::FromRawHandle;
        // The Windows process handle needs only the SYNCHRONIZE access right;
        // it is not used to query or terminate the process or reap the child.
        let handle =
            unsafe { windows_sys::Win32::System::Threading::OpenProcess(0x0010_0000, 0, pid) };
        if handle.is_null() {
            return Err(std::io::Error::last_os_error());
        }
        Ok(Self(unsafe {
            std::os::windows::io::OwnedHandle::from_raw_handle(handle)
        }))
    }

    fn wait(&self, timeout: Duration) -> std::io::Result<bool> {
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::Foundation::{WAIT_OBJECT_0, WAIT_TIMEOUT};
        match unsafe {
            windows_sys::Win32::System::Threading::WaitForSingleObject(
                self.0.as_raw_handle(),
                timeout.as_millis() as u32,
            )
        } {
            WAIT_OBJECT_0 => Ok(true),
            WAIT_TIMEOUT => Ok(false),
            _ => Err(std::io::Error::last_os_error()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn foreground_exit_observer_signals_without_reaping() {
        #[cfg(unix)]
        let mut child = std::process::Command::new("/bin/sh")
            .args(["-c", "sleep 0.1; exit 7"])
            .spawn()
            .unwrap();
        #[cfg(windows)]
        let mut child = std::process::Command::new("cmd.exe")
            .args(["/C", "ping -n 2 127.0.0.1 >NUL & exit 7"])
            .spawn()
            .unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        let _observer = observe_foreground_exit(child.id(), move || tx.send(()).unwrap()).unwrap();
        rx.recv_timeout(Duration::from_secs(10))
            .expect("kernel exit wake");
        assert_eq!(child.wait().unwrap().code(), Some(7));
    }

    #[cfg(unix)]
    #[test]
    fn watchdog_cost_marker_fallback_backs_off_to_five_seconds() {
        let mut backoff = MarkerBackoff::default();
        let now = Instant::now();
        let mut probes = 0;
        for tick in 0..120 {
            probes += usize::from(backoff.due(now + Duration::from_millis(tick * 500)));
        }
        assert_eq!(
            probes, 15,
            "long-run fallback must not resolve artifacts 120 times/minute"
        );
    }

    #[cfg(unix)]
    #[test]
    fn watchdog_cost_kernel_observer_reports_exit_without_marker() {
        let mut child = std::process::Command::new("/bin/sleep")
            .arg("30")
            .spawn()
            .unwrap();
        let monitor = ProcessExit::open(child.id()).unwrap();
        assert!(!monitor.exited().unwrap());
        child.kill().unwrap();
        child.wait().unwrap();
        assert!(monitor.exited().unwrap());
    }
}
