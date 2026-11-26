//! What a kill may signal, and what AFT says when it cannot show a kill
//! reached the command.
//!
//! A saved process or group number can name an unrelated process once the
//! command's own processes have exited. So a number is signalled only while a
//! check made just before the signal ties it to the command: a child of this
//! process that has not been reaped (its pid, and the group it leads, cannot
//! be reused yet), or a recorded process whose saved start still matches and
//! that is still in the group. On Linux a task scope (a cgroup named for the
//! task) is preferred over any number: it holds every process the command
//! started, including ones that left its process group.

use super::*;
#[cfg(unix)]
use crate::bash_background::process::GroupTermination;

/// Reply note when nothing proved a group was still the command's, so no
/// signal was sent while the group still had members.
pub(super) const NOTHING_SENT_NOTE: &str = "AFT could not confirm the kill: the command's processes could no longer be identified, so no signal was sent.";

/// Reply note when SIGTERM was sent but nothing proved the group was still
/// the command's by the time SIGKILL was due.
#[cfg(unix)]
pub(super) const KILL_WITHHELD_NOTE: &str = "AFT could not confirm the kill: the command's processes could no longer be identified after SIGTERM, so SIGKILL was not sent.";

/// Reply note naming recorded processes that a group signal did not reach.
#[cfg(unix)]
pub(super) fn outside_group_note(count: usize) -> String {
    let processes = if count == 1 {
        "1 process".to_string()
    } else {
        format!("{count} processes")
    };
    format!(
        "The kill reached only the command's original process group; AFT still sees {processes} it recorded for this command running outside that group."
    )
}

/// Join a kill's coverage note to any warning about saving its state.
pub(super) fn join_kill_warnings(
    saved: Option<String>,
    coverage: Option<String>,
) -> Option<String> {
    match (saved, coverage) {
        (Some(saved), Some(coverage)) => Some(format!("{saved}\n{coverage}")),
        (saved, coverage) => saved.or(coverage),
    }
}

/// The processes that can show a numbered process group is still a task's.
pub(super) struct GroupEvidence {
    pgid: i32,
    /// A child of this process that leads the group. While it is unreaped the
    /// group number cannot be given to anyone else.
    #[cfg_attr(not(unix), allow(dead_code))]
    owned_leader: Option<u32>,
    identities: Vec<ProcessIdentity>,
}

impl GroupEvidence {
    /// Evidence from a task's saved record: its child (or the group leader,
    /// whose pid is the group number) at its saved start, and the members a
    /// check of the group recorded.
    pub(super) fn for_task(metadata: &PersistedTask, pgid: i32, owned_leader: Option<u32>) -> Self {
        let leader = u32::try_from(pgid).ok();
        let mut identities: Vec<ProcessIdentity> = metadata
            .child_pid
            .into_iter()
            .chain(leader.filter(|leader| metadata.child_pid != Some(*leader)))
            .filter_map(|pid| metadata.child_identity(pid))
            .collect();
        identities.extend(metadata.group_member_identities.iter().cloned());
        Self {
            pgid,
            owned_leader,
            identities,
        }
    }

    /// Whether, right now, the group number still names the task's group.
    pub(super) fn proven(&self) -> bool {
        #[cfg(unix)]
        if self.owned_leader.is_some_and(|pid| {
            i32::try_from(pid) == Ok(self.pgid) && super::super::process::child_unreaped(pid)
        }) {
            return true;
        }
        self.identities
            .iter()
            .any(|identity| super::super::process::is_running_in_group(identity, self.pgid))
    }

    /// Recorded processes still running but no longer in the group.
    #[cfg(unix)]
    pub(super) fn outside_group_note(&self) -> Option<String> {
        let count = super::super::process::running_outside_group(&self.identities, self.pgid);
        (count > 0).then(|| outside_group_note(count))
    }

    /// Whether the task's recorded leader (the process whose pid is the group
    /// number) is still running in the group with its saved start. Unlike
    /// group proof, an exited leader that is not reaped yet (a zombie) counts
    /// as gone here: it still pins the group number, so it proves the group,
    /// but it can no longer act on a hangup, and a kill that hangs it up would
    /// claim an exit the command made on its own.
    #[cfg(unix)]
    pub(super) fn leader_running(&self) -> bool {
        self.identities.iter().any(|identity| {
            i32::try_from(identity.pid) == Ok(self.pgid)
                && super::super::process::is_running_in_group(identity, self.pgid)
                && !super::super::process::has_exited(identity.pid)
        })
    }

    /// Record who is in the group now, if the group is proven before the
    /// listing and still the task's after it. A recorded member still running
    /// in the group proves it later, because a group number cannot be reused
    /// while the group has members. The listing reads only identities, never
    /// account names, which can stall on a slow directory service. Returns
    /// whether the group was proven.
    #[cfg(unix)]
    pub(super) fn remember_members(&mut self) -> bool {
        if !self.proven() {
            return false;
        }
        let Ok(listed) = super::super::process::process_group_identities(self.pgid) else {
            return true;
        };
        // The listing describes the task's group only if the group is still
        // proven after it was taken: its number could otherwise have been
        // freed and reused while it was listed.
        if self.proven() {
            self.identities.extend(listed);
            return true;
        }
        // The earlier proof is gone, typically because the leader exited
        // during the listing. A reused number is held by a newer process
        // leading a new group; while no process holds it, the members listed
        // are what is left of the task's group and still prove it.
        let number_held = u32::try_from(self.pgid).is_ok_and(is_process_alive);
        if number_held || listed.is_empty() {
            return false;
        }
        self.identities.extend(listed);
        true
    }

    /// TERM the group, wait the grace period, then KILL it, proving the group
    /// is the task's before each signal. Before TERM it also records who is in
    /// the group, so the KILL can still be proven after TERM ends the leader.
    /// Returns the note a kill reply should carry, if any.
    #[cfg(unix)]
    pub(super) fn terminate(&mut self) -> Option<String> {
        self.terminate_with(|| {})
    }

    /// [`Self::terminate`], calling `before_first_signal` once, right before
    /// SIGTERM is sent; it is never called when nothing is sent.
    #[cfg(unix)]
    pub(super) fn terminate_with(
        &mut self,
        mut before_first_signal: impl FnMut(),
    ) -> Option<String> {
        let mut first = true;
        let outcome = super::super::process::terminate_proven_pgid(self.pgid, || {
            if std::mem::take(&mut first) {
                let proven = self.remember_members();
                if proven {
                    before_first_signal();
                }
                proven
            } else {
                self.proven()
            }
        });
        self.coverage_note(outcome)
    }
    #[cfg(unix)]
    fn coverage_note(&self, outcome: GroupTermination) -> Option<String> {
        match outcome {
            GroupTermination::Refused if !super::super::process::group_exited(self.pgid) => {
                Some(NOTHING_SENT_NOTE.to_string())
            }
            GroupTermination::Refused => None,
            GroupTermination::KillWithheld => Some(KILL_WITHHELD_NOTE.to_string()),
            GroupTermination::Signalled => self.outside_group_note(),
        }
    }
}

