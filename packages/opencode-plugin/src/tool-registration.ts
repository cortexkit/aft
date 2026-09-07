import type { ToolDefinition } from "@opencode-ai/plugin";

import type { AftConfig } from "./config.js";
import { normalizeToolMap } from "./normalize-schemas.js";
import { astTools } from "./tools/ast.js";
import { conflictTools } from "./tools/conflicts.js";
import {
  projectV2Tool,
  type V2Location,
  type V2ProviderTool,
  type V2ToolConsumers,
} from "./tools/definitions/v2.js";
import { aftPrefixedTools, hoistedTools } from "./tools/hoisted.js";
import { importTools } from "./tools/imports.js";
import { inspectToolSurfaceEnabled, inspectTools } from "./tools/inspect.js";
import { navigationTools } from "./tools/navigation.js";
import { readingTools } from "./tools/reading.js";
import { safetyTools } from "./tools/safety.js";
import { searchTools } from "./tools/search.js";
import { semanticTools } from "./tools/semantic.js";
import type { PluginContext } from "./types.js";

const ALL_ONLY_TOOLS = ["aft_callgraph", "aft_delete", "aft_move"] as const;
const V2_BUILTIN_REPLACEMENTS = new Set(["read", "edit", "write", "apply_patch"]);

export interface V2ToolEditor {
  add(definition: V2ProviderTool): void;
  remove(name: string): void;
}

export interface V2ToolRegistrationContext {
  tool: {
    transform(register: (editor: V2ToolEditor) => void): unknown;
  };
}

/** Returns true when bare `edit` remains available after surface, hoisting, and disable filters. */
export function openCodeEditSlotSurvives(config: AftConfig): boolean {
  return (
    (config.tool_surface ?? "recommended") !== "minimal" &&
    config.hoist_builtin_tools !== false &&
    !(config.disabled_tools ?? []).includes("edit")
  );
}

/**
 * Returns true when bare `read` remains available after surface, hoisting, and
 * disable filters.
 *
 * Only a tagged AFT read mints the `[path#TAG]` snapshots a hashline patch
 * addresses. With the AFT read registration removed, OpenCode keeps serving its
 * own untagged `read`, so the agent can inspect a file and still have no tag to
 * patch with.
 */
export function openCodeReadSlotSurvives(config: AftConfig): boolean {
  return (
    (config.tool_surface ?? "recommended") !== "minimal" &&
    config.hoist_builtin_tools !== false &&
    !(config.disabled_tools ?? []).includes("read")
  );
}

/** Select the hashline schema only when both the edit and tagged-read slots survive. */
export function openCodeHashlineEffective(config: AftConfig): boolean {
  return (
    config.edit_mode === "hashline" &&
    openCodeEditSlotSurvives(config) &&
    openCodeReadSlotSurvives(config)
  );
}

/** Return the process-state flag Rust uses to select the same edit schema arm. */
export function openCodeHashlineEditRegistered(
  config: AftConfig,
  registeredTools: ReadonlySet<string>,
): boolean {
  return (
    openCodeHashlineEffective(config) && registeredTools.has("edit") && registeredTools.has("read")
  );
}

/** One `hashline_downgraded` warning describing why the hashline arm was refused. */
export interface HashlineDowngradeWarning {
  code: "hashline_downgraded";
  reason: "edit_not_registered" | "tagged_read_unavailable";
}

/**
 * Classify a requested-but-refused hashline surface for the warning channel.
 *
 * The read slot is reported first because it is the harder failure to diagnose:
 * a session can keep a working `edit` tool beside the host's own untagged read,
 * and the resulting "I never got a hashline" symptom points at the edit tool,
 * which is not the missing piece. Mirrors `RegistrationRequest::downgrade_warning`.
 */
export function openCodeHashlineDowngrade(
  config: AftConfig,
  registeredTools: ReadonlySet<string>,
): HashlineDowngradeWarning | null {
  if (config.edit_mode !== "hashline") return null;
  if (openCodeHashlineEditRegistered(config, registeredTools)) return null;
  const readSurvives = openCodeReadSlotSurvives(config) && registeredTools.has("read");
  return {
    code: "hashline_downgraded",
    reason: readSurvives ? "edit_not_registered" : "tagged_read_unavailable",
  };
}

/**
 * Build the exact OpenCode registration map without starting a bridge.
 *
 * Production calls this after startup has prepared the transport context. Keeping
 * the selection in one function makes the checked profile tests exercise the same
 * registration path rather than a second test-only inventory implementation.
 */
export function buildAftToolDefinitions(
  ctx: PluginContext,
  config: AftConfig,
  onUnknownDisabled?: (name: string, available: readonly string[]) => void,
): Record<string, ToolDefinition> {
  const surface = config.tool_surface ?? "recommended";
  const allTools = normalizeToolMap(
    {
      ...(surface !== "minimal" &&
        (config.hoist_builtin_tools !== false ? hoistedTools(ctx) : aftPrefixedTools(ctx))),
      ...readingTools(ctx),
      ...(config.backup?.enabled === false ? {} : safetyTools(ctx)),
      ...(surface !== "minimal" && importTools(ctx)),
      ...navigationTools(ctx),
      ...(surface !== "minimal" && astTools(ctx)),
      ...(surface !== "minimal" && config.semantic_search === true && semanticTools(ctx)),
      ...(inspectToolSurfaceEnabled(config) && inspectTools(ctx)),
      ...(surface !== "minimal" && config.search_index === true && searchTools(ctx)),
      ...(surface !== "minimal" && conflictTools(ctx)),
    },
    { hashlineEffective: ctx.hashlineEffective },
  );

  if (surface !== "all") {
    for (const name of ALL_ONLY_TOOLS) delete allTools[name];
  }

  for (const name of config.disabled_tools ?? []) {
    if (name in allTools) {
      delete allTools[name];
    } else {
      onUnknownDisabled?.(name, Object.keys(allTools));
    }
  }

  return allTools;
}

/** Backward-compatible V1 name for the shared definition inventory. */
export function buildOpenCodeToolMap(
  ctx: PluginContext,
  config: AftConfig,
  onUnknownDisabled?: (name: string, available: readonly string[]) => void,
): Record<string, ToolDefinition> {
  return buildAftToolDefinitions(ctx, config, onUnknownDisabled);
}

/**
 * Register the shared definition inventory on V2 as direct provider tools.
 *
 * The transform removes only the host tools AFT replaces, does not mutate the
 * shared V1 definitions, and never calls `update`. Provider and model data are
 * deliberately not inputs, so the same definitions produce the same registered
 * projection on every turn.
 */
export function registerAftTools(
  context: V2ToolRegistrationContext,
  location: V2Location,
  definitions: Readonly<Record<string, ToolDefinition>>,
  consumers: V2ToolConsumers = {},
): unknown {
  const projected = Object.entries(definitions).map(([name, definition]) =>
    projectV2Tool(name, definition, location, consumers),
  );

  return context.tool.transform((editor) => {
    for (const definition of projected) {
      if (V2_BUILTIN_REPLACEMENTS.has(definition.name)) editor.remove(definition.name);
      editor.add(definition);
    }
  });
}
