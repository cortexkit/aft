use super::{SandboxLaunchError, BWRAP};
use std::fs::File;
use std::io::{self, Read};
use std::os::fd::{AsRawFd, FromRawFd, RawFd};
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::process::{self, Command, ExitStatus};

// Landlock's descriptor sweep preserves slots 3 (payload exit) and 4.
pub(super) const INNER_FD: RawFd = 4;
pub(super) const COMPLETE: &[u8; 1] = b"C";
pub(super) const FAILED: &[u8; 1] = b"F";

pub(super) struct Handoff {
    reader: File,
    writer: File,
}

impl Handoff {
    pub(super) fn new() -> io::Result<Self> {
        // Keep the caller's marker writable here, but do not leak inherited
        // descriptors to bwrap. New mount inputs are allocated after this sweep.
        if unsafe {
            libc::syscall(
                libc::SYS_close_range,
                5_u32,
                u32::MAX,
                libc::CLOSE_RANGE_CLOEXEC,
            )
        } < 0
        {
            return Err(io::Error::last_os_error());
        }
        let mut fds = [-1; 2];
        // After wait, the read must not block even if a broken launcher left
        // the writer open in another process.
        if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC | libc::O_NONBLOCK) } < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Self {
            reader: unsafe { File::from_raw_fd(fds[0]) },
            writer: unsafe { File::from_raw_fd(fds[1]) },
        })
    }

    pub(super) fn supervise(self, command: &mut Command) -> Result<ExitStatus, SandboxLaunchError> {
        let Self { mut reader, writer } = self;
        let fd = writer.as_raw_fd();
        // Remap only in the bwrap child: slot 4 in this process may still
        // belong to the engine. The pipe was created before the mount inputs,
        // so its reserved slot cannot overwrite any of those inputs.
        unsafe {
            command.pre_exec(move || {
                let result = if fd == INNER_FD {
                    libc::fcntl(fd, libc::F_SETFD, 0)
                } else {
                    libc::dup2(fd, INNER_FD)
                };
                if result < 0 {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }
        // Inherit stdio, the process group and the controlling terminal. The
        // terminal and AFT's process-group signals still reach the payload.
        let mut child = command.spawn().map_err(|error| {
            SandboxLaunchError::unavailable(format!("cannot execute {BWRAP}: {error}"))
        })?;
        drop(writer);
        let status = child.wait().map_err(|error| {
            SandboxLaunchError::unavailable(format!("cannot wait for bubblewrap: {error}"))
        })?;
        let mut bytes = [0; 2];
        match reader.read(&mut bytes) {
            Ok(1) if &bytes[..1] == COMPLETE => return Ok(status),
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
            Err(error) => {
                return Err(SandboxLaunchError::unavailable(format!(
                    "cannot read bubblewrap setup descriptor: {error}"
                )));
            }
        }
        Err(SandboxLaunchError::unavailable(
            if bytes.contains(&FAILED[0]) {
                format!("bubblewrap inner sandbox setup failed ({status})")
            } else {
                format!("bubblewrap exited before sandbox setup completed ({status})")
            },
        ))
    }
}

pub(super) fn exit_with_status(status: ExitStatus) -> ! {
    if let Some(signal) = status.signal() {
        // Do not send a second signal to the group. Reproduce the child's
        // signal status on the supervisor only, even if it inherited a mask.
        unsafe {
            let mut set = std::mem::zeroed();
            libc::sigemptyset(&mut set);
            libc::sigaddset(&mut set, signal);
            libc::sigprocmask(libc::SIG_UNBLOCK, &set, std::ptr::null_mut());
            libc::signal(signal, libc::SIG_DFL);
            libc::raise(signal);
        }
        process::exit(128 + signal);
    }
    process::exit(status.code().unwrap_or(255))
}
