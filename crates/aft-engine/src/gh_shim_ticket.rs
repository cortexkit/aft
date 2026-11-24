use std::{ops::Deref, path::Path};

use aft_github::gh_shim_ticket::{
    self as ticket,
    spawn_metadata::{ColdTaskMetadata, SpawnMetadata},
};

use crate::bash_background::{persistence::PersistedTask, registry::scope_for_spawn};

pub use aft_github::gh_shim_ticket::*;

/// A ticket for a background task that has not been bound to its task ID yet.
pub struct PendingTicket(ticket::PendingTicket);

impl PendingTicket {
    pub fn issue(session_id: &str, project_root: &str) -> Self {
        Self(ticket::PendingTicket::issue_with_scope(
            session_id,
            project_root,
            &scope_for_spawn,
        ))
    }

    pub(crate) fn issue_for_project(session_id: &str, project_root: Option<&Path>) -> Self {
        Self(ticket::PendingTicket::issue_for_project(
            session_id,
            project_root,
            &scope_for_spawn,
        ))
    }

    pub fn bind_task(self, task_id: &str) {
        self.0.bind_task(task_id);
    }
}

impl Deref for PendingTicket {
    type Target = ticket::PendingTicket;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

/// A ticket that lives for one synchronous `gh` call.
pub struct ScopedTicket(ticket::ScopedTicket);

impl ScopedTicket {
    pub fn issue(session_id: &str, call_id: &str, project_root: &str) -> Self {
        Self(ticket::ScopedTicket::issue_with_scope(
            session_id,
            call_id,
            project_root,
            &scope_for_spawn,
        ))
    }

    pub fn value(&self) -> Option<&str> {
        self.0.value()
    }
}

fn spawn_metadata(task: &PersistedTask) -> SpawnMetadata {
    SpawnMetadata {
        session_id: task.session_id.clone(),
        task_id: task.task_id.clone(),
        child_pid: task.child_pid,
        child_start_time_ms: task.child_start_time_ms,
        remote: task.remote.is_some(),
    }
}

pub(crate) fn capture_spawn(task: &PersistedTask) {
    ticket::capture_spawn(&spawn_metadata(task));
}

pub(crate) fn seal_spawn(task: &PersistedTask) {
    ticket::seal_spawn(&spawn_metadata(task));
}

pub(crate) fn decode_cold_task(row: &str) -> Option<ColdTaskMetadata> {
    let task: PersistedTask = serde_json::from_str(row).ok()?;
    Some(ColdTaskMetadata {
        spawn: spawn_metadata(&task),
        status: task.status,
        harness: task.harness,
        scope: task.scope,
        project_root: task.project_root,
    })
}
