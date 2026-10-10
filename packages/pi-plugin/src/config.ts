import { existsSync, readFileSync } from "node:fs";
import { homedir } from "node:os";
import { isAbsolute } from "node:path";
import {
  type AftConfigFileMigrationResult,
  ConfigRejectedError,
  type ConfigTier,
  DEFAULT_DISABLED_TOOLS,
  DEFAULT_WORKER_WAIT_MAX_MS,
  deliverMigrationNoticeOnce,
  legacyConfigNoticeMessage,
  MIN_WORKER_WAIT_MAX_MS,
  mergeIndexes,
  migrateAftConfigFile as migrateLegacyAftConfigFile,
  noticeDigest,
  noticeProjection,
  OPENCODE_ONLY_KEYS,
  partitionProjectDisables,
  type RawIndexesConfig,
  type ResolvedIndexesConfig,
  readConfigTiers,
  resolveCortexKitConfigPaths,
  resolveIndexes,
  resolveLegacyAftConfigSources,
  semanticCostNotice,
  sortedUnique,
  stripHarnessSpecificConfigKeys,
  stripJsoncSymbols,
  suppliesSemanticIndexInput,
  translateConfigDocument,
  unionDisabledTools,
  validateResolvedConfig,
} from "@cortexkit/aft-bridge";
import { parse as parseJsonc } from "comment-json";
import { z } from "zod";

import { error, log, warn } from "./logger.js";

export { ConfigRejectedError } from "@cortexkit/aft-bridge";

const ACTIVE_HARNESS = "pi";

function isConfigRecord(value: unknown): value is Record<string, unknown> {
  return value !== null && typeof value === "object" && !Array.isArray(value);
}

function normalizeConfigForActiveHarness(value: unknown): unknown {
  const config = stripHarnessSpecificConfigKeys(value, OPENCODE_ONLY_KEYS);
  if (!isConfigRecord(config) || !isConfigRecord(config.harnesses)) return config;

  const override = config.harnesses[ACTIVE_HARNESS];
  return {
    ...config,
    // Other harnesses deliberately remain opaque to this plugin so future
    // harness-specific settings never make an older Pi plugin reject the shared
    // config file.
    harnesses:
      override === undefined
        ? {}
        : { [ACTIVE_HARNESS]: stripHarnessSpecificConfigKeys(override, OPENCODE_ONLY_KEYS) },
  };
}

function warnIgnoredNestedHarnesses(rawConfig: Record<string, unknown>, configPath: string): void {
  const override = isConfigRecord(rawConfig.harnesses)
    ? rawConfig.harnesses[ACTIVE_HARNESS]
    : undefined;
  if (isConfigRecord(override) && Object.hasOwn(override, "harnesses")) {
    warn(
      `Ignoring nested harnesses in harnesses.${ACTIVE_HARNESS} from ${configPath}; harness overrides cannot recurse`,
    );
  }
}

// ---------------------------------------------------------------------------
// Config shape (mirrors aft-opencode's schema, simplified for Pi)
// ---------------------------------------------------------------------------

export type Formatter =
  | "biome"
  | "oxfmt"
  | "prettier"
  | "deno"
  | "ruff"
  | "black"
  | "rustfmt"
  | "goimports"
  | "gofmt"
  | "none";

export type Checker =
  | "tsc"
  | "tsgo"
  | "biome"
  | "pyright"
  | "ruff"
  | "cargo"
  | "go"
  | "staticcheck"
  | "none";

/** How configure-time missing-tool warnings are delivered by the Pi plugin. */
export type ConfigureWarningsDelivery = "toast" | "log" | "chat";

export type SemanticBackend = "fastembed" | "openai_compatible" | "ollama" | "synapse";

export interface BridgeConfig {
  request_timeout_ms?: number;
  hang_threshold?: number;
}

export interface SubcConfig {
  /**
   * Absolute path to the Subconscious (subc) daemon connection file. PRESENT
   * (non-empty) ⇒ talk to AFT as a daemon-supervised module over subc; ABSENT ⇒
   * standalone NDJSON (default). USER/global-tier ONLY (a project must not
   * redirect transport). No auto-derive. macOS default:
   * `~/.local/share/cortexkit/run/subc-connection.json`.
   */
  connection_file?: string;
  /** User-tier root-reaper switch; project config cannot set or override it. */
  client_reaper?: boolean;
}

export interface GhShimConfig {
  /** User-tier AFT image used by the managed `gh` entry. Defaults to the running image. */
  binary_path?: string;
}

/**
 * OpenCode 2 server used for permission prompts. Only the OpenCode plugin reads
 * it; Pi accepts it because both plugins share the same aft.jsonc files.
 * USER-tier ONLY: a project could otherwise send prompts to its own server.
 */
export interface OpenCodeHostConfig {
  server_url?: string;
  server_password_env?: string;
}

export interface GithubConfig {
  /** Interpose the governed `gh` shim in agent child PATHs. Default: true. */
  shim?: boolean;
  /** Allow structured issue:// and pr:// reads. Default: false. */
  read?: boolean;
  /** Allow issue and pull-request comment writes (implies read). Default: false. */
  write?: boolean;
}

export interface GitConfig {
  /** "off" (default), "auto", or an explicit "Name <email>" identity. */
  co_author?: string;
}

export type PiToolPresentation = "top_level" | "host_default";

export interface PiConfig {
  /**
   * Tool presentation on Pi and OMP harnesses.
   * - "top_level": registers tools with `loadMode: "essential"` on OMP so they appear at top level. (default)
   * - "host_default": leaves tool presentation to host defaults (on OMP, tools mount under xd://).
   */
  tool_presentation?: PiToolPresentation;
}

export interface IndexRootConfig {
  path: string;
  indexes: Array<"search" | "semantic" | "callgraph">;
}

export interface IndexConfig {
  roots?: IndexRootConfig[];
}

export interface SemanticConfig {
  backend?: SemanticBackend;
  model?: string;
  base_url?: string;
  api_key_env?: string;
  /**
   * Background embedding request floor in milliseconds. HTTP batch deadlines
   * scale above it from the successful per-item latency EMA.
   */
  timeout_ms?: number;
  query_timeout_ms?: number;
  query_instruction?: string;
  max_batch_size?: number;
  max_input_tokens?: number;
  max_files?: number;
}

export interface RerankConfig {
  /**
   * Reranker for the head of aft_search results: "off" (default), "onnx",
   * "remote" or "synapse". Only prose questions are reranked. A project
   * config may only set "off".
   */
  backend?: "off" | "onnx" | "remote" | "synapse";
  /**
   * For "onnx": bge-reranker-base (default), bge-reranker-v2-m3,
   * jina-reranker-v1-turbo or gte-reranker-modernbert-base. Required for
   * "remote" and "synapse". User config only.
   */
  model?: string;
  /** Base URL of the "remote" endpoint (AFT appends /rerank; tei+ for TEI). User config only. */
  endpoint?: string;
  /** Environment variable holding the "remote" endpoint's API key. User config only. */
  api_key_env?: string;
  /** How many leading results are reranked (default 20, at most 200). User config only. */
  top_n?: number;
  /** Reranking budget per search in milliseconds (default 1500). User config only. */
  timeout_ms?: number;
}

export interface SearchConfig {
  /** Optional cross-encoder reranking of aft_search results. Default off. */
  rerank?: RerankConfig;
}

export interface LspServerConfig {
  id: string;
  /** Omitted when overriding a built-in server to inherit its extensions. */
  extensions?: string[];
  /** Omitted when overriding a built-in server to inherit its binary. */
  binary?: string;
  args: string[];
  root_markers: string[];
  disabled: boolean;
  env?: Record<string, string>;
  initialization_options?: unknown;
}

export interface InspectConfig {
  enabled?: boolean;
  diagnostics_timeout_ms?: number;
  tier2_idle_minutes?: number;
  tier2_pass_timeout_ms?: number;
  categories?: Partial<
    Record<
      | "diagnostics"
      | "todos"
      | "dead_code"
      | "unused_exports"
      | "duplicates"
      | "cycles"
      | "complexity",
      boolean
    >
  >;
  duplicates?: {
    expected_mirrors?: [string, string][];
  };
}

export interface IdleConfig {
  /** Unbound-root artifact eviction idle window in minutes. Default 30; clamped to 5..=30. */
  root_ttl_minutes?: number;
}

export const DEFAULT_INSPECT_DIAGNOSTICS_TIMEOUT_MS = 120_000;
export const MIN_INSPECT_DIAGNOSTICS_TIMEOUT_MS = 10_000;
export const MAX_INSPECT_DIAGNOSTICS_TIMEOUT_MS = 600_000;

function clampInspectDiagnosticsTimeoutMs(value: number): number {
  return Math.min(
    MAX_INSPECT_DIAGNOSTICS_TIMEOUT_MS,
    Math.max(MIN_INSPECT_DIAGNOSTICS_TIMEOUT_MS, value),
  );
}

/** Resolve the blocking diagnostics deadline for tools that wait on `aft_inspect`. */
export function resolveInspectDiagnosticsTimeoutMs(config: AftConfig): number {
  return clampInspectDiagnosticsTimeoutMs(
    config.inspect?.diagnostics_timeout_ms ?? DEFAULT_INSPECT_DIAGNOSTICS_TIMEOUT_MS,
  );
}

export interface BackupConfig {
  enabled?: boolean;
  max_depth?: number;
  max_file_size?: number;
}

export interface LspConfig {
  /** Minutes since the last AFT tool call on that repository; default 60. */
  idle_minutes?: number | "never";
  servers?: Record<string, Omit<LspServerConfig, "id">>;
  disabled?: string[];
  python?: "pyright" | "ty" | "auto";
  /** Restore legacy inline LSP waits on edit/write unless the tool call overrides diagnostics. */
  diagnostics_on_edit?: boolean;
  auto_install?: boolean;
  grace_days?: number;
  versions?: Record<string, string>;
}

export interface ExperimentalConfig {
  bash?: {
    rewrite?: boolean;
    compress?: boolean;
    background?: boolean;
    long_running_reminder_enabled?: boolean;
    long_running_reminder_interval_ms?: number;
  };
  lsp_ty?: boolean;
}

export interface ConfigureLspOverrides {
  experimental_lsp_ty?: boolean;
  lsp_servers?: LspServerConfig[];
  disabled_lsp?: string[];
}

