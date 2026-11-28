//! `bash_write`'s path to a PTY task's terminal input.

use super::*;
use crate::bash_background::pty_input::{PtyWriteError, WriteStop, INPUT_DEADLINE};

impl BgTaskRegistry {
    /// Send `input` to the terminal of PTY task `task_id`, returning the
    /// bytes written. Errors are the codes `task_not_found`, `task_not_pty`,
    /// `task_exited` and `input_timeout`, or a write failure's message.
    pub fn write_pty(
        &self,
        task_id: &str,
        session_id: &str,
        input: &[u8],
    ) -> Result<usize, String> {
        self.write_pty_input(task_id, session_id, input)
            .map_err(PtyWriteError::into_code)
    }

    /// Send `input` to the terminal of PTY task `task_id`. Waits at most
    /// [`INPUT_DEADLINE`] for the program to read it, and stops once the task
    /// stops (a kill, say).
    #[doc(hidden)]
    pub fn write_pty_input(
        &self,
        task_id: &str,
        session_id: &str,
        input: &[u8],
    ) -> Result<usize, PtyWriteError> {
        let task = self
            .task_for_session(task_id, session_id)
            .ok_or(PtyWriteError::TaskNotFound)?;
        let terminal = {
            let state = task
                .lock_state()
                .map_err(|_| PtyWriteError::Failed("background task lock poisoned".into()))?;
            if state.metadata.mode != BgMode::Pty {
                return Err(PtyWriteError::TaskNotPty);
            }
            if state.metadata.status.is_terminal() {
                return Err(PtyWriteError::TaskExited);
            }
            match &state.runtime {
                TaskRuntime::Pty(Some(runtime)) => Arc::clone(&runtime.input),
                TaskRuntime::Pty(None) => return Err(PtyWriteError::TaskExited),
                TaskRuntime::Piped(_) => return Err(PtyWriteError::TaskNotPty),
            }
        };
        let stopped = || {
            !task.lock_state().is_ok_and(|state| {
                !state.metadata.status.is_terminal()
                    && matches!(&state.runtime, TaskRuntime::Pty(Some(runtime))
                        if !runtime.was_killed.load(Ordering::SeqCst))
            })
        };
        #[cfg(any(test, all(unix, feature = "test-support")))]
        crate::bash_background::pty_process::note_pty_write_started_for_test(task_id);
        let result = terminal.write(input, Instant::now() + INPUT_DEADLINE, &stopped);
        // Record how much typed input reached the terminal, so a task that
        // later seems to ignore its input can be told apart from one that
        // never got it. Only sizes are logged: the input can contain secrets.
        match result {
            Ok(()) => {
                crate::slog_info!("bash_write task={task_id} bytes={}", input.len());
                Ok(input.len())
            }
            Err(WriteStop::TimedOut { written }) => {
                crate::slog_info!(
                    "bash_write task={task_id} bytes={written} of {} before the input deadline",
                    input.len()
                );
                Err(PtyWriteError::InputTimeout {
                    written,
                    pending: input.len() - written,
                })
            }
            Err(WriteStop::Stopped) => Err(PtyWriteError::TaskExited),
            Err(WriteStop::Failed(message)) => Err(PtyWriteError::Failed(message)),
        }
    }
}
