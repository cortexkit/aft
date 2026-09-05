import { existsSync, readFileSync, renameSync, unlinkSync, writeFileSync } from "node:fs";
import { homedir } from "node:os";
import { isAbsolute, parse as parsePath, resolve as resolvePath } from "node:path";
import {
  type AftConfigFileMigrationResult,
  type ConfigTier,
  migrateAftConfigFile as migrateLegacyAftConfigFile,
  PI_ONLY_KEYS,
  readConfigTiers,
  resolveCortexKitConfigPaths,
  resolveLegacyAftConfigSources,
  stripHarnessSpecificConfigKeys,
  stripJsoncSymbols,
} from "@cortexkit/aft-bridge";
import { parse as parseJsonc, stringify as stringifyJsonc } from "comment-json";
import { z } from "zod";

import { error, log, warn } from "./logger.js";

const ACTIVE_HARNESS = "opencode";

function isConfigRecord(value: unknown): value is Record<string, unknown> {
  return value !== null && typeof value === "object" && !Array.isArray(value);
}

function normalizeConfigForActiveHarness(value: unknown): unknown {
  const config = stripHarnessSpecificConfigKeys(value, PI_ONLY_KEYS);
  if (!isConfigRecord(config) || !isConfigRecord(config.harnesses)) return config;

  const override = config.harnesses[ACTIVE_HARNESS];
  return {
    ...config,
    // Other harnesses deliberately remain opaque to this plugin so future
    // harness-specific settings never make an older OpenCode plugin reject the
    // shared config file.
    harnesses:
      override === undefined
        ? {}
        : { [ACTIVE_HARNESS]: stripHarnessSpecificConfigKeys(override, PI_ONLY_KEYS) },
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
// Zod schema
// ---------------------------------------------------------------------------

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

/** How configure-time missing-tool warnings reach the user. Default: toast (no chat transcript). */
export const ConfigureWarningsDeliveryEnum = z.enum(["toast", "log", "chat"]);
export type ConfigureWarningsDelivery = z.infer<typeof ConfigureWarningsDeliveryEnum>;

const SemanticBackendEnum = z.enum(["fastembed", "openai_compatible", "ollama", "synapse"]);
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
  /** Semantic backend type: local fastembed, OpenAI-compatible API, Ollama, or Synapse over SubC. */
  backend: SemanticBackendEnum.optional(),
  /** Model identifier passed to the selected semantic backend. */
  model: z.string().trim().min(1).optional(),
  /** Base URL of the backend API endpoint. */
  base_url: z.string().trim().min(1).optional(),
  /** Environment variable that contains the API key used by external backends. */
  api_key_env: z.string().trim().min(1).optional(),
  /** Background build request timeout in milliseconds. */
  timeout_ms: z.number().int().positive().optional(),
  /** Interactive query embedding deadline in milliseconds (clamped to 500..15000). */
  query_timeout_ms: z.number().int().positive().optional(),
  /** Maximum batch size used by the semantic pipeline. */
  max_batch_size: z.number().int().positive().optional(),
  /** Maximum number of project files to semantically index (default 20000). */
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
  // Optional: when overriding a built-in server (e.g. `rust`) to tweak one
  // field, AFT inherits the built-in's extensions/binary. Requiring them here
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
   * edit/write/apply_patch call unless the tool call overrides diagnostics.
   * Default: false.
   */
  diagnostics_on_edit: z.boolean().optional(),
  /**
   * Auto-install npm-distributed and GitHub-release language servers when
   * the project needs them. Default: true. Set false to require manual
   * install via PATH.
   */
  auto_install: z.boolean().optional(),
  /**
   * Supply-chain grace window. AFT only installs versions that have been
   * on the registry / GitHub releases for at least this many days, defending
   * against newly-published malicious versions that get yanked within hours
   * of detection. Default: 7. User pins via `lsp.versions` bypass this.
   */
  // grace_days must be >= 1 because grace_days: 0 disables
  // the supply-chain grace window entirely with no warning. Users debugging
  // can still bypass the grace per-package via `lsp.versions` pins, which is
  // a more explicit and auditable opt-out.
  grace_days: z.number().int().positive().optional(),
  /**
   * Per-package version pin map keyed by npm package or GitHub repo. Pins
   * bypass the grace filter and any weekly version recheck. Examples:
   *   { "typescript-language-server": "5.0.0" }
   *   { "clangd/clangd": "21.1.0" }
   */
  versions: z.record(z.string().trim().min(1), z.string().trim().min(1)).optional(),
});

