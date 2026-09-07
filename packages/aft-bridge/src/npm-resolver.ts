/**
 * Resolve an `npm` executable when it is not on PATH.
 *
 * OpenCode and Pi are frequently launched from a GUI / dock / Desktop app,
 * which gives the process a stripped PATH that does NOT include a Node version
 * manager's bin directory (nvm, mise, volta, fnm, asdf) or even Homebrew. When
 * that happens, `spawn("npm", ...)` fails with "Executable not found in $PATH",
 * so the auto-updater and LSP auto-install silently break. See issue: a user's
 * auto-update churned every launch (rewrite package.json -> delete package ->
 * npm install fails -> restore) and they stayed pinned to the old version.
 *
 * `npm` is itself a Node script (`#!/usr/bin/env node` shebang on Unix), so once
 * we find npm's absolute path we must also make its sibling `node` reachable, or
 * the shebang fails the same way. `resolveNpm()` returns both the command and
 * its bin directory; `npmSpawnEnv()` prepends that directory to PATH for the
 * spawn so npm can find its own node.
 */
import { type ChildProcess, spawn, spawnSync } from "node:child_process";
import { readdirSync, statSync } from "node:fs";
import { homedir } from "node:os";
import { dirname, isAbsolute, join } from "node:path";

import { withPathPrepended } from "./path-env.js";

export interface ResolvedNpm {
  /** Absolute path to npm (or a bare name). Pass it through `npmInvocation()`. */
  command: string;
  /** Directory containing npm, prepended to PATH at spawn time so npm's
   * `#!/usr/bin/env node` shebang can find its sibling node. Null when the
   * command was found via the OS PATH resolver and no augmentation is needed. */
  binDir: string | null;
}

/** Executable and arguments suitable for Node's child-process APIs. */
export interface NpmInvocation {
  command: string;
  args: string[];
  /** Environment additions required by the invocation. */
  env?: Readonly<Record<string, string>>;
  /** Required when passing a fully quoted command line to cmd.exe. */
  windowsVerbatimArguments?: boolean;
  /** True when cmd.exe is wrapping an npm.cmd/npm.bat script. */
  windowsCmdShim?: boolean;
}

interface ResolveNpmDeps {
  platform: NodeJS.Platform;
  env: NodeJS.ProcessEnv;
  home: string;
  execPath: string;
  /**
   * Absolute system bin directories scanned last (e.g. /usr/local/bin). Defaults
   * to the platform's well-known list. Injectable so tests can pass `[]` to stay
   * hermetic — otherwise a real system npm (present on CI runners) leaks in and
   * breaks the "returns null" cases.
   */
  systemNpmDirs?: string[];
}

function defaultDeps(): ResolveNpmDeps {
  return {
    platform: process.platform,
    env: process.env,
    home: homedir(),
    execPath: process.execPath,
  };
}

function npmBinaryName(platform: NodeJS.Platform): string {
  return platform === "win32" ? "npm.cmd" : "npm";
}

function isFile(p: string): boolean {
  try {
    return statSync(p).isFile();
  } catch {
    return false;
  }
}

/** Scan the PATH env for npm. Returns the first match's directory, or null. */
function npmFromPath(deps: ResolveNpmDeps): string | null {
  const name = npmBinaryName(deps.platform);
  const env = withPathPrepended(deps.env, undefined, deps.platform);
  const pathKey =
    deps.platform === "win32"
      ? Object.keys(env).find((key) => key.toLowerCase() === "path")
      : "PATH";
  const raw = pathKey === undefined ? "" : (env[pathKey] ?? "");
  const separator = deps.platform === "win32" ? ";" : ":";
  for (const entry of raw.split(separator)) {
    const dir = entry.trim().replace(/^"|"$/g, "");
    if (!dir || !isAbsolute(dir)) continue;
    if (isFile(join(dir, name))) return dir;
  }
  return null;
}

/** npm ships beside node in standard installs (e.g. /opt/homebrew/bin/{node,npm}). */
function npmAdjacentToNode(deps: ResolveNpmDeps): string | null {
  // process.execPath is the running node/bun binary. Under Node this is
  // .../bin/node with npm as a sibling; under Bun (OpenCode TUI) there is no
  // npm sibling, which is fine — we fall through to well-known locations.
  const dir = dirname(deps.execPath);
  return isFile(join(dir, npmBinaryName(deps.platform))) ? dir : null;
}

/**
 * Pick the highest-version subdirectory under a version-manager `installs`
 * directory that actually contains npm. Used for nvm / mise layouts like
 * `~/.nvm/versions/node/<ver>/bin/npm`.
 */
