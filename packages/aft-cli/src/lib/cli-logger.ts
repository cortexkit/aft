import { appendFileSync, mkdirSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import {
  isTestEnvironment,
  type Logger,
  resolveAftLogPath,
  setActiveLogger,
} from "@cortexkit/aft-bridge";

/**
 * The CLI's own log for messages from the shared bridge package.
 *
 * The bridge logs through a host-provided logger and, when none is set, prints
 * `[aft-bridge] …` lines straight to stderr. Inside the setup and doctor
 * screens those raw lines break the prompt layout, so the CLI installs this
 * logger: every bridge line goes to `aft-cli.log` under the AFT log directory,
 * and is echoed to stderr only with `--verbose`.
 *
 * Code that needs to know WHY a bridge operation failed (the binary download
 * reports its cause only through the log) can listen for lines while the
 * operation runs with {@link captureBridgeLog}.
 */

export type BridgeLogLevel = "info" | "warn" | "error";
type Listener = (level: BridgeLogLevel, message: string) => void;

const listeners = new Set<Listener>();
let installed = false;
let verbose = false;

/** Path of the CLI log. Tests write to the temp directory, never the live log directory. */
export function cliLogPath(): string {
  if (isTestEnvironment()) return join(tmpdir(), "aft-cli-test.log");
  return resolveAftLogPath("aft-cli.log");
}

function write(level: BridgeLogLevel, message: string): void {
  for (const listener of listeners) {
    try {
      listener(level, message);
    } catch {
      // A failing listener must not stop the line reaching the log.
    }
  }
  if (verbose) {
    process.stderr.write(
      `[aft-bridge] ${level === "info" ? "" : `${level.toUpperCase()}: `}${message}\n`,
    );
  }
  try {
    const path = cliLogPath();
    mkdirSync(dirname(path), { recursive: true });
    appendFileSync(path, `${new Date().toISOString()} ${level.toUpperCase()} ${message}\n`);
  } catch {
    // The log is best-effort. An unwritable log directory (for example one a
    // `sudo` install left owned by root) must never break setup or doctor.
  }
}

export const cliLogger: Logger = {
  log: (message) => write("info", message),
  warn: (message) => write("warn", message),
  error: (message) => write("error", message),
  getLogFilePath: () => cliLogPath(),
};

/**
 * Route bridge log lines to the CLI log. Idempotent; `verbose` only ever turns
 * echoing on, so a later call without options keeps an earlier `--verbose`.
 */
export function installCliLogger(options: { verbose?: boolean } = {}): void {
  if (options.verbose) verbose = true;
  if (installed) return;
  installed = true;
  setActiveLogger(cliLogger);
}

/** Run `fn` and return every bridge log line it produced alongside its result. */
export async function captureBridgeLog<T>(
  fn: () => Promise<T>,
): Promise<{ result: T; lines: { level: BridgeLogLevel; message: string }[] }> {
  installCliLogger();
  const lines: { level: BridgeLogLevel; message: string }[] = [];
  const listener: Listener = (level, message) => lines.push({ level, message });
  listeners.add(listener);
  try {
    return { result: await fn(), lines };
  } finally {
    listeners.delete(listener);
  }
}
