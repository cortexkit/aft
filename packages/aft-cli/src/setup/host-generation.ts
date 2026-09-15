import { spawnSync } from "node:child_process";
import { createHash } from "node:crypto";
import {
  accessSync,
  closeSync,
  constants,
  existsSync,
  mkdirSync,
  mkdtempSync,
  openSync,
  readdirSync,
  readFileSync,
  readSync,
  realpathSync,
  rmSync,
  type Stats,
  statSync,
} from "node:fs";
import { homedir, tmpdir } from "node:os";
import { delimiter, dirname, join, parse, resolve } from "node:path";

import { MODERN_V1_VERSION } from "./opencode-config.js";

export type OpenCodeHostGeneration = "v1" | "v2";
export type OpenCodeHostRuntime = "bun" | "node";

export interface OpenCodeHostEvidence {
  generation: OpenCodeHostGeneration;
  executable: string;
  version: string | null;
  runtime: OpenCodeHostRuntime;
  modernV1: boolean;
}

export interface OpenCodeHostDetection {
  status: OpenCodeHostGeneration | "ambiguous" | "unknown";
  generations: OpenCodeHostGeneration[];
  evidence: OpenCodeHostEvidence[];
}

interface PackageMetadata {
  name?: unknown;
  version?: unknown;
}

export interface HostGenerationDependencies {
  path?: string;
  platform?: NodeJS.Platform;
  findExecutable?: (name: "opencode" | "opencode2") => string | null;
  probeV1Version?: (executable: string) => string | null;
}

interface SpawnResult {
  status: number | null;
  stdout?: string | Buffer | null;
  error?: Error;
}

export interface V1VersionProbeOptions {
  env?: NodeJS.ProcessEnv;
  operatorHome?: string;
  tempParent?: string;
  spawn?: (
    executable: string,
    args: string[],
    options: {
      cwd: string;
      encoding: "utf8";
      env: NodeJS.ProcessEnv;
      stdio: ["ignore", "pipe", "pipe"];
      timeout: number;
    },
  ) => SpawnResult;
}

function executableNames(name: string, platform: NodeJS.Platform): string[] {
  if (platform !== "win32") return [name];
  return [name, `${name}.exe`, `${name}.cmd`, `${name}.bat`];
}

export function findExecutableOnPath(
  name: "opencode" | "opencode2",
  pathValue = process.env.PATH ?? "",
  platform: NodeJS.Platform = process.platform,
): string | null {
  const pathDelimiter = platform === "win32" ? ";" : delimiter;
  for (const directory of pathValue.split(pathDelimiter)) {
    if (!directory) continue;
    for (const candidateName of executableNames(name, platform)) {
      const candidate = resolve(directory, candidateName);
      try {
        accessSync(candidate, platform === "win32" ? constants.F_OK : constants.X_OK);
        if (statSync(candidate).isFile()) return candidate;
      } catch {
        // Keep looking through PATH.
      }
    }
  }
  return null;
}

function packageMetadataAt(path: string, expectedName: string): PackageMetadata | null {
  try {
    const metadata = JSON.parse(readFileSync(path, "utf8")) as PackageMetadata;
    return metadata.name === expectedName ? metadata : null;
  } catch {
    return null;
  }
}

function packageMetadataNearExecutable(
  executable: string,
  expectedName: string,
): PackageMetadata | null {
  let current: string;
  try {
    current = dirname(realpathSync(executable));
  } catch {
    current = dirname(executable);
  }

  while (true) {
    const direct = packageMetadataAt(join(current, "package.json"), expectedName);
    if (direct) return direct;
    const parent = dirname(current);
    if (parent === current || current === parse(current).root) return null;
    current = parent;
  }
}

function packageVersion(metadata: PackageMetadata | null): string | null {
  return typeof metadata?.version === "string" && metadata.version.length > 0
    ? metadata.version
    : null;
}

