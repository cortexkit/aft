//! Input to a PTY task's terminal.
//!
//! A pseudoterminal holds only a few KiB of input its program has not read.
//! A blocking write of more waits until the program reads, which may be never:
//! the program may be busy, waiting on something else, or gone while another
//! process still holds the terminal. A writer blocked that way also kept the
//! writer lock, so the reader could not answer a cursor-position query, which
//! stopped it draining output and could stall the program for good.
//!
//! So on Unix the PTY master is non-blocking. Agent input waits for room
//! without holding the writer lock, up to a deadline, and stops early once the
//! task stops. The writer lock is held only for single non-blocking writes. A
//! cursor-position reply that does not fit is queued and written as soon as
//! the terminal has room, ahead of the agent input still to be sent. Windows
//! ConPTY input keeps its blocking writes.

use std::io::{self, Write};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, PoisonError};
use std::time::{Duration, Instant};

/// How long one agent write waits for the program to read its input.
pub(crate) const INPUT_DEADLINE: Duration = Duration::from_secs(5);

/// Longest single wait for room in the terminal, so a task that stops during
/// a write is noticed promptly.
#[cfg(unix)]
const ROOM_WAIT_SLICE: Duration = Duration::from_millis(50);

/// The answer to a cursor-position query (DSR, `ESC[6n`). AFT owns both ends
/// of the terminal and reports a conservative position of row 1, column 1.
const CURSOR_POSITION_REPLY: &[u8] = b"\x1b[1;1R";

pub(crate) type PtyWriter = Arc<Mutex<Box<dyn Write + Send>>>;

/// Why `bash_write` could not send its whole input to a task's terminal.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum PtyWriteError {
    TaskNotFound,
    TaskNotPty,
    /// The task was not running, or stopped during the write.
    TaskExited,
    /// The program did not read its input within [`INPUT_DEADLINE`].
    InputTimeout {
        written: usize,
        pending: usize,
    },
    Failed(String),
}

impl PtyWriteError {
    /// The error code a caller expecting `write_pty`'s plain errors sees.
    pub(crate) fn into_code(self) -> String {
        match self {
            Self::TaskNotFound => "task_not_found".to_string(),
            Self::TaskNotPty => "task_not_pty".to_string(),
            Self::TaskExited => "task_exited".to_string(),
            Self::InputTimeout { .. } => "input_timeout".to_string(),
            Self::Failed(message) => message,
        }
    }
}

/// Why an agent write stopped before all of its input was written.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum WriteStop {
    /// The task stopped running.
    Stopped,
    /// The program did not read its input before the deadline; `written`
    /// bytes were sent.
    TimedOut {
        written: usize,
    },
    Failed(String),
}

pub(crate) struct PtyInput {
    writer: PtyWriter,
    /// The PTY master, set non-blocking, for waiting until it is readable or
    /// writable. `None` keeps blocking writes (Windows, or a master that could
    /// not be made non-blocking).
    #[cfg(unix)]
    master: Option<std::os::fd::OwnedFd>,
    /// Cursor-position replies owed to the program.
    replies_owed: AtomicUsize,
    /// Bytes of the first owed reply already written. Changed only while the
    /// writer lock is held.
    reply_written: AtomicUsize,
    /// Whether an agent write is in progress: whole writes take turns, so two
    /// writes never interleave their bytes.
    writing: Mutex<bool>,
    turn_free: Condvar,
}

impl PtyInput {
    /// Input written with blocking writes and no deadline.
    pub(crate) fn blocking(writer: Box<dyn Write + Send>) -> Self {
        Self {
            writer: Arc::new(Mutex::new(writer)),
            #[cfg(unix)]
            master: None,
            replies_owed: AtomicUsize::new(0),
            reply_written: AtomicUsize::new(0),
            writing: Mutex::new(false),
            turn_free: Condvar::new(),
        }
    }

