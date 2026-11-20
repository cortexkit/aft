//! Restart replay of a pseudoterminal task that was still running.

use super::*;

/// Settle a PTY task that was running when AFT stopped and left no exit
/// marker. A pseudoterminal cannot be reattached after a restart, so a command
/// still running (`process_alive`, checked against its recorded start time) is
/// stopped and reported killed. One that is already gone ended with an outcome
/// nobody observed.
pub(super) fn settle_unattached_pty(
    metadata: &mut PersistedTask,
    paths: &TaskPaths,
    process_alive: bool,
) {
    let leader = metadata
        .child_pid
        .or_else(|| metadata.pgid.and_then(|pgid| u32::try_from(pgid).ok()));
    match leader.filter(|_| process_alive) {
        Some(pid) => {
            terminate_pty_group(pid);
            settle_killed_process_group(&metadata.task_id, metadata.pgid);
            metadata.mark_terminal(
                BgTaskStatus::Killed,
                None,
                Some("pty_lost_on_bridge_restart".to_string()),
            );
        }
        None => {
            let reason = restart_fate_unknown_reason(metadata, paths);
            metadata.mark_terminal(BgTaskStatus::FateUnknown, None, Some(reason));
            crate::bash_background::persistence::persist_terminal_exit(paths, metadata);
        }
    }
}
