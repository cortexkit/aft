/**
 * Disk layout and bookkeeping for AFT's auto-installed LSP cache.
 *
 * Layout under `<aft-cache-root>/lsp-packages/`:
 *
 *   <pkg>/
 *     node_modules/.bin/<binary>     ← actual installed binary
 *     node_modules/<pkg>/...
 *     package.json                   ← created by `bun add`
 *     .aft-version-check             ← JSON: { last_checked: ISO, latest: "X.Y.Z" }
 *     .aft-installing                ← presence = install in progress (lockfile)
 *
 * For scoped packages like `@vue/language-server`, the `@` is preserved in
 * the directory path. `<pkg>` is URL-encoded to keep filesystem-safe paths
 * for any future packages with unusual characters.
 */

import {
  closeSync,
  mkdirSync,
  openSync,
  readFileSync,
  statSync,
  unlinkSync,
  writeFileSync,
} from "node:fs";
import { homedir } from "node:os";
import { join } from "node:path";
import { log, warn } from "./logger.js";

/**
 * Root directory that holds all AFT-installed LSP packages.
 *
 * Honors `AFT_CACHE_DIR` for tests so suites do not pollute the real
 * user cache. Falls back to the platform cache root used by the CLI:
 * `%LOCALAPPDATA%/aft` on Windows, `$XDG_CACHE_HOME/aft` or `~/.cache/aft`
 * elsewhere.
 */
export function aftCacheBase(): string {
  const override = process.env.AFT_CACHE_DIR;
  if (override && override.length > 0) return override;

  if (process.platform === "win32") {
    const localAppData = process.env.LOCALAPPDATA || process.env.APPDATA;
    const base = localAppData || join(homedir(), "AppData", "Local");
    return join(base, "aft");
  }

  const base = process.env.XDG_CACHE_HOME || join(homedir(), ".cache");
  return join(base, "aft");
}

export function lspCacheRoot(): string {
  return join(aftCacheBase(), "lsp-packages");
}

/** Directory for one specific npm package's install. */
export function lspPackageDir(npmPackage: string): string {
  return join(lspCacheRoot(), encodeURIComponent(npmPackage));
}

/** Path to the binary inside that package's `node_modules/.bin/`. */
export function lspBinaryPath(npmPackage: string, binary: string): string {
  return join(lspPackageDir(npmPackage), "node_modules", ".bin", binary);
}

/** Directory passed to Rust as part of `lsp_paths_extra`. */
export function lspBinDir(npmPackage: string): string {
  return join(lspPackageDir(npmPackage), "node_modules", ".bin");
}

/** True when the cached binary file exists. */
export function isInstalled(npmPackage: string, binary: string): boolean {
  for (const candidate of lspBinaryCandidates(binary)) {
    try {
      if (statSync(join(lspBinDir(npmPackage), candidate)).isFile()) return true;
    } catch {
      // Try the next Windows shim extension.
    }
  }
  return false;
}

function lspBinaryCandidates(binary: string): string[] {
  if (process.platform !== "win32") return [binary];
  return [binary, `${binary}.cmd`, `${binary}.exe`, `${binary}.bat`];
}

/**
 * Per-install metadata recorded after a successful install.
 *
 * Audit v0.17 #4: persisting the installed version lets us detect a
 * `lsp.versions` pin change and trigger a transparent reinstall.
 *
 * Audit v0.17 #1: persisting the SHA-256 of the downloaded archive enables
 * Trust-On-First-Use verification — if the same tag is ever reinstalled
 * with a different hash, we reject it (the release was retroactively
 * rewritten or the download was tampered with). `sha256` is optional
 * because npm-installed packages don't have a single archive to hash,
 * and historic installs predating this field have no recorded hash.
 *
 * `version` is the resolved tag/semver string (npm package version or
 * GitHub release tag). `installedAt` is informational only.
 */
export interface InstalledMeta {
  version: string;
  installedAt: string;
  /** SHA-256 of the downloaded archive (GitHub installs only). */
  sha256?: string;
}

const INSTALLED_META_FILE = ".aft-installed";

