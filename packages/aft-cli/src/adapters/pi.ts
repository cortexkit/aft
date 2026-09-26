import { existsSync, readFileSync } from "node:fs";
import { homedir } from "node:os";
import { join, resolve } from "node:path";
import {
  execSync,
  resolveAftLogPath,
  resolveCortexKitUserConfigPath,
  spawnSync,
} from "@cortexkit/aft-bridge";

import { dirSize } from "../lib/fs-util.js";
import { readJsoncFile } from "../lib/jsonc.js";
import { getCortexKitStorageRoot } from "../lib/paths.js";
import type {
  HarnessAdapter,
  HarnessConfigPaths,
  PluginCacheInfo,
  PluginEntryResult,
} from "./types.js";

const PLUGIN_NAME = "@cortexkit/aft-pi";
const PLUGIN_ENTRY = `npm:${PLUGIN_NAME}`;

function getPiAgentDir(): string {
  // Pi supports an explicit agent-dir override for isolated installations.
  // Otherwise follow its $HOME / %USERPROFILE% convention so the CLI probes
  // the same settings and managed npm tree as the host.
  const configuredDir = process.env.PI_CODING_AGENT_DIR?.trim();
  if (configuredDir) return resolve(configuredDir);
  const envHome = process.platform === "win32" ? process.env.USERPROFILE : process.env.HOME;
  const home = envHome && envHome.length > 0 ? envHome : homedir();
  return join(home, ".pi", "agent");
}

/**
 * Pi extensions are installed via `pi install npm:<package>` (or `file:<path>`)
 * and managed by Pi itself — there's no user-editable registration file
 * equivalent to OpenCode's `plugin` array.
 *
 * As of Pi v0.74.0, installed package sources are recorded in
 * `~/.pi/agent/settings.json` under the `packages` array. Each entry is a
 * source string (or an object with a `source` field) in one of these forms
 * (Pi's package-manager.js → parseSource):
 *
 *   - `npm:<spec>`         e.g. `npm:@cortexkit/aft-pi` or `npm:@cortexkit/aft-pi@1.2.3`
 *   - `file:<path>`        local file: URL
 *   - `<rel-path>`         path relative to `~/.pi/agent/` (normalized via
 *                          `relative(baseDir, resolved)` in normalizePackageSourceForSettings)
 *   - `<abs-path>`         absolute path to a local package directory
 *
 * Pre-v0.74 versions used `extensions.json` / `extensions.jsonc` / `config.json`
 * with an `extensions` or `plugins` array. We still check those for back-compat,
 * but the primary path is settings.json now.
 */
function readPiExtensionIndex(): { installed: string[]; path: string | null } {
  // Pi v0.74+: settings.json `packages` array (primary)
  const settingsPath = join(getPiAgentDir(), "settings.json");
  if (existsSync(settingsPath)) {
    try {
      const raw = readFileSync(settingsPath, "utf-8");
      const trimmed = raw.replace(/^\uFEFF/, "");
      const value = JSON.parse(trimmed) as Record<string, unknown>;
      const packages = value.packages;
      if (Array.isArray(packages)) {
        const installed = packages.flatMap((p): string[] => {
          if (typeof p === "string") return [p];
          if (
            typeof p === "object" &&
            p !== null &&
            typeof (p as { source?: unknown }).source === "string"
          ) {
            return [(p as { source: string }).source];
          }
          return [];
        });
        return { installed, path: settingsPath };
      }
    } catch {
      // fall through to legacy files
    }
  }
  // Pre-v0.74: extensions.json / extensions.jsonc / config.json (legacy)
  const candidates = [
    join(getPiAgentDir(), "extensions.json"),
    join(getPiAgentDir(), "extensions.jsonc"),
    join(getPiAgentDir(), "config.json"),
    join(getPiAgentDir(), "config.jsonc"),
  ];
  for (const path of candidates) {
    if (!existsSync(path)) continue;
    try {
      const { value } = readJsoncFile(path);
      const extensions = (value?.extensions ?? value?.plugins ?? []) as unknown;
      if (Array.isArray(extensions)) {
        const installed = extensions
          .map((e) =>
            typeof e === "string"
              ? e
              : typeof (e as { name?: string })?.name === "string"
                ? (e as { name: string }).name
                : "",
          )
          .filter((name): name is string => name.length > 0);
        return { installed, path };
      }
    } catch {
      // try next
    }
  }
  return { installed: [], path: null };
}

