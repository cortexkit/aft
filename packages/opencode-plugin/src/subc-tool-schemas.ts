/**
 * Subc manifest tool schemas: bare manifest names → JSON Schema from agent tools.
 * Shared by scripts/build-tool-schemas.ts and subc-tool-schemas-fresh.test.ts.
 */

import type { BridgePool } from "@cortexkit/aft-bridge";
import type { ToolDefinition } from "@opencode-ai/plugin";
import { tool } from "@opencode-ai/plugin";

import { astTools } from "./tools/ast.js";
import { createBashTool } from "./tools/bash.js";
import { conflictTools } from "./tools/conflicts.js";
import { gatherTools } from "./tools/gather.js";
import { createReadTool, hoistedTools } from "./tools/hoisted.js";
import { importTools } from "./tools/imports.js";
import { inspectTools } from "./tools/inspect.js";
import { navigationTools } from "./tools/navigation.js";
import { readingTools } from "./tools/reading.js";
import { refactoringTools } from "./tools/refactoring.js";
import { safetyTools } from "./tools/safety.js";
import { searchTools } from "./tools/search.js";
import { semanticTools } from "./tools/semantic.js";
import type { PluginContext } from "./types.js";

const z = tool.schema;

const STATUS_DESCRIPTION = "Show AFT status, index health, cache usage, and runtime details";

const STATUS_SCHEMA = {
  type: "object",
  properties: {},
  additionalProperties: false,
} as const;

const BARE_TOOL_ORDER = [
  "status",
  "bash",
  "read",
  "write",
  "edit",
  "apply_patch",
  "grep",
  "glob",
  "search",
  "gather",
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
  "refactor",
  "safety",
] as const;

export type SubcBareToolName = (typeof BARE_TOOL_ORDER)[number];

export function makeSubcSchemaStubCtx(): PluginContext {
  return {
    pool: {
      getBridge: () =>
        ({
          send: async () => ({ success: true }),
        }) as unknown as ReturnType<BridgePool["getBridge"]>,
    } as unknown as BridgePool,
    client: { lsp: {}, find: {} } as PluginContext["client"],
    config: { hoist_builtin_tools: true } as PluginContext["config"],
    storageDir: "/tmp/aft-subc-schema",
  };
}

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
  const grepTool = grepTools.grep ?? grepTools.aft_grep;
  const globTool = grepTools.glob ?? grepTools.aft_glob;
  if (!grepTool || !globTool) {
    throw new Error("searchTools must expose grep/glob or aft_grep/aft_glob");
  }
  const search = semanticTools(ctx).aft_search;
  const gather = gatherTools(ctx).aft_gather_context;
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
  const refactor = refactoringTools(ctx).aft_refactor;
  const safety = safetyTools(ctx).aft_safety;
  if (
    !search ||
    !gather ||
    !outline ||
    !zoom ||
    !inspect ||
    !callgraph ||
    !conflicts ||
    !astSearch ||
    !astReplace ||
    !importTool ||
    !refactor ||
    !safety
  ) {
    throw new Error("all subc manifest tools must expose an agent tool schema");
  }

  const bashSchema = argsToJsonSchema(bash);
  const bashProperties = (bashSchema.properties ??= {}) as Record<string, unknown>;
  bashProperties.foreground_orchestrate = {
    type: "boolean",
    description: "Consumer-set flag enabling server-side foreground orchestration.",
  };
  bashProperties.block_to_completion = {
    type: "boolean",
    description:
      "Consumer-set flag forcing foreground bash to wait until terminal instead of promoting.",
  };

  return {
    status: { ...STATUS_SCHEMA, description: STATUS_DESCRIPTION },
    bash: bashSchema,
    read: argsToJsonSchema(read),
    write: argsToJsonSchema(write),
    edit: argsToJsonSchema(edit),
    apply_patch: argsToJsonSchema(applyPatch),
    grep: argsToJsonSchema(grepTool),
    glob: argsToJsonSchema(globTool),
    search: argsToJsonSchema(search),
    gather: argsToJsonSchema(gather),
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
    refactor: argsToJsonSchema(refactor),
    safety: argsToJsonSchema(safety),
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
