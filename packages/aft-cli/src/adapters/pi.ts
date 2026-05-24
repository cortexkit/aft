import { execSync, spawnSync } from "node:child_process";
import { existsSync, readFileSync } from "node:fs";
import { homedir } from "node:os";
import { join } from "node:path";
import { dirSize } from "../lib/fs-util.js";
import { detectJsoncFile, readJsoncFile } from "../lib/jsonc.js";
import { getTmpLogPath } from "../lib/paths.js";
import type {
  HarnessAdapter,
  HarnessConfigPaths,
  PluginCacheInfo,
  PluginEntryResult,
} from "./types.js";

const PLUGIN_NAME = "@cortexkit/aft-pi";
const PLUGIN_ENTRY = `npm:${PLUGIN_NAME}`;

function getPiAgentDir(): string {
  // Prefer $HOME / %USERPROFILE% so tests can override. Falls back to
  // os.homedir() which reads from getpwuid()/SHGetKnownFolderPath. Matches
  // the convention OpenCode and Pi themselves use (settings-manager.js
  // reads $HOME via getAgentDir()).
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
 * string in one of these forms (Pi's package-manager.js → parseSource):
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
        const installed = packages.filter((p): p is string => typeof p === "string");
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
      execSync("pi --version", { stdio: "ignore" });
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
    const aft = detectJsoncFile(configDir, "aft");
    return {
      configDir,
      harnessConfig: index.path ?? join(configDir, "extensions.json"),
      harnessConfigFormat: index.path ? "json" : "none",
      aftConfig: aft.path,
      aftConfigFormat: aft.format,
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
    // Pi manages its own extension cache location; doctor reports whether the
    // extension is registered, not an on-disk cache path. Best-effort: look
    // for a node_modules install under Pi's agent dir.
    const candidates = [
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
      path: join(getPiAgentDir(), "extensions"),
      exists: false,
    };
  }

  getStorageDir(): string {
    // Pi's storage dir convention from packages/pi-plugin/src/index.ts.
    return join(getPiAgentDir(), "aft");
  }

  getLogFile(): string {
    return getTmpLogPath("aft-pi.log");
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
    };
  }
}
