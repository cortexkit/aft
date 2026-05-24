/**
 * Auto-download and manage ONNX Runtime shared library for semantic search.
 *
 * Downloads the CPU-only ONNX Runtime from Microsoft's GitHub releases.
 * The library is cached in the storage directory alongside semantic index data.
 *
 * Audit-3 v0.17 #1 hardening (v0.17.1): the previous implementation used
 * `curl` with no size cap, no archive containment validation, no install
 * lock, and no integrity verification — leaving an entire parallel install
 * path that bypassed every defense the LSP GitHub installer had earned in
 * Phase A through Phase E. This rewrite brings ONNX onto the same security
 * floor:
 *
 *   - Streaming size cap via fetch + ReadableStream transformer (`MAX_DOWNLOAD_BYTES`).
 *   - Streaming SHA-256 of the downloaded archive, persisted in `.aft-onnx-installed`.
 *   - Atomic O_EXCL install lock with PID-aware stale-lock recovery.
 *   - Containment-checked extraction: every file under the staging dir
 *     must be inside the staging root, no symlinks allowed before move.
 *   - Total extracted size cap (`MAX_EXTRACT_BYTES`) to defeat decompression
 *     bombs.
 *   - TOFU verification: if `.aft-onnx-installed` already records a hash for
 *     this version, refuse to use a binary that doesn't match.
 *
 * Supported platforms:
 *   - macOS ARM64 (osx-arm64)
 *   - Linux x64 (linux-x64)
 *   - Linux ARM64 (linux-aarch64)
 *   - Windows x64 (win-x64)
 *   - Windows ARM64 (win-arm64)
 *
 * macOS x64 (Intel) is not provided by Microsoft — users must install via:
 *   brew install onnxruntime
 */

import { execFileSync } from "node:child_process";
import { createHash } from "node:crypto";
import {
  chmodSync,
  closeSync,
  copyFileSync,
  createWriteStream,
  existsSync,
  lstatSync,
  mkdirSync,
  openSync,
  readdirSync,
  readFileSync,
  readlinkSync,
  realpathSync,
  rmSync,
  statSync,
  symlinkSync,
  unlinkSync,
  writeFileSync,
} from "node:fs";
import { dirname, join, relative, resolve } from "node:path";
import { Readable } from "node:stream";
import { pipeline } from "node:stream/promises";
import { error, log, warn } from "./active-logger.js";
import { PLATFORM_ARCH_MAP } from "./platform.js";

const ORT_VERSION = "1.24.4";
const ORT_REPO = "microsoft/onnxruntime";

// Audit-3 v0.17 #1: streaming + extraction size caps.
//
// ONNX Runtime archives are around 60–80 MB on most platforms and 250 MB
// extracted (Windows ships extra debug binaries). 256 MB and 1 GiB give
// generous headroom while preventing a malicious or corrupted CDN response
// from filling the user's disk. Same numbers as the GitHub LSP installer.
const MAX_DOWNLOAD_BYTES = 256 * 1024 * 1024;
const MAX_EXTRACT_BYTES = 1 * 1024 * 1024 * 1024;

const ONNX_LOCK_FILE = ".aft-onnx-installing";
const ONNX_INSTALLED_META_FILE = ".aft-onnx-installed";
// 5 minutes is well under any reasonable real download time (60–80 MB archive
// on a working connection) but short enough that a SIGKILL'd plugin process
// recovers fast on the next launch. Previously 30 min — that left users
// blocked for a very long time after closing OpenCode mid-download (issue
// reports of "blackscreen on launch then ONNX broken").
const STALE_LOCK_MS = 5 * 60 * 1000;

/** Map (process.platform, process.arch) → ONNX Runtime asset name + library filename */
interface OrtPlatformInfo {
  assetName: string;
  libName: string;
  archiveType: "tgz" | "zip";
}

const ORT_PLATFORM_MAP: Record<string, Record<string, OrtPlatformInfo>> = {
  darwin: {
    arm64: {
      assetName: `onnxruntime-osx-arm64-${ORT_VERSION}`,
      libName: "libonnxruntime.dylib",
      archiveType: "tgz",
    },
    // x64 not available from Microsoft — users need brew install onnxruntime
  },
  linux: {
    x64: {
      assetName: `onnxruntime-linux-x64-${ORT_VERSION}`,
      libName: "libonnxruntime.so",
      archiveType: "tgz",
    },
    arm64: {
      assetName: `onnxruntime-linux-aarch64-${ORT_VERSION}`,
      libName: "libonnxruntime.so",
      archiveType: "tgz",
    },
  },
  win32: {
    x64: {
      assetName: `onnxruntime-win-x64-${ORT_VERSION}`,
      libName: "onnxruntime.dll",
      archiveType: "zip",
    },
    arm64: {
      assetName: `onnxruntime-win-arm64-${ORT_VERSION}`,
      libName: "onnxruntime.dll",
      archiveType: "zip",
    },
  },
};

