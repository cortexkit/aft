import { spawnSync } from "node:child_process";
import { accessSync, constants, existsSync, readFileSync, statSync } from "node:fs";
import { homedir } from "node:os";
import { delimiter, extname, join, resolve } from "node:path";

import { getOmpPackageDir } from "./omp-paths.js";

export interface OmpBinaryInfo {
  path: string;
  source: "path" | "home" | "package";
}

export interface OmpCommandResult {
  ok: boolean;
  stdout: string;
  stderr: string;
}

export interface OmpPluginInfo {
  name: string;
  version: string;
  enabled: boolean;
  path?: string;
}

export interface OmpCommandInvocation {
  command: string;
  args: string[];
}

export const OMP_HOST_PACKAGE = "@oh-my-pi/pi-coding-agent";
export const OMP_PLUGIN_PACKAGE = "@cortexkit/aft-pi";

function isExecutableFile(path: string, platform: NodeJS.Platform = process.platform): boolean {
  try {
    if (!existsSync(path) || !statSync(path).isFile()) return false;
    accessSync(path, platform === "win32" ? constants.F_OK : constants.X_OK);
    return true;
  } catch {
    return false;
  }
}

function executableNames(name: string, platform: NodeJS.Platform): string[] {
  return platform === "win32"
    ? [`${name}.exe`, `${name}.cmd`, `${name}.bat`, `${name}.com`]
    : [name];
}

function findOnPath(
  name: string,
  pathValue = process.env.PATH ?? "",
  platform: NodeJS.Platform = process.platform,
): string | null {
  const pathDelimiter = platform === "win32" ? ";" : delimiter;
  for (const directory of pathValue.split(pathDelimiter)) {
    if (!directory) continue;
    for (const candidateName of executableNames(name, platform)) {
      const candidate = resolve(directory, candidateName);
      if (isExecutableFile(candidate, platform)) return candidate;
    }
  }
  return null;
}

function detectOmpPackageCli(): string | null {
  const packageDir = getOmpPackageDir();
  if (!packageDir || !findOnPath("bun")) return null;
  try {
    const manifest = JSON.parse(readFileSync(join(packageDir, "package.json"), "utf8")) as {
      name?: unknown;
    };
    if (manifest.name !== OMP_HOST_PACKAGE) return null;
    const cli = join(packageDir, "dist", "cli.js");
    return existsSync(cli) ? cli : null;
  } catch {
    return null;
  }
}

/** Build argv for OMP, routing package scripts through Bun and Windows shims through cmd.exe. */
export function getOmpCommandInvocation(ompPath: string, args: string[]): OmpCommandInvocation {
  if (extname(ompPath).toLowerCase() === ".js") {
    const bun = findOnPath("bun");
    if (bun) return { command: bun, args: [ompPath, ...args] };
  }

  const extension = extname(ompPath).toLowerCase();
  if (extension === ".cmd" || extension === ".bat") {
    const command = process.env.ComSpec?.trim() || process.env.COMSPEC?.trim() || "cmd.exe";
    return { command, args: ["/d", "/s", "/c", ompPath, ...args] };
  }
  return { command: ompPath, args };
}

export function getOmpFallbackCandidates(
  platform: NodeJS.Platform,
  home: string,
  appData?: string,
): string[] {
  if (platform !== "win32") {
    return [join(home, ".bun", "bin", "omp"), join(home, ".local", "bin", "omp")];
  }
  const npmRoot = appData?.trim();
  return [
    ...(npmRoot ? [join(npmRoot, "npm", "omp.cmd"), join(npmRoot, "npm", "omp.exe")] : []),
    join(home, ".bun", "bin", "omp.exe"),
    join(home, ".bun", "bin", "omp.cmd"),
  ];
}

export function detectOmpBinary(): OmpBinaryInfo | null {
  const fromPath = findOnPath("omp");
  if (fromPath) return { path: fromPath, source: "path" };

  const fromPackage = detectOmpPackageCli();
  if (fromPackage) return { path: fromPackage, source: "package" };

  const home = process.env.HOME?.trim() || homedir();
  const candidate = getOmpFallbackCandidates(process.platform, home, process.env.APPDATA).find(
    (path) => isExecutableFile(path),
  );
  return candidate ? { path: candidate, source: "home" } : null;
}

export interface OmpCommandExecutionDeps {
  getInvocation: typeof getOmpCommandInvocation;
  spawnSync: typeof spawnSync;
}

const DEFAULT_COMMAND_EXECUTION_DEPS: OmpCommandExecutionDeps = {
  getInvocation: getOmpCommandInvocation,
  spawnSync,
};

export function runOmpCommand(
  ompPath: string,
  args: string[],
  timeout = 30_000,
  overrides: Partial<OmpCommandExecutionDeps> = {},
): OmpCommandResult {
  const deps = { ...DEFAULT_COMMAND_EXECUTION_DEPS, ...overrides };
  try {
    const invocation = deps.getInvocation(ompPath, args);
    const result = deps.spawnSync(invocation.command, invocation.args, {
      encoding: "utf8",
      timeout,
      maxBuffer: 10 * 1024 * 1024,
      stdio: ["ignore", "pipe", "pipe"],
    });
    return {
      ok: result.status === 0 && !result.error,
      stdout: result.stdout?.trim() ?? "",
      stderr: result.stderr?.trim() || result.error?.message || "",
    };
  } catch (error) {
    return {
      ok: false,
      stdout: "",
      stderr: error instanceof Error ? error.message : String(error),
    };
  }
}

export function getOmpVersion(ompPath: string): string | null {
  const result = runOmpCommand(ompPath, ["--version"], 10_000);
  if (!result.ok) return null;
  const match = (result.stdout || result.stderr).match(/(?:omp\/)?(\d+\.\d+\.\d+(?:[-+][\w.-]+)?)/);
  return match?.[1] ?? null;
}

export function listOmpPlugins(ompPath: string): OmpPluginInfo[] | null {
  const result = runOmpCommand(ompPath, ["plugin", "list", "--json"], 30_000);
  if (!result.ok) return null;
  try {
    const parsed = JSON.parse(result.stdout) as { npm?: unknown };
    if (!Array.isArray(parsed.npm)) return [];
    return parsed.npm.flatMap((entry): OmpPluginInfo[] => {
      if (!entry || typeof entry !== "object") return [];
      const value = entry as Record<string, unknown>;
      if (typeof value.name !== "string" || typeof value.version !== "string") return [];
      return [
        {
          name: value.name,
          version: value.version,
          enabled: value.enabled !== false,
          ...(typeof value.path === "string" ? { path: value.path } : {}),
        },
      ];
    });
  } catch {
    return null;
  }
}
