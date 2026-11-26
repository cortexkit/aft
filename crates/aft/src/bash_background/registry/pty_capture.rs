//! Whether a finished PTY task's output capture holds all of its output.

use super::*;

/// The capture file failed before the terminal reached end-of-file.
pub(super) const CAPTURE_FAILED: &str =
    "PTY output may be incomplete: output capture failed before EOF";
/// The terminal had not reached end-of-file when finishing stopped waiting.
pub(super) const DRAIN_EXPIRED: &str =
    "PTY output may be incomplete: output drain deadline expired before EOF";

/// How long finishing a task waits for its PTY reader to reach end-of-file
/// once the process has exited.
const READER_DRAIN_WAIT: Duration = Duration::from_millis(200);

impl BgTaskRegistry {
    /// Wait briefly for the reader of a PTY task whose process exited, then
    /// name the gap if the capture is incomplete: the capture file failed, or
    /// the terminal had not reached end-of-file in time (a background process
    /// still holding it, say). A killed task's output and a ConPTY drain
    /// (`output_drain`) are settled elsewhere, so they report no gap here.
    pub(super) fn pty_capture_gap(&self, task: &BgTask) -> Option<&'static str> {
        let (reader_done, reader_eof) = {
            let state = task.lock_state().ok()?;
            let TaskRuntime::Pty(Some(pty)) = &state.runtime else {
                return None;
            };
            #[cfg(any(windows, test))]
            if pty.output_drain.is_some() {
                return None;
            }
            if pty.was_killed.load(Ordering::SeqCst) {
                return None;
            }
            (Arc::clone(&pty.reader_done), Arc::clone(&pty.reader_eof))
        };
        let deadline = Instant::now() + READER_DRAIN_WAIT;
        while !reader_done.load(Ordering::SeqCst) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        if !reader_done.load(Ordering::SeqCst) {
            Some(DRAIN_EXPIRED)
        } else if !reader_eof.load(Ordering::SeqCst) {
            Some(CAPTURE_FAILED)
        } else {
            None
        }
    }
}
