import { existsSync, readFileSync } from "node:fs";
import { join } from "node:path";
import { resolveAftLogPath, resolveCortexKitUserConfigPath } from "@cortexkit/aft-bridge";

import { dirSize } from "../lib/fs-util.js";
import {
  detectOmpBinary,
  getOmpVersion,
  listOmpPlugins,
  OMP_PLUGIN_PACKAGE,
  runOmpCommand,
} from "../lib/omp-helpers.js";
import { getOmpAgentDir, getOmpPluginsDir, getOmpPluginsLockPath } from "../lib/omp-paths.js";
import { getCortexKitStorageRoot } from "../lib/paths.js";
import type {
  HarnessAdapter,
  HarnessConfigPaths,
  PluginCacheInfo,
  PluginEntryResult,
} from "./types.js";

export interface OmpAdapterDeps {
  detectOmpBinary: typeof detectOmpBinary;
  getOmpVersion: typeof getOmpVersion;
  listOmpPlugins: typeof listOmpPlugins;
  runOmpCommand: typeof runOmpCommand;
}

const DEFAULT_DEPS: OmpAdapterDeps = {
  detectOmpBinary,
  getOmpVersion,
  listOmpPlugins,
  runOmpCommand,
};

export class OmpAdapter implements HarnessAdapter {
  readonly kind = "omp" as const;
  readonly displayName = "Oh My Pi (OMP)";
  readonly pluginPackageName = OMP_PLUGIN_PACKAGE;
  readonly pluginEntryWithVersion = OMP_PLUGIN_PACKAGE;
  private readonly deps: OmpAdapterDeps;

  constructor(deps: Partial<OmpAdapterDeps> = {}) {
    this.deps = { ...DEFAULT_DEPS, ...deps };
  }

  isInstalled(): boolean {
    return this.deps.detectOmpBinary() !== null;
  }

  getHostVersion(): string | null {
    const omp = this.deps.detectOmpBinary();
    return omp ? this.deps.getOmpVersion(omp.path) : null;
  }

  detectConfigPaths(): HarnessConfigPaths {
    const configDir = getOmpAgentDir();
    const harnessConfig = getOmpPluginsLockPath();
    const aftConfig = resolveCortexKitUserConfigPath();
    return {
      configDir,
      harnessConfig,
      harnessConfigFormat: existsSync(harnessConfig) ? "json" : "none",
      aftConfig,
      aftConfigFormat: existsSync(aftConfig) ? "jsonc" : "none",
    };
  }

  hasPluginEntry(): boolean {
    const omp = this.deps.detectOmpBinary();
    if (!omp) return false;
    return (
      this.deps
        .listOmpPlugins(omp.path)
        ?.some((plugin) => plugin.name === OMP_PLUGIN_PACKAGE && plugin.enabled) ?? false
    );
  }

  async ensurePluginEntry(): Promise<PluginEntryResult> {
    const configPath = getOmpPluginsLockPath();
    const omp = this.deps.detectOmpBinary();
    if (!omp) return this.errorResult(configPath, "OMP binary not found");

    const plugins = this.deps.listOmpPlugins(omp.path);
    if (plugins === null) {
      return this.errorResult(configPath, "`omp plugin list --json` failed");
    }
    const installed = plugins.find((plugin) => plugin.name === OMP_PLUGIN_PACKAGE);
    if (installed?.enabled) {
      return {
        ok: true,
        action: "already_present",
        message: `${OMP_PLUGIN_PACKAGE} is already enabled in OMP.`,
        configPath,
      };
    }

    const originalRuntimeEnabled = this.readRuntimeEnabled(configPath);
    const args = installed
      ? ["plugin", "enable", OMP_PLUGIN_PACKAGE]
      : ["plugin", "install", OMP_PLUGIN_PACKAGE];
    const result = this.deps.runOmpCommand(omp.path, args, 120_000);
    if (!result.ok) {
      return this.errorResult(
        configPath,
        result.stderr || result.stdout || `omp ${args.join(" ")} failed`,
      );
    }

    const enabledAfter = this.deps
      .listOmpPlugins(omp.path)
      ?.some((plugin) => plugin.name === OMP_PLUGIN_PACKAGE && plugin.enabled);
    if (!enabledAfter) {
      // Project configuration can override the global enable state even after a
      // successful host command, so undo only the global change made above.
      if (!installed) {
        this.deps.runOmpCommand(omp.path, ["plugin", "uninstall", OMP_PLUGIN_PACKAGE], 120_000);
      } else if (originalRuntimeEnabled !== undefined) {
        this.deps.runOmpCommand(
          omp.path,
          ["plugin", originalRuntimeEnabled ? "enable" : "disable", OMP_PLUGIN_PACKAGE],
          120_000,
        );
      }
      return this.errorResult(
        configPath,
        `${OMP_PLUGIN_PACKAGE} is still disabled in the current project after \`omp ${args.join(" ")}\``,
      );
    }

    return {
      ok: true,
      action: installed ? "updated" : "added",
      message: installed
        ? `Enabled ${OMP_PLUGIN_PACKAGE} in OMP.`
        : `Installed ${OMP_PLUGIN_PACKAGE} in OMP.`,
      configPath,
    };
  }

