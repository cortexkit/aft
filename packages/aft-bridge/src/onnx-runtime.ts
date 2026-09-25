/**
 * Auto-download and manage ONNX Runtime shared library for semantic search.
 *
 * Downloads the CPU-only ONNX Runtime from Microsoft's GitHub releases.
 * The library is cached in the storage directory alongside semantic index data.
 *
 * Security hardening: an earlier implementation used `curl` with no size cap,
 * no archive containment validation, no install lock, and no integrity
 * verification — an install path that bypassed every defense the LSP GitHub
 * installer already had. This brings ONNX onto the same security floor:
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
import { createHash, randomBytes } from "node:crypto";
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
  renameSync,
  rmSync,
  statSync,
  symlinkSync,
  unlinkSync,
  writeFileSync,
} from "node:fs";
import { basename, dirname, isAbsolute, join, relative, resolve, win32 } from "node:path";
import { Readable } from "node:stream";
import { pipeline } from "node:stream/promises";
import { error, log, warn } from "./active-logger.js";
import { probeOnnxRuntimeLoadable } from "./onnx-probe.js";
import { withPathPrepended } from "./path-env.js";
import { PLATFORM_ARCH_MAP } from "./platform.js";
import { execTarExtractionSync } from "./tar-executable.js";

const ORT_VERSION = "1.24.4";
const ORT_REPO = "microsoft/onnxruntime";

// Streaming + extraction size caps.
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
// How long a caller waits for another install (in this process or another
// one) to finish before giving up. The lock turns reclaimable after
// STALE_LOCK_MS, so this leaves room for one stale-lock recovery plus a full
// download of our own.
const LOCK_WAIT_MS = 2 * STALE_LOCK_MS;
const LOCK_POLL_MS = 1000;

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
  return resolveOnnxRuntime(storageDir, {});
}

/**
 * Why the most recent managed install attempt in this process failed, or null
 * when the last attempt succeeded or none has failed. Hosts read it after
 * {@link ensureOnnxRuntime} returns null so the user is told that semantic
 * search is unavailable and why, not only the log.
 */
let lastInstallFailure: string | null = null;

export function getOnnxRuntimeInstallFailure(): string | null {
  return lastInstallFailure;
}

/**
 * One resolution per storage directory at a time in this module instance.
 * Concurrent callers share the same promise instead of racing for the install
 * lock. Settled resolutions are dropped so a later call re-checks the disk.
 */
const inflightResolutions = new Map<string, Promise<string | null>>();

/**
 * Test seams for {@link resolveOnnxRuntime}. Production passes none: the
 * platform table, the standard system locations, and the real download.
 */
interface OnnxRuntimeResolutionSeams {
  platformInfo?: OrtPlatformInfo | null;
  systemSearchPaths?: string[];
  download?: (info: OrtPlatformInfo, targetDir: string) => Promise<string | null>;
  /** How often a caller waiting on another install re-checks the lock. */
  lockPollMs?: number;
}

function resolveOnnxRuntime(
  storageDir: string,
  seams: OnnxRuntimeResolutionSeams,
): Promise<string | null> {
  const existing = inflightResolutions.get(storageDir);
  if (existing) return existing;
  const resolution = resolveOnnxRuntimeUncoalesced(storageDir, seams).finally(() => {
    inflightResolutions.delete(storageDir);
  });
  inflightResolutions.set(storageDir, resolution);
  return resolution;
}

/**
 * The cached managed runtime for this version, or null when there is none or
 * it fails TOFU verification.
 */
