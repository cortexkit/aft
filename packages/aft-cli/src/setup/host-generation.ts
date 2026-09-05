import { spawnSync } from "node:child_process";
import { createHash } from "node:crypto";
import {
  accessSync,
  constants,
  existsSync,
  mkdirSync,
  mkdtempSync,
  readdirSync,
  readFileSync,
  realpathSync,
  rmSync,
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

function snapshotTree(path: string): string {
  if (!existsSync(path)) return "missing";
  const hash = createHash("sha256");
  const visit = (current: string, relativePath: string): void => {
    const info = statSync(current);
    hash.update(`${relativePath}\0${info.mode}\0${info.size}\0`);
    if (info.isDirectory()) {
      for (const entry of readdirSync(current).sort()) {
        visit(join(current, entry), join(relativePath, entry));
      }
      return;
    }
    hash.update(readFileSync(current));
  };
  visit(path, ".");
  return hash.digest("hex");
}

function snapshotOperatorState(operatorHome: string): string {
  return JSON.stringify({
    database: snapshotTree(join(operatorHome, ".local", "share", "opencode", "opencode.db")),
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
