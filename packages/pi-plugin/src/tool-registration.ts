import type { ExtensionAPI, ToolDefinition } from "@earendil-works/pi-coding-agent";
import type { TSchema } from "typebox";

import type { AftConfig } from "./config.js";
import { resolveBashConfig } from "./config.js";
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

export interface PiToolSurface {
  /** Whether AFT replaces host-native tool names instead of registering aft_ alternatives. */
  hoistBuiltinTools: boolean;
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
}

const ALL_ONLY_TOOLS = new Set(["aft_callgraph", "aft_delete", "aft_move"]);

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

/** One `hashline_downgraded` warning describing why the hashline arm was refused. */
export interface PiHashlineDowngradeWarning {
  code: "hashline_downgraded";
  reason: "edit_not_registered" | "tagged_read_unavailable";
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
  return {
    code: "hashline_downgraded",
    reason: surface.hoistRead ? "edit_not_registered" : "tagged_read_unavailable",
  };
}

/** Resolve the feature predicates used by Pi's production registration path. */
export function resolvePiToolSurface(config: AftConfig, pi?: ExtensionAPI): PiToolSurface {
  const surface = config.tool_surface ?? "recommended";
  const disabled = new Set(config.disabled_tools ?? []);
  const hoistBuiltinTools = config.hoist_builtin_tools !== false;
  const ok = (name: string): boolean => !disabled.has(name);
  const builtinToolEnabled = (bareName: string): boolean =>
    ok(hoistBuiltinTools ? bareName : `aft_${bareName}`);
  const allOnly = (name: string): boolean => ALL_ONLY_TOOLS.has(name) && ok(name);
  const restrictToProjectRoot = config.restrict_to_project_root ?? false;
  const powershellEnabled =
    (pi ? piPowerShellEnabledFromHost(pi) : undefined) ?? resolvePiPowerShellFallback(config);

  if (surface === "minimal") {
    return {
      hoistBuiltinTools,
      hoistBash: builtinToolEnabled("bash"),
      hoistPowershell: powershellEnabled && builtinToolEnabled("powershell"),
      hoistRead: false,
      hoistWrite: false,
      hoistEdit: false,
      hoistGrep: false,
      restrictToProjectRoot,
      outline: ok("aft_outline"),
      zoom: ok("aft_zoom"),
      semantic: false,
      inspect: false,
      navigate: false,
      conflicts: false,
      importTool: false,
      safety: ok("aft_safety"),
      delete: false,
      move: false,
      astSearch: false,
      astReplace: false,
    };
  }

  const base: PiToolSurface = {
    hoistBuiltinTools,
    hoistBash: builtinToolEnabled("bash"),
    hoistPowershell: powershellEnabled && builtinToolEnabled("powershell"),
    hoistRead: builtinToolEnabled("read"),
    hoistWrite: builtinToolEnabled("write"),
    hoistEdit: builtinToolEnabled("edit"),
    hoistGrep: builtinToolEnabled("grep") && config.search_index === true,
    restrictToProjectRoot,
    outline: ok("aft_outline"),
    zoom: ok("aft_zoom"),
    semantic: ok("aft_search") && config.semantic_search === true,
    inspect: ok("aft_inspect") && config.inspect?.enabled !== false,
    navigate: false,
    conflicts: ok("aft_conflicts"),
    importTool: ok("aft_import"),
    safety: ok("aft_safety"),
    delete: false,
    move: false,
    astSearch: ok("ast_grep_search"),
    astReplace: ok("ast_grep_replace"),
  };

  if (surface === "all") {
    return {
      ...base,
      navigate: allOnly("aft_callgraph"),
      delete: allOnly("aft_delete"),
      move: allOnly("aft_move"),
    };
  }

  return base;
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
  const bashCfg = resolveBashConfig(ctx.config);
  const bashRegistered = surface.hoistBash && bashCfg.enabled;
  const powershellRegistered = surface.hoistPowershell && bashCfg.enabled;
  if (bashRegistered) {
    registerBashTool(
      boundPi,
      ctx,
      surface.semantic,
      surface.hoistBuiltinTools ? "bash" : "aft_bash",
      false,
    );
  }
  if (powershellRegistered) {
    registerBashTool(
      boundPi,
      ctx,
      surface.semantic,
      surface.hoistBuiltinTools ? "powershell" : "aft_powershell",
      false,
      "powershell",
    );
  }
  // These controls address AFT task IDs, so one shell-family registration makes
  // the shared controls available without colliding with a host-native tool.
  if (bashRegistered || powershellRegistered) registerBashCompanionTools(boundPi, ctx);
  registerHoistedTools(boundPi, ctx, surface);

  if (surface.outline || surface.zoom) registerReadingTools(boundPi, ctx, surface);
  if (surface.semantic) registerSemanticTool(boundPi, ctx);
  if (surface.inspect) registerInspectTool(boundPi, ctx);
  if (surface.navigate) registerNavigateTool(boundPi, ctx);
  if (surface.conflicts) registerConflictsTool(boundPi, ctx);
  if (surface.importTool) registerImportTools(boundPi, ctx);
  if (surface.safety && ctx.config.backup?.enabled !== false) registerSafetyTool(boundPi, ctx);
  if (surface.astSearch || surface.astReplace) registerAstTools(boundPi, ctx, surface);
  if (surface.delete || surface.move) registerFsTools(boundPi, ctx, surface);
}
