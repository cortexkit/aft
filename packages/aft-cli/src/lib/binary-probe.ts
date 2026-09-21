import { execSync, spawnSync } from "node:child_process";
import { existsSync, readdirSync } from "node:fs";
import { createRequire } from "node:module";
import { homedir } from "node:os";
import { join } from "node:path";
import { compareSemver, isNativeExecutable } from "@cortexkit/aft-bridge";
import { getAftBinaryCacheDir, getAftBinaryName } from "./paths.js";

async function loadPluginVersion(): Promise<string> {
  try {
    // Literal specifier so the CLI bundle inlines aft-bridge. A variable
    // specifier leaves a runtime import that resolves to the INSTALLED
    // aft-bridge package, whose dist pulls @cortexkit/subc-client — published
    // as TypeScript source, which Node (npx) refuses to load from
    // node_modules ("Stripping types is currently unsupported").
    const bridge = (await import("@cortexkit/aft-bridge")) as Record<string, unknown>;
    if (typeof bridge.PLUGIN_VERSION === "string" && bridge.PLUGIN_VERSION.length > 0) {
      return bridge.PLUGIN_VERSION;
    }
  } catch {
    // In source tests the workspace package may not have dist/ built yet.
  }

  const require = createRequire(import.meta.url);
  for (const relPath of [
    "../../../aft-bridge/package.json",
    "../../package.json",
    "../package.json",
  ]) {
    try {
      const pkg = require(relPath) as { version?: unknown };
      if (typeof pkg.version === "string" && pkg.version.length > 0) return pkg.version;
    } catch {
      // try next location
    }
  }

  return "unknown";
}

const PLUGIN_VERSION = await loadPluginVersion();

const VERSION_LINE = /^(?:aft\s+)?v?(\d+\.\d+\.\d+(?:[-+][0-9A-Za-z.-]+)?)$/i;

export type BinaryProbeCandidateStatus = "matched" | "unmatched" | "invalid" | "error";

export interface BinaryProbeCandidate {
  path: string;
  status: BinaryProbeCandidateStatus;
  version: string | null;
  output?: string;
  error?: string;
}

export interface BinaryProbeResult {
  version: string | null;
  path: string | null;
  expectedVersion: string;
  expectedMajorMinor: string | null;
  candidates: BinaryProbeCandidate[];
}

function parseVersionOutput(output: string): string | null {
  for (const line of output.split(/\r?\n/)) {
    const match = line.trim().match(VERSION_LINE);
    if (match?.[1]) return match[1];
  }
  return null;
}

function majorMinor(version: string | null | undefined): string | null {
  if (!version) return null;
  const match = version.trim().match(/^v?(\d+)\.(\d+)\.\d+(?:[-+][0-9A-Za-z.-]+)?$/);
  if (!match) return null;
  return `${match[1]}.${match[2]}`;
}

function versionMatchesExpected(candidate: string, expectedVersion: string): boolean {
  const candidateMajorMinor = majorMinor(candidate);
  const expectedMajorMinor = majorMinor(expectedVersion);
  return candidateMajorMinor !== null && candidateMajorMinor === expectedMajorMinor;
}

/**
 * Parse and validate `aft --version` output. Accepts either a plain semver
 * line (`0.30.1`) or the binary's normal `aft 0.30.1` line. Random non-semver
 * output is rejected so PATH garbage is not reported as a healthy AFT binary.
 */
export function normalizeBinaryVersion(output: string): string | null {
  return parseVersionOutput(output);
}

/**
 * Probe `aft --version` from the same prioritized candidate locations used by
 * `findAftBinary()` (cache, npm platform package, PATH, cargo fallback).
 *
 * Returns the first successfully reported version matching the expected
 * major.minor version, or null if nothing resolves. Errors, missing files,
 * invalid version output, and version mismatches are swallowed — callers get a
 * signal, not an exception.
 */
export function probeBinaryVersion(preferredVersion?: string): string | null {
  return probeAftBinary(preferredVersion).version;
}

/** Detailed binary probe used by diagnostics to explain mismatched candidates. */
export function probeAftBinary(preferredVersion?: string): BinaryProbeResult {
  const expectedVersion = preferredVersion ?? PLUGIN_VERSION;
  const expectedMajorMinor = majorMinor(expectedVersion);
  const candidates: BinaryProbeCandidate[] = [];

  for (const candidate of aftBinaryCandidates(preferredVersion)) {
    try {
      if (!existsSync(candidate)) continue;
      const result = spawnSync(candidate, ["--version"], {
        stdio: ["ignore", "pipe", "pipe"],
        encoding: "utf-8",
        timeout: 5_000,
        env: process.env,
      });
      const output = `${result.stdout ?? ""}\n${result.stderr ?? ""}`.trim();
      if (result.error || result.status !== 0) {
        candidates.push({
          path: candidate,
          status: "error",
          version: null,
          ...(output ? { output } : {}),
          error: result.error?.message ?? `exit status ${result.status ?? "unknown"}`,
        });
        continue;
      }

      const version = parseVersionOutput(output);
      if (!version) {
        candidates.push({ path: candidate, status: "invalid", version: null, output });
        continue;
      }

      if (!versionMatchesExpected(version, expectedVersion)) {
        candidates.push({ path: candidate, status: "unmatched", version, output });
        continue;
      }

      candidates.push({ path: candidate, status: "matched", version, output });
      return { version, path: candidate, expectedVersion, expectedMajorMinor, candidates };
    } catch (error) {
      candidates.push({
        path: candidate,
        status: "error",
        version: null,
        error: error instanceof Error ? error.message : String(error),
      });
    }
  }

  return { version: null, path: null, expectedVersion, expectedMajorMinor, candidates };
}

