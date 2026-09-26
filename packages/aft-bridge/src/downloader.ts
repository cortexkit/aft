/**
 * Auto-download the AFT binary from GitHub releases.
 *
 * Resolution order (in resolver.ts):
 *   1. Cached binary in ~/.cache/aft/bin/
 *   2. npm platform package (@cortexkit/aft-darwin-arm64, etc.)
 *   3. PATH lookup (which aft)
 *   4. ~/.cargo/bin/aft
 *   5. Auto-download from GitHub releases (this module)
 *
 * Cache dir respects XDG_CACHE_HOME on Linux/macOS and LOCALAPPDATA on Windows.
 */

import { createHash, randomUUID } from "node:crypto";
import {
  chmodSync,
  closeSync,
  copyFileSync,
  createWriteStream,
  existsSync,
  mkdirSync,
  openSync,
  readdirSync,
  readFileSync,
  renameSync,
  rmSync,
  statSync,
  unlinkSync,
  writeSync,
} from "node:fs";
import { hostname } from "node:os";
import { join } from "node:path";
import { Readable } from "node:stream";
import { pipeline } from "node:stream/promises";
import { error, log, warn } from "./active-logger.js";
import {
  isTrustedCachedBinary,
  recordBinaryIdentity,
  removeBinaryIdentitySidecar,
  writeBinaryIdentitySidecar,
} from "./binary-identity.js";
import { getAftBinaryCacheDir } from "./cache-paths.js";
import { spawnSync } from "./child-process.js";
import { PLATFORM_ARCH_MAP, PLATFORM_ASSET_MAP } from "./platform.js";
import { parseAftVersionOutput, readBinaryVersionOffThread } from "./version-probe.js";

const REPO = "cortexkit/aft";
const DOWNLOAD_TIMEOUT_MS = 300_000;
const LATEST_TAG_TIMEOUT_MS = 30_000;
const MAX_DOWNLOAD_BYTES = 200 * 1024 * 1024;
const DOWNLOAD_LOCK_STALE_MS = 10 * 60_000;
// A waiter must remain active past the stale threshold so a lock held by a
// crashed or unreachable writer can be reclaimed without a second launch.
const DOWNLOAD_LOCK_TIMEOUT_MS = DOWNLOAD_LOCK_STALE_MS + 30_000;

/**
 * Read the version string from an `aft` binary by invoking it with
 * `--version`. Returns the bare version (e.g. `"0.22.1"`) without the
 * leading `v` or the `aft` prefix, or `null` if the invocation fails.
 *
 * BLOCKING: the calling thread waits in `posix_spawn` until the child is
 * loaded, which for a cold binary under memory pressure has taken close to a
 * minute. Only command-line tools may call this. Plugin hosts must use the
 * identity sidecar (`isTrustedCachedBinary`) or `readBinaryVersionOffThread`.
 */
export function readBinaryVersion(binaryPath: string): string | null {
  try {
    const result = spawnSync(binaryPath, ["--version"], {
      encoding: "utf-8",
      stdio: ["pipe", "pipe", "pipe"],
      // macOS assesses an executable once per inode, and a just-downloaded
      // binary is always a new one. That first exec has been measured at
      // seconds — far worse under memory pressure — so a five-second cap
      // would report a perfectly good binary as unreadable and trigger a
      // needless re-download.
      timeout: 60_000,
    });
    return parseAftVersionOutput(result.stdout ?? "", result.stderr ?? "");
  } catch {
    return null;
  }
}

function expectedVersionFromTag(tag: string): string {
  return tag.startsWith("v") ? tag.slice(1) : tag;
}

/**
 * Decide whether the cached binary at `binaryPath` is the release `tag`
 * without executing it on the calling thread. A matching identity sidecar is
 * enough. An entry without one (written before sidecars existed) or whose
 * sidecar no longer matches the file is asked for its version on a worker
 * thread; when that confirms the version, a fresh sidecar is recorded in the
 * background so later lookups are stat-only.
 */