/// Signal a piped task: SIGTERM to its process group, a grace period that
/// ends early once the whole group exits, then SIGKILL (Unix); `taskkill`
/// on the process tree (Windows). On Unix each group signal is sent only while
/// [`GroupEvidence`] still ties the group to the task. Returns the note a kill
/// reply should carry, if any.
pub(super) fn terminate_piped_task(
    metadata: &PersistedTask,
    pgid: Option<i32>,
    child_pid: Option<u32>,
    reason: &str,
    #[allow(unused_mut)] mut child: Option<&mut Child>,
    before_first_signal: &mut dyn FnMut(),
) -> Option<String> {
    let task_id = metadata.task_id.as_str();
    #[cfg(all(test, unix))]
    if TERM_ONLY_KILL_TASKS
        .get()
        .is_some_and(|tasks| tasks.lock().unwrap().remove(task_id))
    {
        if let Some(pgid) = pgid {
            before_first_signal();
            unsafe {
                libc::killpg(pgid, libc::SIGTERM);
            }
        }
        return None;
    }
    #[cfg(unix)]
    {
        let pgid = pgid?;
        log_bash_kill(task_id, child_pid, Some(pgid), reason);
        // The child is reaped only after the KILL, so it keeps the group
        // number pinned to this task for the whole termination.
        let owned_leader = child.as_ref().map(|child| child.id());
        GroupEvidence::for_task(metadata, pgid, owned_leader).terminate_with(before_first_signal)
    }
    #[cfg(windows)]
    {
        let _ = pgid;
        if let Some(pid) = child.as_ref().map(|child| child.id()).or(child_pid) {
            log_bash_kill(task_id, Some(pid), None, reason);
            before_first_signal();
        }
        if let Some(child) = child {
            super::super::process::terminate_process(child);
        } else if let Some(pid) = child_pid {
            terminate_pid(pid);
        }
        None
    }
}

/// Signal a PTY task's process group, whose number is its session leader's
/// pid. `evidence` is what was gathered before the leader was hung up, while
/// it was known to be alive; without it the evidence is gathered now. Returns
/// the note a kill reply should carry, if any.
pub(super) fn terminate_pty_task(
    metadata: &PersistedTask,
    pid: u32,
    #[cfg_attr(windows, allow(unused_variables))] evidence: Option<GroupEvidence>,
    before_first_signal: &mut dyn FnMut(),
) -> Option<String> {
    #[cfg(unix)]
    {
        let pgid = i32::try_from(pid).ok()?;
        evidence
            .unwrap_or_else(|| GroupEvidence::for_task(metadata, pgid, None))
            .terminate_with(before_first_signal)
    }
    #[cfg(windows)]
    {
        let _ = metadata;
        before_first_signal();
        terminate_pid(pid);
        None
    }
}

/// SIGTERM the processes in a task's scope and wait the grace period; the
/// caller then ends the scope with its kill switch. On a kernel without pidfds
/// the scope's processes cannot be signalled one by one, so the task's process
/// group is terminated instead, re-proven before each signal. Returns the note
/// a kill reply should carry, if any.
#[cfg(target_os = "linux")]
fn terminate_scope_or_group(
    scope: &super::super::task_scope::ScopeHandle,
    group: impl FnOnce() -> Option<GroupEvidence>,
    before_first_signal: &mut dyn FnMut(),
    reap: impl FnMut(),
) -> Option<String> {
    if scope.terminate_members(&mut *before_first_signal, reap) {
        return None;
    }
    group()?.terminate_with(before_first_signal)
}

/// Why a live kill's outcome is unknown, when it is: its scope could not be
/// ended, nothing could be sent while the command still had processes, or
/// SIGKILL was withheld while survivors could not be confirmed.
pub(super) fn unconfirmed_kill_reason(
    scope_result: &Result<(), String>,
    note: Option<&str>,
) -> Option<String> {
    match scope_result {
        Err(error) => Some(format!("Task outcome is unknown: {error}.")),
        Ok(()) => note
            .filter(|note| note_is_unconfirmed(note))
            .map(str::to_string),
    }
}

/// Whether a kill note says the kill was not confirmed.
fn note_is_unconfirmed(note: &str) -> bool {
    #[cfg(unix)]
    if note == KILL_WITHHELD_NOTE {
        return true;
    }
    note == NOTHING_SENT_NOTE
}

/// Whether something still answers under the command's saved pid or process
/// group, so that a kill AFT could not deliver may have left it running.
fn command_may_still_run(metadata: &PersistedTask) -> bool {
    let leader = metadata
        .child_pid
        .or_else(|| metadata.pgid.and_then(|pgid| u32::try_from(pgid).ok()));
    if leader.is_some_and(is_process_alive) {
        return true;
    }
    #[cfg(unix)]
    if metadata
        .pgid
        .is_some_and(|pgid| !super::super::process::group_exited(pgid))
    {
        return true;
    }
    false
}

/// A restarted AFT's reason for a command it could not stop: nothing could
/// be tied to it, so no signal was sent.
fn undelivered_kill_reason(metadata: &PersistedTask, paths: &TaskPaths) -> String {
    let output = match metadata.mode {
        BgMode::Pipes => &paths.stdout,
        BgMode::Pty => &paths.pty,
    };
    format!(
        "task {}: daemon restarted and AFT could not deliver the kill; the command's processes may still be running, last output at {}",
        metadata.task_id,
        output.display()
    )
}

/// What a restart may signal for a command whose kill or terminal was
/// interrupted and that left no exit marker.
#[derive(Debug)]
pub(super) enum ReplayKill {
    /// The task's own Linux scope, found under the name AFT gave it.
    #[cfg(target_os = "linux")]
    Scope(super::super::task_scope::ScopeHandle),
    /// The task was placed in a scope that could not be located.
    #[cfg(target_os = "linux")]
    ScopeUnconfirmed(String),
    /// The saved child, still running with its exact saved start.
    Process(u32),
    /// Nothing can be tied to the command; send nothing.
    Unverified,
}

/// The saved child pid to signal, if `matches` confirms the saved identity.
/// Retention liveness (a start no later than the task plus a margin, or an
/// unreadable start) never permits a signal: only an exact match does.
pub(super) fn replay_process(
    metadata: &PersistedTask,
    matches: impl FnOnce(&ProcessIdentity) -> bool,
) -> ReplayKill {
    let pid = metadata
        .child_pid
        .or_else(|| metadata.pgid.and_then(|pgid| u32::try_from(pgid).ok()));
    match pid.and_then(|pid| metadata.child_identity(pid)) {
        Some(saved) if matches(&saved) => ReplayKill::Process(saved.pid),
        _ => ReplayKill::Unverified,
    }
}

impl BgTaskRegistry {
    /// The task scope a kill should go through, when the task has one. A
    /// scope is found by the unit name AFT gave this task, so it can hold only
    /// processes AFT started for it. `Ok(None)` means the task has no scope
    /// (or its scope is gone); an error means a scoped task's scope could not
    /// be located.
    #[cfg(target_os = "linux")]
    pub(super) fn kill_scope_handle(
        &self,
        metadata: &PersistedTask,
    ) -> Result<Option<super::super::task_scope::ScopeHandle>, String> {
        if let Some(handle) = self.inner.task_scopes.get(&metadata.task_id) {
            return Ok(Some(handle));
        }
        let policy = metadata.memory_policy.as_ref().filter(|policy| {
            policy.kind == super::super::memory_limit::MemoryPolicy::TASK_SCOPE_MEMORY_KIND
        });
        if metadata.task_scope_runtime_ms.is_none() && policy.is_none() {
            return Ok(None);
        }
        self.inner.task_scopes.resolve_handle(
            &metadata.task_id,
            policy.and_then(|policy| policy.cgroup.as_deref()),
        )
    }