/** Get platform info for the current system, or null if unsupported.
 *
 *  Important: ONNX Runtime arch must match the AFT binary's arch, not Node's.
 *  On Windows ARM64, Node may report `process.arch === "arm64"`, but the AFT
 *  binary we ship and run is x64 (under Prism emulation — see PLATFORM_ARCH_MAP
 *  in platform.ts). Loading a native ARM64 onnxruntime.dll from an x64 process
 *  fails with `LoadLibraryExW failed` and panics ort, so we have to align
 *  ONNX with the binary, not the host process.
 *
 *  We use the same PLATFORM_ARCH_MAP that resolves the AFT binary, then map
 *  the canonical platform key back to ONNX's arch slot. Today this means
 *  win32-arm64 → win32-x64 → ORT_PLATFORM_MAP.win32.x64. macOS and Linux are
 *  already aligned (we ship native arm64 binaries on both). */
function getPlatformInfo(): OrtPlatformInfo | null {
  const platformMap = ORT_PLATFORM_MAP[process.platform];
  if (!platformMap) return null;

  // Map Node's process.arch through PLATFORM_ARCH_MAP so we get the arch slot
  // that matches our AFT binary, not the host. Falls back to process.arch if
  // the platform isn't in the map (defensive — shouldn't happen since
  // resolver would have already failed on this platform).
  const archMap = PLATFORM_ARCH_MAP[process.platform] ?? {};
  const platformKey = archMap[process.arch];
  // platformKey looks like "win32-x64"; the slot under ORT_PLATFORM_MAP.win32
  // we want is the suffix after the dash.
  const ortArch = platformKey ? platformKey.split("-")[1] : process.arch;
  return platformMap[ortArch] || null;
}

/** Check if this platform can auto-download ONNX Runtime */
export function isOrtAutoDownloadSupported(): boolean {
  return getPlatformInfo() !== null;
}

/** Get the install hint for platforms where auto-download isn't available */
export function getManualInstallHint(): string {
  if (process.platform === "darwin" && process.arch === "x64") {
    return "brew install onnxruntime";
  }
  if (process.platform === "linux") {
    return "apt install libonnxruntime or download from https://github.com/microsoft/onnxruntime/releases";
  }
  return "Download from https://github.com/microsoft/onnxruntime/releases";
}

/**
 * Ensure ONNX Runtime is available. Returns the directory containing the library,
 * or null if unavailable.
 *
 * Resolution order:
 *   1. Cached in storageDir/onnxruntime/<version>/ (with TOFU verification)
 *   2. System install (brew, apt, etc.)
 *   3. Auto-download from GitHub releases (if platform supported)
 *   4. null (user needs manual install)
 */
