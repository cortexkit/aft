import { ADAPTER_UNIMPLEMENTED_TOOLS, CANONICAL_TOOLS } from "@cortexkit/aft-bridge";
import type { ExtensionAPI, ToolDefinition } from "@earendil-works/pi-coding-agent";
import type { TSchema } from "typebox";

import type { AftConfig } from "./config.js";
import { resolveBashConfig, resolvedDisabledTools } from "./config.js";
import { detectPiHarness, type PiHarness } from "./harness.js";
import { prepareToolDefinitionForRegistration } from "./tools/_shared.js";
import { registerAstTools } from "./tools/ast.js";
import { registerBashCompanionTools, registerBashTool } from "./tools/bash.js";
import { registerConflictsTool } from "./tools/conflicts.js";
import { registerFsTools } from "./tools/fs.js";
import { registerHoistedTools } from "./tools/hoisted.js";
import { registerImportTools } from "./tools/imports.js";
import { registerInspectTool } from "./tools/inspect.js";
import { registerNavigateTool } from "./tools/navigate.js";
import { registerReadingTools } from "./tools/reading.js";
import { registerSafetyTool } from "./tools/safety.js";
import { registerSemanticTool } from "./tools/semantic.js";
import type { PluginContext } from "./types.js";

/**
 * Registration predicates for the Pi/OMP adapter. Each flag is exactly "the
 * canonical name is not in the resolved disabled list"; nothing else (index
 * state, runtime gates) removes a registration.
 */
export interface PiToolSurface {
  hoistBash: boolean;
  hoistPowershell: boolean;
  hoistRead: boolean;
  hoistWrite: boolean;
  hoistEdit: boolean;
  hoistGrep: boolean;
  restrictToProjectRoot: boolean;
  outline: boolean;
  zoom: boolean;
  semantic: boolean;
  inspect: boolean;
  navigate: boolean;
  conflicts: boolean;
  importTool: boolean;
  safety: boolean;
  delete: boolean;
  move: boolean;
  astSearch: boolean;
  astReplace: boolean;
  bashStatus: boolean;
  bashWatch: boolean;
  bashWrite: boolean;
  bashKill: boolean;
}

/**
 * Canonical tools the Pi/OMP adapter can register: the full inventory minus
 * the tools this adapter has no implementation for (`glob`, `apply_patch`),
 * which are recorded as data in `ADAPTER_UNIMPLEMENTED_TOOLS`.
 */
export const PI_REGISTRABLE_TOOLS: readonly string[] = CANONICAL_TOOLS.filter(
  (name) => !(ADAPTER_UNIMPLEMENTED_TOOLS.pi as readonly string[]).includes(name),
);

/**
 * Pi's tool registry is unavailable while extension factories load. Older Pi
 * versions expose no registry, so this project-safe switch manually mirrors
 * Pi's default-tools setting in either case.
 */
function resolvePiPowerShellFallback(config: AftConfig): boolean {
  return resolveBashConfig(config).powershell_tool;
}

/** Return Pi's enabled built-in PowerShell state when its live registry is readable. */
export function piPowerShellEnabledFromHost(pi: ExtensionAPI): boolean | undefined {
  const api = pi as unknown as {
    getActiveTools?: () => Array<string | { name: string }>;
    getAllTools?: () => Array<{
      name: string;
      source?: string;
      sourceInfo?: { source?: string };
    }>;
  };
  if (typeof api.getActiveTools !== "function" || typeof api.getAllTools !== "function") {
    return undefined;
  }
  try {
    const tool = api.getAllTools().find((candidate) => candidate.name === "powershell");
    // An empty pre-bind registry is not evidence that PowerShell is disabled;
    // preserve the explicit fallback until Pi exposes a real built-in entry.
    const source = tool?.sourceInfo?.source ?? tool?.source;
    if (!tool || source === undefined) return undefined;
    return (
      source === "builtin" &&
      api
        .getActiveTools()
        .some((active) => (typeof active === "string" ? active : active.name) === "powershell")
    );
  } catch {
    return undefined;
  }
}

/**
 * Select the hashline schema only when both the edit and tagged-read slots survive.
 *
 * Only a tagged AFT read mints the `[path#TAG]` snapshots a hashline patch
 * addresses, so an edit slot on its own is not a usable hashline surface: the
 * host keeps serving its own untagged read and the agent has nothing to patch
 * against. Mirrors `openCodeHashlineEffective` and the core's
 * `RegistrationRequest::effective`.
 */
export function piHashlineEffective(
  config: AftConfig,
  surface: Pick<PiToolSurface, "hoistEdit" | "hoistRead">,
): boolean {
  return config.edit_mode === "hashline" && surface.hoistEdit && surface.hoistRead;
}

/**
 * Configure-time warning for a requested hashline surface that cannot be
 * effective because `read` or `edit` is disabled. Read takes precedence.
 */
export interface PiHashlineDowngradeWarning {
  code: "hashline_read_disabled" | "hashline_edit_disabled";
  message: string;
}

/**
 * Classify a requested-but-refused hashline surface for the warning channel.
 *
 * The read slot is reported first for the same reason the core reports it
 * first: a session can keep a working `edit` tool beside an untagged host read,
 * and the "I never got a hashline" symptom then points at the wrong tool.
 */
