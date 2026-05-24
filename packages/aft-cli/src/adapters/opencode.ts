import { execSync } from "node:child_process";
import { existsSync, readFileSync, rmSync, statSync } from "node:fs";
import { homedir } from "node:os";
import { dirname, join, parse, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { dirSize } from "../lib/fs-util.js";
import { detectJsoncFile, readJsoncFile, writeJsoncFile } from "../lib/jsonc.js";
import { getTmpLogPath } from "../lib/paths.js";
import { getSelfVersion } from "../lib/self-version.js";
import type {
  HarnessAdapter,
  HarnessConfigPaths,
  PluginCacheInfo,
  PluginEntryResult,
} from "./types.js";

const PLUGIN_NAME = "@cortexkit/aft-opencode";
const PLUGIN_ENTRY = `${PLUGIN_NAME}@latest`;

function getOpenCodeConfigDir(): string {
  const envDir = process.env.OPENCODE_CONFIG_DIR?.trim();
  if (envDir) return resolve(envDir);
  const xdg = process.env.XDG_CONFIG_HOME || join(homedir(), ".config");
  return join(xdg, "opencode");
}

function getOpenCodeCacheDir(): string {
  const xdg = process.env.XDG_CACHE_HOME;
  if (xdg) return join(xdg, "opencode");
  if (process.platform === "win32") {
    const localAppData = process.env.LOCALAPPDATA ?? join(homedir(), "AppData", "Local");
    return join(localAppData, "opencode");
  }
  return join(homedir(), ".cache", "opencode");
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

  isInstalled(): boolean {
    try {
      execSync("opencode --version", { stdio: "ignore" });
      return true;
    } catch {
      return false;
    }
  }

  getHostVersion(): string | null {
    try {
      return execSync("opencode --version", { encoding: "utf-8", stdio: "pipe" }).trim();
    } catch {
      return null;
    }
  }

  detectConfigPaths(): HarnessConfigPaths {
    const configDir = getOpenCodeConfigDir();
    const harness = detectJsoncFile(configDir, "opencode");
    const aft = detectJsoncFile(configDir, "aft");
    const tui = detectJsoncFile(configDir, "tui");
    return {
      configDir,
      harnessConfig: harness.path,
      harnessConfigFormat: harness.format,
      aftConfig: aft.path,
      aftConfigFormat: aft.format,
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
    const configPath = paths.harnessConfig;

    if (paths.harnessConfigFormat === "none") {
      // No existing file — create one with the plugin entry.
      const initial = { plugin: [PLUGIN_ENTRY] };
      writeJsoncFile(configPath, initial, "json");
      return {
        ok: true,
        action: "added",
        message: `Created ${configPath} and added ${PLUGIN_ENTRY}`,
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

    const plugins = Array.isArray(value.plugin) ? [...value.plugin] : [];
    const already = plugins.some((entry) => typeof entry === "string" && matchesPluginEntry(entry));
    if (already) {
      return {
        ok: true,
        action: "already_present",
        message: `${PLUGIN_NAME} is already registered in ${configPath}`,
        configPath,
      };
    }

    plugins.push(PLUGIN_ENTRY);
    const updated = { ...value, plugin: plugins };
    writeJsoncFile(configPath, updated, paths.harnessConfigFormat);
    return {
      ok: true,
      action: "added",
      message: `Added ${PLUGIN_ENTRY} to ${configPath}`,
      configPath,
    };
  }

  getPluginCacheInfo(): PluginCacheInfo {
    const path = join(getOpenCodeCacheDir(), "packages", PLUGIN_ENTRY);
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
    const xdg = process.env.XDG_DATA_HOME || join(homedir(), ".local", "share");
    return join(xdg, "opencode", "storage", "plugin", "aft");
  }

  getLogFile(): string {
    return getTmpLogPath("aft-plugin.log");
  }

  getInstallHint(): string {
    return "Install OpenCode: https://opencode.ai/docs/install";
  }

  async clearPluginCache(force: boolean): Promise<{
    action: "cleared" | "up_to_date" | "not_found" | "not_applicable" | "error";
    path: string;
    cached?: string;
    latest?: string;
    error?: string;
  }> {
    const info = this.getPluginCacheInfo();
    if (!info.exists) {
      return { action: "not_found", path: info.path };
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
    };
  }
}
