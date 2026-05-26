import { execSync, spawnSync } from "node:child_process";
import { chmodSync, copyFileSync, existsSync, mkdirSync, readdirSync, renameSync, unlinkSync } from "node:fs";
import { createRequire } from "node:module";
import { homedir } from "node:os";
import { join } from "node:path";
import { log, warn } from "./active-logger.js";
import { ensureBinary, getCacheDir, readBinaryVersion } from "./downloader.js";
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
    const cacheDir = getCacheDir();
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
    // Atomic rename — unlink first on Windows where renameSync fails if target exists
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

/**
 * Compare two semver strings. Returns >0 if a > b, <0 if a < b, 0 if equal.
 * Both versions must be bare (no leading "v").
 */
function compareSemver(a: string, b: string): number {
  const aParts = a.split(".").map(Number);
  const bParts = b.split(".").map(Number);
  for (let i = 0; i < 3; i++) {
    const av = aParts[i] ?? 0;
    const bv = bParts[i] ?? 0;
    if (av > bv) return 1;
    if (av < bv) return -1;
  }
  return 0;
}

function homeDirFromEnv(env: ResolverEnv): string {
  return (process.platform === "win32" ? env.USERPROFILE || env.HOME : env.HOME) || homedir();
}

function cacheDirFromEnv(env: ResolverEnv): string {
  if (process.platform === "win32") {
    const base = env.LOCALAPPDATA || env.APPDATA || join(homeDirFromEnv(env), "AppData", "Local");
    return join(base, "aft", "bin");
  }

  const base = env.XDG_CACHE_HOME || join(homeDirFromEnv(env), ".cache");
  return join(base, "aft", "bin");
}

function cachedBinaryPathFromEnv(version: string, env: ResolverEnv, ext: string): string | null {
  const binaryPath = join(cacheDirFromEnv(env), version, `aft${ext}`);
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
 * Scan the versioned cache directory for the highest available binary version.
 *
 * Iterates over `<cacheDir>/v*/aft[.exe]` entries, reads each binary's version,
 * and returns the one with the highest semantic version. This is the fallback
 * when the exact plugin version isn't cached but a newer version was downloaded
 * by `aft doctor --fix` or a previous `ensureBinary` call.
 *
 * @returns `{ path, version }` for the highest-versioned valid binary, or null.
 */
function findHighestCachedVersion(
  cacheDir: string,
  ext: string,
): { path: string; version: string } | null {
  try {
    const entries = readdirSync(cacheDir, { withFileTypes: true });
    let best: { path: string; version: string } | null = null;

    for (const entry of entries) {
      if (!entry.isDirectory()) continue;
      const tag = entry.name;
      if (!tag.startsWith("v")) continue;

      const binaryPath = join(cacheDir, tag, `aft${ext}`);
      if (!existsSync(binaryPath)) continue;

      const binaryVersion = readBinaryVersion(binaryPath);
      if (binaryVersion === null) continue;

      if (best === null) {
        best = { path: binaryPath, version: binaryVersion };
      } else {
        // Compare semver strings
        const current = best.version.split(".").map(Number);
        const candidate = binaryVersion.split(".").map(Number);
        let isHigher = false;
        for (let i = 0; i < 3; i++) {
          const a = current[i] ?? 0;
          const b = candidate[i] ?? 0;
          if (b > a) { isHigher = true; break; }
          if (b < a) break;
        }
        if (isHigher) {
          best = { path: binaryPath, version: binaryVersion };
        }
      }
    }

    return best;
  } catch {
    return null;
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

/**
 * Locate the `aft` binary synchronously by checking (in order):
 * 1. Cached binary from previous auto-download (~/.cache/aft/bin/)
 *    a. First, check for the **exact** requested version in cache.
 *    b. Then, scan ALL cached versions and return the highest one (handles
 *       the case where `aft doctor --fix` downloaded a newer version but the
 *       plugin still resolves by its own older version).
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
  const ext = process.platform === "win32" ? ".exe" : "";
  const env = { ...process.env };

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

  // 1. Check versioned cache — first exact version, then highest fallback.
  if (pluginVersion) {
    const tag = pluginVersion.startsWith("v") ? pluginVersion : `v${pluginVersion}`;

    // 1a. Exact version in cache.
    const exactCached = cachedBinaryPathFromEnv(tag, env, ext);
    if (exactCached && isExpectedCachedBinary(exactCached, pluginVersion)) return exactCached;

    // 1b. Scan ALL cached versions for the highest available one. This handles
    // the case where `aft doctor --fix` downloaded v0.30.3 but the plugin
    // resolves by its own older version (v0.29.1). Preferring the newer binary
    // is safe because the protocol is backwards compatible.
    const cacheDir = cacheDirFromEnv(env);
    const highestCached = findHighestCachedVersion(cacheDir, ext);
    if (highestCached) {
      const highestBare = normalizeBareVersion(highestCached.version);
      // Only use the highest cached version if it's actually newer than the
      // plugin version. A simple inequality check is not enough — the highest
      // version in the cache could still be lower than the plugin's version
      // (e.g. cache has v0.28.0 but plugin is v0.30.0). We never downgrade.
      const pluginBare = normalizeBareVersion(pluginVersion);
      if (compareSemver(highestBare, pluginBare) > 0) {
        log(
          `Using highest cached version ${highestCached.version} (plugin requested ${pluginVersion}) at ${highestCached.path}`,
        );
        return highestCached.path;
      }
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
        return copied ?? resolved;
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
      const usable = probeBinaryCandidate(candidate, "PATH", expectedVersion);
      if (usable) return usable;
    }
  } catch {
    // not in PATH
  }

  // 4. Check ~/.cargo/bin/aft
  const cargoPath = join(homeDirFromEnv(env), ".cargo", "bin", `aft${ext}`);
  if (existsSync(cargoPath)) {
    const usable = probeBinaryCandidate(cargoPath, "cargo", expectedVersion);
    if (usable) return usable;
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
 *   1. Cached binary (~/.cache/aft/bin/) — exact version, then highest available
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
    log(`Resolved binary: ${syncResult}`);
    return syncResult;
  }

  // 5. Auto-download from GitHub releases
  log("Binary not found locally, attempting auto-download...");
  const downloaded = await ensureBinaryForResolver(expectedVersion);
  if (downloaded) return downloaded;

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
