/**
 * Internal helper that lets shared modules call simple `log/warn/error`
 * functions without threading a {@link Logger} through every signature.
 *
 * The host (OpenCode plugin, Pi plugin) calls {@link setActiveLogger} once at
 * startup before constructing any {@link BridgePool}. Internal callers use
 * {@link log}/{@link warn}/{@link error} which forward to the active logger.
 *
 * If no logger has been set, calls fall back to `console.error` so we never
 * silently drop diagnostics.
 */
import type { Logger, LogMeta } from "./logger.js";

const ACTIVE_LOGGER_SYMBOL = Symbol.for("aft-bridge-active-logger");

interface ActiveLoggerGlobal {
  [ACTIVE_LOGGER_SYMBOL]?: Logger;
}

function loggerGlobal(): ActiveLoggerGlobal {
  return globalThis as ActiveLoggerGlobal;
}

export function setActiveLogger(logger: Logger): void {
  loggerGlobal()[ACTIVE_LOGGER_SYMBOL] = logger;
}

export function getActiveLogger(): Logger | undefined {
  return loggerGlobal()[ACTIVE_LOGGER_SYMBOL];
}

export function getLogFilePath(): string | undefined {
  try {
    return getActiveLogger()?.getLogFilePath?.();
  } catch (err) {
    console.error(
      `[aft-bridge] ERROR: active logger getLogFilePath threw: ${err instanceof Error ? err.message : String(err)}`,
    );
    return undefined;
  }
}

export function log(message: string, meta?: LogMeta): void {
  const active = getActiveLogger();
  if (active) {
    try {
      active.log(message, meta);
    } catch (err) {
      console.error(
        `[aft-bridge] ERROR: active logger log threw: ${err instanceof Error ? err.message : String(err)}`,
      );
      console.error(`[aft-bridge] ${message}`);
    }
  } else {
    console.error(`[aft-bridge] ${message}`);
  }
}

export function warn(message: string, meta?: LogMeta): void {
  const active = getActiveLogger();
  if (active) {
    try {
      active.warn(message, meta);
    } catch (err) {
      console.error(
        `[aft-bridge] ERROR: active logger warn threw: ${err instanceof Error ? err.message : String(err)}`,
      );
      console.error(`[aft-bridge] WARN: ${message}`);
    }
  } else {
    console.error(`[aft-bridge] WARN: ${message}`);
  }
}

export function error(message: string, meta?: LogMeta): void {
  const active = getActiveLogger();
  if (active) {
    try {
      active.error(message, meta);
    } catch (err) {
      console.error(
        `[aft-bridge] ERROR: active logger error threw: ${err instanceof Error ? err.message : String(err)}`,
      );
      console.error(`[aft-bridge] ERROR: ${message}`);
    }
  } else {
    console.error(`[aft-bridge] ERROR: ${message}`);
  }
}

export function sessionLog(sessionId: string | undefined, message: string): void {
  log(message, sessionId ? { sessionId } : undefined);
}

export function sessionWarn(sessionId: string | undefined, message: string): void {
  warn(message, sessionId ? { sessionId } : undefined);
}

export function sessionError(sessionId: string | undefined, message: string): void {
  error(message, sessionId ? { sessionId } : undefined);
}
