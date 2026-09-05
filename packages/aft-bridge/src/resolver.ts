import { execSync } from "node:child_process";
import {
  chmodSync,
  closeSync,
  copyFileSync,
  existsSync,
  mkdirSync,
  openSync,
  readSync,
  renameSync,
  unlinkSync,
} from "node:fs";
import { createRequire } from "node:module";
import { homedir } from "node:os";
import { join } from "node:path";
import { log, warn } from "./active-logger.js";
import { getAftBinaryCacheDir } from "./cache-paths.js";
import { ensureBinary, readBinaryVersion } from "./downloader.js";
import { PLATFORM_ARCH_MAP } from "./platform.js";

type EnsureBinary = typeof ensureBinary;

let ensureBinaryForResolver: EnsureBinary = ensureBinary;

export function __setEnsureBinaryForTests(impl: EnsureBinary | null): void {
  ensureBinaryForResolver = impl ?? ensureBinary;
}

type ResolverEnv = typeof process.env;

export { readBinaryVersion };

/**
 * Copy an npm platform binary to the versioned cache so we never run from
 * node_modules directly. This prevents corruption when npm updates the
 * package while a bridge process is running the binary.
 *
 * @param npmBinaryPath Absolute path to the npm-installed `aft` binary.
 * @param knownVersion Optional pre-resolved version string to skip the extra
 *   `--version` invocation (the caller often has it already).
 */
function copyToVersionedCache(npmBinaryPath: string, knownVersion?: string): string | null {
  try {
    const version = knownVersion ?? readBinaryVersion(npmBinaryPath);
    if (!version) return null;
    const tag = version.startsWith("v") ? version : `v${version}`;
    const cacheDir = getAftBinaryCacheDir();
    const versionedDir = join(cacheDir, tag);
    const ext = process.platform === "win32" ? ".exe" : "";
    const cachedPath = join(versionedDir, `aft${ext}`);

    // Already cached. Probe before trusting the directory label; stale or
    // corrupted cache entries must not shadow PATH/cargo fallback forever.
    if (existsSync(cachedPath)) {
      const cachedVersion = readBinaryVersion(cachedPath);
      if (cachedVersion === version) return cachedPath;
      warn(
        `Cached binary at ${cachedPath} reports ${cachedVersion ?? "no version"}, expected ${version}; refreshing from npm package`,
      );
    }

    // Copy to versioned cache
    mkdirSync(versionedDir, { recursive: true });
    const tmpPath = `${cachedPath}.${process.pid}.${Date.now()}.tmp`;
    copyFileSync(npmBinaryPath, tmpPath);
    if (process.platform !== "win32") {
      chmodSync(tmpPath, 0o755);
    }
    // Best-effort replace — unlink first on Windows where renameSync fails if target exists
    if (process.platform === "win32" && existsSync(cachedPath)) {
      try {
        unlinkSync(cachedPath);
      } catch {
        // best-effort; renameSync will surface the error if unlink fails
      }
    }
    renameSync(tmpPath, cachedPath);
    log(`Copied npm binary to versioned cache: ${cachedPath}`);
    return cachedPath;
  } catch (err) {
    warn(`Failed to copy binary to cache: ${err instanceof Error ? err.message : String(err)}`);
    return null;
  }
}

function normalizeBareVersion(version: string): string {
  return version.startsWith("v") ? version.slice(1) : version;
}

function homeDirFromEnv(env: ResolverEnv): string {
  return (process.platform === "win32" ? env.USERPROFILE || env.HOME : env.HOME) || homedir();
}

function cachedBinaryPathFromEnv(version: string, env: ResolverEnv, ext: string): string | null {
  const binaryPath = join(getAftBinaryCacheDir(env), version, `aft${ext}`);
  return existsSync(binaryPath) ? binaryPath : null;
}

function isExpectedCachedBinary(binaryPath: string, expectedVersion: string): boolean {
  const expected = normalizeBareVersion(expectedVersion);
  const actual = readBinaryVersion(binaryPath);
  if (actual === expected) return true;
  warn(
    `Cached binary at ${binaryPath} reports ${actual ?? "no version"}, expected ${expected}; skipping cache candidate`,
  );
  return false;
}

