import { existsSync, readdirSync } from "node:fs";
import { createRequire } from "node:module";
import { homedir } from "node:os";
import { join } from "node:path";
import {
  compareSemver,
  findExecutablesOnPath,
  isNativeExecutable,
  spawnSync,
} from "@cortexkit/aft-bridge";
import { CLI } from "./cli.js";
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

/** One place a native binary can live, described well enough to report it. */
interface BinarySearchLocation {
  /** Short name of the location, for error messages. */
  label: string;
  /** Candidate binary paths this location offers, in probe order. */
  paths: string[];
  /** What to tell the reader when the location offers nothing. */
  emptyReason: string;
}

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

/**
 * The binary cache is where `aft doctor --fix` downloads to, so it is searched
 * even when the caller names no version — otherwise a freshly fixed machine
 * with no other install source still reports a missing binary.
 */
function cacheLocation(preferredVersion?: string): BinarySearchLocation {
  const cacheDir = getAftBinaryCacheDir();
  const binaryName = getAftBinaryName();
  const label = "binary cache";
  const installedHere = `\`${CLI} doctor --fix\` installs here`;

  if (preferredVersion) {
    const tag = preferredVersion.startsWith("v") ? preferredVersion : `v${preferredVersion}`;
    return {
      label,
      paths: [join(cacheDir, tag, binaryName)],
      emptyReason: `no ${tag}/${binaryName} under ${cacheDir} (${installedHere})`,
    };
  }

  const newest = newestCachedBinary(cacheDir, binaryName);
  return {
    label,
    paths: newest ? [newest] : [],
    emptyReason: `no v<version>/${binaryName} under ${cacheDir} (${installedHere})`,
  };
}

function platformPackageLocation(): BinarySearchLocation {
  const label = "npm platform package";
  const key = platformKey();
  if (!key) {
    return {
      label,
      paths: [],
      emptyReason: `no package published for ${process.platform}-${process.arch}`,
    };
  }

  const packageName = `@cortexkit/aft-${key}`;
  try {
    const require = createRequire(import.meta.url);
    return {
      label,
      paths: [require.resolve(`${packageName}/bin/${getAftBinaryName()}`)],
      emptyReason: `${packageName} not installed`,
    };
  } catch {
    // The platform package is optional; installs can come from anywhere else.
    return { label, paths: [], emptyReason: `${packageName} not installed` };
  }
}

function pathLocation(): BinarySearchLocation {
  const label = "PATH";
  // Searched in-process rather than through `which aft` / `where aft`, which
  // cost a child process (and a console window on Windows) per lookup.
  const hits = findExecutablesOnPath("aft");

  // Guard against self-resolution recursion: `aft` on PATH may be THIS CLI's
  // own node-script shim (npx prepends node_modules/.bin to PATH, and the
  // CLI's bin is named `aft`). Probing it with --version re-enters the CLI and
  // fork-bombs. Only accept native executables. Check every hit so a real
  // native binary after a `.cmd`/script shim on Windows is still found.
  const native = hits.filter((candidate) => isNativeExecutable(candidate));
  return {
    label,
    paths: native,
    emptyReason:
      hits.length > 0
        ? `only non-native \`aft\` entries on PATH (${hits.join(", ")}), skipped because running them would re-enter this CLI`
        : "no `aft` on PATH",
  };
}

function cargoLocation(): BinarySearchLocation {
  const path = join(homedir(), ".cargo", "bin", getAftBinaryName());
  return { label: "cargo install", paths: [path], emptyReason: `no ${path}` };
}

/** Every place a binary may live, in resolution order. */
function aftBinarySearchLocations(preferredVersion?: string): BinarySearchLocation[] {
  return [
    cacheLocation(preferredVersion),
    platformPackageLocation(),
    pathLocation(),
    cargoLocation(),
  ];
}

function aftBinaryCandidates(preferredVersion?: string): string[] {
  const candidates: string[] = [];
  for (const location of aftBinarySearchLocations(preferredVersion)) {
    for (const path of location.paths) pushCandidate(candidates, path);
  }
  return candidates;
}

/** One line per searched location: what was found there, or why nothing was. */
export function describeAftBinarySearch(preferredVersion?: string): string[] {
  return aftBinarySearchLocations(preferredVersion).map((location) => {
    const found = location.paths.filter((path) => {
      try {
        return existsSync(path);
      } catch {
        return false;
      }
    });
    return `${location.label}: ${found.length > 0 ? found.join(", ") : location.emptyReason}`;
  });
}

/**
 * Error text for a command that needs the native binary and found none.
 *
 * It lists every location searched instead of only naming a remedy. A reader
 * who just ran the suggested command successfully needs to know which location
 * we looked in and found empty; "run aft doctor" alone reads as "you did it
 * wrong" and sends them around the same loop again.
 */
export function missingAftBinaryMessage(command: string, preferredVersion?: string): string {
  return [
    `${command} requires a native AFT binary and none was found. Searched:`,
    ...describeAftBinarySearch(preferredVersion).map((line) => `  - ${line}`),
    `\`${CLI} doctor --fix\` installs into the binary cache directory named above; if it already reported success, that line is the directory this command searched — AFT_CACHE_DIR and XDG_CACHE_HOME change it.`,
  ].join("\n");
}

export function findAftBinary(preferredVersion?: string): string | null {
  return firstExisting(aftBinaryCandidates(preferredVersion));
}
