import type { BashWatchLineBatch } from "./bridge.js";

const MAX_PENDING_WATCH_LINES = 50;
const DEFAULT_MASKING_LINE_CAP_BYTES = 8 * 1024;
export const UNAVAILABLE_MATCH_SINGULAR =
  "1 matching line over {limit} KiB was not shown; AFT can check lines only up to {limit} KiB for secrets.";
export const UNAVAILABLE_MATCH_PLURAL =
  "{count} matching lines over {limit} KiB were not shown; AFT can check lines only up to {limit} KiB for secrets.";

function unavailableMatchText(count: number, limit: number): string {
  return (count === 1 ? UNAVAILABLE_MATCH_SINGULAR : UNAVAILABLE_MATCH_PLURAL)
    .replaceAll("{count}", String(count))
    .replaceAll("{limit}", String(Math.ceil(limit / 1024)));
}
export const WATCH_EACH_LINE_NEEDS_BACKGROUND =
  'bash_watch: deliver: "each_line" needs background: true.';

/** Live pushes are deltas; durable replay is a replacement snapshot. */
export function mergeWatchLineBatches(
  previous: BashWatchLineBatch,
  next: BashWatchLineBatch,
): BashWatchLineBatch {
  if (next.snapshot) return next;
  if (previous.pending_id && next.pending_id && previous.pending_id !== next.pending_id)
    return next;
  const merged = { ...previous, lines: previous.lines.map((line) => ({ ...line })) };
  merged.masking_limit_bytes = next.masking_limit_bytes ?? previous.masking_limit_bytes;
  merged.pending_id = next.pending_id ?? previous.pending_id;
  merged.version = next.version ?? previous.version;
  if (next.retry_gap) merged.retry_gap = next.retry_gap;
  let retained = merged.lines.reduce((sum, line) => sum + line.count, 0);
  for (const line of next.lines) {
    const keep = Math.min(line.count, MAX_PENDING_WATCH_LINES - retained);
    const dropped = line.count - keep;
    if (dropped > 0 && merged.first_dropped_offset === null) {
      merged.first_dropped_offset = line.offset + keep * (line.end_offset - line.offset);
      merged.first_dropped_stream = line.stream;
    }
    merged.dropped_lines += dropped;
    if (keep > 0) {
      const last = merged.lines.at(-1);
      if (
        last &&
        last.stream === line.stream &&
        last.text === line.text &&
        Boolean(last.unavailable) === Boolean(line.unavailable) &&
        last.end_offset - last.offset === line.end_offset - line.offset &&
        last.offset + last.count * (last.end_offset - last.offset) === line.offset
      )
        last.count += keep;
      else merged.lines.push({ ...line, count: keep });
      retained += keep;
    }
  }
  merged.dropped_lines += next.dropped_lines;
  merged.skipped_long_lines += next.skipped_long_lines;
  if (merged.first_dropped_offset === null) {
    merged.first_dropped_offset = next.first_dropped_offset;
    merged.first_dropped_stream = next.first_dropped_stream;
  }
  return merged;
}

/** Render masked source lines and copyable, host-specific byte-page recovery calls. */
export function formatWatchLineBatch(
  taskId: string,
  batch: BashWatchLineBatch,
  camelCase: boolean,
): string {
  const lines = batch.lines.map(
    (line) =>
      `${line.stream} byte ${line.offset}${line.count > 1 ? ` ×${line.count}` : ""}: ${line.unavailable ? unavailableMatchText(line.count, batch.masking_limit_bytes ?? DEFAULT_MASKING_LINE_CAP_BYTES) : line.text}`,
  );
  if (batch.dropped_lines > 0 || batch.skipped_long_lines > 0) {
    const taskKey = camelCase ? "taskId" : "task_id";
    const cursor = batch.first_dropped_stream === "stderr" ? "stderr" : "output";
    const offsetKey = camelCase ? `${cursor}Offset` : `${cursor}_offset`;
    const incomplete =
      batch.skipped_long_lines > 0
        ? ` ${batch.skipped_long_lines} lines longer than 64 KiB were skipped, so the scan is incomplete.`
        : "";
    lines.push(
      `${batch.dropped_lines} matching lines dropped.${incomplete} Read them with bash_status({ ${taskKey}: ${JSON.stringify(taskId)}, ${offsetKey}: ${batch.first_dropped_offset ?? 0} }).`,
    );
  }
  if (batch.retry_gap && batch.retry_gap.dropped_notifications > 0) {
    const gap = batch.retry_gap;
    const taskKey = camelCase ? "taskId" : "task_id";
    const cursor = gap.stream === "stderr" ? "stderr" : "output";
    const offsetKey = camelCase ? `${cursor}Offset` : `${cursor}_offset`;
    const missing =
      gap.known_missing_occurrences === 0
        ? "Some matches may be missing."
        : gap.known_missing_occurrences === 1
          ? "At least 1 match is missing."
          : `At least ${gap.known_missing_occurrences} matches are missing.`;
    lines.push(
      `Some watch notices were lost: a stalled connection dropped ${gap.dropped_notifications} of them, and they cannot be recovered. ${missing} Read the task output with bash_status({ ${taskKey}: ${JSON.stringify(taskId)}, ${offsetKey}: ${gap.first_offset ?? 0} }).`,
    );
  }
  return lines.join("\n");
}

/** Page text is masked by the engine; never decode raw transport bytes for display. */
export function formatBashOutputPages(data: Record<string, unknown>, camelCase: boolean): string {
  const lines: string[] = [];
  for (const stream of ["output", "stderr"]) {
    if (typeof data[`${stream}_page_text`] !== "string") continue;
    const next = camelCase ? `${stream}Offset` : `${stream}_offset`;
    lines.push(
      `${stream} page (next ${next}: ${data[`${stream}_next_offset`]}, has_more: ${data[`${stream}_has_more`]}):\n${data[`${stream}_page_text`]}`,
    );
  }
  return lines.join("\n");
}
