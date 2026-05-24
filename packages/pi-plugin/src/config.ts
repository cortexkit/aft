import { existsSync, readFileSync, renameSync, unlinkSync, writeFileSync } from "node:fs";
import { homedir } from "node:os";
import { join } from "node:path";
import { parse as parseJsonc, stringify as stringifyJsonc } from "comment-json";
import { z } from "zod";
import { error, log, warn } from "./logger.js";

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

export type SemanticBackend = "fastembed" | "openai_compatible" | "ollama";

export interface SemanticConfig {
  backend?: SemanticBackend;
  model?: string;
  base_url?: string;
  api_key_env?: string;
  timeout_ms?: number;
  max_batch_size?: number;
}

export interface LspServerConfig {
  id: string;
  extensions: string[];
  binary: string;
  args: string[];
  root_markers: string[];
  disabled: boolean;
  env?: Record<string, string>;
  initialization_options?: unknown;
}

export interface LspConfig {
  servers?: Record<string, Omit<LspServerConfig, "id">>;
  disabled?: string[];
  python?: "pyright" | "ty" | "auto";
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

/**
 * Graduated `bash` config. Replaces `experimental.bash.*` in v0.27.2.
 *
 * Mirrors the OpenCode plugin's `AftConfig.bash` shape exactly so projects
 * using both harnesses get identical resolution semantics. See
 * `resolveBashConfig` below for precedence rules.
 *
 * Three shapes:
 *   - `bash: true`     → all sub-features on
 *   - `bash: false`    → hoist disabled entirely; Pi's native bash stays
 *   - `bash: { ... }`  → partial override; missing sub-keys default to true
 */
export interface BashConfig {
  rewrite?: boolean;
  compress?: boolean;
  background?: boolean;
  long_running_reminder_enabled?: boolean;
  long_running_reminder_interval_ms?: number;
}

export interface AftConfig {
  /**
   * Optional JSON Schema URL for editor tooling. Runtime no-op — only present
   * so VS Code/Cursor/etc. pick up the published schema for autocomplete +
   * validation. `aft setup` auto-inserts this.
   */
  $schema?: string;
  format_on_edit?: boolean;
  /** Maximum formatter subprocess wallclock seconds. Bounded 1..=600. Default 10. */
  formatter_timeout_secs?: number;
  validate_on_edit?: "syntax" | "full";
  formatter?: Record<string, Formatter>;
  checker?: Record<string, Checker>;
  tool_surface?: ToolSurface;
  disabled_tools?: string[];
  restrict_to_project_root?: boolean;
  search_index?: boolean;
  semantic_search?: boolean;
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
  /**
   * Maximum source files allowed for call-graph operations (callers, trace_to,
   * trace_data, impact). Projects above this size return `project_too_large`.
   * Default: 20000 (applied Rust-side; undefined here means "use default").
   */
  max_callgraph_files?: number;
}

/**
 * Resolved bash config: every flag has an explicit boolean.
 */
export interface ResolvedBashConfig {
  enabled: boolean;
  rewrite: boolean;
  compress: boolean;
  background: boolean;
  long_running_reminder_enabled?: boolean;
  long_running_reminder_interval_ms?: number;
}

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

