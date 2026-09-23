import { appendFile, chmod, mkdir, rename, rm, stat } from "node:fs/promises";
import { dirname } from "node:path";

/** Maximum size of the active plugin log before its single backup rotates in. */
export const DEFAULT_LOG_BYTES = 32 * 1024 * 1024;
/** Retention hygiene keeps one backup generation and no numbered chain. */
export const DEFAULT_LOG_GENERATIONS = 1;
/**
 * Log lines can quote paths, commands and server output, so the log directory
 * is owner-only and log files are owner read/write, matching the Rust daemon's
 * logs in the same directory. POSIX only: Windows ignores these modes.
 */
export const LOG_DIR_MODE = 0o700;
export const LOG_FILE_MODE = 0o600;

import { resolveAftLogPath, resolveAftStorageRoot } from "./storage-paths.js";

export { resolveAftLogPath, resolveAftStorageRoot };

export interface RotatingLogOptions {
  maxBytes?: number;
  generations?: number;
}

/**
 * Asynchronous append-only sink with bounded size rotation.
 *
 * Callers only enqueue strings; directory creation, stat, append, and rename
 * operations run on a serialized promise chain outside the logging hot path.
 */
export class RotatingLogSink {
  readonly path: string;
  private readonly maxBytes: number;
  private readonly generations: number;
  private estimatedBytes: number | null = null;
  private queue: Promise<void> = Promise.resolve();
  private disabled = false;
  private failureReported = false;

  constructor(path: string, options: RotatingLogOptions = {}) {
    this.path = path;
    this.maxBytes = options.maxBytes ?? DEFAULT_LOG_BYTES;
    this.generations = options.generations ?? DEFAULT_LOG_GENERATIONS;
  }

  append(data: string): void {
    if (this.disabled || data.length === 0) return;
    this.queue = this.queue
      .then(() => this.write(data))
      .catch((error: unknown) => {
        this.disabled = true;
        if (!this.failureReported) {
          this.failureReported = true;
          try {
            process.stderr.write(
              `[aft-plugin] durable log disabled for ${this.path}: ${error instanceof Error ? error.message : String(error)}\n`,
            );
          } catch {
            // Logging failures must never escape into the host process.
          }
        }
      });
  }

  /** Wait for queued writes. Intended for shutdown hooks and tests. */
  async drain(): Promise<void> {
    await this.queue;
  }

  private async write(data: string): Promise<void> {
    const dir = dirname(this.path);
    await mkdir(dirname(dir), { recursive: true });
    try {
      await mkdir(dir, { mode: LOG_DIR_MODE });
    } catch (error: unknown) {
      if (!hasCode(error, "EEXIST")) throw error;
    }
    if (this.estimatedBytes === null) {
      // First write from this sink: tighten a directory and files left over
      // from before logs were private. Later files are created owner-only.
      await tightenIfOwned(dir, LOG_DIR_MODE);
      await tightenIfOwned(this.path, LOG_FILE_MODE);
      for (let generation = 1; generation <= this.generations; generation += 1) {
        await tightenIfOwned(`${this.path}.${generation}`, LOG_FILE_MODE);
      }
      try {
        this.estimatedBytes = (await stat(this.path)).size;
      } catch (error: unknown) {
        if (!isMissing(error)) throw error;
        this.estimatedBytes = 0;
      }
    }

    const bytes = Buffer.byteLength(data);
    if (this.estimatedBytes > 0 && this.estimatedBytes + bytes > this.maxBytes) {
      await this.rotate();
    }
    await appendFile(this.path, data, { encoding: "utf8", mode: LOG_FILE_MODE });
    this.estimatedBytes = (this.estimatedBytes ?? 0) + bytes;
  }

  private async rotate(): Promise<void> {
    if (this.generations <= 0) {
      await removeIfPresent(this.path);
      this.estimatedBytes = 0;
      return;
    }
    await removeIfPresent(`${this.path}.${this.generations}`);
    for (let generation = this.generations - 1; generation >= 1; generation -= 1) {
      await renameIfPresent(`${this.path}.${generation}`, `${this.path}.${generation + 1}`);
    }
    await renameIfPresent(this.path, `${this.path}.1`);
    this.estimatedBytes = 0;
  }
}

function isMissing(error: unknown): boolean {
  return hasCode(error, "ENOENT");
}

function hasCode(error: unknown, code: string): boolean {
  return typeof error === "object" && error !== null && "code" in error && error.code === code;
}

/**
 * Drop group/other permission bits when the current user owns `path`. Best
 * effort: a missing path, another owner, or a failed chmod leaves it as is
 * rather than disabling the log.
 */
async function tightenIfOwned(path: string, mode: number): Promise<void> {
  if (process.platform === "win32" || typeof process.getuid !== "function") return;
  try {
    const info = await stat(path);
    if (info.uid !== process.getuid() || (info.mode & 0o777 & ~mode) === 0) return;
    await chmod(path, mode);
  } catch {
    // Best effort; see above.
  }
}

async function removeIfPresent(path: string): Promise<void> {
  try {
    await rm(path, { force: true });
  } catch (error: unknown) {
    if (!isMissing(error)) throw error;
  }
}

async function renameIfPresent(from: string, to: string): Promise<void> {
  try {
    await rename(from, to);
  } catch (error: unknown) {
    if (!isMissing(error)) throw error;
  }
}
