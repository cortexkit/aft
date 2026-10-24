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

export interface StatusRemovalHealth {
  available: boolean;
  usage_window_days?: number;
  project_roots_served?: number;
  sessions_served?: number;
  project_roots_source?: "durable_project_keys_approximation" | string;
  running_background_tasks?: number;
  undo_history_sessions?: number;
  message?: string;
}

export interface StatusResponse extends AftSuccessResponse {
  compression?: StatusCompression;
  /** Present only for the read-only removal-time status request used by doctor. */
  removal?: StatusRemovalHealth;
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
  _bash_changed_files?: BashChangedFiles;
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
  status_reason?: string;
  /** Capture was incomplete; status and exit_code still describe the command. */
  output_incomplete?: boolean;
  live_descendants?: LiveDescendant[] | null;
  live_descendants_omitted?: number;
  /** Why `live_descendants` is null for a finished task: the group could not be checked. */
  live_descendants_unavailable?: LiveDescendantsUnavailable;
  live_descendants_summary?: string;
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
  _bash_changed_files?: BashChangedFiles;
  task_id: string;
  status: string;
  exit_code: number | null;
  command: string;
  duration_ms?: number;
  runtime_ms?: number;
  runtime?: number;
  /**
   * Exit-aware preview of stdout+stderr captured at completion: success =
   * short tail (≤600 B), failure = small head + larger tail (≤2.25 KiB).
   */
  output_preview?: string;
  /** True when the captured tail is shorter than the actual output. */
  output_truncated?: boolean;
  /** Token counts arrive in v0.27 but remain unused by plugins until commit 13. */
  original_tokens?: number;
  compressed_tokens?: number;
  tokens_skipped?: boolean;
  status_reason?: string;
  /** Capture was incomplete; status and exit_code still describe the command. */
  output_incomplete?: boolean;
  live_descendants?: LiveDescendant[] | null;
  live_descendants_omitted?: number;
  /** Why `live_descendants` is null for a finished task: the group could not be checked. */
  live_descendants_unavailable?: LiveDescendantsUnavailable;
  live_descendants_summary?: string;
}

/**
 * A process still running in a finished task's process group, as AFT lists
 * it after the task ends or is killed.
 */
export interface LiveDescendant {
  pid: number;
  comm: string;
  /** First word of the process's first argument. */
  argv0: string;
  /** Owner of the process (real user ID), or the numeric ID. Absent when unreadable. */
  user?: string;
  /**
   * Executable name, followed by "…" when the process had arguments. The
   * arguments themselves are never sent: they can hold credentials.
   */
  command?: string;
}

/**
 * Why AFT could not list a finished task's process group, in the order the
 * engine declares them. A list, even an empty one, always means it checked.
 */
export const LIVE_DESCENDANTS_UNAVAILABLE_REASONS = Object.freeze([
  "unsupported_platform",
  "no_process_group",
  "listing_failed",
  "listing_incomplete",
  "not_checked",
] as const);

export type LiveDescendantsUnavailable = (typeof LIVE_DESCENDANTS_UNAVAILABLE_REASONS)[number];

export const TERMINAL_BG_TASK_STATUSES = Object.freeze([
  "completed",
  "failed",
  "killed",
  "timed_out",
  "fate_unknown",
] as const);

const TERMINAL_BASH_STATUSES = new Set<string>(TERMINAL_BG_TASK_STATUSES);

/** Daemon terminal states; standalone compatibility callers can opt into historical aliases. */
export function isTerminalBashStatus(status: unknown, compatibilityAliases = false): boolean {
  return (
    typeof status === "string" &&
    (TERMINAL_BASH_STATUSES.has(status) ||
      (compatibilityAliases && (status === "cancelled" || status === "timeout")))
  );
}

/** Watcher-observed project changes during one shell task. */
export interface BashChangedFiles {
  text: string;
  files: Array<{ path: string; status: string; diff?: string; reason?: string }>;
  incomplete: string[];
}