    /// What a restart may signal for this task.
    pub(super) fn replay_kill(&self, metadata: &PersistedTask) -> ReplayKill {
        #[cfg(target_os = "linux")]
        match self.kill_scope_handle(metadata) {
            Ok(Some(scope)) => return ReplayKill::Scope(scope),
            Ok(None) => {}
            Err(error) => return ReplayKill::ScopeUnconfirmed(error),
        }
        replay_process(metadata, |saved| {
            #[cfg(unix)]
            {
                metadata
                    .pgid
                    .is_some_and(|pgid| super::super::process::is_running_in_group(saved, pgid))
            }
            #[cfg(windows)]
            {
                ProcessIdentity::of(saved.pid).is_some_and(|current| saved.same_process(&current))
            }
        })
    }

    /// Finish a kill that was in flight when AFT stopped, then record its
    /// outcome on `metadata`: killed when a scope or exactly matched process
    /// was signalled, otherwise fate unknown without any signal.
    pub(super) fn resume_replay_kill(
        &self,
        metadata: &mut PersistedTask,
        paths: &TaskPaths,
        killed_reason: &str,
    ) {
        let reason = "resume_interrupted_kill";
        let note = match self.replay_kill(metadata) {
            #[cfg(target_os = "linux")]
            ReplayKill::Scope(scope) => {
                if !scope.has_members() {
                    // The scope outlived its processes: the command had
                    // already ended, with an outcome nobody observed.
                    let reason = restart_fate_unknown_reason(metadata, paths);
                    metadata.mark_terminal(BgTaskStatus::FateUnknown, None, Some(reason));
                    super::super::persistence::persist_terminal_exit(paths, metadata);
                    return;
                }
                log_bash_kill(&metadata.task_id, metadata.child_pid, metadata.pgid, reason);
                let note = terminate_scope_or_group(
                    &scope,
                    || {
                        metadata
                            .pgid
                            .map(|pgid| GroupEvidence::for_task(metadata, pgid, None))
                    },
                    &mut || {},
                    || {},
                );
                if let Err(error) = self.inner.task_scopes.kill_with_audit(
                    &metadata.task_id,
                    Some(scope.cgroup()),
                    metadata.child_pid,
                    metadata.pgid,
                    reason,
                ) {
                    let reason = format!("Task outcome is unknown: {error}.");
                    metadata.mark_terminal(BgTaskStatus::FateUnknown, None, Some(reason));
                    super::super::persistence::persist_terminal_exit(paths, metadata);
                    return;
                }
                note
            }
            #[cfg(target_os = "linux")]
            ReplayKill::ScopeUnconfirmed(error) => {
                let reason = format!("Task outcome is unknown: {error}.");
                metadata.mark_terminal(BgTaskStatus::FateUnknown, None, Some(reason));
                super::super::persistence::persist_terminal_exit(paths, metadata);
                return;
            }
            ReplayKill::Process(pid) => {
                let note = match metadata.mode {
                    BgMode::Pipes => terminate_piped_task(
                        metadata,
                        metadata.pgid,
                        Some(pid),
                        reason,
                        None,
                        &mut || {},
                    ),
                    BgMode::Pty => terminate_pty_task(metadata, pid, None, &mut || {}),
                };
                if note.as_deref() == Some(NOTHING_SENT_NOTE) {
                    // The process stopped matching between the check and the
                    // signal: nothing was sent.
                    let reason = undelivered_kill_reason(metadata, paths);
                    metadata.mark_terminal(BgTaskStatus::FateUnknown, None, Some(reason));
                    super::super::persistence::persist_terminal_exit(paths, metadata);
                    return;
                }
                // A PTY signal targets the group named by its leader's PID.
                let pgid = if metadata.mode == BgMode::Pty {
                    metadata.pgid.filter(|&group| u32::try_from(group) == Ok(pid))
                } else {
                    metadata.pgid
                };
                let group_gone = settle_killed_process_group(&metadata.task_id, pgid);
                if metadata.mode == BgMode::Pty && !group_gone {
                    let reason = note.unwrap_or_else(|| restart_fate_unknown_reason(metadata, paths));
                    metadata.mark_terminal(BgTaskStatus::FateUnknown, None, Some(reason));
                    super::super::persistence::persist_terminal_exit(paths, metadata);
                    return;
                }
                note
            }
            ReplayKill::Unverified => {
                let reason = if command_may_still_run(metadata) {
                    undelivered_kill_reason(metadata, paths)
                } else {
                    restart_fate_unknown_reason(metadata, paths)
                };
                metadata.mark_terminal(BgTaskStatus::FateUnknown, None, Some(reason));
                super::super::persistence::persist_terminal_exit(paths, metadata);
                return;
            }
        };
        // SIGKILL withheld while survivors could not be confirmed: the
        // command may still be running, so its outcome is unknown.
        if let Some(note) = note.as_deref().filter(|note| note_is_unconfirmed(note)) {
            metadata.mark_terminal(BgTaskStatus::FateUnknown, None, Some(note.to_string()));
            super::super::persistence::persist_terminal_exit(paths, metadata);
            return;
        }
        let mut reason = killed_reason.to_string();
        if let Some(note) = note {
            reason = format!("{reason}; {note}");
        }
        if metadata.mode == BgMode::Pipes {
            if let Err(error) = write_kill_marker_if_absent(paths) {
                reason = kill_marker_failure_reason(
                    Some(&reason),
                    &format!("failed to write kill marker: {error}"),
                );
            }
        }
        metadata.mark_terminal(BgTaskStatus::Killed, None, Some(reason));
    }

    /// Signal the surviving members of an already-terminal task's process
    /// group, then record what is still alive. Runs without the state lock
    /// held across the signal and its grace period. Returns whether a signal
    /// was sent and the note a kill reply should carry, if any.
    #[cfg(unix)]
    pub(super) fn terminate_terminal_survivors(
        &self,
        task: &Arc<BgTask>,
        pgid: i32,
        reason: &str,
    ) -> Result<(bool, Option<String>), String> {
        let metadata = task
            .lock_state()
            .map_err(|_| "background task lock poisoned")?
            .metadata
            .clone();
        #[cfg(target_os = "linux")]
        let scoped = match self.kill_scope_handle(&metadata) {
            Ok(Some(scope)) => Some(terminate_scope_or_group(
                &scope,
                || Some(GroupEvidence::for_task(&metadata, pgid, None)),
                &mut || {},
                || {},
            )),
            _ => None,
        };
        #[cfg(not(target_os = "linux"))]
        let scoped: Option<Option<String>> = None;
        let note =
            scoped.unwrap_or_else(|| GroupEvidence::for_task(&metadata, pgid, None).terminate());
        let signalled = note.as_deref() != Some(NOTHING_SENT_NOTE);
        // The scope's result is reported after the post-kill check is saved.
        let scope_result = self.kill_task_scope_with_reason(&task.task_id, reason);
        settle_killed_process_group(&task.task_id, Some(pgid));
        let sample = live_process_group_members(pgid);
        let mut db = DeferredDbWrites::new(self, task);
        let mut state = task
            .lock_state()
            .map_err(|_| "background task lock poisoned".to_string())?;
        state.metadata.group_kill_sent |= signalled;
        record_group_check(&mut state.metadata, &sample);
        self.persist_task_locked(task, &state.metadata, &mut db)
            .map_err(|error| format!("failed to persist post-kill descendant sample: {error}"))?;
        scope_result?;
        Ok((signalled, note))
    }