function probeBinaryCandidate(
  binaryPath: string,
  source: string,
  expectedVersion?: string,
): string | null {
  const actual = readBinaryVersion(binaryPath);
  if (actual === null) {
    warn(`${source} binary at ${binaryPath} did not report a version; skipping`);
    return null;
  }
  if (expectedVersion && actual !== normalizeBareVersion(expectedVersion)) {
    warn(
      `${source} binary at ${binaryPath} reports ${actual}, expected ${normalizeBareVersion(expectedVersion)}; skipping`,
    );
    return null;
  }
  return binaryPath;
}

function parsePathLookupOutput(output: string): string[] {
  return output
    .split(/\r?\n/)
    .map((candidate) => candidate.trim())
    .filter(Boolean);
}

/**
 * True when `binaryPath` begins with a recognized native-executable magic
 * number (Mach-O, ELF, or PE/`MZ`). False for script shims that start with a
 * shebang (`#!`) or anything else.
 *
 * This is the guard against a `which aft` / `where aft` PATH lookup resolving to
 * the `@cortexkit/aft` CLI's OWN node-script shim. The CLI publishes a `bin`
 * named `aft` (same name as the native binary), and npx prepends its
 * `node_modules/.bin` to PATH, so `which aft` can resolve to that shim. Probing
 * it with `--version` re-enters the CLI, which probes `which aft` again, which
 * forks `opencode --version` / `pi --version` for its harness report — an
 * exponential fork bomb (issue: self-resolution recursion). Native binaries
 * never start with `#!`, so a magic-number check rejects the shim regardless of
 * where it lives (npx cache, global `npm i -g`, etc.).
 */
export function isNativeExecutable(binaryPath: string): boolean {
  let fd: number | null = null;
  try {
    fd = openSync(binaryPath, "r");
    const buf = Buffer.alloc(4);
    const read = readSync(fd, buf, 0, 4, 0);
    if (read < 2) return false;
    const b0 = buf[0];
    const b1 = buf[1];
    // Shebang script shim (`#!`) — the recursion vector. Reject outright.
    if (b0 === 0x23 && b1 === 0x21) return false;
    const m32 = buf.readUInt32BE(0);
    // Mach-O: feedface/feedfacf (BE) + cefaedfe/cffaedfe (LE) + cafebabe (fat).
    const machO = new Set([0xfeedface, 0xfeedfacf, 0xcefaedfe, 0xcffaedfe, 0xcafebabe]);
    if (read >= 4 && machO.has(m32)) return true;
    // ELF: 0x7f 'E' 'L' 'F'.
    if (read >= 4 && m32 === 0x7f454c46) return true;
    // PE (Windows .exe): 'MZ'.
    if (b0 === 0x4d && b1 === 0x5a) return true;
    return false;
  } catch {
    // If we can't read it, don't trust it as a native binary.
    return false;
  } finally {
    if (fd !== null) {
      try {
        closeSync(fd);
      } catch {
        // best-effort
      }
    }
  }
}

/**
 * Map the current `process.platform` and `process.arch` to the npm platform
 * package suffix (e.g. `"darwin-arm64"`, `"linux-x64"`).
 *
 * Exported for testability — agents and scripts can call this directly to
 * verify the platform mapping without running the full resolver.
 *
 * @throws {Error} with the exact `process.platform` and `process.arch` values
 *   when the combination is unsupported.
 */
export function platformKey(
  platform: string = process.platform,
  arch: string = process.arch,
): string {
  const archMap = PLATFORM_ARCH_MAP[platform];
  if (!archMap) {
    throw new Error(
      `Unsupported platform: ${platform} (arch: ${arch}). ` +
        `Supported platforms: ${Object.keys(PLATFORM_ARCH_MAP).join(", ")}`,
    );
  }
  const key = archMap[arch];
  if (!key) {
    throw new Error(
      `Unsupported architecture: ${arch} on platform ${platform}. ` +
        `Supported architectures for ${platform}: ${Object.keys(archMap).join(", ")}`,
    );
  }
  return key;
}