    /// Input for the PTY whose master is `master_fd`. The master's file
    /// description is shared with `writer` and the output reader, so making it
    /// non-blocking here makes all of them non-blocking.
    #[cfg(unix)]
    pub(crate) fn for_master(
        writer: Box<dyn Write + Send>,
        master_fd: Option<std::os::fd::RawFd>,
    ) -> Self {
        let mut input = Self::blocking(writer);
        input.master = master_fd.and_then(|fd| match non_blocking_duplicate(fd) {
            Ok(master) => Some(master),
            Err(error) => {
                crate::slog_warn!("PTY input stays blocking: {error}");
                None
            }
        });
        input
    }

    pub(crate) fn writer(&self) -> &PtyWriter {
        &self.writer
    }

    /// Whether reads and writes on this terminal can return `WouldBlock`.
    pub(crate) fn non_blocking(&self) -> bool {
        #[cfg(unix)]
        {
            self.master.is_some()
        }
        #[cfg(not(unix))]
        {
            false
        }
    }

    /// Answer a cursor-position query the reader saw. Never waits for an
    /// agent write: a reply that cannot be written now is queued.
    pub(crate) fn reply_cursor_position(&self) {
        if !self.non_blocking() {
            if let Ok(mut writer) = self.writer.lock() {
                let _ = writer.write_all(CURSOR_POSITION_REPLY);
                let _ = writer.flush();
            }
            return;
        }
        self.replies_owed.fetch_add(1, Ordering::SeqCst);
        self.try_send_replies();
    }

    /// Write owed replies if the writer is free and the terminal has room.
    pub(crate) fn try_send_replies(&self) {
        if self.replies_owed.load(Ordering::SeqCst) == 0 {
            return;
        }
        if let Ok(mut writer) = self.writer.try_lock() {
            let _ = self.send_replies(&mut writer);
        }
    }

    /// Wait until the terminal has output to read (or has hung up). While
    /// replies are owed, also wakes when there is room to send them and sends
    /// what fits.
    pub(crate) fn wait_for_output(&self) {
        #[cfg(unix)]
        if let Some(master) = &self.master {
            let owed = self.replies_owed.load(Ordering::SeqCst) > 0;
            let events = if owed {
                libc::POLLIN | libc::POLLOUT
            } else {
                libc::POLLIN
            };
            poll(master, events, owed.then_some(ROOM_WAIT_SLICE));
            self.try_send_replies();
        }
    }

    /// Write owed replies, in order. Returns whether all were written; the
    /// caller holds the writer lock.
    fn send_replies(&self, writer: &mut Box<dyn Write + Send>) -> io::Result<bool> {
        while self.replies_owed.load(Ordering::SeqCst) > 0 {
            let sent = self.reply_written.load(Ordering::SeqCst);
            match writer.write(&CURSOR_POSITION_REPLY[sent..]) {
                Ok(0) => return Ok(false),
                Ok(n) if sent + n == CURSOR_POSITION_REPLY.len() => {
                    self.reply_written.store(0, Ordering::SeqCst);
                    self.replies_owed.fetch_sub(1, Ordering::SeqCst);
                }
                Ok(n) => self.reply_written.store(sent + n, Ordering::SeqCst),
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => return Ok(false),
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(error) => return Err(error),
            }
        }
        Ok(true)
    }

    /// Write all of `input` as one agent write: after any earlier agent write
    /// and before any later one. Waits for room at most until `deadline` and
    /// stops once `stopped` reports the task no longer runs.
    pub(crate) fn write(
        &self,
        input: &[u8],
        deadline: Instant,
        stopped: &dyn Fn() -> bool,
    ) -> Result<(), WriteStop> {
        let _turn = self.take_turn(deadline, stopped)?;
        #[cfg(unix)]
        if let Some(master) = &self.master {
            return self.write_non_blocking(master, input, deadline, stopped);
        }
        let mut writer = self.writer.lock().map_err(|_| poisoned())?;
        writer
            .write_all(input)
            .map_err(|error| WriteStop::Failed(format!("failed to write to PTY: {error}")))?;
        writer
            .flush()
            .map_err(|error| WriteStop::Failed(format!("failed to flush PTY writer: {error}")))
    }

