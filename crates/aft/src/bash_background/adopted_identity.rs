//! Process identity checks for a piped task AFT no longer holds a child
//! handle for, such as one adopted after a restart. AFT is not the parent of
//! that command, so a recorded pid can be reused once the command exits.

use super::*;
use crate::bash_background::process::{recorded_process_liveness, ProcessLiveness};

/// Whether the task's recorded child is gone: not running, or running with a
/// start time after the task began, so the pid now names another process.
pub(super) fn child_gone(metadata: &PersistedTask) -> bool {
    metadata
        .child_pid
        .is_some_and(|pid| !is_recorded_process_alive(pid, metadata.started_at))
}

/// The process group and child pid a kill of the task may signal. With its
/// child handle held, AFT keeps both: an unreaped child keeps its pid. Without
/// one, each id is kept only while it still provably names the task's
/// processes, so a reused pid is never signalled.
pub(super) fn signal_targets(state: &BgTaskState) -> (Option<i32>, Option<u32>) {
    let metadata = &state.metadata;
    if matches!(state.runtime, TaskRuntime::Piped(Some(_))) {
        return (metadata.pgid, metadata.child_pid);
    }
    let verified =
        |pid: u32| recorded_process_liveness(pid, metadata.started_at) == ProcessLiveness::Alive;
    let pgid = metadata.pgid.filter(|&pgid| {
        // A group id can be taken again only by a new process whose own pid
        // equals it. While no process holds the leader's pid, whatever is
        // left in the group is the task's own.
        u32::try_from(pgid).is_ok_and(|leader| !is_process_alive(leader) || verified(leader))
    });
    (pgid, metadata.child_pid.filter(|&pid| verified(pid)))
}