    /// Process groups are not enumerated or signaled as a group on this
    /// platform, so a terminal task has no survivors to signal.
    #[cfg(not(unix))]
    pub(super) fn terminate_terminal_survivors(
        &self,
        _task: &Arc<BgTask>,
        _pgid: i32,
        _reason: &str,
    ) -> Result<(bool, Option<String>), String> {
        Ok((true, None))
    }

    /// Signal a live piped task, then end its Linux task scope. A task with a
    /// scope is stopped through it: SIGTERM to each process in the scope, the
    /// grace period, then the scope's kill switch. Its process group is not
    /// signalled by number. Returns the scope kill's result and the note a
    /// kill reply should carry, if any.
    pub(super) fn signal_live_piped_kill(
        &self,
        task: &Arc<BgTask>,
        pgid: Option<i32>,
        child_pid: Option<u32>,
        reason: &str,
        #[allow(unused_mut)] mut child: Option<&mut Child>,
        before_first_signal: &mut dyn FnMut(),
    ) -> (Result<(), String>, Option<String>) {
        let metadata = match task.lock_state() {
            Ok(state) => state.metadata.clone(),
            Err(_) => return (Err("background task lock poisoned".into()), None),
        };
        #[cfg(target_os = "linux")]
        if let Ok(Some(scope)) = self.kill_scope_handle(&metadata) {
            log_bash_kill(&task.task_id, child_pid, pgid, reason);
            let owned_leader = child.as_ref().map(|child| child.id());
            let note = terminate_scope_or_group(
                &scope,
                || pgid.map(|pgid| GroupEvidence::for_task(&metadata, pgid, owned_leader)),
                before_first_signal,
                || {
                    if let Some(child) = child.as_deref_mut() {
                        let _ = child.try_wait();
                    }
                },
            );
            return (
                self.kill_task_scope_with_reason(&task.task_id, reason),
                note,
            );
        }
        let note = terminate_piped_task(
            &metadata,
            pgid,
            child_pid,
            reason,
            child,
            before_first_signal,
        );
        (
            self.kill_task_scope_with_reason(&task.task_id, reason),
            note,
        )
    }

    /// Signal a live PTY task: through its Linux task scope when it has one,
    /// otherwise its process group, using `evidence` gathered before the
    /// leader was hung up. Returns the note a kill reply should carry, if any.
    pub(super) fn signal_live_pty_kill(
        &self,
        task: &Arc<BgTask>,
        pid: u32,
        evidence: Option<GroupEvidence>,
    ) -> Option<String> {
        let metadata = task.lock_state().ok()?.metadata.clone();
        // The kill commits when its first signal is sent: from then on an exit
        // the PTY waiter observes is the kill's, whatever its code.
        let mut commit = || {
            if let Ok(state) = task.lock_state() {
                if let TaskRuntime::Pty(Some(pty)) = &state.runtime {
                    pty.was_killed.store(true, Ordering::SeqCst);
                }
            }
        };
        #[cfg(target_os = "linux")]
        if let Ok(Some(scope)) = self.kill_scope_handle(&metadata) {
            let note = terminate_scope_or_group(&scope, || evidence, &mut commit, || {});
            // The caller writes the scope's `cgroup.kill` next, which is a
            // signal sent even when no SIGTERM could be.
            commit();
            return note;
        }
        terminate_pty_task(&metadata, pid, evidence, &mut commit)
    }

    /// Reap a terminal task's own wrapper if a kill handed it back still
    /// running. Returns whether the child slot is now empty; until then the
    /// task's files must stay (on Windows the child keeps their handles open).
    pub(crate) fn reap_terminal_child(&self, task: &BgTask) -> bool {
        let Ok(mut state) = task.lock_state() else {
            return false;
        };
        if !state.metadata.status.is_terminal() {
            return true;
        }
        let TaskRuntime::Piped(slot) = &mut state.runtime else {
            return true;
        };
        let reaped = match slot.as_mut().map(|child| child.try_wait()) {
            None => return true,
            Some(Ok(None)) => return false,
            Some(Ok(Some(_))) => true,
            // As in the kill's bounded reap wait: a child that cannot be
            // waited for (ECHILD once something else reaped it) is dropped,
            // so the task is never kept forever.
            Some(Err(error)) => {
                crate::slog_warn!(
                    "background task {} wrapper could not be reaped: {error}",
                    task.task_id
                );
                false
            }
        };
        *slot = None;
        state.child_exit_observed |= reaped;
        true
    }
}

/// Keeps a kill's `kill_in_flight` claim until the kill has published its
/// outcome, and releases it on every exit, early returns and panics included,
/// so no task is left `killing` with nothing to finish it.
pub(super) struct KillInFlight<'a>(Option<&'a BgTask>);

impl<'a> KillInFlight<'a> {
    /// A claim for `task`, if this kill set its `kill_in_flight`.
    pub(super) fn new(task: &'a BgTask, claimed: bool) -> Self {
        Self(claimed.then_some(task))
    }

    /// Release the claim now, waking callers waiting for this kill.
    pub(super) fn finish(&mut self) {
        if let Some(task) = self.0.take() {
            if let Ok(mut state) = task.lock_state() {
                state.kill_in_flight = false;
            }
            task.kill_settled.notify_all();
        }
    }
}

impl Drop for KillInFlight<'_> {
    fn drop(&mut self) {
        self.finish();
    }
}