  const base: ResolvedBashConfig = {
    enabled: false,
    rewrite: false,
    compress: false,
    background: false,
    long_running_reminder_enabled: reminderEnabled,
    long_running_reminder_interval_ms: reminderInterval,
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

// TODO: move this schema to a shared package/module with aft-opencode to avoid drift.

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

const SemanticConfigSchema = z.object({
  backend: z.enum(["fastembed", "openai_compatible", "ollama"]).optional(),
  model: z.string().trim().min(1).optional(),
  base_url: z.string().trim().min(1).optional(),
  api_key_env: z.string().trim().min(1).optional(),
  timeout_ms: z.number().int().positive().optional(),
  max_batch_size: z.number().int().positive().optional(),
});

const LspExtensionSchema = z
  .string()
  .trim()
  .min(1)
  .refine((value) => value.replace(/^\.+/, "").length > 0, {
    message: "Extension must include characters other than leading dots",
  });

const LspServerEntrySchema = z.object({
  extensions: z.array(LspExtensionSchema).min(1),
  binary: z.string().trim().min(1),
  args: z.array(z.string()).optional().default([]),
  root_markers: z.array(z.string().trim().min(1)).optional().default([".git"]),
  disabled: z.boolean().optional().default(false),
  /** Extra environment variables passed to the LSP server child process. */
  env: z.record(z.string().min(1), z.string()).optional(),
  /** JSON value passed as `initializationOptions` in the LSP `initialize` request. */
  initialization_options: z.unknown().optional(),
});

export const LspServerSchema = LspServerEntrySchema.extend({
  id: z.string().trim().min(1),
});

const LspConfigSchema = z.object({
  servers: z.record(z.string().trim().min(1), LspServerEntrySchema).optional(),
  disabled: z.array(z.string().trim().min(1)).optional(),
  python: z.enum(["pyright", "ty", "auto"]).optional(),
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
  // Audit-2 v0.17 #10: grace_days must be >= 1 because grace_days: 0 disables
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
  long_running_reminder_enabled: z.boolean().optional(),
  long_running_reminder_interval_ms: z.number().int().positive().optional(),
});
const BashConfigSchema = z.union([z.boolean(), BashFeaturesSchema]);

export const AftConfigSchema = z
  .object({
    /**
     * Optional JSON Schema URL for editor tooling. Ignored by the plugin at
     * runtime — only present so VS Code/Cursor/etc. pick up the published
     * schema for autocomplete + validation. `aft setup` auto-inserts this.
     */
    $schema: z.string().optional(),
    format_on_edit: z.boolean().optional(),
    formatter_timeout_secs: z.number().int().min(1).max(600).optional(),
    validate_on_edit: z.enum(["syntax", "full"]).optional(),
    formatter: z.record(z.string(), FormatterEnum).optional(),
    checker: z.record(z.string(), CheckerEnum).optional(),
    tool_surface: z.enum(["minimal", "recommended", "all"]).optional(),
    disabled_tools: z.array(z.string()).optional(),
    restrict_to_project_root: z.boolean().optional(),
    search_index: z.boolean().optional(),
    semantic_search: z.boolean().optional(),
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
    max_callgraph_files: z.number().int().positive().optional(),
  })
  .strict();

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
      extensions: server.extensions.map(normalizeLspExtension),
      binary: server.binary,
      args: server.args,
      root_markers: server.root_markers,
      disabled: server.disabled,
    };
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

function detectConfigFile(basePath: string): {
  format: "json" | "jsonc" | "none";
  path: string;
} {
  const jsoncPath = `${basePath}.jsonc`;
  const jsonPath = `${basePath}.json`;
  if (existsSync(jsoncPath)) return { format: "jsonc", path: jsoncPath };
  if (existsSync(jsonPath)) return { format: "json", path: jsonPath };
  return { format: "none", path: jsonPath };
}

function loadConfigFromPath(configPath: string): AftConfig | null {
  try {
    if (!existsSync(configPath)) return null;
    const content = readFileSync(configPath, "utf-8");
    const rawConfig = parseJsonc<Record<string, unknown>>(content);
    if (!rawConfig || typeof rawConfig !== "object" || Array.isArray(rawConfig)) {
      warn(`Config validation error in ${configPath}: root must be an object`);
      return null;
    }
    migrateRawConfig(rawConfig, configPath, { log, warn });
    const result = AftConfigSchema.safeParse(rawConfig);

    if (result.success) {
      log(`Config loaded from ${configPath}`);
      return result.data;
    }

    const errorMsg = result.error.issues.map((i) => `${i.path.join(".")}: ${i.message}`).join(", ");
    warn(`Config validation error in ${configPath}: ${errorMsg}`);
    return parseConfigPartially(rawConfig);
  } catch (err) {
    const errorMsg = err instanceof Error ? err.message : String(err);
    error(`Error loading config from ${configPath}: ${errorMsg}`);
    return null;
  }
}

function parseConfigPartially(rawConfig: Record<string, unknown>): AftConfig {
  const partialConfig: Record<string, unknown> = {};
  const invalidSections: string[] = [];

  for (const key of Object.keys(rawConfig)) {
    const sectionResult = AftConfigSchema.safeParse({ [key]: rawConfig[key] });
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
  const projectSafe: LspConfig = {};
  if (override?.python !== undefined) projectSafe.python = override.python;

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

/**
 * Deep-merge top-level `bash` config across user + project. Mirrors the
 * OpenCode plugin so a project can override one sub-feature without nuking
 * the user's other sub-features. Handles boolean and object shapes.
 */
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
 * Audit v0.17 #17: previously `restrict_to_project_root`, `url_fetch_allow_private`,
 * and `max_callgraph_files` flowed through the implicit `...safeOverride` spread,
 * allowing project config to weaken security boundaries.
 *
 * (Note: `storage_dir` is not a config-schema field — the plugin always sets
 * it at configure time. It cannot be set from any aft.jsonc file.)
 */
const PROJECT_SAFE_TOP_LEVEL_FIELDS = new Set<keyof AftConfig>([
  "tool_surface",
  // (Pi schema does not currently expose `hoist_builtin_tools`; if added, mark safe.)
  "format_on_edit",
  "validate_on_edit",
  // Experimental flags: project-settable so users can enable globally
  // and toggle per-project (or vice versa). Project value overrides user value.
  "search_index",
  "semantic_search",
  "experimental",
  // Graduated bash family (v0.27.2). Same reasoning as `experimental`:
  // project-settable so users can opt out per-repo (e.g. `bash: false` in a
  // repo with weird shell needs) or opt in. NOT a security boundary — bash
  // hoist disabling is a UX/safety preference, not access control.
  "bash",
  // "disabled_tools" handled separately — unioned via array merge.
  // "formatter"/"checker" handled separately — deep-merged.
  // "semantic"/"lsp" handled separately — strict field-level merge.
  // "restrict_to_project_root" — USER ONLY (security boundary).
  // "url_fetch_allow_private" — USER ONLY (SSRF surface).
  // "max_callgraph_files" — USER ONLY (resource budget).
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
  if (override.max_callgraph_files !== undefined) stripped.push("max_callgraph_files");
  return stripped;
}

function mergeConfigs(base: AftConfig, override: AftConfig): AftConfig {
  const disabledTools = [...(base.disabled_tools ?? []), ...(override.disabled_tools ?? [])];
  const formatter = { ...base.formatter, ...override.formatter };
  const checker = { ...base.checker, ...override.checker };
  const semantic = mergeSemanticConfig(base.semantic, override.semantic);
  const lsp = mergeLspConfig(base.lsp, override.lsp);
  const experimental = mergeExperimentalConfig(base.experimental, override.experimental);
  const bash = mergeBashConfig(base.bash, override.bash);

  // STRICT ALLOWLIST: only project-safe top-level fields are inherited.
  // See PROJECT_SAFE_TOP_LEVEL_FIELDS above for the full security rationale.
  // We deep-merge `bash` separately so the field-by-field union beats the
  // shallow allowlist spread; otherwise project's `bash: { compress: false }`
  // would wipe out user's `bash: { rewrite: true }`.
  const safeOverride = pickProjectSafeFields(override);
  delete safeOverride.bash;

  return {
    ...base,
    ...safeOverride,
    ...(Object.keys(formatter).length > 0 ? { formatter } : {}),
    ...(Object.keys(checker).length > 0 ? { checker } : {}),
    ...(lsp ? { lsp } : {}),
    ...(bash !== undefined ? { bash } : {}),
    experimental,
    semantic,
    ...(disabledTools.length > 0 ? { disabled_tools: [...new Set(disabledTools)] } : {}),
  };
}

// ---------------------------------------------------------------------------
// Pi config directory detection
//
// Pi's convention:
//   - Global: ~/.pi/agent/
//   - Project: <projectDir>/.pi/
// ---------------------------------------------------------------------------

function getGlobalPiDir(): string {
  return join(homedir(), ".pi", "agent");
}

/**
 * Load AFT config:
 *   1. User-level:    ~/.pi/agent/aft.jsonc (or .json)
 *   2. Project-level: <project>/.pi/aft.jsonc (or .json)
 *
 * Project config merges on top of user config.
 */
export function loadAftConfig(projectDirectory: string): AftConfig {
  const userBasePath = join(getGlobalPiDir(), "aft");
  migrateAftConfigFile(`${userBasePath}.jsonc`);
  migrateAftConfigFile(`${userBasePath}.json`);
  const userDetected = detectConfigFile(userBasePath);
  const userConfigPath =
    userDetected.format !== "none" ? userDetected.path : `${userBasePath}.json`;

  const projectBasePath = join(projectDirectory, ".pi", "aft");
  migrateAftConfigFile(`${projectBasePath}.jsonc`);
  migrateAftConfigFile(`${projectBasePath}.json`);
  const projectDetected = detectConfigFile(projectBasePath);
  const projectConfigPath =
    projectDetected.format !== "none" ? projectDetected.path : `${projectBasePath}.json`;

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
