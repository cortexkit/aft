/**
 * Subc manifest tool schemas: bare manifest names → JSON Schema from agent tools.
 * Shared by scripts/build-tool-schemas.ts and subc-tool-schemas-fresh.test.ts.
 */

import { BASH_RUNON_DESCRIPTION, type BridgePool } from "@cortexkit/aft-bridge";
import type { ToolDefinition } from "@opencode-ai/plugin";
import { tool } from "@opencode-ai/plugin";
import { resolveBashConfig } from "./config.js";
import { astTools } from "./tools/ast.js";
import {
  bashCompanionRegistered,
  bashTimeoutDescription,
  bashToolDescription,
  createBashKillTool,
  createBashStatusTool,
  createBashTool,
} from "./tools/bash.js";
import { bashWatchDescription, createBashWatchTool } from "./tools/bash_watch.js";
import { createBashWriteTool } from "./tools/bash_write.js";
import { conflictTools } from "./tools/conflicts.js";
import { createReadTool, hoistedTools } from "./tools/hoisted.js";
import { importTools } from "./tools/imports.js";
import { inspectTools } from "./tools/inspect.js";
import { navigationTools } from "./tools/navigation.js";
import { readingTools } from "./tools/reading.js";
import { safetyTools } from "./tools/safety.js";
import { searchTools } from "./tools/search.js";
import { semanticTools } from "./tools/semantic.js";
import type { PluginContext } from "./types.js";

const z = tool.schema;

const STATUS_DESCRIPTION = "Show AFT status, index health, cache usage, and runtime details";

/**
 * Catalog wording for `bash_status`. The OpenCode description ends with "To
 * wait, use bash_watch", but the module catalog cannot offer `bash_watch`: its
 * waiting loop is implemented in the OpenCode and Pi plugins, not the module.
 */
const BASH_STATUS_CATALOG_DESCRIPTION =
  "Read-only snapshot of a background or PTY bash task's current state and output. Returns immediately and never waits; the task keeps running, and a completion reminder arrives when it exits.";

const STATUS_SCHEMA = {
  type: "object",
  properties: {},
  additionalProperties: false,
} as const;

const BARE_TOOL_ORDER = [
  "status",
  "bash",
  "powershell",
  "read",
  "write",
  "edit",
  "apply_patch",
  "grep",
  "glob",
  "search",
  "outline",
  "zoom",
  "inspect",
  "callgraph",
  "conflicts",
  "ast_search",
  "ast_replace",
  "delete",
  "move",
  "import",
  "safety",
  "bash_status",
  "bash_kill",
  "bash_write",
] as const;

export type SubcBareToolName = (typeof BARE_TOOL_ORDER)[number];

/**
 * Context used to render the subc tool manifest.
 *
 * This manifest is generated once and compiled into the binary, so unlike the
 * per-directory plugin surface it is ONE contract for every project a subc
 * consumer may bind. Config-gated arguments therefore have to be declared here
 * whenever the runtime honours them in any project, or a consumer working in a
 * project that does enable the feature could never see it. `sandbox.enabled` is
 * exactly that case: it is project-settable one-way hardening, and the subc
 * bash path reads a passed `sandbox` argument regardless of this manifest.
 */
export function makeSubcSchemaStubCtx(): PluginContext {
  return {
    pool: {
      getBridge: () =>
        ({
          send: async () => ({ success: true }),
        }) as unknown as ReturnType<BridgePool["getBridge"]>,
    } as unknown as BridgePool,
    client: { lsp: {}, find: {} } as PluginContext["client"],
    config: {
      disabled_tools: [],
      sandbox: { enabled: true },
    } as PluginContext["config"],
    storageDir: "/tmp/aft-subc-schema",
  };
}

/**
 * JSON Schema extension key marking a property that AFT's own plugins set and
 * a model never should. This marker is the single record of which properties
 * are consumer-only: the Rust manifest (`crates/aft/src/subc/manifest.rs`)
 * strips every marked property before it serves the catalog to subc
 * consumers, and the runtime still honours the value when a plugin sends it.
 */
export const CONSUMER_ONLY_MARKER = "x-aft-consumer-only";

function consumerOnly(property: Record<string, unknown>): Record<string, unknown> {
  return { ...property, [CONSUMER_ONLY_MARKER]: true };
}

/**
 * JSON Schema extension key on bash's `runon` property. The Rust manifest and
 * every catalog strip the property by default and add it back (without the
 * marker) only for a session that may run commands remotely, so it appears
 * exactly where remote runs are configured.
 */
export const RUNON_MARKER = "x-aft-runon";

function argsToJsonSchema(def: ToolDefinition): Record<string, unknown> {
  const wrapped = z.object(def.args);
  const jsonSchema = z.toJSONSchema(wrapped, { io: "input" }) as Record<string, unknown>;
  if (typeof def.description === "string" && def.description.length > 0) {
    return { ...jsonSchema, description: def.description };
  }
  return jsonSchema;
}

