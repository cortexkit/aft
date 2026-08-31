import { existsSync, readFileSync, renameSync, unlinkSync, writeFileSync } from "node:fs";
import { homedir } from "node:os";
import { isAbsolute } from "node:path";
import {
  type AftConfigFileMigrationResult,
  type ConfigTier,
  migrateAftConfigFile as migrateLegacyAftConfigFile,
  OPENCODE_ONLY_KEYS,
  readConfigTiers,
  resolveCortexKitConfigPaths,
  resolveLegacyAftConfigSources,
  stripHarnessSpecificConfigKeys,
  stripJsoncSymbols,
} from "@cortexkit/aft-bridge";
import { parse as parseJsonc, stringify as stringifyJsonc } from "comment-json";
import { z } from "zod";

import { error, log, warn } from "./logger.js";

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
  /** Operator hard-off for child PATH injection and governed `gh` routing. */
  enabled?: boolean;
  /** User-tier AFT image used by the managed `gh` entry. Defaults to the running image. */
  binary_path?: string;
}

export interface GhReadConfig {
  /** User-tier operator opt-in for structured issue:// and pr:// reads. Default: false. */
  enabled?: boolean;
}

export interface GitConfig {
  /** "off" (default), "auto", or an explicit "Name <email>" identity. */
  co_author?: string;
}

export interface IndexRootConfig {
  path: string;
  indexes: Array<"search" | "semantic" | "callgraph">;
}

export interface IndexConfig {
  resource_policy?: "balanced" | "performance";
  roots?: IndexRootConfig[];
}

export interface SemanticConfig {
  backend?: SemanticBackend;
  model?: string;
  base_url?: string;
  api_key_env?: string;
  timeout_ms?: number;
  query_timeout_ms?: number;
  max_batch_size?: number;
  max_files?: number;
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
  categories?: Record<string, boolean>;
  tier2_soft_deadline_ms?: number;
  max_drill_down_items?: number;
  duplicates?: {
    expected_mirrors?: [string, string][];
  };
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

export type ToolSurface = "minimal" | "recommended" | "all";

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
  rewrite?: boolean;
  compress?: boolean;
  background?: boolean;
  /** Permit per-command host fallback after AFT transport failure. Default false. */
  host_fallback?: boolean;
  /** Detach wait:true bash calls on user messages; `&detach` overrides, is stripped before delivery, and a token-only message gets a minimal replacement. */
  detach_on_user_message?: boolean;
  long_running_reminder_enabled?: boolean;
  long_running_reminder_interval_ms?: number;
  /**
   * How long foreground bash blocks before auto-promoting to background.
   * Default 15000ms; values below the 5000ms floor are clamped up.
   */
  foreground_wait_window_ms?: number;
  /** Manual fallback for Pi versions that do not expose enabled default tools. */
  powershell_tool?: boolean;
}

export interface AftConfig {
  /**
   * Optional JSON Schema URL for editor tooling. Runtime no-op — only present
   * so VS Code/Cursor/etc. pick up the published schema for autocomplete +
   * validation. `aft setup` auto-inserts this.
   */
  $schema?: string;
  /** Master switch for AFT. Default true. Project config may set it because turning AFT off is not a privilege escalation. */
  enabled?: boolean;
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
  /** Replace host-native read/write/edit/grep/bash with AFT tools. Default: true; false registers AFT's replacements under aft_ names. */
  hoist_builtin_tools?: boolean;
  tool_surface?: ToolSurface;
  disabled_tools?: string[];
  restrict_to_project_root?: boolean;
  search_index?: boolean;
  /** User-tier standing roots; project config cannot configure this machine state. */
  index?: IndexConfig;
  semantic_search?: boolean;
  callgraph_store?: boolean;
  /** Number of files to parse in a single batch during callgraph store cold build. Lower values reduce peak memory during cold build. Default: 100. */
  callgraph_chunk_size?: number;
  /** Codebase health inspection config. Enabled by default; set inspect.enabled=false to hide aft_inspect. */
  inspect?: InspectConfig;
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
   * Bash tool family (hoist + rewrite + compress + background execution).
   * Default on for `tool_surface: recommended`/`all`, off for `minimal`.
   * Graduated from `experimental.bash.*` in v0.27.2; the legacy nested
   * form is still accepted for backward compat.
   *
   * - `true`  — all sub-features on, hoist enabled
   * - `false` — hoist disabled entirely; Pi's native bash stays
   * - `{ rewrite?, compress?, background?, ... }` — partial override;
   *   missing sub-keys default to `true`
   */
  bash?: boolean | BashConfig;
  experimental?: ExperimentalConfig;
  lsp?: LspConfig;
  url_fetch_allow_private?: boolean;
  semantic?: SemanticConfig;
  bridge?: BridgeConfig;
  subc?: SubcConfig;
  gh_shim?: GhShimConfig;
  gh_read?: GhReadConfig;
  git?: GitConfig;
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
  /** Detach wait:true bash calls on user messages; `&detach` overrides, is stripped before delivery, and a token-only message gets a minimal replacement. */
  detach_on_user_message: boolean;
  long_running_reminder_enabled?: boolean;
  long_running_reminder_interval_ms?: number;
  /**
   * Foreground poll window before auto-promotion to background, in ms.
   * Always resolved: defaults to 15000, floored at 5000.
   */
  foreground_wait_window_ms: number;
  /** Manual PowerShell registration fallback. Default false. */
  powershell_tool: boolean;
}