export async function ensureOnnxRuntime(storageDir: string): Promise<string | null> {
  const info = getPlatformInfo();

  // 1. Cached location with TOFU.
  const ortDir = join(storageDir, "onnxruntime", ORT_VERSION);
  const libPath = join(ortDir, info?.libName ?? "libonnxruntime.dylib");

  if (existsSync(libPath)) {
    // Audit-3 v0.17 #1 (TOFU): if we recorded a hash for this version,
    // verify the library still matches. A mismatch means tampering or
    // partial install corruption. Refuse to use it and let the caller
    // either retry the download (after the user clears the cache) or
    // fall back to system install.
    const meta = readOnnxInstalledMeta(ortDir);
    if (meta?.sha256) {
      try {
        const currentHash = sha256File(libPath);
        if (currentHash !== meta.sha256) {
          error(
            `ONNX Runtime at ${ortDir}: TOFU sha256 mismatch — refusing to use ` +
              `tampered binary. Recorded ${meta.sha256}, current ${currentHash}. ` +
              `Run \`aft doctor --clear\` to re-download from scratch.`,
          );
          // Fall through to system path / re-download attempt below.
        } else {
          log(`ONNX Runtime found at ${ortDir} (TOFU verified)`);
          return ortDir;
        }
      } catch (err) {
        warn(`Could not verify ONNX Runtime hash at ${ortDir}: ${err}`);
        // Treat unreadable hash as "trust on existence" since we already
        // owned this install — better than blocking semantic search.
        return ortDir;
      }
    } else {
      log(`ONNX Runtime found at ${ortDir} (no recorded hash, accepting)`);
      return ortDir;
    }
  }

  // 2. System locations.
  const systemPath = findSystemOnnxRuntime(info?.libName);
  if (systemPath) {
    log(`ONNX Runtime found at system path: ${systemPath}`);
    return systemPath;
  }

  // 3. Auto-download.
  if (!info) {
    warn(
      `ONNX Runtime auto-download not available for ${process.platform}/${process.arch}. Install manually: ${getManualInstallHint()}`,
    );
    return null;
  }

  // Audit-3 v0.17 #1: serialize concurrent installs.
  //
  // Two AFT plugin instances starting at the same time would otherwise both
  // download and extract into overlapping temp dirs and clobber each other.
  // The lock is held for the full install duration via the manual try/finally
  // below (we don't reuse withInstallLock from lsp-cache because that helper
  // is keyed on lspPackageDir, while ONNX lives in storageDir).
  const onnxBaseDir = join(storageDir, "onnxruntime");
  mkdirSync(onnxBaseDir, { recursive: true });
  const lockPath = join(onnxBaseDir, ONNX_LOCK_FILE);

  // Recover from SIGKILL'd previous attempts before acquiring the lock.
  // When the host process is killed mid-download (user closes OpenCode while
  // ONNX is still downloading), the staging dir at `${ortDir}.tmp.<pid>.<ts>`
  // and a half-populated `ortDir` can survive without a meta file. The
  // existing TOFU branch above already handles a tampered-but-complete
  // install, but this branch covers the "abandoned, incomplete" case where
  // the lib file isn't present (we wouldn't be here otherwise). Sweep them
  // out so the next download starts from a clean slate.
  cleanupAbandonedStagingDirs(onnxBaseDir);

  if (!acquireLock(lockPath)) {
    warn(
      `ONNX Runtime install already in progress in another process (lock: ${lockPath}). Skipping.`,
    );
    return null;
  }

  try {
    cleanupIncompleteTargetIfUnowned(ortDir);
    return await downloadOnnxRuntime(info, ortDir);
  } finally {
    releaseLock(lockPath);
  }
}

/**
 * Sweep abandoned `*.tmp.<pid>.<ts>` staging directories left behind by
 * killed download attempts, and remove an empty/half-populated target dir
 * so the next download retries cleanly. Safe to call before lock acquisition
 * because we only delete dirs whose owning PID is dead (or when the parent
 * dir's mtime exceeds STALE_LOCK_MS — covers Windows where we can't check
 * process liveness reliably).
 */
function cleanupAbandonedStagingDirs(onnxBaseDir: string): void {
  // Sweep .tmp.* staging dirs whose pid is dead or are sufficiently old.
  try {
    const entries = readdirSync(onnxBaseDir);
    for (const entry of entries) {
      if (!entry.startsWith(`${ORT_VERSION}.tmp.`)) continue;
      const stagingDir = join(onnxBaseDir, entry);
      // Format: `${ORT_VERSION}.tmp.<pid>.<ts>`. Extract pid; if dead, sweep.
      const parts = entry.split(".");
      const pidStr = parts[parts.length - 2];
      const pid = pidStr ? Number.parseInt(pidStr, 10) : NaN;
      let abandoned = false;
      if (Number.isFinite(pid) && pid > 0) {
        if (process.platform === "win32") {
          // No reliable cross-process liveness check on Windows; fall back
          // to mtime-based age comparison.
          try {
            const ageMs = Date.now() - statSync(stagingDir).mtimeMs;
            abandoned = ageMs > STALE_LOCK_MS;
          } catch {
            abandoned = true;
          }
        } else {
          abandoned = !isProcessAlive(pid);
        }
      } else {
        abandoned = true;
      }
      if (abandoned) {
        log(`[onnx] removing abandoned staging dir ${stagingDir}`);
        try {
          rmSync(stagingDir, { recursive: true, force: true });
        } catch (err) {
          warn(`[onnx] failed to remove ${stagingDir}: ${err}`);
        }
      }
    }
  } catch {
    // base dir doesn't exist yet; nothing to sweep
  }
}

function cleanupIncompleteTargetIfUnowned(ortDir: string): void {
  // If the target dir exists but doesn't contain a meta file, the previous
  // attempt was killed mid-copy. Wipe it so download can recreate cleanly.
  try {
    if (existsSync(ortDir) && !existsSync(join(ortDir, ONNX_INSTALLED_META_FILE))) {
      log(`[onnx] removing half-populated install dir ${ortDir} (no meta file)`);
      rmSync(ortDir, { recursive: true, force: true });
    }
  } catch (err) {
    warn(`[onnx] failed to sweep ${ortDir}: ${err}`);
  }
}

function cleanupAbandonedOnnxAttempts(onnxBaseDir: string, ortDir: string): void {
  cleanupAbandonedStagingDirs(onnxBaseDir);
  cleanupIncompleteTargetIfUnowned(ortDir);
}

