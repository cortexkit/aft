import { closeSync, openSync, readSync, statSync } from "node:fs";
import { isErrorLogLine } from "./issue-body.js";
import type { RecentSession } from "./sessions.js";

const MAX_SCAN_BYTES = 64 * 1024 * 1024;
const CHUNK_BYTES = 64 * 1024;
// Untagged engine lines often carry `root=/path` rather than a session tag.
// An explicit foreign path must not be attributed by timestamp alone.
const PROJECT_PATH = /\broot\s*=|(?:^|[\s"'(=])(?:\/[^\s"'(),]+|[A-Za-z]:[\\/][^\s"'(),]+)/;

export interface SessionLogResult {
  lines: string[];
  errors: string[];
  boundHit: boolean;
}

/** Read newest matching lines from the active log and its single rotated predecessor. */
export function readSessionLog(
  path: string,
  session: RecentSession,
  maxBytes = MAX_SCAN_BYTES,
): SessionLogResult {
  return readLogHistory([path, `${path}.1`], session, maxBytes);
}

/** Share the scan and output budgets across newest-first diagnostic log files. */
export function readLogHistory(
  files: string[],
  session: RecentSession | null,
  maxBytes = MAX_SCAN_BYTES,
): SessionLogResult {
  const lines: string[] = [];
  const errors: string[] = [];
  let remaining = maxBytes;
  let boundHit = false;
  const bareId = session?.id.replace(/^ses_/, "");
  const selectedTags = [`[ses_${bareId}]`, `[${bareId}]`];
  const tagPattern = /\[ses_[^\]\s]+\]|\[[0-9a-fA-F]{8}(?:-[0-9a-fA-F]{4}){3}-[0-9a-fA-F]{12}\]/;

  const accept = (line: string) => {
    if (!line) return;
    const tag = tagPattern.exec(line)?.[0];
    if (session && tag && !selectedTags.includes(tag)) return;
    if (session && !tag) {
      const stamp =
        /^\[([^\]]+)\]/.exec(line)?.[1] ??
        /^(\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}\.\d{3}Z) \[aft(?:-lsp)?\] /.exec(line)?.[1];
      const time = stamp ? Date.parse(stamp) : NaN;
      const inWindow =
        Number.isFinite(time) &&
        (session.startedAt === undefined || time >= session.startedAt) &&
        time <= session.lastActivity;
      if (session.projectRoot) {
        // When timestamps are available, a root mention outside the session's lifetime is not enough.
        if (Number.isFinite(time) && session.startedAt !== undefined && !inWindow) return;
        if (!line.includes(session.projectRoot) && (!inWindow || PROJECT_PATH.test(line))) return;
      } else if (!inWindow) {
        return;
      }
    }
    if (lines.length < 200) lines.push(line);
    if (errors.length < 20 && isErrorLogLine(line)) errors.push(line);
  };

  for (const file of files) {
    if (remaining <= 0 || (lines.length >= 200 && errors.length >= 20)) break;
    let fd: number | undefined;
    try {
      fd = openSync(file, "r");
      let position = statSync(file).size;
      let suffix = Buffer.alloc(0);
      // A complete final line is valid even without a trailing newline.
      while (position > 0 && remaining > 0 && (lines.length < 200 || errors.length < 20)) {
        const length = Math.min(CHUNK_BYTES, position, remaining);
        position -= length;
        remaining -= length;
        const chunk = Buffer.allocUnsafe(length);
        const count = readSync(fd, chunk, 0, length, position);
        const bytes = Buffer.concat([chunk.subarray(0, count), suffix]);
        let end = bytes.length;
        for (let i = bytes.length - 1; i >= 0; i -= 1) {
          if (bytes[i] !== 10) continue;
          accept(
            bytes
              .subarray(i + 1, end)
              .toString("utf8")
              .replace(/\r$/, ""),
          );
          end = i;
          if (lines.length >= 200 && errors.length >= 20) break;
        }
        suffix = bytes.subarray(0, end);
      }
      if (position === 0) accept(suffix.toString("utf8").replace(/\r$/, ""));
      else if (remaining === 0) boundHit = true; // The incomplete oldest line is discarded.
    } catch {
      // A missing or unreadable generation should not prevent reading the other one.
    } finally {
      if (fd !== undefined) closeSync(fd);
    }
  }
  return { lines: lines.reverse(), errors: errors.reverse(), boundHit };
}