async function isExpectedCachedBinary(binaryPath: string, tag: string): Promise<boolean> {
  const expected = expectedVersionFromTag(tag);
  if (isTrustedCachedBinary(binaryPath, expected)) return true;
  const actual = await readBinaryVersionOffThread(binaryPath);
  if (actual === expected) {
    void recordBinaryIdentity(binaryPath, expected);
    return true;
  }
  warn(
    `Cached binary at ${binaryPath} reports ${actual ?? "no version"}, expected ${expected}; refreshing cache entry`,
  );
  return false;
}

/** Keep getCacheDir as an alias for getAftBinaryCacheDir so existing callers using the older name continue to work. */
export { getAftBinaryCacheDir as getCacheDir } from "./cache-paths.js";

/** Binary name for the current platform. */
export function getBinaryName(): string {
  return process.platform === "win32" ? "aft.exe" : "aft";
}

/** Return the cached binary path if it exists, otherwise null.
 *  Checks the version-specific cache directory only.
 *  The legacy flat cache (~/.cache/aft/bin/aft) is intentionally NOT checked
 *  because it can be overwritten by other instances, corrupting running processes. */
export function getCachedBinaryPath(version?: string): string | null {
  if (!version) return null;
  const binaryPath = join(getAftBinaryCacheDir(), version, getBinaryName());
  return existsSync(binaryPath) ? binaryPath : null;
}

/**
 * Download the AFT binary for the current platform from GitHub releases.
 *
 * @param version - Git tag to download from. Accepts either `"v0.1.0"` or
 *   `"0.1.0"`; the `v` prefix is normalized internally. If omitted, fetches
 *   the latest release tag via the GitHub API.
 * @returns Absolute path to the downloaded binary, or null on failure.
 */