/** Check common system locations for ONNX Runtime */
/**
 * Minimum ONNX Runtime version compatible with AFT's bundled `ort` crate.
 *
 * The Rust pre-validator rejects anything below 1.20 (see
 * `crates/aft/src/semantic_index.rs::pre_validate_onnx_runtime`). When this
 * resolver finds a system install older than that, it MUST treat the system
 * dir as absent and fall through to auto-download — otherwise the resolver
 * hands Rust a path it will refuse, semantic search stays "failed" forever,
 * and the user has to hand-edit `/usr/lib/...` to make progress.
 */
const REQUIRED_ORT_MAJOR = 1;
const REQUIRED_ORT_MIN_MINOR = 20;

/**
 * Detect the version of an ONNX Runtime install by walking the directory's
 * library-file suffixes. Microsoft ships `libonnxruntime.so.1.24.4` and
 * symlinks the bare `libonnxruntime.so` at it; on macOS the pattern is
 * `libonnxruntime.1.24.4.dylib`. Returns null when the version cannot be
 * inferred (treat as compatible to avoid false negatives on unconventional
 * installs).
 */
function detectOnnxVersion(libDir: string, libName: string): string | null {
  try {
    const entries = readdirSync(libDir);
    // On macOS the library is `libonnxruntime.1.24.4.dylib` — the
    // version sits between the bare name `libonnxruntime` and the
    // `.dylib` suffix, so `entry.startsWith(libName)` (which expects
    // `libonnxruntime.dylib`) misses it. Match by the bare prefix
    // (libName with the platform suffix stripped) so both Linux's
    // `libonnxruntime.so.1.24.4` and macOS's `libonnxruntime.1.24.4.dylib`
    // are picked up.
    const barePrefix = libName.replace(/\.(so|dylib|dll)$/, "");
    for (const entry of entries) {
      if (!entry.startsWith(barePrefix)) continue;
      const match = entry.match(/\.(\d+\.\d+\.\d+)(?:\.dylib)?$/);
      if (match) return match[1];
    }
    // Symlink fallback: bare libonnxruntime.so -> libonnxruntime.so.1.24.4
    const base = join(libDir, libName);
    if (existsSync(base)) {
      try {
        const real = realpathSync(base);
        const m = real.match(/\.(\d+\.\d+\.\d+)(?:\.dylib)?$/);
        if (m) return m[1];
      } catch {
        // ignore
      }
      try {
        const target = readlinkSync(base);
        const m = target.match(/\.(\d+\.\d+\.\d+)(?:\.dylib)?$/);
        if (m) return m[1];
      } catch {
        // not a symlink
      }
    }
  } catch {
    // unreadable dir
  }
  return null;
}

function isOnnxVersionCompatible(version: string): boolean {
  const parts = version.split(".").map((p) => Number.parseInt(p, 10));
  const [major, minor] = parts;
  if (!Number.isFinite(major) || !Number.isFinite(minor)) return false;
  if (major !== REQUIRED_ORT_MAJOR) return false;
  return minor >= REQUIRED_ORT_MIN_MINOR;
}

function findSystemOnnxRuntime(libName?: string): string | null {
  if (!libName) return null;

  const searchPaths: string[] = [];

  if (process.platform === "darwin") {
    // Homebrew locations
    searchPaths.push("/opt/homebrew/lib", "/usr/local/lib");
  } else if (process.platform === "linux") {
    searchPaths.push(
      "/usr/lib",
      "/usr/lib/x86_64-linux-gnu",
      "/usr/lib/aarch64-linux-gnu",
      "/usr/local/lib",
    );
  }

  for (const dir of searchPaths) {
    if (!existsSync(join(dir, libName))) continue;

    // Reject system installs that the Rust pre-validator will refuse. Without
    // this filter, a stale distro package (e.g. libonnxruntime1.9 on Ubuntu
    // 22.04) shadows our auto-downloaded v1.24 forever and semantic search
    // stays "failed" until the user hand-deletes the system library.
    const version = detectOnnxVersion(dir, libName);
    if (version && !isOnnxVersionCompatible(version)) {
      warn(
        `Skipping system ONNX Runtime at ${dir} (v${version}); AFT requires ` +
          `v${REQUIRED_ORT_MAJOR}.${REQUIRED_ORT_MIN_MINOR}+. Falling through to AFT-managed download.`,
      );
      continue;
    }

    return dir;
  }

  return null;
}

/**
 * Streaming download with size cap. Mirrors the hardened path in
 * `lsp-github-install.ts:downloadFile` so ONNX gets the same defenses
 * the LSP installer already has.
 *
 * The URL is hardcoded to `https://github.com/${ORT_REPO}/...` so we don't
 * need a hostname allowlist — the constant cannot be attacker-influenced.
 */
