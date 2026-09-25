import {
  closeSync,
  existsSync,
  mkdirSync,
  openSync,
  readFileSync,
  renameSync,
  rmSync,
  statSync,
  writeFileSync,
} from "node:fs";
import { dirname } from "node:path";
import { compareSemver, isNpmAvailable, repairRootScopedStorageFile } from "@cortexkit/aft-bridge";
import type { PluginInput } from "@opencode-ai/plugin";

import { log, warn } from "../../logger.js";
import { preparePackageUpdate, resolveInstallContext, runNpmInstallSafe } from "./cache.js";
import {
  extractChannel,
  findPluginEntry,
  getCachedVersion,
  getLatestVersion,
  getLocalDevVersion,
} from "./checker.js";
import { cacheDir, NPM_FETCH_TIMEOUT, NPM_REGISTRY_URL, PACKAGE_NAME } from "./constants.js";
import type { AutoUpdateCheckerOptions } from "./types.js";

type OpenCodeEvent = {
  type: string;
  properties?: unknown;
};

type ToastVariant = "info" | "warning" | "error" | "success";

type ResolvedAutoUpdateCheckerOptions = Required<
  Omit<AutoUpdateCheckerOptions, "enabled" | "storageDir">
> & { storageDir: string | null };

type CheckSlotLock = {
  release: () => void;
};

type CheckLockOwner = {
  pid: number;
  startedMs: number;
};

type CheckLockState = {
  pid: number | null;
  startedMs: number;
};

const DEFAULT_CHECK_INTERVAL_MS = 60 * 60 * 1000; // 1 hour
const CHECK_LOCK_STALE_MS = 60 * 60 * 1000; // 1 hour
const DEFAULT_INIT_DELAY_MS = 5_000;
// v0.27 commit 11 deferral: the legacy `last-update-check.json` file is read at
// plugin init, BEFORE any bridge is spawned (lazy-spawn architecture per commit
// 29508a5). Refactoring to `bridge.send("db_get_state")` would force eager bridge
// spawn at every plugin init. Deferred to a future version that decides whether
// to accept that trade-off; this path stays on direct legacy-file access.
const TIMESTAMP_FILENAME = "last-update-check.json";

/**
 * Auto-update checker.
 *
 * Trigger model (rewritten in v0.19.7):
 *
 * The check fires from plugin initialization itself via a `setTimeout`
 * scheduled when this hook is created. We do NOT gate on
 * `session.created` events — that gate was unreliable because:
 *
 *   - TUI restart with a resumed session never fires `session.created`
 *     (the event fires on session creation, not on plugin reload).
 *   - Multi-project plugin reloads each get their own plugin lifetime
 *     with `hasChecked = false`, so only whichever project happens to
 *     create a fresh session first ever runs the check.
 *   - Sidebar/status polling and idle TUI use also never fire
 *     `session.created`.
 *
 * Multi-project coordination is now handled by an on-disk timestamp at
 * `<storageDir>/opencode/last-update-check.json`. Every plugin instance reads
 * the timestamp before checking; if it's within `checkIntervalMs` of
 * now, the check is skipped. The first instance to claim the slot
 * writes the timestamp atomically (temp + rename) so concurrent
 * instances don't all hit npm.
 *
 * The returned event hook is preserved as a no-op so existing tests
 * that pass synthetic events keep working — the hook itself never
 * triggers a check now.
 */
export function createAutoUpdateCheckerHook(
  ctx: PluginInput,
  options: AutoUpdateCheckerOptions = {},
) {
  const {
    enabled = true,
    showStartupToast = true,
    autoUpdate = true,
    npmRegistryUrl = NPM_REGISTRY_URL,
    fetchTimeoutMs = NPM_FETCH_TIMEOUT,
    signal = new AbortController().signal,
    storageDir = null,
    checkIntervalMs = DEFAULT_CHECK_INTERVAL_MS,
    initDelayMs = DEFAULT_INIT_DELAY_MS,
  } = options;

  if (!enabled) {
    return async (_input: { event: OpenCodeEvent }) => {
      // Disabled — never check.
    };
  }

  // Schedule the check on plugin init, not on any event. The setTimeout
  // intentionally returns control to OpenCode immediately so plugin init
  // never blocks on the npm round-trip.
  const initTimer = setTimeout(() => {
    void maybeRunCheck(ctx, {
      showStartupToast,
      autoUpdate,
      npmRegistryUrl,
      fetchTimeoutMs,
      signal,
      storageDir,
      checkIntervalMs,
      initDelayMs,
    }).catch((err) => {
      warn(`[auto-update-checker] Background update check failed: ${String(err)}`);
    });
  }, initDelayMs);

  // Don't keep the Node event loop alive just for this timer.
  if (typeof initTimer === "object" && initTimer !== null && "unref" in initTimer) {
    (initTimer as { unref: () => void }).unref();
  }

  // Cancel the pending check if the host aborts (plugin shutdown).
  signal.addEventListener(
    "abort",
    () => {
      clearTimeout(initTimer);
    },
    { once: true },
  );

  // Event hook is now a no-op. Kept for API/test compatibility.
  return async (_input: { event: OpenCodeEvent }) => {
    // Intentionally empty — see hook comment.
  };
}