/**
 * Write the installed-version record into `installDir`. Used by both the
 * npm install path (cache layout `<lspPackageDir>/<pkg>/`) and the GitHub
 * install path (cache layout `<ghPackageDir>/<spec.id>/`). Best-effort —
 * failures only logged.
 *
 * Pass `sha256` for GitHub installs so the next session can do TOFU
 * verification (audit v0.17 #1). npm installs leave it undefined since
 * there's no single archive to hash.
 */
export function writeInstalledMetaIn(installDir: string, version: string, sha256?: string): void {
  try {
    mkdirSync(installDir, { recursive: true });
    const meta: InstalledMeta = {
      version,
      installedAt: new Date().toISOString(),
      ...(sha256 ? { sha256 } : {}),
    };
    writeFileSync(join(installDir, INSTALLED_META_FILE), JSON.stringify(meta), "utf8");
  } catch (err) {
    log(`[lsp-cache] failed to write installed-meta in ${installDir}: ${err}`);
  }
}

/** Read the installed-version record from `installDir`, or null if missing/corrupt. */
export function readInstalledMetaIn(installDir: string): InstalledMeta | null {
  const path = join(installDir, INSTALLED_META_FILE);
  try {
    if (!statSync(path).isFile()) return null;
    const raw = readFileSync(path, "utf8");
    const parsed = JSON.parse(raw) as Partial<InstalledMeta>;
    if (typeof parsed.version !== "string" || parsed.version.length === 0) return null;
    return {
      version: parsed.version,
      installedAt: typeof parsed.installedAt === "string" ? parsed.installedAt : "",
      ...(typeof parsed.sha256 === "string" && parsed.sha256.length > 0
        ? { sha256: parsed.sha256 }
        : {}),
    };
  } catch {
    return null;
  }
}

/** npm install path: write installed metadata into the package cache dir.
 *
 * Audit-2 v0.17 #1: pass `sha256` of the installed binary so the next
 * session can do TOFU verification on reinstalls of the same version.
 */
export function writeInstalledMeta(packageKey: string, version: string, sha256?: string): void {
  writeInstalledMetaIn(lspPackageDir(packageKey), version, sha256);
}

/** npm install path: read installed metadata from the package cache dir. */
export function readInstalledMeta(packageKey: string): InstalledMeta | null {
  return readInstalledMetaIn(lspPackageDir(packageKey));
}

/**
 * Path to the install-in-progress lockfile.
 *
 * Convention: presence of this file means an install is currently running
 * (or crashed). The auto-installer treats it as "do not start another".
 * Stale lockfiles older than 30 minutes are auto-cleared.
 */
function lockPath(npmPackage: string): string {
  return join(lspPackageDir(npmPackage), ".aft-installing");
}

const STALE_LOCK_MS = 30 * 60 * 1000;

/**
 * Acquire an install lock for `lockKey` using an atomic `O_EXCL` open.
 *
 * The previous implementation used `existsSync` + `writeFileSync({flag:"w"})`
 * which is a textbook TOCTOU: two concurrent processes both pass the check,
 * both call write (which truncates with flag "w"), and both think they own
 * the lock. With `wx` the second process's open() fails atomically.
 *
 * Stale-lock recovery: if the lock file exists, we read it. If the recorded
 * PID is no longer alive OR the file is older than `STALE_LOCK_MS`, we claim
 * the lock by atomically replacing it (unlink + create with `wx`). If that
 * race fails (someone else just claimed it), we return false honestly.
 *
 * The lock file content is `<pid>\n<iso-timestamp>\n` so other processes
 * can detect dead owners.
 */
