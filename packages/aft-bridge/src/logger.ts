/**
 * Host-injected logger contract.
 *
 * The shared bridge package never owns a log file, log-format, or tag — every
 * host (OpenCode, Pi, future MCP harnesses) has its own log conventions and
 * operators rely on the per-host filename for support flows like
 * `aft doctor --issue`.
 *
 * The transport consumes only the {@link Logger} interface; plugins implement
 * it on top of their own file/console handler.
 */
export interface Logger {
  log(message: string, meta?: LogMeta): void;
  warn(message: string, meta?: LogMeta): void;
  error(message: string, meta?: LogMeta): void;
  /**
   * Optional. When implemented, returns the path to the on-disk log file so
   * bridge-side error messages can point operators directly at it.
   */
  getLogFilePath?(): string | undefined;
}

export interface LogMeta {
  /** Optional session id for correlating multi-session host activity. */
  sessionId?: string;
  /** Optional structured metadata (pid, exit code, etc.). */
  [key: string]: unknown;
}