/**
 * Build the bare-name → JSON Schema map for subc build_manifest.
 */
export function buildSubcToolSchemas(): Record<SubcBareToolName, Record<string, unknown>> {
  const ctx = makeSubcSchemaStubCtx();
  const bash = createBashTool(ctx);
  const read = createReadTool(ctx);
  const hoisted = hoistedTools(ctx);
  const {
    write,
    edit,
    apply_patch: applyPatch,
    aft_delete: deleteTool,
    aft_move: moveTool,
  } = hoisted;
  if (!write || !edit || !applyPatch || !deleteTool || !moveTool) {
    throw new Error("hoistedTools must expose write, edit, apply_patch, aft_delete, and aft_move");
  }
  const grepTools = searchTools(ctx);
  const grepTool = grepTools.grep;
  const globTool = grepTools.glob;
  if (!grepTool || !globTool) {
    throw new Error("searchTools must expose grep and glob");
  }
  const search = semanticTools(ctx).aft_search;
  const reading = readingTools(ctx);
  const outline = reading.aft_outline;
  const zoom = reading.aft_zoom;
  const inspect = inspectTools(ctx).aft_inspect;
  const callgraph = navigationTools(ctx).aft_callgraph;
  const conflicts = conflictTools(ctx).aft_conflicts;
  const ast = astTools(ctx);
  const astSearch = ast.ast_grep_search ?? ast.aft_ast_search;
  const astReplace = ast.ast_grep_replace ?? ast.aft_ast_replace;
  const importTool = importTools(ctx).aft_import;
  const safety = safetyTools(ctx).aft_safety;
  if (
    !search ||
    !outline ||
    !zoom ||
    !inspect ||
    !callgraph ||
    !conflicts ||
    !astSearch ||
    !astReplace ||
    !importTool ||
    !safety
  ) {
    throw new Error("all subc manifest tools must expose an agent tool schema");
  }

  const bashSchema = argsToJsonSchema(bash);
  // The catalog has no `bash_watch` (the waiting loop lives in the OpenCode and
  // Pi plugins, not in the module), so the catalog description must not steer
  // the model to it. Only the description differs; the bash arguments stay the
  // OpenCode tool's own.
  const bashConfig = resolveBashConfig(ctx.config);
  bashSchema.description = bashToolDescription(
    false,
    bashConfig.compress,
    bashConfig.background,
    true,
    true,
    false,
  );
  const bashProperties = (bashSchema.properties ??= {}) as Record<string, unknown>;
  bashProperties.foreground_orchestrate = consumerOnly({
    type: "boolean",
    description: "Consumer-set flag enabling server-side foreground orchestration.",
  });
  bashProperties.block_to_completion = consumerOnly({
    type: "boolean",
    description:
      "Consumer-set flag forcing foreground bash to wait until terminal instead of promoting.",
  });
  bashProperties.shell = consumerOnly({
    type: "string",
    enum: ["powershell"],
    description: "Consumer-set shell selector for Pi's PowerShell tool.",
  });
  // The powershell entry is always generated; whether it is advertised is
  // decided per host when the Rust manifest is served (only where pwsh runs).
  // It never carries `runon`: the remote runner runs bash, not PowerShell.
  const powershellSchema = {
    ...bashSchema,
    properties: { ...bashProperties },
    description:
      "Execute PowerShell commands through AFT's bash task family with UTF-8 output and conservative per-command approval.",
  };
  bashProperties.runon = {
    type: "string",
    description: BASH_RUNON_DESCRIPTION,
    [RUNON_MARKER]: true,
  };

  return {
    status: { ...STATUS_SCHEMA, description: STATUS_DESCRIPTION },
    bash: bashSchema,
    powershell: powershellSchema,
    read: argsToJsonSchema(read),
    write: argsToJsonSchema(write),
    edit: argsToJsonSchema(edit),
    apply_patch: argsToJsonSchema(applyPatch),
    grep: argsToJsonSchema(grepTool),
    glob: argsToJsonSchema(globTool),
    search: argsToJsonSchema(search),
    outline: argsToJsonSchema(outline),
    zoom: argsToJsonSchema(zoom),
    inspect: argsToJsonSchema(inspect),
    callgraph: argsToJsonSchema(callgraph),
    conflicts: argsToJsonSchema(conflicts),
    ast_search: argsToJsonSchema(astSearch),
    ast_replace: argsToJsonSchema(astReplace),
    delete: argsToJsonSchema(deleteTool),
    move: argsToJsonSchema(moveTool),
    import: argsToJsonSchema(importTool),
    safety: argsToJsonSchema(safety),
    // Companions for the task ids `bash` hands back: its reply text tells the
    // model to call them, so a consumer that builds its surface from the catalog
    // must be offered them. Arguments match the OpenCode tools exactly.
    bash_status: {
      ...argsToJsonSchema(createBashStatusTool(ctx)),
      description: BASH_STATUS_CATALOG_DESCRIPTION,
    },
    bash_kill: argsToJsonSchema(createBashKillTool(ctx)),
    bash_write: argsToJsonSchema(createBashWriteTool(ctx)),
  };
}

