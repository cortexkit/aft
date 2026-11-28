use super::*;

/// Reconcile an unloaded task after the sweep has ruled out a live owner.
pub(super) fn recover(
    metadata: &PersistedTask,
    paths: &TaskPaths,
    past_modification_grace: bool,
    now_ms: u64,
) -> Option<PersistedTask> {
    match read_exit_marker(paths).ok()? {
        Some(marker) => {
            let finished_at = open_task_artifact(paths, TaskArtifact::Exit)
                .and_then(|artifact| artifact.try_clone_file())
                .and_then(|file| file.metadata()?.modified())
                .ok()
                .and_then(|at| at.duration_since(UNIX_EPOCH).ok())
                .map(|age| age.as_millis().min(u128::from(u64::MAX)) as u64)
                .unwrap_or_else(unix_millis)
                .max(metadata.started_at)
                .min(unix_millis());
            let mut terminal = terminal_metadata_from_marker(metadata.clone(), marker, None);
            terminal.finished_at = Some(finished_at);
            terminal.duration_ms = Some(finished_at.saturating_sub(terminal.started_at));
            Some(terminal)
        }
        None if past_modification_grace
            && now_ms.saturating_sub(metadata.started_at)
                >= PERSISTED_GC_GRACE.as_millis() as u64
            && replay_liveness::recorded_local_task_is_gone(metadata) =>
        {
            let mut terminal = metadata.clone();
            let reason = restart_fate_unknown_reason(metadata, paths);
            terminal.mark_terminal(BgTaskStatus::FateUnknown, None, Some(reason.clone()));
            // Keep the result deliverable while leaving the unobserved exit artifact untouched.
            terminal.completion_delivered |= metadata.completion_delivered;
            terminal.exit_marker_unavailable = Some(reason);
            Some(terminal)
        }
        None => None,
    }
}