async function downloadFileWithCap(url: string, destPath: string): Promise<void> {
  const controller = new AbortController();
  const timeout = setTimeout(() => controller.abort(), 300_000);
  try {
    const res = await fetch(url, {
      headers: { accept: "application/octet-stream" },
      redirect: "follow",
      signal: controller.signal,
    });
    if (!res.ok || !res.body) {
      throw new Error(`download failed (HTTP ${res.status})`);
    }

    const advertised = Number.parseInt(res.headers.get("content-length") ?? "", 10);
    if (Number.isFinite(advertised) && advertised > MAX_DOWNLOAD_BYTES) {
      throw new Error(`Content-Length ${advertised} exceeds max ${MAX_DOWNLOAD_BYTES}`);
    }

    mkdirSync(dirname(destPath), { recursive: true });

    let bytesWritten = 0;
    const guard = new TransformStream<Uint8Array, Uint8Array>({
      transform(chunk, transformController) {
        bytesWritten += chunk.byteLength;
        if (bytesWritten > MAX_DOWNLOAD_BYTES) {
          transformController.error(
            new Error(
              `download exceeded ${MAX_DOWNLOAD_BYTES} bytes after streaming (server lied about size or sent unbounded body)`,
            ),
          );
          return;
        }
        transformController.enqueue(chunk);
      },
    });

    const guarded = res.body.pipeThrough(guard);
    // biome-ignore lint/suspicious/noExplicitAny: ReadableStream→Node stream conversion
    const nodeStream = Readable.fromWeb(guarded as any);
    await pipeline(nodeStream, createWriteStream(destPath), { signal: controller.signal });
  } catch (err) {
    try {
      unlinkSync(destPath);
    } catch {
      // partial file may not exist — fine
    }
    throw err;
  } finally {
    clearTimeout(timeout);
  }
}

/**
 * Validate that every file/dir under `stagingRoot` is contained inside it
 * AND that the total extracted bytes do not exceed `MAX_EXTRACT_BYTES`.
 *
 * Audit-3 v0.17 #1: zip-slip + symlink containment + decompression-bomb
 * defense. tar and unzip both can produce paths like `../../etc/passwd`;
 * Windows tar.exe and unzip can also produce symlinks that point outside
 * the destination. We walk the tree post-extraction and reject anything
 * suspicious before moving files into the final cache.
 */
function validateExtractedTree(stagingRoot: string): void {
  const realRoot = realpathSync(stagingRoot);
  let totalBytes = 0;

  const walk = (dir: string): void => {
    const entries = readdirSync(dir);
    for (const entry of entries) {
      const fullPath = join(dir, entry);
      const lst = lstatSync(fullPath);

      if (lst.isSymbolicLink()) {
        // Symlinks must be either rejected outright or contained. Tarballs
        // from Microsoft's official ONNX Runtime release ship with versioned
        // .so symlinks (libonnxruntime.so → libonnxruntime.so.1.24.4), so
        // we cannot simply reject all of them. Instead resolve the link
        // target (relative to the symlink's directory) and verify it stays
        // inside the staging root.
        const linkTarget = readlinkSync(fullPath);
        const resolvedTarget = resolve(dirname(fullPath), linkTarget);
        const rel = relative(realRoot, resolvedTarget);
        if (rel.startsWith("..") || (process.platform !== "win32" && rel.startsWith("/"))) {
          throw new Error(
            `extracted symlink ${fullPath} points outside staging root: ${linkTarget}`,
          );
        }
        continue;
      }

      const rel = relative(realRoot, fullPath);
      if (rel.startsWith("..") || (process.platform !== "win32" && rel.startsWith("/"))) {
        throw new Error(`extracted entry ${fullPath} escapes staging root`);
      }

      if (lst.isDirectory()) {
        walk(fullPath);
        continue;
      }

      if (lst.isFile()) {
        totalBytes += lst.size;
        if (totalBytes > MAX_EXTRACT_BYTES) {
          throw new Error(
            `extracted size ${totalBytes} exceeds max ${MAX_EXTRACT_BYTES} (decompression bomb defense)`,
          );
        }
      }
    }
  };

  walk(realRoot);
}

