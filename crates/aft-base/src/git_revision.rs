//! Deadline- and output-bounded reads from git's object store.

use std::ffi::OsStr;
use std::io::{Read, Write};
use std::path::Path;
use std::process::{Child, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant};

/// Output kept from git's small answers (rev-parse, ls-tree) and stderr.
pub const SMALL_OUTPUT_BYTES: usize = 64 * 1024;

/// Where a git command's standard output goes, and how much of it is allowed.
pub enum Sink {
    Memory { limit: u64 },
    File { file: std::fs::File, limit: u64 },
}

impl Sink {
    pub(crate) fn memory(limit: usize) -> Self {
        Self::Memory {
            limit: limit as u64,
        }
    }

    pub(crate) fn file(file: std::fs::File, limit: u64) -> Self {
        Self::File { file, limit }
    }
}

pub(crate) struct GitOutput {
    pub(crate) status: ExitStatus,
    /// Captured standard output; empty when it was written to a file.
    pub(crate) stdout: Vec<u8>,
    pub(crate) stderr: Vec<u8>,
}

pub enum GitFailure {
    Spawn(std::io::Error),
    Timeout,
    TooLarge,
    Io(std::io::Error),
}

/// Run one read-only git command in `directory` under a deadline, killing it
/// (and anything it started) when the deadline passes or its output grows
/// past the sink's limit.
pub fn bounded_git_bytes(
    directory: &Path,
    args: &[&str],
    deadline: Instant,
    limit: usize,
) -> Option<Vec<u8>> {
    let output = run_git(directory, args, deadline, Sink::memory(limit)).ok()?;
    output.status.success().then_some(output.stdout)
}

pub fn run_git<I, S>(
    directory: &Path,
    args: I,
    deadline: Instant,
    sink: Sink,
) -> Result<GitOutput, GitFailure>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let mut command = crate::git_probe::command();
    command
        .arg("-C")
        .arg(directory)
        .args(args)
        // Paths are file names, never patterns or pathspec magic.
        .env("GIT_LITERAL_PATHSPECS", "1")
        // Never take the index lock for an opportunistic refresh.
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    crate::process_group::isolate_in_process_group(&mut command);
    let mut child = command.spawn().map_err(GitFailure::Spawn)?;
    let stdout = child.stdout.take().expect("piped stdout");
    let stderr = child.stderr.take().expect("piped stderr");
    let overflow = Arc::new(AtomicBool::new(false));
    let (closed_tx, closed_rx) = mpsc::channel::<()>();

    let stdout_closed = closed_tx.clone();
    let stdout_overflow = overflow.clone();
    let stdout_thread = std::thread::spawn(move || {
        let result = copy_limited(stdout, sink, &stdout_overflow);
        let _ = stdout_closed.send(());
        result
    });
    let stderr_thread = std::thread::spawn(move || {
        let mut captured = Vec::new();
        let _ = stderr
            .take(SMALL_OUTPUT_BYTES as u64)
            .read_to_end(&mut captured);
        let _ = closed_tx.send(());
        captured
    });

    let mut open_pipes = 2u8;
    let status = loop {
        if overflow.load(Ordering::Acquire) {
            stop(&mut child);
            return Err(GitFailure::TooLarge);
        }
        let now = Instant::now();
        if now >= deadline {
            stop(&mut child);
            return Err(GitFailure::Timeout);
        }
        let wait = (deadline - now).min(Duration::from_millis(20));
        if open_pipes > 0 {
            match closed_rx.recv_timeout(wait) {
                Ok(()) => open_pipes -= 1,
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => open_pipes = 0,
            }
            continue;
        }
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => std::thread::sleep(wait.min(Duration::from_millis(2))),
            Err(error) => {
                stop(&mut child);
                return Err(GitFailure::Io(error));
            }
        }
    };
    let stdout = stdout_thread
        .join()
        .unwrap_or_else(|_| Err(std::io::Error::other("output reader panicked")))
        .map_err(GitFailure::Io)?;
    if overflow.load(Ordering::Acquire) {
        return Err(GitFailure::TooLarge);
    }
    let stderr = stderr_thread.join().unwrap_or_default();
    Ok(GitOutput {
        status,
        stdout,
        stderr,
    })
}

fn stop(child: &mut Child) {
    crate::process_group::kill_process_tree(child);
    let _ = child.wait();
}

/// Copy `reader` into the sink, stopping (and raising `overflow`) once more
/// than the sink's limit has arrived. Returns the captured bytes for a
/// memory sink.
fn copy_limited(
    mut reader: impl Read,
    sink: Sink,
    overflow: &AtomicBool,
) -> std::io::Result<Vec<u8>> {
    let (mut file, limit) = match sink {
        Sink::Memory { limit } => (None, limit),
        Sink::File { file, limit } => (Some(file), limit),
    };
    let mut captured = Vec::new();
    let mut total = 0u64;
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let read = match reader.read(&mut buffer) {
            Ok(0) => break,
            Ok(read) => read,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        };
        total += read as u64;
        if total > limit {
            overflow.store(true, Ordering::Release);
            return Ok(captured);
        }
        match file.as_mut() {
            Some(file) => file.write_all(&buffer[..read])?,
            None => captured.extend_from_slice(&buffer[..read]),
        }
    }
    if let Some(file) = file.as_mut() {
        file.flush()?;
    }
    Ok(captured)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn memory_sink_keeps_exact_limit_and_flags_overflow() {
        let overflow = AtomicBool::new(false);
        assert_eq!(
            copy_limited(&b"abc"[..], Sink::memory(3), &overflow).unwrap(),
            b"abc"
        );
        assert!(!overflow.load(Ordering::Acquire));
        assert!(copy_limited(&b"abcd"[..], Sink::memory(3), &overflow)
            .unwrap()
            .is_empty());
        assert!(overflow.load(Ordering::Acquire));
    }

    #[test]
    fn file_sink_flushes_bytes_without_capturing_them() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("blob");
        let overflow = AtomicBool::new(false);
        let sink = Sink::file(std::fs::File::create(&path).unwrap(), 3);
        assert!(copy_limited(&b"abc"[..], sink, &overflow)
            .unwrap()
            .is_empty());
        assert_eq!(std::fs::read(path).unwrap(), b"abc");
        assert!(!overflow.load(Ordering::Acquire));
    }
}