async function maybeRunCheck(
  ctx: PluginInput,
  options: ResolvedAutoUpdateCheckerOptions,
): Promise<void> {
  if (options.signal.aborted) return;

  // Honor the cross-process dedup window first. If another plugin
  // instance recently checked or is currently checking, skip silently. The
  // lock stays held until the full check/install path completes so two
  // plugin processes cannot race through preparePackageUpdate() and
  // runNpmInstallSafe() against the same OpenCode install root.
  const checkSlot = claimCheckSlot(options.storageDir, options.checkIntervalMs);
  if (!checkSlot) {
    log("[auto-update-checker] Skipping check (another instance ran one recently)");
    return;
  }

  try {
    await runStartupCheck(ctx, options);
  } finally {
    checkSlot.release();
  }
}

/**
 * Try to claim the next check slot via the on-disk timestamp file plus an
 * atomic lockfile. The returned lock must be released after the check (and any
 * install) completes.
 *
 * The timestamp preserves the cross-process dedup window. The lock closes the
 * TOCTOU gap around that timestamp and, more importantly, stays held through
 * preparePackageUpdate() + runNpmInstallSafe() so one updater cannot roll back
 * another updater's successful install from a separate process.
 */
function claimCheckSlot(storageDir: string | null, intervalMs: number): CheckSlotLock | null {
  if (!storageDir) return { release: () => {} }; // No storage available — fail open.

  const file = repairRootScopedStorageFile(storageDir, "opencode", TIMESTAMP_FILENAME);
  try {
    if (hasRecentCheckTimestamp(file, intervalMs)) return null;

    mkdirSync(dirname(file), { recursive: true });
    const lockPath = `${file}.lock`;
    const owner = { pid: process.pid, startedMs: Date.now() };
    let lockFd: number | null = null;
    try {
      // `wx` maps to O_CREAT | O_EXCL | O_WRONLY: exactly one process wins.
      lockFd = openSync(lockPath, "wx");
      writeFileSync(lockFd, JSON.stringify(owner));
      closeSync(lockFd);
      lockFd = null;
    } catch (err) {
      if (lockFd !== null) {
        try {
          closeSync(lockFd);
        } catch {
          // The process no longer needs the descriptor; lock ownership is path-based.
        }
      }
      if ((err as NodeJS.ErrnoException).code !== "EEXIST") throw err;
      if (!isOrphanedCheckLock(lockPath)) return null;

      // Replacing the orphan atomically avoids a remove/create interval in which
      // another process could mistake the lock as available and start a second update.
      const replacementPath = `${lockPath}.tmp.${process.pid}`;
      writeFileSync(replacementPath, JSON.stringify(owner), { encoding: "utf-8", mode: 0o600 });
      try {
        renameSync(replacementPath, lockPath);
      } catch (replaceError) {
        rmSync(replacementPath, { force: true });
        throw replaceError;
      }
    }

    const lock: CheckSlotLock = {
      release: () => {
        // A stalled owner can resume after its lock was reclaimed. Only remove the
        // pathname when it still names this acquisition, not a replacement owner.
        if (checkLockIsOwnedBy(lockPath, owner)) {
          rmSync(lockPath, { force: true });
        }
      },
    };

    try {
      if (hasRecentCheckTimestamp(file, intervalMs)) {
        lock.release();
        return null;
      }

      writeCheckTimestamp(file);
      return lock;
    } catch (err) {
      lock.release();
      throw err;
    }
  } catch (err) {
    warn(`[auto-update-checker] Could not coordinate via timestamp file: ${String(err)}`);
    return null;
  }
}

