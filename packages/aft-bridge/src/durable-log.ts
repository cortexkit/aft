import { constants } from "node:fs";
import { appendFile, lstat, mkdir, open, rename, rm, stat } from "node:fs/promises";
import { dirname } from "node:path";
import {
  openPrivateStorageDir,
  PRIVATE_DIRECTORY_MODE,
  PRIVATE_FILE_MODE,
} from "./private-storage.js";

/** Maximum size of the active plugin log before its single backup rotates in. */
export const DEFAULT_LOG_BYTES = 32 * 1024 * 1024;
/** Retention hygiene keeps one backup generation and no numbered chain. */
export const DEFAULT_LOG_GENERATIONS = 1;
/**
 * Log lines can quote paths, commands and server output, so the log directory
 * is owner-only and log files are owner read/write, matching the Rust daemon's
 * logs in the same directory. POSIX only: Windows ignores these modes.
 */
export const LOG_DIR_MODE = PRIVATE_DIRECTORY_MODE;
export const LOG_FILE_MODE = PRIVATE_FILE_MODE;

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
  private queuedBytes = 0;
  private queuedRecords = 0;
  private droppedBytes = 0;
  private overflowQueued = false;
  /**
   * The log directory was created (or found) by an earlier write. Each write
   * then appends directly instead of issuing two mkdir calls first; an append
   * that finds the directory gone recreates it and retries once.
   */
  private directoryReady = false;
  /** mkdir calls issued; tests use it to pin the per-write cost. */
  private mkdirCalls = 0;

  constructor(path: string, options: RotatingLogOptions = {}) {
    this.path = path;
    this.maxBytes = options.maxBytes ?? DEFAULT_LOG_BYTES;
    this.generations = options.generations ?? DEFAULT_LOG_GENERATIONS;
  }

  append(data: string): void {
    if (this.disabled || data.length === 0) return;
    const bytes = Buffer.byteLength(data);
    const overflow = this.queuedBytes + bytes > 1024 * 1024 || this.queuedRecords >= 4096;
    if (overflow) {
      this.droppedBytes += bytes;
      if (this.overflowQueued) return;
      this.overflowQueued = true;
      data = "";
    } else {
      this.queuedBytes += bytes;
      this.queuedRecords += 1;
    }
    this.queue = this.queue
      .then(async () => {
        if (overflow) {
          const dropped = this.droppedBytes;
          this.droppedBytes = 0;
          this.overflowQueued = false;
          await this.write(
            `[aft-plugin] durable log queue overflow: dropped ${dropped} bytes (1MiB/4096-record pending limit); reduce diagnostic volume.\n`,
          );
        } else {
          try {
            await this.write(data);
          } finally {
            this.queuedBytes -= bytes;
            this.queuedRecords -= 1;
          }
        }
      })
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

  /** Test hook: mkdir calls this sink has issued. */
  __mkdirCallsForTests(): number {
    return this.mkdirCalls;
  }

  /** Wait for queued writes. Intended for shutdown hooks and tests. */
  async drain(): Promise<void> {
    await this.queue;
  }

  private async ensureDirectory(): Promise<string> {
    const dir = dirname(this.path);
    this.mkdirCalls += 2;
    await mkdir(dirname(dir), { recursive: true, mode: LOG_DIR_MODE });
    try {
      await mkdir(dir, { mode: LOG_DIR_MODE });
    } catch (error: unknown) {
      if (!hasCode(error, "EEXIST")) throw error;
    }
    openPrivateStorageDir(dirname(dir), dir);
    this.directoryReady = true;
    return dir;
  }

  private async write(data: string): Promise<void> {
    if (!this.directoryReady || this.estimatedBytes === null) {
      const dir = await this.ensureDirectory();
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
    }

    const bytes = Buffer.byteLength(data);
    if ((this.estimatedBytes ?? 0) > 0 && (this.estimatedBytes ?? 0) + bytes > this.maxBytes) {
      await this.rotate();
    }
    try {
      await appendFile(this.path, data, { encoding: "utf8", mode: LOG_FILE_MODE });
    } catch (error: unknown) {
      if (!isMissing(error)) throw error;
      // The directory was removed since the last write: recreate it, as the
      // per-write mkdir used to, and retry once.
      this.directoryReady = false;
      await this.ensureDirectory();
      await appendFile(this.path, data, { encoding: "utf8", mode: LOG_FILE_MODE });
    }
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
  let file: Awaited<ReturnType<typeof open>> | undefined;
  try {
    for (const dir of [dirname(path), dirname(dirname(path))]) {
      if ((await lstat(dir)).isSymbolicLink()) return;
    }
    file = await open(path, constants.O_RDONLY | constants.O_NOFOLLOW);
    const info = await file.stat();
    if (info.uid !== process.getuid() || (info.mode & 0o777 & ~mode) === 0) return;
    await file.chmod(mode);
  } catch {
    // Best effort; see above.
  } finally {
    await file?.close();
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
