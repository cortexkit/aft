/**
 * Feature-based configuration policy shared by the OpenCode and Pi plugins.
 *
 * Mirrors `crates/aft/src/feature_config.rs`: the literal tool inventory, the
 * table of retired keys, the per-block translation of those keys into
 * canonical `disabled_tools` / `indexes` / `github` / `lsp` leaves (retired
 * keys are never refused),
 * validation of a resolved configuration, and the migration-notice projection
 * digest. The config parity fixtures and the shared notice-projection fixture
 * (`crates/aft/tests/fixtures/feature_config/notice_projection.json`) keep the
 * two implementations equal.
 */

import { createHash } from "node:crypto";

/** Every agent-visible tool name a registration adapter may publish (sorted). */
export const CANONICAL_TOOLS = [
  "aft_callgraph",
  "aft_conflicts",
  "aft_delete",
  "aft_import",
  "aft_inspect",
  "aft_move",
  "aft_outline",
  "aft_safety",
  "aft_search",
  "aft_zoom",
  "apply_patch",
  "ast_grep_replace",
  "ast_grep_search",
  "bash",
  "bash_kill",
  "bash_status",
  "bash_watch",
  "bash_write",
  "edit",
  "glob",
  "grep",
  "read",
  "write",
] as const;

export type CanonicalTool = (typeof CANONICAL_TOOLS)[number];

/** The seven host tool slots AFT takes over. */
export const HOST_TOOL_NAMES = [
  "apply_patch",
  "bash",
  "edit",
  "glob",
  "grep",
  "read",
  "write",
] as const;

/** Disables applied when the user base makes no registration choice. */
export const DEFAULT_DISABLED_TOOLS = ["aft_delete", "aft_move"] as const;

/** Historical prefixed tool names that disabled lists may still contain. */
export const LEGACY_TOOL_ALIASES: Readonly<Record<string, string>> = {
  aft_read: "read",
  aft_write: "write",
  aft_edit: "edit",
  aft_apply_patch: "apply_patch",
  aft_grep: "grep",
  aft_glob: "glob",
  aft_bash: "bash",
};

/** Names the historical bash registration gate removed (canonical names). */
export const BASH_GATE_DISABLES = [
  "bash",
  "bash_kill",
  "bash_status",
  "bash_watch",
  "bash_write",
] as const;

export const MIGRATION_POLICY_ID = "feature-config-v1";
export const POLICY_INTRODUCED_MINOR: readonly [number, number] = [0, 58];

/** Retired config paths and their replacement. */
export const RETIRED_PATHS: ReadonlyArray<readonly [string, string]> = [
  ["tool_surface", "disabled_tools"],
  ["hoist_builtin_tools", "disabled_tools"],
  ["enabled", "disabled_tools"],
  ["search_index", "indexes.trigram"],
  ["experimental_search_index", "indexes.trigram"],
  ["semantic_search", "indexes.semantic"],
  ["experimental_semantic_search", "indexes.semantic"],
  ["callgraph_store", "indexes.callgraph"],
  ["github.enabled", "github.read,github.write,github.shim"],
  ["experimental_lsp_ty", "experimental.lsp_ty"],
  ["experimental_bash_rewrite", "bash.rewrite"],
  ["experimental_bash_compress", "bash.compress"],
  ["experimental_bash_background", "bash.background"],
  ["experimental.bash", "bash"],
];

/** Index leaf, immediate legacy key and optional experimental alias. */
export const INDEX_INPUTS: ReadonlyArray<readonly [string, string, string | undefined]> = [
  ["trigram", "search_index", "experimental_search_index"],
  ["semantic", "semantic_search", "experimental_semantic_search"],
  ["callgraph", "callgraph_store", undefined],
];

/**
 * Registration adapters that cannot register some canonical tools because the
 * adapter has no implementation for them. The registered set on an adapter is
 * the canonical inventory minus resolved disables minus this list; setup
 * reports these rows as unavailable with reason `no_implementation_on_harness`.
 */
export const ADAPTER_UNIMPLEMENTED_TOOLS: Readonly<
  Record<"opencode-v1" | "opencode-v2" | "pi" | "omp", readonly CanonicalTool[]>
> = {
  "opencode-v1": [],
  "opencode-v2": [],
  pi: ["apply_patch", "glob"],
  omp: ["apply_patch", "glob"],
};