export function acquireInstallLock(lockKey: string): boolean {
  mkdirSync(lspPackageDir(lockKey), { recursive: true });
  const lock = lockPath(lockKey);
  const tryClaim = (): boolean => {
    try {
      // Atomic exclusive create. If the file exists, throws EEXIST.
      const fd = openSync(lock, "wx");
      try {
        writeFileSync(fd, `${process.pid}\n${new Date().toISOString()}\n`);
      } finally {
        closeSync(fd);
      }
      return true;
    } catch (err) {
      const code = (err as NodeJS.ErrnoException).code;
      if (code === "EEXIST") return false;
      warn(`[lsp] unexpected error acquiring install lock for ${lockKey}: ${err}`);
      return false;
    }
  };

  if (tryClaim()) return true;

  // Failed — inspect the existing lock to decide whether to steal it.
  let owningPid: number | null = null;
  let lockMtimeMs = 0;
  try {
    const raw = readFileSync(lock, "utf8");
    const firstLine = raw.split(/\r?\n/, 1)[0]?.trim() ?? "";
    const parsed = Number.parseInt(firstLine, 10);
    if (Number.isFinite(parsed) && parsed > 0) owningPid = parsed;
    lockMtimeMs = statSync(lock).mtimeMs;
  } catch {
    // Lock vanished between failure and inspection — try once more.
    return tryClaim();
  }

  // Owning process still alive AND lock is fresh? Don't steal.
  //
  // Audit-2 v0.17 #7:
  //   (a) Large-negative `age` happens when the system clock moves backward
  //       (NTP correction, sleep/wake on a stale RTC, manual clock change).
  //       The original `age < STALE_LOCK_MS` check treats huge negatives as
  //       "fresh", deadlocking forever. Use Math.abs so locks with mtime
  //       further than STALE_LOCK_MS in EITHER direction get reclaimed.
  //       Small negative ages (FS mtime rounding on freshly-written locks)
  //       still count as fresh, which is what we want.
  //   (b) On Windows, `process.kill(pid, 0)` is unreliable: it can throw
  //       EPERM for a long-dead PID that's been recycled, and some Node
  //       builds throw other codes that our isProcessAlive treats as
  //       "alive". Skip the PID liveness check on Windows entirely and
  //       rely on the age timeout. The worst case is we wait 30 minutes
  //       to reclaim a dead lock — that's the documented STALE_LOCK_MS
  //       guarantee anyway.
  const age = Date.now() - lockMtimeMs;
  const ageWithinFresh = Math.abs(age) < STALE_LOCK_MS;
  const skipLiveness = process.platform === "win32";
  const ownerAlive = !skipLiveness && owningPid !== null && isProcessAlive(owningPid);
  if (skipLiveness ? ageWithinFresh : ownerAlive && ageWithinFresh) {
    return false;
  }

  // Steal: log why, then atomically replace. unlink+wx avoids a brief window
  // where another process could observe "no lock". If unlink races, retry once.
  log(
    `[lsp] reclaiming install lock for ${lockKey} (owner_pid=${owningPid ?? "unknown"}, alive=${ownerAlive}, age_ms=${age})`,
  );
  try {
    unlinkSync(lock);
  } catch {
    // Already gone — fine.
  }
  return tryClaim();
}

/**
 * Best-effort liveness check for a PID. Sending signal 0 doesn't actually
 * deliver anything; it just reports whether we'd be allowed to. Errors:
 * `ESRCH` = no such process (dead). `EPERM` = exists but not ours (alive).
 * Any other error: assume alive (safer for stealing).
 */
function isProcessAlive(pid: number): boolean {
  try {
    process.kill(pid, 0);
    return true;
  } catch (err) {
    const code = (err as NodeJS.ErrnoException).code;
    if (code === "ESRCH") return false;
    return true;
  }
}

