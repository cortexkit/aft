use super::*;
use crate::bash_background::buffer::{running_disk_limit_bytes, OUTPUT_LIMIT_REASON};

#[derive(Default)]
pub(in crate::bash_background) struct KillStateSave(Option<String>);

impl KillStateSave {
    pub(in crate::bash_background) fn record(
        &mut self,
        task: &BgTask,
        result: std::io::Result<()>,
        terminal: bool,
    ) {
        if let Err(error) = result {
            if terminal {
                task.terminal_persist_dirty.store(true, Ordering::SeqCst);
            }
            crate::slog_warn!("failed to save kill state for {}: {error}", task.task_id);
            self.0.get_or_insert_with(|| {
                format!("The command was stopped, but AFT could not save its state: {error}.")
            });
        }
    }

    pub(in crate::bash_background) fn into_warning(self) -> Option<String> {
        self.0
    }
}

impl BgTaskRegistry {
    pub(super) fn pty_kill_observed(task: &BgTask) -> bool {
        task.lock_state().is_ok_and(|state| {
            matches!(&state.runtime, TaskRuntime::Pty(Some(pty))
                if pty.was_killed.load(Ordering::SeqCst) && pty.exit_observed.load(Ordering::SeqCst))
        })
    }

    pub(in crate::bash_background) fn output_limit_exceeded(&self, task: &BgTask) -> bool {
        let Ok(mut state) = task.lock_state() else {
            return false;
        };
        if state.metadata.status != BgTaskStatus::Running || state.metadata.remote.is_some() {
            return false;
        }
        let limit = running_disk_limit_bytes(&task.paths.session_dir);
        if state.buffer.running_disk_bytes() <= limit {
            return false;
        }
        state.pending_terminal_override = Some(BgTaskStatus::Failed);
        state.metadata.status_reason = Some(OUTPUT_LIMIT_REASON.into());
        true
    }

    pub(in crate::bash_background) fn kill_for_output(&self, task: &BgTask) {
        let _ = self.kill_with_status_reason(
            &task.task_id,
            &task.session_id,
            BgTaskStatus::Failed,
            Some(OUTPUT_LIMIT_REASON.into()),
        );
    }

    /// Re-take the state lock after a PTY kill's signal and close the master.
    /// Returns whether this call published the terminal state.
    pub(super) fn finish_pty_kill(
        &self,
        task: &Arc<BgTask>,
        terminal_status: BgTaskStatus,
        reason: Option<String>,
        resource_kill: bool,
        #[cfg_attr(not(windows), allow(unused_variables))] saves: &mut KillStateSave,
    ) -> Result<bool, String> {
        #[cfg_attr(not(windows), allow(unused_mut, unused_variables))]
        let mut db = DeferredDbWrites::new(self, task);
        let mut state = task
            .lock_state()
            .map_err(|_| "background task lock poisoned".to_string())?;
        state.kill_in_flight = false;
        task.kill_settled.notify_all();

        // On Unix the PTY waiter writes the exit marker and the watchdog
        // publishes the terminal state from it. Windows ConPTY gives no such
        // signal, so the kill publishes it here.
        #[cfg(windows)]
        if !state.metadata.status.is_terminal() {
            let resource_reason = pending_resource_stop_reason(&state);
            let resource_kill = resource_kill || resource_reason.is_some();
            let reason = resource_reason.or(reason);
            let default_status = if terminal_status == BgTaskStatus::TimedOut {
                BgTaskStatus::TimedOut
            } else {
                BgTaskStatus::Killed
            };
            let target_status = state
                .pending_terminal_override
                .take()
                .unwrap_or(default_status);
            if !task.paths.exit.exists() {
                saves.record(task, write_kill_marker_if_absent(&task.paths), false);
            }

            let exit_code = forced_terminal_exit_code(&target_status, resource_kill);
            state
                .metadata
                .mark_terminal(target_status, exit_code, reason.clone());

            state.pending_terminal_override = None;
            task.mark_terminal_now();
            if let TaskRuntime::Pty(runtime) = &mut state.runtime {
                *runtime = None;
            }
            state.detached = true;
            saves.record(
                task,
                self.persist_task_locked(task, &state.metadata, &mut db),
                true,
            );
            return Ok(true);
        }
        #[cfg(not(windows))]
        let _ = (terminal_status, reason, resource_kill);
        Ok(false)
    }
}
