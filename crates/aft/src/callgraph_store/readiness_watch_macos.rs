//! Nonrecursive cache-directory readiness notifications on macOS.
//!
//! A lease is a hard-linked file that its holder unlinks on release. The
//! directory's vnode reports that entry change directly, without depending on
//! FSEvents delivering a separate item-removal event for the lease.

use std::ffi::CString;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::sync::Arc;
use std::thread::JoinHandle;

pub(crate) struct DirectoryWatch {
    queue: Arc<OwnedFd>,
    worker: Option<JoinHandle<()>>,
}

fn registration(ident: usize, filter: i16, flags: u16, fflags: u32) -> libc::kevent {
    libc::kevent {
        ident,
        filter,
        flags,
        fflags,
        data: 0,
        udata: std::ptr::null_mut(),
    }
}

fn submit(queue: &OwnedFd, changes: &[libc::kevent]) -> io::Result<()> {
    // SAFETY: changes points to initialized records for this live kqueue. No
    // output buffer is requested; the kernel copies the registrations.
    let result = unsafe {
        libc::kevent(
            queue.as_raw_fd(),
            changes.as_ptr(),
            changes.len() as i32,
            std::ptr::null_mut(),
            0,
            std::ptr::null(),
        )
    };
    if result == -1 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

impl DirectoryWatch {
    pub(crate) fn start<F>(dir: &Path, mut handler: F) -> notify::Result<Self>
    where
        F: FnMut(notify::Result<notify::Event>) + Send + 'static,
    {
        let path = CString::new(dir.as_os_str().as_bytes())
            .map_err(|_| notify::Error::generic("cache watch path contains NUL"))?;
        // SAFETY: path is NUL-terminated. O_EVTONLY opens the directory for
        // metadata notifications without requiring access to its contents.
        let directory = unsafe { libc::open(path.as_ptr(), libc::O_EVTONLY | libc::O_CLOEXEC) };
        if directory == -1 {
            return Err(notify::Error::io(io::Error::last_os_error()));
        }
        // SAFETY: open returned a new descriptor, transferred exactly once.
        let directory = unsafe { OwnedFd::from_raw_fd(directory) };
        // SAFETY: kqueue takes no arguments and returns a fresh descriptor.
        let queue = unsafe { libc::kqueue() };
        if queue == -1 {
            return Err(notify::Error::io(io::Error::last_os_error()));
        }
        // SAFETY: kqueue returned a new descriptor, transferred exactly once.
        let queue = Arc::new(unsafe { OwnedFd::from_raw_fd(queue) });
        // SAFETY: the queue descriptor is live; this prevents inheritance by
        // child tools, which could otherwise keep a dropped subscription open.
        if unsafe { libc::fcntl(queue.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC) } == -1 {
            return Err(notify::Error::io(io::Error::last_os_error()));
        }
        submit(
            &queue,
            &[
                registration(
                    directory.as_raw_fd() as usize,
                    libc::EVFILT_VNODE,
                    libc::EV_ADD | libc::EV_CLEAR,
                    libc::NOTE_WRITE | libc::NOTE_DELETE | libc::NOTE_RENAME | libc::NOTE_REVOKE,
                ),
                registration(1, libc::EVFILT_USER, libc::EV_ADD | libc::EV_CLEAR, 0),
            ],
        )
        .map_err(notify::Error::io)?;
        let worker_queue = Arc::clone(&queue);
        let dir = dir.to_path_buf();
        let worker = std::thread::Builder::new()
            .name("aft-readiness-vnode".into())
            .spawn(move || {
                // Keep the watched vnode and its queue alive until the blocking
                // wait ends. Drop signals EVFILT_USER rather than polling a timer.
                let _directory = directory;
                loop {
                    let mut event = std::mem::MaybeUninit::<libc::kevent>::uninit();
                    // SAFETY: the live queue writes at most one initialized record
                    // into event; it is read only when the return value is one.
                    let result = unsafe {
                        libc::kevent(
                            worker_queue.as_raw_fd(),
                            std::ptr::null(),
                            0,
                            event.as_mut_ptr(),
                            1,
                            std::ptr::null(),
                        )
                    };
                    if result == -1 {
                        let error = io::Error::last_os_error();
                        if error.kind() == io::ErrorKind::Interrupted {
                            continue;
                        }
                        handler(Err(notify::Error::io(error)));
                        break;
                    }
                    if result == 0 {
                        continue;
                    }
                    // SAFETY: the successful kevent call initialized this record.
                    let event = unsafe { event.assume_init() };
                    if event.filter == libc::EVFILT_USER {
                        break;
                    }
                    handler(Ok(notify::Event::new(notify::EventKind::Modify(
                        notify::event::ModifyKind::Any,
                    ))
                    .add_path(dir.clone())));
                }
            })
            .map_err(notify::Error::io)?;
        Ok(Self {
            queue,
            worker: Some(worker),
        })
    }
}

impl Drop for DirectoryWatch {
    fn drop(&mut self) {
        // Wake the blocked worker before joining; no directory activity or
        // timeout is required to release the subscription's descriptors.
        if submit(
            &self.queue,
            &[registration(1, libc::EVFILT_USER, 0, libc::NOTE_TRIGGER)],
        )
        .is_ok()
        {
            if let Some(worker) = self.worker.take() {
                let _ = worker.join();
            }
        }
    }
}