function highestVersionedNodeBin(installsDir: string, name: string): string | null {
  let entries: string[];
  try {
    entries = readdirSync(installsDir);
  } catch {
    return null;
  }
  const candidates = entries
    .filter((v) => isFile(join(installsDir, v, "bin", name)))
    .sort((a, b) => compareVersionsDesc(a, b));
  return candidates.length > 0 ? join(installsDir, candidates[0], "bin") : null;
}

/** Descending semver-ish compare; non-numeric segments sort after numeric. */
function compareVersionsDesc(a: string, b: string): number {
  const pa = a
    .replace(/^v/, "")
    .split(".")
    .map((n) => Number.parseInt(n, 10));
  const pb = b
    .replace(/^v/, "")
    .split(".")
    .map((n) => Number.parseInt(n, 10));
  for (let i = 0; i < Math.max(pa.length, pb.length); i++) {
    const na = Number.isFinite(pa[i]) ? pa[i] : -1;
    const nb = Number.isFinite(pb[i]) ? pb[i] : -1;
    if (na !== nb) return nb - na;
  }
  return b.localeCompare(a);
}

/** Well-known npm bin directories, in priority order, for the current platform. */
function wellKnownNpmDirs(deps: ResolveNpmDeps): string[] {
  const { platform, env, home } = deps;
  const name = npmBinaryName(platform);
  const dirs: string[] = [];
  const push = (dir: string | null | undefined) => {
    if (dir && !dirs.includes(dir)) dirs.push(dir);
  };

  if (platform === "win32") {
    const programFiles = env.ProgramFiles || "C:\\Program Files";
    const appData = env.APPDATA;
    const localAppData = env.LOCALAPPDATA;
    push(join(programFiles, "nodejs"));
    if (appData) push(join(appData, "npm"));
    if (localAppData) push(join(localAppData, "Volta", "bin"));
    // nvm-windows
    if (env.NVM_SYMLINK) push(env.NVM_SYMLINK);
  } else {
    // Active node version manager hints (set even when PATH is otherwise stripped).
    if (env.NVM_BIN) push(env.NVM_BIN);
    // Version-manager installs (pick highest version with npm).
    push(highestVersionedNodeBin(join(home, ".nvm", "versions", "node"), name));
    push(highestVersionedNodeBin(join(home, ".local", "share", "mise", "installs", "node"), name));
    push(highestVersionedNodeBin(join(home, ".asdf", "installs", "nodejs"), name));
    // Fixed-location managers.
    push(join(home, ".volta", "bin"));
    push(join(home, ".asdf", "shims"));
    // Homebrew + system (injectable so tests stay hermetic; see ResolveNpmDeps).
    const systemDirs =
      deps.systemNpmDirs ??
      (platform === "darwin"
        ? ["/opt/homebrew/bin", "/usr/local/bin"]
        : ["/usr/local/bin", "/usr/bin", join(home, ".local", "bin")]);
    for (const dir of systemDirs) push(dir);
  }
  return dirs;
}

/**
 * Resolve npm, preferring PATH, then node-adjacent, then well-known version
 * manager / system locations. Returns null only when npm genuinely cannot be
 * found anywhere we know to look.
 */
export function resolveNpm(deps: ResolveNpmDeps = defaultDeps()): ResolvedNpm | null {
  const name = npmBinaryName(deps.platform);

  // 1. PATH — respects the user's own setup when it survived to this process.
  const onPath = npmFromPath(deps);
  if (onPath) return { command: join(onPath, name), binDir: onPath };

  // 2. Node-adjacent (npm sits next to node in standard installs).
  const adjacent = npmAdjacentToNode(deps);
  if (adjacent) return { command: join(adjacent, name), binDir: adjacent };

  // 3. Well-known version-manager / system locations.
  for (const dir of wellKnownNpmDirs(deps)) {
    const candidate = join(dir, name);
    if (isFile(candidate)) return { command: candidate, binDir: dir };
  }

  return null;
}

