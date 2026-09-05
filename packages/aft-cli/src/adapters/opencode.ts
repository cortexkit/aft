import { existsSync, readFileSync, rmSync, statSync } from "node:fs";
import { homedir } from "node:os";
import { dirname, join, parse, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import {
  getOpenCodeCacheRoot,
  getOpenCodeConfigRoot,
  resolveAftLogPath,
  resolveCortexKitUserConfigPath,
} from "@cortexkit/aft-bridge";

import { dirSize } from "../lib/fs-util.js";
import { detectJsoncFile, readJsoncFile, writeJsoncFile } from "../lib/jsonc.js";
import { getCortexKitStorageRoot } from "../lib/paths.js";
import { getSelfVersion } from "../lib/self-version.js";
import {
  detectOpenCodeHostGeneration,
  findExecutableOnPath,
  type OpenCodeHostDetection,
} from "../setup/host-generation.js";
import {
  AFT_OPENCODE_PACKAGE,
  ensurePinnedPluginConfig,
  isAftNpmEntry,
  pinnedPluginEntry,
  pluginConfigNeedsUpdate,
} from "../setup/opencode-config.js";
import type {
  HarnessAdapter,
  HarnessConfigPaths,
  PluginCacheInfo,
  PluginEntryResult,
} from "./types.js";

const PLUGIN_NAME = AFT_OPENCODE_PACKAGE;
const PLUGIN_ENTRY = pinnedPluginEntry(getSelfVersion());
const LEGACY_PLUGIN_ENTRY = `${PLUGIN_NAME}@latest`;

function getOpenCodeConfigDir(): string {
  const envDir = process.env.OPENCODE_CONFIG_DIR?.trim();
  if (envDir) return resolve(envDir);
  return getOpenCodeConfigRoot();
}

function getLegacyOpenCodePluginCachePath(primaryPath: string): string | null {
  if (process.platform !== "win32") return null;
  const localAppData = process.env.LOCALAPPDATA ?? join(homedir(), "AppData", "Local");
  const legacyPath = join(localAppData, "opencode", "packages", LEGACY_PLUGIN_ENTRY);
  return resolve(legacyPath) === resolve(primaryPath) ? null : legacyPath;
}

function clearLegacyOpenCodePluginCache(primaryPath: string): {
  clearedPath: string | null;
  error?: string;
} {
  const legacyPath = getLegacyOpenCodePluginCachePath(primaryPath);
  if (!legacyPath || !existsSync(legacyPath)) return { clearedPath: null };
  try {
    rmSync(legacyPath, { recursive: true, force: true });
    return { clearedPath: legacyPath };
  } catch (error) {
    return { clearedPath: null, error: error instanceof Error ? error.message : String(error) };
  }
}

function getOpenCodeCacheDir(): string {
  return getOpenCodeCacheRoot();
}

/** True when either generation's CLI is present on PATH. */
function hasOpenCodeCli(): boolean {
  return Boolean(findExecutableOnPath("opencode") ?? findExecutableOnPath("opencode2"));
}

/**
 * True when a known OpenCode Desktop or package-install marker exists.
 * Used when the config directory has not been created yet, such as after a fresh
 * install that has not been launched.
 */
function openCodeDesktopAppExists(): boolean {
  const candidates: string[] = [];
  if (process.platform === "darwin") {
    candidates.push(
      "/Applications/OpenCode.app",
      "/Applications/OpenCode Beta.app",
      join(homedir(), "Applications", "OpenCode.app"),
      join(homedir(), "Applications", "OpenCode Beta.app"),
    );
  } else if (process.platform === "win32") {
    const localAppData = process.env.LOCALAPPDATA ?? join(homedir(), "AppData", "Local");
    candidates.push(join(localAppData, "Programs", "opencode"), join(localAppData, "opencode"));
  } else {
    // Linux: common AppImage / package install hints.
    candidates.push(
      "/opt/OpenCode",
      "/usr/lib/opencode",
      join(homedir(), ".local", "share", "applications", "opencode.desktop"),
    );
  }
  return candidates.some((p) => {
    try {
      return existsSync(p);
    } catch {
      return false;
    }
  });
}

/**
 * Convert a plugin entry string to a filesystem path if it represents one.
 *
 * Plugin entries may be:
 * - npm package names: `@cortexkit/aft-opencode` (returns null)
 * - npm package@version: `@cortexkit/aft-opencode@latest` (returns null)
 * - file URLs: `file:///path/to/dir` (returns the resolved path)
 * - absolute Unix paths: `/Users/x/work/aft` (returns as-is)
 * - absolute Windows paths: `F:\path\to\plugin` or `C:/path/to/plugin` (returns as-is)
 */
function pathFromEntry(entry: string): string | null {
  if (entry.startsWith("file://")) {
    try {
      return fileURLToPath(entry);
    } catch {
      return null;
    }
  }
  if (entry.startsWith("/") || /^[A-Za-z]:[/\\]/.test(entry)) return entry;
  return null;
}

/**
 * Verify a path entry resolves to our actual plugin package by reading its
 * package.json and checking the name field. Required because the previous
 * substring-based heuristic (`includes("/opencode-plugin")`) produced false
 * positives for unrelated third-party plugins whose paths happened to contain
 * "opencode-plugin" — for example a user with
 * `file:///F:/hackingtool-plugin/opencode-plugin` in their config would have
 * AFT report itself as registered when it wasn't.
 */
function pathPointsToOurPlugin(entry: string): boolean {
  const fsPath = pathFromEntry(entry);
  if (!fsPath) return false;
  try {
    if (!existsSync(fsPath)) return false;
    let searchDir = statSync(fsPath).isDirectory() ? fsPath : dirname(fsPath);
    let pkgJsonPath: string | null = null;
    while (true) {
      const candidate = join(searchDir, "package.json");
      if (existsSync(candidate)) {
        pkgJsonPath = candidate;
        break;
      }
      const parent = dirname(searchDir);
      if (parent === searchDir || searchDir === parse(searchDir).root) break;
      searchDir = parent;
    }
    if (!pkgJsonPath) return false;
    const parsed = JSON.parse(readFileSync(pkgJsonPath, "utf-8")) as { name?: unknown };
    return parsed.name === PLUGIN_NAME;
  } catch {
    return false;
  }
}

function matchesPluginEntry(entry: string): boolean {
  if (entry === PLUGIN_NAME) return true;
  if (entry.startsWith(`${PLUGIN_NAME}@`)) return true;
  return pathPointsToOurPlugin(entry);
}

export class OpenCodeAdapter implements HarnessAdapter {
  readonly kind = "opencode" as const;
  readonly displayName = "OpenCode";
  readonly pluginPackageName = PLUGIN_NAME;
  readonly pluginEntryWithVersion = PLUGIN_ENTRY;
  private hostDetection: OpenCodeHostDetection | undefined;

  isInstalled(): boolean {
    // Treat the configured root, known installation markers, or either CLI as
    // evidence that OpenCode is installed. Check filesystem signals before PATH
    // discovery so Desktop-only users can run setup without booting the host; a
    // prior self-resolution probe was slow and could recurse into this CLI.
    if (existsSync(getOpenCodeConfigDir())) return true;
    // App bundle exists but config dir not yet created (freshly installed,
    // never launched).
    if (openCodeDesktopAppExists()) return true;
    // Last resort: the CLI is on PATH but hasn't created a config dir yet.
    return hasOpenCodeCli();
  }

  detectHostGeneration(): OpenCodeHostDetection {
    this.hostDetection ??= detectOpenCodeHostGeneration();
    return this.hostDetection;
  }

  getHostVersion(): string | null {
    const versions = this.detectHostGeneration()
      .evidence.map((item) => item.version)
      .filter((version): version is string => Boolean(version));
    return versions.length > 0 ? versions.join(" + ") : null;
  }

  detectConfigPaths(): HarnessConfigPaths {
    const configDir = getOpenCodeConfigDir();
    const harness = detectJsoncFile(configDir, "opencode");
    // AFT config lives in the shared CortexKit location since the v0.40.0
    // consolidation, not the per-harness opencode config dir. Use the bridge's
    // canonical path so the CLI and the plugin agree byte-for-byte (and so a
    // fresh `setup` creates aft.jsonc, the only name the plugin reads).
    const aftConfigPath = resolveCortexKitUserConfigPath();
    const aftConfigExists = existsSync(aftConfigPath);
    const tui = detectJsoncFile(configDir, "tui");
    return {
      configDir,
      harnessConfig: harness.path,
      harnessConfigFormat: harness.format,
      aftConfig: aftConfigPath,
      aftConfigFormat: aftConfigExists ? "jsonc" : "none",
      tuiConfig: tui.path,
      tuiConfigFormat: tui.format,
    };
  }

  hasPluginEntry(): boolean {
    const paths = this.detectConfigPaths();
    const { value } = readJsoncFile(paths.harnessConfig);
    const plugins = Array.isArray(value?.plugin) ? value.plugin : [];
    return plugins.some((entry) => typeof entry === "string" && matchesPluginEntry(entry));
  }

  async ensurePluginEntry(): Promise<PluginEntryResult> {
    const paths = this.detectConfigPaths();
    return this.ensureConfigEntry(paths.harnessConfig, paths.harnessConfigFormat, "server config");
  }

  needsPluginEntryUpdate(): boolean {
    const paths = this.detectConfigPaths();
    return this.configEntryNeedsUpdate(paths.harnessConfig, paths.harnessConfigFormat);
  }

  hasTuiPluginEntry(): boolean {
    const paths = this.detectConfigPaths();
    if (!paths.tuiConfig) return false;
    const { value } = readJsoncFile(paths.tuiConfig);
    const plugins = Array.isArray(value?.plugin) ? value.plugin : [];
    return plugins.some((entry) => typeof entry === "string" && matchesPluginEntry(entry));
  }

  /**
   * Register the TUI sidebar plugin in tui.json(c). Only setup/doctor call
   * this: the plugin itself must never auto-inject the entry at load time,
   * because that silently undoes a user's deliberate removal on every launch.
   */
  async ensureTuiPluginEntry(): Promise<PluginEntryResult> {
    const paths = this.detectConfigPaths();
    if (!paths.tuiConfig) {
      return {
        ok: false,
        action: "error",
        message: "No TUI config path detected",
        configPath: paths.configDir,
      };
    }
    return this.ensureConfigEntry(paths.tuiConfig, paths.tuiConfigFormat ?? "none", "TUI config");
  }

  needsTuiPluginEntryUpdate(): boolean {
    const paths = this.detectConfigPaths();
    return paths.tuiConfig
      ? this.configEntryNeedsUpdate(paths.tuiConfig, paths.tuiConfigFormat ?? "none")
      : true;
  }

  private ensureConfigEntry(
    configPath: string,
    format: HarnessConfigPaths["harnessConfigFormat"],
    label: string,
  ): PluginEntryResult {
    if (format === "none") {
      writeJsoncFile(configPath, { plugin: [PLUGIN_ENTRY] }, "json");
      return {
        ok: true,
        action: "added",
        message: `Created ${configPath} and added ${PLUGIN_ENTRY} (${label})`,
        configPath,
      };
    }

    const { value, error } = readJsoncFile(configPath);
    if (error || !value) {
      return {
        ok: false,
        action: "error",
        message: `Could not parse ${configPath}: ${error ?? "unknown error"}`,
        configPath,
      };
    }

    const update = ensurePinnedPluginConfig(value, getSelfVersion(), pathPointsToOurPlugin);
    if (!update.changed) {
      return {
        ok: true,
        action: "already_present",
        message: `${PLUGIN_ENTRY} is already registered in ${configPath}`,
        configPath,
      };
    }

    writeJsoncFile(configPath, value, format);
    return {
      ok: true,
      action: update.action,
      message: `${update.action === "added" ? "Added" : "Updated"} ${PLUGIN_ENTRY} in ${configPath} (${label})`,
      configPath,
    };
  }

  private configEntryNeedsUpdate(
    configPath: string,
    format: HarnessConfigPaths["harnessConfigFormat"],
  ): boolean {
    if (format === "none") return true;
    const { value, error } = readJsoncFile(configPath);
    if (error || !value) return false;
    return pluginConfigNeedsUpdate(value, getSelfVersion(), pathPointsToOurPlugin);
  }

  getPluginCacheInfo(): PluginCacheInfo {
    const configPath = this.detectConfigPaths().harnessConfig;
    const { value } = readJsoncFile(configPath);
    const configuredEntry = Array.isArray(value?.plugin)
      ? value.plugin.find(isAftNpmEntry)
      : undefined;
    const path = join(getOpenCodeCacheDir(), "packages", configuredEntry ?? PLUGIN_ENTRY);
    let cached: string | undefined;
    try {
      const installedPkgPath = join(
        path,
        "node_modules",
        "@cortexkit",
        "aft-opencode",
        "package.json",
      );
      if (existsSync(installedPkgPath)) {
        const pkg = JSON.parse(readFileSync(installedPkgPath, "utf-8")) as { version?: unknown };
        cached = typeof pkg.version === "string" ? pkg.version : undefined;
      }
    } catch {
      cached = undefined;
    }
    return {
      path,
      cached,
      latest: getSelfVersion(),
      exists: existsSync(path),
    };
  }

  getStorageDir(): string {
    return getCortexKitStorageRoot();
  }

  getLogFile(): string {
    return resolveAftLogPath("aft-plugin.log");
  }

  getInstallHint(): string {
    return "Install OpenCode: https://opencode.ai/docs/install";
  }

  async clearPluginCache(force: boolean): Promise<{
    action:
      | "cleared"
      | "legacy_path_cleared"
      | "up_to_date"
      | "not_found"
      | "not_applicable"
      | "error";
    path: string;
    cached?: string;
    latest?: string;
    error?: string;
    legacy_path_cleared?: string;
  }> {
    const info = this.getPluginCacheInfo();
    const clearLegacy = force ? clearLegacyOpenCodePluginCache(info.path) : { clearedPath: null };
    if (clearLegacy.error) {
      return {
        action: "error",
        path: info.path,
        error: `Could not clear legacy OpenCode cache: ${clearLegacy.error}`,
        ...(clearLegacy.clearedPath ? { legacy_path_cleared: clearLegacy.clearedPath } : {}),
      };
    }
    if (!info.exists) {
      return clearLegacy.clearedPath
        ? {
            action: "legacy_path_cleared",
            path: clearLegacy.clearedPath,
            legacy_path_cleared: clearLegacy.clearedPath,
          }
        : { action: "not_found", path: info.path };
    }
    if (!force && info.cached && info.cached === info.latest) {
      return {
        action: "up_to_date",
        path: info.path,
        cached: info.cached,
        latest: info.latest,
      };
    }
    try {
      rmSync(info.path, { recursive: true, force: true });
      return {
        action: "cleared",
        path: info.path,
        cached: info.cached,
        latest: info.latest,
        ...(clearLegacy.clearedPath ? { legacy_path_cleared: clearLegacy.clearedPath } : {}),
      };
    } catch (error) {
      return {
        action: "error",
        path: info.path,
        cached: info.cached,
        latest: info.latest,
        error: error instanceof Error ? error.message : String(error),
      };
    }
  }

  /** Exposed for diagnostic reporting — harness-specific side data. */
  getOpenCodeCacheDir(): string {
    return getOpenCodeCacheDir();
  }

  /** For doctor: directory size helpers for each storage subtree. */
  describeStorageSubtrees(): Record<string, number> {
    const storage = this.getStorageDir();
    return {
      index: dirSize(join(storage, "index")),
      semantic: dirSize(join(storage, "semantic")),
      backups: dirSize(join(storage, "backups")),
      url_cache: dirSize(join(storage, "url_cache")),
      onnxruntime: dirSize(join(storage, "onnxruntime")),
      logs: dirSize(join(storage, "logs")),
    };
  }
}