function pushCandidate(candidates: string[], candidate: string | null | undefined): void {
  if (!candidate) return;
  if (!candidates.includes(candidate)) candidates.push(candidate);
}

function firstExisting(candidates: string[]): string | null {
  for (const candidate of candidates) {
    try {
      if (!existsSync(candidate)) continue;
      return candidate;
    } catch {
      // try next
    }
  }
  return null;
}

export function platformKey(
  platform: string = process.platform,
  arch: string = process.arch,
): string | null {
  const table: Record<string, Record<string, string>> = {
    darwin: { arm64: "darwin-arm64", x64: "darwin-x64" },
    linux: { arm64: "linux-arm64", x64: "linux-x64" },
    win32: { x64: "win32-x64" },
  };
  return table[platform]?.[arch] ?? null;
}

/** The `v<semver>` directory names `aft doctor --fix` creates in the binary cache. */
const CACHE_VERSION_DIR = /^v(\d+\.\d+\.\d+(?:-[0-9A-Za-z.-]+)?)$/;

/**
 * Newest binary cached under `<cache>/v<semver>/<name>`.
 *
 * Versions are compared semantically, not lexically or by mtime: `v0.9.0` must
 * not beat `v0.10.0`, and a leftover `v0.0.0` directory must not win by sorting.
 * Only directories that actually hold the binary are considered, so an
 * interrupted download cannot shadow a complete older install.
 */
function newestCachedBinary(cacheDir: string, binaryName: string): string | null {
  let entries: string[];
  try {
    entries = readdirSync(cacheDir);
  } catch {
    // Cache directory absent or unreadable — nothing has been installed here.
    return null;
  }

  let best: { version: string; path: string } | null = null;
  for (const entry of entries) {
    const version = entry.match(CACHE_VERSION_DIR)?.[1];
    if (!version) continue;
    const path = join(cacheDir, entry, binaryName);
    try {
      if (!existsSync(path)) continue;
    } catch {
      continue;
    }
    if (!best || compareSemver(version, best.version) > 0) best = { version, path };
  }
  return best?.path ?? null;
}

function aftBinaryCandidates(preferredVersion?: string): string[] {
  const candidates: string[] = [];
  const cacheDir = getAftBinaryCacheDir();
  const binaryName = getAftBinaryName();
  if (preferredVersion) {
    const tag = preferredVersion.startsWith("v") ? preferredVersion : `v${preferredVersion}`;
    pushCandidate(candidates, join(cacheDir, tag, binaryName));
  } else {
    // The cache is where `aft doctor --fix` downloads to, so it is searched
    // even when the caller names no version. Skipping it meant that on a
    // machine with no other install source — no platform package, no `aft` on
    // PATH, no cargo install — a successful `doctor --fix` was invisible to
    // the very next command.
    pushCandidate(candidates, newestCachedBinary(cacheDir, binaryName));
  }

  const key = platformKey();
  if (key) {
    try {
      const require = createRequire(import.meta.url);
      pushCandidate(candidates, require.resolve(`@cortexkit/aft-${key}/bin/${getAftBinaryName()}`));
    } catch {
      // platform package is optional
    }
  }

  try {
    const lookup = process.platform === "win32" ? "where aft" : "which aft";
    const resolved = execSync(lookup, {
      stdio: "pipe",
      encoding: "utf-8",
      env: process.env,
    }).trim();
    // Guard against self-resolution recursion: `aft` on PATH may be THIS CLI's
    // own node-script shim (npx prepends node_modules/.bin to PATH, and the
    // CLI's bin is named `aft`). Probing it with --version re-enters the CLI and
    // fork-bombs. Only accept native executables. Iterate all lines so a real
    // native binary after a `.cmd`/script shim (Windows `where`) is still found.
    for (const line of resolved.split(/\r?\n/)) {
      const candidate = line.trim();
      if (candidate && isNativeExecutable(candidate)) {
        pushCandidate(candidates, candidate);
      }
    }
  } catch {
    // ignore — PATH lookup is best-effort
  }

  pushCandidate(candidates, join(homedir(), ".cargo", "bin", getAftBinaryName()));
  return candidates;
}

export function findAftBinary(preferredVersion?: string): string | null {
  return firstExisting(aftBinaryCandidates(preferredVersion));
}