function isOrphanedCheckLock(lockPath: string): boolean {
  const state = readCheckLockState(lockPath);
  const ageMs = Date.now() - state.startedMs;
  if (ageMs >= CHECK_LOCK_STALE_MS) return true;
  if (state.pid === null) return false;

  try {
    process.kill(state.pid, 0);
    return false;
  } catch (err) {
    const code = (err as NodeJS.ErrnoException).code;
    if (code === "ESRCH") return true;
    if (code === "EPERM") return false;
    throw err;
  }
}

function readCheckLockState(lockPath: string): CheckLockState {
  const fallbackStartedMs = statSync(lockPath).mtimeMs;
  try {
    const parsed = JSON.parse(readFileSync(lockPath, "utf-8")) as Partial<CheckLockOwner>;
    const pid =
      Number.isSafeInteger(parsed.pid) && (parsed.pid ?? 0) > 0 ? (parsed.pid ?? null) : null;
    const startedMs =
      typeof parsed.startedMs === "number" && Number.isFinite(parsed.startedMs)
        ? parsed.startedMs
        : fallbackStartedMs;
    return { pid, startedMs };
  } catch {
    // A crash between exclusive creation and metadata write leaves an empty lock.
    return { pid: null, startedMs: fallbackStartedMs };
  }
}

function checkLockIsOwnedBy(lockPath: string, owner: CheckLockOwner): boolean {
  try {
    const parsed = JSON.parse(readFileSync(lockPath, "utf-8")) as Partial<CheckLockOwner>;
    return parsed.pid === owner.pid && parsed.startedMs === owner.startedMs;
  } catch {
    return false;
  }
}

function hasRecentCheckTimestamp(file: string, intervalMs: number): boolean {
  if (!existsSync(file)) return false;
  try {
    const raw = JSON.parse(readFileSync(file, "utf-8")) as { lastCheckedMs?: unknown };
    const last = typeof raw.lastCheckedMs === "number" ? raw.lastCheckedMs : 0;
    return Number.isFinite(last) && Date.now() - last < intervalMs;
  } catch {
    // Corrupt timestamp file — overwrite it after the lock is acquired.
    return false;
  }
}

function writeCheckTimestamp(file: string): void {
  const tmp = `${file}.tmp.${process.pid}`;
  writeFileSync(tmp, JSON.stringify({ lastCheckedMs: Date.now() }), "utf-8");
  renameSync(tmp, file);
}

/**
 * True only when `candidate` is strictly newer than `installed` by semver.
 * A version string that does not parse as semver is never treated as newer,
 * so a malformed registry answer cannot trigger an install.
 */
export function isNewerVersion(candidate: string, installed: string): boolean {
  const semver = /^\d+\.\d+\.\d+(?:-[0-9A-Za-z.-]+)?(?:\+[0-9A-Za-z.-]+)?$/;
  const strip = (version: string) => version.trim().replace(/^v/, "").split("+", 1)[0];
  const a = strip(candidate);
  const b = strip(installed);
  if (!semver.test(a) || !semver.test(b)) return false;
  return compareSemver(a, b) > 0;
}

async function runStartupCheck(
  ctx: PluginInput,
  options: ResolvedAutoUpdateCheckerOptions,
): Promise<void> {
  if (options.signal.aborted) return;

  const cachedVersion = getCachedVersion();
  const localDevVersion = getLocalDevVersion(ctx.directory);
  const displayVersion = localDevVersion ?? cachedVersion;

  if (localDevVersion) {
    if (options.showStartupToast) {
      showToast(ctx, `AFT ${displayVersion} (dev)`, "Running in local development mode.", "info");
    }
    log("[auto-update-checker] Local development mode");
    return;
  }

  if (options.showStartupToast) {
    showToast(
      ctx,
      `AFT ${displayVersion ?? "unknown"}`,
      "@cortexkit/aft-opencode is active.",
      "info",
    );
  }

  await runBackgroundUpdateCheck(ctx, options);
}

