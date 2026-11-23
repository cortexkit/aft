use super::*;
use crate::bash_background::buffer::{running_disk_limit_bytes, OUTPUT_LIMIT_REASON};

impl TaskSink {
    pub(super) fn drop_output_over_limit(
        &mut self,
        seq: u64,
        stream: Option<OutputStream>,
        offset: u64,
        bytes: u64,
    ) -> io::Result<bool> {
        let progress = self.output_progress;
        let previous = progress.output_limit_dropped_bytes;
        let total = progress
            .stdout_len
            .saturating_add(progress.stderr_len)
            .saturating_add(progress.unknown_len)
            .saturating_add(bytes);
        if bytes == 0
            || (previous == 0 && total <= running_disk_limit_bytes(&self.task.paths.session_dir))
        {
            return Ok(false);
        }
        if progress.last_seq.is_some_and(|last| seq <= last) {
            return Ok(true);
        }

        self.output_progress.output_limit_dropped_bytes = previous.saturating_add(bytes);
        if previous == 0 {
            // Use the same runner cancellation as an explicit task kill.
            // Stop writing now rather than waiting for the runner to answer.
            let mut state = self
                .task
                .lock_state()
                .map_err(|_| io::Error::other("task lock poisoned"))?;
            let remote = state
                .metadata
                .remote
                .as_mut()
                .ok_or_else(|| io::Error::other("remote record absent"))?;
            request_cancel(remote, &self.task.paths, false);
            drop(state);
            self.output_progress.last_seq = Some(seq);
            if let Err(error) = self.commit(|remote| remote.output_received |= stream.is_some()) {
                crate::slog_warn!(
                    "failed to save remote output limit for {}: {error}",
                    self.task.task_id
                );
            }
            let _ = self.registry.inner.wake_tx.try_send(());
        } else if let Err(error) = self.progress(seq, stream, offset) {
            self.output_progress = progress;
            return Err(error);
        }
        Ok(true)
    }
}

pub(super) fn output_limit_reason(
    metadata: &PersistedTask,
    status: &BgTaskStatus,
) -> Option<String> {
    (*status == BgTaskStatus::Killed
        && metadata
            .remote
            .as_ref()
            .is_some_and(|remote| remote.output_limit_dropped_bytes > 0))
    .then(|| OUTPUT_LIMIT_REASON.to_string())
}

impl BgTaskRegistry {
    #[cfg(test)]
    pub(super) fn record_remote_cancel(&self, task: &Arc<BgTask>) -> Result<bool, String> {
        self.record_remote_cancel_owed(task, false)
    }

    /// Record the cancellation intent on a remote task that is not finished
    /// yet, and wake its worker. With `owed`, also record that the runner
    /// is still owed the cancel (`RemoteTask::cancel_pending`), in the same
    /// write: a caller about to mark the task finished before the worker has
    /// sent it must leave that debt in the saved record. Nothing is owed for
    /// a job the runner already reported a terminal for. Returns false when
    /// the task runs locally.
    pub(in crate::bash_background::registry) fn record_remote_cancel_owed(
        &self,
        task: &Arc<BgTask>,
        owed: bool,
    ) -> Result<bool, String> {
        self.record_remote_cancel_with_saves(task, owed, &mut KillStateSave::default())
    }

    pub(super) fn record_remote_cancel_with_saves(
        &self,
        task: &Arc<BgTask>,
        owed: bool,
        saves: &mut KillStateSave,
    ) -> Result<bool, String> {
        {
            let mut db = DeferredDbWrites::new(self, task);
            let mut state = task.state.lock().map_err(|_| "task lock poisoned")?;
            if !state.metadata.is_terminal() {
                let Some(remote) = state.metadata.remote.as_mut() else {
                    return Ok(false);
                };
                request_cancel(remote, &task.paths, owed);
                saves.record(
                    task,
                    self.persist_task_locked(task, &state.metadata, &mut db),
                    false,
                );
            }
        }
        task.remote_cancel_notify.notify_one();
        Ok(true)
    }

    #[cfg(test)]
    pub(super) fn kill_remote_task(&self, task: &Arc<BgTask>) -> Result<BgTaskSnapshot, String> {
        self.kill_remote_task_with_saves(task, KillStateSave::default())
    }

    pub(in crate::bash_background::registry) fn kill_remote_task_with_saves(
        &self,
        task: &Arc<BgTask>,
        mut saves: KillStateSave,
    ) -> Result<BgTaskSnapshot, String> {
        let metadata = task
            .lock_state()
            .map_err(|_| "task lock poisoned")?
            .metadata
            .clone();
        if let Some(job_id) = unknown_remote_cancel_job(&metadata) {
            // The original follower has stopped. Probe cancellation separately,
            // without replacing its saved unknown outcome with an acknowledgement.
            let bound = remote_kill_settle_bound().min(ReattachBudget::until_job_end(&metadata).0);
            let result = std::thread::scope(|scope| {
                scope
                    .spawn(|| {
                        let runtime = tokio::runtime::Builder::new_current_thread()
                            .enable_all()
                            .build()
                            .map_err(|error| error.to_string())?;
                        runtime.block_on(async {
                            tokio::time::timeout(bound, cancel_unknown_remote(&metadata, job_id))
                                .await
                                .map_err(|_| "runner confirmation timed out".to_string())?
                        })
                    })
                    .join()
            })
            .unwrap_or_else(|_| Err("runner cancellation probe stopped".into()));
            let mut snapshot = self.snapshot_with_terminal_cache(task, 8 * 1024);
            snapshot.remote_cancel = Some(result.unwrap_or_else(|error| {
                format!("remote cancellation unconfirmed; outcome is still unknown: {error}")
            }));
            snapshot.kill_state_warning = saves.into_warning();
            return Ok(snapshot);
        }
        if !self.record_remote_cancel_with_saves(task, false, &mut saves)? {
            return self.kill(&task.task_id, &task.session_id);
        }
        let bound = remote_kill_settle_bound();
        let state = task.state.lock().map_err(|_| "task lock poisoned")?;
        let (state, _) = task
            .kill_settled
            .wait_timeout_while(state, bound, |s| !s.metadata.is_terminal())
            .map_err(|_| "task lock poisoned")?;
        if !state.metadata.is_terminal() {
            if state
                .metadata
                .remote
                .as_ref()
                .is_some_and(|remote| remote.job_id.is_none())
            {
                return Err(format!(
                    "remote cancellation pending for task {}; no job id was received, so the job may still run; AFT will cancel it when its job id arrives; inspect bash_status (never rerun)",
                    task.task_id,
                ));
            }
            let job_id = state
                .metadata
                .remote
                .as_ref()
                .and_then(|remote| remote.job_id)
                .unwrap();
            return Err(format!(
                "remote cancellation pending for job {job_id}; the job may still run until the build server confirms it stopped; inspect bash_status (never rerun)"
            ));
        }
        drop(state);
        let mut snapshot = self.snapshot_with_terminal_cache(task, 8 * 1024);
        snapshot.kill_state_warning = saves.into_warning();
        Ok(snapshot)
    }
}

pub(super) fn request_cancel(remote: &mut RemoteTask, paths: &TaskPaths, owed: bool) {
    remote.cancel_requested = true;
    if owed && remote.terminal.is_none() {
        // Index the cancellation debt before saving it in the task record.
        write_owed_cancel_marker(paths);
        remote.cancel_pending = true;
    }
}
