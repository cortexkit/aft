/**
 * HarnessAdapter — abstracts what the unified AFT CLI needs to know about a
 * specific agent harness (OpenCode, Pi, and future entries).
 *
 * Each adapter covers:
 *   1. *Detection* — is the harness installed? is AFT registered with it?
 *   2. *Configuration* — where do its config files live, how are they read/written?
 *   3. *Runtime state* — log file, storage dir, plugin cache dir
 *   4. *Setup* — how a user installs/registers our plugin with it
 *
 * Everything here is synchronous or returns synchronously-resolvable
 * structures; async work lives in the command layer.
 */

export type HarnessKind = "opencode" | "pi";

export interface HarnessConfigPaths {
  /** Primary config dir (e.g. `~/.config/opencode`, `~/.pi/agent`). */
  configDir: string;
  /** Harness's main config file (opencode.jsonc, pi's internal config if any). */
  harnessConfig: string;
  harnessConfigFormat: "json" | "jsonc" | "none";
  /** AFT user-level config (`<configDir>/aft.jsonc` or equivalent). */
  aftConfig: string;
  aftConfigFormat: "json" | "jsonc" | "none";
  /** Optional harness-specific UI/TUI config (OpenCode's `tui.jsonc`; undefined for Pi). */
  tuiConfig?: string;
  tuiConfigFormat?: "json" | "jsonc" | "none";
}

export interface PluginCacheInfo {
  path: string;
  cached?: string;
  latest?: string;
  exists: boolean;
}

export interface PluginEntryResult {
  /** True if the plugin was added or was already present. */
  ok: boolean;
  /** Human-readable action description for the doctor/setup output. */
  action: "already_present" | "added" | "updated" | "config_missing" | "error";
  message: string;
  configPath: string;
}

export interface HarnessAdapter {
  readonly kind: HarnessKind;
  readonly displayName: string;
  readonly pluginPackageName: string;
  readonly pluginEntryWithVersion: string;

  /** Is the harness's host CLI (`opencode`, `pi`) on PATH? */
  isInstalled(): boolean;
  getHostVersion(): string | null;

  detectConfigPaths(): HarnessConfigPaths;

  /**
   * Check whether our plugin is registered with the harness. For OpenCode this
   * reads the `plugin` array in opencode.jsonc. For Pi it checks the extension
   * index because Pi manages its own registration.
   */
  hasPluginEntry(): boolean;

  /**
   * Ensure the plugin is registered. May prompt, may run shell commands (Pi),
   * may edit config files (OpenCode). Idempotent.
   */
  ensurePluginEntry(): Promise<PluginEntryResult>;

  getPluginCacheInfo(): PluginCacheInfo;
  getStorageDir(): string;
  getLogFile(): string;

  /** User-facing hint when the harness isn't installed. */
  getInstallHint(): string;

  /**
   * Harness-aware version of `doctor --force` cache clearing. OpenCode nukes
   * its npm/npx package cache (`~/.cache/opencode/packages/...`); Pi may be a
   * no-op because Pi caches are managed by `pi install` itself.
   */
  clearPluginCache(force: boolean): Promise<{
    action: "cleared" | "up_to_date" | "not_found" | "not_applicable" | "error";
    path: string;
    cached?: string;
    latest?: string;
    error?: string;
  }>;
}