function quoteCmdArgument(value: string): string {
  // cmd.exe expands percent variables even inside quotes, while quotes and line
  // breaks can terminate the argument and append another command. npm's current
  // callers use fixed flags and validated package specs, so reject these unsafe
  // forms rather than silently introducing shell parsing.
  if (/[\0\r\n"%]/.test(value)) {
    throw new Error(
      `npm argument cannot be represented safely for cmd.exe: ${JSON.stringify(value)}`,
    );
  }
  return `"${value}"`;
}

/**
 * Build a cross-platform child-process invocation for a resolved npm command.
 *
 * Windows `.cmd`/`.bat` shims are scripts, not native executables, and direct
 * `spawn()`/`execFileSync()` calls fail with EINVAL. Route only those shims
 * through cmd.exe; native executables and Unix npm scripts remain direct.
 */
export function npmInvocation(
  resolved: ResolvedNpm,
  npmArgs: readonly string[],
  platform: NodeJS.Platform = process.platform,
  env: NodeJS.ProcessEnv = process.env,
): NpmInvocation {
  if (platform !== "win32" || !/\.(?:cmd|bat)$/i.test(resolved.command)) {
    return { command: resolved.command, args: [...npmArgs] };
  }

  if (/[\0\r\n"]/.test(resolved.command)) {
    throw new Error(
      `npm command cannot be represented safely for cmd.exe: ${JSON.stringify(resolved.command)}`,
    );
  }

  // Keep the path out of cmd.exe's command text. In particular, cmd expands
  // `%NAME%` even inside quotes; expansion is single-pass, so a literal `%` in
  // the environment value remains literal after `%AFT_NPM_COMMAND%` expands.
  const commandEnvName = "AFT_NPM_COMMAND";
  const quotedArgs = npmArgs.map(quoteCmdArgument);
  const commandLine = `""%${commandEnvName}%"${quotedArgs.length > 0 ? ` ${quotedArgs.join(" ")}` : ""}"`;
  return {
    command: env.ComSpec ?? env.COMSPEC ?? "cmd.exe",
    args: ["/d", "/s", "/v:off", "/c", commandLine],
    env: { [commandEnvName]: resolved.command },
    windowsVerbatimArguments: true,
    windowsCmdShim: true,
  };
}

/**
 * Terminate an npm child safely. Windows cmd shims create a cmd.exe -> node.exe
 * tree, so killing only the immediate child can leave npm writing in the
 * background after a rollback or install-lock release. Resolves after the
 * immediate child exits, or once Windows tree termination is confirmed at the
 * grace deadline. Direct children escalate to SIGKILL after a grace period.
 * Windows tree-kill failures reject as unknown outcomes instead of falling back
 * to killing cmd.exe alone.
 */
export class NpmTerminationUnknownError extends Error {
  readonly code = "npm_termination_unknown";

  constructor(detail: string) {
    super(`npm process-tree termination could not be confirmed: ${detail}`);
    this.name = "NpmTerminationUnknownError";
  }
}

function terminateDirectNpmChild(child: ChildProcess, gracePeriodMs: number): Promise<void> {
  if (child.exitCode !== null || child.signalCode !== null) return Promise.resolve();

  return new Promise((resolve, reject) => {
    let settled = false;
    let forceTimer: ReturnType<typeof setTimeout> | null = null;
    let confirmationTimer: ReturnType<typeof setTimeout> | null = null;
    let signalFailure: string | null = null;
    const cleanup = () => {
      if (forceTimer) clearTimeout(forceTimer);
      if (confirmationTimer) clearTimeout(confirmationTimer);
      child.removeListener("exit", finish);
      child.removeListener("error", onChildError);
    };
    const finish = () => {
      if (settled) return;
      settled = true;
      cleanup();
      resolve();
    };
    const fail = () => {
      if (settled) return;
      if (child.exitCode !== null || child.signalCode !== null) {
        finish();
        return;
      }
      settled = true;
      cleanup();
      reject(
        new NpmTerminationUnknownError(
          signalFailure ?? `direct npm child did not exit after SIGKILL within ${gracePeriodMs}ms`,
        ),
      );
    };
    const onChildError = (error: Error) => {
      signalFailure = `direct npm child error during termination: ${String(error)}`;
    };
    const signal = (value?: NodeJS.Signals) => {
      try {
        if (!child.kill(value)) {
          signalFailure = `direct npm child rejected ${value ?? "SIGTERM"}`;
        }
      } catch (error) {
        signalFailure = `direct npm child ${value ?? "SIGTERM"} failed: ${String(error)}`;
      }
    };

    child.once("exit", finish);
    child.on("error", onChildError);
    forceTimer = setTimeout(() => {
      signal("SIGKILL");
      confirmationTimer = setTimeout(fail, gracePeriodMs);
    }, gracePeriodMs);
    signal();
    if (child.exitCode !== null || child.signalCode !== null) finish();
  });
}

export function terminateNpmProcessTree(
  child: ChildProcess,
  invocation: NpmInvocation,
  env: NodeJS.ProcessEnv = process.env,
  gracePeriodMs = 5_000,
): Promise<void> {
  if (!invocation.windowsCmdShim) return terminateDirectNpmChild(child, gracePeriodMs);
  const exitedSuccessfully = () => child.exitCode === 0 && child.signalCode === null;
  // A shim that completed successfully before cancellation won the race: cmd.exe
  // waited for npm.cmd, which waited for node, so there is no live tree to kill.
  // Do not generalize this to nonzero or signal exits; those remain unknown.
  if (exitedSuccessfully()) return Promise.resolve();
  if (child.pid === undefined) {
    return Promise.reject(new NpmTerminationUnknownError("cmd.exe child has no process ID"));
  }

  return new Promise((resolve, reject) => {
    let settled = false;
    let childExited = child.exitCode !== null || child.signalCode !== null;
    let treeKillConfirmed = false;
    let treeKillFailure: string | null = null;
    let killer: ChildProcess | null = null;
    const timeout = setTimeout(() => {
      try {
        killer?.kill();
      } catch {
        // The taskkill process may already have exited.
      }
      if (treeKillConfirmed || exitedSuccessfully()) {
        settled = true;
        cleanup();
        resolve();
        return;
      }
      fail(treeKillFailure ?? `taskkill.exe did not finish within ${gracePeriodMs}ms`);
    }, gracePeriodMs);
    const cleanup = () => {
      clearTimeout(timeout);
      child.removeListener("exit", onChildExit);
    };
    const succeedIfConfirmed = () => {
      if (settled || !childExited || !treeKillConfirmed) return;
      settled = true;
      cleanup();
      resolve();
    };
    const fail = (detail: string) => {
      if (settled) return;
      settled = true;
      cleanup();
      reject(new NpmTerminationUnknownError(detail));
    };
    function onChildExit() {
      childExited = true;
      if (treeKillFailure !== null) {
        if (exitedSuccessfully()) {
          settled = true;
          cleanup();
          resolve();
        } else {
          fail(treeKillFailure);
        }
        return;
      }
      succeedIfConfirmed();
    }
    const recordTreeKillFailure = (detail: string) => {
      if (settled) return;
      if (exitedSuccessfully()) {
        settled = true;
        cleanup();
        resolve();
        return;
      }
      treeKillFailure = detail;
      if (childExited) fail(detail);
    };

    child.once("exit", onChildExit);
    const systemRoot = env.SystemRoot ?? env.SYSTEMROOT;
    const taskkill = systemRoot ? join(systemRoot, "System32", "taskkill.exe") : "taskkill.exe";
    try {
      killer = spawn(taskkill, ["/pid", String(child.pid), "/t", "/f"], {
        stdio: "ignore",
        windowsHide: true,
      });
      killer.once("error", (error) =>
        recordTreeKillFailure(`taskkill.exe failed to start: ${String(error)}`),
      );
      killer.once("exit", (code) => {
        if (code !== 0) {
          recordTreeKillFailure(`taskkill.exe exited with code ${code ?? "unknown"}`);
          return;
        }
        treeKillConfirmed = true;
        succeedIfConfirmed();
      });
    } catch (error) {
      recordTreeKillFailure(`taskkill.exe failed to start: ${String(error)}`);
    }
  });
}

/**
 * Build a spawn env that makes a resolved npm runnable: prepend its bin dir to
 * PATH so npm's `#!/usr/bin/env node` shebang finds its sibling node, even when
 * the inherited PATH was stripped by a GUI launch.
 */
export function npmSpawnEnv(
  resolved: ResolvedNpm,
  baseEnv: NodeJS.ProcessEnv = process.env,
  platform: NodeJS.Platform = process.platform,
): NodeJS.ProcessEnv {
  return withPathPrepended(baseEnv, resolved.binDir, platform);
}

/**
 * Quick boolean check: can we run npm at all? Used by pre-flight gating before
 * destructive auto-update steps.
 */
export function isNpmAvailable(deps: ResolveNpmDeps = defaultDeps()): boolean {
  return resolveNpm(deps) !== null;
}

/** Test seam: verify a resolved npm actually executes (used by diagnostics). */
export function probeNpmVersion(resolved: ResolvedNpm): string | null {
  try {
    const invocation = npmInvocation(resolved, ["--version"]);
    const result = spawnSync(invocation.command, invocation.args, {
      env: { ...npmSpawnEnv(resolved), ...invocation.env },
      encoding: "utf-8",
      timeout: 5000,
      stdio: ["ignore", "pipe", "ignore"],
      windowsVerbatimArguments: invocation.windowsVerbatimArguments,
    });
    if (result.error || result.status !== 0) return null;
    const version = result.stdout.trim();
    return /^\d+\.\d+\.\d+/.test(version) ? version : null;
  } catch {
    return null;
  }
}