  async removePluginEntry(): Promise<PluginEntryResult> {
    const configPath = getOmpPluginsLockPath();
    const omp = this.deps.detectOmpBinary();
    if (!omp) return this.errorResult(configPath, "OMP binary not found");

    const plugins = this.deps.listOmpPlugins(omp.path);
    if (plugins === null) {
      return this.errorResult(configPath, "`omp plugin list --json` failed");
    }
    if (!plugins.some((plugin) => plugin.name === OMP_PLUGIN_PACKAGE)) {
      return {
        ok: true,
        action: "already_present",
        message: `${OMP_PLUGIN_PACKAGE} is not installed in OMP.`,
        configPath,
      };
    }

    const result = this.deps.runOmpCommand(
      omp.path,
      ["plugin", "uninstall", OMP_PLUGIN_PACKAGE],
      120_000,
    );
    if (!result.ok) {
      return this.errorResult(
        configPath,
        result.stderr || result.stdout || "OMP plugin uninstall failed",
      );
    }
    return {
      ok: true,
      action: "updated",
      message: `Uninstalled ${OMP_PLUGIN_PACKAGE} from OMP.`,
      configPath,
    };
  }

  getInstalledPluginVersion(): string | null {
    const omp = this.deps.detectOmpBinary();
    if (!omp) return null;
    return (
      this.deps.listOmpPlugins(omp.path)?.find((plugin) => plugin.name === OMP_PLUGIN_PACKAGE)
        ?.version ?? null
    );
  }

  getPluginCacheInfo(): PluginCacheInfo {
    const omp = this.deps.detectOmpBinary();
    const plugin = omp
      ? this.deps.listOmpPlugins(omp.path)?.find((entry) => entry.name === OMP_PLUGIN_PACKAGE)
      : undefined;
    const path = plugin?.path ?? getOmpPluginsDir();
    return {
      path,
      cached: plugin?.version,
      latest: undefined,
      exists: Boolean(plugin) || existsSync(path),
    };
  }

  getStorageDir(): string {
    return getCortexKitStorageRoot();
  }

  getLogFile(): string {
    return resolveAftLogPath("aft-plugin.log");
  }

  getLogPath(): string {
    return this.getLogFile();
  }

  getInstallHint(): string {
    return "Install OMP: https://omp.sh (npm: @oh-my-pi/pi-coding-agent)";
  }

  async clearPluginCache(_force: boolean): Promise<{
    action: "not_applicable";
    path: string;
  }> {
    return {
      action: "not_applicable",
      path: this.getPluginCacheInfo().path,
    };
  }

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

  private readRuntimeEnabled(configPath: string): boolean | undefined {
    try {
      const lock = JSON.parse(readFileSync(configPath, "utf8")) as {
        plugins?: Record<string, { enabled?: unknown }>;
      };
      const enabled = lock.plugins?.[OMP_PLUGIN_PACKAGE]?.enabled;
      return typeof enabled === "boolean" ? enabled : undefined;
    } catch {
      return undefined;
    }
  }

  private errorResult(configPath: string, message: string): PluginEntryResult {
    return {
      ok: false,
      action: "error",
      message: `Failed to configure OMP: ${message}`,
      configPath,
    };
  }
}
