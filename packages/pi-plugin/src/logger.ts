import * as fs from "node:fs";
import * as os from "node:os";
import * as path from "node:path";

const TAG = "[aft-pi]";

const isTestEnv = process.env.BUN_TEST === "1" || process.env.NODE_ENV === "test";
const logFile = path.join(os.tmpdir(), isTestEnv ? "aft-pi-test.log" : "aft-pi.log");

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
 * specific Pi session (per-request errors, timeouts, crashes during a
 * session's tool call). Bridge-lifecycle logs (spawn, version, idle) are
 * project-scoped, not session-scoped — use `log`/`warn`/`error` for those.
 */
export function sessionLog(sessionId: string, message: string, data?: unknown): void {
  write("INFO", message, data, sessionId);
}

export function sessionWarn(sessionId: string, message: string, data?: unknown): void {
  write("WARN", message, data, sessionId);
}

export function sessionError(sessionId: string, message: string, data?: unknown): void {
  write("ERROR", message, data, sessionId);
}

export function getLogFilePath(): string {
  return logFile;
}

/**
 * Adapter that exposes this logger as a {@link import("@cortexkit/aft-bridge").Logger}
 * for the shared bridge package.
 */
export const bridgeLogger = {
  log(message: string, meta?: { sessionId?: string }) {
    if (meta?.sessionId) sessionLog(meta.sessionId, message);
    else write("INFO", message);
  },
  warn(message: string, meta?: { sessionId?: string }) {
    if (meta?.sessionId) sessionWarn(meta.sessionId, message);
    else write("WARN", message);
  },
  error(message: string, meta?: { sessionId?: string }) {
    if (meta?.sessionId) sessionError(meta.sessionId, message);
    else write("ERROR", message);
  },
  getLogFilePath: () => logFile,
};