export interface ConfigureExperimentalOverrides {
  experimental_bash_rewrite?: boolean;
  experimental_bash_compress?: boolean;
  experimental_bash_background?: boolean;
  bash_long_running_reminder_enabled?: boolean;
  bash_long_running_reminder_interval_ms?: number;
  experimental_lsp_ty?: boolean;
}

/** Sandbox settings for first-party Pi processes. */
export interface SandboxConfig {
  /** Enable native containment for first-party bash and PTY processes. Default: false. */
  enabled?: boolean;
  /** Additional absolute directories where sandboxed commands may write. User config only. */
  write_allow?: string[];
  /** Additional absolute paths sandboxed commands may not read. Project config may add entries. */
  read_deny?: string[];
}

/**
 * Graduated `bash` config. It accepts `true`, `false`, or an object whose
 * omitted fields use the normal bash defaults during resolution.
 */
export interface BashConfig {
  /** Runtime gate for every bash operation. Default true; false reports `bash_disabled`. */
  enabled?: boolean;
  rewrite?: boolean;
  compress?: boolean;
  background?: boolean;
  /** Permit per-command host fallback after AFT transport failure. Default false. */
  host_fallback?: boolean;
  runon_enabled?: boolean;
  /**
   * Allow worker sessions (headless `pi -p` / JSON runs, or MAGIC_CONTEXT_PI_SUBAGENT=1)
   * to use background bash; when false, requests block to completion and async
   * bash_watch becomes a sync wait. Default true.
   */
  subagent_background?: boolean;
  /** Detach wait:true bash calls on user messages; `&detach` overrides, is stripped before delivery, and a token-only message gets a minimal replacement. */
  detach_on_user_message?: boolean;
  db_schema_hints?: boolean;
  long_running_reminder_enabled?: boolean;
  long_running_reminder_interval_ms?: number;
  /**
   * How long foreground bash blocks before auto-promoting to background.
   * Default 15000ms; values below the 5000ms floor are clamped up.
   */
  foreground_wait_window_ms?: number;
  /** Maximum synchronous bash_watch wait; values outside 1000..1800000 are clamped. Default 120000. */
  watch_sync_max_ms?: number;
  /**
   * Longest a delegated worker's wait on one command blocks before control
   * returns to it. Default 1800000 (30 minutes); values below 60000 are a
   * config error.
   */
  worker_wait_max_ms?: number;
  /** Linux-only user-tier opt-in for transient systemd user scopes. Default false. */
  linux_scope?: boolean;
  /** macOS agent commands do not inherit the supervisor's privacy grants. Default false. */
  disclaim_privacy?: boolean;
  /** Manual fallback for Pi versions that do not expose enabled default tools. */
  powershell_tool?: boolean;
}

export interface ResolvedGithubConfig {
  shim: boolean;
  read: boolean;
  write: boolean;
}

/** Resolve GitHub capability leaves and the write-implies-read safety rule. */
export function resolveGithubConfig(config: AftConfig): ResolvedGithubConfig {
  const write = config.github?.write === true;
  return {
    shim: config.github?.shim ?? true,
    read: (config.github?.read ?? false) || write,
    write,
  };
}

/**
 * Whether a tool is registered, for description wording only (for example,
 * whether to mention `aft_zoom`). Registration itself goes through
 * {@link resolvedDisabledTools}, which refuses a config without a resolved list.
 */
export function toolEnabled(config: AftConfig, toolName: string): boolean {
  return !(config.disabled_tools ?? []).includes(toolName);
}

/**
 * The resolved disabled list. `loadAftConfig` always sets it; a config that
 * reaches registration without it is rejected instead of treated as [].
 */
export function resolvedDisabledTools(config: AftConfig): readonly string[] {
  if (config.disabled_tools === undefined) {
    throw new ConfigRejectedError(["invalid_resolved_config:missing:disabled_tools"]);
  }
  return config.disabled_tools;
}

/** Resolved index switches (all default on). */
export function resolvedIndexes(config: AftConfig): ResolvedIndexesConfig {
  return resolveIndexes(config.indexes);
}

export interface ViewsConfig {
  /** Enable content-addressed index views. Default: false. */
  enabled?: boolean;
}

/** `remote_exec` (see `RemoteExecConfigSchema`). */
export interface RemoteExecConfig {
  enabled?: boolean;
  default_demand?: string;
  /**
   * Set by the tier merge, never read from a file: a project config turned
   * remote runs off, so a `runon` call can be refused with that reason.
   */
  project_off?: boolean;
}

export interface AftConfig {
  /**
   * Optional JSON Schema URL for editor tooling. Runtime no-op — only present
   * so VS Code/Cursor/etc. pick up the published schema for autocomplete +
   * validation. `aft setup` auto-inserts this.
   */
  $schema?: string;
  /** Select the edit/read surface. `hashline` exposes tagged reads and `{ patch }` edits. */
  edit_mode?: "default" | "hashline";
  format_on_edit?: boolean;
  /** Maximum formatter subprocess wallclock seconds. Bounded 1..=600. Default 10. */
  formatter_timeout_secs?: number;
  validate_on_edit?: "syntax" | "full";
  formatter?: Record<string, Formatter>;
  checker?: Record<string, Checker>;
  /** Configure-time missing-tool warning delivery. Default: toast. */
  configure_warnings_delivery?: ConfigureWarningsDelivery;
  /**
   * Tool names that are not registered; every other AFT tool is registered.
   * Absent from the user config ⇒ ["aft_move", "aft_delete"]; an explicit list
   * (including []) replaces that default. A project config may only add
   * names and cannot disable aft_safety or a host tool slot.
   */
  disabled_tools?: string[];
  restrict_to_project_root?: boolean;
  /** Background indexes. Each defaults on; a project config can only turn one off. */
  indexes?: RawIndexesConfig;
  /** User-tier standing roots; project config cannot configure this machine state. */
  index?: IndexConfig;
  /** Content-addressed index views. Disabled by default. */
  views?: ViewsConfig;
  /** Number of files to parse in a single batch during callgraph store cold build. Lower values reduce peak memory during cold build. Default: 100. */
  callgraph_chunk_size?: number;
  /** Codebase health inspection config. `inspect.enabled=false` makes aft_inspect report inspect_disabled. */
  inspect?: InspectConfig;
  /** Idle reclamation windows for unbound-root artifacts and language servers. */
  idle?: IdleConfig;
  /** Undo backup config. User-only: project config cannot disable or shrink a user's safety net. */
  backup?: BackupConfig;
  /**
   * Linked-worktree RAM overlay. Default off. A repo may opt its worktrees
   * in at project tier; it only spends that machine's RAM.
   */
  worktree?: {
    /** Apply local watcher events to the in-RAM trigram delta. Never writes shared artifacts. */
    ram_overlay?: boolean;
  };
  /** Native first-party bash sandbox. Write allowances are user-only; a project may enable but never disable. */
  sandbox?: SandboxConfig;
  /**
   * Bash runtime configuration (runtime gate + rewrite + compress +
   * background). Registration is controlled by `disabled_tools` only.
   * Graduated from `experimental.bash.*` in v0.27.2; the legacy nested
   * form is still accepted for backward compat.
   *
   * - `true`  — runtime on, all sub-features on (the default)
   * - `false` — runtime gate off; bash operations report `bash_disabled`
   * - `{ enabled?, rewrite?, compress?, background?, ... }` — partial
   *   override; missing sub-keys default to `true`
   */
  bash?: boolean | BashConfig;
  experimental?: ExperimentalConfig;
  lsp?: LspConfig;
  url_fetch_allow_private?: boolean;
  semantic?: SemanticConfig;
  /** aft_search settings; a project config may only turn reranking off. */
  search?: SearchConfig;
  bridge?: BridgeConfig;
  subc?: SubcConfig;
  /** OpenCode 2 server used for permission prompts (user-only; read by the OpenCode plugin). */
  opencode?: OpenCodeHostConfig;
  github?: GithubConfig;
  /** Managed `gh` shim binary override (user-only). Whether the shim is used is `github.shim`. */
  gh_shim?: GhShimConfig;
  /** Remote runs requested per bash call with `runon` (user-only; a project may only turn it off). */
  remote_exec?: RemoteExecConfig;
  git?: GitConfig;
  /** Pi and OMP harness-specific configuration. */
  pi?: PiConfig;
  /** Per-harness config overrides; nested harnesses are ignored. */
  harnesses?: Record<string, Omit<AftConfig, "harnesses">>;
}

/**
 * Resolved bash config: every flag has an explicit boolean.
 */
export interface ResolvedBashConfig {
  enabled: boolean;
  rewrite: boolean;
  compress: boolean;
  background: boolean;
  /** Emergency local execution gate. Default false, including for `bash: true`. */
  host_fallback: boolean;
  runon_enabled: boolean;
  /** Allow subagents to use background bash; default true. */
  subagent_background: boolean;
  /** Detach wait:true bash calls on user messages; `&detach` overrides, is stripped before delivery, and a token-only message gets a minimal replacement. */
  detach_on_user_message: boolean;
  /** Read-only database CLI schema hints after missing-table/column errors. */
  db_schema_hints: boolean;
  long_running_reminder_enabled?: boolean;
  long_running_reminder_interval_ms?: number;
  /**
   * Foreground poll window before auto-promotion to background, in ms.
   * Always resolved: defaults to 15000, floored at 5000.
   */
  foreground_wait_window_ms: number;
  /** Maximum synchronous bash_watch wait. Defaults to 120000 and is clamped to 1000..1800000. */
  watch_sync_max_ms: number;
  /** Longest a delegated worker's wait blocks (ms). Defaults to 1800000; at least 60000. */
  worker_wait_max_ms: number;
  /** Manual PowerShell registration fallback. Default false. */
  powershell_tool: boolean;
}

/** Default foreground wait-window before auto-promotion (ms). */
export const FOREGROUND_WAIT_WINDOW_DEFAULT_MS = 15_000;
/** Minimum allowed foreground wait-window (ms); smaller values clamp up. */
export const FOREGROUND_WAIT_WINDOW_MIN_MS = 5_000;
/** Default maximum synchronous bash_watch wait (ms). */
export const DEFAULT_BASH_WATCH_SYNC_MAX_MS = 120_000;
/** Minimum synchronous bash_watch wait cap (ms). */
export const MIN_BASH_WATCH_SYNC_MAX_MS = 1_000;
/** Static schema maximum and hard upper bound for synchronous bash_watch waits (ms). */
export const MAX_BASH_WATCH_SYNC_MAX_MS = 1_800_000;