export function piHashlineDowngrade(
  config: AftConfig,
  surface: Pick<PiToolSurface, "hoistEdit" | "hoistRead">,
): PiHashlineDowngradeWarning | null {
  if (config.edit_mode !== "hashline") return null;
  if (piHashlineEffective(config, surface)) return null;
  return surface.hoistRead
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

/** Resolve the registration predicates used by Pi's production registration path. */
export function resolvePiToolSurface(config: AftConfig, pi?: ExtensionAPI): PiToolSurface {
  const disabled = new Set(resolvedDisabledTools(config));
  const ok = (name: string): boolean => !disabled.has(name);
  const powershellEnabled =
    (pi ? piPowerShellEnabledFromHost(pi) : undefined) ?? resolvePiPowerShellFallback(config);

  return {
    hoistBash: ok("bash"),
    // PowerShell is a host-dependent extra slot, not a canonical tool.
    hoistPowershell: powershellEnabled && ok("powershell"),
    hoistRead: ok("read"),
    hoistWrite: ok("write"),
    hoistEdit: ok("edit"),
    hoistGrep: ok("grep"),
    restrictToProjectRoot: config.restrict_to_project_root ?? false,
    outline: ok("aft_outline"),
    zoom: ok("aft_zoom"),
    semantic: ok("aft_search"),
    inspect: ok("aft_inspect"),
    navigate: ok("aft_callgraph"),
    conflicts: ok("aft_conflicts"),
    importTool: ok("aft_import"),
    safety: ok("aft_safety"),
    delete: ok("aft_delete"),
    move: ok("aft_move"),
    astSearch: ok("ast_grep_search"),
    astReplace: ok("ast_grep_replace"),
    bashStatus: ok("bash_status"),
    bashWatch: ok("bash_watch"),
    bashWrite: ok("bash_write"),
    bashKill: ok("bash_kill"),
  };
}

const FUNNEL_BOUND = Symbol.for("aft.pi.registration_funnel_bound");

/**
 * Wrap an ExtensionAPI instance so every `pi.registerTool` call flows through the
 * shared registration funnel with harness-aware loadMode and guidance folding.
 */
export function bindToolRegistrationFunnel(
  pi: ExtensionAPI,
  ctx: PluginContext,
  harness?: PiHarness,
): ExtensionAPI {
  if ((pi as unknown as Record<string | symbol, unknown>)[FUNNEL_BOUND]) {
    return pi;
  }

  const effectiveHarness = harness ?? detectPiHarness(pi);
  const presentation = ctx.config.pi?.tool_presentation ?? "top_level";
  const originalRegisterTool = pi.registerTool.bind(pi);

  const wrappedRegisterTool = <
    TParams extends TSchema = TSchema,
    TDetails = unknown,
    TState = unknown,
  >(
    tool: ToolDefinition<TParams, TDetails, TState>,
  ): void => {
    const prepared = prepareToolDefinitionForRegistration(tool, effectiveHarness, presentation);
    originalRegisterTool(prepared as ToolDefinition<TParams, TDetails, TState>);
  };

  return new Proxy(pi, {
    get(target, prop, receiver) {
      if (prop === FUNNEL_BOUND) return true;
      if (prop === "registerTool") return wrappedRegisterTool;
      return Reflect.get(target, prop, receiver);
    },
  });
}

/**
 * Invoke every Pi tool registration branch for the resolved production surface.
 * Commands, prompt hints, and lifecycle hooks intentionally remain outside this
 * function because they are not entries in the agent-facing tool registry.
 */
export function registerPiToolSurface(
  pi: ExtensionAPI,
  ctx: PluginContext,
  surface: PiToolSurface,
  harness?: PiHarness,
): void {
  const boundPi = bindToolRegistrationFunnel(pi, ctx, harness);
  // The bash runtime gate (`bash.enabled`) never removes a registration; the
  // engine answers `bash_disabled` when it is off.
  if (surface.hoistBash) registerBashTool(boundPi, ctx, surface.semantic, "bash", false);
  if (surface.hoistPowershell) {
    registerBashTool(boundPi, ctx, surface.semantic, "powershell", false, "powershell");
  }
  // Companions are independent registrations: disabling `bash` leaves them.
  registerBashCompanionTools(boundPi, ctx, surface);
  registerHoistedTools(boundPi, ctx, surface);

  if (surface.outline || surface.zoom) registerReadingTools(boundPi, ctx, surface);
  if (surface.semantic) registerSemanticTool(boundPi, ctx);
  if (surface.inspect) registerInspectTool(boundPi, ctx);
  if (surface.navigate) registerNavigateTool(boundPi, ctx);
  if (surface.conflicts) registerConflictsTool(boundPi, ctx);
  if (surface.importTool) registerImportTools(boundPi, ctx);
  if (surface.safety) registerSafetyTool(boundPi, ctx);
  if (surface.astSearch || surface.astReplace) registerAstTools(boundPi, ctx, surface);
  if (surface.delete || surface.move) registerFsTools(boundPi, ctx, surface);
}
