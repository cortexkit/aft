import { unknownDisabledTools } from "@cortexkit/aft-bridge";
import type { ToolDefinition } from "@opencode-ai/plugin";

import { type AftConfig, resolvedDisabledTools } from "./config.js";
import { normalizeToolMap } from "./normalize-schemas.js";
import { astTools } from "./tools/ast.js";
import { conflictTools } from "./tools/conflicts.js";
import {
  projectV2Tool,
  type V2Location,
  type V2ProviderTool,
  type V2ToolConsumers,
} from "./tools/definitions/v2.js";
import { hoistedTools } from "./tools/hoisted.js";
import { importTools } from "./tools/imports.js";
import { inspectTools } from "./tools/inspect.js";
import { navigationTools } from "./tools/navigation.js";
import { readingTools } from "./tools/reading.js";
import { safetyTools } from "./tools/safety.js";
import { searchTools } from "./tools/search.js";
import { semanticTools } from "./tools/semantic.js";
import type { PluginContext } from "./types.js";

/**
 * Host tool names AFT removes before adding its own.
 *
 * `bash` is deliberately absent. Two things were observed on a real OpenCode 2
 * host rather than reasoned about: its own shell surface registers under the
 * name `shell`, so nothing the host owns is displaced by AFT taking `bash`;
 * and when a second plugin does claim `bash` first, the registry keeps the
 * later registration, because adding a tool overwrites any entry already under
 * that name. AFT's `bash` is the one the host holds either way, so a preceding
 * remove would change nothing. The load matrix keeps that observation as a row.
 */
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

/** Returns true when `edit` is registered, i.e. not in the resolved disabled list. */
export function openCodeEditSlotSurvives(config: AftConfig): boolean {
  return !resolvedDisabledTools(config).includes("edit");
}

/**
 * Returns true when `read` is registered, i.e. not in the resolved disabled list.
 *
 * Only a tagged AFT read mints the `[path#TAG]` snapshots a hashline patch
 * addresses. With the AFT read registration removed, OpenCode keeps serving its
 * own untagged `read`, so the agent can inspect a file and still have no tag to
 * patch with.
 */
export function openCodeReadSlotSurvives(config: AftConfig): boolean {
  return !resolvedDisabledTools(config).includes("read");
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

/**
 * Configure-time warning for a requested hashline surface that cannot be
 * effective because `read` or `edit` is disabled. Read takes precedence.
 */
export interface HashlineDowngradeWarning {
  code: "hashline_read_disabled" | "hashline_edit_disabled";
  message: string;
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
  return readSurvives
    ? {
        code: "hashline_edit_disabled",
        message:
          'edit_mode "hashline" is not in effect because "edit" is in disabled_tools; the registered read tool keeps its ordinary behavior.',
      }
    : {
        code: "hashline_read_disabled",
        message:
          'edit_mode "hashline" is not in effect because "read" is in disabled_tools (hashline edits need tagged reads); registered tools keep their ordinary behavior.',
      };
}

/**
 * Build the exact OpenCode registration map without starting a bridge.
 *
 * A tool is registered exactly when its canonical name is absent from the
 * resolved `disabled_tools`. Index state, backends and runtime gates (bash,
 * backup, inspect) never remove a registration; those tools report their
 * runtime state when called. Production calls this after startup has prepared
 * the transport context, and the registration tests call the same function.
 *
 * `onUnknownDisabled` receives, once per call, the sorted distinct disabled
 * names that are not in the canonical tool inventory.
 */
export function buildAftToolDefinitions(
  ctx: PluginContext,
  config: AftConfig,
  onUnknownDisabled?: (names: readonly string[]) => void,
): Record<string, ToolDefinition> {
  const disabled = resolvedDisabledTools(config);
  const allTools = normalizeToolMap(
    {
      ...hoistedTools(ctx),
      ...readingTools(ctx),
      ...safetyTools(ctx),
      ...importTools(ctx),
      ...navigationTools(ctx),
      ...astTools(ctx),
      ...semanticTools(ctx),
      ...inspectTools(ctx),
      ...searchTools(ctx),
      ...conflictTools(ctx),
    },
    { hashlineEffective: ctx.hashlineEffective },
  );

  for (const name of disabled) delete allTools[name];
  const unknown = unknownDisabledTools(disabled);
  if (unknown.length > 0) onUnknownDisabled?.(unknown);

  return allTools;
}

/** Backward-compatible V1 name for the shared definition inventory. */
export function buildOpenCodeToolMap(
  ctx: PluginContext,
  config: AftConfig,
  onUnknownDisabled?: (names: readonly string[]) => void,
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
