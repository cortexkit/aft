import type { ToolDefinition } from "@opencode-ai/plugin";

import type { AftConfig } from "./config.js";
import { normalizeToolMap } from "./normalize-schemas.js";
import { astTools } from "./tools/ast.js";
import { conflictTools } from "./tools/conflicts.js";
import { gatherTools } from "./tools/gather.js";
import { aftPrefixedTools, hoistedTools } from "./tools/hoisted.js";
import { importTools } from "./tools/imports.js";
import { inspectToolSurfaceEnabled, inspectTools } from "./tools/inspect.js";
import { navigationTools } from "./tools/navigation.js";
import { readingTools } from "./tools/reading.js";
import { refactoringTools } from "./tools/refactoring.js";
import { safetyTools } from "./tools/safety.js";
import { searchTools } from "./tools/search.js";
import { semanticTools } from "./tools/semantic.js";
import type { PluginContext } from "./types.js";

const ALL_ONLY_TOOLS = [
  "aft_callgraph",
  "aft_gather_context",
  "aft_delete",
  "aft_move",
  "aft_refactor",
] as const;

/** Returns true when bare `edit` remains available after surface, hoisting, and disable filters. */
export function openCodeEditSlotSurvives(config: AftConfig): boolean {
  return (
    (config.tool_surface ?? "recommended") !== "minimal" &&
    config.hoist_builtin_tools !== false &&
    !(config.disabled_tools ?? []).includes("edit")
  );
}

/** Select the hashline schema only when the host's final edit slot can survive. */
export function openCodeHashlineEffective(config: AftConfig): boolean {
  return config.edit_mode === "hashline" && openCodeEditSlotSurvives(config);
}

/** Return the process-state flag Rust uses to select the same edit schema arm. */
export function openCodeHashlineEditRegistered(
  config: AftConfig,
  registeredTools: ReadonlySet<string>,
): boolean {
  return openCodeHashlineEffective(config) && registeredTools.has("edit");
}

/**
 * Build the exact OpenCode registration map without starting a bridge.
 *
 * Production calls this after startup has prepared the transport context. Keeping
 * the selection in one function makes the checked profile tests exercise the same
 * registration path rather than a second test-only inventory implementation.
 */
export function buildOpenCodeToolMap(
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
      ...refactoringTools(ctx),
      ...(surface !== "minimal" && conflictTools(ctx)),
      ...gatherTools(ctx),
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