/**
 * Largest file whose full bytes are hashed. Beyond this the canary hashes the
 * head and tail plus size and mtime: a live `opencode.db` is routinely several
 * gigabytes (issue #316 reported 54 GiB, still 4 GiB after pruning), and
 * reading one whole costs more than the probe it guards — under Node it is not
 * possible at all past 2 GiB, which is how the crash reached users.
 */
const FULL_CONTENT_HASH_MAX_BYTES = 64 * 1024 * 1024;
/** Head and tail window hashed for a file over the full-content limit. */
const PARTIAL_CONTENT_WINDOW_BYTES = 1024 * 1024;

/**
 * Hash a file's bytes while holding at most one window in memory. Whole small
 * files are read window by window; a large file contributes its first and last
 * window, which is where SQLite's header page and newly appended pages live, so
 * a write during the probe still moves the digest.
 */
function hashFileBytes(hash: ReturnType<typeof createHash>, path: string, size: number): void {
  const buffer = Buffer.allocUnsafe(Math.min(Math.max(size, 1), PARTIAL_CONTENT_WINDOW_BYTES));
  const descriptor = openSync(path, "r");
  try {
    if (size <= FULL_CONTENT_HASH_MAX_BYTES) {
      let position = 0;
      while (position < size) {
        const read = readSync(descriptor, buffer, 0, buffer.length, position);
        if (read <= 0) break;
        hash.update(buffer.subarray(0, read));
        position += read;
      }
      return;
    }
    hash.update("partial\0");
    const head = readSync(descriptor, buffer, 0, buffer.length, 0);
    hash.update(buffer.subarray(0, Math.max(head, 0)));
    const tailStart = Math.max(size - PARTIAL_CONTENT_WINDOW_BYTES, 0);
    const tail = readSync(descriptor, buffer, 0, buffer.length, tailStart);
    hash.update(buffer.subarray(0, Math.max(tail, 0)));
  } finally {
    closeSync(descriptor);
  }
}

function snapshotTree(path: string): string {
  if (!existsSync(path)) return "missing";
  const hash = createHash("sha256");
  const visit = (current: string, relativePath: string): void => {
    let info: Stats;
    try {
      info = statSync(current);
    } catch (error) {
      // An entry that becomes unreadable between the two snapshots is itself a
      // change, so the failure is hashed rather than skipped.
      hash.update(`${relativePath}\0unreadable\0${(error as NodeJS.ErrnoException).code}\0`);
      return;
    }
    hash.update(`${relativePath}\0${info.mode}\0${info.size}\0${info.mtimeMs}\0`);
    if (info.isDirectory()) {
      for (const entry of readdirSync(current).sort()) {
        visit(join(current, entry), join(relativePath, entry));
      }
      return;
    }
    if (!info.isFile()) return;
    try {
      hashFileBytes(hash, current, info.size);
    } catch (error) {
      hash.update(`unreadable\0${(error as NodeJS.ErrnoException).code}\0`);
    }
  };
  visit(path, ".");
  return hash.digest("hex");
}

function snapshotOperatorState(operatorHome: string): string {
  const database = join(operatorHome, ".local", "share", "opencode", "opencode.db");
  return JSON.stringify({
    database: snapshotTree(database),
    // In WAL mode a write lands in the sidecar files first, so a canary that
    // watched only the main database could miss the very write it exists to
    // catch.
    databaseWal: snapshotTree(`${database}-wal`),
    databaseShm: snapshotTree(`${database}-shm`),
    logs: snapshotTree(join(operatorHome, ".local", "share", "opencode", "log")),
  });
}

/**
 * Run the V1 version probe in disposable host roots. The operator canary fails
 * the probe if the live OpenCode database or logs change while it runs.
 */