export function clampBashWatchSyncMaxMs(value: number | undefined): number {
  const raw = value ?? DEFAULT_BASH_WATCH_SYNC_MAX_MS;
  const clamped = Math.min(MAX_BASH_WATCH_SYNC_MAX_MS, Math.max(MIN_BASH_WATCH_SYNC_MAX_MS, raw));
  if (clamped !== raw) {
    warn(
      `bash.watch_sync_max_ms=${raw} is outside ${MIN_BASH_WATCH_SYNC_MAX_MS}..=${MAX_BASH_WATCH_SYNC_MAX_MS}; clamped to ${clamped}`,
    );
  }
  return clamped;
}

/**
 * Single source of truth for bash config across the Pi plugin. Resolution
 * order (highest priority wins):
 *
 *   1. Top-level `bash: false` → runtime off (sub-features all false)
 *   2. Top-level `bash: true`  → fully enabled (sub-features all true)
 *   3. Top-level `bash: { ... }` → runtime gate from `enabled` (default
 *      true); each sub-feature defaults true when not specified
 *   4. Top-level `bash` absent + any `experimental.bash.*` set → legacy
 *      fallback; sub-features take their explicit values (default false
 *      to preserve pre-v0.27.2 behavior — that block was opt-in); the
 *      runtime gate stays on
 *   5. Top-level `bash` absent + no experimental → everything on
 *
 * Mirrors OpenCode's resolver exactly. Reminder tuning rides through from
 * whichever surface specified it (top-level wins, legacy fills the gap).
 */
export function resolveBashConfig(config: AftConfig): ResolvedBashConfig {
  const top = config.bash;
  const legacy = config.experimental?.bash;

  const reminderEnabled =
    (typeof top === "object" && top !== null ? top.long_running_reminder_enabled : undefined) ??
    legacy?.long_running_reminder_enabled;
  const reminderInterval =
    (typeof top === "object" && top !== null ? top.long_running_reminder_interval_ms : undefined) ??
    legacy?.long_running_reminder_interval_ms;

  // Foreground wait-window: only the object form can set it; clamp to the
  // 5000ms floor and default to 15000ms when unset.
  const topDetachOnUserMessage =
    typeof top === "object" && top !== null ? (top.detach_on_user_message ?? true) : true;

  const rawForegroundWait =
    typeof top === "object" && top !== null ? top.foreground_wait_window_ms : undefined;
  const foregroundWaitWindowMs = Math.max(
    FOREGROUND_WAIT_WINDOW_MIN_MS,
    rawForegroundWait ?? FOREGROUND_WAIT_WINDOW_DEFAULT_MS,
  );
  const rawWatchSyncMax =
    typeof top === "object" && top !== null ? top.watch_sync_max_ms : undefined;
  const watchSyncMaxMs = clampBashWatchSyncMaxMs(rawWatchSyncMax);

  const base: ResolvedBashConfig = {
    enabled: false,
    rewrite: false,
    compress: false,
    background: false,
    host_fallback: false,
    runon_enabled: false,
    subagent_background: true,
    detach_on_user_message: true,
    db_schema_hints: typeof top === "object" && top !== null ? (top.db_schema_hints ?? true) : true,
    long_running_reminder_enabled: reminderEnabled,
    long_running_reminder_interval_ms: reminderInterval,
    foreground_wait_window_ms: foregroundWaitWindowMs,
    watch_sync_max_ms: watchSyncMaxMs,
    worker_wait_max_ms:
      (typeof top === "object" && top !== null ? top.worker_wait_max_ms : undefined) ??
      DEFAULT_WORKER_WAIT_MAX_MS,
    powershell_tool:
      typeof top === "object" && top !== null ? (top.powershell_tool ?? false) : false,
  };

  if (top === false) return base;
  if (top === true) {
    return { ...base, enabled: true, rewrite: true, compress: true, background: true };
  }
  if (typeof top === "object" && top !== null) {
    return {
      ...base,
      enabled: top.enabled ?? true,
      rewrite: top.rewrite ?? true,
      compress: top.compress ?? true,
      background: top.background ?? true,
      host_fallback: top.host_fallback ?? false,
      runon_enabled: top.runon_enabled ?? false,
      subagent_background: top.subagent_background ?? true,
      detach_on_user_message: topDetachOnUserMessage,
    };
  }

  // Top-level absent. Honor legacy experimental.bash.* if any sub-flag was
  // explicitly set — preserves pre-v0.27.2 opt-in semantics. An empty
  // `experimental.bash: {}` (object present but feature keys absent) falls
  // through to surface default; this avoids accidentally disabling bash for
  // users who wrote an empty experimental block while migrating.
  const hasLegacyFeatureFlag =
    legacy &&
    (legacy.rewrite !== undefined ||
      legacy.compress !== undefined ||
      legacy.background !== undefined);
  if (hasLegacyFeatureFlag) {
    const rewrite = legacy.rewrite === true;
    const compress = legacy.compress === true;
    const background = legacy.background === true;
    return { ...base, enabled: true, rewrite, compress, background };
  }

  return { ...base, enabled: true, rewrite: true, compress: true, background: true };
}

// This schema is intentionally duplicated with aft-opencode's config.ts rather
// than shared: the two plugins bundle independently and a shared package would
// couple their release artifacts. Drift between the copies (and against the
// Rust resolver) is enforced by the cross-language config parity fixtures in
// crates/aft/tests/integration/config_parity_test.rs - schema changes that
// disagree across the three resolvers fail that gate.

const FormatterEnum = z.enum([
  "biome",
  "oxfmt",
  "prettier",
  "deno",
  "ruff",
  "black",
  "rustfmt",
  "goimports",
  "gofmt",
  "none",
]);

const CheckerEnum = z.enum([
  "tsc",
  "tsgo",
  "biome",
  "pyright",
  "ruff",
  "cargo",
  "go",
  "staticcheck",
  "none",
]);

const ConfigureWarningsDeliveryEnum = z.enum(["toast", "log", "chat"]);
const IndexKindSchema = z.enum(["search", "semantic", "callgraph"]);

function isSupportedAbsoluteIndexPath(path: string): boolean {
  if (path === "~" || path.startsWith("~/") || path.startsWith("~\\")) {
    return isAbsolute(homedir());
  }
  return isAbsolute(path);
}

const IndexRootSchema = z
  .object({
    path: z.string().refine(isSupportedAbsoluteIndexPath, {
      message: "index.roots path must be absolute after ~ expansion",
    }),
    indexes: z.array(IndexKindSchema).min(1, "index.roots indexes must be non-empty"),
  })
  .superRefine(({ indexes }, context) => {
    if (new Set(indexes).size !== indexes.length) {
      context.addIssue({
        code: "custom",
        message: "index.roots indexes must not contain duplicates",
      });
    }
  })
  .transform(({ path, indexes }) => ({
    path,
    indexes: (["search", "semantic", "callgraph"] as const).filter(
      (kind) => indexes.includes(kind) || (kind === "search" && indexes.includes("semantic")),
    ),
  }));

const IndexConfigSchema = z.object({
  roots: z.array(IndexRootSchema).optional(),
});

const SemanticConfigSchema = z.object({
  backend: z.enum(["fastembed", "openai_compatible", "ollama", "synapse"]).optional(),
  model: z.string().trim().min(1).optional(),
  base_url: z.string().trim().min(1).optional(),
  api_key_env: z.string().trim().min(1).optional(),
  timeout_ms: z.number().int().positive().optional(),
  query_timeout_ms: z.number().int().positive().optional(),
  query_instruction: z.string().trim().min(1).optional(),
  max_batch_size: z.number().int().positive().optional(),
  max_input_tokens: z.number().int().positive().optional(),
  max_files: z.number().int().positive().optional(),
});

const RerankConfigSchema = z.object({
  backend: z.enum(["off", "onnx", "remote", "synapse"]).optional(),
  model: z.string().trim().min(1).optional(),
  endpoint: z.string().trim().min(1).optional(),
  api_key_env: z.string().trim().min(1).optional(),
  top_n: z.number().int().positive().optional(),
  timeout_ms: z.number().int().positive().optional(),
});

const SearchConfigSchema = z.object({
  rerank: RerankConfigSchema.optional(),
});

const LspExtensionSchema = z
  .string()
  .trim()
  .min(1)
  .refine((value) => value.replace(/^\.+/, "").length > 0, {
    message: "Extension must include characters other than leading dots",
  });

const LspServerEntrySchema = z.object({
  // Optional: overriding a built-in server (e.g. `rust`) to tweak one field
  // inherits the built-in's extensions/binary downstream. Requiring them here
  // silently dropped the whole `lsp` section on a partial override.
  extensions: z.array(LspExtensionSchema).min(1).optional(),
  binary: z.string().trim().min(1).optional(),
  args: z.array(z.string()).optional().default([]),
  root_markers: z.array(z.string().trim().min(1)).optional().default([".git"]),
  disabled: z.boolean().optional().default(false),
  /** Extra environment variables passed to the LSP server child process. */
  env: z.record(z.string().min(1), z.string()).optional(),
  /** JSON value passed as `initializationOptions` in the LSP `initialize` request. */
  initialization_options: z.unknown().optional(),
});

const LspConfigSchema = z.object({
  idle_minutes: z
    .union([
      z
        .number()
        .int()
        .transform((value) => Math.min(1440, Math.max(5, value))),
      z.literal("never"),
    ])
    .optional(),
  servers: z.record(z.string().trim().min(1), LspServerEntrySchema).optional(),
  disabled: z.array(z.string().trim().min(1)).optional(),
  python: z.enum(["pyright", "ty", "auto"]).optional(),
  /**
   * Restore legacy edit behavior by waiting for inline LSP diagnostics on every
   * edit/write call unless the tool call overrides diagnostics. Default: false.
   */
  diagnostics_on_edit: z.boolean().optional(),
  /**
   * Auto-install npm-distributed and GitHub-release language servers when
   * the project needs them. Default: true.
   */
  auto_install: z.boolean().optional(),
  /**
   * Supply-chain grace window. AFT only installs versions that have been on
   * the registry / GitHub releases for at least this many days. Default: 7.
   * User pins via `lsp.versions` bypass this.
   */
  // grace_days must be >= 1 because grace_days: 0 disables
  // the supply-chain grace window entirely with no warning. Users debugging
  // can still bypass the grace per-package via `lsp.versions` pins.
  grace_days: z.number().int().positive().optional(),
  /**
   * Per-package version pin map (npm package or GitHub repo).
   * Pins bypass the grace filter and any weekly version recheck.
   */
  versions: z.record(z.string().trim().min(1), z.string().trim().min(1)).optional(),
});