export function isKnownTool(name: string): boolean {
  return (CANONICAL_TOOLS as readonly string[]).includes(name);
}

export function isProjectProtectedTool(name: string): boolean {
  return name === "aft_safety" || (HOST_TOOL_NAMES as readonly string[]).includes(name);
}

/** Literal disabled set a legacy `tool_surface` value translates to. */
export function surfaceDisables(surface: unknown): string[] | undefined {
  switch (surface) {
    case "all":
      return [];
    case "recommended":
      return ["aft_callgraph", "aft_delete", "aft_move"];
    case "minimal":
      return CANONICAL_TOOLS.filter(
        (name) => name !== "aft_outline" && name !== "aft_zoom" && name !== "aft_safety",
      );
    default:
      return undefined;
  }
}

export interface TranslationWarning {
  code: string;
  key: string;
  message: string;
  /**
   * Deliver through the once-per-identity migration notice channel instead of
   * warning on every load. Set for notes about files that are already fixed,
   * which would otherwise repeat forever.
   */
  once?: boolean;
}

export interface DocumentTranslation {
  warnings: TranslationWarning[];
  /** True when a retired key or alias was supplied (a migration notice applies). */
  legacyInput: boolean;
  /**
   * Every retired key the document supplied, sorted and de-duplicated. A key
   * inside a harness block carries its `harnesses.<id>.` prefix; a prefixed
   * tool name inside `disabled_tools` is listed by that name.
   */
  retiredKeys: string[];
  /**
   * True when some block set the retired top-level `enabled: false`. It is
   * translated into a disabled_tools list only, so indexes keep building;
   * the migration notice has to say so.
   */
  retiredEnabledFalse: boolean;
}

type JsonRecord = Record<string, unknown>;