export function probeOpenCodeV1Version(
  executable: string,
  options: V1VersionProbeOptions = {},
): string | null {
  const operatorHome = options.operatorHome ?? homedir();
  const before = snapshotOperatorState(operatorHome);
  const root = mkdtempSync(join(options.tempParent ?? tmpdir(), "aft-opencode-probe-"));
  const home = join(root, "home");
  const config = join(root, "config");
  const data = join(root, "data");
  const state = join(root, "state");
  const cache = join(root, "cache");
  const project = join(root, "project");
  for (const directory of [home, config, data, state, cache, project]) {
    mkdirSync(directory, { recursive: true });
  }

  const env: NodeJS.ProcessEnv = {
    ...(options.env ?? process.env),
    HOME: home,
    XDG_CONFIG_HOME: config,
    XDG_DATA_HOME: data,
    XDG_STATE_HOME: state,
    XDG_CACHE_HOME: cache,
    TMPDIR: join(root, "tmp"),
  };
  mkdirSync(env.TMPDIR as string, { recursive: true });
  delete env.OPENCODE_CONFIG;
  delete env.OPENCODE_CONFIG_CONTENT;
  delete env.OPENCODE_CONFIG_DIR;
  delete env.OPENCODE_SERVER;

  let result: SpawnResult | undefined;
  let probeError: unknown;
  try {
    result = (options.spawn ?? spawnSync)(executable, ["--standalone", "--version"], {
      cwd: project,
      encoding: "utf8",
      env,
      stdio: ["ignore", "pipe", "pipe"],
      timeout: 5_000,
    });
  } catch (error) {
    probeError = error;
  }
  const after = snapshotOperatorState(operatorHome);
  rmSync(root, { recursive: true, force: true });
  if (after !== before) {
    throw new Error("OpenCode host probe changed the operator database or log directory");
  }
  if (probeError) throw probeError;

  if (!result || result.error || result.status !== 0) return null;
  const output = String(result.stdout ?? "").trim();
  return output.length > 0 ? output : null;
}

export function detectOpenCodeHostGeneration(
  dependencies: HostGenerationDependencies = {},
): OpenCodeHostDetection {
  const findExecutable =
    dependencies.findExecutable ??
    ((name: "opencode" | "opencode2") =>
      findExecutableOnPath(name, dependencies.path, dependencies.platform));
  const v1Executable = findExecutable("opencode");
  const v2Executable = findExecutable("opencode2");
  const evidence: OpenCodeHostEvidence[] = [];

  if (v1Executable) {
    const v1Metadata = packageMetadataNearExecutable(v1Executable, "opencode-ai");
    const v2Metadata = packageMetadataNearExecutable(v1Executable, "@opencode-ai/cli");
    const metadataVersion = packageVersion(v2Metadata) ?? packageVersion(v1Metadata);
    const version =
      metadataVersion ?? (dependencies.probeV1Version ?? probeOpenCodeV1Version)(v1Executable);
    const isV2 = Boolean(v2Metadata) || /^0\.0\.0-(?:beta|dev)-/.test(version ?? "");
    evidence.push({
      generation: isV2 ? "v2" : "v1",
      executable: v1Executable,
      version,
      runtime: "bun",
      modernV1: !isV2 && version === MODERN_V1_VERSION,
    });
  }

  if (v2Executable) {
    const version = packageVersion(packageMetadataNearExecutable(v2Executable, "@opencode-ai/cli"));
    evidence.push({
      generation: "v2",
      executable: v2Executable,
      version,
      runtime: "bun",
      modernV1: false,
    });
  }

  const generations = [
    ...new Set(evidence.map((item) => item.generation)),
  ].sort() as OpenCodeHostGeneration[];
  return {
    status:
      generations.length === 0
        ? "unknown"
        : generations.length === 1
          ? generations[0]
          : "ambiguous",
    generations,
    evidence,
  };
}

export function formatHostGenerations(detection: OpenCodeHostDetection): string {
  if (detection.status === "ambiguous") return "ambiguous (V1, V2)";
  if (detection.status === "v1") return "V1";
  if (detection.status === "v2") return "V2";
  return "unknown";
}
