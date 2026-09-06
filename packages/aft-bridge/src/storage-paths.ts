import { homedir } from "node:os";
import { join, resolve } from "node:path";

export type StoragePlatform = "windows" | "other";
export type StorageEnvironmentLookup = (name: string) => string | undefined;

export interface StoragePathContext {
  lookup?: StorageEnvironmentLookup;
  platform?: StoragePlatform;
  fallbackHome?: string;
  currentDirectory?: string;
}

function environmentValue(context: StoragePathContext, name: string): string | undefined {
  const value = (context.lookup ?? ((key) => process.env[key]))(name);
  return value !== undefined && value.length > 0 ? value : undefined;
}

function storagePlatform(context: StoragePathContext): StoragePlatform {
  return context.platform ?? (process.platform === "win32" ? "windows" : "other");
}

function homeDir(context: StoragePathContext): string {
  const configured =
    storagePlatform(context) === "windows"
      ? (environmentValue(context, "USERPROFILE") ?? environmentValue(context, "HOME"))
      : (environmentValue(context, "HOME") ?? environmentValue(context, "USERPROFILE"));
  return configured ?? context.fallbackHome ?? homedir();
}

/**
 * Resolve the CortexKit data-home ladder before appending a module directory.
 *
 * Last re-derived 2026-09-06 against subconscious d5e09914b0791a66f2a5a00a9bb3422860ade95e:
 * compare ordered variables, platform gates, and empty-value guards with
 * `subc-core/src/daemon_config.rs::default_data_home`, resolve named constants,
 * then preserve the documented Windows cache-class divergence.
 */
export function resolveDataHome(context: StoragePathContext = {}): string {
  const xdg = environmentValue(context, "XDG_DATA_HOME");
  if (xdg) return xdg;
  if (storagePlatform(context) === "windows") {
    // AFT stores indexes, backups, and checkpoints here.
    // cache-class storage; stable for existing installs.
    // Do not move this shipped ladder to Roaming.
    const localAppData = environmentValue(context, "LOCALAPPDATA");
    if (localAppData) return localAppData;
    const userProfile = environmentValue(context, "USERPROFILE");
    if (userProfile) return join(userProfile, "AppData", "Local");
  }
  const home = environmentValue(context, "HOME");
  if (home) return join(home, ".local", "share");
  return join(".local", "share");
}

/**
 * Expand the supported storage-root spellings and anchor them to this process's
 * cwd. Every caller receives one absolute spelling, so a bridge and a plugin
 * cannot select different roots merely because their cwd differs.
 */
export function resolveStoragePath(raw: string, context: StoragePathContext = {}): string {
  let expanded = raw;
  if (raw === "~") {
    expanded = homeDir(context);
  } else if (raw.startsWith("~/") || raw.startsWith("~\\")) {
    expanded = join(homeDir(context), raw.slice(2));
  }
  return resolve(context.currentDirectory ?? process.cwd(), expanded);
}

/** Resolve the shared CortexKit storage root used by every plugin host. */
export function resolveCortexKitStorageRoot(context: StoragePathContext = {}): string {
  const override = environmentValue(context, "AFT_STORAGE_DIR");
  if (override) return resolveStoragePath(override, context);
  const legacyCache = environmentValue(context, "AFT_CACHE_DIR");
  if (legacyCache) return join(resolveStoragePath(legacyCache, context), "aft");
  return resolveStoragePath(join(resolveDataHome(context), "cortexkit", "aft"), context);
}

/**
 * Resolve a process-state storage root. AFT_STORAGE_DIR is checked here rather
 * than at injection time so it wins over a stale or plugin-injected wire value.
 */
export function resolveAftStorageRoot(
  configuredRoot?: string,
  context: StoragePathContext = {},
): string {
  if (environmentValue(context, "AFT_STORAGE_DIR")) {
    return resolveCortexKitStorageRoot(context);
  }
  // Explicit process-state paths are caller-owned; preserve their spelling so
  // every downstream read/write uses the exact configured root.
  if (configuredRoot !== undefined && configuredRoot.length > 0) return configuredRoot;
  return resolveCortexKitStorageRoot(context);
}

export function resolveAftLogPath(
  filename: string,
  configuredRoot?: string,
  context: StoragePathContext = {},
): string {
  return join(resolveAftStorageRoot(configuredRoot, context), "logs", filename);
}