/** Default foreground wait-window before auto-promotion (ms). */
export const FOREGROUND_WAIT_WINDOW_DEFAULT_MS = 15_000;
/** Minimum allowed foreground wait-window (ms); smaller values clamp up. */
export const FOREGROUND_WAIT_WINDOW_MIN_MS = 5_000;

/**
 * Single source of truth for bash config across the Pi plugin. Resolution
 * order (highest priority wins):
 *
 *   1. Top-level `bash: false` → fully disabled (sub-features all false)
 *   2. Top-level `bash: true`  → fully enabled (sub-features all true)
 *   3. Top-level `bash: { ... }` → enabled; each sub-feature defaults true
 *      when not specified
 *   4. Top-level `bash` absent + any `experimental.bash.*` set → legacy
 *      fallback; sub-features take their explicit values (default false
 *      to preserve pre-v0.27.2 behavior — that block was opt-in)
 *   5. Top-level `bash` absent + no experimental → tool_surface default:
 *        - "minimal" → disabled
 *        - "recommended" or "all" → enabled with all sub-features on
 *
 * Mirrors OpenCode's resolver exactly. Reminder tuning rides through from
 * whichever surface specified it (top-level wins, legacy fills the gap).
 */
export function resolveBashConfig(config: AftConfig): ResolvedBashConfig {
  const top = config.bash;
  const legacy = config.experimental?.bash;
  const surface = config.tool_surface ?? "recommended";
  const surfaceDefaultEnabled = surface !== "minimal";

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

  const base: ResolvedBashConfig = {
    enabled: false,
    rewrite: false,
    compress: false,
    background: false,
    host_fallback: false,
    detach_on_user_message: true,
    long_running_reminder_enabled: reminderEnabled,
    long_running_reminder_interval_ms: reminderInterval,
    foreground_wait_window_ms: foregroundWaitWindowMs,
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
      enabled: true,
      rewrite: top.rewrite ?? true,
      compress: top.compress ?? true,
      background: top.background ?? true,
      host_fallback: top.host_fallback ?? false,
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
    return { ...base, enabled: rewrite || compress || background, rewrite, compress, background };
  }

  return {
    ...base,
    enabled: surfaceDefaultEnabled,
    rewrite: surfaceDefaultEnabled,
    compress: surfaceDefaultEnabled,
    background: surfaceDefaultEnabled,
  };
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
  resource_policy: z.enum(["balanced", "performance"]).optional(),
  roots: z.array(IndexRootSchema).optional(),
});

