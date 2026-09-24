import {
  chmodSync,
  closeSync,
  copyFileSync,
  existsSync,
  mkdirSync,
  openSync,
  readFileSync,
  readSync,
  renameSync,
  unlinkSync,
} from "node:fs";
import { createRequire } from "node:module";
import { homedir } from "node:os";
import { delimiter, dirname, isAbsolute, join } from "node:path";
import { log, warn } from "./active-logger.js";
import {
  isTrustedCachedBinary,
  recordBinaryIdentity,
  removeBinaryIdentitySidecar,
} from "./binary-identity.js";
import { getAftBinaryCacheDir } from "./cache-paths.js";
import { ensureBinary, readBinaryVersion } from "./downloader.js";
import { PLATFORM_ARCH_MAP } from "./platform.js";
import { readBinaryVersionOffThread } from "./version-probe.js";

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
 * `version` comes from the platform package's own package.json, so nothing is
 * executed. An existing cache entry is reused only when its identity sidecar
 * vouches for it; otherwise it is replaced from the npm package, which is
 * cheaper than proving the old file's version by running it. The new entry's
 * sidecar needs a hash of the bytes, so it is recorded in the background.
 */
function copyToVersionedCache(npmBinaryPath: string, version: string): string | null {
  try {
    const tag = version.startsWith("v") ? version : `v${version}`;
    const cacheDir = getAftBinaryCacheDir();
    const versionedDir = join(cacheDir, tag);
    const ext = process.platform === "win32" ? ".exe" : "";
    const cachedPath = join(versionedDir, `aft${ext}`);

    if (existsSync(cachedPath) && isTrustedCachedBinary(cachedPath, version)) return cachedPath;

    // Copy to versioned cache
    mkdirSync(versionedDir, { recursive: true });
    const tmpPath = `${cachedPath}.${process.pid}.${Date.now()}.tmp`;
    copyFileSync(npmBinaryPath, tmpPath);
    if (process.platform !== "win32") {
      chmodSync(tmpPath, 0o755);
    }
    removeBinaryIdentitySidecar(cachedPath);
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
    void recordBinaryIdentity(cachedPath, version);
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

/**
 * Ask `binaryPath` for its version on a worker thread and accept it when it
 * reports `expectedVersion` (any version when none is expected). The host's
 * event loop keeps running while the binary is loaded.
 */
async function probeBinaryCandidateOffThread(
  binaryPath: string,
  source: string,
  expectedVersion?: string | null,
): Promise<string | null> {
  const actual = await readBinaryVersionOffThread(binaryPath);
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

/**
 * Every `aft` executable on PATH, in PATH order, found by checking each
 * directory in-process instead of forking `which aft` / `where aft`.
 * Relative PATH entries are ignored so a project directory can never plant a
 * binary that the resolver would pick up.
 */
function pathCandidates(env: ResolverEnv, ext: string): string[] {
  const rawPath = env.PATH ?? (process.platform === "win32" ? env.Path : undefined) ?? "";
  const seen = new Set<string>();
  const candidates: string[] = [];
  for (const dir of rawPath.split(delimiter)) {
    if (!dir || !isAbsolute(dir)) continue;
    const candidate = join(dir, `aft${ext}`);
    if (seen.has(candidate)) continue;
    seen.add(candidate);
    if (existsSync(candidate)) candidates.push(candidate);
  }
  return candidates;
}

/**
 * True when `binaryPath` begins with a recognized native-executable magic
 * number (Mach-O, ELF, or PE/`MZ`). False for script shims that start with a
 * shebang (`#!`) or anything else.
 *
 * This is the guard against a PATH lookup for `aft` resolving to
 * the `@cortexkit/aft` CLI's OWN node-script shim. The CLI publishes a `bin`
 * named `aft` (same name as the native binary), and npx prepends its
 * `node_modules/.bin` to PATH, so the lookup can resolve to that shim. Probing
 * it with `--version` re-enters the CLI, which looks `aft` up again, which
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

function explicitBinaryOverride(env: ResolverEnv): string | null {
  const explicitBinary = env.AFT_BINARY_PATH?.trim();
  if (!explicitBinary) return null;
  // Hermetic host probes provide the just-built binary explicitly. Treat a bad
  // override as an error instead of falling through to an operator cache or PATH.
  if (!existsSync(explicitBinary) || !isNativeExecutable(explicitBinary)) {
    throw new Error(`AFT_BINARY_PATH does not name a native executable: ${explicitBinary}`);
  }
  return explicitBinary;
}

/** The version this package ships with, used when the caller names none. */
function ownPackageVersion(): string | null {
  try {
    const req = createRequire(import.meta.url);
    return (req("../package.json") as { version: string }).version;
  } catch {
    return null;
  }
}

/**
 * Read the npm platform package's binary path and version. The version comes
 * from the package's package.json, which npm installs together with the
 * binary, so no exec is needed to learn it.
 */
function npmPlatformPackage(ext: string): { binaryPath: string; version: string } | null {
  try {
    const req = createRequire(import.meta.url);
    const manifestPath = req.resolve(`@cortexkit/aft-${platformKey()}/package.json`);
    const manifest = JSON.parse(readFileSync(manifestPath, "utf8")) as { version?: unknown };
    const binaryPath = join(dirname(manifestPath), "bin", `aft${ext}`);
    if (typeof manifest.version !== "string" || !existsSync(binaryPath)) return null;
    return { binaryPath, version: normalizeBareVersion(manifest.version) };
  } catch {
    // npm package not installed or resolution failed
    return null;
  }
}

/**
 * Locate the `aft` binary synchronously WITHOUT executing anything, so it is
 * safe on a plugin host's only JavaScript thread. Checks, in order:
 * 0. `AFT_BINARY_PATH` (must be a native executable; its version is checked
 *    by {@link findBinary} and by the bridge handshake, not here)
 * 1. The versioned cache (`~/.cache/aft/bin/v<version>/aft`), accepted only
 *    when its identity sidecar still matches the file (stat only)
 * 2. The npm platform package `@cortexkit/aft-<platform>`, whose version is
 *    read from its package.json; the binary is copied into the versioned cache
 *
 * PATH and `~/.cargo/bin` binaries are not considered here: their version is
 * unknown until they run. {@link findBinary} probes them on a worker thread.
 *
 * @param expectedVersion Optional version (without `v` prefix). Defaults to
 *   this package's own version.
 * @returns Absolute path to a binary whose identity is established, or null.
 */
export function findBinarySync(expectedVersion?: string): string | null {
  const resolution = findTrustedBinarySync(expectedVersion, { ...process.env });
  // Keep one durable, source-labeled line after every successful binary-resolution
  // path. Operators need to distinguish a stale cache hit from npm, PATH, cargo, or a download.
  if (resolution) logBinaryResolution(resolution);
  return resolution?.path ?? null;
}

function findTrustedBinarySync(
  expectedVersion: string | undefined,
  env: ResolverEnv,
): BinaryResolution | null {
  const ext = process.platform === "win32" ? ".exe" : "";

  const explicitBinary = explicitBinaryOverride(env);
  if (explicitBinary) return { path: explicitBinary, source: "AFT_BINARY_PATH" };

  const pluginVersion = expectedVersion ?? ownPackageVersion();

  // 1. Versioned cache, vouched for by its identity sidecar.
  if (pluginVersion) {
    const tag = pluginVersion.startsWith("v") ? pluginVersion : `v${pluginVersion}`;
    const cached = cachedBinaryPathFromEnv(tag, env, ext);
    if (cached && isTrustedCachedBinary(cached, pluginVersion)) {
      return { path: cached, source: "versioned cache" };
    }
  }

  // 2. npm platform package — copy to versioned cache to avoid corruption
  // when npm updates the package while a bridge is running.
  //
  // IMPORTANT: when `pluginVersion` is known, REJECT npm packages whose
  // version does not match. A workspace with bun-cached older versions of
  // `@cortexkit/aft-<platform>` (e.g. v0.19.5 left over after upgrading the
  // plugin to v0.22.x) can otherwise hijack resolution and produce stale
  // task-id slugs / outdated protocol behavior.
  const npm = npmPlatformPackage(ext);
  if (npm) {
    if (pluginVersion && npm.version !== normalizeBareVersion(pluginVersion)) {
      warn(
        `npm platform package binary v${npm.version} does not match plugin v${pluginVersion}; skipping`,
      );
    } else {
      const copied = copyToVersionedCache(npm.binaryPath, npm.version);
      return { path: copied ?? npm.binaryPath, source: "npm platform package" };
    }
  }

  return null;
}

/**
 * Candidates whose version can only be learned by running them, probed on a
 * worker thread in resolution order: a versioned-cache entry without a
 * matching identity sidecar (for example one written before sidecars
 * existed), every `aft` on PATH, then `~/.cargo/bin/aft`.
 */
async function findProbedBinary(
  expectedVersion: string | undefined,
  env: ResolverEnv,
): Promise<BinaryResolution | null> {
  const ext = process.platform === "win32" ? ".exe" : "";
  const pluginVersion = expectedVersion ?? ownPackageVersion();

  // 1. An unvouched versioned-cache entry. Verified by running it off the
  // host thread; once confirmed, a sidecar is recorded in the background so
  // the next lookup is stat-only.
  if (pluginVersion) {
    const tag = pluginVersion.startsWith("v") ? pluginVersion : `v${pluginVersion}`;
    const cached = cachedBinaryPathFromEnv(tag, env, ext);
    if (cached) {
      const usable = await probeBinaryCandidateOffThread(cached, "Cached", pluginVersion);
      if (usable) {
        void recordBinaryIdentity(usable, pluginVersion);
        return { path: usable, source: "versioned cache" };
      }
    }
  }

  // 2. PATH
  for (const candidate of pathCandidates(env, ext)) {
    // Guard against self-resolution: a PATH hit can be the @cortexkit/aft
    // CLI's own node-script shim (npx prepends its node_modules/.bin to
    // PATH). Probing it with --version re-enters the CLI and fork-bombs.
    // Only accept native executables here.
    if (!isNativeExecutable(candidate)) {
      warn(`PATH binary at ${candidate} is not a native executable (script shim?); skipping`);
      continue;
    }
    const usable = await probeBinaryCandidateOffThread(candidate, "PATH", expectedVersion);
    if (usable) return { path: usable, source: "PATH" };
  }

  // 3. ~/.cargo/bin/aft
  const cargoPath = join(homeDirFromEnv(env), ".cargo", "bin", `aft${ext}`);
  if (existsSync(cargoPath)) {
    const usable = await probeBinaryCandidateOffThread(cargoPath, "cargo", expectedVersion);
    if (usable) return { path: usable, source: "cargo" };
  }

  return null;
}

export const __test__ = {
  pathCandidates,
};

/**
 * Locate the `aft` binary, with auto-download as a last resort. Never
 * executes a binary on the calling thread: identities come from sidecars and
 * package manifests, and anything that has to be run to learn its version is
 * run on a worker thread while the caller awaits.
 *
 * Resolution order:
 *   0. Explicit AFT_BINARY_PATH (hermetic host probes; version verified)
 *   1. Versioned cache (~/.cache/aft/bin/) with a matching identity sidecar
 *   2. npm platform package (@cortexkit/aft-<platform>)
 *   3. Versioned cache entry without a sidecar, verified by running it
 *   4. PATH lookup
 *   5. ~/.cargo/bin/aft
 *   6. Auto-download from GitHub releases
 *
 * Returns the absolute path to the binary.
 * Throws a descriptive error with install instructions if all sources fail.
 */
export async function findBinary(expectedVersion?: string): Promise<string> {
  const env = { ...process.env };

  const explicitBinary = explicitBinaryOverride(env);
  if (explicitBinary) {
    const usable = await probeBinaryCandidateOffThread(
      explicitBinary,
      "AFT_BINARY_PATH",
      expectedVersion,
    );
    if (!usable) {
      throw new Error(
        `AFT_BINARY_PATH is incompatible with the requested AFT version: ${explicitBinary}`,
      );
    }
    logBinaryResolution({ path: usable, source: "AFT_BINARY_PATH" });
    return usable;
  }

  const resolution =
    findTrustedBinarySync(expectedVersion, env) ?? (await findProbedBinary(expectedVersion, env));
  if (resolution) {
    logBinaryResolution(resolution);
    return resolution.path;
  }

  // 6. Auto-download from GitHub releases
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
      "  - PATH lookup",
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