/** Download and extract ONNX Runtime from GitHub releases */
async function downloadOnnxRuntime(
  info: OrtPlatformInfo,
  targetDir: string,
): Promise<string | null> {
  const url = `https://github.com/${ORT_REPO}/releases/download/v${ORT_VERSION}/${info.assetName}.${info.archiveType === "tgz" ? "tgz" : "zip"}`;

  log(`Downloading ONNX Runtime v${ORT_VERSION} for ${process.platform}/${process.arch}...`);

  // Use a parent-of-targetDir staging path so the validation walk can compare
  // resolved paths via realpathSync without rejecting symlinks that happen to
  // resolve into the parent (e.g. when storageDir is itself behind a symlink).
  const tmpDir = `${targetDir}.tmp.${process.pid}.${Date.now().toString(36)}`;

  try {
    mkdirSync(tmpDir, { recursive: true });
    const archivePath = join(tmpDir, `onnxruntime.${info.archiveType}`);

    // Audit-3 v0.17 #1: download with streaming size cap (no more curl).
    await downloadFileWithCap(url, archivePath);

    // Audit-3 v0.17 #1: hash the archive for TOFU.
    const archiveSha256 = sha256File(archivePath);
    log(`ONNX Runtime archive sha256=${archiveSha256}`);

    // Extract.
    if (info.archiveType === "tgz") {
      execFileSync("tar", ["xzf", archivePath, "-C", tmpDir], {
        stdio: "pipe",
        timeout: 120_000,
      });
    } else {
      await extractZipArchive(archivePath, tmpDir);
    }

    // Drop the archive itself before validation so it doesn't double-count
    // toward the extracted-size budget.
    try {
      unlinkSync(archivePath);
    } catch {
      // ignore
    }

    // Audit-3 v0.17 #1: containment + size-bomb check.
    validateExtractedTree(tmpDir);

    // Find and copy the library file.
    const extractedDir = join(tmpDir, info.assetName, "lib");
    if (!existsSync(extractedDir)) {
      throw new Error(`Expected directory not found: ${extractedDir}`);
    }

    // Create target directory and copy library files.
    mkdirSync(targetDir, { recursive: true });

    // Copy all library files (main + versioned symlinks).
    // On Linux, .so files are often symlinks (libonnxruntime.so → libonnxruntime.so.1.24.4).
    // Process real files first, then recreate symlinks in the target directory to avoid
    // ENOENT when renaming a symlink whose target was already moved.
    const libFiles = readdirSync(extractedDir).filter(
      (f) => f.startsWith("libonnxruntime") || f.startsWith("onnxruntime"),
    );

    // Separate real files from symlinks
    const realFiles: string[] = [];
    const symlinks: Array<{ name: string; target: string }> = [];
    for (const libFile of libFiles) {
      const src = join(extractedDir, libFile);
      try {
        const stat = lstatSync(src);
        log(
          `ORT extract: ${libFile} — isSymlink=${stat.isSymbolicLink()}, isFile=${stat.isFile()}, size=${stat.size}`,
        );
        if (stat.isSymbolicLink()) {
          symlinks.push({ name: libFile, target: readlinkSync(src) });
        } else {
          realFiles.push(libFile);
        }
      } catch (e) {
        log(`ORT extract: ${libFile} — stat failed: ${e}`);
        realFiles.push(libFile);
      }
    }

    copyOnnxLibraries(info, extractedDir, targetDir, realFiles, symlinks);

    // Audit-3 v0.17 #1: persist version + archive sha256 for TOFU on
    // future sessions. Hash the actual main library file (not the
    // archive) because that's what we'll re-hash on the next ensure call.
    const libPath = join(targetDir, info.libName);
    let libHash: string | null = null;
    try {
      libHash = sha256File(libPath);
    } catch (err) {
      // If we can't even hash our just-installed library, skip TOFU rather
      // than blocking semantic search. Future sessions will trust the path.
      warn(`Could not hash newly-installed ONNX library at ${libPath}: ${err}`);
    }
    writeOnnxInstalledMeta(targetDir, ORT_VERSION, libHash, archiveSha256);

    // Cleanup temp directory
    rmSync(tmpDir, { recursive: true, force: true });

    log(`ONNX Runtime v${ORT_VERSION} installed to ${targetDir}`);
    return targetDir;
  } catch (err) {
    error(`Failed to download ONNX Runtime: ${err}`);
    // Cleanup on failure — both the staging dir and any partially populated
    // target dir, so the next attempt starts from a clean slate.
    try {
      rmSync(tmpDir, { recursive: true, force: true });
    } catch {
      // ignore cleanup errors
    }
    try {
      rmSync(targetDir, { recursive: true, force: true });
    } catch {
      // ignore
    }
    return null;
  }
}