const SemanticConfigSchema = z.object({
  backend: z.enum(["fastembed", "openai_compatible", "ollama"]).optional(),
  model: z.string().trim().min(1).optional(),
  base_url: z.string().trim().min(1).optional(),
  api_key_env: z.string().trim().min(1).optional(),
  timeout_ms: z.number().int().positive().optional(),
  query_timeout_ms: z.number().int().positive().optional(),
  max_batch_size: z.number().int().positive().optional(),
  max_files: z.number().int().positive().optional(),
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
  rewrite: z.boolean().optional(),
  compress: z.boolean().optional(),
  background: z.boolean().optional(),
  host_fallback: z.boolean().optional(),
  detach_on_user_message: z.boolean().optional(),
  long_running_reminder_enabled: z.boolean().optional(),
  long_running_reminder_interval_ms: z.number().int().positive().optional(),
  foreground_wait_window_ms: z.number().int().positive().optional(),
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
  /** Operator hard-off for child PATH injection and governed `gh` routing. */
  enabled: z.boolean().optional(),
  /** User-tier AFT image used by the managed `gh` entry. Defaults to the running image. */
  binary_path: z
    .string()
    .trim()
    .refine(isAbsolute, "gh_shim.binary_path must be absolute")
    .optional(),
});

const GhReadConfigSchema = z.object({
  /** User-tier operator opt-in for structured issue:// and pr:// reads. Default: false. */
  enabled: z.boolean().optional(),
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
  categories: z.record(z.string(), z.boolean()).optional(),
  tier2_soft_deadline_ms: z.number().int().positive().optional(),
  max_drill_down_items: z.number().int().positive().max(100).optional(),
  duplicates: z
    .object({
      expected_mirrors: z
        .array(z.tuple([z.string().trim().min(1), z.string().trim().min(1)]))
        .optional(),
    })
    .optional(),
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
  max_file_size: z.number().int().positive().optional(),
});