export function releaseInstallLock(lockKey: string): void {
  const lock = lockPath(lockKey);
  // Audit-3 v0.17 #3: TOCTOU defense.
  //
  // The previous implementation did `existsSync(lock) && unlinkSync(lock)` with
  // no ownership check. If process A's lock was reclaimed by process B (because
  // A exceeded STALE_LOCK_MS), A's finally block would `unlinkSync` B's
  // newly-created lock, breaking mutual exclusion: B believes it owns the lock
  // while the file is gone, allowing process C to claim it. Both B and C then
  // run concurrent installs into the same cache directory.
  //
  // Fix: read the PID from the lock file and only unlink if it matches our own.
  // Any other state (lock missing, lock owned by a different PID, lock corrupt)
  // is a no-op — exactly the safe behavior. We catch ENOENT explicitly so the
  // common "lock vanished between read and unlink" race is silent.
  try {
    let owningPid: number | null = null;
    try {
      const raw = readFileSync(lock, "utf8");
      const firstLine = raw.split(/\r?\n/, 1)[0]?.trim() ?? "";
      const parsed = Number.parseInt(firstLine, 10);
      if (Number.isFinite(parsed) && parsed > 0) owningPid = parsed;
    } catch (readErr) {
      const code = (readErr as NodeJS.ErrnoException).code;
      if (code === "ENOENT") return; // already gone, nothing to do
      // Corrupt or unreadable lock — leave it alone; stale-lock recovery in
      // acquireInstallLock will eventually reclaim it.
      warn(`[lsp] could not read install lock for ${lockKey} during release: ${readErr}`);
      return;
    }

    if (owningPid !== process.pid) {
      // Either we already released it and another process re-claimed (unusual
      // but possible if our task somehow ran twice), or our lock was stolen
      // because we exceeded STALE_LOCK_MS. In both cases, leave the file
      // alone — the current owner's release will handle it.
      log(
        `[lsp] not releasing install lock for ${lockKey}: owned by pid ${owningPid ?? "unknown"} (we are ${process.pid})`,
      );
      return;
    }

    try {
      unlinkSync(lock);
    } catch (unlinkErr) {
      const code = (unlinkErr as NodeJS.ErrnoException).code;
      if (code !== "ENOENT") {
        warn(`[lsp] failed to release install lock for ${lockKey}: ${unlinkErr}`);
      }
    }
  } catch (err) {
    warn(`[lsp] unexpected error releasing install lock for ${lockKey}: ${err}`);
  }
}

/**
 * Run `task` while holding the install lock. The lock is released only
 * when `task` settles (resolves or rejects), not when it starts.
 *
 * Returns the task's resolved value, or `null` if the lock could not be
 * acquired (caller should treat as "another install in progress").
 */
export async function withInstallLock<T>(
  lockKey: string,
  task: () => Promise<T>,
): Promise<T | null> {
  if (!acquireInstallLock(lockKey)) return null;
  try {
    return await task();
  } finally {
    releaseInstallLock(lockKey);
  }
}

/** Last-checked metadata stored next to the install. */
interface VersionCheckRecord {
  /** ISO timestamp of the last `npm registry probe`. */
  last_checked: string;
  /** Version string that was eligible at last check (after grace filter). */
  latest_eligible: string | null;
}

const VERSION_CHECK_FILE = ".aft-version-check";

export function readVersionCheck(npmPackage: string): VersionCheckRecord | null {
  const file = join(lspPackageDir(npmPackage), VERSION_CHECK_FILE);
  try {
    const raw = readFileSync(file, "utf8");
    const parsed = JSON.parse(raw) as Partial<VersionCheckRecord>;
    if (typeof parsed.last_checked === "string") {
      return {
        last_checked: parsed.last_checked,
        latest_eligible: typeof parsed.latest_eligible === "string" ? parsed.latest_eligible : null,
      };
    }
    return null;
  } catch {
    return null;
  }
}

export function writeVersionCheck(npmPackage: string, latest: string | null): void {
  mkdirSync(lspPackageDir(npmPackage), { recursive: true });
  const file = join(lspPackageDir(npmPackage), VERSION_CHECK_FILE);
  const record: VersionCheckRecord = {
    last_checked: new Date().toISOString(),
    latest_eligible: latest,
  };
  writeFileSync(file, JSON.stringify(record, null, 2));
}

/** True if more than `graceDays` × 24h have elapsed since `last_checked`. */
export function shouldRecheckVersion(
  record: VersionCheckRecord | null,
  weeklyCheckIntervalMs = 7 * 24 * 60 * 60 * 1000,
): boolean {
  if (!record) return true;
  const age = Date.now() - new Date(record.last_checked).getTime();
  if (Number.isNaN(age) || age < 0) return true;
  return age >= weeklyCheckIntervalMs;
}