function isRecord(value: unknown): value is JsonRecord {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

function hasPath(map: JsonRecord, path: string): boolean {
  const dot = path.indexOf(".");
  if (dot < 0) return Object.hasOwn(map, path);
  const container = map[path.slice(0, dot)];
  return isRecord(container) && Object.hasOwn(container, path.slice(dot + 1));
}

function falseRuntimeGates(map: JsonRecord): string[] {
  const gates: string[] = [];
  if (isRecord(map.backup) && map.backup.enabled === false) gates.push("backup.enabled");
  if (isRecord(map.inspect) && map.inspect.enabled === false) gates.push("inspect.enabled");
  if (map.bash === false) gates.push("bash");
  else if (isRecord(map.bash) && map.bash.enabled === false) gates.push("bash.enabled");
  return gates;
}

export function sortedUnique(names: Iterable<string>): string[] {
  return [...new Set(names)].sort();
}

/** `lsp.idle_minutes` default and range; mirrors `crates/aft/src/config.rs`. */
export const LSP_IDLE_MINUTES_DEFAULT = 60;
export const LSP_IDLE_MINUTES_MIN = 5;
export const LSP_IDLE_MINUTES_MAX = 1440;

function recordRetired(out: DocumentTranslation, prefix: string, key: string): void {
  out.legacyInput = true;
  out.retiredKeys.push(`${prefix}${key}`);
}

function superseded(out: DocumentTranslation, key: string, message: string): void {
  out.warnings.push({ code: "superseded_legacy_config", key, message });
}

function nestedBool(map: JsonRecord, container: string, leaf: string): boolean | undefined {
  const inner = map[container];
  return isRecord(inner) && typeof inner[leaf] === "boolean" ? (inner[leaf] as boolean) : undefined;
}

/**
 * Translate the GitHub enable aliases (`gh_read.enabled`, `gh_shim.enabled`)
 * of one block into `github.read` / `github.shim`, exactly as `doctor --fix`
 * rewrites them. Precedence: the canonical leaf, then the `false` that a
 * `github.enabled: false` in the same block sets for every leaf, then the
 * alias. The aliases are removed; `gh_shim.binary_path` is kept.
 */
function translateGithubAliases(map: JsonRecord, prefix: string, out: DocumentTranslation): void {
  const masterOff = nestedBool(map, "github", "enabled") === false;
  for (const [alias, leaf, retiredKey] of [
    ["gh_read", "read", "gh_read"],
    ["gh_shim", "shim", "gh_shim.enabled"],
  ] as const) {
    const aliasBlock = map[alias];
    const supplied =
      alias === "gh_read"
        ? Object.hasOwn(map, alias)
        : isRecord(aliasBlock) && Object.hasOwn(aliasBlock, "enabled");
    if (!supplied) continue;
    recordRetired(out, prefix, retiredKey);
    const aliasValue =
      isRecord(aliasBlock) && typeof aliasBlock.enabled === "boolean"
        ? aliasBlock.enabled
        : undefined;
    if (aliasValue !== undefined) {
      const canonical = nestedBool(map, "github", leaf);
      if (canonical !== undefined) {
        if (canonical !== aliasValue) {
          superseded(
            out,
            `${prefix}${retiredKey}`,
            `${alias}.enabled=${aliasValue} is ignored because github.${leaf}=${canonical} is set`,
          );
        }
      } else if (masterOff) {
        if (aliasValue) {
          superseded(
            out,
            `${prefix}${retiredKey}`,
            `${alias}.enabled=true is ignored because github.enabled=false switches github.${leaf} off`,
          );
        }
      } else {
        if (!Object.hasOwn(map, "github")) map.github = {};
        if (isRecord(map.github)) map.github[leaf] = aliasValue;
      }
    }
    if (alias === "gh_read") {
      delete map.gh_read;
    } else if (isRecord(aliasBlock)) {
      delete aliasBlock.enabled;
      if (Object.keys(aliasBlock).length === 0) delete map.gh_shim;
    }
  }
}

/** Remove `container.leaf` when present, dropping the container if that left it empty. */
function takeNested(
  map: JsonRecord,
  container: string,
  leaf: string,
): { found: boolean; value?: unknown } {
  const inner = map[container];
  if (!isRecord(inner) || !Object.hasOwn(inner, leaf)) return { found: false };
  const value = inner[leaf];
  delete inner[leaf];
  if (Object.keys(inner).length === 0) delete map[container];
  return { found: true, value };
}

/**
 * Translate the retired inspect/LSP keys of one block, exactly as
 * `doctor --fix` rewrites them: `idle.lsp_ttl_minutes` becomes
 * `lsp.idle_minutes` (clamped to its range) unless that is already set, and
 * the two inspect keys that never had an effect are dropped.
 */
function translateInspectLspPaths(map: JsonRecord, prefix: string, out: DocumentTranslation): void {
  const ttl = takeNested(map, "idle", "lsp_ttl_minutes");
  if (ttl.found) {
    recordRetired(out, prefix, "idle.lsp_ttl_minutes");
    if (isRecord(map.lsp) && map.lsp.idle_minutes !== undefined) {
      superseded(
        out,
        `${prefix}idle.lsp_ttl_minutes`,
        "idle.lsp_ttl_minutes is ignored because lsp.idle_minutes is set",
      );
    } else {
      const raw =
        typeof ttl.value === "number" && Number.isSafeInteger(ttl.value)
          ? ttl.value
          : LSP_IDLE_MINUTES_DEFAULT;
      const minutes = Math.min(LSP_IDLE_MINUTES_MAX, Math.max(LSP_IDLE_MINUTES_MIN, raw));
      if (!Object.hasOwn(map, "lsp")) map.lsp = {};
      if (isRecord(map.lsp)) map.lsp.idle_minutes = minutes;
    }
  }
  for (const leaf of ["tier2_soft_deadline_ms", "max_drill_down_items"]) {
    if (takeNested(map, "inspect", leaf).found) recordRetired(out, prefix, `inspect.${leaf}`);
  }
}

/**
 * Translate flat experimental aliases to nested settings. Graduating
 * experimental.bash explicitly sets omitted feature flags to false;
 * otherwise the current default-on bash settings would silently enable them.
 */
function translateExperimentalPaths(
  map: JsonRecord,
  prefix: string,
  out: DocumentTranslation,
): void {
  for (const [oldKey, leaf] of [
    ["experimental_lsp_ty", "lsp_ty"],
    ["experimental_bash_rewrite", "rewrite"],
    ["experimental_bash_compress", "compress"],
    ["experimental_bash_background", "background"],
  ]) {
    if (!Object.hasOwn(map, oldKey)) continue;
    recordRetired(out, prefix, oldKey);
    const value = map[oldKey];
    delete map[oldKey];
    if (!isRecord(map.experimental)) map.experimental = {};
    const experimental = map.experimental as JsonRecord;
    let destination = experimental;
    if (leaf !== "lsp_ty") {
      if (!isRecord(experimental.bash)) experimental.bash = {};
      destination = experimental.bash as JsonRecord;
    }
    if (Object.hasOwn(destination, leaf)) {
      superseded(
        out,
        oldKey,
        `${oldKey} is ignored because experimental.${leaf === "lsp_ty" ? leaf : `bash.${leaf}`} is already set`,
      );
    } else {
      destination[leaf] = value;
    }
  }

  if (!isRecord(map.experimental) || !Object.hasOwn(map.experimental, "bash")) return;
  const experimental = map.experimental;
  const legacy = experimental.bash;
  if (isRecord(legacy)) {
    if (!["rewrite", "compress", "background"].some((leaf) => Object.hasOwn(legacy, leaf))) return;
    for (const leaf of Object.keys(legacy)) recordRetired(out, prefix, `experimental.bash.${leaf}`);
    if (Object.hasOwn(map, "bash")) {
      superseded(
        out,
        "experimental.bash",
        'experimental.bash is ignored because top-level "bash" is already set',
      );
    } else {
      const bash: JsonRecord = {
        rewrite: legacy.rewrite === true,
        compress: legacy.compress === true,
        background: legacy.background === true,
      };
      for (const leaf of ["long_running_reminder_enabled", "long_running_reminder_interval_ms"]) {
        if (Object.hasOwn(legacy, leaf)) bash[leaf] = legacy[leaf];
      }
      map.bash = bash;
    }
  } else {
    recordRetired(out, prefix, "experimental.bash");
  }
  delete experimental.bash;
  if (Object.keys(experimental).length === 0) delete map.experimental;
}

/**
 * Translate the retired keys of one tier/harness block in place.
 *
 * The false runtime gates (`backup.enabled`, `inspect.enabled`, `bash`,
 * `bash.enabled`) are current keys and only switch their behaviour off; they
 * never remove a tool registration. Only `disabled_tools` does that.
 */
function translateBlock(
  map: JsonRecord,
  // True only for the user file's base block, the one place the absent-base
  // default disables may be added.
  userBase: boolean,
  blockLabel: string,
  out: DocumentTranslation,
): void {
  const prefix = blockLabel === "base" ? "" : `${blockLabel}.`;
  translateGithubAliases(map, prefix, out);
  translateInspectLspPaths(map, prefix, out);
  translateExperimentalPaths(map, prefix, out);

  let explicitList: string[] | undefined;
  const rawList = map.disabled_tools;
  if (Array.isArray(rawList) && rawList.every((entry) => typeof entry === "string")) {
    explicitList = (rawList as string[]).map((name) => {
      const host = LEGACY_TOOL_ALIASES[name];
      if (host === undefined) return name;
      recordRetired(out, prefix, name);
      return host;
    });
  }

  for (const [path] of RETIRED_PATHS) {
    // A block with only reminder settings never opted into experimental
    // bash features. Leave it unchanged instead of adding false flags.
    if (path === "experimental.bash") continue;
    if (hasPath(map, path)) recordRetired(out, prefix, path);
  }
  const gates = falseRuntimeGates(map);

  if (explicitList !== undefined) map.disabled_tools = explicitList;

  for (const [leaf, legacy, experimental] of INDEX_INPUTS) {
    const indexes = isRecord(map.indexes) ? map.indexes : undefined;
    const canonical = typeof indexes?.[leaf] === "boolean" ? (indexes[leaf] as boolean) : undefined;
    const inputs: Array<[string, unknown]> = [];
    if (Object.hasOwn(map, legacy)) {
      inputs.push([legacy, map[legacy]]);
      delete map[legacy];
    }
    if (experimental !== undefined && Object.hasOwn(map, experimental)) {
      inputs.push([experimental, map[experimental]]);
      delete map[experimental];
    }
    let chosen: boolean | undefined;
    for (const [key, value] of inputs) {
      if (typeof value !== "boolean") {
        out.warnings.push({
          code: "invalid_legacy_config",
          key,
          message: `Ignoring non-boolean ${key}; use indexes.${leaf}`,
        });
        continue;
      }
      if (canonical !== undefined) {
        if (canonical !== value) {
          superseded(
            out,
            key,
            `${key}=${value} is ignored because indexes.${leaf}=${canonical} is set`,
          );
        }
      } else if (chosen !== undefined) {
        if (chosen !== value) {
          superseded(
            out,
            key,
            `${key}=${value} is ignored because ${legacy}=${chosen} takes precedence`,
          );
        }
      } else {
        chosen = value;
      }
    }
    if (canonical === undefined && chosen !== undefined) {
      if (!Object.hasOwn(map, "indexes")) map.indexes = {};
      if (isRecord(map.indexes)) map.indexes[leaf] = chosen;
    }
  }

  if (isRecord(map.github) && Object.hasOwn(map.github, "enabled")) {
    const github = map.github;
    const master = github.enabled;
    delete github.enabled;
    if (master === false) {
      for (const leaf of ["read", "write", "shim"]) {
        if (!Object.hasOwn(github, leaf)) github[leaf] = false;
        else if (github[leaf] === true) {
          superseded(
            out,
            "github.enabled",
            `github.enabled=false is ignored for github.${leaf} because github.${leaf}=true is set`,
          );
        }
      }
    }
  }

  const surface = map.tool_surface;
  const hasSurface = Object.hasOwn(map, "tool_surface");
  const hoist = map.hoist_builtin_tools;
  const enabled = map.enabled;
  delete map.tool_surface;
  delete map.hoist_builtin_tools;
  delete map.enabled;
  const generated = new Set<string>();
  let explicitSurface = false;
  if (hasSurface) {
    const names = surfaceDisables(surface);
    if (names === undefined) {
      out.warnings.push({
        code: "invalid_legacy_config",
        key: "tool_surface",
        message: "Ignoring unknown tool_surface value; use disabled_tools",
      });
    } else {
      explicitSurface = true;
      for (const name of names) generated.add(name);
    }
  }
  if (hoist === false) for (const name of HOST_TOOL_NAMES) generated.add(name);
  if (enabled === false) {
    for (const name of CANONICAL_TOOLS) generated.add(name);
    out.retiredEnabledFalse = true;
    out.warnings.push({
      code: "legacy_enabled_false_indexes_still_build",
      key: blockLabel === "base" ? "enabled" : `${blockLabel}.enabled`,
      message: RETIRED_ENABLED_FALSE_INDEXES_NOTE,
    });
  }

  if (gates.length > 0 && explicitList === undefined) {
    out.warnings.push({
      code: "legacy_runtime_gate_runtime_only",
      key: blockLabel,
      message: `${gates.join(", ")} now restricts runtime behavior only and no longer removes tool registrations; list tools in disabled_tools to unregister them`,
    });
  }

  if (explicitList !== undefined) {
    if (explicitSurface || generated.size > 0) {
      out.warnings.push({
        code: "superseded_legacy_config",
        key: blockLabel,
        message:
          "disabled_tools is set explicitly, so legacy tool_surface/hoist_builtin_tools/enabled do not change registration",
        once: true,
      });
    }
    return;
  }

  let list: string[] | undefined;
  if (userBase) {
    if (explicitSurface) list = sortedUnique(generated);
    else if (generated.size > 0) list = sortedUnique([...generated, ...DEFAULT_DISABLED_TOOLS]);
  } else if (generated.size > 0) {
    list = sortedUnique(generated);
  }
  if (list !== undefined) map.disabled_tools = list;
}

/**
 * What a retired `enabled: false` no longer does. The translation only hides
 * tools; it never stops indexing, so a user who relied on it to keep AFT out
 * of a repository must also switch the indexes off.
 */
export const RETIRED_ENABLED_FALSE_INDEXES_NOTE =
  "enabled: false no longer turns AFT off: it is translated to disabling every tool, but the trigram, semantic and callgraph indexes still build. To keep AFT from indexing this repository, also set indexes.trigram, indexes.semantic and indexes.callgraph to false.";

/**
 * The once-per-identity notice for a project config file whose retired keys
 * were translated in memory. The file itself is never written: it is shared
 * through its repository. Mirrors `project_retired_keys_notice` in
 * `crates/aft/src/feature_config.rs`.
 */
export function legacyConfigNoticeMessage(
  configPath: string,
  translation: Pick<DocumentTranslation, "retiredEnabledFalse" | "retiredKeys">,
): string {
  const base = `${configPath} uses retired keys (${translation.retiredKeys.join(", ")}); AFT applied their current equivalents, with the same limits a project config has for those keys. Run \`npx @cortexkit/aft doctor --fix\` to update the file.`;
  return translation.retiredEnabledFalse ? `${base} ${RETIRED_ENABLED_FALSE_INDEXES_NOTE}` : base;
}

/**
 * Translate every block (base plus each `harnesses.<id>`) in place. Retired
 * keys are never refused: their current equivalents then pass through the
 * normal user/project trust rules, exactly as if the file had spelled them
 * out.
 *
 * Only the user file's base block receives the absent-base default
 * (`aft_move`/`aft_delete`) when its legacy keys generate disables: that
 * default applies once, at user-base resolution. A project file's base block
 * contributes only the names its own legacy keys imply, so a legacy key in a
 * repository can never re-disable tools the user enabled.
 */
export function translateConfigDocument(
  map: JsonRecord,
  tier: "user" | "project",
): DocumentTranslation {
  const out: DocumentTranslation = {
    warnings: [],
    legacyInput: false,
    retiredKeys: [],
    retiredEnabledFalse: false,
  };
  translateBlock(map, tier === "user", "base", out);
  if (isRecord(map.harnesses)) {
    for (const [name, block] of Object.entries(map.harnesses)) {
      if (isRecord(block)) translateBlock(block, false, `harnesses.${name}`, out);
    }
  }
  out.retiredKeys = sortedUnique(out.retiredKeys);
  return out;
}

/** Distinct unknown names in a resolved disabled list, sorted. */
export function unknownDisabledTools(disabled: readonly string[]): string[] {
  return sortedUnique(disabled.filter((name) => !isKnownTool(name)));
}

/**
 * Validate a resolved configuration object. Missing fields are never replaced
 * by defaults here; missing containers are reported instead of their children
 * and errors are sorted by path.
 */
export function validateResolvedConfig(value: unknown): string[] {
  if (!isRecord(value)) return ["invalid_resolved_config:type:$"];
  const errors: string[] = [];
  const disabled = value.disabled_tools;
  if (disabled === undefined) errors.push("invalid_resolved_config:missing:disabled_tools");
  else if (!Array.isArray(disabled) || !disabled.every((entry) => typeof entry === "string")) {
    errors.push("invalid_resolved_config:type:disabled_tools");
  }
  const indexes = value.indexes;
  if (indexes === undefined) errors.push("invalid_resolved_config:missing:indexes");
  else if (!isRecord(indexes)) errors.push("invalid_resolved_config:type:indexes");
  else {
    for (const leaf of ["callgraph", "semantic", "trigram"]) {
      if (indexes[leaf] === undefined)
        errors.push(`invalid_resolved_config:missing:indexes.${leaf}`);
      else if (typeof indexes[leaf] !== "boolean") {
        errors.push(`invalid_resolved_config:type:indexes.${leaf}`);
      }
    }
  }
  const path = (error: string) => error.slice(error.lastIndexOf(":") + 1);
  return errors.sort((left, right) =>
    path(left) < path(right) ? -1 : path(left) > path(right) ? 1 : 0,
  );
}

const ABSENT = { absent: true } as const;

function projectBlock(block: JsonRecord): JsonRecord {
  const legacyPaths: JsonRecord = {};
  for (const [path] of RETIRED_PATHS) {
    const dot = path.indexOf(".");
    if (dot < 0) {
      if (Object.hasOwn(block, path)) legacyPaths[path] = block[path];
    } else {
      const container = block[path.slice(0, dot)];
      const leaf = path.slice(dot + 1);
      if (isRecord(container) && Object.hasOwn(container, leaf))
        legacyPaths[path] = container[leaf];
    }
  }
  const gates: JsonRecord = {};
  if (isRecord(block.backup) && Object.hasOwn(block.backup, "enabled")) {
    gates["backup.enabled"] = block.backup.enabled;
  }
  if (isRecord(block.inspect) && Object.hasOwn(block.inspect, "enabled")) {
    gates["inspect.enabled"] = block.inspect.enabled;
  }
  if (typeof block.bash === "boolean") gates.bash = block.bash;
  else if (isRecord(block.bash) && Object.hasOwn(block.bash, "enabled")) {
    gates["bash.enabled"] = block.bash.enabled;
  }
  let aliases: string[] = [];
  let disabled: unknown = ABSENT;
  if (Array.isArray(block.disabled_tools)) {
    const names = block.disabled_tools.filter(
      (entry): entry is string => typeof entry === "string",
    );
    aliases = sortedUnique(names.filter((name) => LEGACY_TOOL_ALIASES[name] !== undefined));
    disabled = sortedUnique(names);
  } else if (Object.hasOwn(block, "disabled_tools")) {
    disabled = block.disabled_tools;
  }
  const github: JsonRecord = {};
  for (const leaf of ["read", "shim", "write"]) {
    github[leaf] =
      isRecord(block.github) && Object.hasOwn(block.github, leaf) ? block.github[leaf] : ABSENT;
  }
  const indexes: JsonRecord = {};
  for (const [leaf, legacy, experimental] of INDEX_INPUTS) {
    const inputs: JsonRecord = {
      canonical:
        isRecord(block.indexes) && Object.hasOwn(block.indexes, leaf)
          ? block.indexes[leaf]
          : ABSENT,
      [legacy]: Object.hasOwn(block, legacy) ? block[legacy] : ABSENT,
    };
    if (experimental !== undefined) {
      inputs[experimental] = Object.hasOwn(block, experimental) ? block[experimental] : ABSENT;
    }
    indexes[leaf] = inputs;
  }
  return {
    legacy_paths: legacyPaths,
    runtime_gates: gates,
    legacy_disabled_entries: aliases,
    disabled_tools: disabled,
    github,
    indexes,
  };
}

/**
 * Build the `notice_projection_v1` object for one raw (untranslated) config
 * document. Only translation-relevant inputs are projected, so comments,
 * formatting, key order and unrelated settings never change it.
 */
export function noticeProjection(raw: JsonRecord | undefined): JsonRecord {
  const blocks: JsonRecord = {};
  if (raw !== undefined) {
    blocks.base = projectBlock(raw);
    if (isRecord(raw.harnesses)) {
      for (const [name, block] of Object.entries(raw.harnesses)) {
        if (isRecord(block)) blocks[`harnesses.${name}`] = projectBlock(block);
      }
    }
  }
  return { projection: "notice_projection_v1", file_absent: raw === undefined, blocks };
}

/** Compact JSON with object keys sorted at every level. */
export function canonicalJson(value: unknown): string {
  if (Array.isArray(value)) return `[${value.map(canonicalJson).join(",")}]`;
  if (isRecord(value)) {
    return `{${Object.keys(value)
      .sort()
      .map((key) => `${JSON.stringify(key)}:${canonicalJson(value[key])}`)
      .join(",")}}`;
  }
  return JSON.stringify(value);
}

/** SHA-256 hex digest of a canonical projection. */
export function noticeDigest(projection: unknown): string {
  return createHash("sha256").update(canonicalJson(projection), "utf8").digest("hex");
}

/**
 * One-time notice shown when the semantic index is on only because indexes now
 * default on. The text follows the feature-config spec, except that commands
 * are spelled as `npx @cortexkit/aft …`: users reach this notice through the
 * plugin, with no `aft` command on their PATH.
 */
export const SEMANTIC_COST_NOTICE =
  "AFT indexes now default on; the local semantic backend may download an ONNX runtime and model and use CPU. Run npx @cortexkit/aft setup to change indexes.semantic.";

/**
 * Identity of the semantic cost notice. It is a constant rather than a digest
 * of the config file, so the notice is delivered once per user config path and
 * unrelated config edits never bring it back.
 */
export const SEMANTIC_COST_NOTICE_DIGEST = noticeDigest({ notice: "semantic_default_cost_v1" });

/**
 * True when a raw config document supplies any semantic index input (the
 * canonical `indexes.semantic` leaf or a legacy/experimental alias) in its base
 * block or in the block of `activeHarness`. A supplied value, true or false,
 * means the user chose the setting, so the default-on cost notice does not
 * apply.
 */
export function suppliesSemanticIndexInput(
  raw: JsonRecord | undefined,
  activeHarness: string,
): boolean {
  if (raw === undefined) return false;
  const [leaf, immediate, experimental] = INDEX_INPUTS.find(([name]) => name === "semantic") ?? [
    "semantic",
    "semantic_search",
    "experimental_semantic_search",
  ];
  const blockSupplies = (block: unknown): boolean => {
    if (!isRecord(block)) return false;
    if (isRecord(block.indexes) && block.indexes[leaf] !== undefined) return true;
    if (block[immediate] !== undefined) return true;
    return experimental !== undefined && block[experimental] !== undefined;
  };
  const harnessBlock = isRecord(raw.harnesses) ? raw.harnesses[activeHarness] : undefined;
  return blockSupplies(raw) || blockSupplies(harnessBlock);
}

/**
 * The semantic cost notice for one load, or null when it does not apply. It is
 * a migration notice: it applies only to an existing config file that relied
 * on the old default, where the semantic index is now effectively on because
 * of the new default. That means a config file was loaded, no loaded tier
 * supplied a semantic input, and no non-local embedding backend is configured
 * (a remote backend downloads nothing). A fresh install has no file that ever
 * relied on the old default, so it gets no notice.
 */
export function semanticCostNotice(options: {
  userConfigPath: string;
  /** True when at least one config tier (user or project) had a file on disk. */
  configFileLoaded: boolean;
  semanticEffective: boolean;
  semanticInputSupplied: boolean;
  semanticBackend: string | undefined;
}): { configPath: string; digest: string; message: string } | null {
  if (!options.configFileLoaded) return null;
  if (!options.semanticEffective || options.semanticInputSupplied) return null;
  if (options.semanticBackend !== undefined && options.semanticBackend !== "fastembed") return null;
  return {
    configPath: options.userConfigPath,
    digest: SEMANTIC_COST_NOTICE_DIGEST,
    message: SEMANTIC_COST_NOTICE,
  };
}

/** Raw `indexes` block as it appears in one config tier. */
export interface RawIndexesConfig {
  trigram?: boolean;
  semantic?: boolean;
  callgraph?: boolean;
}

/** Resolved index switches (all default on). */
export interface ResolvedIndexesConfig {
  trigram: boolean;
  semantic: boolean;
  callgraph: boolean;
}

export const DEFAULT_INDEXES: ResolvedIndexesConfig = {
  trigram: true,
  semantic: true,
  callgraph: true,
};

/**
 * Whole-load rejection: the candidate config must not be used at all. Raised
 * when a resolved configuration lacks its registration list or an index
 * switch; retired keys are translated and never raise it.
 */
export class ConfigRejectedError extends Error {
  readonly errors: string[];
  constructor(errors: string[], source?: string) {
    super(`AFT configuration${source ? ` at ${source}` : ""} was rejected: ${errors.join(", ")}.`);
    this.name = "ConfigRejectedError";
    this.errors = errors;
  }
}

/**
 * Union two raw disabled lists while preserving presence: an explicit empty
 * list on either side yields an explicit (possibly empty) sorted result.
 */
export function unionDisabledTools(
  base: readonly string[] | undefined,
  override: readonly string[] | undefined,
): string[] | undefined {
  if (override === undefined) return base === undefined ? undefined : [...base];
  return sortedUnique([...(base ?? []), ...override]);
}

/**
 * Merge index switches. Trusted overrides (user harness block) replace each
 * supplied leaf; restricted overrides (project base or harness) can only turn
 * a leaf off.
 */
export function mergeIndexes(
  base: RawIndexesConfig | undefined,
  override: RawIndexesConfig | undefined,
  restricted: boolean,
): RawIndexesConfig | undefined {
  if (override === undefined) return base;
  const merged: RawIndexesConfig = { ...base };
  for (const leaf of ["trigram", "semantic", "callgraph"] as const) {
    const value = override[leaf];
    if (value === false) merged[leaf] = false;
    else if (value === true && !restricted) merged[leaf] = true;
  }
  return merged;
}

/** Resolve raw index switches onto their defaults. */
export function resolveIndexes(raw: RawIndexesConfig | undefined): ResolvedIndexesConfig {
  return {
    trigram: raw?.trigram ?? DEFAULT_INDEXES.trigram,
    semantic: raw?.semantic ?? DEFAULT_INDEXES.semantic,
    callgraph: raw?.callgraph ?? DEFAULT_INDEXES.callgraph,
  };
}

/**
 * Split a project disabled list into accepted names and ignored protected
 * slots (aft_safety and the seven host tools).
 */
export function partitionProjectDisables(names: readonly string[] | undefined): {
  accepted: string[] | undefined;
  ignored: string[];
} {
  if (names === undefined) return { accepted: undefined, ignored: [] };
  return {
    accepted: names.filter((name) => !isProjectProtectedTool(name)),
    ignored: sortedUnique(names.filter(isProjectProtectedTool)),
  };
}

/** The resolved disabled list of a finalized config; absence is a bug, not []. */
export function requireResolvedDisabledTools(config: {
  disabled_tools?: readonly string[];
}): readonly string[] {
  if (config.disabled_tools === undefined) {
    throw new ConfigRejectedError(["invalid_resolved_config:missing:disabled_tools"]);
  }
  return config.disabled_tools;
}