const ExperimentalConfigSchema = z.object({
  /**
   * @deprecated The bash family graduated from experimental in v0.27.2. Use
   * the top-level `bash` key instead. Still accepted for backward compat —
   * when present and top-level `bash` is absent, its values seed the
   * resolved bash config. Will be removed in v0.28.
   */
  bash: z
    .object({
      rewrite: z.boolean().optional(),
      compress: z.boolean().optional(),
      background: z.boolean().optional(),
      long_running_reminder_enabled: z.boolean().optional(),
      long_running_reminder_interval_ms: z.number().int().positive().optional(),
    })
    .optional(),
  lsp_ty: z.boolean().optional(),
});

/**
 * Graduated `bash` config schema. Replaces `experimental.bash.*` in v0.27.2.
 * Three shapes: boolean (true/false) or partial object override.
 */
const BashFeaturesSchema = z.object({
  enabled: z.boolean().optional(),
  rewrite: z.boolean().optional(),
  compress: z.boolean().optional(),
  background: z.boolean().optional(),
  host_fallback: z.boolean().optional(),
  runon_enabled: z.boolean().optional(),
  /** When false, subagent background requests block up to the hard cap. Default true for multi-turn workers using bash_watch. */
  subagent_background: z.boolean().optional(),
  detach_on_user_message: z.boolean().optional(),
  db_schema_hints: z.boolean().optional(),
  long_running_reminder_enabled: z.boolean().optional(),
  long_running_reminder_interval_ms: z.number().int().positive().optional(),
  foreground_wait_window_ms: z.number().int().positive().optional(),
  /** Maximum synchronous bash_watch wait in milliseconds; clamped to 1000..1800000. Default 120000. */
  watch_sync_max_ms: z.number().int().positive().optional(),
  /** Longest a delegated worker's wait blocks (ms). Default 1800000; below 60000 is a config error. */
  worker_wait_max_ms: z
    .number()
    .int()
    .min(MIN_WORKER_WAIT_MAX_MS, {
      message: `bash.worker_wait_max_ms must be at least ${MIN_WORKER_WAIT_MAX_MS}`,
    })
    .optional(),
  /** Linux-only user-tier opt-in for transient systemd user scopes. Default false. */
  linux_scope: z.boolean().optional(),
  disclaim_privacy: z.boolean().optional(),
  // Pi mirrors the host's optional PowerShell default tool when its API can
  // report that state. This project-safe fallback is used only on older hosts.
  powershell_tool: z.boolean().optional(),
});
const BashConfigSchema = z.union([z.boolean(), BashFeaturesSchema]);

const SandboxConfigSchema = z.object({
  enabled: z.boolean().optional(),
  write_allow: z.array(z.string()).optional(),
  read_deny: z.array(z.string()).optional(),
});

const BridgeConfigSchema = z.object({
  request_timeout_ms: z
    .number()
    .int()
    .min(1000, { message: "bridge.request_timeout_ms must be at least 1000" })
    .optional(),
  hang_threshold: z
    .number()
    .int()
    .min(1, { message: "bridge.hang_threshold must be at least 1" })
    .optional(),
});

const SubcConfigSchema = z.object({
  /** User-tier root-reaper switch; project config cannot set or override it. */
  client_reaper: z.boolean().optional(),
  connection_file: z.string().optional(),
});

const GhShimConfigSchema = z.object({
  /** User-tier AFT image used by the managed `gh` entry. Defaults to the running image. */
  binary_path: z
    .string()
    .trim()
    .refine(isAbsolute, "gh_shim.binary_path must be absolute")
    .optional(),
});

/**
 * `remote_exec`: whether bash calls may ask, with `runon`, to run on the remote
 * build server. USER-tier only, except that a project may turn it off for
 * itself (never on). `default_demand` is the runner demand a `runon` call
 * without specifics runs under; it never makes a call remote by itself.
 */
const RemoteExecConfigSchema = z.object({
  enabled: z.boolean().optional(),
  default_demand: z.string().optional(),
});

const OpenCodeHostConfigSchema = z.object({
  server_url: z.string().optional(),
  server_password_env: z.string().optional(),
});

const GithubConfigSchema = z.object({
  /** Interpose the governed `gh` shim in agent child PATHs. Default: true. */
  shim: z.boolean().optional(),
  /** Allow structured issue:// and pr:// reads. Default: false. */
  read: z.boolean().optional(),
  /** Allow issue and pull-request comment writes. Default: false. */
  write: z.boolean().optional(),
});

const IndexesConfigSchema = z.object({
  trigram: z.boolean().optional(),
  semantic: z.boolean().optional(),
  callgraph: z.boolean().optional(),
});

const GitCoAuthorSchema = z
  .string()
  .trim()
  .refine(
    (value) =>
      value === "off" || value === "auto" || /^[^<>\r\n]+\s+<[^<>\s@]+@[^<>\s@]+>$/.test(value),
    "git.co_author must be 'off', 'auto', or an explicit 'Name <email>' identity",
  );

const GitConfigSchema = z.object({
  /** Attribution injected into Git commits made by AFT-spawned agent children. */
  co_author: GitCoAuthorSchema.optional(),
});

const InspectConfigSchema = z.object({
  enabled: z.boolean().optional(),
  diagnostics_timeout_ms: z
    .number()
    .int()
    .positive()
    .optional()
    .transform((value) =>
      value === undefined ? undefined : clampInspectDiagnosticsTimeoutMs(value),
    ),
  tier2_idle_minutes: z.number().min(0).optional(),
  tier2_pass_timeout_ms: z.number().int().positive().optional(),
  categories: z
    .object({
      diagnostics: z.boolean().optional(),
      todos: z.boolean().optional(),
      dead_code: z.boolean().optional(),
      unused_exports: z.boolean().optional(),
      duplicates: z.boolean().optional(),
      cycles: z.boolean().optional(),
      complexity: z.boolean().optional(),
    })
    .strict()
    .optional(),
  duplicates: z
    .object({
      expected_mirrors: z
        .array(z.tuple([z.string().trim().min(1), z.string().trim().min(1)]))
        .optional(),
    })
    .optional(),
});

function clampIdleRootTtlMinutes(value: number): number {
  return Math.min(30, Math.max(5, value));
}

const IdleConfigSchema = z.object({
  root_ttl_minutes: z
    .number()
    .int()
    .optional()
    .transform((value) => (value === undefined ? undefined : clampIdleRootTtlMinutes(value))),
});

const ViewsConfigSchema = z.object({
  /** Enable content-addressed index views. Default: false. */
  enabled: z.boolean().optional(),
});

const WorktreeConfigSchema = z.object({
  /**
   * When true, a linked worktree applies local file-watcher events to the
   * in-RAM trigram delta (and symbol-cache invalidation) so search reflects
   * edits in that worktree. Default: false. Never writes the shared on-disk
   * index. Semantic search and callgraph stay frozen.
   */
  ram_overlay: z.boolean().optional(),
});

const BackupConfigSchema = z.object({
  enabled: z.boolean().optional(),
  max_depth: z.number().int().positive().optional(),
  /** Skip backup capture above 64 MiB by default. Zero disables snapshots. */
  max_file_size: z.number().int().nonnegative().optional(),
});

const AftConfigFieldsSchema = z.object({
  /**
   * Optional JSON Schema URL for editor tooling. Ignored by the plugin at
   * runtime — only present so VS Code/Cursor/etc. pick up the published
   * schema for autocomplete + validation. `aft setup` auto-inserts this.
   */
  $schema: z.string().optional(),
  /** Select the edit/read surface. `hashline` exposes tagged reads and `{ patch }` edits. */
  edit_mode: z.enum(["default", "hashline"]).optional(),
  /**
   * Whether to auto-format files after edits. Default: false — formatting can
   * reflow the file under the agent and stale the next edit's context. Opt in
   * with `true` if you want AFT to format after edits.
   */
  format_on_edit: z.boolean().optional(),
  formatter_timeout_secs: z.number().int().min(1).max(600).optional(),
  validate_on_edit: z.enum(["syntax", "full"]).optional(),
  formatter: z.record(z.string(), FormatterEnum).optional(),
  checker: z.record(z.string(), CheckerEnum).optional(),
  configure_warnings_delivery: ConfigureWarningsDeliveryEnum.optional(),
  disabled_tools: z.array(z.string()).optional(),
  restrict_to_project_root: z.boolean().optional(),
  indexes: IndexesConfigSchema.optional(),
  /** User-configured filesystem roots for indexed search; project config cannot change them. */
  index: IndexConfigSchema.optional(),
  views: ViewsConfigSchema.optional(),
  callgraph_chunk_size: z.number().optional(),
  inspect: InspectConfigSchema.optional(),
  idle: IdleConfigSchema.optional(),
  backup: BackupConfigSchema.optional(),
  worktree: WorktreeConfigSchema.optional(),
  sandbox: SandboxConfigSchema.optional(),
  /**
   * Bash runtime configuration. Three shapes: `true`, `false` (runtime gate
   * off), or `{ enabled?, rewrite?, compress?, background?, ... }`.
   * Replaces `experimental.bash.*` (still accepted for backward compat).
   */
  bash: BashConfigSchema.optional(),
  experimental: ExperimentalConfigSchema.optional(),
  lsp: LspConfigSchema.optional(),
  url_fetch_allow_private: z.boolean().optional(),
  semantic: SemanticConfigSchema.optional(),
  search: SearchConfigSchema.optional(),
  bridge: BridgeConfigSchema.optional(),
  subc: SubcConfigSchema.optional(),
  opencode: OpenCodeHostConfigSchema.optional(),
  github: GithubConfigSchema.optional(),
  gh_shim: GhShimConfigSchema.optional(),
  remote_exec: RemoteExecConfigSchema.optional(),
  git: GitConfigSchema.optional(),
});

const HarnessOverrideSchema = z.preprocess((value) => {
  if (!isConfigRecord(value) || !Object.hasOwn(value, "harnesses")) return value;
  const { harnesses: _nestedHarnesses, ...override } = value;
  return override;
}, AftConfigFieldsSchema.strict());