    #[cfg(unix)]
    fn write_non_blocking(
        &self,
        master: &std::os::fd::OwnedFd,
        input: &[u8],
        deadline: Instant,
        stopped: &dyn Fn() -> bool,
    ) -> Result<(), WriteStop> {
        let mut written = 0;
        loop {
            if stopped() {
                return Err(WriteStop::Stopped);
            }
            {
                let mut writer = self.writer.lock().map_err(|_| poisoned())?;
                let failed = |error: io::Error| {
                    WriteStop::Failed(format!("failed to write to PTY: {error}"))
                };
                // Replies the program is waiting for go ahead of input it has
                // not been sent yet, as a terminal would answer them.
                if self.send_replies(&mut writer).map_err(failed)? {
                    while written < input.len() {
                        match writer.write(&input[written..]) {
                            Ok(0) => break,
                            Ok(n) => written += n,
                            Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
                            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                            Err(error) => return Err(failed(error)),
                        }
                    }
                }
                if written == input.len() {
                    return writer.flush().map_err(|error| {
                        WriteStop::Failed(format!("failed to flush PTY writer: {error}"))
                    });
                }
            }
            let now = Instant::now();
            if now >= deadline {
                return Err(WriteStop::TimedOut { written });
            }
            poll(
                master,
                libc::POLLOUT,
                Some((deadline - now).min(ROOM_WAIT_SLICE)),
            );
        }
    }

    /// Wait for earlier agent writes to finish, then hold the turn until the
    /// returned guard is dropped.
    fn take_turn(
        &self,
        deadline: Instant,
        stopped: &dyn Fn() -> bool,
    ) -> Result<TurnGuard<'_>, WriteStop> {
        let mut writing = self.writing.lock().unwrap_or_else(PoisonError::into_inner);
        while *writing {
            if stopped() {
                return Err(WriteStop::Stopped);
            }
            let now = Instant::now();
            if now >= deadline {
                return Err(WriteStop::TimedOut { written: 0 });
            }
            writing = self
                .turn_free
                .wait_timeout(writing, (deadline - now).min(Duration::from_millis(50)))
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
        *writing = true;
        Ok(TurnGuard(self))
    }
}

struct TurnGuard<'a>(&'a PtyInput);

impl Drop for TurnGuard<'_> {
    fn drop(&mut self) {
        *self
            .0
            .writing
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = false;
        self.0.turn_free.notify_one();
    }
}

fn poisoned() -> WriteStop {
    WriteStop::Failed("PTY writer lock poisoned".to_string())
}

/// A non-blocking duplicate of `fd`. The flag is on the file description that
/// every duplicate of the master shares.
#[cfg(unix)]
fn non_blocking_duplicate(fd: std::os::fd::RawFd) -> io::Result<std::os::fd::OwnedFd> {
    use std::os::fd::{AsRawFd, BorrowedFd};
    // SAFETY: the caller's master stays open for this call.
    let duplicate = unsafe { BorrowedFd::borrow_raw(fd) }.try_clone_to_owned()?;
    let raw = duplicate.as_raw_fd();
    // SAFETY: `raw` is open and owned by `duplicate`.
    let flags = unsafe { libc::fcntl(raw, libc::F_GETFL) };
    if flags < 0 || unsafe { libc::fcntl(raw, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(duplicate)
}

/// Wait until `fd` reports one of `events` (or hangs up), at most `timeout`
/// when given.
#[cfg(unix)]
fn poll(fd: &std::os::fd::OwnedFd, events: libc::c_short, timeout: Option<Duration>) {
    use std::os::fd::AsRawFd;
    let mut request = libc::pollfd {
        fd: fd.as_raw_fd(),
        events,
        revents: 0,
    };
    // Round up so a wait under a millisecond does not become a busy loop.
    let timeout_ms = timeout.map_or(-1, |timeout| {
        timeout
            .as_micros()
            .div_ceil(1000)
            .clamp(1, i32::MAX as u128) as libc::c_int
    });
    // SAFETY: `request` is one valid pollfd for the duration of the call.
    unsafe {
        libc::poll(&mut request, 1, timeout_ms);
    }
}