function findCachedOnnxRuntime(ortVersionDir: string, libName: string): string | null {
  // Keep the version root separate from the resolved library directory. The
  // root owns cleanup, downloads, and TOFU metadata; the resolved dir only
  // feeds the return value / ORT_DYLIB_PATH and may be `<version>/lib` for
  // manual Microsoft-archive installs (#71).
  const resolvedOrtDir = resolveCachedOnnxRuntimeDir(ortVersionDir, libName);
  const libPath = join(resolvedOrtDir, libName);

  if (existsSync(libPath)) {
    // TOFU: if we recorded a hash for this version,
    // verify the library still matches. A mismatch means tampering or
    // partial install corruption. Refuse to use it and let the caller
    // either retry the download (after the user clears the cache) or
    // fall back to system install.
    // Our installer writes the TOFU meta file to the version root, never the
    // lib/ subdir, so always read it from there.
    const meta = readOnnxInstalledMeta(ortVersionDir);
    if (meta?.sha256) {
      try {
        const currentHash = sha256File(libPath);
        if (currentHash !== meta.sha256) {
          error(
            `ONNX Runtime at ${resolvedOrtDir}: TOFU sha256 mismatch — refusing to use ` +
              `tampered binary. Recorded ${meta.sha256}, current ${currentHash}. ` +
              `Run \`npx @cortexkit/aft doctor --clear\` to re-download from scratch.`,
          );
          // Fall through to system path / re-download attempt below.
        } else {
          log(`ONNX Runtime found at ${resolvedOrtDir} (TOFU verified)`);
          return resolvedOrtDir;
        }
      } catch (err) {
        warn(`Could not verify ONNX Runtime hash at ${resolvedOrtDir}: ${err}`);
        // Treat unreadable hash as "trust on existence" since we already
        // owned this install — better than blocking semantic search.
        return resolvedOrtDir;
      }
    } else {
      log(`ONNX Runtime found at ${resolvedOrtDir} (no recorded hash, accepting)`);
      return resolvedOrtDir;
    }
  }
  return null;
}

async function resolveOnnxRuntimeUncoalesced(
  storageDir: string,
  seams: OnnxRuntimeResolutionSeams,
): Promise<string | null> {
  const info = seams.platformInfo !== undefined ? seams.platformInfo : getPlatformInfo();

  // 1. Cached location with TOFU.
  const ortVersionDir = join(storageDir, "onnxruntime", ORT_VERSION);
  const libName = info?.libName ?? "libonnxruntime.dylib";
  const cached = findCachedOnnxRuntime(ortVersionDir, libName);
  if (cached) return cached;

  // 2. System locations.
  const systemPath = findSystemOnnxRuntime(info?.libName, seams.systemSearchPaths);
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

  // Serialize installs across processes.
  //
  // Several plugin starts can ask for the runtime at once: OpenCode may load
  // the plugin more than once per process in module graphs that share no
  // state, and two OpenCode windows can start together. Exactly one of them
  // installs while holding the lock file; the others wait for it and then use
  // the runtime it published, instead of giving up with no runtime. (We don't
  // reuse withInstallLock from lsp-cache because that helper is keyed on
  // lspPackageDir, while ONNX lives in storageDir.)
  const onnxBaseDir = join(storageDir, "onnxruntime");
  mkdirSync(onnxBaseDir, { recursive: true });
  const lockPath = join(onnxBaseDir, ONNX_LOCK_FILE);

  const pollMs = seams.lockPollMs ?? LOCK_POLL_MS;
  const deadline = Date.now() + LOCK_WAIT_MS;
  let announcedWait = false;
  while (!acquireLock(lockPath)) {
    if (!announcedWait) {
      log(`ONNX Runtime install already in progress (lock: ${lockPath}); waiting for it.`);
      announcedWait = true;
    }
    if (Date.now() > deadline) {
      lastInstallFailure = `timed out waiting for another ONNX Runtime install to finish (lock: ${lockPath})`;
      warn(`ONNX Runtime unavailable: ${lastInstallFailure}`);
      return null;
    }
    await new Promise((done) => setTimeout(done, pollMs));
    const published = findCachedOnnxRuntime(ortVersionDir, libName);
    if (published) {
      lastInstallFailure = null;
      return published;
    }
  }

  try {
    // Another holder may have finished between our cache check and taking
    // the lock; use its runtime rather than downloading again.
    const published = findCachedOnnxRuntime(ortVersionDir, libName);
    if (published) {
      lastInstallFailure = null;
      return published;
    }

    // Recover from SIGKILL'd previous attempts. When the host process is
    // killed mid-download (user closes OpenCode while ONNX is still
    // downloading), a staging dir at `${ortVersionDir}.tmp.<pid>.<id>` and a
    // half-populated `ortVersionDir` without a meta file can survive. The
    // sweep runs only while holding the lock, so it can never remove the
    // staging dir of an install that is still running.
    cleanupAbandonedStagingDirs(onnxBaseDir);
    cleanupIncompleteTargetIfUnowned(ortVersionDir);
    const installed = await (seams.download ?? downloadOnnxRuntime)(info, ortVersionDir);
    if (installed) lastInstallFailure = null;
    else lastInstallFailure ??= "ONNX Runtime install failed (see the AFT plugin log)";
    return installed;
  } finally {
    releaseLock(lockPath);
  }
}

