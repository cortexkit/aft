use serde::{Deserialize, Serialize};

use crate::bash_background::BgTaskStatus;

/// Full payload returned by the `status` command and cached by status push frames.
pub type StatusPayload = serde_json::Value;

/// v0.18 streaming semantics for hoisted bash.
///
/// Foreground `bash` execution may emit zero or more `progress` frames before
/// its final `Response`. Each progress frame is NDJSON on stdout with the same
/// `request_id` as the original request and a `kind` of `stdout` or `stderr`.
/// The final response remains the existing `{ id, success, ... }` envelope so
/// older callers can ignore streaming frames. Bash permission prompts use the
/// recognized `permission_required` error code; Phase 1 Track C will attach the
/// full permission ask payload and retry loop.
pub const ERROR_PERMISSION_REQUIRED: &str = "permission_required";

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProgressKind {
    Stdout,
    Stderr,
}

#[derive(Debug, Clone, Serialize)]
pub struct ProgressFrame {
    #[serde(rename = "type")]
    pub frame_type: &'static str,
    pub request_id: String,
    pub kind: ProgressKind,
    pub chunk: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct PermissionAskFrame {
    #[serde(rename = "type")]
    pub frame_type: &'static str,
    pub request_id: String,
    pub asks: serde_json::Value,
}

#[derive(Debug, Clone, Serialize)]
pub struct BashCompletedFrame {
    #[serde(rename = "type")]
    pub frame_type: &'static str,
    pub task_id: String,
    pub session_id: String,
    pub status: BgTaskStatus,
    pub exit_code: Option<i32>,
    pub command: String,
    /// Tail of stdout+stderr (≤300 bytes), already decoded as lossy UTF-8.
    /// Empty string when no output was captured. Used by plugins to inline
    /// short results in the system-reminder so agents don't need a follow-up
    /// `bash_status` round-trip for typical short commands.
    #[serde(default)]
    pub output_preview: String,
    /// True when the task produced more output than `output_preview` shows
    /// (rotated buffer, file > 300 bytes, etc). Plugins use this to render a
    /// `…` prefix and signal that `bash_status` would return more.
    #[serde(default)]
    pub output_truncated: bool,
    /// Token count of raw stdout+stderr before compression. Omitted when the
    /// payload exceeded the 128 KiB per-stream tokenization cap.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub original_tokens: Option<u32>,
    /// Token count of the compressed completion payload. Omitted when raw
    /// tokenization was skipped due to the cap.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub compressed_tokens: Option<u32>,
    /// True when output exceeded the tokenization cap and was not measured.
    #[serde(default)]
    pub tokens_skipped: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct BashLongRunningFrame {
    #[serde(rename = "type")]
    pub frame_type: &'static str,
    pub task_id: String,
    pub session_id: String,
    pub command: String,
    pub elapsed_ms: u64,
}

/// Pushed after configure has completed, when the deferred file walk and
/// language detection produce warnings (missing formatter/checker/LSP binaries,
/// or "project too large" file-count exceeded). The walk runs in a background
/// thread so configure itself returns in <100 ms even on huge directories
/// (e.g. user's $HOME). When the walk finishes, AFT pushes one frame with
/// the merged warnings — the plugin delivers them through the same path as
/// the synchronous warnings that configure used to return.
#[derive(Debug, Clone, Serialize)]
pub struct ConfigureWarningsFrame {
    #[serde(rename = "type")]
    pub frame_type: &'static str,
    /// Session id from the configure request that spawned the deferred walk.
    /// Project-shared bridges can serve multiple sessions, so plugins need this
    /// to route async warning notifications back to the initiating session.
    #[serde(default)]
    pub session_id: Option<String>,
    /// Project root the warnings refer to. Plugins use this to scope the
    /// session-id deduplication of repeated identical warnings.
    pub project_root: String,
    /// Source-file count discovered by the bounded walk (may stop short of
    /// the full count if `max_callgraph_files` is exceeded).
    pub source_file_count: usize,
    /// `true` when the walk hit the configured `max_callgraph_files` cap;
    /// in that case `source_file_count` is `cap + 1`.
    pub source_file_count_exceeds_max: bool,
    /// Configured callgraph file cap, echoed for plugin display.
    pub max_callgraph_files: usize,
    /// Merged formatter/checker/LSP missing-binary warnings.
    pub warnings: Vec<serde_json::Value>,
}

#[derive(Debug, Clone, Serialize)]
pub struct StatusChangedFrame {
    #[serde(rename = "type")]
    pub frame_type: &'static str,
    #[serde(default)]
    pub session_id: Option<String>,
    pub snapshot: StatusPayload,
}

#[derive(Debug, Clone, Serialize)]
#[serde(untagged)]
pub enum PushFrame {
    Progress(ProgressFrame),
    BashCompleted(BashCompletedFrame),
    BashLongRunning(BashLongRunningFrame),
    ConfigureWarnings(ConfigureWarningsFrame),
    StatusChanged(StatusChangedFrame),
}

impl PermissionAskFrame {
    pub fn new(request_id: impl Into<String>, asks: serde_json::Value) -> Self {
        Self {
            frame_type: "permission_ask",
            request_id: request_id.into(),
            asks,
        }
    }
}

impl ProgressFrame {
    pub fn new(
        request_id: impl Into<String>,
        kind: ProgressKind,
        chunk: impl Into<String>,
    ) -> Self {
        Self {
            frame_type: "progress",
            request_id: request_id.into(),
            kind,
            chunk: chunk.into(),
        }
    }
}

impl ConfigureWarningsFrame {
    pub fn new(
        project_root: impl Into<String>,
        source_file_count: usize,
        source_file_count_exceeds_max: bool,
        max_callgraph_files: usize,
        warnings: Vec<serde_json::Value>,
    ) -> Self {
        Self::new_with_session_id(
            None,
            project_root,
            source_file_count,
            source_file_count_exceeds_max,
            max_callgraph_files,
            warnings,
        )
    }

