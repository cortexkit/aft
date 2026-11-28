//! Stable payloads carried by the management route's correlated frames.
//!
//! Codes deliberately remain strings so clients can display new values without
//! treating a newer provider as a protocol error. Missing facts are omitted.
use serde::{Deserialize, Serialize};

pub(in crate::subc) const CAPABILITY: &str = "shells/v1";
pub(super) const PAGE_SIZE: usize = 50;
pub(super) const RETENTION_MS: u64 = 30 * 60 * 1000;
pub(super) const DEFAULT_CHUNK_BYTES: usize = 16 * 1024;
pub(super) const MAX_CHUNK_BYTES: usize = 64 * 1024;
pub(super) const BACKFILL_BYTES: u64 = 64 * 1024;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct Holder {
    pub kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scope_ref: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub execution_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub harness: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct Remote {
    pub job_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
    pub phase: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct Offsets {
    pub stdout: u64,
    pub stderr: u64,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct Exit {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub code: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub signal: Option<i32>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct Row {
    pub task_id: String,
    pub revision: u64,
    pub holder: Holder,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub call_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    pub command: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub command_truncated: Option<bool>,
    pub cwd: String,
    pub kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub remote: Option<Remote>,
    pub state: String,
    pub started_at_ms: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_output_at_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ended_at_ms: Option<u64>,
    pub output_bytes: Offsets,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exit: Option<Exit>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub end_reason: Option<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Filter {
    pub agent_ids: Option<Vec<String>>,
    pub include_workers: Option<bool>,
    pub cursor: Option<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ListParams {
    pub agent_ids: Option<Vec<String>>,
    pub include_workers: Option<bool>,
    pub states: Option<Vec<String>>,
    pub cursor: Option<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct WatchParams {
    pub agent_ids: Option<Vec<String>>,
    pub include_workers: Option<bool>,
    pub cursor: Option<String>,
    pub status: Option<bool>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct OutputParams {
    pub task_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub from_offset: Option<Offsets>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ansi: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_chunk_bytes: Option<usize>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct ListReply {
    pub rows: Vec<Row>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unknown_agent_ids: Option<Vec<String>>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct Readiness {
    pub state: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct Lsp {
    pub server: String,
    pub state: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct RootStatus {
    pub root: String,
    pub agent_ids: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_ids_truncated: Option<bool>,
    pub text_index: Readiness,
    pub call_graph: Readiness,
    pub meaning: Readiness,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lsp: Option<Vec<Lsp>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub errors: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub warnings: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub todos: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct Aft {
    pub version: String,
    pub card: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct Status {
    pub aft: Aft,
    pub roots: Vec<RootStatus>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(super) enum WatchFrame {
    Resync {
        reason: String,
        part: u64,
        #[serde(rename = "final")]
        final_part: bool,
        rows: Vec<Row>,
        #[serde(skip_serializing_if = "Option::is_none")]
        status: Option<Status>,
        #[serde(skip_serializing_if = "Option::is_none")]
        cursor: Option<String>,
    },
    Upsert {
        row: Row,
        cursor: String,
    },
    Remove {
        task_id: String,
        cursor: String,
    },
    Status {
        status: Status,
        cursor: String,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct ScreenCursor {
    pub row: usize,
    pub col: usize,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(super) enum OutputFrame {
    Start {
        task_id: String,
        ansi: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        truncated_before: Option<Offsets>,
    },
    Chunk {
        stream: String,
        offset: u64,
        end_offset: u64,
        text: String,
    },
    Screen {
        rows: usize,
        cols: usize,
        lines: Vec<String>,
        cursor: ScreenCursor,
    },
    End {
        row: Row,
    },
}

pub(super) fn error(code: &str, message: &str) -> subc_protocol::ErrorBody {
    subc_protocol::ErrorBody::new(code, message.to_owned())
}
