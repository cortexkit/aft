//! Shared rows for owner-only observation of running background tasks.

use serde::{Deserialize, Serialize};
use subc_protocol::Principal;

pub(crate) const COMMAND_BYTES: usize = 2048;

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
#[doc(hidden)]
pub struct Selector {
    pub owner: Principal,
    #[serde(rename = "ref")]
    pub scope_ref: String,
    pub epoch: u64,
}

impl Selector {
    #[doc(hidden)]
    pub fn matches(&self, stamp: &subc_protocol::scope::ScopeStamp) -> bool {
        self.owner == stamp.owner
            && self.scope_ref == stamp.scope_ref
            && self.epoch == stamp.scope_epoch
    }
}

#[derive(Debug, Serialize)]
#[doc(hidden)]
pub struct Running {
    pub task_id: String,
    pub title: String,
    pub command: String,
    pub started_at_ms: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub remote: Option<Remote>,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub command_truncated: bool,
}

#[path = "scoped_tasks_types.rs"]
pub(crate) mod scoped_tasks_types;
pub(crate) use scoped_tasks_types::Remote;
pub(crate) fn running(metadata: &crate::bash_background::persistence::PersistedTask) -> Running {
    let mut command = crate::log_redact::redact_command(&metadata.command).into_owned();
    let command_truncated = command.len() > COMMAND_BYTES;
    if command_truncated {
        let mut end = COMMAND_BYTES;
        while !command.is_char_boundary(end) {
            end -= 1;
        }
        command.truncate(end);
    }
    Running {
        task_id: metadata.task_id.clone(),
        title: command.clone(),
        command,
        command_truncated,
        started_at_ms: metadata.started_at,
        remote: metadata
            .remote
            .as_ref()
            .and_then(|remote| remote.job_id)
            .map(|job_id| Remote {
                job_id: job_id.to_string(),
            }),
    }
}