const ExperimentalConfigSchema = z.object({
  /**
   * @deprecated The bash family graduated from experimental in v0.27.2. Use the
   * top-level `bash` key instead. This nested form is still accepted for
   * backward compatibility — when present and top-level `bash` is absent,
   * its values seed the resolved bash config. Will be removed in v0.28.
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
 * Graduated `bash` config. Replaces `experimental.bash.*` in v0.27.2.
 * Default behavior:
 *   - tool_surface "recommended" or "all" → bash hoist on, all sub-features on
 *   - tool_surface "minimal" → bash hoist off (user explicitly wants minimal)
 * Three shapes:
 *   - `bash: true`     → identical to default (all on)
 *   - `bash: false`    → hoist disabled entirely; OpenCode native bash stays
 *   - `bash: { ... }`  → partial override; missing sub-keys default to true
 */
const BashFeaturesSchema = z.object({
  rewrite: z.boolean().optional(),
  compress: z.boolean().optional(),
  background: z.boolean().optional(),
  /**
   * Permit an emergency, per-command host execution when the AFT transport is
   * unavailable. Every fallback command still requires a fresh host permission
   * prompt. Default: false.
   */
  host_fallback: z.boolean().optional(),
  /**
   * Allow OpenCode subagents to use real background bash (`background: true`
   * and auto-promotion). Default: false — subagents fall back to synchronous
   * foreground polling because they can't survive turn-end to receive the
   * wake-up reminder. When true, subagents get the same bg semantics as
   * primary sessions and MUST explicitly wait for their bg tasks with
   * `bash_status({ taskId, exit: true, ... })` before returning to parent.
   * Setting this is essentially a contract with your subagent prompts that
   * they know how to use bash_status's wait mode.
   */
  subagent_background: z.boolean().optional(),
  /**
   * Detach a wait:true bash call when a new user message arrives. Default: true.
   * Set false to keep the wait blocking; a message containing `&detach` still
   * forces detachment; a token-only message becomes `(requested background detach)`.
   */
  detach_on_user_message: z.boolean().optional(),
  long_running_reminder_enabled: z.boolean().optional(),
  long_running_reminder_interval_ms: z.number().int().positive().optional(),
  /**
   * How long foreground bash blocks before auto-promoting the task to
   * background. Default 15000ms; values below the 5000ms floor are clamped up.
   */
  foreground_wait_window_ms: z.number().int().positive().optional(),
  /** Maximum synchronous bash_watch wait in milliseconds; clamped to 1000..1800000. Default 120000. */
  watch_sync_max_ms: z.number().int().positive().optional(),
  // Pi-only registration fallback. OpenCode accepts this shared config key but
  // never registers a PowerShell tool.
  powershell_tool: z.boolean().optional(),
});

const BashConfigSchema = z.union([z.boolean(), BashFeaturesSchema]);

const SandboxConfigSchema = z.object({
  /** Enable native containment for first-party bash and PTY processes. Default: false. */
  enabled: z.boolean().optional(),
  /** Additional absolute directories where sandboxed commands may write. User config only. */
  // (write_allow weakens confinement, so it never merges from project tier.)
  write_allow: z.array(z.string()).optional(),
  /** Additional absolute paths sandboxed commands may not read. Project config may add entries. */
  read_deny: z.array(z.string()).optional(),
});

const BridgeConfigSchema = z.object({
  /**
   * Per-request bridge transport timeout in milliseconds. Default: 30000.
   * Raise on slow filesystems (WSL/DrvFs/NFS) where cold `aft` operations exceed the default.
   */
  request_timeout_ms: z
    .number()
    .int()
    .min(1000, { message: "bridge.request_timeout_ms must be at least 1000" })
    .optional(),
  /**
   * Consecutive silent request timeouts before the bridge is killed and respawned.
   * Default: 2. Raise when many editor windows share one bridge process.
   */
  hang_threshold: z
    .number()
    .int()
    .min(1, { message: "bridge.hang_threshold must be at least 1" })
    .optional(),
});

const SubcConfigSchema = z.object({
  /**
   * Absolute path to the Subconscious (subc) daemon connection file. When PRESENT
   * (and non-empty), the plugin talks to AFT as a daemon-supervised module over
   * subc instead of spawning the `aft` binary; ABSENT/empty ⇒ standalone NDJSON
   * (the default). USER/global-tier ONLY — a project config must never redirect
   * transport (enforced by getStrippedTopLevelKeys). No auto-derive: the macOS
   * plugin process has no XDG_RUNTIME_DIR, so an auto-derived path would silently
   * diverge from the daemon's. The macOS default is
   * `~/.local/share/cortexkit/run/subc-connection.json`.
   */
  connection_file: z.string().optional(),
  /**
   * Enable the client-side root reaper. This is a user-tier switch; project
   * config is rejected from the merge so a repository cannot enable or disable
   * lifecycle cleanup for the user's host process.
   */
  client_reaper: z.boolean().optional(),
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

export const DEFAULT_INSPECT_DIAGNOSTICS_TIMEOUT_MS = 120_000;
export const MIN_INSPECT_DIAGNOSTICS_TIMEOUT_MS = 10_000;
export const MAX_INSPECT_DIAGNOSTICS_TIMEOUT_MS = 600_000;

function clampInspectDiagnosticsTimeoutMs(value: number): number {
  return Math.min(
    MAX_INSPECT_DIAGNOSTICS_TIMEOUT_MS,
    Math.max(MIN_INSPECT_DIAGNOSTICS_TIMEOUT_MS, value),
  );
}

const InspectConfigSchema = z.object({
  /** Master switch for the aft_inspect tool. Defaults to true. */
  enabled: z.boolean().optional(),
  /** Blocking diagnostics deadline in milliseconds (clamped to 10000..600000). */
  diagnostics_timeout_ms: z
    .number()
    .int()
    .positive()
    .optional()
    .transform((value) =>
      value === undefined ? undefined : clampInspectDiagnosticsTimeoutMs(value),
    ),
  /** OpenCode session.idle delay before Tier 2 inspect prewarm. Default: 4 minutes. */
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

function clampIdleRootTtlMinutes(value: number): number {
  return Math.min(30, Math.max(5, value));
}

function clampIdleLspTtlMinutes(value: number): number {
  return Math.min(10, Math.max(1, value));
}

const IdleConfigSchema = z.object({
  /** Unbound-root artifact eviction idle window in minutes. Default 30; clamped to 5..=30. */
  root_ttl_minutes: z
    .number()
    .int()
    .optional()
    .transform((value) => (value === undefined ? undefined : clampIdleRootTtlMinutes(value))),
  /** Language-server idle window in minutes. Default 10; clamped to 1..=10. Independent of artifact TTL. */
  lsp_ttl_minutes: z
    .number()
    .int()
    .optional()
    .transform((value) => (value === undefined ? undefined : clampIdleLspTtlMinutes(value))),
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
  /** Master switch for agent-facing undo backups. Defaults to true. */
  enabled: z.boolean().optional(),
  /** Per-file undo stack depth. Defaults to 20. */
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
  /**
   * Master switch for AFT in this config scope. Default: true. Set false in
   * user config to disable AFT everywhere, or in project config to disable it
   * only for that project. Project config may set this field because turning
   * AFT off is not a privilege escalation.
   */
  enabled: z.boolean().optional(),
  /** Select the edit/read surface. `hashline` exposes tagged reads and `{ patch }` edits. */
  edit_mode: z.enum(["default", "hashline"]).optional(),
  /**
   * Whether to auto-format files after edits. Default: false — formatting can
   * reflow the file under the agent and stale the next edit's context. Opt in
   * with `true` if you want AFT to format after edits.
   */
  format_on_edit: z.boolean().optional(),
  /**
   * Maximum seconds an external formatter is allowed to run before AFT
   * kills it and reports `format_skipped_reason: "timeout"`. Bounded
   * 1..=600. Default: 10. Raise for slow formatters (e.g. ruff in large
   * Python projects); lower for tighter test loops.
   */
  formatter_timeout_secs: z.number().int().min(1).max(600).optional(),
  /** Auto-validate after edits: "syntax" (tree-sitter) or "full" (runs type checker). */
  validate_on_edit: z.enum(["syntax", "full"]).optional(),
  /** Per-language formatter overrides. Keys: "typescript", "python", "rust", "go". */
  formatter: z.record(z.string(), FormatterEnum).optional(),
  /** Per-language type checker overrides. Keys: "typescript", "python", "rust", "go". */
  checker: z.record(z.string(), CheckerEnum).optional(),
  /**
   * How missing formatter/checker/LSP warnings are shown after configure.
   * - `toast`: 10s TUI toast (or HTTP show-toast when available); no session chat
   * - `log`: plugin log only
   * - `chat`: legacy ignored user messages in the session transcript
   *
   * There is no top-level `formatters` key — use `format_on_edit`, `formatter`, and
   * `checker` instead.
   */
  configure_warnings_delivery: ConfigureWarningsDeliveryEnum.optional(),
  /**
   * Replace the host's native file and shell tools with AFT implementations.
   * Default: true. When false, AFT registers its replacements under aft_
   * names and leaves host-native tools available.
   */
  hoist_builtin_tools: z.boolean().optional(),
  /**
   * Tool surface level. Controls which tools are registered:
   * - "minimal":     aft_outline, aft_zoom, aft_safety (no hoisting)
   * - "recommended": minimal + hoisted read/write/edit/apply_patch
   *                  + ast_grep_search/replace + aft_import (default)
   * - "all":         recommended + aft_callgraph, aft_delete, aft_move
   */
  tool_surface: z.enum(["minimal", "recommended", "all"]).optional(),
  /**
   * List of tool names to disable. Disabled tools are not registered with
   * OpenCode and will be invisible to agents. Use exact tool names, e.g.
   * ["aft_callgraph", "aft_move"]. Hoisted names ("read", "edit") and
   * aft-prefixed names both work. Applied after tool_surface filtering.
   */
  disabled_tools: z.array(z.string()).optional(),
  /**
   * Restrict file operations to within the project root directory.
   * When true, write-capable commands reject paths outside project_root.
   * Default: false (matches OpenCode's built-in behavior).
   */
  restrict_to_project_root: z.boolean().optional(),
  /** Enable indexed search for grep and glob hoisting. Default: false. */
  search_index: z.boolean().optional(),
  /** User-configured filesystem roots for indexed search; project config cannot change them. */
  index: IndexConfigSchema.optional(),
  /** Enable semantic search. Default: false. */
  semantic_search: z.boolean().optional(),
  /** Enable the persisted callgraph store substrate. Default: true. */
  callgraph_store: z.boolean().optional(),
  /** Number of files to parse in a single batch during callgraph store cold build. Lower values reduce peak memory during cold build. Default: 100. */
  callgraph_chunk_size: z.number().optional(),
  /** Codebase health inspection config. Enabled by default; set inspect.enabled=false to hide aft_inspect. */
  inspect: InspectConfigSchema.optional(),
  /** Idle reclamation windows for unbound-root artifacts and language servers. User and project tiers. */
  idle: IdleConfigSchema.optional(),
  /** Undo backup config. User-only: project config cannot disable or shrink a user's safety net. */
  backup: BackupConfigSchema.optional(),
  /**
   * Linked-worktree RAM overlay. Default off. A repo may opt its worktrees
   * in at project tier; it only spends that machine's RAM.
   */
  worktree: WorktreeConfigSchema.optional(),
  /** Native first-party bash sandbox. Write allowances are user-only; a project may enable but never disable. */
  sandbox: SandboxConfigSchema.optional(),
  /**
   * Bash tool family (hoist + rewrite + compress + background execution).
   * Default on for `tool_surface: recommended`/`all`, off for `minimal`.
   *
   * Accepts three shapes:
   *   - `true`  — all sub-features on, hoist enabled
   *   - `false` — hoist disabled entirely; OpenCode's native bash stays
   *   - `{ rewrite?, compress?, background?, ... }` — partial override;
   *     missing sub-keys default to `true`
   *
   * Replaces `experimental.bash.*` (still accepted for backward compat).
   */
  bash: BashConfigSchema.optional(),
  /** Experimental opt-in features. Default: all false. */
  experimental: ExperimentalConfigSchema.optional(),
  /** User-defined and built-in LSP server configuration. */
  lsp: LspConfigSchema.optional(),
  /** Allow URL fetch tools to request private/link-local hosts. Default: false. */
  url_fetch_allow_private: z.boolean().optional(),
  /** External semantic backend configuration for embedding and retrieval. */
  semantic: SemanticConfigSchema.optional(),
  /** Auto-refresh OpenCode's cached @cortexkit/aft-opencode package when a newer channel version exists. */
  auto_update: z.boolean().optional(),
  /** Per-bridge transport timeout and stalled-connection handling; user-only across the shared pool. */
  bridge: BridgeConfigSchema.optional(),
  /** Subconscious daemon transport selection (USER-only; presence ⇒ subc mode). */
  subc: SubcConfigSchema.optional(),
  /** `gh` routing shim gate and binary location; user configuration only, enabled by default. */
  gh_shim: GhShimConfigSchema.optional(),
  /** Structured GitHub resource reads. Project config overrides user config. Default: disabled. */
  gh_read: GhReadConfigSchema.optional(),
  /** Agent-child Git attribution. Project config may override user config. */
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

export type AftConfig = z.infer<typeof AftConfigSchema>;

export function toolEnabled(config: AftConfig, toolName: string): boolean {
  return !(config.disabled_tools ?? []).includes(toolName);
}
type AftConfigFields = z.infer<typeof AftConfigFieldsSchema>;

function applyActiveHarnessOverride(config: AftConfig): AftConfig {
  const { harnesses: _harnesses, ...base } = config;
  return { ...base, ...config.harnesses?.[ACTIVE_HARNESS] } as AftConfigFields;
}

/** Resolve the blocking diagnostics deadline for tools that wait on `aft_inspect`. */
export function resolveInspectDiagnosticsTimeoutMs(config: AftConfig): number {
  return clampInspectDiagnosticsTimeoutMs(
    config.inspect?.diagnostics_timeout_ms ?? DEFAULT_INSPECT_DIAGNOSTICS_TIMEOUT_MS,
  );
}

export interface ConfigureLspServer {
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

export interface ConfigureLspOverrides {
  experimental_lsp_ty?: boolean;
  lsp_servers?: ConfigureLspServer[];
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
    const entry: ConfigureLspServer = {
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
 * Build the per-project subset of configure overrides that come from
 * `aft.jsonc` (user config merged with project config). Used by the OpenCode
 * plugin's per-bridge `projectConfigLoader` so each project's `aft.jsonc` wins
 * over the user-level config for that project's bridge, instead of every
 * bridge inheriting whatever project was visible at plugin init.
 *
 * **DO NOT** put genuinely-global fields here. Things like `storage_dir`,
 * `_ort_dylib_dir`, `harness`, `lsp_paths_extra`, `bash_permissions` are set
 * at plugin init from process state (XDG dirs, ONNX download path, etc.) and
 * MUST NOT be re-derived per-bridge — they're identical across all bridges in
 * one OpenCode/Pi process.
 *
 * **DO NOT** put fields that affect plugin-side tool registration here.
 * `enabled`, `tool_surface`, and `hoist_builtin_tools` lock
 * at plugin init because OpenCode registers tools synchronously when the plugin
 * function returns. Per-bridge changes to those fields wouldn't take effect.
 */
export function resolveProjectOverridesForConfigure(config: AftConfig): Record<string, unknown> {
  const overrides: Record<string, unknown> = {};

  // The Rust server also needs disabled_tools to decide which tools to omit from
  // rendered steering text. loadAftConfig has already merged the project and
  // session values, so forward the merged value unchanged.
  if (config.disabled_tools !== undefined) overrides.disabled_tools = config.disabled_tools;

  // Edit-pipeline behavior — overridable per-project.
  if (config.edit_mode !== undefined) overrides.hashline_enabled = config.edit_mode === "hashline";
  if (config.format_on_edit !== undefined) overrides.format_on_edit = config.format_on_edit;
  if (config.formatter_timeout_secs !== undefined)
    overrides.formatter_timeout_secs = config.formatter_timeout_secs;
  if (config.validate_on_edit !== undefined) overrides.validate_on_edit = config.validate_on_edit;
  if (config.formatter !== undefined) overrides.formatter = config.formatter;
  if (config.checker !== undefined) overrides.checker = config.checker;

  // Project containment — default false at the plugin layer (parity with
  // OpenCode's built-in tools). Users opt in with `restrict_to_project_root: true`.
  overrides.restrict_to_project_root = config.restrict_to_project_root ?? false;

  // Indexed search and semantic search — both are per-project opt-ins.
  if (config.search_index !== undefined) overrides.search_index = config.search_index;
  if (config.index !== undefined) overrides.index = config.index;
  if (config.semantic_search !== undefined) overrides.semantic_search = config.semantic_search;
  if (config.callgraph_store !== undefined) overrides.callgraph_store = config.callgraph_store;
  if (config.callgraph_chunk_size !== undefined)
    overrides.callgraph_chunk_size = config.callgraph_chunk_size;

  // Bash / LSP / semantic all flow through dedicated resolvers because they
  // have their own merge / project-safety rules.
  Object.assign(overrides, resolveExperimentalConfigForConfigure(config));
  if (
    typeof config.bash === "object" &&
    (config.bash.host_fallback !== undefined ||
      config.bash.detach_on_user_message !== undefined ||
      config.bash.watch_sync_max_ms !== undefined ||
      config.bash.powershell_tool !== undefined)
  ) {
    overrides.bash = {
      ...(config.bash.host_fallback !== undefined
        ? { host_fallback: config.bash.host_fallback }
        : {}),
      ...(config.bash.detach_on_user_message !== undefined
        ? { detach_on_user_message: config.bash.detach_on_user_message }
        : {}),
      ...(config.bash.watch_sync_max_ms !== undefined
        ? { watch_sync_max_ms: config.bash.watch_sync_max_ms }
        : {}),
      ...(config.bash.powershell_tool !== undefined
        ? { powershell_tool: config.bash.powershell_tool }
        : {}),
    };
  }
  Object.assign(overrides, resolveLspConfigForConfigure(config));
  if (config.semantic !== undefined) overrides.semantic = config.semantic;
  if (config.inspect !== undefined) overrides.inspect = config.inspect;
  if (config.idle !== undefined) overrides.idle = config.idle;
  if (config.backup !== undefined) overrides.backup = config.backup;
  if (config.worktree !== undefined) overrides.worktree = config.worktree;
  if (config.sandbox !== undefined) overrides.sandbox = config.sandbox;
  if (config.gh_read !== undefined) overrides.gh_read = config.gh_read;
  if (config.git !== undefined) overrides.git = config.git;

  return overrides;
}

/**
 * Resolved bash configuration after merging top-level `bash`, the
 * legacy `experimental.bash.*` fallback, and tool_surface defaults.
 * Everything downstream — hoist gating, configure-override emission,
 * workflow-hints — reads from this single shape.
 *
 * `enabled` controls hoist registration ONLY; the three sub-features
 * (rewrite/compress/background) are independent feature flags within
 * an enabled bash surface. `enabled: false` forces all three off and
 * disables hoist; OpenCode's native bash stays in place.
 */
export interface ResolvedBashConfig {
  enabled: boolean;
  rewrite: boolean;
  compress: boolean;
  background: boolean;
  /** Emergency local execution gate. Default false, including for `bash: true`. */
  host_fallback: boolean;
  /** See BashFeaturesSchema.subagent_background. Default false. */
  subagent_background: boolean;
  /** Detach wait:true bash calls on user messages; `&detach` overrides and is stripped before delivery. */
  detach_on_user_message: boolean;
  long_running_reminder_enabled?: boolean;
  long_running_reminder_interval_ms?: number;
  /**
   * Foreground poll window before auto-promotion to background, in ms.
   * Always resolved: defaults to 15000, floored at 5000.
   */
  foreground_wait_window_ms: number;
  /** Maximum synchronous bash_watch wait. Defaults to 120000 and is clamped to 1000..1800000. */
  watch_sync_max_ms: number;
  /** Pi-only manual PowerShell registration fallback. Default false. */
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
 * Single source of truth for bash config across the plugin. Resolution
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
 * The long_running_reminder_* tuning fields are passed through from
 * whichever surface specified them; they live alongside the feature
 * flags because they're emitted to Rust through the same configure
 * call shape.
 */
export function resolveBashConfig(config: AftConfig): ResolvedBashConfig {
  const top = config.bash;
  const legacy = config.experimental?.bash;
  const surface = config.tool_surface ?? "recommended";
  const surfaceDefaultEnabled = surface !== "minimal";

  // Reminder tuning rides along from whichever surface set it — top-level
  // wins, legacy fills in the gap.
  const reminderEnabled =
    (typeof top === "object" && top !== null ? top.long_running_reminder_enabled : undefined) ??
    legacy?.long_running_reminder_enabled;
  const reminderInterval =
    (typeof top === "object" && top !== null ? top.long_running_reminder_interval_ms : undefined) ??
    legacy?.long_running_reminder_interval_ms;

  // subagent_background defaults FALSE everywhere (object form, legacy form,
  // surface default). It's an explicit opt-in even when bash: true. Top-level
  // wins; only the object form can set it.
  const topSubagentBg =
    typeof top === "object" && top !== null ? top.subagent_background === true : false;
  const topDetachOnUserMessage =
    typeof top === "object" && top !== null ? (top.detach_on_user_message ?? true) : true;

  // Foreground wait-window: only the object form can set it; clamp to the
  // 5000ms floor and default to 15000ms when unset.
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
    subagent_background: false,
    detach_on_user_message: true,
    long_running_reminder_enabled: reminderEnabled,
    long_running_reminder_interval_ms: reminderInterval,
    foreground_wait_window_ms: foregroundWaitWindowMs,
    watch_sync_max_ms: watchSyncMaxMs,
    powershell_tool:
      typeof top === "object" && top !== null ? (top.powershell_tool ?? false) : false,
  };

  // Top-level wins over legacy when both are present.
  if (top === false) {
    return base; // hard disable
  }
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
      subagent_background: topSubagentBg,
      detach_on_user_message: topDetachOnUserMessage,
      powershell_tool: top.powershell_tool ?? false,
    };
  }

  // Top-level absent. Honor legacy experimental.bash.* if any sub-flag was
  // explicitly set — preserves pre-v0.27.2 opt-in semantics. We treat
  // `experimental.bash: {}` (object present but all keys absent) the same as
  // legacy entirely absent so we don't accidentally disable bash for users
  // who wrote an empty experimental block while migrating.
  const hasLegacyFeatureFlag =
    legacy &&
    (legacy.rewrite !== undefined ||
      legacy.compress !== undefined ||
      legacy.background !== undefined);
  if (hasLegacyFeatureFlag) {
    const rewrite = legacy.rewrite === true;
    const compress = legacy.compress === true;
    const background = legacy.background === true;
    return {
      ...base,
      enabled: rewrite || compress || background,
      rewrite,
      compress,
      background,
    };
  }

  // No top-level, no legacy → fall back to surface default.
  return {
    ...base,
    enabled: surfaceDefaultEnabled,
    rewrite: surfaceDefaultEnabled,
    compress: surfaceDefaultEnabled,
    background: surfaceDefaultEnabled,
  };
}

export function resolveExperimentalConfigForConfigure(
  config: AftConfig,
): ConfigureExperimentalOverrides {
  const overrides: ConfigureExperimentalOverrides = {};
  // Bash sub-features always flow through `resolveBashConfig` now — that
  // function handles the graduated top-level surface, the legacy
  // experimental fallback, and the tool_surface default in one place. We
  // still emit the three flat `experimental_bash_*` wire keys because the
  // Rust configure protocol hasn't been renamed; renaming there too would
  // require a coordinated binary bump.
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
 * source string. Inline trailing comments are kept verbatim; block comments
 * are normalized to one line. Used as a backup safety net during migration so
 * comments attached to deleted/reshaped keys don't disappear silently — any
 * captured comment that doesn't survive the comment-json round-trip is
 * prepended to the rewritten file.
 */
function extractCommentsForPreservation(content: string): string[] {
  const comments: string[] = [];
  // Match `//` line comments — both standalone (own-line) and inline trailing
  // (after a value). Stripping any leading whitespace gives us a normalized
  // form that we can dedupe against the rewritten file later.
  const linePattern = /\/\/[^\n]*/g;
  for (const match of content.match(linePattern) ?? []) {
    comments.push(match.trim());
  }
  // Block comments may span multiple lines; collapse internal whitespace so
  // they fit on a single preservation line if we have to relocate them.
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
  oldKeys.push(...migrateExperimentalBashBlock(rawConfig, configPath, logger));
  return oldKeys;
}

/**
 * Graduation migration: `experimental.bash.*` → top-level `bash.*` (v0.27.2).
 *
 * Different shape than the flat-key migrations above: we move a whole nested
 * object up one level AND normalize defaults so the user's pre-migration
 * runtime behavior is preserved exactly. Inspired by magic-context's
 * `migrateLegacyExperimental` pattern (`packages/plugin/src/config/index.ts`
 * in opencode-magic-context), adapted for AFT's already-on-disk rewrite path
 * (so users don't even need to run `doctor`).
 *
 * Behavior:
 *   - If user has BOTH `experimental.bash` and top-level `bash`, top-level
 *     wins and we still strip the experimental block so the config stays
 *     clean (warned so the user knows their experimental keys were dropped).
 *   - If user has only `experimental.bash`, it lifts to top-level `bash` as
 *     an explicit object with all three sub-features materialized. This
 *     preserves the old default semantics: `experimental.bash: { rewrite:
 *     true }` had `compress: false, background: false` by default (the
 *     experimental block was opt-in). The new top-level `bash: { rewrite:
 *     true }` defaults `compress` and `background` to `true` (the block
 *     itself graduated to on-by-default). To prevent a silent behavior
 *     change, we materialize the implicit `false`s so the migrated config
 *     reads exactly as the old runtime did. Users can manually trim it to
 *     `bash: true` (or remove it for the new default) afterwards.
 *   - Tuning fields (`long_running_reminder_*`) carry through unchanged.
 *   - If `experimental` becomes an empty object after removing the bash
 *     block, the whole `experimental` key is dropped so we don't leave a
 *     dangling `"experimental": {}` in the user's file.
 *
 * Returns the list of migrated keys (formatted as `experimental.bash.*`) so
 * the caller's "migrated config" log line mentions them.
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

  // Non-object legacy value (e.g. `experimental.bash: true`) — shouldn't
  // exist historically but be defensive. Drop it without inventing a
  // top-level shape; the user can rewrite it themselves.
  if (typeof legacyBash !== "object" || legacyBash === null || Array.isArray(legacyBash)) {
    delete expRecord.bash;
    if (Object.keys(expRecord).length === 0) delete rawConfig.experimental;
    return ["experimental.bash"];
  }

  const bashRecord = legacyBash as Record<string, unknown>;
  const hasFeatureFlag =
    "rewrite" in bashRecord || "compress" in bashRecord || "background" in bashRecord;

  // Pure tuning-only block (e.g. only long_running_reminder_*). Nothing
  // semantic to graduate — materializing implicit feature flags here would
  // surprise users who never opted into bash hoisting. Leave it alone.
  if (!hasFeatureFlag) return [];

  const movedKeys = Object.keys(bashRecord).map((k) => `experimental.bash.${k}`);

  if (Object.hasOwn(rawConfig, "bash")) {
    logger?.warn(
      `Config migration conflict at ${configPath}: experimental.bash dropped because top-level "bash" is already set`,
    );
  } else {
    // Materialize all three sub-features with their pre-migration runtime
    // values. `=== true` collapses missing/undefined/null to false, which
    // is exactly how the old experimental block treated unset sub-flags.
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

  // Strip an empty experimental object so the user's file doesn't keep an
  // orphan `"experimental": {}` after migration.
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

    // `comment-json` preserves comments natively through parse → mutate →
    // stringify round-trip, including inline trailing comments and block
    // comments — for any keys that survived the migration. Comments
    // attached to keys we DELETED get dropped (they have no semantic anchor
    // in the new shape). To keep user-authored prose around, we pull every
    // comment out of the original file and prepend any that didn't make it
    // into the rewritten form back onto the top so nothing is silently lost.
    const serialized = `${stringifyJsonc(rawConfig, null, 2)}\n`;
    const originalComments = extractCommentsForPreservation(content);
    const droppedComments = originalComments.filter(
      (comment) => !serialized.includes(comment.trim()),
    );
    const nextContent =
      droppedComments.length > 0 ? `${droppedComments.join("\n")}\n${serialized}` : serialized;

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
// Partial parse (valid sections survive, invalid sections are skipped)
// ---------------------------------------------------------------------------

function parseConfigPartially(rawConfig: Record<string, unknown>): AftConfig | null {
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

  const fullResult = AftConfigSchema.safeParse(configForParsing);
  if (fullResult.success) {
    return fullResult.data;
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
// Load config from a single file path
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Config parse failures (syntax errors — file exists but unparseable)
// ---------------------------------------------------------------------------

export type ConfigLoadError = { path: string; message: string };

let configLoadErrors: ConfigLoadError[] = [];

/** Errors from the most recent {@link loadAftConfig} call (parse failures only). */
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

// ---------------------------------------------------------------------------
// Load config from a single file path
// ---------------------------------------------------------------------------

function loadConfigFromPath(configPath: string): AftConfig | null {
  try {
    if (!existsSync(configPath)) {
      return null;
    }

    const content = readFileSync(configPath, "utf-8");
    const rawConfig = parseJsonc<Record<string, unknown>>(content);
    migrateRawConfig(rawConfig, configPath, { log, warn });
    // comment-json attaches Symbol(before/after:<key>) props to track comments.
    // Zod stringifies keys when building error paths, which throws on those
    // symbols and would silently drop the whole config to defaults (issue #88).
    // Validate against a symbol-free deep copy; the migration disk-write path
    // above still uses the symbol-bearing object so comments survive.
    const cleanConfig = stripJsoncSymbols(rawConfig);
    warnIgnoredNestedHarnesses(cleanConfig, configPath);
    const result = AftConfigSchema.safeParse(cleanConfig);

    if (result.success) {
      log(`Config loaded from ${configPath}`);
      return applyActiveHarnessOverride(result.data);
    }

    const errorMsg = result.error.issues.map((i) => `${i.path.join(".")}: ${i.message}`).join(", ");
    warn(`Config validation error in ${configPath}: ${errorMsg}`);

    const partial = parseConfigPartially(cleanConfig);
    return partial ? applyActiveHarnessOverride(partial) : null;
  } catch (err) {
    const errorMsg = err instanceof Error ? err.message : String(err);
    error(`Error loading config from ${configPath}: ${errorMsg}`);
    recordConfigParseFailure(configPath, errorMsg);
    return null;
  }
}

// ---------------------------------------------------------------------------
// Merge configs (project overrides user, deep-merge nested maps/blocks)
// ---------------------------------------------------------------------------

function mergeSemanticConfig(
  baseSemantic: AftConfig["semantic"],
  overrideSemantic: AftConfig["semantic"],
): AftConfig["semantic"] {
  // Only include DEFINED safe fields from the project override.
  // Undefined fields must NOT overwrite user-level values via spread.
  const projectSemantic: Record<string, unknown> = {};
  if (overrideSemantic) {
    if (overrideSemantic.model !== undefined) projectSemantic.model = overrideSemantic.model;
    if (overrideSemantic.timeout_ms !== undefined)
      projectSemantic.timeout_ms = overrideSemantic.timeout_ms;
    if (overrideSemantic.max_batch_size !== undefined)
      projectSemantic.max_batch_size = overrideSemantic.max_batch_size;
    if (overrideSemantic.max_files !== undefined)
      projectSemantic.max_files = overrideSemantic.max_files;
  }

  const semantic = {
    ...baseSemantic,
    ...projectSemantic,
  };

  if (Object.values(semantic).every((value) => value === undefined)) {
    return undefined;
  }

  return Object.fromEntries(
    Object.entries(semantic).filter(([, value]) => value !== undefined),
  ) as AftConfig["semantic"];
}

function mergeLspConfig(
  baseLsp: AftConfig["lsp"],
  overrideLsp: AftConfig["lsp"],
): AftConfig["lsp"] {
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
  // SAFE project-level fields:
  //   - `python` (per-language preference, no executable origin)
  //   - `diagnostics_on_edit` (agent workflow/latency preference only)
  const projectLsp: AftConfig["lsp"] = {};
  if (overrideLsp?.python !== undefined) projectLsp.python = overrideLsp.python;
  if (overrideLsp?.diagnostics_on_edit !== undefined) {
    projectLsp.diagnostics_on_edit = overrideLsp.diagnostics_on_edit;
  }

  // disabled comes from user config ONLY.
  const userDisabled = baseLsp?.disabled ?? [];

  const lsp = {
    ...baseLsp,
    ...projectLsp,
    ...(userDisabled.length > 0 ? { disabled: [...userDisabled] } : {}),
  };

  if (Object.values(lsp).every((value) => value === undefined)) {
    return undefined;
  }

  return Object.fromEntries(
    Object.entries(lsp).filter(([, value]) => value !== undefined),
  ) as AftConfig["lsp"];
}

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

/** Merge sandbox settings while allowing project tiers to add only read denies or enable containment. */
function mergeSandboxConfig(
  base: AftConfig["sandbox"],
  project: AftConfig["sandbox"],
): AftConfig["sandbox"] {
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

/**
 * Deep-merge top-level `bash` config across user and project tiers. Boolean
 * values expand to all feature fields, while object values merge per field.
 */
function mergeBashConfig(
  baseBash: AftConfig["bash"],
  overrideBash: AftConfig["bash"],
): AftConfig["bash"] {
  if (baseBash === undefined && overrideBash === undefined) return undefined;
  if (baseBash === undefined) return overrideBash;
  if (overrideBash === undefined) return baseBash;

  // Expand booleans into the full object so the deep merge below behaves
  // consistently regardless of input shape.
  const expand = (value: AftConfig["bash"]): Record<string, unknown> => {
    if (value === true) return { rewrite: true, compress: true, background: true };
    if (value === false) return { rewrite: false, compress: false, background: false };
    return { ...(value ?? {}) };
  };

  return { ...expand(baseBash), ...expand(overrideBash) };
}

function mergeExperimentalConfig(
  baseExperimental: AftConfig["experimental"],
  overrideExperimental: AftConfig["experimental"],
): AftConfig["experimental"] {
  const bash: Record<string, unknown> = {
    ...baseExperimental?.bash,
    ...overrideExperimental?.bash,
  };
  const experimental: Record<string, unknown> = {
    ...baseExperimental,
    ...overrideExperimental,
  };

  if (Object.values(bash).some((value) => value !== undefined)) {
    experimental.bash = bash;
  } else {
    delete experimental.bash;
  }
  if (Object.values(experimental).every((value) => value === undefined)) {
    return undefined;
  }

  return Object.fromEntries(
    Object.entries(experimental).filter(([, value]) => value !== undefined),
  ) as AftConfig["experimental"];
}

function getProjectLspStrippedKeys(lsp: AftConfig["lsp"]): string[] {
  if (!lsp) {
    return [];
  }

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
  "idle",
  "worktree",
  // Git attribution only changes commit metadata; it grants no capabilities and
  // does not select an executable, so project configuration may override it.
  "git",
  "experimental",
  // Graduated bash family (v0.27.2). Same reasoning as `experimental`:
  // project-settable so users can opt out per-repo (e.g. `bash: false` in
  // a repo with weird shell needs) or opt in. NOT a security boundary —
  // bash hoist disabling is a UX/safety preference, and OpenCode's permission
  // rules still gate the underlying execution either way.
  "bash",
  // "disabled_tools" handled separately — unioned via array merge.
  // "formatter"/"checker" handled separately — deep-merged.
  // "semantic"/"lsp" handled separately — strict field-level merge.
  // "inspect"/"worktree" handled separately — deep-merged.
  // "backup" — USER ONLY (project config cannot disable or shrink undo backups).
  // "index" — USER ONLY (a repository must not mint machine standing roots).
  // "restrict_to_project_root" — USER ONLY (security boundary).
  // "url_fetch_allow_private" — USER ONLY (SSRF surface).
  // "storage_dir" — USER ONLY (controls where AFT writes).
  // "auto_update" — USER ONLY (silently suppressing security updates is a real risk).
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

function mergeProjectBackupConfig(
  base: AftConfig["backup"],
  project: AftConfig["backup"],
): AftConfig["backup"] {
  if (project?.max_file_size === undefined) return base;
  return { ...base, max_file_size: project.max_file_size };
}

function getStrippedTopLevelKeys(override: AftConfig): string[] {
  const stripped: string[] = [];
  if (override.restrict_to_project_root !== undefined) stripped.push("restrict_to_project_root");
  if (override.url_fetch_allow_private !== undefined) stripped.push("url_fetch_allow_private");
  if (override.auto_update !== undefined) stripped.push("auto_update");
  if (override.bridge !== undefined) stripped.push("bridge");
  if (override.backup?.enabled !== undefined || override.backup?.max_depth !== undefined)
    stripped.push("backup");
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
  // Union disabled_tools from both levels (user + project).
  // disabled_tools governs WHICH AFT TOOLS the agent sees — a hostile repo
  // disabling tools is a mild annoyance, not a security boundary, so the
  // union is acceptable here.
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
  const backup = mergeProjectBackupConfig(base.backup, override.backup);
  const bridge = base.bridge;

  // STRICT ALLOWLIST: only project-safe top-level fields are inherited.
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
    // Deep-merge language-scoped maps instead of replacing
    ...(Object.keys(formatter).length > 0 ? { formatter } : {}),
    ...(Object.keys(checker).length > 0 ? { checker } : {}),
    ...(lsp ? { lsp } : {}),
    ...(bash !== undefined ? { bash } : {}),
    ...(inspect !== undefined ? { inspect } : {}),
    ...(worktree !== undefined ? { worktree } : {}),
    ...(sandbox !== undefined ? { sandbox } : {}),
    ...(backup !== undefined ? { backup } : {}),
    experimental,
    // Always set semantic to the merge result (even if undefined) to prevent
    // override.semantic from leaking through any future spread above.
    semantic,
    ...(bridge !== undefined ? { bridge } : {}),
    // Union — both levels contribute to the disabled set
    ...(disabledTools.length > 0 ? { disabled_tools: [...new Set(disabledTools)].sort() } : {}),
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
      operatingHarness: "opencode",
      logger,
    }),
    migrateLegacyAftConfigFile({
      scope: "project",
      targetPath: paths.projectConfigPath,
      legacySources: legacy.project,
      operatingHarness: "opencode",
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

/** Resolve the checkout whose project config controls OpenCode's registered tool schema. */
export function resolveOpenCodeRegistrationRoot(directory: string, worktree?: string): string {
  if (!worktree) return directory;
  const resolvedWorktree = resolvePath(worktree);
  // OpenCode uses the filesystem root as a sentinel for non-Git projects.
  return parsePath(resolvedWorktree).root === resolvedWorktree ? directory : resolvedWorktree;
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

  // Load user config first (base)
  let config: AftConfig = loadConfigFromPath(userConfigPath) ?? {};

  // Override with project config
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