/**
 * Sweep abandoned `*.tmp.<pid>.<ts>` staging directories left behind by
 * killed download attempts, and remove an empty/half-populated target dir
 * so the next download retries cleanly. Only dirs whose owning PID is dead
 * (or, on Windows, very old while the owner is still reported alive) are
 * removed, and the installer calls this only while holding the install lock,
 * so a staging dir that belongs to a running install is never touched.
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
          const ownerAlive = isProcessAlive(pid);
          if (!ownerAlive) {
            abandoned = true;
          } else {
            // Keep the existing stale-age escape hatch for very old Windows
            // attempts, but no longer impose a blind 5-minute wait when the
            // owning process has already exited.
            try {
              const ageMs = Date.now() - statSync(stagingDir).mtimeMs;
              abandoned = ageMs > STALE_LOCK_MS;
            } catch {
              abandoned = true;
            }
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
const INVALID_ORT_VERSION = "<invalid>";

function parseOnnxVersionFromPath(value: string): string | null {
  const name = basename(value);
  const semverish = name.match(
    /(?:^|[._-])(\d+\.\d+\.\d+(?:[-+][A-Za-z0-9.-]+)?)(?:\.(?:dylib|dll))?$/,
  );
  if (semverish) return semverish[1].split(/[-+]/, 1)[0];
  return /\d+\.\d+\.\d+/.test(name) ? INVALID_ORT_VERSION : null;
}

function parseOnnxVersionFromDirectoryPath(value: string): string | null {
  const parts = value
    .split(/[\\/]+/)
    .filter(Boolean)
    .reverse();
  for (const part of parts) {
    const version = parseOnnxVersionFromPath(part);
    if (version) return version;
  }
  return null;
}

function isPathInsideRoot(root: string, candidate: string): boolean {
  const rel = relative(root, candidate);
  return (
    rel === "" ||
    (!rel.startsWith("../") &&
      !rel.startsWith("..\\") &&
      rel !== ".." &&
      !isAbsolute(rel) &&
      !win32.isAbsolute(rel))
  );
}

/**
 * Detect the version of an ONNX Runtime install by walking the directory's
 * library-file suffixes. Microsoft ships `libonnxruntime.so.1.24.4` and
 * symlinks the bare `libonnxruntime.so` at it; on macOS the pattern is
 * `libonnxruntime.1.24.4.dylib`. Returns null only when no version-shaped
 * suffix is present (treat as compatible to avoid false negatives on
 * unconventional installs). Malformed version-shaped suffixes return an invalid
 * sentinel so the system install is rejected instead of silently accepted.
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
    // The file the bare name resolves to is the one the loader opens, so its
    // version wins. A directory scan alone can report a stale sibling: a
    // leftover `libonnxruntime.1.30.0.dylib` next to a link that points at a
    // 1.28.0 keg reported 1.30.0 for a runtime that was really 1.28.0.
    try {
      const resolved = parseOnnxVersionFromPath(realpathSync(join(libDir, libName)));
      if (resolved && resolved !== INVALID_ORT_VERSION) return resolved;
    } catch {
      // No bare library, or a dangling link; fall back to the directory scan.
    }
    const barePrefix = libName.replace(/\.(so|dylib|dll)$/, "");
    const expectedPrefix = process.platform === "win32" ? barePrefix.toLowerCase() : barePrefix;
    for (const entry of entries) {
      const comparable = process.platform === "win32" ? entry.toLowerCase() : entry;
      if (!comparable.startsWith(expectedPrefix)) continue;
      const version = parseOnnxVersionFromPath(entry);
      if (version) return version;
    }
    // Symlink fallback: bare libonnxruntime.so -> libonnxruntime.so.1.24.4
    const base = join(libDir, libName);
    if (existsSync(base)) {
      try {
        const real = realpathSync(base);
        const version = parseOnnxVersionFromPath(real) ?? parseOnnxVersionFromDirectoryPath(real);
        if (version) return version;
      } catch {
        // ignore
      }
      try {
        const target = readlinkSync(base);
        const version = parseOnnxVersionFromPath(target);
        if (version) return version;
      } catch {
        // not a symlink
      }
    }
    return parseOnnxVersionFromDirectoryPath(libDir);
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

function pathEnvValue(): string {
  const env = withPathPrepended(process.env);
  if (process.platform !== "win32") return env.PATH ?? "";
  const key = Object.keys(env).find((candidate) => candidate.toLowerCase() === "path");
  return key === undefined ? "" : (env[key] ?? "");
}

function pathEntriesForPlatform(): string[] {
  const delimiter = process.platform === "win32" ? ";" : ":";
  return pathEnvValue()
    .split(delimiter)
    .map((entry) => entry.trim().replace(/^"|"$/g, ""))
    .filter((entry) => {
      if (!entry || entry === "." || entry.includes("\0")) return false;
      return isAbsolute(entry) || win32.isAbsolute(entry);
    });
}

function isWindowsSystem32Directory(dir: string): boolean {
  if (process.platform !== "win32") return false;

  const normalizedDir = win32
    .resolve(dir)
    .replace(/[\\/]+$/, "")
    .toLowerCase();
  const windowsRoots = [process.env.SystemRoot, process.env.windir, "C:\\Windows"];
  return windowsRoots.some((root) => {
    if (!root) return false;
    return (
      normalizedDir ===
      win32
        .resolve(root, "System32")
        .replace(/[\\/]+$/, "")
        .toLowerCase()
    );
  });
}

function directoryContainsLibrary(dir: string, libName: string): boolean {
  try {
    const entries = readdirSync(dir);
    if (process.platform === "win32") {
      const expected = libName.toLowerCase();
      return entries.some((entry) => entry.toLowerCase() === expected);
    }
    return entries.includes(libName);
  } catch {
    return false;
  }
}

function resolveCachedOnnxRuntimeDir(ortVersionDir: string, libName: string): string {
  // AFT's own installer flattens the runtime libraries into the version root.
  // Microsoft's archives keep them under lib/. Prefer the root when both exist
  // because downloads and metadata are anchored there.
  //
  // MIRROR: the Rust standalone binary resolves the same layout in
  // `crates/aft/src/semantic_index.rs::find_managed_onnx_runtime` /
  // `managed_ort_lib_in_version_dir` (version root, then `lib/` subdir). A
  // layout change here must update that resolver too, and vice versa.
  if (existsSync(join(ortVersionDir, libName))) return ortVersionDir;
  const libSubdir = join(ortVersionDir, "lib");
  if (existsSync(join(libSubdir, libName))) return libSubdir;
  return ortVersionDir;
}

function findSystemOnnxRuntime(libName?: string, searchPathsOverride?: string[]): string | null {
  if (!libName) return null;

  const searchPaths: string[] = [...(searchPathsOverride ?? [])];

  if (searchPathsOverride) {
    // Tests supply the exact candidate list.
  } else if (process.platform === "darwin") {
    // Homebrew locations
    searchPaths.push("/opt/homebrew/lib", "/usr/local/lib");
  } else if (process.platform === "linux") {
    searchPaths.push(
      "/usr/lib",
      "/usr/lib/x86_64-linux-gnu",
      "/usr/lib/aarch64-linux-gnu",
      "/usr/local/lib",
    );
  } else if (process.platform === "win32") {
    // Common Windows install locations for ONNX Runtime
    const programFiles = process.env.ProgramFiles ?? "C:\\Program Files";
    const programFilesX86 = process.env["ProgramFiles(x86)"] ?? "C:\\Program Files (x86)";
    searchPaths.push(
      join(programFiles, "onnxruntime", "lib"),
      join(programFiles, "Microsoft ONNX Runtime", "lib"),
      join(programFiles, "Microsoft Machine Learning", "lib"),
      join(programFilesX86, "onnxruntime", "lib"),
      // Windows NuGet package layout:
      //   <user>\.nuget\packages\microsoft.ml.onnxruntime\<version>\runtimes\win-{x64,arm64}\native\
      // Scan all installed versions since we don't know which one is present.
      ...(() => {
        const nugetPaths: string[] = [];
        const userProfile = process.env.USERPROFILE ?? "";
        if (!userProfile) return nugetPaths;
        const nugetPackageDir = join(userProfile, ".nuget", "packages", "microsoft.ml.onnxruntime");
        if (!existsSync(nugetPackageDir)) return nugetPaths;
        try {
          for (const entry of readdirSync(nugetPackageDir, { withFileTypes: true })) {
            if (!entry.isDirectory()) continue;
            // Skip well-known non-version entries
            if (entry.name === "__globalPackagesFolder" || entry.name.startsWith(".")) continue;
            nugetPaths.push(
              join(nugetPackageDir, entry.name, "runtimes", "win-x64", "native"),
              join(nugetPackageDir, entry.name, "runtimes", "win-arm64", "native"),
            );
          }
        } catch (err) {
          warn(
            `Failed to scan NuGet ONNX Runtime cache ${nugetPackageDir}: ${err instanceof Error ? err.message : String(err)}`,
          );
        }
        return nugetPaths;
      })(),
    );
    // Also include absolute PATH entries (reuses the existing helper that
    // validates absolute paths, rejects null bytes, strips quotes, excludes ".").
    searchPaths.push(...pathEntriesForPlatform());
  }

  // Deduplicate paths while preserving order.
  // On case-insensitive filesystems (Windows, macOS) normalize casing for
  // comparison; on Linux the raw path casing is the authority.
  const normalizeCase = process.platform === "win32" || process.platform === "darwin";
  const seen = new Set<string>();
  const uniquePaths = searchPaths.filter((p) => {
    let key = resolve(p).replace(/[/\\]+$/, "");
    if (normalizeCase) key = key.toLowerCase();
    if (seen.has(key)) return false;
    seen.add(key);
    return true;
  });

  const unknownVersionPaths: string[] = [];

  for (const dir of uniquePaths) {
    const libPath = join(dir, libName);
    if (process.platform === "win32") {
      if (!directoryContainsLibrary(dir, libName)) continue;
    } else if (!existsSync(libPath)) {
      continue;
    }

    // Reject system installs that the Rust pre-validator will refuse. Without
    // this filter, a stale distro package (e.g. libonnxruntime1.9 on Ubuntu
    // 22.04) shadows our auto-downloaded v1.24 forever and semantic search
    // stays "failed" until the user hand-deletes the system library.
    //
    // Windows PATH/NuGet installs often expose only `onnxruntime.dll`, so we
    // also mine version-bearing parent directories and symlink targets. When a
    // version is unknown, keep it as a last-choice fallback rather than letting
    // it shadow a later candidate with a known compatible version.
    // A compatible version number does not mean the library loads: a Homebrew
    // runtime whose abseil dependency was upgraded away fails dlopen while its
    // file name still reads 1.2x. Skip anything whose required dependencies
    // are missing so AFT's own runtime is used instead. PE imports cannot be
    // checked by reading the file, so Windows candidates are not probed.
    if (process.platform !== "win32") {
      const probe = probeOnnxRuntimeLoadable(libPath);
      if (!probe.loadable) {
        warn(
          `Ignoring system ONNX Runtime at ${dir}: ${probe.reason}. Falling through to AFT-managed download.`,
        );
        continue;
      }
    }

    const version = detectOnnxVersion(dir, libName);
    if (!version) {
      // Windows ships an unversioned ONNX Runtime in System32 on some releases.
      // Rust can discover that DLL is too old only after startup, so never let
      // an unverifiable OS copy prevent AFT from downloading its managed runtime.
      if (isWindowsSystem32Directory(dir)) {
        warn(
          `Skipping unversioned Windows system ONNX Runtime at ${dir}; falling through to AFT-managed download.`,
        );
        continue;
      }
      unknownVersionPaths.push(dir);
      continue;
    }
    if (!isOnnxVersionCompatible(version)) {
      warn(
        `Skipping system ONNX Runtime at ${dir} (v${version}); AFT requires ` +
          `v${REQUIRED_ORT_MAJOR}.${REQUIRED_ORT_MIN_MINOR}+. Falling through to AFT-managed download.`,
      );
      continue;
    }

    return dir;
  }

  return unknownVersionPaths[0] ?? null;
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
 * zip-slip + symlink containment + decompression-bomb
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
        // A target inside realRoot yields a relative path. If `relative()`
        // returns an absolute path it escaped the root — on POSIX (`/...`) or
        // across Windows drives (`D:\...`, where the win32 check is essential).
        if (rel.startsWith("..") || isAbsolute(rel) || win32.isAbsolute(rel)) {
          throw new Error(
            `extracted symlink ${fullPath} points outside staging root: ${linkTarget}`,
          );
        }
        continue;
      }

      const rel = relative(realRoot, fullPath);
      if (rel.startsWith("..") || isAbsolute(rel) || win32.isAbsolute(rel)) {
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

  // Keep every unverified byte under the version-scoped staging directory.
  // The managed target is not touched until extraction and library validation
  // have completed, so a failed repair cannot erase a working older runtime.
  // The pid stays the second-to-last dot segment so the abandoned-attempt
  // sweep can tell whether the owner is alive. The last segment adds random
  // bytes so two attempts in one process (separate module instances loaded in
  // the same millisecond) can never share a staging dir.
  const tmpDir = `${targetDir}.tmp.${process.pid}.${Date.now().toString(36)}${randomBytes(4).toString("hex")}`;
  const extractionRoot = join(tmpDir, "extract");
  const stagedInstallDir = join(tmpDir, "install");

  try {
    mkdirSync(extractionRoot, { recursive: true });
    const archivePath = join(tmpDir, `onnxruntime.${info.archiveType}`);

    // Download with a streaming size cap.
    await downloadFileWithCap(url, archivePath);

    // Hash the archive for TOFU.
    const archiveSha256 = sha256File(archivePath);
    log(`ONNX Runtime archive sha256=${archiveSha256}`);

    if (info.archiveType === "tgz") {
      execTarExtractionSync(["xzf", archivePath, "-C", extractionRoot], 120_000);
    } else {
      await extractZipArchive(archivePath, extractionRoot);
    }

    // Drop the archive itself before validation so it doesn't double-count
    // toward the extracted-size budget.
    try {
      unlinkSync(archivePath);
    } catch {
      // ignore
    }

    // Containment + size-bomb check.
    validateExtractedTree(extractionRoot);

    // Find and copy the library file.
    const extractedDir = join(extractionRoot, info.assetName, "lib");
    if (!existsSync(extractedDir)) {
      throw new Error(`Expected directory not found: ${extractedDir}`);
    }

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

    copyOnnxLibraries(info, extractedDir, stagedInstallDir, realFiles, symlinks);

    // Persist the installed version, archive SHA-256, and main-library hash so
    // future sessions can verify the runtime against the first installation.
    // Hash the actual main library because steady-state resolution checks it.
    const libPath = join(stagedInstallDir, info.libName);
    const libHash = sha256File(libPath);
    writeOnnxInstalledMeta(stagedInstallDir, ORT_VERSION, libHash, archiveSha256);

    publishOnnxRuntime(stagedInstallDir, targetDir);
    rmSync(tmpDir, { recursive: true, force: true });

    log(`ONNX Runtime v${ORT_VERSION} installed to ${targetDir}`);
    return targetDir;
  } catch (err) {
    error(`Failed to download ONNX Runtime: ${err}`);
    lastInstallFailure = `ONNX Runtime download failed: ${err instanceof Error ? err.message : String(err)}`;
    try {
      rmSync(tmpDir, { recursive: true, force: true });
    } catch {
      // ignore cleanup errors
    }
    return null;
  }
}

function publishOnnxRuntime(stagedInstallDir: string, targetDir: string): void {
  const backupDir = `${targetDir}.backup.${process.pid}.${Date.now().toString(36)}`;
  const hadTarget = existsSync(targetDir);

  if (hadTarget) renameSync(targetDir, backupDir);
  try {
    renameSync(stagedInstallDir, targetDir);
  } catch (installError) {
    if (hadTarget) {
      try {
        renameSync(backupDir, targetDir);
      } catch (restoreError) {
        throw new Error(
          `ONNX Runtime replacement failed and the previous runtime could not be restored from ${backupDir}: ${restoreError}`,
          { cause: installError },
        );
      }
    }
    throw installError;
  }

  if (hadTarget) {
    try {
      rmSync(backupDir, { recursive: true, force: true });
    } catch (err) {
      warn(`Could not remove replaced ONNX Runtime backup at ${backupDir}: ${err}`);
    }
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

  // The installer passes a fresh staging directory that does not exist yet.
  // Without this, every copy fails with ENOENT on the destination path and
  // no managed install can ever succeed.
  mkdirSync(targetDir, { recursive: true });

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
  const targetRoot = realpathSync(targetDir);
  for (const link of symlinks) {
    const dst = join(targetDir, link.name);
    try {
      unlinkSync(dst); // remove if exists from a previous partial install
    } catch {
      // ignore
    }

    const dstForContainment = join(targetRoot, link.name);
    const resolvedTarget = resolve(dirname(dstForContainment), link.target);
    if (!isPathInsideRoot(targetRoot, resolvedTarget)) {
      const message = `ONNX Runtime symlink ${link.name} points outside install dir: ${link.target}`;
      if (requiredLibs.has(link.name)) {
        rmSync(targetDir, { recursive: true, force: true });
        throw new Error(message);
      }
      log(`ORT extract: skipping optional symlink ${link.name}: ${message}`);
      continue;
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
    // Avoid PowerShell and PATH-resolved GNU tar. System32 bsdtar accepts
    // drive-letter paths and direct argv execution adds no shell parser.
    execTarExtractionSync(["-xf", archivePath, "-C", destinationDir], 120_000);
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
  const ownerAlive = owningPid !== null && isProcessAlive(owningPid);
  if (ownerAlive && ageWithinFresh) {
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

function tasklistPidFromCsvLine(line: string): string | null {
  const quoted = line.match(/"([^"]*)"/g);
  if (quoted && quoted.length >= 2) return quoted[1].slice(1, -1);
  const cells = line.split(",").map((cell) => cell.trim().replace(/^"|"$/g, ""));
  return cells[1] ?? null;
}

function isWindowsProcessAlive(pid: number): boolean {
  if (!Number.isInteger(pid) || pid <= 0) return false;
  try {
    const output = execFileSync("tasklist.exe", ["/FI", `PID eq ${pid}`, "/FO", "CSV", "/NH"], {
      encoding: "utf8",
      timeout: 2_000,
      windowsHide: true,
    });
    const expected = String(pid);
    return output.split(/\r?\n/).some((line) => tasklistPidFromCsvLine(line) === expected);
  } catch {
    return false;
  }
}

function isProcessAlive(pid: number): boolean {
  if (process.platform === "win32") return isWindowsProcessAlive(pid);
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
  resolveCachedOnnxRuntimeDir,
  ORT_VERSION,
  ONNX_INSTALLED_META_FILE,
  detectOnnxVersion,
  parseOnnxVersionFromPath,
  isOnnxVersionCompatible,
  findSystemOnnxRuntime,
  resolveOnnxRuntime,
  acquireLock,
  releaseLock,
  REQUIRED_ORT_MAJOR,
  REQUIRED_ORT_MIN_MINOR,
};
