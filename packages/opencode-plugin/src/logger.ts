import * as fs from "node:fs";
import * as os from "node:os";
import * as path from "node:path";

const TAG = "[aft-plugin]";

// Route test runs to a separate log file so `bun test` never pollutes the
// live session log that users read to diagnose problems. Bun sets BUN_TEST=1
// automatically; NODE_ENV=test covers other test harnesses.
const isTestEnv = process.env.BUN_TEST === "1" || process.env.NODE_ENV === "test";
const logFile = path.join(os.tmpdir(), isTestEnv ? "aft-plugin-test.log" : "aft-plugin.log");

/**
 * When AFT_LOG_STDERR=1, logs go to stderr (useful for subprocess tests that
 * capture stderr output). Otherwise logs go to the temp file.
 */
const useStderr = process.env.AFT_LOG_STDERR === "1";

let buffer: string[] = [];
let flushTimer: ReturnType<typeof setTimeout> | null = null;
const FLUSH_INTERVAL_MS = 500;
const BUFFER_SIZE_LIMIT = 50;

function flush(): void {
  if (buffer.length === 0) return;
  const data = buffer.join("");
  buffer = [];
  try {
    if (useStderr) {
      process.stderr.write(data);
    } else {
      fs.appendFileSync(logFile, data);
    }
  } catch {
    // Intentional: logging must never throw
  }
}

function scheduleFlush(): void {
  if (flushTimer) return;
  flushTimer = setTimeout(() => {
    flushTimer = null;
    flush();
  }, FLUSH_INTERVAL_MS);
  // Don't prevent Node from exiting
  if (flushTimer && typeof flushTimer === "object" && "unref" in flushTimer) {
    flushTimer.unref();
  }
}

function write(level: string, message: string, data?: unknown, sessionId?: string): void {
  try {
    const timestamp = new Date().toISOString();
    const serialized = data === undefined ? "" : ` ${JSON.stringify(data)}`;
    const sessionPrefix = sessionId ? ` [${sessionId}]` : "";
    const line = `[${timestamp}] ${level} ${TAG}${sessionPrefix} ${message}${serialized}\n`;
    if (useStderr) {
      // Write immediately in stderr mode (subprocess tests need it before exit)
      process.stderr.write(line);
      return;
    }
    buffer.push(line);
    if (buffer.length >= BUFFER_SIZE_LIMIT) {
      flush();
    } else {
      scheduleFlush();
    }
  } catch {
    // Intentional: logging must never throw
  }
}
export function log(message: string, data?: unknown): void {
  write("INFO", message, data);
}

export function warn(message: string, data?: unknown): void {
  write("WARN", message, data);
}

export function error(message: string, data?: unknown): void {
  write("ERROR", message, data);
}

/**
 * Log with a session-id prefix. Use for messages that originate from a
 * specific OpenCode session (per-request errors, timeouts, crashes during
 * a session's tool call). Bridge-lifecycle logs (spawn, version, idle) are
 * project-scoped, not session-scoped — use `log`/`warn`/`error` for those.
 */
export function sessionLog(sessionId: string | undefined, message: string, data?: unknown): void {
  write("INFO", message, data, sessionId);
}

export function sessionWarn(sessionId: string | undefined, message: string, data?: unknown): void {
  write("WARN", message, data, sessionId);
}

export function sessionError(sessionId: string | undefined, message: string, data?: unknown): void {
  write("ERROR", message, data, sessionId);
}

export function getLogFilePath(): string {
  return logFile;
}

/**
 * Adapter that exposes this logger as a {@link import("@cortexkit/aft-bridge").Logger}
 * for the shared bridge package. The bridge package never knows about this log
 * file or the `[aft-plugin]` tag — it just calls `log/warn/error` and we map
 * its `LogMeta.sessionId` into our internal session-prefix shape.
 */
export const bridgeLogger = {
  log(message: string, meta?: { sessionId?: string }) {
    sessionLog(meta?.sessionId, message);
  },
  warn(message: string, meta?: { sessionId?: string }) {
    sessionWarn(meta?.sessionId, message);
  },
  error(message: string, meta?: { sessionId?: string }) {
    sessionError(meta?.sessionId, message);
  },
  getLogFilePath: () => logFile,
};
