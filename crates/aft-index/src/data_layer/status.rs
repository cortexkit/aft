pub use aft_base::semantic_status::*;
use std::time::Duration;

use serde::Serialize;

/// Degraded reason recorded while a project's checkout view fails to load.
/// Cleared by the next successful load; the history counts are kept.
/// Rendered as-is by status surfaces, like the other degraded reasons.
pub(crate) const VIEW_LOAD_DEGRADED_REASON: &str = "view_load_failed";

/// Add reporting guidance only when a view-load failure becomes an agent reply.
/// Busy loads and measured disk-reserve refusals are expected conditions,
/// not internal errors that need a bug report.
pub(crate) fn view_load_failure_message(error: impl std::fmt::Display) -> String {
    let error = error.to_string();
    if crate::view_storage_reserve::is_message(&error) {
        return error;
    }
    format!("{error}\nThis is an AFT bug: tell the user to report it with `npx @cortexkit/aft doctor --issue`; don't run it yourself.")
}

/// Render an index read failure as the call's error. No other index or
/// filesystem walk answers in its place.
#[doc(hidden)]
pub fn view_load_error_response(id: &str, message: String) -> crate::protocol::Response {
    crate::protocol::Response::error(id, "internal_error", message)
}

/// Error-level log throttle: one line per project and plane per minute.
/// Repeats inside the window are counted and reported on the next emission.
pub(crate) const VIEW_LOAD_LOG_INTERVAL: Duration = Duration::from_secs(60);

/// The most recent checkout view load failure of a project. Planes are the
/// query planes a failed load serves: `trigram` for the lexical view behind
/// grep, glob and aft_search, `callgraph` for the graph behind navigation,
/// zoom and inspect, and `semantic` when a built semantic view cannot serve
/// or score a query. The semantic lane still answers `unavailable` with its
/// cause and the query continues lexically; health counts the failure too.
/// Pending fill coverage is normal progress and is not counted.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct ViewLoadFailure {
    pub plane: String,
    /// Bucketed cause (`busy`, `io`, `invalid`, `unavailable` or `other`);
    /// the full error stays in `error`.
    pub cause: String,
    pub error: String,
    /// Failures with this plane and cause since this process started.
    pub count: u64,
    /// All view load failures since this process started.
    pub total: u64,
    pub last_at_ms: u64,
}

/// Bucket a view load error into a stable short cause. I/O failures already
/// name their path and operation; verification and manifest problems name
/// what mismatches or is missing; the full text stays in the recorded error.
pub(crate) fn view_load_cause(error: &str) -> &'static str {
    let error = error.to_lowercase();
    if error.contains("busy") {
        "busy"
    } else if error.contains("view i/o failed") {
        "io"
    } else if error.contains("invalid")
        || error.contains("mismatch")
        || error.contains("missing")
        || error.contains("unreadable")
        || error.contains("unverified")
        || error.contains("verification")
    {
        "invalid"
    } else if error.contains("unavailable")
        || error.contains("not ready")
        || error.contains("not built")
        || error.contains("disabled")
    {
        "unavailable"
    } else {
        "other"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn view_load_cause_buckets_errors_by_kind() {
        // Transient contention, whatever database reports it.
        assert_eq!(
            view_load_cause("callgraph derived store is busy: database is locked"),
            "busy"
        );
        assert_eq!(
            view_load_cause("view loader [driver]: v3 store busy: members.sqlite: checkpoint WAL"),
            "busy"
        );
        // I/O errors name their path and operation.
        assert_eq!(
            view_load_cause(
                "view loader [driver]: view I/O failed reading /root/a.ts: No such file"
            ),
            "io"
        );
        // Verification and manifest problems.
        assert_eq!(
            view_load_cause("invalid view manifest: duplicate member"),
            "invalid"
        );
        assert_eq!(
            view_load_cause("view producer mismatch: trigram policy changed"),
            "invalid"
        );
        assert_eq!(
            view_load_cause("view generation abc manifest unreadable: gone"),
            "invalid"
        );
        assert_eq!(
            view_load_cause("manifest references missing trigram blob deadbeef"),
            "invalid"
        );
        assert_eq!(
            view_load_cause("lexical view has unverified writes"),
            "invalid"
        );
        // Loads that cannot serve yet or anymore.
        assert_eq!(
            view_load_cause("lexical payload not ready: /root/a.ts"),
            "unavailable"
        );
        assert_eq!(
            view_load_cause(
                "callgraph view unavailable: view loader [reconciliation]: walked away"
            ),
            "unavailable"
        );
        assert_eq!(
            view_load_cause(
                "call graph is disabled (indexes.callgraph=false): this view generation was published without call graph data"
            ),
            "unavailable"
        );
        assert_eq!(
            view_load_cause("lexical source deliberately refused"),
            "other"
        );
    }
}