export async function downloadBinary(version?: string): Promise<string | null> {
  // Resolve via the shared platform table rather than concatenating
  // process.platform + process.arch directly. This matters for Windows
  // ARM64, which the table maps to win32-x64 (Prism emulation) — a naive
  // concat would produce "win32-arm64" and miss the x64 asset that
  // actually runs on those machines.
  const archMap = PLATFORM_ARCH_MAP[process.platform] ?? {};
  const platformKey = archMap[process.arch];
  const assetName = platformKey ? PLATFORM_ASSET_MAP[platformKey] : undefined;

  if (!platformKey || !assetName) {
    error(`Unsupported platform: ${process.platform}-${process.arch}`);
    return null;
  }

  // Resolve version if not provided
  const rawTag = version ?? (await fetchLatestTag());
  if (!rawTag) {
    error("Could not determine latest release version.");
    return null;
  }
  // Normalize tag to always have the `v` prefix. GitHub release URLs and the
  // versioned cache directory both expect `v`-prefixed tags. Without this,
  // callers passing the bare version (e.g. `"0.25.1"`) construct broken
  // URLs (404) and split the cache layout.
  const tag = rawTag.startsWith("v") ? rawTag : `v${rawTag}`;

  // Version-specific cache: ~/.cache/aft/bin/<tag>/aft
  const versionedCacheDir = join(getAftBinaryCacheDir(), tag);
  const binaryName = getBinaryName();
  const binaryPath = join(versionedCacheDir, binaryName);

  // Already cached for this version. Probe the binary itself before trusting
  // the cache directory name; stale hot-swap entries can otherwise shadow a
  // freshly requested compatible version forever.
  if (existsSync(binaryPath) && (await isExpectedCachedBinary(binaryPath, tag))) {
    return binaryPath;
  }

  const downloadUrl = `https://github.com/${REPO}/releases/download/${tag}/${assetName}`;
  const checksumUrl = `https://github.com/${REPO}/releases/download/${tag}/checksums.sha256`;

  log(`Downloading AFT binary (${tag}) for ${platformKey}...`);

  const lockPath = join(versionedCacheDir, ".download.lock");
  let releaseLock: (() => void) | null = null;
  let binaryController: AbortController | null = null;
  let checksumController: AbortController | null = null;
  let binaryTimeout: ReturnType<typeof setTimeout> | null = null;
  let checksumTimeout: ReturnType<typeof setTimeout> | null = null;
  const tmpPath = `${binaryPath}.${process.pid}.${Date.now()}.${Math.random().toString(16).slice(2)}.tmp`;
  const cleanUpPartialDownload = () => {
    try {
      if (existsSync(tmpPath)) unlinkSync(tmpPath);
    } catch {
      // Signal and exit handlers cannot wait for an in-flight stream to close.
      // A later lock holder sweeps any temp file this synchronous cleanup misses.
    }
  };
  let interrupted = false;
  const cleanUpInterruptedDownload = () => {
    if (interrupted) return;
    interrupted = true;
    binaryController?.abort();
    checksumController?.abort();
    cleanUpPartialDownload();
    releaseLock?.();
    releaseLock = null;
  };
  const handleSigint = () => {
    cleanUpInterruptedDownload();
    process.off("SIGINT", handleSigint);
    // Installing a signal listener suppresses Node's default SIGINT exit. Send
    // the signal again after synchronous cleanup to preserve that default.
    process.kill(process.pid, "SIGINT");
  };
  const handleExit = () => cleanUpInterruptedDownload();

  // SIGINT bypasses async catch/finally cleanup. Keep these handlers scoped to
  // the active download so an interrupted first launch releases its lock.
  process.once("SIGINT", handleSigint);
  process.once("exit", handleExit);

  try {
    // Ensure versioned cache directory exists before taking the per-version lock.
    if (!existsSync(versionedCacheDir)) {
      mkdirSync(versionedCacheDir, { recursive: true });
    }

    releaseLock = await acquireDownloadLock(lockPath);
    sweepStaleDownloadTemps(versionedCacheDir, binaryName);

    // Another process may have completed the same version while we waited.
    // Re-probe here too because a stale owner might have left a mismatched
    // binary in the versioned directory before this process acquired the lock.
    if (existsSync(binaryPath) && (await isExpectedCachedBinary(binaryPath, tag))) {
      return binaryPath;
    }

    // Download binary and checksum file in parallel
    binaryController = new AbortController();
    checksumController = new AbortController();
    const activeBinaryController = binaryController;
    const activeChecksumController = checksumController;
    binaryTimeout = setTimeout(() => activeBinaryController.abort(), DOWNLOAD_TIMEOUT_MS);
    checksumTimeout = setTimeout(() => activeChecksumController.abort(), DOWNLOAD_TIMEOUT_MS);
    const [binaryResponse, checksumResponse] = await Promise.all([
      fetch(downloadUrl, { redirect: "follow", signal: activeBinaryController.signal }),
      fetch(checksumUrl, { redirect: "follow", signal: activeChecksumController.signal }),
    ]);

    if (!binaryResponse.ok) {
      throw new Error(
        `HTTP ${binaryResponse.status}: ${binaryResponse.statusText} (${downloadUrl})`,
      );
    }
    if (!binaryResponse.body) {
      throw new Error(`Download response for ${assetName} had no body`);
    }

    const advertised = Number.parseInt(binaryResponse.headers.get("content-length") ?? "", 10);
    if (Number.isFinite(advertised) && advertised > MAX_DOWNLOAD_BYTES) {
      throw new Error(`Content-Length ${advertised} exceeds max ${MAX_DOWNLOAD_BYTES}`);
    }

    // Verify checksum - MANDATORY for security
    if (!checksumResponse.ok) {
      warn(
        `Checksum verification failed: no checksums.sha256 found for ${tag}. ` +
          "Binary download aborted for security reasons.",
      );
      return null;
    }

    const checksumText = await checksumResponse.text();
    clearTimeout(checksumTimeout);
    checksumTimeout = null;
    const expectedHash = parseChecksumForAsset(checksumText, assetName);
    if (!expectedHash) {
      warn(
        `Checksum verification failed: checksums.sha256 found but no entry for ${assetName}. ` +
          "Binary download aborted for security reasons.",
      );
      return null;
    }

    const hash = createHash("sha256");
    let bytesWritten = 0;
    const guard = new TransformStream<Uint8Array, Uint8Array>({
      transform(chunk, controller) {
        bytesWritten += chunk.byteLength;
        if (bytesWritten > MAX_DOWNLOAD_BYTES) {
          controller.error(
            new Error(
              `download exceeded ${MAX_DOWNLOAD_BYTES} bytes after streaming (server lied about size or sent unbounded body)`,
            ),
          );
          return;
        }
        hash.update(chunk);
        controller.enqueue(chunk);
      },
    });

    const guarded = binaryResponse.body.pipeThrough(guard);
    // biome-ignore lint/suspicious/noExplicitAny: ReadableStream→Node stream conversion
    const nodeStream = Readable.fromWeb(guarded as any);
    await pipeline(nodeStream, createWriteStream(tmpPath), { signal: binaryController.signal });
    clearTimeout(binaryTimeout);
    binaryTimeout = null;

    const actualHash = hash.digest("hex");
    if (actualHash !== expectedHash) {
      throw new Error(
        `Checksum mismatch for ${assetName}: expected ${expectedHash}, got ${actualHash}. ` +
          "The binary may have been tampered with.",
      );
    }
    log(`Checksum verified (SHA-256: ${actualHash.slice(0, 16)}...)`);

    // Drop the old entry's sidecar first so no reader pairs it with the new
    // bytes; the stat check would reject that pairing anyway.
    removeBinaryIdentitySidecar(binaryPath);
    // Atomic rename (POSIX) or copy (Windows — renameSync fails with EEXIST
    // when target exists). On Windows, copyFileSync overwrites the target;
    // if it fails the original binary at binaryPath is preserved.
    if (process.platform === "win32") {
      copyFileSync(tmpPath, binaryPath);
    } else {
      chmodSync(tmpPath, 0o755);
      renameSync(tmpPath, binaryPath);
    }
    // The bytes matched the release checksum for `tag`, so the version is
    // known without running the binary. Recorded only once the binary is in
    // its final place, so the sidecar describes that exact file.
    try {
      writeBinaryIdentitySidecar(binaryPath, tag, actualHash);
    } catch (err) {
      warn(
        `Could not record identity for ${binaryPath}: ${err instanceof Error ? err.message : String(err)}`,
      );
    }

    // Binary was replaced successfully. Clean up the temp file best-effort;
    // a cleanup failure should NOT propagate as a download failure.
    try {
      if (existsSync(tmpPath)) unlinkSync(tmpPath);
    } catch {
      warn(`Could not clean up temporary download file ${tmpPath} — it can be removed manually.`);
    }

    log(`AFT binary ready at ${binaryPath}`);
    return binaryPath;
  } catch (err) {
    const msg = err instanceof Error ? err.message : String(err);
    error(`Failed to download AFT binary: ${msg}`);

    cleanUpPartialDownload();
    return null;
  } finally {
    process.off("SIGINT", handleSigint);
    process.off("exit", handleExit);
    if (binaryTimeout) {
      binaryController?.abort();
      clearTimeout(binaryTimeout);
    }
    if (checksumTimeout) {
      checksumController?.abort();
      clearTimeout(checksumTimeout);
    }
    releaseLock?.();
  }
}

