import { existsSync, readFileSync, renameSync, unlinkSync, writeFileSync } from "node:fs";
import { homedir } from "node:os";
import { join } from "node:path";
import { parse as parseJsonc, stringify as stringifyJsonc } from "comment-json";
import { z } from "zod";
import { error, log, warn } from "./logger.js";

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
  "biome",
  "pyright",
  "ruff",
  "cargo",
  "go",
  "staticcheck",
  "none",
]);

const SemanticBackendEnum = z.enum(["fastembed", "openai_compatible", "ollama"]);

const SemanticConfigSchema = z.object({
  /** Semantic backend type: local fastembed, OpenAI-compatible API, or Ollama. */
  backend: SemanticBackendEnum.optional(),
  /** Model identifier passed to the selected semantic backend. */
  model: z.string().trim().min(1).optional(),
  /** Base URL of the backend API endpoint. */
  base_url: z.string().trim().min(1).optional(),
  /** Environment variable that contains the API key used by external backends. */
  api_key_env: z.string().trim().min(1).optional(),
  /** Backend request timeout in milliseconds. */
  timeout_ms: z.number().int().positive().optional(),
  /** Maximum batch size used by the semantic pipeline. */
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
  // Audit-2 v0.17 #10: grace_days must be >= 1 because grace_days: 0 disables
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

export const AftConfigSchema = z
  .object({
    /**
     * Optional JSON Schema URL for editor tooling. Ignored by the plugin at
     * runtime — only present so VS Code/Cursor/etc. pick up the published
     * schema for autocomplete + validation. `aft setup` auto-inserts this.
     */
    $schema: z.string().optional(),
    /** Whether to auto-format files after edits. Default: true. */
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
     * Replace opencode's built-in read/write/edit/apply_patch tools with AFT's
     * faster Rust implementations. Adds backup tracking, auto-formatting,
     * inline diagnostics, and permission checks. Default: true.
     */
    hoist_builtin_tools: z.boolean().optional(),
    /**
     * Tool surface level. Controls which tools are registered:
     * - "minimal":     aft_outline, aft_zoom, aft_safety (no hoisting)
     * - "recommended": minimal + hoisted read/write/edit/apply_patch + lsp_diagnostics
     *                  + ast_grep_search/replace + aft_import (default)
     * - "all":         recommended + aft_navigate, aft_delete, aft_move, aft_transform, aft_refactor
     */
    tool_surface: z.enum(["minimal", "recommended", "all"]).optional(),
    /**
     * List of tool names to disable. Disabled tools are not registered with
     * OpenCode and will be invisible to agents. Use exact tool names, e.g.
     * ["aft_navigate", "aft_refactor"]. Hoisted names ("read", "edit") and
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
    /** Enable semantic search. Default: false. */
    semantic_search: z.boolean().optional(),
    /** Experimental opt-in features. Default: all false. */
    experimental: ExperimentalConfigSchema.optional(),
    /** User-defined and built-in LSP server configuration. */
    lsp: LspConfigSchema.optional(),
    /** Allow URL fetch tools to request private/link-local hosts. Default: false. */
    url_fetch_allow_private: z.boolean().optional(),
    /** External semantic backend configuration for embedding and retrieval. */
    semantic: SemanticConfigSchema.optional(),
    /**
     * Maximum source files allowed for call-graph operations (callers, trace_to,
     * trace_data, impact). Projects above this size return `project_too_large`
     * instead of attempting the reverse-index build. Does not affect grep,
     * glob, read, edit, or any other tool. Default: 5000.
     */
    max_callgraph_files: z.number().int().positive().optional(),
    /** Auto-refresh OpenCode's cached @cortexkit/aft-opencode package when a newer channel version exists. */
    auto_update: z.boolean().optional(),
  })
  .strict();

export type AftConfig = z.infer<typeof AftConfigSchema>;

export type LspServerConfig = z.infer<typeof LspServerSchema>;

export interface ConfigureLspServer {
  id: string;
  extensions: string[];
  binary: string;
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
 * `tool_surface`, `disabled_tools`, and `hoist_builtin_tools` lock at plugin
 * init because OpenCode registers tools synchronously when the plugin
 * function returns. Per-bridge changes to those fields wouldn't take effect.
 */
export function resolveProjectOverridesForConfigure(config: AftConfig): Record<string, unknown> {
  const overrides: Record<string, unknown> = {};

  // Edit-pipeline behavior — overridable per-project.
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
  if (config.semantic_search !== undefined) overrides.semantic_search = config.semantic_search;

  // Bash / LSP / semantic / max_callgraph_files — all flow through dedicated
  // resolvers because they have their own merge / project-safety rules.
  Object.assign(overrides, resolveExperimentalConfigForConfigure(config));
  Object.assign(overrides, resolveLspConfigForConfigure(config));
  if (config.semantic !== undefined) overrides.semantic = config.semantic;
  if (config.max_callgraph_files !== undefined)
    overrides.max_callgraph_files = config.max_callgraph_files;

  return overrides;
}

export function resolveExperimentalConfigForConfigure(
  config: AftConfig,
): ConfigureExperimentalOverrides {
  const overrides: ConfigureExperimentalOverrides = {};
  if (config.experimental?.bash?.rewrite !== undefined) {
    overrides.experimental_bash_rewrite = config.experimental.bash.rewrite;
  }
  if (config.experimental?.bash?.compress !== undefined) {
    overrides.experimental_bash_compress = config.experimental.bash.compress;
  }
  if (config.experimental?.bash?.background !== undefined) {
    overrides.experimental_bash_background = config.experimental.bash.background;
  }
  if (config.experimental?.bash?.long_running_reminder_enabled !== undefined) {
    overrides.bash_long_running_reminder_enabled =
      config.experimental.bash.long_running_reminder_enabled;
  }
  if (config.experimental?.bash?.long_running_reminder_interval_ms !== undefined) {
    overrides.bash_long_running_reminder_interval_ms =
      config.experimental.bash.long_running_reminder_interval_ms;
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
  return oldKeys;
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

// JSONC parsing via comment-json (bundled at build time).
// Preserves comments during round-trip in tui-config.ts.

// ---------------------------------------------------------------------------
// Config file detection (.jsonc preferred over .json)
// ---------------------------------------------------------------------------

function detectConfigFile(basePath: string): {
  format: "json" | "jsonc" | "none";
  path: string;
} {
  const jsoncPath = `${basePath}.jsonc`;
  const jsonPath = `${basePath}.json`;

  if (existsSync(jsoncPath)) {
    return { format: "jsonc", path: jsoncPath };
  }
  if (existsSync(jsonPath)) {
    return { format: "json", path: jsonPath };
  }
  return { format: "none", path: jsonPath };
}

// ---------------------------------------------------------------------------
// Partial parse (valid sections survive, invalid sections are skipped)
// ---------------------------------------------------------------------------

function parseConfigPartially(rawConfig: Record<string, unknown>): AftConfig | null {
  const fullResult = AftConfigSchema.safeParse(rawConfig);
  if (fullResult.success) {
    return fullResult.data;
  }

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
  // SAFE project-level fields: `python` (per-language preference, no
  //   executable origin) and (none right now).
  const projectLsp: AftConfig["lsp"] = {};
  if (overrideLsp?.python !== undefined) projectLsp.python = overrideLsp.python;

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
 * Audit v0.17 #17: previously `restrict_to_project_root`, `url_fetch_allow_private`,
 * and `max_callgraph_files` flowed through the implicit `...safeOverride` spread,
 * allowing project config to weaken security boundaries.
 *
 * (Note: `storage_dir` is not a config-schema field — the plugin always sets
 * it at configure time. It cannot be set from any aft.jsonc file.)
 */
const PROJECT_SAFE_TOP_LEVEL_FIELDS = new Set<keyof AftConfig>([
  "tool_surface",
  "hoist_builtin_tools",
  "format_on_edit",
  "validate_on_edit",
  // Experimental flags: project-settable so users can enable globally
  // and toggle per-project (or vice versa). Project value overrides user value.
  "search_index",
  "semantic_search",
  "experimental",
  // "disabled_tools" handled separately — unioned via array merge.
  // "formatter"/"checker" handled separately — deep-merged.
  // "semantic"/"lsp" handled separately — strict field-level merge.
  // "restrict_to_project_root" — USER ONLY (security boundary).
  // "url_fetch_allow_private" — USER ONLY (SSRF surface).
  // "storage_dir" — USER ONLY (controls where AFT writes).
  // "max_callgraph_files" — USER ONLY (resource budget).
  // "auto_update" — USER ONLY (silently suppressing security updates is a real risk).
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
  if (override.auto_update !== undefined) stripped.push("auto_update");
  return stripped;
}

function mergeConfigs(base: AftConfig, override: AftConfig): AftConfig {
  // Union disabled_tools from both levels (user + project).
  // disabled_tools governs WHICH AFT TOOLS the agent sees — a hostile repo
  // disabling tools is a mild annoyance, not a security boundary, so the
  // union is acceptable here.
  const disabledTools = [...(base.disabled_tools ?? []), ...(override.disabled_tools ?? [])];
  const formatter = { ...base.formatter, ...override.formatter };
  const checker = { ...base.checker, ...override.checker };
  const semantic = mergeSemanticConfig(base.semantic, override.semantic);
  const lsp = mergeLspConfig(base.lsp, override.lsp);
  const experimental = mergeExperimentalConfig(base.experimental, override.experimental);

  // STRICT ALLOWLIST: only project-safe top-level fields are inherited.
  // See PROJECT_SAFE_TOP_LEVEL_FIELDS above for the full security rationale.
  const safeOverride = pickProjectSafeFields(override);

  return {
    ...base,
    ...safeOverride,
    // Deep-merge language-scoped maps instead of replacing
    ...(Object.keys(formatter).length > 0 ? { formatter } : {}),
    ...(Object.keys(checker).length > 0 ? { checker } : {}),
    ...(lsp ? { lsp } : {}),
    experimental,
    // Always set semantic to the merge result (even if undefined) to prevent
    // override.semantic from leaking through any future spread above.
    semantic,
    // Union — both levels contribute to the disabled set
    ...(disabledTools.length > 0 ? { disabled_tools: [...new Set(disabledTools)] } : {}),
  };
}

// ---------------------------------------------------------------------------
// OpenCode config directory detection (same logic as oh-my-opencode)
// ---------------------------------------------------------------------------

function getOpenCodeConfigDir(): string {
  const envDir = process.env.OPENCODE_CONFIG_DIR?.trim();
  if (envDir) {
    return envDir;
  }

  // XDG_CONFIG_HOME or homedir()/.config, then /opencode
  const xdgConfig = process.env.XDG_CONFIG_HOME || join(homedir(), ".config");
  return join(xdgConfig, "opencode");
}

// ---------------------------------------------------------------------------
// Public API: loadAftConfig
// ---------------------------------------------------------------------------

/**
 * Load AFT config using the same two-level pattern as oh-my-opencode:
 *
 * 1. User-level:    ~/.config/opencode/aft.jsonc (or .json)
 * 2. Project-level: <project>/.opencode/aft.jsonc (or .json)
 *
 * Project config merges on top of user config.
 * Both support JSONC (comments allowed).
 * Invalid sections are skipped, valid sections still load.
 */
export function loadAftConfig(projectDirectory: string): AftConfig {
  // User-level config
  const configDir = getOpenCodeConfigDir();
  const userBasePath = join(configDir, "aft");
  migrateAftConfigFile(`${userBasePath}.jsonc`);
  migrateAftConfigFile(`${userBasePath}.json`);
  const userDetected = detectConfigFile(userBasePath);
  const userConfigPath =
    userDetected.format !== "none" ? userDetected.path : `${userBasePath}.json`;

  // Project-level config
  const projectBasePath = join(projectDirectory, ".opencode", "aft");
  migrateAftConfigFile(`${projectBasePath}.jsonc`);
  migrateAftConfigFile(`${projectBasePath}.json`);
  const projectDetected = detectConfigFile(projectBasePath);
  const projectConfigPath =
    projectDetected.format !== "none" ? projectDetected.path : `${projectBasePath}.json`;

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
