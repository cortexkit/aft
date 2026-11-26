import { randomBytes } from "node:crypto";
import { type BigIntStats, constants } from "node:fs";
import { appendFile, link, lstat, mkdir, open, readFile, rename, rm, stat } from "node:fs/promises";
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
/** How long a sink drops lines after a failed write before it tries again. */
export const DEFAULT_LOG_RETRY_DELAY_MS = 30_000;
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
  /** Pause after a failed write before the next attempt (default 30 s). */
  retryDelayMs?: number;
}

/**
 * Asynchronous append-only sink with bounded size rotation.
 *
 * Callers only enqueue strings; directory creation, stat, append, and rename
 * operations run on a serialized promise chain outside the logging hot path.
 *
 * Several processes (every OpenCode and Pi host) append to the same file, so
 * the rotation decision reads the file's real size before each write rather
 * than trusting a count of this sink's own appends.
 */
export class RotatingLogSink {
  readonly path: string;
  private readonly maxBytes: number;
  private readonly generations: number;
  private readonly retryDelayMs: number;
  /** The first write's one-time permission repair has run. */
  private initialized = false;
  private queue: Promise<void> = Promise.resolve();
  /**
   * After a failed write the sink drops lines until this time, then tries
   * again, so a passing problem (a full disk, a directory being replaced) does
   * not turn the log off for the rest of the process.
   */
  private pausedUntil = 0;
  /** Bytes dropped since the last successful write because writes failed. */
  private failedBytes = 0;
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
    this.retryDelayMs = options.retryDelayMs ?? DEFAULT_LOG_RETRY_DELAY_MS;
  }

  append(data: string): void {
    if (data.length === 0) return;
    const bytes = Buffer.byteLength(data);
    if (this.isPaused()) {
      this.failedBytes += bytes;
      return;
    }
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
          await this.writeAfterFailures(
            `[aft-plugin] durable log queue overflow: dropped ${dropped} bytes (1MiB/4096-record pending limit); reduce diagnostic volume.\n`,
          );
        } else {
          try {
            // Lines queued before a failure was seen are dropped like new ones.
            if (this.isPaused()) this.failedBytes += bytes;
            else await this.writeAfterFailures(data);
          } finally {
            this.queuedBytes -= bytes;
            this.queuedRecords -= 1;
          }
        }
      })
      .catch((error: unknown) => {
        this.failedBytes += bytes;
        this.pausedUntil = Date.now() + this.retryDelayMs;
        // Re-check the directory on the next attempt; it may have been replaced.
        this.directoryReady = false;
        if (!this.failureReported) {
          this.failureReported = true;
          try {
            process.stderr.write(
              `[aft-plugin] durable log paused for ${this.path}: ${error instanceof Error ? error.message : String(error)}; retrying in ${Math.round(this.retryDelayMs / 1000)}s\n`,
            );
          } catch {
            // Logging failures must never escape into the host process.
          }
        }
      });
  }

  private isPaused(): boolean {
    return this.pausedUntil > 0 && Date.now() < this.pausedUntil;
  }

  /** Write `data`, first marking any span lost to earlier failed writes. */
  private async writeAfterFailures(data: string): Promise<void> {
    const dropped = this.failedBytes;
    if (dropped === 0) {
      await this.write(data);
      return;
    }
    await this.write(
      `[aft-plugin] durable log resumed after a write failure; dropped ${dropped} bytes.\n${data}`,
    );
    this.failedBytes -= dropped;
    this.pausedUntil = 0;
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
    if (!this.directoryReady || !this.initialized) {
      const dir = await this.ensureDirectory();
      if (!this.initialized) {
        // First write from this sink: tighten a directory and files left over
        // from before logs were private. Later files are created owner-only.
        await tightenIfOwned(dir, LOG_DIR_MODE);
        await tightenIfOwned(this.path, LOG_FILE_MODE);
        for (let generation = 1; generation <= this.generations; generation += 1) {
          await tightenIfOwned(`${this.path}.${generation}`, LOG_FILE_MODE);
        }
        this.initialized = true;
      }
    }

    await this.rotateIfFull(Buffer.byteLength(data));
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
  }

  /** Rotate when appending `bytes` would take the file, as it is now, past the cap. */
  private async rotateIfFull(bytes: number): Promise<void> {
    let current: BigIntStats;
    try {
      current = await stat(this.path, { bigint: true });
    } catch (error: unknown) {
      if (isMissing(error)) return;
      throw error;
    }
    const size = Number(current.size);
    if (size === 0 || size + bytes <= this.maxBytes) return;
    await this.rotate(current);
  }

  /**
   * Move the full file `measured` out of the way. Two processes can both find
   * the file over the cap. Each first renames the live file to a name only it
   * uses: just one of them can take the full file, and the other gets either
   * nothing or the fresh file the first one has started. That fresh file goes
   * back into the live log instead of replacing the backup just made.
   */
  private async rotate(measured: BigIntStats): Promise<void> {
    const claim = `${this.path}.rotating-${process.pid}-${randomBytes(6).toString("hex")}`;
    try {
      await rename(this.path, claim);
    } catch (error: unknown) {
      if (isMissing(error)) return;
      throw error;
    }
    const claimed = await stat(claim, { bigint: true });
    if (claimed.ino !== measured.ino || claimed.dev !== measured.dev) {
      try {
        // Keep the same inode so writes through already-open handles still
        // reach the live log. Linking won't replace a live file recreated by
        // another writer.
        await link(claim, this.path);
      } catch (error: unknown) {
        if (!canCopyClaim(error)) throw error;
        const fresh = await readFile(claim);
        await appendFile(this.path, fresh, { mode: LOG_FILE_MODE });
      }
      await removeIfPresent(claim);
      return;
    }
    if (this.generations <= 0) {
      await removeIfPresent(claim);
      return;
    }
    await removeIfPresent(`${this.path}.${this.generations}`);
    for (let generation = this.generations - 1; generation >= 1; generation -= 1) {
      await renameIfPresent(`${this.path}.${generation}`, `${this.path}.${generation + 1}`);
    }
    await rename(claim, `${this.path}.1`);
  }
}

function isMissing(error: unknown): boolean {
  return hasCode(error, "ENOENT");
}

function hasCode(error: unknown, code: string): boolean {
  return typeof error === "object" && error !== null && "code" in error && error.code === code;
}

function canCopyClaim(error: unknown): boolean {
  // Unsupported links can look like EPERM on Linux or EISDIR on Windows
  // (libuv's mapping of ERROR_INVALID_FUNCTION).
  return (
    ["EEXIST", "EPERM", "ENOTSUP", "EOPNOTSUPP", "ENOSYS"].some((code) => hasCode(error, code)) ||
    (process.platform === "win32" && hasCode(error, "EISDIR"))
  );
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