export const AftConfigSchema = z.preprocess(
  normalizeConfigForActiveHarness,
  AftConfigFieldsSchema.extend({
    harnesses: z.record(z.string(), HarnessOverrideSchema).optional(),
  }).strict(),
);

type AftConfigFields = z.infer<typeof AftConfigFieldsSchema>;

/**
 * Apply the active harness block. Disables only accumulate (a harness block
 * can add names, never re-enable a base disable); a user harness block
 * overrides index switches, a project harness block can only turn them off.
 */
function applyActiveHarnessOverride(config: AftConfig, trusted: boolean): AftConfig {
  const { harnesses: _harnesses, ...base } = config;
  const override = config.harnesses?.[ACTIVE_HARNESS];
  if (override === undefined) return base as AftConfigFields;
  const merged = { ...base, ...override } as AftConfigFields;
  const disabled = unionDisabledTools(base.disabled_tools, override.disabled_tools);
  if (disabled === undefined) delete merged.disabled_tools;
  else merged.disabled_tools = disabled;
  const indexes = mergeIndexes(base.indexes, override.indexes, !trusted);
  if (indexes === undefined) delete merged.indexes;
  else merged.indexes = indexes;
  return merged;
}

function normalizeLspExtension(extension: string): string {
  return extension.trim().replace(/^\.+/, "");
}

export function resolveLspConfigForConfigure(config: AftConfig): ConfigureLspOverrides {
  const overrides: ConfigureLspOverrides = {};
  const disabled = new Set(config.lsp?.disabled ?? []);
  let experimentalTy = config.experimental?.lsp_ty;

  // Server IDs match Rust's `ServerKind::id_str()` — built-in Pyright is
  // identified as "python", and the experimental Astral checker as "ty".
  // Custom IDs are case-insensitive.
  switch (config.lsp?.python ?? "auto") {
    case "ty":
      experimentalTy = true;
      disabled.add("python");
      break;
    case "pyright":
      experimentalTy = false;
      disabled.add("ty");
      break;
    case "auto":
      break;
  }

  if (experimentalTy !== undefined) {
    overrides.experimental_lsp_ty = experimentalTy;
  }

  const servers = Object.entries(config.lsp?.servers ?? {}).map(([id, server]) => {
    const entry: LspServerConfig = {
      id,
      args: server.args,
      root_markers: server.root_markers,
      disabled: server.disabled,
    };
    if (server.extensions && server.extensions.length > 0) {
      entry.extensions = server.extensions.map(normalizeLspExtension);
    }
    if (server.binary) {
      entry.binary = server.binary;
    }
    if (server.env && Object.keys(server.env).length > 0) {
      entry.env = server.env;
    }
    if (server.initialization_options !== undefined) {
      entry.initialization_options = server.initialization_options;
    }
    return entry;
  });
  if (servers.length > 0) {
    overrides.lsp_servers = servers;
  }

  if (disabled.size > 0) {
    overrides.disabled_lsp = [...disabled];
  }

  return overrides;
}

/**
 * Build the configure overrides that can legitimately differ per project.
 *
 * Pi runs one project per plugin process today, but keeping this shape in
 * parity with OpenCode's `resolveProjectOverridesForConfigure` prevents drift
 * in the Rust configure payload and keeps project-safe config forwarding in one
 * place.
 */
export function resolveProjectOverridesForConfigure(config: AftConfig): Record<string, unknown> {
  const overrides: Record<string, unknown> = {};

  // Forward the resolved disabled list; the engine answers registration
  // questions (steering text, hashline slots) from the same list.
  overrides.disabled_tools = resolvedDisabledTools(config);

  if (config.edit_mode !== undefined) overrides.hashline_enabled = config.edit_mode === "hashline";
  if (config.format_on_edit !== undefined) overrides.format_on_edit = config.format_on_edit;
  if (config.formatter_timeout_secs !== undefined)
    overrides.formatter_timeout_secs = config.formatter_timeout_secs;
  if (config.validate_on_edit !== undefined) overrides.validate_on_edit = config.validate_on_edit;
  if (config.formatter !== undefined) overrides.formatter = config.formatter;
  if (config.checker !== undefined) overrides.checker = config.checker;

  overrides.restrict_to_project_root = config.restrict_to_project_root ?? false;

  overrides.indexes = resolvedIndexes(config);
  if (config.index !== undefined) overrides.index = config.index;
  if (config.views !== undefined) overrides.views = config.views;
  if (config.callgraph_chunk_size !== undefined)
    overrides.callgraph_chunk_size = config.callgraph_chunk_size;

  Object.assign(overrides, resolveExperimentalConfigForConfigure(config));
  if (
    typeof config.bash === "object" &&
    (config.bash.enabled !== undefined ||
      config.bash.runon_enabled !== undefined ||
      config.bash.host_fallback !== undefined ||
      config.bash.detach_on_user_message !== undefined ||
      config.bash.db_schema_hints !== undefined ||
      config.bash.watch_sync_max_ms !== undefined ||
      config.bash.worker_wait_max_ms !== undefined ||
      config.bash.disclaim_privacy !== undefined ||
      config.bash.powershell_tool !== undefined)
  ) {
    overrides.bash = {
      ...(config.bash.disclaim_privacy !== undefined
        ? { disclaim_privacy: config.bash.disclaim_privacy }
        : {}),
      ...(config.bash.enabled !== undefined ? { enabled: config.bash.enabled } : {}),
      ...(config.bash.runon_enabled !== undefined
        ? { runon_enabled: config.bash.runon_enabled }
        : {}),
      ...(config.bash.host_fallback !== undefined
        ? { host_fallback: config.bash.host_fallback }
        : {}),
      ...(config.bash.detach_on_user_message !== undefined
        ? { detach_on_user_message: config.bash.detach_on_user_message }
        : {}),
      ...(config.bash.db_schema_hints !== undefined
        ? { db_schema_hints: config.bash.db_schema_hints }
        : {}),
      ...(config.bash.watch_sync_max_ms !== undefined
        ? { watch_sync_max_ms: config.bash.watch_sync_max_ms }
        : {}),
      ...(config.bash.worker_wait_max_ms !== undefined
        ? { worker_wait_max_ms: config.bash.worker_wait_max_ms }
        : {}),
      ...(config.bash.powershell_tool !== undefined
        ? { powershell_tool: config.bash.powershell_tool }
        : {}),
    };
  }
  Object.assign(overrides, resolveLspConfigForConfigure(config));
  if (config.lsp?.idle_minutes !== undefined) overrides.lsp_idle_minutes = config.lsp.idle_minutes;
  if (config.semantic !== undefined) overrides.semantic = config.semantic;
  const rerank = definedEntries(config.search?.rerank);
  if (rerank !== undefined) overrides.search = { rerank };
  if (config.inspect !== undefined) overrides.inspect = config.inspect;
  if (config.idle !== undefined) overrides.idle = config.idle;
  if (config.backup !== undefined) overrides.backup = config.backup;
  if (config.worktree !== undefined) overrides.worktree = config.worktree;
  if (config.sandbox !== undefined) overrides.sandbox = config.sandbox;
  if (config.github !== undefined) overrides.github = resolveGithubConfig(config);
  if (config.bash === false) overrides.bash = { enabled: false };
  if (config.git !== undefined) overrides.git = config.git;
  if (config.remote_exec !== undefined) {
    const remoteExec = config.remote_exec;
    overrides.remote_exec = {
      enabled: remoteExec.enabled === true && remoteExec.project_off !== true,
      ...(remoteExec.default_demand !== undefined
        ? { default_demand: remoteExec.default_demand }
        : {}),
      ...(remoteExec.project_off === true ? { project_off: true } : {}),
    };
  }

  return overrides;
}

export function resolveExperimentalConfigForConfigure(
  config: AftConfig,
): ConfigureExperimentalOverrides {
  const overrides: ConfigureExperimentalOverrides = {};

  // Bash sub-features always flow through `resolveBashConfig` now — that's
  // the single source of truth across top-level `bash`, legacy
  // `experimental.bash.*`, and surface defaults. See the resolver above.
  const bash = resolveBashConfig(config);
  overrides.experimental_bash_rewrite = bash.rewrite;
  overrides.experimental_bash_compress = bash.compress;
  overrides.experimental_bash_background = bash.background;
  if (bash.long_running_reminder_enabled !== undefined) {
    overrides.bash_long_running_reminder_enabled = bash.long_running_reminder_enabled;
  }
  if (bash.long_running_reminder_interval_ms !== undefined) {
    overrides.bash_long_running_reminder_interval_ms = bash.long_running_reminder_interval_ms;
  }

  // lsp_ty stays nested under experimental — it didn't graduate.
  if (config.experimental?.lsp_ty !== undefined) {
    overrides.experimental_lsp_ty = config.experimental.lsp_ty;
  }
  return overrides;
}

type Logger = {
  log: (message: string) => void;
  warn: (message: string) => void;
};

// ---------------------------------------------------------------------------
// Config file detection (.jsonc preferred over .json)
// ---------------------------------------------------------------------------

export type ConfigLoadError = { path: string; message: string };

let configLoadErrors: ConfigLoadError[] = [];
let configValidationErrors: ConfigLoadError[] = [];
let configLoadSources: string[] = [];
let configLoadTexts = new Map<string, string>();

/**
 * The config files the last load actually read, in load order. A file that
 * did not exist when the loader looked is not listed. A live config reload
 * compares this with the previous accepted load to tell that a file it relied
 * on has disappeared, rather than probing the filesystem separately.
 */
export function getConfigLoadSources(): readonly string[] {
  return configLoadSources;
}

/** The text of each file the last load read, keyed by path. */
export function getConfigLoadTexts(): ReadonlyMap<string, string> {
  return configLoadTexts;
}

/**
 * Settings the last load dropped because their value did not validate. A
 * normal load keeps the rest of the file and uses defaults for these; a live
 * config reload treats any of them as an invalid file and keeps the last valid
 * config instead, so a typo cannot reset a key while the host runs.
 */
export function getConfigValidationErrors(): readonly ConfigLoadError[] {
  return configValidationErrors;
}

export function getConfigLoadErrors(): readonly ConfigLoadError[] {
  return configLoadErrors;
}

