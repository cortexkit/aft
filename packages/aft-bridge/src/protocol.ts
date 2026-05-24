/**
 * Wire-protocol types shared between the AFT plugins and the Rust `aft` binary.
 *
 * These mirror the structures defined in `crates/aft/src/protocol.rs`.
 * Tool-specific request/response shapes intentionally stay in plugin code —
 * this module owns only the envelope and the host-relevant push frames so that
 * the transport never needs to know about every command schema.
 */

export interface AftRequestEnvelope {
  /** Stable request id, returned in the matching response. */
  id: string;
  /** Bridge command name (e.g. `"bash"`, `"read"`). */
  command: string;
  /** Optional session id; partitions backup/checkpoint state per-session. */
  session_id?: string;
  /** Optional LSP hint payload threaded through edit-time diagnostics. */
  lsp_hints?: unknown;
  /** Tool-specific parameters live alongside the envelope at the top level. */
  [key: string]: unknown;
}

export interface AftSuccessResponse {
  id: string;
  success: true;
  /** Tool-specific result fields live on the same object. */
  [key: string]: unknown;
}

export interface AftErrorResponse {
  id: string;
  success: false;
  /** Machine-actionable error code (e.g. `"path_not_found"`). */
  code?: string;
  /** Human-readable error message. */
  message?: string;
  /** Tool-specific error metadata may be attached. */
  [key: string]: unknown;
}

export type AftResponse = AftSuccessResponse | AftErrorResponse;

export interface StatusCompressionAggregate {
  events: number;
  original_tokens: number;
  compressed_tokens: number;
  savings_tokens: number;
}

export interface StatusCompression {
  project: StatusCompressionAggregate;
  session: StatusCompressionAggregate;
}

export interface StatusResponse extends AftSuccessResponse {
  compression?: StatusCompression;
}

/**
 * Server-pushed frames (no client-side `id`). The transport recognises these
 * and dispatches them through {@link BridgeOptions.onPushFrame} rather than
 * matching them against a pending request.
 */
export type AftPushFrame =
  | BashCompletedFrame
  | BashLongRunningFrame
  | PermissionAskFrame
  | ProgressFrame
  | ConfigureWarningFrame
  | StatusChangedFrame;

export interface BashCompletedFrame {
  type: "bash_completed";
  task_id: string;
  status: string;
  exit_code: number | null;
  command: string;
  duration_ms?: number;
  runtime_ms?: number;
  runtime?: number;
  output_preview?: string;
  output_truncated?: boolean;
  original_tokens?: number;
  compressed_tokens?: number;
  tokens_skipped?: boolean;
  session_id?: string;
}

export interface BashLongRunningFrame {
  type: "bash_long_running";
  task_id: string;
  session_id: string;
  command: string;
  elapsed_ms: number;
}

export interface PermissionAskFrame {
  type: "permission_ask";
  request_id: string;
  prompt: string;
  options?: string[];
  session_id?: string;
}

export interface ProgressFrame {
  type: "progress";
  task_id?: string;
  message?: string;
  [key: string]: unknown;
}

export interface ConfigureWarningFrame {
  type: "configure_warning";
  code?: string;
  message: string;
  [key: string]: unknown;
}

export interface StatusChangedFrame {
  type: "status_changed";
  session_id?: string | null;
  snapshot: Record<string, unknown>;
}

/**
 * Background-bash completion record carried in `bash_drain_completions`
 * responses and `bash_completed` push frames. Plugins consume this through
 * their own bg-notification machinery.
 */
export interface BgCompletion {
  task_id: string;
  status: string;
  exit_code: number | null;
  command: string;
  duration_ms?: number;
  runtime_ms?: number;
  runtime?: number;
  /** Tail of stdout+stderr captured at completion (≤300 bytes from Rust). */
  output_preview?: string;
  /** True when the captured tail is shorter than the actual output. */
  output_truncated?: boolean;
  /** Token counts arrive in v0.27 but remain unused by plugins until commit 13. */
  original_tokens?: number;
  compressed_tokens?: number;
  tokens_skipped?: boolean;
}