/**
 * Match a Pi `packages` entry against AFT's package. Handles all four
 * source-string forms documented in `readPiExtensionIndex`. For local
 * paths we verify the package.json name field — symmetric with OpenCode's
 * `pathPointsToOurPlugin` heuristic.
 */
function piEntryMatchesAft(entry: string): boolean {
  // npm:<spec> — e.g. `npm:@cortexkit/aft-pi`, `npm:@cortexkit/aft-pi@1.2.3`
  if (entry.startsWith("npm:")) {
    const spec = entry.slice("npm:".length).trim();
    if (spec === PLUGIN_NAME) return true;
    if (spec.startsWith(`${PLUGIN_NAME}@`)) return true;
    return false;
  }
  // file:<path> — file URL
  let resolved: string | null = null;
  if (entry.startsWith("file:")) {
    try {
      // Strip `file:` prefix manually — fileURLToPath would require `file://`.
      const stripped = entry.slice("file:".length);
      resolved = stripped.startsWith("//") ? stripped.slice(2) : stripped;
    } catch {
      return false;
    }
  } else if (entry.startsWith("/")) {
    resolved = entry;
  } else if (entry.length > 0) {
    // Relative path — resolve against the Pi agent dir (matches Pi's
    // `normalizePackageSourceForSettings` `relative(baseDir, ...)` behavior).
    resolved = join(getPiAgentDir(), entry);
  }
  if (!resolved) return false;
  try {
    if (!existsSync(resolved)) return false;
    // Look for package.json in resolved or its parent directories.
    const pkgPath = join(resolved, "package.json");
    if (!existsSync(pkgPath)) return false;
    const pkg = JSON.parse(readFileSync(pkgPath, "utf-8")) as { name?: unknown };
    return pkg.name === PLUGIN_NAME;
  } catch {
    return false;
  }
}

function piHasOurExtension(): boolean {
  const { installed } = readPiExtensionIndex();
  return installed.some(piEntryMatchesAft);
}

export class PiAdapter implements HarnessAdapter {
  readonly kind = "pi" as const;
  readonly displayName = "Pi";
  readonly pluginPackageName = PLUGIN_NAME;
  readonly pluginEntryWithVersion = PLUGIN_ENTRY;

  isInstalled(): boolean {
    try {
      // timeout: a misbehaving/booting host must never hang the probe.
      execSync("pi --version", { stdio: "ignore", timeout: 5000 });
      return true;
    } catch {
      return false;
    }
  }

  getHostVersion(): string | null {
    // Pi v0.74.0+ writes `--version` output through its `takeOverStdout()`
    // redirector, which sends everything stdout-bound to stderr. Pre-v0.74
    // writes the version to stdout. Capture both so we work either way.
    try {
      const result = spawnSync("pi", ["--version"], {
        stdio: ["ignore", "pipe", "pipe"],
        encoding: "utf-8",
        timeout: 5000,
      });
      if (result.status !== 0) return null;
      const stdout = (result.stdout ?? "").trim();
      const stderr = (result.stderr ?? "").trim();
      const text = stdout || stderr;
      // Some Pi versions print a leading banner before the version; the version
      // itself is always the last semver-looking token. Take the first line that
      // looks like a version.
      const semverLine = text.split(/\r?\n/).find((l) => /^\d+\.\d+\.\d+/.test(l.trim()));
      return semverLine?.trim() ?? text ?? null;
    } catch {
      return null;
    }
  }