#[derive(Debug, Clone, serde::Serialize, PartialEq, Eq)]
pub struct SemanticHealthComponentSnapshot {
    pub status: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub since_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_retry_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub paused_until_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stage: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub embedded_chunks: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total_chunks: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub current_batch: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total_batches: Option<usize>,
    /// The data layer's meaning plane: how far its fill has got and what it
    /// could not embed. Present only while that plane reports for the root.
    #[serde(flatten)]
    pub fill: Option<SemanticFillHealth>,
}

/// The fill of the data layer's meaning plane as each root's health carries
/// it inside `semantic_index`, beside the status, `stage` and `reason`.
/// Health replies are held to a byte budget, so only what says something is
/// present: `files` while files still wait for vectors, every other count
/// only when it is above zero, `filling` only while true, and
/// `last_progress_at_ms` only while the plane is building. A fill that
/// waits for another fill of the repository says so in `stage`
/// (`waiting_for_another_fill`).
#[derive(Debug, Clone, Default, serde::Serialize, PartialEq, Eq)]
pub struct SemanticFillHealth {
    /// Files meaning search applies to.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub files: Option<usize>,
    /// Files whose every chunk has a vector or a recorded failure.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub files_embedded: Option<usize>,
    /// Files within `semantic.max_files` still waiting for vectors.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub files_pending: Option<usize>,
    /// Files over the size limit, which get no chunks.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub files_too_large: Option<usize>,
    /// Files past `semantic.max_files` that no checkout has embedded.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub files_past_cap: Option<usize>,
    /// Files that could not be read or cut into chunks.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub files_failed: Option<usize>,
    /// Chunks the embedder refused.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub chunks_failed: Option<usize>,
    /// A fill round is queued or running.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub filling: Option<bool>,
    /// While the plane is building: when a fill last stored vectors and
    /// chunk maps, in milliseconds since the Unix epoch, as the fill itself
    /// recorded it. Health's stuck-plane check reads it with `since_ms`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_progress_at_ms: Option<u64>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EmbeddingBackendHealthSnapshot {
    pub available: bool,
    pub last_error: Option<String>,
    pub since_ms: Option<u64>,
    pub(crate) next_retry_ms: Option<u64>,
    pub paused_until_ms: Option<u64>,
    /// Why the backend refused requests for now, when `paused_until_ms` is set.
    pub pause_cause: Option<crate::synapse_embed::PauseCause>,
}

pub enum SemanticIndexEvent {
    Progress {
        stage: String,
        files: Option<usize>,
        entries_done: Option<usize>,
        entries_total: Option<usize>,
    },
    /// Emitted when the semantic worker avoids or pauses full project corpus
    /// collection before reaching terminal Ready/Failed, such as after loading a
    /// cached index or while waiting to retry an embedding backend with no vectors
    /// retained. Work that was waiting for the full index can proceed.
    ColdSeedGateCleared,
    Ready(crate::semantic_index::SemanticIndex),
    Failed(String),
}