export function formatConfigParseFailureMessage(configPath: string, errorMessage: string): string {
  return (
    `AFT config at ${configPath} failed to parse and was ignored (running on defaults): ${errorMessage}. ` +
    "Fix the syntax or run `npx @cortexkit/aft doctor`."
  );
}

function recordConfigParseFailure(configPath: string, errorMessage: string): void {
  configLoadErrors.push({ path: configPath, message: errorMessage });
  warn(formatConfigParseFailureMessage(configPath, errorMessage));
}

function warnIgnoredHarnessSpecificConfigKeys(
  config: Record<string, unknown>,
  configPath: string,
): void {
  const ignored = OPENCODE_ONLY_KEYS.filter((key) => Object.hasOwn(config, key));
  if (ignored.length === 0) return;
  warn(
    `Ignoring OpenCode-only config key${ignored.length === 1 ? "" : "s"} ${ignored.map((key) => `\`${key}\``).join(", ")} in ${configPath}; ${ignored.length === 1 ? "it has" : "they have"} no effect on the Pi harness.`,
  );
}

/** One migration notice awaiting delivery by the plugin. */
export interface ConfigLoadNotice {
  configPath: string;
  digest: string;
  message: string;
}

let configLoadNotices: ConfigLoadNotice[] = [];

/** Whether any tier of the current load supplied a semantic index input. */
let semanticInputSupplied = false;

/** Migration notices from the most recent {@link loadAftConfig} call. */
export function getConfigLoadNotices(): readonly ConfigLoadNotice[] {
  return configLoadNotices;
}

/** Deliver the last load's notices once per notice identity across restarts. */
export function deliverConfigLoadNotices(
  deliver: (message: string) => void,
  notices: readonly ConfigLoadNotice[] = configLoadNotices,
): void {
  for (const notice of notices) deliverMigrationNoticeOnce({ ...notice, deliver });
}

function loadConfigFromPath(configPath: string, tier: "user" | "project"): AftConfig | null {
  let cleanConfig: Record<string, unknown>;
  try {
    if (!existsSync(configPath)) return null;
    const content = readFileSync(configPath, "utf-8");
    configLoadSources.push(configPath);
    configLoadTexts.set(configPath, content);
    const rawConfig = parseJsonc<Record<string, unknown>>(content);
    if (!rawConfig || typeof rawConfig !== "object" || Array.isArray(rawConfig)) {
      recordConfigParseFailure(configPath, "root must be an object");
      return null;
    }
    // comment-json attaches Symbol(before/after:<key>) props to track comments.
    // Zod stringifies keys when building error paths, which throws on those
    // symbols and would silently drop the whole config to defaults (issue #88).
    // Validate against a symbol-free deep copy.
    cleanConfig = stripJsoncSymbols(rawConfig);
  } catch (err) {
    const errorMsg = err instanceof Error ? err.message : String(err);
    error(`Error loading config from ${configPath}: ${errorMsg}`);
    recordConfigParseFailure(configPath, errorMsg);
    return null;
  }

  // Retired keys are translated on the raw document, before schema
  // validation, so they never reach Zod and never fail the load.
  const projection = noticeProjection(structuredClone(cleanConfig));
  if (suppliesSemanticIndexInput(cleanConfig, ACTIVE_HARNESS)) semanticInputSupplied = true;
  const translation = translateConfigDocument(cleanConfig, tier);
  for (const warning of translation.warnings) {
    const text = `Config ${configPath} [${warning.key}]: ${warning.message} (${warning.code})`;
    if (warning.once) {
      // A note about an already-fixed file: deliver it once per input
      // identity like the migration notices, not on every load.
      configLoadNotices.push({
        configPath,
        digest: `${noticeDigest(projection)}:${warning.code}:${warning.key}`,
        message: text,
      });
    } else {
      warn(text);
    }
  }
  if (translation.legacyInput && tier === "user") {
    // The AFT binary rewrites the user file to current keys when its
    // configure next reads the file, and reports that rewrite to the user as
    // a `config_migrated` configure warning. Queuing a notice here as well
    // would show the user two notices, so the plugin only logs.
    log(
      `Config ${configPath} uses retired keys (${translation.retiredKeys.join(", ")}); applied their current equivalents in memory`,
    );
  } else if (translation.legacyInput) {
    configLoadNotices.push({
      configPath,
      digest: noticeDigest(projection),
      message: legacyConfigNoticeMessage(configPath, translation),
    });
  }

  warnIgnoredHarnessSpecificConfigKeys(cleanConfig, configPath);
  warnIgnoredNestedHarnesses(cleanConfig, configPath);
  const result = AftConfigSchema.safeParse(cleanConfig);
  let parsed: AftConfig;
  if (result.success) {
    log(`Config loaded from ${configPath}`);
    parsed = result.data;
  } else {
    const errorMsg = result.error.issues.map((i) => `${i.path.join(".")}: ${i.message}`).join(", ");
    warn(`Config validation error in ${configPath}: ${errorMsg}`);
    configValidationErrors.push({ path: configPath, message: errorMsg });
    parsed = parseConfigPartially(cleanConfig);
  }
  if (tier === "user" && parsed.disabled_tools === undefined) {
    // The absent-base default applies once, to the user base, before the
    // harness block and the project tier are merged.
    parsed = { ...parsed, disabled_tools: [...DEFAULT_DISABLED_TOOLS] };
  }
  return applyActiveHarnessOverride(parsed, tier === "user");
}

function parseConfigPartially(rawConfig: Record<string, unknown>): AftConfig {
  let configForParsing = rawConfig;
  if (
    Object.hasOwn(rawConfig, "edit_mode") &&
    rawConfig.edit_mode !== "default" &&
    rawConfig.edit_mode !== "hashline"
  ) {
    warn(
      `Unknown edit_mode value ${JSON.stringify(rawConfig.edit_mode)}; falling back to "default"`,
    );
    configForParsing = { ...rawConfig, edit_mode: "default" };
  }

  const partialConfig: Record<string, unknown> = {};
  const invalidSections: string[] = [];

  for (const key of Object.keys(configForParsing)) {
    const sectionResult = AftConfigSchema.safeParse({ [key]: configForParsing[key] });
    if (sectionResult.success) {
      const parsed = sectionResult.data as Record<string, unknown>;
      if (parsed[key] !== undefined) {
        partialConfig[key] = parsed[key];
      }
    } else {
      const sectionErrors = sectionResult.error.issues
        .filter((i) => i.path[0] === key)
        .map((i) => `${i.path.join(".")}: ${i.message}`)
        .join(", ");
      if (sectionErrors) {
        invalidSections.push(`${key}: ${sectionErrors}`);
      }
    }
  }

  if (invalidSections.length > 0) {
    warn(`Partial config loaded — invalid sections skipped: ${invalidSections.join("; ")}`);
  }

  return partialConfig as AftConfig;
}

// ---------------------------------------------------------------------------
// Merge configs (project overrides user, deep-merge nested maps)
// ---------------------------------------------------------------------------

function mergeSemanticConfig(
  base?: SemanticConfig,
  override?: SemanticConfig,
): SemanticConfig | undefined {
  // SECURITY: Only safe fields from project override are merged.
  // Sensitive fields (backend, base_url, api_key_env) must come from user config.
  const projectSafe: SemanticConfig = {};
  if (override?.model !== undefined) projectSafe.model = override.model;
  if (override?.timeout_ms !== undefined) projectSafe.timeout_ms = override.timeout_ms;
  if (override?.max_batch_size !== undefined) projectSafe.max_batch_size = override.max_batch_size;
  if (override?.max_files !== undefined) projectSafe.max_files = override.max_files;

  const semantic: SemanticConfig = { ...base, ...projectSafe };
  if (Object.values(semantic).every((v) => v === undefined)) return undefined;

  return Object.fromEntries(
    Object.entries(semantic).filter(([, v]) => v !== undefined),
  ) as SemanticConfig;
}

function mergeLspConfig(base?: LspConfig, override?: LspConfig): LspConfig | undefined {
  // STRICT ALLOWLIST: only safe fields from project override are honored.
  //
  // EXECUTABLE-ORIGIN fields (servers, versions, auto_install, grace_days)
  // must come from user config — a hostile repo could otherwise specify
  // which binary AFT installs and runs (audit v0.17 #1).
  //
  // ATTACK-DEFENSE fields (disabled) cannot be set from project config
  // either — a hostile repo could silently disable LSP servers the user
  // relies on, suppressing diagnostics for its own malicious code
  // (audit v0.17 #5).
  //
  // SAFE project-level fields: python (per-language preference) and
  // diagnostics_on_edit (agent workflow/latency preference only).
  const projectSafe: LspConfig = {};
  if (override?.python !== undefined) projectSafe.python = override.python;
  if (override?.diagnostics_on_edit !== undefined) {
    projectSafe.diagnostics_on_edit = override.diagnostics_on_edit;
  }
  const floor = base?.idle_minutes ?? 60;
  const idle = override?.idle_minutes;
  if (idle !== undefined && (floor === "never" || (idle !== "never" && idle <= floor))) {
    projectSafe.idle_minutes = idle;
  }

  // disabled comes from user config ONLY.
  const userDisabled = base?.disabled ?? [];
  const lsp: LspConfig = {
    ...base,
    ...projectSafe,
    ...(userDisabled.length > 0 ? { disabled: [...userDisabled] } : {}),
  };

  if (Object.values(lsp).every((v) => v === undefined)) return undefined;

  return Object.fromEntries(Object.entries(lsp).filter(([, v]) => v !== undefined)) as LspConfig;
}

/** Merge ordinary worktree options from user and project tiers. */
function mergeWorktreeConfig(
  baseWorktree: AftConfig["worktree"],
  overrideWorktree: AftConfig["worktree"],
): AftConfig["worktree"] {
  if (baseWorktree === undefined && overrideWorktree === undefined) return undefined;
  if (overrideWorktree === undefined) return baseWorktree;
  if (baseWorktree === undefined) return overrideWorktree;
  return { ...baseWorktree, ...overrideWorktree };
}

function mergeInspectConfig(
  baseInspect: AftConfig["inspect"],
  overrideInspect: AftConfig["inspect"],
): AftConfig["inspect"] {
  const diagnosticsTimeoutConfigured =
    baseInspect?.diagnostics_timeout_ms !== undefined ||
    overrideInspect?.diagnostics_timeout_ms !== undefined;
  // A project may ask for more time, but it must not silently shrink another
  // consumer's diagnostic completeness by reducing the user's effective wait.
  const diagnosticsTimeoutMs = Math.max(
    clampInspectDiagnosticsTimeoutMs(
      baseInspect?.diagnostics_timeout_ms ?? DEFAULT_INSPECT_DIAGNOSTICS_TIMEOUT_MS,
    ),
    clampInspectDiagnosticsTimeoutMs(
      overrideInspect?.diagnostics_timeout_ms ?? DEFAULT_INSPECT_DIAGNOSTICS_TIMEOUT_MS,
    ),
  );
  const inspect = {
    ...baseInspect,
    ...overrideInspect,
    categories:
      baseInspect?.categories || overrideInspect?.categories
        ? Object.fromEntries([
            ...Object.entries(baseInspect?.categories ?? {}),
            ...Object.entries(overrideInspect?.categories ?? {}).map(([key, enabled]) => [
              key,
              enabled &&
                (baseInspect?.categories as Record<string, boolean | undefined> | undefined)?.[
                  key
                ] !== false,
            ]),
          ])
        : undefined,
    ...(diagnosticsTimeoutConfigured ? { diagnostics_timeout_ms: diagnosticsTimeoutMs } : {}),
    duplicates:
      baseInspect?.duplicates || overrideInspect?.duplicates
        ? {
            ...baseInspect?.duplicates,
            ...overrideInspect?.duplicates,
          }
        : undefined,
  };
  if (Object.values(inspect).every((value) => value === undefined)) {
    return undefined;
  }
  return Object.fromEntries(
    Object.entries(inspect).filter(([, value]) => value !== undefined),
  ) as AftConfig["inspect"];
}

/**
 * Whether bash offers `runon`: only in subc mode (the remote runner is reached
 * through the daemon), and only when the user config enables remote runs and
 * the project has not turned them off. Decided once, when the tool is built.
 */
export function remoteRunsOffered(config: AftConfig): boolean {
  return (
    process.platform !== "win32" &&
    resolveBashConfig(config).runon_enabled &&
    Boolean(config.subc?.connection_file?.trim()) &&
    config.remote_exec?.enabled === true &&
    config.remote_exec.project_off !== true
  );
}

/**
 * Merge `remote_exec`: the user tier decides it, and a project may only turn it
 * off (recorded as `project_off`, so the refusal can name the project).
 */
function mergeRemoteExecConfig(
  base: RemoteExecConfig | undefined,
  project: RemoteExecConfig | undefined,
): RemoteExecConfig | undefined {
  if (project?.enabled !== false) return base;
  return { ...base, enabled: false, project_off: true };
}

function mergeSandboxConfig(
  base?: SandboxConfig,
  project?: SandboxConfig,
): SandboxConfig | undefined {
  const readDeny = [...new Set([...(base?.read_deny ?? []), ...(project?.read_deny ?? [])])];
  const merged = {
    ...base,
    ...(readDeny.length > 0 ? { read_deny: readDeny } : {}),
    // A project may ENABLE the sandbox for itself (hardening is one-way);
    // project enabled:false can never switch off what the user turned on.
    ...(project?.enabled === true ? { enabled: true } : {}),
  };
  return Object.keys(merged).length > 0 ? merged : undefined;
}

/** Merge top-level `bash` config, expanding booleans before per-field merge. */
function mergeBashConfig(
  baseBash: AftConfig["bash"],
  overrideBash: AftConfig["bash"],
): AftConfig["bash"] {
  if (baseBash === undefined && overrideBash === undefined) return undefined;
  if (baseBash === undefined) return overrideBash;
  if (overrideBash === undefined) return baseBash;

  const expand = (value: AftConfig["bash"]): Record<string, unknown> => {
    if (value === true) return { rewrite: true, compress: true, background: true };
    if (value === false) return { rewrite: false, compress: false, background: false };
    return { ...(value ?? {}) };
  };

  return { ...expand(baseBash), ...expand(overrideBash) };
}

function mergeExperimentalConfig(
  base?: ExperimentalConfig,
  override?: ExperimentalConfig,
): ExperimentalConfig | undefined {
  const bash: Record<string, unknown> = {
    ...base?.bash,
    ...override?.bash,
  };
  const experimental: Record<string, unknown> = {
    ...base,
    ...override,
  };

  if (Object.values(bash).some((value) => value !== undefined)) {
    experimental.bash = bash;
  } else {
    delete experimental.bash;
  }
  if (Object.values(experimental).every((value) => value === undefined)) return undefined;

  return Object.fromEntries(
    Object.entries(experimental).filter(([, value]) => value !== undefined),
  ) as ExperimentalConfig;
}

function getProjectLspStrippedKeys(lsp?: LspConfig): string[] {
  if (!lsp) return [];

  const strippedKeys: string[] = [];
  if (lsp.servers !== undefined) strippedKeys.push("lsp.servers");
  if (lsp.versions !== undefined) strippedKeys.push("lsp.versions");
  if (lsp.auto_install !== undefined) strippedKeys.push("lsp.auto_install");
  if (lsp.grace_days !== undefined) strippedKeys.push("lsp.grace_days");
  if (lsp.disabled !== undefined) strippedKeys.push("lsp.disabled");
  return strippedKeys;
}

/**
 * Top-level fields that are SAFE to inherit from project config.
 *
 * Anything NOT in this list flows from user config only. This is the
 * strict-allowlist trust boundary — adding a new field requires explicit
 * security review of whether a hostile repo could weaponize it.
 *
 * Previously `restrict_to_project_root` and `url_fetch_allow_private` flowed
 * through the implicit `...safeOverride` spread, allowing project config to
 * weaken security boundaries.
 *
 * (Note: `storage_dir` is not a config-schema field — the plugin always sets
 * it at configure time. It cannot be set from any aft.jsonc file.)
 */
const PROJECT_SAFE_TOP_LEVEL_FIELDS = new Set<keyof AftConfig>([
  "edit_mode",
  "format_on_edit",
  "validate_on_edit",
  "configure_warnings_delivery",
  // "indexes" handled separately — a project can only switch an index off.
  "views",
  "callgraph_chunk_size",
  "inspect",
  "idle",
  "worktree",
  // Git attribution only changes commit metadata; it grants no capabilities and
  // does not select an executable, so project configuration may override it.
  "git",
  "pi",
  "experimental",
  // Graduated bash family (v0.27.2). Same reasoning as `experimental`:
  // project-settable so users can opt out per-repo (e.g. `bash: false` in a
  // repo with weird shell needs) or opt in. NOT a security boundary — bash
  // hoist disabling is a UX/safety preference, not access control.
  "bash",
  // "disabled_tools" handled separately — unioned via array merge.
  // "formatter"/"checker" handled separately — deep-merged.
  // "semantic"/"lsp" handled separately — strict field-level merge.
  // "inspect"/"worktree" handled separately — deep-merged.
  // "backup" — USER ONLY (project config cannot disable or shrink undo backups).
  // "index" — USER ONLY (a repository must not mint machine standing roots).
  // "restrict_to_project_root" — USER ONLY (security boundary).
  // "url_fetch_allow_private" — USER ONLY (SSRF surface).
  // "bridge" — USER ONLY (governs bridge safety/restart + per-machine transport budget).
  // "github" and its deprecated aliases are USER ONLY because they change
  // capabilities and global tool descriptions. Project-specific surface changes
  // would also destabilize prefix caches.
]);

function pickProjectSafeFields(override: AftConfig): Partial<AftConfig> {
  const safe: Partial<AftConfig> = {};
  for (const key of PROJECT_SAFE_TOP_LEVEL_FIELDS) {
    if (override[key] !== undefined) {
      // biome-ignore lint/suspicious/noExplicitAny: field-by-field copy with key set guarantee
      (safe as any)[key] = override[key];
    }
  }
  return safe;
}

function mergeProjectBackupConfig(
  base: AftConfig["backup"],
  project: AftConfig["backup"],
): AftConfig["backup"] {
  if (project?.max_file_size === undefined) return base;
  return { ...base, max_file_size: project.max_file_size };
}

function mergePiConfig(base?: PiConfig, override?: PiConfig): PiConfig | undefined {
  if (!base && !override) return undefined;
  return { ...base, ...override };
}

function getStrippedTopLevelKeys(override: AftConfig): string[] {
  const stripped: string[] = [];
  if (override.restrict_to_project_root !== undefined) stripped.push("restrict_to_project_root");
  if (override.url_fetch_allow_private !== undefined) stripped.push("url_fetch_allow_private");
  if (override.bridge !== undefined) stripped.push("bridge");
  if (override.backup?.enabled !== undefined || override.backup?.max_depth !== undefined)
    stripped.push("backup");
  if (override.index?.roots !== undefined) stripped.push("index.roots");
  // enabled:true is an accepted project-tier hardening opt-in; only the
  // weakening direction (enabled:false) is stripped as user-only.
  if (override.sandbox?.enabled === false) stripped.push("sandbox.enabled");
  if (typeof override.bash === "object" && override.bash.disclaim_privacy === false)
    stripped.push("bash.disclaim_privacy");
  if (override.sandbox?.write_allow !== undefined) stripped.push("sandbox.write_allow");
  if (override.subc !== undefined) stripped.push("subc");
  if (override.opencode !== undefined) stripped.push("opencode");
  if (override.github !== undefined) stripped.push("github");
  if (override.gh_shim !== undefined) stripped.push("gh_shim");
  if (override.remote_exec?.enabled === true) stripped.push("remote_exec.enabled");
  if (override.remote_exec?.default_demand !== undefined)
    stripped.push("remote_exec.default_demand");
  for (const tool of partitionProjectDisables(override.disabled_tools).ignored) {
    stripped.push(`disabled_tools.${tool}`);
  }
  stripped.push(...projectRerankStrippedKeys(override.search?.rerank));
  return stripped;
}

/** Project rerank keys that are ignored: everything except `backend: "off"`. */
function projectRerankStrippedKeys(rerank: RerankConfig | undefined): string[] {
  if (rerank === undefined) return [];
  const stripped: string[] = [];
  if (rerank.backend !== undefined && rerank.backend !== "off")
    stripped.push("search.rerank.backend");
  for (const key of ["model", "endpoint", "api_key_env", "top_n", "timeout_ms"] as const) {
    if (rerank[key] !== undefined) stripped.push(`search.rerank.${key}`);
  }
  return stripped;
}

/** A project may set `search.rerank.backend: "off"` and nothing else. */
function mergeProjectSearchConfig(
  base: SearchConfig | undefined,
  project: SearchConfig | undefined,
): SearchConfig | undefined {
  if (project?.rerank?.backend !== "off") return base;
  return { ...base, rerank: { ...base?.rerank, backend: "off" } };
}

/** A copy of `value` without its undefined entries, or undefined when none are left. */
function definedEntries<T extends object>(value: T | undefined): Partial<T> | undefined {
  if (value === undefined) return undefined;
  const entries = Object.entries(value).filter(([, entry]) => entry !== undefined);
  return entries.length > 0 ? (Object.fromEntries(entries) as Partial<T>) : undefined;
}

function mergeConfigs(base: AftConfig, override: AftConfig): AftConfig {
  // Project disables union with the user list, except aft_safety and host
  // tool slots, which are ignored and reported by getStrippedTopLevelKeys.
  const disabledTools = unionDisabledTools(
    base.disabled_tools,
    partitionProjectDisables(override.disabled_tools).accepted,
  );
  // A project may switch an index off, never back on.
  const indexes = mergeIndexes(base.indexes, override.indexes, true);
  const formatter = { ...base.formatter, ...override.formatter };
  const checker = { ...base.checker, ...override.checker };
  const semantic = mergeSemanticConfig(base.semantic, override.semantic);
  const search = mergeProjectSearchConfig(base.search, override.search);
  const lsp = mergeLspConfig(base.lsp, override.lsp);
  const experimental = mergeExperimentalConfig(base.experimental, override.experimental);
  const projectBash = typeof override.bash === "object" ? { ...override.bash } : override.bash;
  if (typeof projectBash === "object" && projectBash.disclaim_privacy !== true)
    delete projectBash.disclaim_privacy;
  // Strip only the weakening direction before the usual field-wise merge.
  if (typeof projectBash === "object") delete projectBash.runon_enabled;
  const bash = mergeBashConfig(base.bash, projectBash);
  const inspect = mergeInspectConfig(base.inspect, override.inspect);
  const worktree = mergeWorktreeConfig(base.worktree, override.worktree);
  const sandbox = mergeSandboxConfig(base.sandbox, override.sandbox);
  const remoteExec = mergeRemoteExecConfig(base.remote_exec, override.remote_exec);
  const backup = mergeProjectBackupConfig(base.backup, override.backup);
  const pi = mergePiConfig(base.pi, override.pi);
  const bridge = base.bridge;

  // STRICT ALLOWLIST: only project-safe top-level fields are inherited.
  // See PROJECT_SAFE_TOP_LEVEL_FIELDS above for the full security rationale.
  // We deep-merge `bash` separately so the field-by-field union beats the
  // shallow allowlist spread; otherwise project's `bash: { compress: false }`
  // would wipe out user's `bash: { rewrite: true }`.
  const safeOverride = pickProjectSafeFields(override);
  delete safeOverride.indexes;
  delete safeOverride.bash;
  delete safeOverride.inspect;
  delete safeOverride.worktree;
  delete safeOverride.pi;

  return {
    ...base,
    ...safeOverride,
    ...(Object.keys(formatter).length > 0 ? { formatter } : {}),
    ...(Object.keys(checker).length > 0 ? { checker } : {}),
    ...(lsp ? { lsp } : {}),
    ...(bash !== undefined ? { bash } : {}),
    ...(inspect !== undefined ? { inspect } : {}),
    ...(worktree !== undefined ? { worktree } : {}),
    ...(sandbox !== undefined ? { sandbox } : {}),
    ...(remoteExec !== undefined ? { remote_exec: remoteExec } : {}),
    ...(backup !== undefined ? { backup } : {}),
    ...(pi !== undefined ? { pi } : {}),
    experimental,
    semantic,
    // Only the project-safe rerank disable is merged.
    search,
    ...(bridge !== undefined ? { bridge } : {}),
    ...(indexes !== undefined ? { indexes } : {}),
    ...(disabledTools !== undefined ? { disabled_tools: disabledTools } : {}),
  };
}

/** Defaults for bridge transport when omitted from config. */
export const DEFAULT_BRIDGE_REQUEST_TIMEOUT_MS = 30_000;
export const DEFAULT_BRIDGE_HANG_THRESHOLD = 2;

/** Default the client-side root reaper to disabled when no user configuration is provided; production deployments may enable it through their own configuration. */
export const DEFAULT_SUBC_CLIENT_REAPER = false;
const SUBC_CLIENT_REAPER_PROCESS_KEY = "subc_client_reaper";

export function resolveSubcClientReaper(config: AftConfig): boolean {
  return config.subc?.client_reaper ?? DEFAULT_SUBC_CLIENT_REAPER;
}

/** Resolved pool/bridge options from `config.bridge` (defaults 30000 / 2). */
export function resolveBridgePoolTransportOptions(config: AftConfig): {
  timeoutMs: number;
  hangThreshold: number;
} {
  return {
    timeoutMs: config.bridge?.request_timeout_ms ?? DEFAULT_BRIDGE_REQUEST_TIMEOUT_MS,
    hangThreshold: config.bridge?.hang_threshold ?? DEFAULT_BRIDGE_HANG_THRESHOLD,
  };
}

// ---------------------------------------------------------------------------
// CortexKit config path resolution
//
// Pi and OpenCode now share the same aft.jsonc files.
// ---------------------------------------------------------------------------

export interface ResolvedAftConfigPaths {
  userConfigPath: string;
  projectConfigPath: string;
}

export function migrateAftConfigLocations(
  projectDirectory: string,
  logger: Logger = { log, warn },
): AftConfigFileMigrationResult[] {
  const paths = resolveCortexKitConfigPaths(projectDirectory);
  const legacy = resolveLegacyAftConfigSources(projectDirectory);
  return [
    migrateLegacyAftConfigFile({
      scope: "user",
      targetPath: paths.userConfigPath,
      legacySources: legacy.user,
      operatingHarness: "pi",
      logger,
    }),
    migrateLegacyAftConfigFile({
      scope: "project",
      targetPath: paths.projectConfigPath,
      legacySources: legacy.project,
      operatingHarness: "pi",
      logger,
    }),
  ];
}

export function resolveAftConfigPaths(projectDirectory: string): ResolvedAftConfigPaths {
  return resolveCortexKitConfigPaths(projectDirectory);
}

export function buildConfigTierConfigureParams(
  projectDirectory: string,
  processState: Record<string, unknown> = {},
): Record<string, unknown> & {
  config: ConfigTier[];
  cortexkit_user_config_path: string;
  subc_client_reaper?: boolean;
} {
  const paths = resolveAftConfigPaths(projectDirectory);
  // Only plugin initialization supplies process state. Keep this user-tier
  // lifecycle switch out of per-project configure payloads, where it cannot
  // change an existing registration.
  const initialPluginState = Object.keys(processState).length > 0;
  return {
    ...processState,
    ...(initialPluginState
      ? {
          [SUBC_CLIENT_REAPER_PROCESS_KEY]: resolveSubcClientReaper(
            loadAftConfig(projectDirectory),
          ),
        }
      : {}),
    cortexkit_user_config_path: paths.userConfigPath,
    config: readConfigTiers(paths),
  };
}

// ---------------------------------------------------------------------------
// Public API: loadAftConfig
// ---------------------------------------------------------------------------

/**
 * Load and resolve the user and project config for one project. The result
 * always carries a sorted `disabled_tools` list and fully resolved `indexes`.
 * Retired keys are translated, never refused. Throws {@link ConfigRejectedError}
 * when the resolved configuration is incomplete.
 */
export function loadAftConfig(projectDirectory: string): AftConfig {
  configLoadErrors = [];
  configValidationErrors = [];
  configLoadSources = [];
  configLoadTexts = new Map();
  configLoadNotices = [];
  semanticInputSupplied = false;

  const { userConfigPath, projectConfigPath } = resolveAftConfigPaths(projectDirectory);

  // A missing or unreadable user file behaves like `{}`, which still receives
  // the absent-base disabled default.
  let config: AftConfig = loadConfigFromPath(userConfigPath, "user") ?? {
    disabled_tools: [...DEFAULT_DISABLED_TOOLS],
  };

  const projectConfig = loadConfigFromPath(projectConfigPath, "project");
  if (projectConfig) {
    if (
      projectConfig.semantic?.backend !== undefined ||
      projectConfig.semantic?.base_url !== undefined ||
      projectConfig.semantic?.api_key_env !== undefined
    ) {
      warn(
        "Ignoring semantic.backend/base_url/api_key_env from project config (security: use user config for external backends)",
      );
    }
    const strippedLspKeys = getProjectLspStrippedKeys(projectConfig.lsp);
    if (strippedLspKeys.length > 0) {
      warn(
        `Ignoring ${strippedLspKeys.join(", ")} from project config ${projectConfigPath} (security: these LSP settings only honor user-level config)`,
      );
    }
    const strippedTopLevelKeys = getStrippedTopLevelKeys(projectConfig);
    if (strippedTopLevelKeys.length > 0) {
      warn(
        `Ignoring ${strippedTopLevelKeys.join(", ")} from project config ${projectConfigPath} (security: these settings only honor user-level config — a project should not weaken security boundaries for the user)`,
      );
    }
    config = mergeConfigs(config, projectConfig);
  }

  if (config.disabled_tools === undefined) {
    // Unreachable by construction; reject rather than silently enable every tool.
    throw new ConfigRejectedError(["invalid_resolved_config:missing:disabled_tools"]);
  }
  const resolved: AftConfig = {
    ...config,
    disabled_tools: sortedUnique(config.disabled_tools),
    indexes: resolveIndexes(config.indexes),
  };
  const invalid = validateResolvedConfig(resolved);
  if (invalid.length > 0) throw new ConfigRejectedError(invalid);
  // Queued with the migration notices, which the adapters deliver before any
  // ONNX Runtime download or index work starts.
  const costNotice = semanticCostNotice({
    userConfigPath,
    configFileLoaded: existsSync(userConfigPath) || existsSync(projectConfigPath),
    semanticEffective: resolved.indexes?.semantic === true,
    semanticInputSupplied,
    semanticBackend: resolved.semantic?.backend,
  });
  if (costNotice !== null) configLoadNotices.push(costNotice);
  return resolved;
}