type BinaryResolutionSource =
  | "AFT_BINARY_PATH"
  | "versioned cache"
  | "npm platform package"
  | "PATH"
  | "cargo"
  | "auto-download";

type BinaryResolution = {
  path: string;
  source: BinaryResolutionSource;
};

function logBinaryResolution(resolution: BinaryResolution): void {
  log(`Resolved binary from ${resolution.source}: ${resolution.path}`);
}

/**
 * Locate the `aft` binary synchronously by checking (in order):
 * 1. Cached binary from previous auto-download (~/.cache/aft/bin/)
 * 2. npm platform package via `require.resolve(@cortexkit/aft-<platform>/bin/aft)`
 * 3. PATH lookup via `which aft` (or `where aft` on Windows)
 * 4. ~/.cargo/bin/aft (Rust cargo install location)
 *
 * @param expectedVersion Optional version (without `v` prefix) — when set, the
 *   versioned cache for that version is checked first. Hosts that ship in
 *   lock-step with the binary should pass their own package version so a
 *   freshly downloaded binary is picked up before fallback resolution.
 * @returns Absolute path to the first binary found, or null if none found.
 */
export function findBinarySync(expectedVersion?: string): string | null {
  const resolution = findBinarySyncInner(expectedVersion);
  // Keep one durable, source-labeled line after every successful binary-resolution
  // path. Operators need to distinguish a stale cache hit from npm, PATH, cargo, or a download.
  if (resolution) logBinaryResolution(resolution);
  return resolution?.path ?? null;
}

function findBinarySyncInner(expectedVersion?: string): BinaryResolution | null {
  const ext = process.platform === "win32" ? ".exe" : "";
  const env = { ...process.env };

  // Hermetic host probes provide the just-built binary explicitly. Treat a bad
  // override as an error instead of falling through to an operator cache or PATH.
  const explicitBinary = env.AFT_BINARY_PATH?.trim();
  if (explicitBinary) {
    if (!existsSync(explicitBinary) || !isNativeExecutable(explicitBinary)) {
      throw new Error(`AFT_BINARY_PATH does not name a native executable: ${explicitBinary}`);
    }
    const usable = probeBinaryCandidate(explicitBinary, "AFT_BINARY_PATH", expectedVersion);
    if (!usable) {
      throw new Error(
        `AFT_BINARY_PATH is incompatible with the requested AFT version: ${explicitBinary}`,
      );
    }
    return { path: usable, source: "AFT_BINARY_PATH" };
  }

  // 1. Check versioned cache for the requested version (or this package's own
  // version as a fallback so direct callers without a host still benefit from
  // the cache).
  const pluginVersion =
    expectedVersion ??
    (() => {
      try {
        const req = createRequire(import.meta.url);
        return (req("../package.json") as { version: string }).version;
      } catch {
        return null;
      }
    })();
  if (pluginVersion) {
    const tag = pluginVersion.startsWith("v") ? pluginVersion : `v${pluginVersion}`;
    const versionCached = cachedBinaryPathFromEnv(tag, env, ext);
    if (versionCached && isExpectedCachedBinary(versionCached, pluginVersion)) {
      return { path: versionCached, source: "versioned cache" };
    }
  }

  // 2. Check npm platform package — copy to versioned cache to avoid
  // corruption when npm updates the package while a bridge is running.
  //
  // IMPORTANT: when `pluginVersion` is known, REJECT npm binaries whose
  // version does not match. A workspace with bun-cached older versions of
  // `@cortexkit/aft-<platform>` (e.g. v0.19.5 left over after upgrading the
  // plugin to v0.22.x) can otherwise hijack resolution and produce stale
  // task-id slugs / outdated protocol behavior. Skip to step 3 (PATH) so a
  // freshly built local binary can take over.
  try {
    const key = platformKey();
    const packageBin = `@cortexkit/aft-${key}/bin/aft${ext}`;
    const req = createRequire(import.meta.url);
    const resolved = req.resolve(packageBin);
    if (existsSync(resolved)) {
      const npmVersion = readBinaryVersion(resolved);
      if (npmVersion === null) {
        warn(
          `npm platform package binary at ${resolved} did not report a version; skipping (continuing to PATH lookup)`,
        );
      } else if (pluginVersion && npmVersion !== normalizeBareVersion(pluginVersion)) {
        warn(
          `npm platform package binary v${npmVersion} does not match plugin v${pluginVersion}; skipping (continuing to PATH lookup)`,
        );
      } else {
        const copied = copyToVersionedCache(resolved, npmVersion);
        return { path: copied ?? resolved, source: "npm platform package" };
      }
    }
  } catch {
    // npm package not installed or resolution failed
  }

  // 3. Check PATH
  try {
    const whichCmd = process.platform === "win32" ? "where aft" : "which aft";
    const result = execSync(whichCmd, {
      encoding: "utf-8",
      env,
      stdio: ["pipe", "pipe", "pipe"],
    }).trim();
    for (const candidate of parsePathLookupOutput(result)) {
      // Guard against self-resolution: a `which aft` hit can be the
      // @cortexkit/aft CLI's own node-script shim (npx prepends its
      // node_modules/.bin to PATH). Probing it with --version re-enters the CLI
      // and fork-bombs. Only accept native executables here.
      if (!isNativeExecutable(candidate)) {
        warn(`PATH binary at ${candidate} is not a native executable (script shim?); skipping`);
        continue;
      }
      const usable = probeBinaryCandidate(candidate, "PATH", expectedVersion);
      if (usable) return { path: usable, source: "PATH" };
    }
  } catch {
    // not in PATH
  }

  // 4. Check ~/.cargo/bin/aft
  const cargoPath = join(homeDirFromEnv(env), ".cargo", "bin", `aft${ext}`);
  if (existsSync(cargoPath)) {
    const usable = probeBinaryCandidate(cargoPath, "cargo", expectedVersion);
    if (usable) return { path: usable, source: "cargo" };
  }

  return null;
}