  detectConfigPaths(): HarnessConfigPaths {
    const configDir = getPiAgentDir();
    // Pi doesn't have a user-editable "harness config" analogous to opencode.jsonc;
    // point at the likely extensions index for diagnostic purposes only.
    const index = readPiExtensionIndex();
    // AFT config lives in the shared CortexKit location since the v0.40.0
    // consolidation, not ~/.pi/agent. Use the bridge's canonical path so the CLI
    // and the plugin agree byte-for-byte (and so a fresh `setup` creates
    // aft.jsonc, the only name the plugin reads).
    const aftConfigPath = resolveCortexKitUserConfigPath();
    const aftConfigExists = existsSync(aftConfigPath);
    return {
      configDir,
      harnessConfig: index.path ?? join(configDir, "extensions.json"),
      harnessConfigFormat: index.path ? "json" : "none",
      aftConfig: aftConfigPath,
      aftConfigFormat: aftConfigExists ? "jsonc" : "none",
    };
  }

  hasPluginEntry(): boolean {
    return piHasOurExtension();
  }

  async ensurePluginEntry(): Promise<PluginEntryResult> {
    if (this.hasPluginEntry()) {
      return {
        ok: true,
        action: "already_present",
        message: `${PLUGIN_NAME} is already installed`,
        configPath: this.detectConfigPaths().harnessConfig,
      };
    }
    if (!this.isInstalled()) {
      return {
        ok: false,
        action: "error",
        message: "pi CLI not found on PATH. Install Pi first: https://github.com/badlogic/pi-mono",
        configPath: this.detectConfigPaths().harnessConfig,
      };
    }
    try {
      execSync(`pi install ${PLUGIN_ENTRY}`, { stdio: "inherit" });
      return {
        ok: true,
        action: "added",
        message: `Installed ${PLUGIN_ENTRY} via \`pi install\``,
        configPath: this.detectConfigPaths().harnessConfig,
      };
    } catch (error) {
      return {
        ok: false,
        action: "error",
        message: `Failed to run \`pi install ${PLUGIN_ENTRY}\`: ${error instanceof Error ? error.message : String(error)}`,
        configPath: this.detectConfigPaths().harnessConfig,
      };
    }
  }

  getPluginCacheInfo(): PluginCacheInfo {
    // Pi manages user-scoped npm packages below its agent directory rather than
    // the project or the process-wide npm root. Read the installed manifest so
    // doctor reports the version that the registered extension actually loads.
    const candidates = [
      join(getPiAgentDir(), "npm", "node_modules", "@cortexkit", "aft-pi", "package.json"),
      // Keep older layouts readable for users upgrading from pre-managed installs.
      join(getPiAgentDir(), "node_modules", "@cortexkit", "aft-pi", "package.json"),
      join(getPiAgentDir(), "extensions", "node_modules", "@cortexkit", "aft-pi", "package.json"),
    ];
    for (const candidate of candidates) {
      if (!existsSync(candidate)) continue;
      try {
        const pkg = JSON.parse(readFileSync(candidate, "utf-8")) as { version?: unknown };
        const cached = typeof pkg.version === "string" ? pkg.version : undefined;
        return {
          path: candidate,
          cached,
          latest: undefined,
          exists: true,
        };
      } catch {
        // next
      }
    }
    return {
      path: join(getPiAgentDir(), "npm", "node_modules", "@cortexkit", "aft-pi", "package.json"),
      exists: false,
    };
  }

  getStorageDir(): string {
    return getCortexKitStorageRoot();
  }

  getLogFile(): string {
    return resolveAftLogPath("aft-plugin.log");
  }

  getInstallHint(): string {
    return "Install Pi: https://github.com/badlogic/pi-mono";
  }

  async clearPluginCache(_force: boolean): Promise<{
    action: "cleared" | "up_to_date" | "not_found" | "not_applicable" | "error";
    path: string;
    cached?: string;
    latest?: string;
    error?: string;
  }> {
    // Pi owns its extension cache — we don't touch it from here. `doctor --force`
    // is an OpenCode-specific remedy for the npm/npx package cache that
    // OpenCode populates under `~/.cache/opencode/packages/`.
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
}