    pub fn new_with_session_id(
        session_id: Option<String>,
        project_root: impl Into<String>,
        source_file_count: usize,
        source_file_count_exceeds_max: bool,
        max_callgraph_files: usize,
        warnings: Vec<serde_json::Value>,
    ) -> Self {
        Self {
            frame_type: "configure_warnings",
            session_id,
            project_root: project_root.into(),
            source_file_count,
            source_file_count_exceeds_max,
            max_callgraph_files,
            warnings,
        }
    }
}

impl StatusChangedFrame {
    pub fn new(session_id: Option<String>, snapshot: StatusPayload) -> Self {
        Self {
            frame_type: "status_changed",
            session_id,
            snapshot,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;
    use serde_json::json;

    #[derive(Debug, Deserialize)]
    struct ConfigureWarningsFrameRoundTrip {
        #[serde(rename = "type")]
        frame_type: String,
        session_id: Option<String>,
        project_root: String,
        source_file_count: usize,
        max_callgraph_files: usize,
        warnings: Vec<serde_json::Value>,
    }

    #[test]
    fn configure_warnings_frame_serializes_null_session_id_by_default() {
        let frame = ConfigureWarningsFrame::new(
            "/repo",
            42,
            false,
            5_000,
            vec![json!({
                "kind": "formatter_not_installed",
                "tool": "biome",
                "hint": "Install biome."
            })],
        );

        let json = serde_json::to_string(&frame).expect("serialize ConfigureWarningsFrame");
        let decoded: ConfigureWarningsFrameRoundTrip =
            serde_json::from_str(&json).expect("deserialize ConfigureWarningsFrame JSON");

        assert_eq!(decoded.session_id, None);
    }

    #[test]
    fn configure_warnings_frame_serializes_session_id() {
        let frame = ConfigureWarningsFrame::new_with_session_id(
            Some("session-1".to_string()),
            "/repo",
            42,
            false,
            5_000,
            vec![json!({
                "kind": "formatter_not_installed",
                "tool": "biome",
                "hint": "Install biome."
            })],
        );

        let json = serde_json::to_string(&frame).expect("serialize ConfigureWarningsFrame");
        let decoded: ConfigureWarningsFrameRoundTrip =
            serde_json::from_str(&json).expect("deserialize ConfigureWarningsFrame JSON");

        assert_eq!(decoded.frame_type, "configure_warnings");
        assert_eq!(decoded.session_id.as_deref(), Some("session-1"));
        assert_eq!(decoded.project_root, "/repo");
        assert_eq!(decoded.source_file_count, 42);
        assert_eq!(decoded.max_callgraph_files, 5_000);
        assert_eq!(decoded.warnings[0]["tool"], "biome");
    }

    #[test]
    fn status_changed_frame_serializes_correctly() {
        let frame = StatusChangedFrame::new(
            None,
            json!({
                "version": "0.24.0",
                "project_root": "/repo",
                "cache_role": "main",
                "canonical_root": "/repo",
                "search_index": { "status": "ready" },
                "semantic_index": { "status": "disabled" },
            }),
        );

        let json = serde_json::to_value(PushFrame::StatusChanged(frame)).unwrap();
        assert_eq!(json["type"], "status_changed");
        assert!(json["session_id"].is_null());
        assert_eq!(json["snapshot"]["cache_role"], "main");
        assert_eq!(json["snapshot"]["project_root"], "/repo");
    }
}

impl BashCompletedFrame {
    pub fn new(
        task_id: impl Into<String>,
        session_id: impl Into<String>,
        status: BgTaskStatus,
        exit_code: Option<i32>,
        command: impl Into<String>,
        output_preview: impl Into<String>,
        output_truncated: bool,
        original_tokens: Option<u32>,
        compressed_tokens: Option<u32>,
        tokens_skipped: bool,
    ) -> Self {
        Self {
            frame_type: "bash_completed",
            task_id: task_id.into(),
            session_id: session_id.into(),
            status,
            exit_code,
            command: command.into(),
            output_preview: output_preview.into(),
            output_truncated,
            original_tokens,
            compressed_tokens,
            tokens_skipped,
        }
    }
}

impl BashLongRunningFrame {
    pub fn new(
        task_id: impl Into<String>,
        session_id: impl Into<String>,
        command: impl Into<String>,
        elapsed_ms: u64,
    ) -> Self {
        Self {
            frame_type: "bash_long_running",
            task_id: task_id.into(),
            session_id: session_id.into(),
            command: command.into(),
            elapsed_ms,
        }
    }
}

/// Fallback session identifier used when a request arrives without one.
///
/// Introduced alongside project-shared bridges (issue #14): one `aft` process
/// can now serve many OpenCode sessions in the same project. Undo/checkpoint
/// state is partitioned by session inside Rust, but callers that haven't been
/// updated to pass `session_id` (older plugins, direct CLI usage, tests) still
/// need to work — they share this default namespace.
///
/// Also used as the migration target for legacy pre-session backups on disk.
pub const DEFAULT_SESSION_ID: &str = "__default__";

/// Inbound request envelope.
///
/// Two-stage parse: deserialize this first to get `id` + `command`, then
/// dispatch on `command` and pull specific params from the flattened `params`.
#[derive(Debug, Deserialize)]
pub struct RawRequest {
    pub id: String,
    #[serde(alias = "method")]
    pub command: String,
    /// Optional LSP hints from the plugin (R031 forward compatibility).
    #[serde(default)]
    pub lsp_hints: Option<serde_json::Value>,
    /// Optional session namespace for undo/checkpoint isolation.
    ///
    /// When the plugin passes `session_id`, Rust partitions backup/checkpoint
    /// state by it so concurrent OpenCode sessions sharing one bridge can't
    /// see or restore each other's snapshots. When absent, falls back to
    /// [`DEFAULT_SESSION_ID`].
    #[serde(default)]
    pub session_id: Option<String>,
    /// All remaining fields are captured here for per-command deserialization.
    #[serde(flatten)]
    pub params: serde_json::Value,
}

impl RawRequest {
    /// Session namespace for this request, falling back to [`DEFAULT_SESSION_ID`]
    /// when the plugin didn't supply one.
    pub fn session(&self) -> &str {
        self.session_id.as_deref().unwrap_or(DEFAULT_SESSION_ID)
    }
}

/// Outbound response envelope.
///
/// `data` is flattened into the top-level JSON object, so a response like
/// `Response { id: "1", success: true, data: json!({"command": "pong"}) }`
/// serializes to `{"id":"1","success":true,"command":"pong"}`.
///
/// # Honest reporting convention (tri-state)
///
/// Tools that search, check, or otherwise produce results MUST follow this
/// convention so agents can distinguish "did the work, found nothing" from
/// "couldn't do the work" from "partially did the work":
///
/// 1. **`success: false`** — the requested work could not be performed.
///    Includes a `code` (e.g., `"path_not_found"`, `"no_lsp_server"`,
///    `"project_too_large"`) and a human-readable `message`. The agent
///    should treat this as an error and read the message.
///
/// 2. **`success: true` + completion signaling** — the work was performed.
///    Tools must report whether the result is *complete* OR which subset
///    was actually performed. Conventional fields:
///    - `complete: true` — full result, agent can trust absence of items
///    - `complete: false` + `pending_files: [...]` / `unchecked_files: [...]`
///      / `scope_warnings: [...]` — partial result, with named gaps
///    - `removed: true|false` (for mutations) — did the file actually change
///    - `skipped_files: [{file, reason}]` — files we couldn't process inside
///      the requested scope
///    - `no_files_matched_scope: bool` — the scope (path/glob) found zero
///      candidates (distinct from "candidates found, no matches")
///
/// 3. **Side-effect skip codes** — when the main work succeeded but a
///    non-essential side step was skipped (e.g., post-write formatting),
///    use a `<step>_skipped_reason` field. Approved values:
///    - `format_skipped_reason`: `"unsupported_language"` |
///      `"no_formatter_configured"` | `"formatter_not_installed"` |
///      `"formatter_excluded_path"` | `"timeout"` | `"error"`
///    - `validate_skipped_reason`: `"unsupported_language"` |
///      `"no_checker_configured"` | `"checker_not_installed"` |
///      `"timeout"` | `"error"`
///
/// **Anti-patterns to avoid:**
/// - Returning `success: true` with empty results when the scope didn't
///   resolve to any files — agent reads as "all clear" but really nothing
///   was checked. Use `no_files_matched_scope: true` or
///   `success: false, code: "path_not_found"`.
/// - Reusing `format_skipped_reason: "not_found"` for two different causes
///   ("no formatter configured" vs "configured formatter binary missing").
///   The agent can't act on the ambiguous code.
///
/// See ARCHITECTURE.md "Honest reporting convention" for the full rationale.
#[derive(Debug, Serialize)]
pub struct Response {
    pub id: String,
    pub success: bool,
    #[serde(flatten)]
    pub data: serde_json::Value,
}

/// Parameters for the `echo` command.
#[derive(Debug, Deserialize)]
pub struct EchoParams {
    pub message: String,
}

impl Response {
    /// Build a success response with arbitrary data merged at the top level.
    pub fn success(id: impl Into<String>, data: serde_json::Value) -> Self {
        Response {
            id: id.into(),
            success: true,
            data,
        }
    }

    /// Build an error response with `code` and `message` fields.
    pub fn error(id: impl Into<String>, code: &str, message: impl Into<String>) -> Self {
        Response {
            id: id.into(),
            success: false,
            data: serde_json::json!({
                "code": code,
                "message": message.into(),
            }),
        }
    }

    /// Build an error response with `code`, `message`, and additional structured data.
    ///
    /// The `extra` fields are merged into the top-level response alongside `code` and `message`.
    pub fn error_with_data(
        id: impl Into<String>,
        code: &str,
        message: impl Into<String>,
        extra: serde_json::Value,
    ) -> Self {
        let mut data = serde_json::json!({
            "code": code,
            "message": message.into(),
        });
        if let (Some(base), Some(ext)) = (data.as_object_mut(), extra.as_object()) {
            for (k, v) in ext {
                base.insert(k.clone(), v.clone());
            }
        }
        Response {
            id: id.into(),
            success: false,
            data,
        }
    }
}
