import { existsSync, mkdirSync, readFileSync, renameSync, writeFileSync } from "node:fs";
import { dirname, join } from "node:path";
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
import { CACHE_DIR, NPM_FETCH_TIMEOUT, NPM_REGISTRY_URL, PACKAGE_NAME } from "./constants.js";
import type { AutoUpdateCheckerOptions } from "./types.js";

type OpenCodeEvent = {
  type: string;
  properties?: unknown;
};

type ToastVariant = "info" | "warning" | "error" | "success";

type ResolvedAutoUpdateCheckerOptions = Required<
  Omit<AutoUpdateCheckerOptions, "enabled" | "storageDir">
> & { storageDir: string | null };

const DEFAULT_CHECK_INTERVAL_MS = 60 * 60 * 1000; // 1 hour
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
 * `<storageDir>/last-update-check.json`. Every plugin instance reads
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
  // instance recently checked, skip silently.
  if (!claimCheckSlot(options.storageDir, options.checkIntervalMs)) {
    log("[auto-update-checker] Skipping check (another instance ran one recently)");
    return;
  }

  await runStartupCheck(ctx, options);
}

/**
 * Try to claim the next check slot via the on-disk timestamp file.
 *
 * Returns true if this caller should run the check. Returns false if
 * another instance already claimed the slot inside `intervalMs` of now,
 * or if the storage directory isn't usable (we fail open in that case
 * by returning true — the worst outcome is a duplicate npm hit, not a
 * missed check).
 *
 * Race semantics: read → check window → write. With concurrent plugin
 * inits, two callers can race here and both pass the window check before
 * either writes. That's tolerable: at worst we hit npm twice in one
 * launch. The atomic temp+rename write ensures the file is always
 * fully-formed JSON for the next read, even mid-race.
 */
function claimCheckSlot(storageDir: string | null, intervalMs: number): boolean {
  if (!storageDir) return true; // No storage available — fail open.
  try {
    const file = join(storageDir, TIMESTAMP_FILENAME);
    if (existsSync(file)) {
      try {
        const raw = JSON.parse(readFileSync(file, "utf-8")) as { lastCheckedMs?: unknown };
        const last = typeof raw.lastCheckedMs === "number" ? raw.lastCheckedMs : 0;
        if (Number.isFinite(last) && Date.now() - last < intervalMs) {
          return false;
        }
      } catch {
        // Corrupt timestamp file — overwrite it below.
      }
    }
    mkdirSync(dirname(file), { recursive: true });
    const tmp = `${file}.tmp.${process.pid}`;
    writeFileSync(tmp, JSON.stringify({ lastCheckedMs: Date.now() }), "utf-8");
    renameSync(tmp, file);
    return true;
  } catch (err) {
    warn(`[auto-update-checker] Could not coordinate via timestamp file: ${String(err)}`);
    return true;
  }
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

  if (currentVersion === latestVersion) {
    log(`[auto-update-checker] Already on latest version for channel: ${channel}`);
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

  const installSuccess = await runNpmInstallSafe(installDir, { signal: options.signal });
  if (installSuccess) {
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
  warn("[auto-update-checker] npm install failed; update not installed");
}

export function getAutoUpdateInstallDir(): string {
  return resolveInstallContext()?.installDir ?? CACHE_DIR;
}

function showToast(
  ctx: PluginInput,
  title: string,
  message: string,
  variant: ToastVariant = "info",
  duration = 3000,
): void {
  ctx.client.tui.showToast({ body: { title, message, variant, duration } }).catch(() => {});
}

export type { AutoUpdateCheckerOptions } from "./types.js";