/** In-process single-flight for plugin startup and first-tool-call resolution. */
const ensureBinaryInFlight = new Map<string, Promise<string | null>>();

function ensureBinaryKey(version?: string): string {
  if (!version) return "latest";
  return version.startsWith("v") ? version : `v${version}`;
}

/**
 * Ensure the AFT binary is available: check cache, then download if needed.
 * This is the main entry point called by the resolver and by plugin startup
 * warmups. Sharing the promise keeps those two paths on one download while the
 * filesystem lock still coordinates separate host processes.
 *
 * @param version - Git tag (e.g. `"v0.25.1"` or `"0.25.1"` — both accepted).
 *   Normalized to a `v`-prefixed tag internally so the on-disk cache layout
 *   stays consistent regardless of caller convention.
 */
export async function ensureBinary(version?: string): Promise<string | null> {
  const key = ensureBinaryKey(version);
  const existing = ensureBinaryInFlight.get(key);
  if (existing) return existing;

  const task = (async () => {
    if (version) {
      // Normalize tag for cache lookup so a caller passing the bare version
      // (e.g. "0.25.1") finds the same cache entry that `downloadBinary`
      // and `findBinarySync` write to (`~/.cache/aft/bin/v0.25.1/aft`).
      const tag = ensureBinaryKey(version);

      // When a specific version is requested, ONLY check the versioned cache.
      // Do NOT fall back to legacy flat cache — it may contain a different version,
      // causing an infinite spawn-check-replace loop.
      const versionCached = getCachedBinaryPath(tag);
      if (versionCached && (await isExpectedCachedBinary(versionCached, tag))) {
        log(`Found cached binary for ${tag}: ${versionCached}`);
        return versionCached;
      }
      log(`No cached binary for ${tag}, downloading...`);
      return downloadBinary(tag);
    }
    // No version requested — download latest.
    log("No cached binary found, downloading latest...");
    return downloadBinary();
  })();
  ensureBinaryInFlight.set(key, task);
  try {
    return await task;
  } finally {
    if (ensureBinaryInFlight.get(key) === task) ensureBinaryInFlight.delete(key);
  }
}