/**
 * Deterministic JSON bytes: top-level keys sorted, 2-space indent, trailing newline.
 */
export function serializeSubcToolSchemas(schemas: Record<string, Record<string, unknown>>): string {
  const sorted: Record<string, Record<string, unknown>> = {};
  for (const key of BARE_TOOL_ORDER) {
    if (schemas[key] !== undefined) {
      sorted[key] = schemas[key];
    }
  }
  for (const key of Object.keys(schemas).sort()) {
    if (sorted[key] === undefined) {
      sorted[key] = schemas[key];
    }
  }
  return `${JSON.stringify(sorted, null, 2)}\n`;
}

export function buildSubcToolSchemasJson(): string {
  return serializeSubcToolSchemas(buildSubcToolSchemas());
}

export const SUBC_BARE_TOOL_NAMES: readonly SubcBareToolName[] = BARE_TOOL_ORDER;

/**
 * Tool schemas the module catalog's `worker` preset serves in place of the
 * base ones, plus the tools only that preset serves. The `head` preset is the
 * base artifact unchanged, and the `reader` preset only narrows the tool list,
 * so neither needs entries here.
 *
 * A worker is a delegated session: once its turn ends it has delivered its
 * result, and nothing (no completion reminder, no async notification) wakes
 * it. Every text here is therefore the worker wording of the same builders
 * the OpenCode tools use, so the strings are never forked: `bash` and
 * `powershell` keep their arguments but describe promotion and waiting for a
 * worker, `bash_status` is the OpenCode tool's own (which points at
 * `bash_watch`), and `bash_watch` is the OpenCode tool's schema with its
 * worker description.
 */
export function buildSubcToolPresets(): Record<string, Record<string, Record<string, unknown>>> {
  const ctx = makeSubcSchemaStubCtx();
  const base = buildSubcToolSchemas();
  const bashConfig = resolveBashConfig(ctx.config);
  const statusRegistered = bashCompanionRegistered(ctx.config, "bash_status");
  const workerTimeout = bashTimeoutDescription(bashConfig.background, "worker");
  const withWorkerTimeout = (schema: Record<string, unknown>): Record<string, unknown> => {
    const clone = structuredClone(schema);
    const properties = clone.properties as Record<string, Record<string, unknown>>;
    properties.timeout = { ...properties.timeout, description: workerTimeout };
    return clone;
  };
  const bash = withWorkerTimeout(base.bash);
  bash.description = bashToolDescription(
    false,
    bashConfig.compress,
    bashConfig.background,
    true,
    true,
    true,
    { role: "worker" },
  );
  return {
    worker: {
      bash,
      powershell: withWorkerTimeout(base.powershell),
      bash_status: argsToJsonSchema(createBashStatusTool(ctx)),
      bash_watch: workerBashWatchSchema(
        argsToJsonSchema(createBashWatchTool(ctx)),
        bashWatchDescription("worker", statusRegistered),
      ),
    },
  };
}

/**
 * The worker preset's `bash_watch`: the OpenCode tool's arguments with the
 * worker description. The module serves this tool itself and always waits
 * synchronously, because a catalog consumer has no channel that would deliver
 * an async watch's notification; the two async-only arguments say so.
 */
function workerBashWatchSchema(
  schema: Record<string, unknown>,
  description: string,
): Record<string, unknown> {
  const clone = structuredClone(schema);
  const properties = clone.properties as Record<string, Record<string, unknown>>;
  properties.background = {
    ...properties.background,
    description:
      "Accepted for compatibility. This watch always waits: a delegated session gets no async notification, so background: true waits up to the worker wait limit like a watch without a timeout.",
  };
  properties.once = {
    ...properties.once,
    description: "Accepted for compatibility; only an async watch reads it.",
  };
  return { ...clone, description };
}

/** Deterministic JSON bytes for the preset artifact, keys sorted at every preset level. */
export function buildSubcToolPresetsJson(): string {
  const presets = buildSubcToolPresets();
  const sorted: Record<string, Record<string, Record<string, unknown>>> = {};
  for (const preset of Object.keys(presets).sort()) {
    const tools: Record<string, Record<string, unknown>> = {};
    for (const name of Object.keys(presets[preset]).sort()) tools[name] = presets[preset][name];
    sorted[preset] = tools;
  }
  return `${JSON.stringify(sorted, null, 2)}\n`;
}
