/** Shared pure formatting helpers for host bash tool wrappers. */

/** Render a millisecond duration as a compact seconds string (8000 -> "8s", 5500 -> "5.5s"). */
export function formatSeconds(ms: number): string {
  return `${Number((ms / 1000).toFixed(1))}s`;
}

export function formatForegroundResult(data: Record<string, unknown>): string {
  const output = (data.output_preview as string | undefined) ?? "";
  const outputPath = data.output_path as string | undefined;
  const truncated = data.output_truncated === true;
  const status = data.status as string | undefined;
  const exit = data.exit_code as number | undefined;
  let rendered = output;
  if (truncated && outputPath) {
    rendered += `\n[output truncated; full output at ${outputPath}]`;
  }
  if (status === "timed_out") {
    rendered += `\n[command timed out]`;
    // Name the limit that killed it, so AFT's own default limit is not
    // mistaken for the command failing.
    if (typeof data.status_reason === "string" && data.status_reason !== "") {
      rendered += ` ${data.status_reason}`;
    }
  }
  if (typeof exit === "number" && exit !== 0) {
    rendered += `\n[exit code: ${exit}]`;
  }
  return rendered;
}

export const TERMINAL_BG_TASK_STATUSES = Object.freeze([
  "completed",
  "failed",
  "killed",
  "timed_out",
  "fate_unknown",
] as const);

const terminalBgTaskStatusSet = new Set<string>(TERMINAL_BG_TASK_STATUSES);

export function isTerminalStatus(status: unknown): boolean {
  return typeof status === "string" && terminalBgTaskStatusSet.has(status);
}

export function isLiveBackgroundTaskReply(
  reply: unknown,
): reply is Record<string, unknown> & { task_id: string; status: string } {
  if (!reply || typeof reply !== "object" || Array.isArray(reply)) return false;
  const response = reply as Record<string, unknown>;
  return (
    typeof response.task_id === "string" &&
    response.task_id.length > 0 &&
    typeof response.status === "string" &&
    !terminalBgTaskStatusSet.has(response.status)
  );
}

export function sleep(ms: number): Promise<void> {
  return new Promise((resolve) => setTimeout(resolve, ms));
}

/**
 * Milliseconds on a monotonic clock, for measuring how long a wait held a call.
 * `Date.now()` follows the wall clock, which can step forward or back (NTP
 * correction, wake from sleep, a manual change); a wait deadline built on it
 * can expire early and then report the stepped distance as time waited.
 */
export function monotonicNowMs(): number {
  return performance.now();
}