async function runBackgroundUpdateCheck(
  ctx: PluginInput,
  options: ResolvedAutoUpdateCheckerOptions,
): Promise<void> {
  if (options.signal.aborted) return;

  const pluginInfo = findPluginEntry(ctx.directory);
  if (!pluginInfo) {
    log("[auto-update-checker] Plugin not found in config");
    return;
  }

  const cachedVersion = getCachedVersion(pluginInfo.entry);
  const currentVersion = cachedVersion ?? pluginInfo.pinnedVersion;
  if (!currentVersion) {
    log("[auto-update-checker] No version found (cached or pinned)");
    return;
  }

  const channel = extractChannel(pluginInfo.pinnedVersion ?? currentVersion);
  const latestVersion = await getLatestVersion(channel, {
    registryUrl: options.npmRegistryUrl,
    timeoutMs: options.fetchTimeoutMs,
    signal: options.signal,
  });
  if (!latestVersion) {
    warn(`[auto-update-checker] Failed to fetch latest version for channel: ${channel}`);
    showToast(
      ctx,
      "AFT update check failed",
      "Could not check npm for @cortexkit/aft-opencode updates. Continuing with the cached version.",
      "warning",
      8000,
    );
    return;
  }

  // Compare as semver, prerelease included. An installed version at or above
  // the channel's dist-tag (a fresh release before the tag moves, or a
  // prerelease build) is current: offering the tagged version would be a
  // downgrade.
  if (!isNewerVersion(latestVersion, currentVersion)) {
    log(
      `[auto-update-checker] Already on latest version for channel: ${channel} (installed ${currentVersion}, ${channel} ${latestVersion})`,
    );
    return;
  }

  log(`[auto-update-checker] Update available (${channel}): ${currentVersion} → ${latestVersion}`);

  if (pluginInfo.isPinned) {
    showToast(
      ctx,
      `AFT ${latestVersion}`,
      `v${latestVersion} available. Version is pinned; update your OpenCode plugin config to upgrade.`,
      "info",
      8000,
    );
    log("[auto-update-checker] Version is pinned; skipping auto-update");
    return;
  }

  if (!options.autoUpdate) {
    showToast(
      ctx,
      `AFT ${latestVersion}`,
      `v${latestVersion} available. Auto-update is disabled.`,
      "info",
      8000,
    );
    log("[auto-update-checker] Auto-update disabled, notification only");
    return;
  }

  // Pre-flight: confirm npm is runnable BEFORE preparePackageUpdate() deletes
  // the installed package and lock entry. Without this, a GUI launch with no
  // npm on PATH would delete the working package, fail to reinstall, and depend
  // on the restore-on-failure snapshot to recover — a crash/quit in that window
  // would brick the plugin. If npm is unreachable, skip the destructive steps
  // entirely and tell the user how to fix it.
  if (!isNpmAvailable()) {
    showToast(
      ctx,
      `AFT ${latestVersion} available`,
      `v${latestVersion} is ready, but npm was not found so it can't auto-install. Run \`npx @cortexkit/aft doctor --fix\` from a terminal, or launch OpenCode from a shell where npm is on PATH.`,
      "warning",
      10000,
    );
    warn(
      "[auto-update-checker] npm not found on PATH or known version-manager locations; skipping destructive auto-update steps",
    );
    return;
  }

  const installDir = preparePackageUpdate(latestVersion, PACKAGE_NAME);
  if (!installDir) {
    showToast(
      ctx,
      `AFT ${latestVersion}`,
      `v${latestVersion} available. Auto-update could not prepare the active install.`,
      "warning",
      8000,
    );
    warn("[auto-update-checker] Failed to prepare install root for auto-update");
    return;
  }

  const installResult = await runNpmInstallSafe(installDir, { signal: options.signal });
  if (installResult.ok) {
    showToast(
      ctx,
      "AFT Updated!",
      `v${currentVersion} → v${latestVersion}\nRestart OpenCode to apply.`,
      "success",
      8000,
    );
    log(`[auto-update-checker] Update installed: ${currentVersion} → ${latestVersion}`);
    return;
  }

  showToast(
    ctx,
    `AFT ${latestVersion}`,
    `v${latestVersion} available, but auto-update failed to install it. Check logs or retry manually.`,
    "error",
    8000,
  );
  const failureDetail = installResult.reason ? `: ${installResult.reason}` : "";
  const stderrDetail = installResult.stderrTail
    ? `\nstderr tail:\n${installResult.stderrTail}`
    : "";
  warn(
    `[auto-update-checker] npm install failed; update not installed${failureDetail}${stderrDetail}`,
  );
}

export function getAutoUpdateInstallDir(): string {
  return resolveInstallContext()?.installDir ?? cacheDir();
}

function showToast(
  ctx: PluginInput,
  title: string,
  message: string,
  variant: ToastVariant = "info",
  duration = 3000,
): void {
  const tui = ctx.client.tui;
  if (typeof tui?.showToast !== "function") return;
  tui.showToast({ body: { title, message, variant, duration } }).catch(() => {});
}

export const __test__ = { claimCheckSlot };

export type { AutoUpdateCheckerOptions } from "./types.js";