export const __test__ = {
  parsePathLookupOutput,
};

/**
 * Locate the `aft` binary, with auto-download as a last resort.
 *
 * Resolution order:
 *   0. Explicit AFT_BINARY_PATH (hermetic host probes)
 *   1. Cached binary (~/.cache/aft/bin/)
 *   2. npm platform package (@cortexkit/aft-<platform>)
 *   3. PATH lookup (which aft)
 *   4. ~/.cargo/bin/aft
 *   5. Auto-download from GitHub releases
 *
 * Returns the absolute path to the binary.
 * Throws a descriptive error with install instructions if all sources fail.
 */
export async function findBinary(expectedVersion?: string): Promise<string> {
  // Try synchronous resolution first (fast path)
  const syncResult = findBinarySync(expectedVersion);
  if (syncResult) {
    return syncResult;
  }

  // 5. Auto-download from GitHub releases
  log("Binary not found locally, attempting auto-download...");
  const downloaded = await ensureBinaryForResolver(expectedVersion);
  if (downloaded) {
    logBinaryResolution({ path: downloaded, source: "auto-download" });
    return downloaded;
  }

  // All sources exhausted
  throw new Error(
    [
      "Could not find the `aft` binary.",
      "",
      "Attempted sources:",
      "  - Cache directory (~/.cache/aft/bin/)",
      "  - npm platform package (@cortexkit/aft-<platform>)",
      "  - PATH lookup (which aft)",
      "  - ~/.cargo/bin/aft",
      "  - Auto-download from GitHub releases (failed)",
      "",
      "Install it using one of these methods:",
      "  npm install @cortexkit/aft-opencode        # installs platform-specific binary via npm",
      "  cargo install agent-file-tools             # from crates.io",
      "  cargo build --release         # from source (binary at target/release/aft)",
      "",
      "Or add the aft directory to your PATH.",
    ].join("\n"),
  );
}