const AftConfigFieldsSchema = z.object({
  /**
   * Optional JSON Schema URL for editor tooling. Ignored by the plugin at
   * runtime — only present so VS Code/Cursor/etc. pick up the published
   * schema for autocomplete + validation. `aft setup` auto-inserts this.
   */
  $schema: z.string().optional(),
  /** Master switch for AFT. Default true. Project config may set it because turning AFT off is not a privilege escalation. */
  enabled: z.boolean().optional(),
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
  /**
   * Replace Pi's native read/write/edit/grep/bash tools with AFT's
   * implementations. Default: true. When false, AFT registers its
   * equivalents under aft_ names and leaves Pi's native tools available.
   */
  hoist_builtin_tools: z.boolean().optional(),
  tool_surface: z.enum(["minimal", "recommended", "all"]).optional(),
  disabled_tools: z.array(z.string()).optional(),
  restrict_to_project_root: z.boolean().optional(),
  search_index: z.boolean().optional(),
  /** User-configured filesystem roots for indexed search; project config cannot change them. */
  index: IndexConfigSchema.optional(),
  semantic_search: z.boolean().optional(),
  callgraph_store: z.boolean().optional(),
  callgraph_chunk_size: z.number().optional(),
  inspect: InspectConfigSchema.optional(),
  backup: BackupConfigSchema.optional(),
  worktree: WorktreeConfigSchema.optional(),
  sandbox: SandboxConfigSchema.optional(),
  /**
   * Bash tool family (hoist + rewrite + compress + background execution).
   * Default on for `tool_surface: recommended`/`all`, off for `minimal`.
   * Three shapes: `true`, `false`, or `{ rewrite?, compress?, background?, ... }`.
   * Replaces `experimental.bash.*` (still accepted for backward compat).
   */
  bash: BashConfigSchema.optional(),
  experimental: ExperimentalConfigSchema.optional(),
  lsp: LspConfigSchema.optional(),
  url_fetch_allow_private: z.boolean().optional(),
  semantic: SemanticConfigSchema.optional(),
  bridge: BridgeConfigSchema.optional(),
  subc: SubcConfigSchema.optional(),
  gh_shim: GhShimConfigSchema.optional(),
  gh_read: GhReadConfigSchema.optional(),
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

function applyActiveHarnessOverride(config: AftConfig): AftConfig {
  const { harnesses: _harnesses, ...base } = config;
  return { ...base, ...config.harnesses?.[ACTIVE_HARNESS] } as AftConfigFields;
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

  if (config.edit_mode !== undefined) overrides.hashline_enabled = config.edit_mode === "hashline";
  if (config.format_on_edit !== undefined) overrides.format_on_edit = config.format_on_edit;
  if (config.formatter_timeout_secs !== undefined)
    overrides.formatter_timeout_secs = config.formatter_timeout_secs;
  if (config.validate_on_edit !== undefined) overrides.validate_on_edit = config.validate_on_edit;
  if (config.formatter !== undefined) overrides.formatter = config.formatter;
  if (config.checker !== undefined) overrides.checker = config.checker;

  overrides.restrict_to_project_root = config.restrict_to_project_root ?? false;

  if (config.search_index !== undefined) overrides.search_index = config.search_index;
  if (config.index !== undefined) overrides.index = config.index;
  if (config.semantic_search !== undefined) overrides.semantic_search = config.semantic_search;
  if (config.callgraph_store !== undefined) overrides.callgraph_store = config.callgraph_store;
  if (config.callgraph_chunk_size !== undefined)
    overrides.callgraph_chunk_size = config.callgraph_chunk_size;

  Object.assign(overrides, resolveExperimentalConfigForConfigure(config));
  if (
    typeof config.bash === "object" &&
    (config.bash.host_fallback !== undefined ||
      config.bash.detach_on_user_message !== undefined ||
      config.bash.powershell_tool !== undefined)
  ) {
    overrides.bash = {
      ...(config.bash.host_fallback !== undefined
        ? { host_fallback: config.bash.host_fallback }
        : {}),
      ...(config.bash.detach_on_user_message !== undefined
        ? { detach_on_user_message: config.bash.detach_on_user_message }
        : {}),
      ...(config.bash.powershell_tool !== undefined
        ? { powershell_tool: config.bash.powershell_tool }
        : {}),
    };
  }
  Object.assign(overrides, resolveLspConfigForConfigure(config));
  if (config.semantic !== undefined) overrides.semantic = config.semantic;
  if (config.inspect !== undefined) overrides.inspect = config.inspect;
  if (config.backup !== undefined) overrides.backup = config.backup;
  if (config.worktree !== undefined) overrides.worktree = config.worktree;
  if (config.sandbox !== undefined) overrides.sandbox = config.sandbox;
  if (config.gh_read !== undefined) overrides.gh_read = config.gh_read;
  if (config.git !== undefined) overrides.git = config.git;

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

type MigrationTarget = {
  oldKey: string;
  newPath: readonly string[];
};

const CONFIG_MIGRATIONS: readonly MigrationTarget[] = [
  { oldKey: "experimental_search_index", newPath: ["search_index"] },
  { oldKey: "experimental_semantic_search", newPath: ["semantic_search"] },
  { oldKey: "experimental_lsp_ty", newPath: ["experimental", "lsp_ty"] },
  { oldKey: "experimental_bash_rewrite", newPath: ["experimental", "bash", "rewrite"] },
  { oldKey: "experimental_bash_compress", newPath: ["experimental", "bash", "compress"] },
  { oldKey: "experimental_bash_background", newPath: ["experimental", "bash", "background"] },
];

function isWritableMigrationError(errorValue: unknown): boolean {
  const code = (errorValue as { code?: unknown })?.code;
  return code === "EROFS" || code === "EACCES" || code === "EPERM";
}

/**
 * Pulls all `//` line comments and `/* ... *​/` block comments out of a JSONC
 * source string. Used as a backup safety net during migration so comments
 * attached to deleted/reshaped keys don't disappear silently.
 */
function extractCommentsForPreservation(content: string): string[] {
  const comments: string[] = [];
  const linePattern = /\/\/[^\n]*/g;
  for (const match of content.match(linePattern) ?? []) {
    comments.push(match.trim());
  }
  const blockPattern = /\/\*[\s\S]*?\*\//g;
  for (const match of content.match(blockPattern) ?? []) {
    comments.push(match.replace(/\s+/g, " ").trim());
  }
  return comments;
}

function ensureRecordAtPath(root: Record<string, unknown>, path: readonly string[]) {
  let current = root;
  for (const segment of path) {
    const existing = current[segment];
    if (!existing || typeof existing !== "object" || Array.isArray(existing)) {
      current[segment] = {};
    }
    current = current[segment] as Record<string, unknown>;
  }
  return current;
}

function hasPath(root: Record<string, unknown>, path: readonly string[]): boolean {
  let current: unknown = root;
  for (const segment of path) {
    if (!current || typeof current !== "object" || Array.isArray(current)) return false;
    const record = current as Record<string, unknown>;
    if (!Object.hasOwn(record, segment)) return false;
    current = record[segment];
  }
  return true;
}

function setPath(root: Record<string, unknown>, path: readonly string[], value: unknown): void {
  const parent = ensureRecordAtPath(root, path.slice(0, -1));
  parent[path[path.length - 1]] = value;
}

function migrateRawConfig(
  rawConfig: Record<string, unknown>,
  configPath: string,
  logger?: Logger,
): string[] {
  const oldKeys: string[] = [];
  for (const migration of CONFIG_MIGRATIONS) {
    if (!Object.hasOwn(rawConfig, migration.oldKey)) continue;

    if (hasPath(rawConfig, migration.newPath)) {
      logger?.warn(
        `Config migration conflict at ${configPath}: ${migration.oldKey} ignored because ${migration.newPath.join(".")} is already set`,
      );
    } else {
      setPath(rawConfig, migration.newPath, rawConfig[migration.oldKey]);
    }
    delete rawConfig[migration.oldKey];
    oldKeys.push(migration.oldKey);
  }

  // The flat-key table above runs first so `experimental_bash_*` flat keys
  // (an even older shape) get lifted into the legacy `experimental.bash`
  // nested block; THEN this graduation step lifts that block to the new
  // top-level `bash`. Order matters: doing graduation first would leave
  // the flat keys behind.
  oldKeys.push(...migrateExperimentalBashBlock(rawConfig, configPath, logger));
  return oldKeys;
}

/**
 * Graduate `experimental.bash` → top-level `bash` (v0.27.2). Mirrors the
 * OpenCode plugin's `migrateExperimentalBashBlock` exactly.
 *
 * Critical semantic detail: the SAME object shape means different things
 * under the two surfaces. In old `experimental.bash: { rewrite, compress,
 * background }`, missing sub-keys defaulted to `false` (the whole block was
 * opt-in). In new top-level `bash: { ... }`, missing sub-keys default to
 * `true` (the block itself is on-by-default). To preserve exact pre-v0.27.2
 * behavior, we materialize all three keys explicitly when migrating —
 * including implicit `false` values the old block carried.
 *
 * Returns the list of migrated keys so the caller's log line mentions them.
 */
function migrateExperimentalBashBlock(
  rawConfig: Record<string, unknown>,
  configPath: string,
  logger?: Logger,
): string[] {
  const experimental = rawConfig.experimental;
  if (typeof experimental !== "object" || experimental === null || Array.isArray(experimental)) {
    return [];
  }
  const expRecord = experimental as Record<string, unknown>;
  if (!Object.hasOwn(expRecord, "bash")) return [];

  const legacyBash = expRecord.bash;

  // Non-object legacy value — drop without inventing a top-level shape.
  if (typeof legacyBash !== "object" || legacyBash === null || Array.isArray(legacyBash)) {
    delete expRecord.bash;
    if (Object.keys(expRecord).length === 0) delete rawConfig.experimental;
    return ["experimental.bash"];
  }

  const bashRecord = legacyBash as Record<string, unknown>;
  const hasFeatureFlag =
    "rewrite" in bashRecord || "compress" in bashRecord || "background" in bashRecord;

  // Pure tuning-only block (only long_running_reminder_*). Nothing
  // semantic to graduate — materializing implicit feature flags would
  // surprise users who never opted into bash hoisting.
  if (!hasFeatureFlag) return [];

  const movedKeys = Object.keys(bashRecord).map((k) => `experimental.bash.${k}`);

  if (Object.hasOwn(rawConfig, "bash")) {
    logger?.warn(
      `Config migration conflict at ${configPath}: experimental.bash dropped because top-level "bash" is already set`,
    );
  } else {
    const migrated: Record<string, unknown> = {
      rewrite: bashRecord.rewrite === true,
      compress: bashRecord.compress === true,
      background: bashRecord.background === true,
    };
    if (bashRecord.long_running_reminder_enabled !== undefined) {
      migrated.long_running_reminder_enabled = bashRecord.long_running_reminder_enabled;
    }
    if (bashRecord.long_running_reminder_interval_ms !== undefined) {
      migrated.long_running_reminder_interval_ms = bashRecord.long_running_reminder_interval_ms;
    }
    rawConfig.bash = migrated;
  }
  delete expRecord.bash;

  if (Object.keys(expRecord).length === 0) {
    delete rawConfig.experimental;
  }

  return movedKeys;
}

export function migrateAftConfigFile(
  configPath: string,
  logger: Logger = { log, warn },
): { migrated: boolean; oldKeys: string[] } {
  if (!existsSync(configPath)) {
    return { migrated: false, oldKeys: [] };
  }

  let tmpPath: string | null = null;
  let oldKeys: string[] = [];
  try {
    const content = readFileSync(configPath, "utf-8");
    const rawConfig = parseJsonc<Record<string, unknown>>(content);
    if (!rawConfig || typeof rawConfig !== "object" || Array.isArray(rawConfig)) {
      return { migrated: false, oldKeys: [] };
    }

    oldKeys = migrateRawConfig(rawConfig, configPath, logger);
    if (oldKeys.length === 0) {
      return { migrated: false, oldKeys: [] };
    }

    const serialized = `${stringifyJsonc(rawConfig, null, 2)}\n`;
    const preservedComments = extractCommentsForPreservation(content).filter(
      (comment) => !serialized.includes(comment.trim()),
    );
    const nextContent =
      preservedComments.length > 0 ? `${preservedComments.join("\n")}\n${serialized}` : serialized;

    tmpPath = `${configPath}.tmp.${process.pid}`;
    writeFileSync(tmpPath, nextContent, "utf-8");
    renameSync(tmpPath, configPath);
    logger.log(`Migrated config at ${configPath}: removed ${oldKeys.join(", ")}`);
    return { migrated: true, oldKeys };
  } catch (err) {
    if (tmpPath) {
      try {
        unlinkSync(tmpPath);
      } catch {
        // best-effort cleanup
      }
    }
    if (isWritableMigrationError(err)) {
      const errorMsg = err instanceof Error ? err.message : String(err);
      logger.warn(
        `Config migration could not write ${configPath} (${errorMsg}); using migrated config in memory`,
      );
      return { migrated: oldKeys.length > 0, oldKeys };
    }
    return { migrated: false, oldKeys: [] };
  }
}

// ---------------------------------------------------------------------------
// Config file detection (.jsonc preferred over .json)
// ---------------------------------------------------------------------------

export type ConfigLoadError = { path: string; message: string };

let configLoadErrors: ConfigLoadError[] = [];

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

function loadConfigFromPath(configPath: string): AftConfig | null {
  try {
    if (!existsSync(configPath)) return null;
    const content = readFileSync(configPath, "utf-8");
    const rawConfig = parseJsonc<Record<string, unknown>>(content);
    if (!rawConfig || typeof rawConfig !== "object" || Array.isArray(rawConfig)) {
      recordConfigParseFailure(configPath, "root must be an object");
      return null;
    }
    migrateRawConfig(rawConfig, configPath, { log, warn });
    // comment-json attaches Symbol(before/after:<key>) props to track comments.
    // Zod stringifies keys when building error paths, which throws on those
    // symbols and would silently drop the whole config to defaults (issue #88).
    // Validate against a symbol-free deep copy.
    const cleanConfig = stripJsoncSymbols(rawConfig);
    warnIgnoredHarnessSpecificConfigKeys(cleanConfig, configPath);
    warnIgnoredNestedHarnesses(cleanConfig, configPath);
    const result = AftConfigSchema.safeParse(cleanConfig);

    if (result.success) {
      log(`Config loaded from ${configPath}`);
      return applyActiveHarnessOverride(result.data);
    }

    const errorMsg = result.error.issues.map((i) => `${i.path.join(".")}: ${i.message}`).join(", ");
    warn(`Config validation error in ${configPath}: ${errorMsg}`);
    return applyActiveHarnessOverride(parseConfigPartially(cleanConfig));
  } catch (err) {
    const errorMsg = err instanceof Error ? err.message : String(err);
    error(`Error loading config from ${configPath}: ${errorMsg}`);
    recordConfigParseFailure(configPath, errorMsg);
    return null;
  }
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
  "enabled",
  "edit_mode",
  "tool_surface",
  // This only changes the names Pi exposes; AFT permissions and capabilities stay the same.
  "hoist_builtin_tools",
  "format_on_edit",
  "validate_on_edit",
  "configure_warnings_delivery",
  // Experimental flags: project-settable so users can enable globally
  // and toggle per-project (or vice versa). Project value overrides user value.
  "search_index",
  "semantic_search",
  "callgraph_store",
  "callgraph_chunk_size",
  "inspect",
  "worktree",
  // Git attribution only changes commit metadata; it grants no capabilities and
  // does not select an executable, so project configuration may override it.
  "git",
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
  // "gh_read" — USER ONLY because it changes the global tool description.
  // Advertising disabled resource spellings wastes prompt tokens and confuses
  // steering; project-specific surface changes also destabilize prefix caches.
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

function getStrippedTopLevelKeys(override: AftConfig): string[] {
  const stripped: string[] = [];
  if (override.restrict_to_project_root !== undefined) stripped.push("restrict_to_project_root");
  if (override.url_fetch_allow_private !== undefined) stripped.push("url_fetch_allow_private");
  if (override.bridge !== undefined) stripped.push("bridge");
  if (override.backup !== undefined) stripped.push("backup");
  if (override.index?.roots !== undefined) stripped.push("index.roots");
  // enabled:true is an accepted project-tier hardening opt-in; only the
  // weakening direction (enabled:false) is stripped as user-only.
  if (override.sandbox?.enabled === false) stripped.push("sandbox.enabled");
  if (override.sandbox?.write_allow !== undefined) stripped.push("sandbox.write_allow");
  if (override.subc !== undefined) stripped.push("subc");
  if (override.gh_shim !== undefined) stripped.push("gh_shim");
  if (override.gh_read !== undefined) stripped.push("gh_read");
  if (override.disabled_tools?.includes("aft_safety")) stripped.push("disabled_tools.aft_safety");
  return stripped;
}

function mergeConfigs(base: AftConfig, override: AftConfig): AftConfig {
  const disabledTools = [
    ...(base.disabled_tools ?? []),
    ...(override.disabled_tools ?? []).filter((tool: string) => tool !== "aft_safety"),
  ];
  const formatter = { ...base.formatter, ...override.formatter };
  const checker = { ...base.checker, ...override.checker };
  const semantic = mergeSemanticConfig(base.semantic, override.semantic);
  const lsp = mergeLspConfig(base.lsp, override.lsp);
  const experimental = mergeExperimentalConfig(base.experimental, override.experimental);
  const bash = mergeBashConfig(base.bash, override.bash);
  const inspect = mergeInspectConfig(base.inspect, override.inspect);
  const worktree = mergeWorktreeConfig(base.worktree, override.worktree);
  const sandbox = mergeSandboxConfig(base.sandbox, override.sandbox);
  const bridge = base.bridge;

  // STRICT ALLOWLIST: only project-safe top-level fields are inherited.
  // See PROJECT_SAFE_TOP_LEVEL_FIELDS above for the full security rationale.
  // We deep-merge `bash` separately so the field-by-field union beats the
  // shallow allowlist spread; otherwise project's `bash: { compress: false }`
  // would wipe out user's `bash: { rewrite: true }`.
  const safeOverride = pickProjectSafeFields(override);
  delete safeOverride.bash;
  delete safeOverride.inspect;
  delete safeOverride.worktree;

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
    experimental,
    semantic,
    ...(bridge !== undefined ? { bridge } : {}),
    ...(disabledTools.length > 0 ? { disabled_tools: [...new Set(disabledTools)] } : {}),
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
  const paths = resolveCortexKitConfigPaths(projectDirectory);
  migrateAftConfigFile(paths.userConfigPath);
  migrateAftConfigFile(paths.projectConfigPath);
  return paths;
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

export function loadAftConfig(projectDirectory: string): AftConfig {
  configLoadErrors = [];

  const { userConfigPath, projectConfigPath } = resolveAftConfigPaths(projectDirectory);

  let config: AftConfig = loadConfigFromPath(userConfigPath) ?? {};

  const projectConfig = loadConfigFromPath(projectConfigPath);
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

  return config;
}