thread_local! {
    static POLLING_AS_KILL: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Poll a PTY task on behalf of the kill that owns it, so the kill can
/// publish its own outcome while it still holds `kill_in_flight` (other
/// pollers wait for it).
#[cfg(unix)]
pub(super) fn poll_as_kill(registry: &BgTaskRegistry, task: &Arc<BgTask>) {
    with_polling_as_kill(|| {
        let _ = registry.poll_task(task);
    });
}

/// Run `poll` with this thread marked as polling for the kill in flight. The
/// mark is cleared on every exit, a panic included, so later polls on a
/// long-lived worker thread still wait for other kills.
#[cfg(unix)]
fn with_polling_as_kill<T>(poll: impl FnOnce() -> T) -> T {
    struct Clear;
    impl Drop for Clear {
        fn drop(&mut self) {
            POLLING_AS_KILL.with(|flag| flag.set(false));
        }
    }
    POLLING_AS_KILL.with(|flag| flag.set(true));
    let _clear = Clear;
    poll()
}

/// Whether this thread is polling on behalf of the kill in flight.
pub(super) fn polling_as_kill() -> bool {
    POLLING_AS_KILL.with(std::cell::Cell::get)
}

/// The evidence a PTY kill needs, gathered before its leader is hung up and
/// without the task's state lock (listing a group can be slow): the leader is
/// still running then, so it proves the group, and the members listed then
/// prove it after the leader is gone. The caller re-checks the leader under
/// the lock before the hangup.
#[cfg(unix)]
pub(super) fn pty_evidence_before_hangup(metadata: &PersistedTask) -> Option<GroupEvidence> {
    let pgid = metadata.pgid?;
    let mut evidence = GroupEvidence::for_task(metadata, pgid, None);
    evidence.remember_members();
    Some(evidence)
}

/// How long a kill waits for its own wrapper to exit before leaving it for
/// the watchdog's reap pass. A wrapper stuck in uninterruptible I/O must not
/// hold the kill, and the completion it publishes, indefinitely.
const KILL_REAP_WAIT: Duration = Duration::from_secs(5);

/// Reap a killed task's own wrapper, waiting at most [`KILL_REAP_WAIT`].
/// Returns whether it was reaped. `child` is left set only when the wrapper
/// has not exited yet, so the caller can hand it back for a later reap.
pub(super) fn reap_within(child: &mut Option<Child>) -> bool {
    let Some(running) = child.as_mut() else {
        return false;
    };
    let deadline = Instant::now() + KILL_REAP_WAIT;
    let reaped = loop {
        match running.try_wait() {
            Ok(Some(_)) => break true,
            Ok(None) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(10));
            }
            Ok(None) => return false,
            Err(_) => break false,
        }
    };
    *child = None;
    reaped
}

/// Settle a PTY task that was running when AFT stopped and left no exit
/// marker. A pseudoterminal cannot be reattached after a restart, so a command
/// that can still be tied to the task is stopped and reported killed. One that
/// cannot is left alone and its outcome reported unknown.
pub(super) fn settle_unattached_pty(
    registry: &BgTaskRegistry,
    metadata: &mut PersistedTask,
    paths: &TaskPaths,
) {
    registry.resume_replay_kill(metadata, paths, "pty_lost_on_bridge_restart");
}

// Sending a group signal and reaping its leader do not mean descendants have
// exited yet. Settle the signalled group before sampling the kill completion,
// but keep reporting real survivors when this bounded grace expires.
#[cfg(unix)]
const KILL_GROUP_SETTLEMENT_GRACE: Duration = Duration::from_secs(2);

#[cfg(unix)]
fn kill_settlement_now() -> Instant {
    #[cfg(test)]
    if let Some(now) = KILL_SETTLEMENT_TEST_NOW.with(std::cell::Cell::get) {
        return now;
    }
    Instant::now()
}

