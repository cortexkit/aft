import { spawn } from "node:child_process";
import { cpSync, existsSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { basename, dirname, join } from "node:path";
import { parse as parseJsonc } from "comment-json";

import { log, warn } from "../../logger.js";
import { getCurrentRuntimePackageJsonPath } from "./checker.js";
import { CACHE_DIR, PACKAGE_NAME } from "./constants.js";
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

  const legacyPackageJsonPath = join(dirname(CACHE_DIR), "package.json");
  if (existsSync(legacyPackageJsonPath)) {
    return { installDir: dirname(CACHE_DIR), packageJsonPath: legacyPackageJsonPath };
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
export async function runNpmInstallSafe(
  installDir: string,
  options: { timeoutMs?: number; signal?: AbortSignal } = {},
): Promise<boolean> {
  let timeout: ReturnType<typeof setTimeout> | null = null;

  try {
    if (options.signal?.aborted) return false;
    const proc = spawn("npm", ["install", "--no-audit", "--no-fund", "--no-progress"], {
      cwd: installDir,
      stdio: "ignore",
    });

    const abortProcess = () => {
      try {
        proc.kill();
      } catch {
        // best-effort
      }
    };
    options.signal?.addEventListener("abort", abortProcess, { once: true });

    const exitPromise = new Promise<boolean>((resolveExit) => {
      proc.on("error", () => resolveExit(false));
      proc.on("exit", (code) => resolveExit(code === 0));
    });
    const timeoutPromise = new Promise<"timeout">((resolveTimeout) => {
      timeout = setTimeout(() => resolveTimeout("timeout"), options.timeoutMs ?? 60_000);
    });
    const result = await Promise.race([exitPromise, timeoutPromise]);
    options.signal?.removeEventListener("abort", abortProcess);

    if (result === "timeout" || options.signal?.aborted) {
      abortProcess();
      const snapshot = pendingSnapshots.get(installDir);
      if (snapshot) {
        pendingSnapshots.delete(installDir);
        restoreAutoUpdateSnapshot(snapshot);
      }
      return false;
    }
    const snapshot = pendingSnapshots.get(installDir);
    pendingSnapshots.delete(installDir);
    if (!result && snapshot) {
      restoreAutoUpdateSnapshot(snapshot);
    } else if (snapshot) {
      rmSync(snapshot.tempDir, { recursive: true, force: true });
    }
    return result;
  } catch (err) {
    const snapshot = pendingSnapshots.get(installDir);
    if (snapshot) {
      pendingSnapshots.delete(installDir);
      restoreAutoUpdateSnapshot(snapshot);
    }
    warn(`[auto-update-checker] npm install error: ${String(err)}`);
    return false;
  } finally {
    if (timeout) clearTimeout(timeout);
  }
}
