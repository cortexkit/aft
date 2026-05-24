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

import { createHash } from "node:crypto";
import {
  chmodSync,
  closeSync,
  createWriteStream,
  existsSync,
  mkdirSync,
  openSync,
  renameSync,
  rmSync,
  statSync,
  unlinkSync,
} from "node:fs";
import { homedir } from "node:os";
import { join } from "node:path";
import { Readable } from "node:stream";
import { pipeline } from "node:stream/promises";
import { error, log, warn } from "./active-logger.js";
import { PLATFORM_ARCH_MAP, PLATFORM_ASSET_MAP } from "./platform.js";

const REPO = "cortexkit/aft";
const DOWNLOAD_TIMEOUT_MS = 300_000;
const LATEST_TAG_TIMEOUT_MS = 30_000;
const MAX_DOWNLOAD_BYTES = 200 * 1024 * 1024;
const DOWNLOAD_LOCK_TIMEOUT_MS = 120_000;
const DOWNLOAD_LOCK_STALE_MS = 10 * 60_000;

/** Get the cache directory, respecting XDG_CACHE_HOME / LOCALAPPDATA. */
export function getCacheDir(): string {
  if (process.platform === "win32") {
    const localAppData = process.env.LOCALAPPDATA || process.env.APPDATA;
    const base = localAppData || join(homedir(), "AppData", "Local");
    return join(base, "aft", "bin");
  }

  const base = process.env.XDG_CACHE_HOME || join(homedir(), ".cache");
  return join(base, "aft", "bin");
}

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
  const binaryPath = join(getCacheDir(), version, getBinaryName());
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
  const versionedCacheDir = join(getCacheDir(), tag);
  const binaryName = getBinaryName();
  const binaryPath = join(versionedCacheDir, binaryName);

  // Already cached for this version
  if (existsSync(binaryPath)) {
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

  try {
    // Ensure versioned cache directory exists before taking the per-version lock.
    if (!existsSync(versionedCacheDir)) {
      mkdirSync(versionedCacheDir, { recursive: true });
    }

    releaseLock = await acquireDownloadLock(lockPath);

    // Another process may have completed the same version while we waited.
    if (existsSync(binaryPath)) {
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

    // Make executable
    if (process.platform !== "win32") {
      chmodSync(tmpPath, 0o755);
    }

    // Atomic rename
    renameSync(tmpPath, binaryPath);

    log(`AFT binary ready at ${binaryPath}`);
    return binaryPath;
  } catch (err) {
    const msg = err instanceof Error ? err.message : String(err);
    error(`Failed to download AFT binary: ${msg}`);

    // Clean up partial download
    if (existsSync(tmpPath)) {
      try {
        unlinkSync(tmpPath);
      } catch {
        // ignore cleanup failure
      }
    }

    return null;
  } finally {
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

/**
 * Ensure the AFT binary is available: check cache, then download if needed.
 * This is the main entry point called by the resolver.
 *
 * @param version - Git tag (e.g. `"v0.25.1"` or `"0.25.1"` — both accepted).
 *   Normalized to a `v`-prefixed tag internally so the on-disk cache layout
 *   stays consistent regardless of caller convention.
 */
export async function ensureBinary(version?: string): Promise<string | null> {
  if (version) {
    // Normalize tag for cache lookup so a caller passing the bare version
    // (e.g. `"0.25.1"`) finds the same cache entry that `downloadBinary`
    // and `findBinarySync` write to (`~/.cache/aft/bin/v0.25.1/aft`).
    const tag = version.startsWith("v") ? version : `v${version}`;

    // When a specific version is requested, ONLY check the versioned cache.
    // Do NOT fall back to legacy flat cache — it may contain a different version,
    // causing an infinite spawn-check-replace loop.
    const versionCached = getCachedBinaryPath(tag);
    if (versionCached) {
      log(`Found cached binary for ${tag}: ${versionCached}`);
      return versionCached;
    }
    log(`No cached binary for ${tag}, downloading...`);
    return downloadBinary(tag);
  }
  // No version requested — download latest.
  log("No cached binary found, downloading latest...");
  return downloadBinary();
}

async function acquireDownloadLock(lockPath: string): Promise<() => void> {
  const startedAt = Date.now();
  while (true) {
    try {
      const fd = openSync(lockPath, "wx");
      return () => {
        try {
          closeSync(fd);
        } catch {
          // already closed — ignore
        }
        try {
          rmSync(lockPath, { force: true });
        } catch {
          // best-effort lock cleanup
        }
      };
    } catch (err) {
      const code = (err as NodeJS.ErrnoException).code;
      if (code !== "EEXIST") throw err;

      try {
        const ageMs = Date.now() - statSync(lockPath).mtimeMs;
        if (ageMs > DOWNLOAD_LOCK_STALE_MS) {
          rmSync(lockPath, { force: true });
          continue;
        }
      } catch {
        continue;
      }

      if (Date.now() - startedAt > DOWNLOAD_LOCK_TIMEOUT_MS) {
        throw new Error(`Timed out waiting for download lock: ${lockPath}`);
      }
      await new Promise((resolve) => setTimeout(resolve, 100));
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