type DownloadLockTiming = {
  timeoutMs?: number;
  staleMs?: number;
  pollIntervalMs?: number;
};

type DownloadLockOwner = {
  pid: number | null;
  hostname: string | null;
};

function createDownloadLockOwner(): string {
  return JSON.stringify({
    pid: process.pid,
    hostname: hostname(),
    createdAt: Date.now(),
    token: randomUUID(),
  });
}

function parseDownloadLockOwner(raw: string): DownloadLockOwner {
  try {
    const parsed: unknown = JSON.parse(raw);
    if (typeof parsed === "object" && parsed !== null) {
      const { pid, hostname: ownerHostname } = parsed as Record<string, unknown>;
      return {
        pid: typeof pid === "number" && Number.isSafeInteger(pid) && pid > 0 ? pid : null,
        hostname: typeof ownerHostname === "string" && ownerHostname ? ownerHostname : null,
      };
    }
  } catch {
    // Older lock files used colon-delimited owner strings without a hostname.
  }

  const legacyPid = Number(raw.split(":", 1)[0]);
  return {
    pid: Number.isSafeInteger(legacyPid) && legacyPid > 0 ? legacyPid : null,
    hostname: null,
  };
}

function isProcessAlive(pid: number): boolean {
  try {
    process.kill(pid, 0);
    return true;
  } catch (err) {
    // ESRCH means no process has this PID. Permission errors still mean the
    // process exists, and unknown failures are safer to treat as live.
    return (err as NodeJS.ErrnoException).code !== "ESRCH";
  }
}

function isReclaimableDownloadLock(
  owner: DownloadLockOwner,
  ageMs: number,
  staleMs: number,
): boolean {
  if (Math.abs(ageMs) > staleMs) return true;
  // Legacy locks omitted a hostname and were written to the local cache. A
  // recorded foreign hostname is the only case where probing its PID is unsafe.
  return (
    (owner.hostname === null || owner.hostname === hostname()) &&
    owner.pid !== null &&
    !isProcessAlive(owner.pid)
  );
}

function reclaimDownloadLock(lockPath: string, expectedOwner: string): boolean {
  try {
    // Re-check ownership immediately before removing the file so a waiter does
    // not delete a replacement lock that another process acquired meanwhile.
    if (readFileSync(lockPath, "utf-8") !== expectedOwner) return false;
    rmSync(lockPath, { force: true });
    return true;
  } catch (err) {
    if ((err as NodeJS.ErrnoException).code === "ENOENT") return true;
    throw err;
  }
}

