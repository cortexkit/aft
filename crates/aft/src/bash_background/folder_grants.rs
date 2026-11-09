//! Which saved folder grants each running command's sandbox was built with,
//! so revoking one grant on the permissions page can stop exactly the
//! commands that still hold that folder.
//!
//! An operating-system sandbox cannot be narrowed once a process runs: a
//! command keeps every folder its launch profile allowed until it exits. The
//! spawn records, on the task, the scope whose saved grants were read and
//! each granted path its profile allowed. A revoke then stops the holder's
//! running local commands whose record names the revoked path; commands
//! whose sandbox never had the folder keep running.
use super::{BgTaskRegistry, BgTaskStatus};
use crate::db::access_grants::ScopeKey;
use serde::{Deserialize, Serialize};
use std::cell::RefCell;
use std::path::{Path, PathBuf};

/// Status reason of a command stopped because the person revoked a saved
/// folder grant its sandbox was built with.
pub const FOLDER_ACCESS_REVOKED_REASON: &str = "folder_access_revoked";

/// How the agent is told why such a command stopped: its status reads
/// "killed (folder access revoked)" (see `super::scope_end_label`).
pub const FOLDER_ACCESS_REVOKED_LABEL: &str = "folder access revoked";

/// The saved grants a task's sandbox profile allowed, and whose they are.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskFolderGrants {
    pub owner: String,
    pub scope_ref: String,
    pub epoch: u64,
    /// Granted paths, exactly as stored in aft.db, that the profile allows.
    pub paths: Vec<PathBuf>,
}

impl TaskFolderGrants {
    fn holds(&self, scope: &ScopeKey, path: &Path) -> bool {
        self.owner == scope.owner
            && self.scope_ref == scope.scope_ref
            && self.epoch == scope.epoch
            && self.paths.iter().any(|granted| granted == path)
    }
}

thread_local! {
    static SPAWN: RefCell<Option<TaskFolderGrants>> = const { RefCell::new(None) };
}

/// Restores the previous spawn record when dropped.
pub(crate) struct SpawnGuard(Option<TaskFolderGrants>);

impl Drop for SpawnGuard {
    fn drop(&mut self) {
        SPAWN.with(|slot| *slot.borrow_mut() = self.0.take());
    }
}

/// Make `grants` the record a task created on this thread takes, until the
/// guard drops. The spawn sets it between resolving the sandbox profile and
/// registering the task.
pub(crate) fn spawn_guard(grants: Option<TaskFolderGrants>) -> SpawnGuard {
    SpawnGuard(SPAWN.with(|slot| slot.replace(grants)))
}

/// The record for a task being created on this thread now.
pub(crate) fn for_spawn() -> Option<TaskFolderGrants> {
    SPAWN.with(|slot| slot.borrow().clone())
}

/// The saved grants of the calling route's scope that a launch profile
/// allows. `None` without a native launcher profile (nothing confines such a
/// command, so revoking cannot narrow it) or without a scope with saved
/// grants.
pub(crate) fn for_plan(
    ctx: &crate::context::AppContext,
    plan: &crate::sandbox_spawn::SpawnPlan,
) -> Option<TaskFolderGrants> {
    let scope = crate::access_policy::current_scope()?;
    let profile = plan.changed_files_read_profile()?;
    // The same policy the profile was compiled from: the launcher profile
    // reads `access_policy("")` for this call too.
    let granted = ctx.access_policy("").ok()?.grants;
    for_profile(Some(scope), &granted, Some(profile))
}

/// The entries of `granted` that `profile` allows, recorded for `scope`.
pub(crate) fn for_profile(
    scope: Option<ScopeKey>,
    granted: &[crate::access_policy::AccessGrant],
    profile: Option<&crate::sandbox_profile::SandboxProfile>,
) -> Option<TaskFolderGrants> {
    let (scope, profile) = (scope?, profile?);
    let allowed = |path: &Path| {
        profile
            .writable_roots
            .iter()
            .chain(&profile.read_allow)
            .chain(&profile.data_policy.read_allow)
            .chain(&profile.data_policy.write_allow)
            .any(|allowed| allowed == path)
    };
    let mut paths: Vec<PathBuf> = granted
        .iter()
        .map(|grant| &grant.path)
        .filter(|path| allowed(path))
        .cloned()
        .collect();
    paths.sort_unstable();
    paths.dedup();
    (!paths.is_empty()).then_some(TaskFolderGrants {
        owner: scope.owner,
        scope_ref: scope.scope_ref,
        epoch: scope.epoch,
        paths,
    })
}

/// A running command that holds a revoked folder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeldTask {
    pub task_id: String,
    pub session_id: String,
}

impl BgTaskRegistry {
    /// This registry's unfinished local commands whose sandbox allowed
    /// `path` from `scope`'s saved grants. Build-server jobs are left out:
    /// they never run under a local sandbox.
    pub(crate) fn folder_grant_tasks(&self, scope: &ScopeKey, path: &Path) -> Vec<HeldTask> {
        self.task_arcs()
            .into_iter()
            .filter(|task| !task.is_terminal())
            .filter(|task| {
                task.lock_state().is_ok_and(|state| {
                    state.metadata.remote.is_none()
                        && state
                            .metadata
                            .folder_grants
                            .as_ref()
                            .is_some_and(|grants| grants.holds(scope, path))
                })
            })
            .map(|task| HeldTask {
                task_id: task.task_id.clone(),
                session_id: task.session_id.clone(),
            })
            .collect()
    }

    /// Stop one command because a folder its sandbox allowed was revoked,
    /// through the same process-group kill a scope end uses. Returns whether
    /// this kill ended it (a command can finish on its own meanwhile).
    pub(crate) fn kill_for_folder_revocation(&self, task: &HeldTask) -> Result<bool, String> {
        self.kill_with_status_reason(
            &task.task_id,
            &task.session_id,
            BgTaskStatus::Killed,
            Some(FOLDER_ACCESS_REVOKED_REASON.into()),
        )
        .map(|snapshot| snapshot.info.status == BgTaskStatus::Killed)
        .map_err(|error| error.to_string())
    }
}

#[cfg(test)]
#[path = "folder_grants_tests.rs"]
mod tests;