#[cfg(unix)]
pub(super) fn settle_killed_process_group(task_id: &str, pgid: Option<i32>) -> bool {
    let Some(pgid) = pgid.filter(|pgid| *pgid > 0) else {
        return false;
    };
    let started = kill_settlement_now();
    #[cfg(test)]
    let diagnostic_started = Instant::now();
    loop {
        let exists = unsafe { libc::killpg(pgid, 0) } == 0;
        if !exists && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH) {
            return true;
        }
        // Zombies cannot run or retain output pipes. An unreaped orphan may
        // still make killpg(0) succeed, but is not a live descendant to report.
        let listing = live_process_group_members(pgid);
        #[cfg(test)]
        KILL_SETTLEMENT_TEST_SAMPLES.with(|samples| {
            if let Some(samples) = samples.borrow().as_ref() {
                samples.lock().unwrap().record(
                    pgid,
                    format!(
                        "real={:?}, settlement={:?}, exists={exists}, listing={listing:?}",
                        diagnostic_started.elapsed(),
                        kill_settlement_now().saturating_duration_since(started)
                    ),
                );
            }
        });
        if listing.is_ok_and(|listing| listing.is_clean()) {
            return true;
        }
        #[cfg(test)]
        if let Some(notice) = KILL_SETTLEMENT_NOTICES
            .get()
            .and_then(|notices| notices.lock().unwrap().remove(task_id))
        {
            let _ = notice.send(());
        }
        let _ = task_id;
        if kill_settlement_now().saturating_duration_since(started) >= KILL_GROUP_SETTLEMENT_GRACE {
            return false;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[cfg(not(unix))]
pub(super) fn settle_killed_process_group(_task_id: &str, _pgid: Option<i32>) -> bool {
    false
}

/// Whether a finished task's process group, answering under its saved
/// number, is still that task's group and may be signalled. The number is the
/// group leader's pid, and once the original group has exited the system can
/// hand it to an unrelated process. So the group is the task's only while its
/// leader, or a member a check of this task found, is still running in it
/// with the start time AFT recorded. Without such evidence it is left alone,
/// even when a check could not see every member.
pub(super) fn finished_group_is_still_the_tasks(metadata: &PersistedTask, pgid: i32) -> bool {
    GroupEvidence::for_task(metadata, pgid, None).proven()
}

/// Store the outcome of one check of a task's process group: the surviving
/// members, why they could not be listed, or both when the listing was
/// incomplete. An empty list is stored only for a clean check, so `[]` always
/// means the group was seen to be empty.
pub(super) fn record_group_check(
    metadata: &mut PersistedTask,
    check: &Result<GroupListing, LiveDescendantsUnavailable>,
) {
    match check {
        Ok(listing) => {
            let found_any = !listing.members.is_empty() || listing.omitted > 0;
            metadata.live_descendants =
                (found_any || !listing.incomplete).then(|| listing.members.clone());
            metadata.live_descendants_omitted = listing.omitted;
            metadata.live_descendants_unavailable = listing
                .incomplete
                .then_some(LiveDescendantsUnavailable::ListingIncomplete);
            if found_any {
                metadata.group_member_identities = listing.identities.clone();
                metadata.group_member_identities_truncated = listing.identities_truncated;
            } else if !listing.incomplete {
                metadata.group_member_identities.clear();
                metadata.group_member_identities_truncated = false;
            }
        }
        Err(reason) => {
            metadata.live_descendants = None;
            metadata.live_descendants_omitted = 0;
            metadata.live_descendants_unavailable = Some(*reason);
        }
    }
}

pub(in crate::bash_background) fn log_bash_kill(
    task_id: &str,
    pid: Option<u32>,
    pgid: Option<i32>,
    reason: &str,
) {
    let target = pid.or_else(|| pgid.and_then(|pgid| u32::try_from(pgid).ok()));
    match target {
        Some(pid) => {
            crate::slog_info!("bash kill task={task_id} pid={pid} pgid={pgid:?} reason={reason:?}")
        }
        None => crate::slog_info!(
            "bash kill task={task_id} pid=unknown pgid={pgid:?} reason={reason:?}"
        ),
    }
}

/// Stop independent commands concurrently so a batch shares one grace window.
/// Callers collect targets before entering here, without registry/task locks.
pub fn stop_tasks_in_parallel<T: Send, R: Send>(
    tasks: impl IntoIterator<Item = T>,
    stop: impl Fn(T) -> R + Sync,
) -> Vec<R> {
    #[cfg(unix)]
    {
        std::thread::scope(|scope| {
            let stop = &stop;
            let workers: Vec<_> = tasks
                .into_iter()
                .map(|task| scope.spawn(move || stop(task)))
                .collect();
            workers
                .into_iter()
                .map(|worker| worker.join().expect("command stop worker panicked"))
                .collect()
        })
    }
    #[cfg(not(unix))]
    tasks.into_iter().map(stop).collect()
}

/// Close a PTY's pseudoterminal so its reader reaches end-of-file and
/// the task can finalize. On Windows, ConPTY's reader sees end-of-file only
/// once the pseudoconsole is closed, and closing it can wait for conhost to
/// flush; it runs on its own thread so no caller (or lock) waits on that.
pub(super) fn close_pty_master(master: Option<Box<dyn portable_pty::MasterPty + Send>>) {
    let Some(master) = master else {
        return;
    };
    #[cfg(windows)]
    close_pty_master_off_thread(Some(master));
    #[cfg(not(windows))]
    drop(master);
}

#[cfg(any(test, feature = "test-support"))]
#[cfg_attr(not(unix), allow(dead_code))]
pub(crate) fn install_kill_signal_gate_for_test(
    task_id: &str,
) -> (
    crossbeam_channel::Receiver<()>,
    crossbeam_channel::Sender<()>,
) {
    let (reached_tx, reached_rx) = crossbeam_channel::bounded(1);
    let (release_tx, release_rx) = crossbeam_channel::bounded(1);
    super::KILL_SIGNAL_GATES
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap()
        .insert(
            task_id.to_string(),
            super::KillSignalGate {
                reached: reached_tx,
                release: release_rx,
            },
        );
    (reached_rx, release_tx)
}

#[cfg(any(test, feature = "test-support"))]
pub(super) fn wait_on_kill_signal_gate_for_test(task_id: &str) {
    let gate = super::KILL_SIGNAL_GATES
        .get()
        .and_then(|gates| gates.lock().unwrap().remove(task_id));
    if let Some(gate) = gate {
        let _ = gate.reached.send(());
        let _ = gate.release.recv_timeout(Duration::from_secs(30));
    }
    // Stands in for a kill thread that dies after claiming its task.
    #[cfg(test)]
    if KILL_PANICS.lock().unwrap().remove(task_id) {
        panic!("kill thread failed");
    }
}

#[cfg(test)]
pub(crate) static KILL_PANICS: Mutex<BTreeSet<String>> = Mutex::new(BTreeSet::new());

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(target_os = "linux")]
    #[test]
    fn pty_replay_unconfirmed_group_is_fate_unknown() {
        use crate::bash_background::persistence::{allocate_task_layout, write_task_at};
        use crate::bash_background::registry::root_reclaim_fixture::OwnedGroup;

        let storage = tempfile::tempdir().unwrap();
        let resolved = allocate_task_layout(storage.path(), "session").unwrap();
        let paths = &resolved.paths;
        let mut metadata = PersistedTask::starting(
            paths.task_id.clone(),
            "session".into(),
            "sleep 60".into(),
            storage.path().into(),
            None,
            None,
            true,
            false,
        );
        metadata.mode = BgMode::Pty;
        let mut child = OwnedGroup::spawn();
        let pid = child.0.id();
        let pgid = i32::try_from(pid).unwrap();
        metadata.mark_running(pid, pgid);
        write_task_at(&resolved, &metadata).unwrap();
        std::fs::write(&paths.exit, b"").unwrap();

        let registry = BgTaskRegistry::default();
        assert!(matches!(registry.replay_kill(&metadata), ReplayKill::Process(found) if found == pid));
        // An unreaped child keeps the group visible even after it stops.
        let _listing_fault = crate::bash_background::process::test_hooks::scoped_listing_failure(pgid);
        settle_unattached_pty(&registry, &mut metadata, paths);
        assert!(child.0.try_wait().unwrap().is_some());
        assert_eq!(metadata.status, BgTaskStatus::FateUnknown);
        assert_eq!(metadata.exit_code, None);
        assert_eq!(metadata.exit_marker_unavailable, metadata.status_reason);
        assert!(!paths.exit.exists());
    }

    fn task_with_child(pid: u32, started_at_ms: Option<u64>) -> PersistedTask {
        let mut task = PersistedTask::starting(
            "bash-0123456789abcdef".into(),
            "session".into(),
            "sleep 60".into(),
            std::path::PathBuf::new(),
            None,
            None,
            false,
            false,
        );
        task.child_pid = Some(pid);
        task.pgid = Some(pid as i32);
        task.child_start_time_ms = started_at_ms;
        task
    }

    #[test]
    fn replay_signals_only_an_exactly_matched_saved_identity() {
        let task = task_with_child(4242, Some(1_000));
        let mut seen = None;
        let plan = replay_process(&task, |saved| {
            seen = Some((saved.pid, saved.started_at_ms));
            true
        });
        assert!(matches!(plan, ReplayKill::Process(4242)), "{plan:?}");
        assert_eq!(seen, Some((4242, 1_000)));
        let plan = replay_process(&task, |_| false);
        assert!(matches!(plan, ReplayKill::Unverified), "{plan:?}");
        // A record without a saved start cannot be matched exactly, so no
        // probe runs and nothing is signalled.
        let unreadable = task_with_child(4242, None);
        let plan = replay_process(&unreadable, |_| panic!("no saved identity to probe"));
        assert!(matches!(plan, ReplayKill::Unverified), "{plan:?}");
    }

    #[cfg(unix)]
    #[test]
    fn kill_coverage_notes_name_the_outside_count() {
        assert_eq!(
            outside_group_note(1),
            "The kill reached only the command's original process group; AFT still sees 1 process it recorded for this command running outside that group."
        );
        assert!(outside_group_note(3).contains("AFT still sees 3 processes it recorded"));
        assert_eq!(
            join_kill_warnings(Some("a".into()), Some("b".into())).as_deref(),
            Some("a\nb")
        );
        assert_eq!(
            join_kill_warnings(None, Some("b".into())).as_deref(),
            Some("b")
        );
    }

    #[cfg(unix)]
    fn spawn_session(script: &str) -> std::process::Child {
        let mut command = std::process::Command::new("/bin/sh");
        command.args(["-c", script]);
        super::super::super::process::start_new_session(&mut command);
        command.spawn().unwrap()
    }

    #[cfg(unix)]
    fn replay_record(
        dir: &std::path::Path,
        pid: u32,
        mode: BgMode,
        status: BgTaskStatus,
    ) -> (PersistedTask, TaskPaths) {
        let paths = crate::bash_background::persistence::task_paths(
            dir,
            "session",
            &super::super::random_slug(),
        )
        .unwrap();
        std::fs::create_dir_all(&paths.io_dir).unwrap();
        let mut metadata = PersistedTask::starting(
            paths.task_id.clone(),
            "session".into(),
            "sleep 60".into(),
            dir.into(),
            None,
            None,
            true,
            false,
        );
        metadata.mark_running(pid, pid as i32);
        metadata.mode = mode;
        metadata.status = status;
        (metadata, paths)
    }

    /// After a restart, a pid whose saved start no longer matches names some
    /// other process. Liveness that passes the loose retention check must not
    /// be taken as permission to signal it.
    #[cfg(unix)]
    #[test]
    fn replay_leaves_a_process_whose_saved_start_differs_untouched() {
        for (mode, status) in [
            (BgMode::Pipes, BgTaskStatus::Killing),
            (BgMode::Pty, BgTaskStatus::Running),
        ] {
            let mut other = spawn_session("exec sleep 60");
            let pid = other.id();
            let dir = tempfile::tempdir().unwrap();
            let (mut metadata, paths) =
                replay_record(dir.path(), pid, mode.clone(), status.clone());
            let started = metadata.child_start_time_ms.expect("live process start");
            metadata.child_start_time_ms = Some(started.saturating_sub(5_000));
            metadata.child_native_birth = None;
            crate::bash_background::persistence::write_task(&paths.json, &metadata).unwrap();
            let registry = BgTaskRegistry::default();
            registry
                .rehydrate_resolved_task_with_process_probe(
                    metadata,
                    paths.clone(),
                    "session",
                    |_| true,
                )
                .unwrap();
            let untouched = other.try_wait().unwrap().is_none();
            let _ = other.kill();
            let _ = other.wait();
            let persisted = crate::bash_background::persistence::read_task(&paths.json).unwrap();
            assert!(
                untouched,
                "{mode:?} replay signalled an unidentified process"
            );
            assert_eq!(persisted.status, BgTaskStatus::FateUnknown, "{mode:?}");
            assert!(persisted
                .status_reason
                .as_deref()
                .is_some_and(|reason| reason.contains(
                    "AFT could not deliver the kill; the command's processes may still be running"
                )));
        }
    }

    /// The recorded child can be gone while a descendant in the task's scope
    /// still runs. A restart finishing the interrupted kill stops the scope.
    #[cfg(target_os = "linux")]
    #[test]
    fn replay_stops_a_scoped_command_whose_first_process_is_gone() {
        let mut survivor = spawn_session("exec sleep 60");
        let dir = tempfile::tempdir().unwrap();
        let (metadata, paths) =
            replay_record(dir.path(), u32::MAX, BgMode::Pipes, BgTaskStatus::Killing);
        crate::bash_background::persistence::write_task(&paths.json, &metadata).unwrap();
        let cgroup = dir.path().join("scope");
        std::fs::create_dir(&cgroup).unwrap();
        std::fs::write(cgroup.join("cgroup.procs"), format!("{}\n", survivor.id())).unwrap();
        std::fs::write(cgroup.join("cgroup.kill"), "0").unwrap();
        let registry = BgTaskRegistry::default();
        registry
            .inner
            .task_scopes
            .register(super::super::super::task_scope::handle_for_test(
                &paths.task_id,
                cgroup.clone(),
            ));
        let events = Arc::new(Mutex::new(Vec::new()));
        super::super::super::task_scope::with_kill_events_for_test(events.clone(), || {
            registry
                .rehydrate_resolved_task_with_process_probe(
                    metadata,
                    paths.clone(),
                    "session",
                    |_| false,
                )
                .unwrap()
        });
        let stopped = survivor.try_wait().unwrap().is_some();
        let _ = survivor.kill();
        let _ = survivor.wait();
        assert_eq!(*events.lock().unwrap(), vec!["scope-term", "scope-kill"]);
        assert_eq!(
            std::fs::read_to_string(cgroup.join("cgroup.kill")).unwrap(),
            "1"
        );
        assert!(stopped, "the scope's SIGTERM did not reach its process");
        let persisted = crate::bash_background::persistence::read_task(&paths.json).unwrap();
        assert_eq!(persisted.status, BgTaskStatus::Killed);
    }

    /// A scope whose folder still exists after its processes ended says the
    /// command had already finished: nothing is killed and the outcome is
    /// recorded the way an exited command's is.
    #[cfg(target_os = "linux")]
    #[test]
    fn replay_into_an_empty_scope_records_the_command_as_ended() {
        let dir = tempfile::tempdir().unwrap();
        let (metadata, paths) =
            replay_record(dir.path(), u32::MAX, BgMode::Pipes, BgTaskStatus::Killing);
        crate::bash_background::persistence::write_task(&paths.json, &metadata).unwrap();
        let cgroup = dir.path().join("scope");
        std::fs::create_dir(&cgroup).unwrap();
        std::fs::write(cgroup.join("cgroup.procs"), "").unwrap();
        std::fs::write(cgroup.join("cgroup.kill"), "0").unwrap();
        let registry = BgTaskRegistry::default();
        registry
            .inner
            .task_scopes
            .register(super::super::super::task_scope::handle_for_test(
                &paths.task_id,
                cgroup.clone(),
            ));
        let events = Arc::new(Mutex::new(Vec::new()));
        super::super::super::task_scope::with_kill_events_for_test(events.clone(), || {
            registry
                .rehydrate_resolved_task_with_process_probe(
                    metadata,
                    paths.clone(),
                    "session",
                    |_| false,
                )
                .unwrap()
        });
        let persisted = crate::bash_background::persistence::read_task(&paths.json).unwrap();
        assert!(
            events.lock().unwrap().is_empty(),
            "{:?}",
            events.lock().unwrap()
        );
        assert_eq!(persisted.status, BgTaskStatus::FateUnknown);
        assert_eq!(
            persisted.status_reason,
            Some(restart_fate_unknown_reason(&persisted, &paths))
        );
    }

    /// The members listed while the group was proven are kept only if the
    /// group is still the task's after the listing. Here the member that
    /// proved it exits during the listing while another process holds the
    /// group number with a start the task never recorded: the listing could
    /// describe a reused group, so it proves nothing.
    #[cfg(unix)]
    #[test]
    fn remember_members_drops_a_listing_it_cannot_re_prove() {
        use std::os::unix::process::CommandExt;
        // A group in this session, so another process can join it.
        let mut leader = std::process::Command::new("sleep")
            .arg("30")
            .process_group(0)
            .spawn()
            .unwrap();
        let pgid = leader.id() as i32;
        let member = std::process::Command::new("sleep")
            .arg("30")
            .process_group(pgid)
            .spawn()
            .unwrap();
        let member_pid = member.id();
        let mut evidence = GroupEvidence {
            pgid,
            owned_leader: None,
            identities: vec![
                ProcessIdentity::from_epoch(leader.id(), 1),
                ProcessIdentity::of(member_pid).unwrap(),
            ],
        };
        let member = std::cell::RefCell::new(Some(member));
        let proven = super::super::super::process::test_hooks::with_listing_observer(
            move || {
                if let Some(mut member) = member.borrow_mut().take() {
                    let _ = member.kill();
                    let _ = member.wait();
                }
            },
            || evidence.remember_members(),
        );
        let _ = leader.kill();
        let _ = leader.wait();
        assert!(!proven, "a listing that could not be re-proven was kept");
        assert!(!evidence
            .identities
            .iter()
            .any(|identity| identity.pid == pgid as u32 && identity.started_at_ms != 1));
    }

    /// A kill's claim on `kill_in_flight` is released even when the kill
    /// thread dies after claiming its task, so a later kill finishes it.
    #[cfg(unix)]
    #[test]
    fn kill_in_flight_is_released_when_the_kill_thread_panics() {
        let registry = BgTaskRegistry::default();
        registry.inner.shutdown.store(true, Ordering::SeqCst);
        let dir = tempfile::tempdir().unwrap();
        let task_id = registry
            .spawn(
                SpawnPlan::Unsandboxed,
                "sleep 30",
                "session".into(),
                dir.path().into(),
                HashMap::new(),
                crate::bash_background::HardKill::After(std::time::Duration::from_secs(30)),
                None,
                dir.path().into(),
                8,
                false,
                false,
                None,
            )
            .unwrap();
        let task = registry.task(&task_id).unwrap();
        let pgid = task.lock_state().unwrap().metadata.pgid.unwrap();
        KILL_PANICS.lock().unwrap().insert(task_id.clone());
        let panicked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            registry.kill(&task_id, "session")
        }));
        let released = !task.lock_state().unwrap().kill_in_flight;
        let finished = registry.kill(&task_id, "session");
        unsafe {
            libc::killpg(pgid, libc::SIGKILL);
        }
        assert!(panicked.is_err(), "the kill was expected to die");
        assert!(released, "a dead kill left the task claimed");
        assert!(
            finished.unwrap().info.status.is_terminal(),
            "a later kill could not finish the task"
        );
    }

    /// Whether `pid` has exited and waits to be reaped.
    #[cfg(unix)]
    fn is_zombie(pid: u32) -> bool {
        #[cfg(target_os = "linux")]
        return std::fs::read_to_string(format!("/proc/{pid}/stat"))
            .ok()
            .and_then(|stat| {
                stat.rfind(") ")
                    .map(|close| stat[close + 2..].starts_with('Z'))
            })
            .unwrap_or(false);
        #[cfg(target_os = "macos")]
        return super::super::super::process::child_is_zombie_for_test(pid);
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        false
    }

    /// A leader that has exited but is not reaped yet (a zombie) is not a
    /// running leader, so the kill does not send it a hangup or claim the
    /// exit it made on its own as the kill's.
    #[cfg(unix)]
    #[test]
    fn a_zombie_leader_is_not_running() {
        use std::os::unix::process::CommandExt;
        let mut leader = std::process::Command::new("sh")
            .args(["-c", "read _"])
            .stdin(std::process::Stdio::piped())
            .process_group(0)
            .spawn()
            .unwrap();
        let pid = leader.id();
        let identity = ProcessIdentity::of(pid).unwrap();
        let evidence = GroupEvidence {
            pgid: pid as i32,
            owned_leader: None,
            identities: vec![identity.clone()],
        };
        assert!(evidence.leader_running(), "precondition: a running leader");
        drop(leader.stdin.take());
        let deadline = Instant::now() + std::time::Duration::from_secs(10);
        while !is_zombie(pid) {
            assert!(
                Instant::now() < deadline,
                "the leader never became a zombie"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        let running = evidence.leader_running();
        // Unreaped, it still pins the group number, so it still proves it.
        let proven = evidence.proven();
        let _ = leader.wait();
        assert!(!running, "a zombie leader was taken as running");
        #[cfg(target_os = "linux")]
        assert!(proven, "an unreaped leader stopped proving its group");
        let _ = proven;
    }

    /// The mark that lets a kill's own poll past the in-flight deferral is
    /// cleared even when the poll panics.
    #[cfg(unix)]
    #[test]
    fn polling_as_kill_is_cleared_when_the_poll_panics() {
        let panicked = std::panic::catch_unwind(|| {
            with_polling_as_kill(|| {
                assert!(polling_as_kill());
                panic!("finalization failed");
            })
        });
        assert!(panicked.is_err());
        assert!(!polling_as_kill(), "the mark outlived a panicking poll");
    }

    /// A terminal task's wrapper that cannot be waited for (something else
    /// already reaped it) is dropped, so the task is not kept forever.
    #[cfg(unix)]
    #[test]
    fn a_wrapper_that_cannot_be_waited_for_is_dropped() {
        let registry = BgTaskRegistry::default();
        registry.inner.shutdown.store(true, Ordering::SeqCst);
        let dir = tempfile::tempdir().unwrap();
        let task_id = registry
            .spawn(
                SpawnPlan::Unsandboxed,
                "sleep 30",
                "session".into(),
                dir.path().into(),
                HashMap::new(),
                crate::bash_background::HardKill::After(std::time::Duration::from_secs(30)),
                None,
                dir.path().into(),
                8,
                false,
                false,
                None,
            )
            .unwrap();
        let task = registry.task(&task_id).unwrap();
        let pid = task.lock_state().unwrap().metadata.child_pid.unwrap() as i32;
        unsafe {
            libc::kill(pid, libc::SIGKILL);
            libc::waitpid(pid, std::ptr::null_mut(), 0);
        }
        task.lock_state()
            .unwrap()
            .metadata
            .mark_terminal(BgTaskStatus::Killed, Some(137), None);
        let emptied = registry.reap_terminal_child(&task);
        let slot_empty = matches!(task.lock_state().unwrap().runtime, TaskRuntime::Piped(None));
        assert!(
            emptied,
            "the task was kept for a child that cannot be waited for"
        );
        assert!(slot_empty);
    }

    /// A group number is checked again right before SIGKILL; a group no longer
    /// tied to the task is not killed, and the reply says so.
    #[cfg(target_os = "linux")]
    #[test]
    fn group_kill_is_withheld_when_the_group_cannot_be_proven_again() {
        let dir = tempfile::tempdir().unwrap();
        let ready = dir.path().join("ready");
        let mut leader = spawn_session(&format!(
            "trap '' TERM; : > '{}'; while :; do sleep 1; done",
            ready.display()
        ));
        while !ready.exists() {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        let pgid = leader.id() as i32;
        let events = Arc::new(Mutex::new(Vec::new()));
        let mut checks = 0;
        let outcome = super::super::super::process::test_hooks::with_signal_events(
            pgid,
            events.clone(),
            || {
                super::super::super::process::terminate_proven_pgid(pgid, || {
                    checks += 1;
                    checks == 1
                })
            },
        );
        let alive = leader.try_wait().unwrap().is_none();
        unsafe {
            libc::killpg(pgid, libc::SIGKILL);
        }
        let _ = leader.wait();
        assert_eq!(outcome, GroupTermination::KillWithheld);
        assert_eq!(*events.lock().unwrap(), vec!["group-term"]);
        assert!(alive);
        assert_eq!(checks, 2);
    }

    /// A child this process has not reaped pins its group number, so the
    /// group stays provable through SIGKILL even with no recorded identity.
    #[cfg(unix)]
    #[test]
    fn an_unreaped_leader_proves_its_group_until_it_is_reaped() {
        let mut leader = spawn_session("exit 0");
        let pid = leader.id();
        let dir = tempfile::tempdir().unwrap();
        let (mut metadata, _) =
            replay_record(dir.path(), pid, BgMode::Pipes, BgTaskStatus::Running);
        metadata.child_start_time_ms = None;
        let evidence = GroupEvidence::for_task(&metadata, pid as i32, Some(pid));
        std::thread::sleep(std::time::Duration::from_millis(200));
        assert!(
            evidence.proven(),
            "an exited but unreaped leader still pins its group"
        );
        leader.wait().unwrap();
        assert!(!evidence.proven(), "a reaped leader proves nothing");
    }
}