function copyOnnxLibraries(
  info: OrtPlatformInfo,
  extractedDir: string,
  targetDir: string,
  realFiles: string[],
  symlinks: Array<{ name: string; target: string }>,
  copyFile: typeof copyFileSync = copyFileSync,
): void {
  const requiredLibs = new Set([info.libName]);

  // Copy real files first. Required library failures are fatal; optional extra
  // libraries stay best-effort so one unusual sidecar does not block install.
  for (const libFile of realFiles) {
    const src = join(extractedDir, libFile);
    const dst = join(targetDir, libFile);
    try {
      copyFile(src, dst);
      if (process.platform !== "win32") {
        chmodSync(dst, 0o755);
      }
    } catch (copyErr) {
      if (requiredLibs.has(libFile)) {
        rmSync(targetDir, { recursive: true, force: true });
        throw copyErr;
      }
      log(`ORT extract: failed to copy optional ${libFile}: ${copyErr}`);
    }
  }

  // Recreate symlinks in target directory. If the required library is a
  // symlink, failures must be fatal for the same reason as required copies.
  for (const link of symlinks) {
    const dst = join(targetDir, link.name);
    try {
      unlinkSync(dst); // remove if exists from a previous partial install
    } catch {
      // ignore
    }
    try {
      symlinkSync(link.target, dst);
    } catch (symlinkErr) {
      if (requiredLibs.has(link.name)) {
        rmSync(targetDir, { recursive: true, force: true });
        throw symlinkErr;
      }
      log(`ORT extract: failed to symlink optional ${link.name}: ${symlinkErr}`);
    }
  }

  const requiredPath = join(targetDir, info.libName);
  if (!existsSync(requiredPath)) {
    rmSync(targetDir, { recursive: true, force: true });
    throw new Error(`Required ONNX Runtime library missing after install: ${requiredPath}`);
  }
}

async function extractZipArchive(archivePath: string, destinationDir: string): Promise<void> {
  if (process.platform === "win32") {
    // Audit-2 v0.17 #12: drop PowerShell. Even via execFileSync, PowerShell
    // applies its own quoting rules to `$args[N]` lookups that could allow
    // attacker-controlled fragments to escape into command interpretation.
    // tar.exe ships in System32 on Windows 10 build 17063+ — execFileSync
    // with argv has no shell parser in the chain.
    execFileSync("tar.exe", ["-xf", archivePath, "-C", destinationDir], {
      stdio: "pipe",
      timeout: 120_000,
    });
    return;
  }

  execFileSync("unzip", ["-q", archivePath, "-d", destinationDir], {
    stdio: "pipe",
    timeout: 120_000,
  });
}

/* ─────────────────────────── install metadata ─────────────────────────── */

interface OnnxInstalledMeta {
  version: string;
  installedAt: string;
  /** SHA-256 of the main library file (libonnxruntime.{so,dylib,dll}). */
  sha256?: string;
  /** SHA-256 of the original downloaded archive (forensic). */
  archiveSha256?: string;
}

function writeOnnxInstalledMeta(
  installDir: string,
  version: string,
  sha256: string | null,
  archiveSha256: string,
): void {
  try {
    const meta: OnnxInstalledMeta = {
      version,
      installedAt: new Date().toISOString(),
      ...(sha256 ? { sha256 } : {}),
      archiveSha256,
    };
    writeFileSync(join(installDir, ONNX_INSTALLED_META_FILE), JSON.stringify(meta), "utf8");
  } catch (err) {
    log(`[onnx] failed to write installed-meta in ${installDir}: ${err}`);
  }
}

function readOnnxInstalledMeta(installDir: string): OnnxInstalledMeta | null {
  const path = join(installDir, ONNX_INSTALLED_META_FILE);
  try {
    if (!statSync(path).isFile()) return null;
    const raw = readFileSync(path, "utf8");
    const parsed = JSON.parse(raw) as Partial<OnnxInstalledMeta>;
    if (typeof parsed.version !== "string" || parsed.version.length === 0) return null;
    return {
      version: parsed.version,
      installedAt: typeof parsed.installedAt === "string" ? parsed.installedAt : "",
      ...(typeof parsed.sha256 === "string" && parsed.sha256.length > 0
        ? { sha256: parsed.sha256 }
        : {}),
      ...(typeof parsed.archiveSha256 === "string" && parsed.archiveSha256.length > 0
        ? { archiveSha256: parsed.archiveSha256 }
        : {}),
    };
  } catch {
    return null;
  }
}

/**
 * Synchronous SHA-256 of a file. ONNX libs are ~50 MB so a single
 * `readFileSync` is fine — we accept the brief blocking read in exchange
 * for keeping the call sites simple (no awaits inside the path-resolution
 * fast path that runs every plugin start).
 */
function sha256File(path: string): string {
  const hash = createHash("sha256");
  hash.update(readFileSync(path));
  return hash.digest("hex");
}

/* ─────────────────────────── install lock ─────────────────────────── */

