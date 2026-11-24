//! Shell task metadata needed to verify a GitHub bot-write ticket.

use std::path::PathBuf;

use subc_protocol::scope::ScopeStamp;

use crate::protocol::BgTaskStatus;

#[doc(hidden)]
pub struct SpawnMetadata {
    pub session_id: String,
    pub task_id: String,
    pub child_pid: Option<u32>,
    pub child_start_time_ms: Option<u64>,
    pub remote: bool,
}

#[doc(hidden)]
pub struct ColdTaskMetadata {
    pub spawn: SpawnMetadata,
    pub status: BgTaskStatus,
    pub harness: Option<String>,
    pub scope: Option<ScopeStamp>,
    pub project_root: Option<PathBuf>,
}

/// Checks the complete shell-owned record before projecting its ticket fields.
#[doc(hidden)]
pub type ColdTaskDecoder = fn(&str) -> Option<ColdTaskMetadata>;
