import { cpSync, existsSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { basename, dirname, join } from "node:path";
import {
  npmInvocation,
  npmSpawnEnv,
  resolveNpm,
  spawn,
  terminateNpmProcessTree,
} from "@cortexkit/aft-bridge";
import { parse as parseJsonc } from "comment-json";

import { log, warn } from "../../logger.js";
import { getCurrentRuntimePackageJsonPath } from "./checker.js";
import { cacheDir, PACKAGE_NAME } from "./constants.js";
import { PackageJsonSchema } from "./types.js";

/**
 * package-lock.json shape (npm v7+) — minimal subset we need.
 * Both `dependencies` (legacy v6) and `packages` (modern v7+) entry forms are
 * present so we clean either layout if encountered.
 */
interface PackageLockfile {
  dependencies?: Record<string, unknown>;
  packages?: Record<string, unknown>;
}

interface AutoUpdateInstallContext {
  installDir: string;
  packageJsonPath: string;
}

interface AutoUpdateSnapshot {
  packageJsonPath: string;
  packageJson: string | null;
  lockfilePath: string;
  lockfile: string | null;
  packageDir: string;
  stagedPackageDir: string | null;
  tempDir: string;
}

const pendingSnapshots = new Map<string, AutoUpdateSnapshot>();
const quarantinedInstallDirs = new Set<string>();

function createAutoUpdateSnapshot(
  installDir: string,
  packageJsonPath: string,
  packageName: string,
) {
  const packageDir = join(installDir, "node_modules", packageName);
  const lockfilePath = join(installDir, "package-lock.json");
  const tempDir = mkdtempSync(join(tmpdir(), "aft-auto-update-"));
  const stagedPackageDir = existsSync(packageDir) ? join(tempDir, "package") : null;
  if (stagedPackageDir) cpSync(packageDir, stagedPackageDir, { recursive: true });
  return {
    packageJsonPath,
    packageJson: existsSync(packageJsonPath) ? readFileSync(packageJsonPath, "utf-8") : null,
    lockfilePath,
    lockfile: existsSync(lockfilePath) ? readFileSync(lockfilePath, "utf-8") : null,
    packageDir,
    stagedPackageDir,
    tempDir,
  };
}

function restoreAutoUpdateSnapshot(snapshot: AutoUpdateSnapshot): void {
  try {
    if (snapshot.packageJson === null) rmSync(snapshot.packageJsonPath, { force: true });
    else writeFileSync(snapshot.packageJsonPath, snapshot.packageJson);
    if (snapshot.lockfile === null) rmSync(snapshot.lockfilePath, { force: true });
    else writeFileSync(snapshot.lockfilePath, snapshot.lockfile);
    rmSync(snapshot.packageDir, { recursive: true, force: true });
    if (snapshot.stagedPackageDir) {
      cpSync(snapshot.stagedPackageDir, snapshot.packageDir, { recursive: true });
    }
  } finally {
    rmSync(snapshot.tempDir, { recursive: true, force: true });
  }
}

function stripPackageNameFromPath(pathValue: string, packageName: string): string | null {
  let current = pathValue;
  for (const segment of [...packageName.split("/")].reverse()) {
    if (basename(current) !== segment) return null;
    current = dirname(current);
  }
  return current;
}

/**
 * Remove our package's entries from package-lock.json so the next `npm install`
 * recomputes them fresh against the new version spec in package.json.
 *
 * Earlier this code targeted `bun.lock` because we used to spawn `bun install`.
 * OpenCode actually installs plugins with npm under the hood, so the install dir
 * always contains `package-lock.json`, never `bun.lock`. Keeping bun.lock
 * handling around would have been dead code that diverged from OpenCode's
 * installer behavior — every auto-update would either no-op (no bun.lock to
 * clean) or generate a parallel bun.lock that drifted from npm's view.
 */
function removeFromPackageLock(installDir: string, packageName: string): boolean {
  const lockPath = join(installDir, "package-lock.json");
  if (!existsSync(lockPath)) return false;

  try {
    const lock = parseJsonc(readFileSync(lockPath, "utf-8")) as PackageLockfile;
    let modified = false;

    // npm v7+ stores entries under `packages` keyed by `node_modules/<name>`.
    if (lock.packages) {
      const key = `node_modules/${packageName}`;
      if (lock.packages[key] !== undefined) {
        delete lock.packages[key];
        modified = true;
      }
    }

    // Legacy `dependencies` map (npm v6 and older) — also clean it for safety.
    if (lock.dependencies?.[packageName]) {
      delete lock.dependencies[packageName];
      modified = true;
    }

    if (modified) {
      writeFileSync(lockPath, JSON.stringify(lock, null, 2));
      log(`[auto-update-checker] Removed from package-lock.json: ${packageName}`);
    }

    return modified;
  } catch {
    return false;
  }
}

function ensureDependencyVersion(
  packageJsonPath: string,
  packageName: string,
  version: string,
): boolean {
  if (!existsSync(packageJsonPath)) return false;

  try {
    const raw = parseJsonc(readFileSync(packageJsonPath, "utf-8"));
    const pkgJson = PackageJsonSchema.safeParse(raw);
    if (!pkgJson.success) return false;

    const nextPackageJson = { ...pkgJson.data };
    const dependencies = { ...(nextPackageJson.dependencies ?? {}) };
    if (dependencies[packageName] === version) return true;

    dependencies[packageName] = version;
    nextPackageJson.dependencies = dependencies;
    writeFileSync(packageJsonPath, JSON.stringify(nextPackageJson, null, 2));
    log(`[auto-update-checker] Updated dependency in package.json: ${packageName} → ${version}`);
    return true;
  } catch (err) {
    warn(`[auto-update-checker] Failed to update package.json dependency: ${String(err)}`);
    return false;
  }
}

function restorePendingSnapshot(installDir: string): void {
  const snapshot = pendingSnapshots.get(installDir);
  if (!snapshot) return;
  pendingSnapshots.delete(installDir);
  restoreAutoUpdateSnapshot(snapshot);
}

function removeInstalledPackage(installDir: string, packageName: string): boolean {
  const packageDir = join(installDir, "node_modules", packageName);
  if (!existsSync(packageDir)) return false;

  rmSync(packageDir, { recursive: true, force: true });
  log(`[auto-update-checker] Package removed: ${packageDir}`);
  return true;
}

export function resolveInstallContext(
  runtimePackageJsonPath: string | null = getCurrentRuntimePackageJsonPath(),
): AutoUpdateInstallContext | null {
  if (runtimePackageJsonPath) {
    const packageDir = dirname(runtimePackageJsonPath);
    const nodeModulesDir = stripPackageNameFromPath(packageDir, PACKAGE_NAME);

    if (nodeModulesDir && basename(nodeModulesDir) === "node_modules") {
      const installDir = dirname(nodeModulesDir);
      const packageJsonPath = join(installDir, "package.json");
      if (existsSync(packageJsonPath)) return { installDir, packageJsonPath };
    }

    return null;
  }

  const cacheRoot = dirname(cacheDir());
  const legacyPackageJsonPath = join(cacheRoot, "package.json");
  if (existsSync(legacyPackageJsonPath)) {
    return { installDir: cacheRoot, packageJsonPath: legacyPackageJsonPath };
  }

  return null;
}

export function preparePackageUpdate(
  version: string,
  packageName: string = PACKAGE_NAME,
  runtimePackageJsonPath: string | null = getCurrentRuntimePackageJsonPath(),
): string | null {
  try {
    const installContext = resolveInstallContext(runtimePackageJsonPath);
    if (!installContext) {
      warn("[auto-update-checker] No install context found for auto-update");
      return null;
    }

    if (quarantinedInstallDirs.has(installContext.installDir)) {
      const recoverySnapshot = pendingSnapshots.get(installContext.installDir);
      const recoveryDetail = recoverySnapshot
        ? ` Recovery snapshot: ${recoverySnapshot.tempDir}`
        : "";
      warn(
        `[auto-update-checker] Auto-update blocked after unconfirmed npm termination; ` +
          `restart OpenCode to retry.${recoveryDetail}`,
      );
      return null;
    }

    const snapshot = createAutoUpdateSnapshot(
      installContext.installDir,
      installContext.packageJsonPath,
      packageName,
    );
    pendingSnapshots.set(installContext.installDir, snapshot);

    if (!ensureDependencyVersion(installContext.packageJsonPath, packageName, version)) {
      pendingSnapshots.delete(installContext.installDir);
      restoreAutoUpdateSnapshot(snapshot);
      return null;
    }

    const packageRemoved = removeInstalledPackage(installContext.installDir, packageName);
    const lockRemoved = removeFromPackageLock(installContext.installDir, packageName);

    if (!packageRemoved && !lockRemoved) {
      log(
        `[auto-update-checker] No cached package artifacts removed for ${packageName}; continuing with updated dependency spec`,
      );
    }

    return installContext.installDir;
  } catch (err) {
    warn(`[auto-update-checker] Failed to prepare package update: ${String(err)}`);
    return null;
  }
}

/**
 * Run `npm install` in the install dir to materialize the dependency version
 * we just rewrote into package.json. Earlier versions used `bun install`,
 * but OpenCode itself installs plugins via npm — the install dir always
 * contains `package-lock.json`, never `bun.lock` — so calling npm matches
 * the existing lockfile shape and avoids generating a parallel bun.lock
 * that drifts from OpenCode's view.
 *
 * `--no-audit --no-fund --no-progress` keeps the output minimal and avoids
 * noisy network calls during background auto-updates.
 *
 * The default timeout is 60s — long enough for a typical reinstall over a
 * mediocre network, short enough that a stuck install doesn't pin the plugin
 * process. Caller can override.
 */
const STDERR_TAIL_BYTES = 16 * 1024;

export async function runNpmInstallSafe(
  installDir: string,
  options: { timeoutMs?: number; signal?: AbortSignal } = {},
): Promise<{ ok: boolean; reason?: string; stderrTail?: string }> {
  let timeout: ReturnType<typeof setTimeout> | null = null;
  let stderrTail = "";

  try {
    if (options.signal?.aborted) {
      restorePendingSnapshot(installDir);
      return { ok: false, reason: "aborted" };
    }
    // Resolve npm beyond PATH: GUI/Desktop launches often have a stripped PATH
    // with no version-manager bin dir, so a bare `npm` spawn fails with ENOENT
    // and the update silently never installs. resolveNpm() also yields the bin
    // dir so npm's `#!/usr/bin/env node` shebang can find its sibling node.
    const npm = resolveNpm();
    if (!npm) {
      restorePendingSnapshot(installDir);
      const reason = "npm not found on PATH or in known version-manager locations";
      warnNpmInstallFailure(reason, stderrTail);
      return { ok: false, reason };
    }
    const invocation = npmInvocation(npm, [
      "install",
      "--no-audit",
      "--no-fund",
      "--no-progress",
      "--ignore-scripts",
    ]);
    const proc = spawn(invocation.command, invocation.args, {
      cwd: installDir,
      stdio: ["ignore", "pipe", "pipe"],
      env: { ...npmSpawnEnv(npm), ...invocation.env },
      windowsVerbatimArguments: invocation.windowsVerbatimArguments,
    });
    proc.stderr?.on("data", (chunk: Buffer) => {
      stderrTail += chunk.toString("utf8");
      if (stderrTail.length > STDERR_TAIL_BYTES) {
        stderrTail = stderrTail.slice(-STDERR_TAIL_BYTES);
      }
    });
    proc.stdout?.on("data", () => {
      // Drain stdout too; stderr carries the actionable failure detail.
    });

    let terminationPromise: Promise<void> | null = null;
    const abortProcess = () => {
      terminationPromise ??= terminateNpmProcessTree(proc, invocation);
      return terminationPromise;
    };

    const exitPromise = new Promise<{ ok: boolean; reason?: string }>((resolveExit) => {
      proc.on("error", (err) => resolveExit({ ok: false, reason: `spawn error: ${String(err)}` }));
      proc.on("exit", (code) =>
        resolveExit(
          code === 0
            ? { ok: true }
            : { ok: false, reason: `npm install exited with code ${code ?? "signal/unknown"}` },
        ),
      );
    });
    const timeoutPromise = new Promise<"timeout">((resolveTimeout) => {
      timeout = setTimeout(() => resolveTimeout("timeout"), options.timeoutMs ?? 60_000);
    });
    let resolveAbort!: (result: "abort") => void;
    const abortPromise = new Promise<"abort">((resolve) => {
      resolveAbort = resolve;
    });
    const onAbort = () => resolveAbort("abort");
    options.signal?.addEventListener("abort", onAbort, { once: true });
    // Close the registration race for signals aborted before the listener was attached.
    if (options.signal?.aborted) onAbort();

    const result = await Promise.race([exitPromise, timeoutPromise, abortPromise]);
    options.signal?.removeEventListener("abort", onAbort);

    if (result === "timeout" || result === "abort") {
      try {
        await abortProcess();
      } catch (error) {
        // Fail closed: restoring while an unobserved npm descendant may still
        // write would turn a timeout into cache corruption. Keep the staged
        // snapshot and report the unknown outcome for manual recovery/restart.
        quarantinedInstallDirs.add(installDir);
        const recoverySnapshot = pendingSnapshots.get(installDir);
        const recoveryDetail = recoverySnapshot
          ? `; auto-update quarantined for this session; recovery snapshot: ${recoverySnapshot.tempDir}`
          : "; auto-update quarantined for this session";
        const reason = `termination outcome unknown: ${String(error)}${recoveryDetail}`;
        warnNpmInstallFailure(reason, stderrTail);
        return { ok: false, reason, stderrTail: stderrTail || undefined };
      }
      restorePendingSnapshot(installDir);
      const reason = result === "abort" ? "aborted" : "timeout";
      warnNpmInstallFailure(reason, stderrTail);
      return { ok: false, reason, stderrTail: stderrTail || undefined };
    }
    const snapshot = pendingSnapshots.get(installDir);
    pendingSnapshots.delete(installDir);
    if (!result.ok && snapshot) {
      restoreAutoUpdateSnapshot(snapshot);
    } else if (snapshot) {
      rmSync(snapshot.tempDir, { recursive: true, force: true });
    }
    if (!result.ok) {
      warnNpmInstallFailure(result.reason ?? "npm install failed", stderrTail);
    }
    return { ...result, stderrTail: stderrTail || undefined };
  } catch (err) {
    restorePendingSnapshot(installDir);
    const reason = `exception: ${String(err)}`;
    warnNpmInstallFailure(reason, stderrTail);
    return { ok: false, reason, stderrTail: stderrTail || undefined };
  } finally {
    if (timeout) clearTimeout(timeout);
  }
}

function warnNpmInstallFailure(reason: string, stderrTail?: string): void {
  const tail = stderrTail ? `\nstderr tail:\n${stderrTail}` : "";
  warn(`[auto-update-checker] npm install failed (${reason})${tail}`);
}