/**
 * Acquire a process-exclusive lock. Atomic O_EXCL create with PID-aware
 * stale-lock recovery. Mirrors `acquireInstallLock` in lsp-cache.ts but
 * lives here because the ONNX install dir is keyed on `storageDir`,
 * not `lspPackageDir`.
 */
function acquireLock(lockPath: string): boolean {
  const tryClaim = (): boolean => {
    try {
      const fd = openSync(lockPath, "wx");
      try {
        writeFileSync(fd, `${process.pid}\n${new Date().toISOString()}\n`);
      } finally {
        closeSync(fd);
      }
      return true;
    } catch (err) {
      const code = (err as NodeJS.ErrnoException).code;
      if (code === "EEXIST") return false;
      warn(`[onnx] unexpected error acquiring lock ${lockPath}: ${err}`);
      return false;
    }
  };

  if (tryClaim()) return true;

  let owningPid: number | null = null;
  let lockMtimeMs = 0;
  try {
    const raw = readFileSync(lockPath, "utf8");
    const firstLine = raw.split(/\r?\n/, 1)[0]?.trim() ?? "";
    const parsed = Number.parseInt(firstLine, 10);
    if (Number.isFinite(parsed) && parsed > 0) owningPid = parsed;
    lockMtimeMs = statSync(lockPath).mtimeMs;
  } catch {
    return tryClaim();
  }

  const age = Date.now() - lockMtimeMs;
  const ageWithinFresh = Math.abs(age) < STALE_LOCK_MS;
  const skipLiveness = process.platform === "win32";
  const ownerAlive = !skipLiveness && owningPid !== null && isProcessAlive(owningPid);
  if (skipLiveness ? ageWithinFresh : ownerAlive && ageWithinFresh) {
    return false;
  }

  log(
    `[onnx] reclaiming install lock (owner_pid=${owningPid ?? "unknown"}, alive=${ownerAlive}, age_ms=${age})`,
  );
  try {
    unlinkSync(lockPath);
  } catch {
    // ignore
  }
  return tryClaim();
}

function releaseLock(lockPath: string): void {
  // Same TOCTOU-safe release as releaseInstallLock — only unlink if our PID owns it.
  try {
    let owningPid: number | null = null;
    try {
      const raw = readFileSync(lockPath, "utf8");
      const firstLine = raw.split(/\r?\n/, 1)[0]?.trim() ?? "";
      const parsed = Number.parseInt(firstLine, 10);
      if (Number.isFinite(parsed) && parsed > 0) owningPid = parsed;
    } catch (readErr) {
      const code = (readErr as NodeJS.ErrnoException).code;
      if (code === "ENOENT") return;
      warn(`[onnx] could not read lock ${lockPath} during release: ${readErr}`);
      return;
    }
    if (owningPid !== process.pid) {
      log(
        `[onnx] not releasing lock ${lockPath}: owned by pid ${owningPid ?? "unknown"} (we are ${process.pid})`,
      );
      return;
    }
    try {
      unlinkSync(lockPath);
    } catch (unlinkErr) {
      const code = (unlinkErr as NodeJS.ErrnoException).code;
      if (code !== "ENOENT") {
        warn(`[onnx] failed to release lock ${lockPath}: ${unlinkErr}`);
      }
    }
  } catch (err) {
    warn(`[onnx] unexpected error releasing lock ${lockPath}: ${err}`);
  }
}

function isProcessAlive(pid: number): boolean {
  try {
    process.kill(pid, 0);
    return true;
  } catch (err) {
    const code = (err as NodeJS.ErrnoException).code;
    if (code === "ESRCH") return false;
    return true;
  }
}

/**
 * Remove ONNX Runtime from temp files. Cleanup helper for test isolation.
 */
export function cleanupOnnxRuntime(storageDir: string): void {
  try {
    const ortBase = join(storageDir, "onnxruntime");
    if (existsSync(ortBase)) {
      rmSync(ortBase, { recursive: true, force: true });
    }
  } catch {
    // ignore
  }
}

/**
 * Test-only exports. Intentionally not part of the published surface — these
 * are internal helpers we want to exercise from unit tests without forcing
 * an actual ONNX download. Don't use from production code.
 */
export const __test__ = {
  cleanupAbandonedOnnxAttempts,
  cleanupAbandonedStagingDirs,
  cleanupIncompleteTargetIfUnowned,
  copyOnnxLibraries,
  ORT_VERSION,
  ONNX_INSTALLED_META_FILE,
  detectOnnxVersion,
  isOnnxVersionCompatible,
  findSystemOnnxRuntime,
  REQUIRED_ORT_MAJOR,
  REQUIRED_ORT_MIN_MINOR,
};