function sweepStaleDownloadTemps(versionedCacheDir: string, binaryName: string): void {
  const tempPrefix = `${binaryName}.`;
  try {
    for (const entry of readdirSync(versionedCacheDir, { withFileTypes: true })) {
      if (!entry.isFile() || !entry.name.startsWith(tempPrefix) || !entry.name.endsWith(".tmp")) {
        continue;
      }
      const tempPath = join(versionedCacheDir, entry.name);
      const ageMs = Date.now() - statSync(tempPath).mtimeMs;
      // Treat large future ages as stale too; a backward clock adjustment
      // should not make an interrupted artifact permanent.
      if (Math.abs(ageMs) > DOWNLOAD_LOCK_STALE_MS) unlinkSync(tempPath);
    }
  } catch {
    // Temp cleanup is best-effort. A failed sweep must not block a download.
  }
}

async function acquireDownloadLock(
  lockPath: string,
  timing: DownloadLockTiming = {},
): Promise<() => void> {
  const timeoutMs = timing.timeoutMs ?? DOWNLOAD_LOCK_TIMEOUT_MS;
  const staleMs = timing.staleMs ?? DOWNLOAD_LOCK_STALE_MS;
  const pollIntervalMs = timing.pollIntervalMs ?? 100;
  const startedAt = Date.now();
  while (true) {
    try {
      const owner = createDownloadLockOwner();
      const fd = openSync(lockPath, "wx");
      try {
        writeSync(fd, owner);
      } finally {
        closeSync(fd);
      }
      return () => {
        try {
          if (readFileSync(lockPath, "utf-8") === owner) {
            rmSync(lockPath, { force: true });
          }
        } catch {
          // Best-effort lock cleanup; missing or reclaimed locks are fine.
        }
      };
    } catch (err) {
      const code = (err as NodeJS.ErrnoException).code;
      if (code !== "EEXIST") throw err;

      let existingOwner: string;
      let ageMs: number;
      try {
        existingOwner = readFileSync(lockPath, "utf-8");
        ageMs = Date.now() - statSync(lockPath).mtimeMs;
      } catch (readErr) {
        if ((readErr as NodeJS.ErrnoException).code === "ENOENT") continue;
        throw readErr;
      }

      if (isReclaimableDownloadLock(parseDownloadLockOwner(existingOwner), ageMs, staleMs)) {
        if (reclaimDownloadLock(lockPath, existingOwner)) continue;
      }

      if (Date.now() - startedAt > timeoutMs) {
        throw new Error(`Timed out waiting for download lock: ${lockPath}`);
      }
      await new Promise((resolve) => setTimeout(resolve, pollIntervalMs));
    }
  }
}

/**
 * Parse a checksums.sha256 file (GNU coreutils format) and return the hash
 * for the given asset name, or null if not found.
 *
 * Expected format: `<hex-hash>  <filename>` (two spaces between hash and name)
 */
function parseChecksumForAsset(checksumText: string, assetName: string): string | null {
  for (const line of checksumText.split("\n")) {
    const trimmed = line.trim();
    if (!trimmed) continue;
    // Format: "abc123...  aft-darwin-arm64"
    const match = trimmed.match(/^([0-9a-f]{64})\s+(.+)$/);
    if (match && match[2] === assetName) {
      return match[1];
    }
  }
  return null;
}

/** Fetch the latest release tag from GitHub API. */
async function fetchLatestTag(): Promise<string | null> {
  const controller = new AbortController();
  const timeout = setTimeout(() => controller.abort(), LATEST_TAG_TIMEOUT_MS);
  try {
    const response = await fetch(`https://api.github.com/repos/${REPO}/releases/latest`, {
      headers: { Accept: "application/vnd.github.v3+json" },
      signal: controller.signal,
    });
    if (!response.ok) return null;
    const data = (await response.json()) as { tag_name?: string };
    return data.tag_name ?? null;
  } catch {
    return null;
  } finally {
    clearTimeout(timeout);
  }
}

export const __test__ = {
  acquireDownloadLock,
};
