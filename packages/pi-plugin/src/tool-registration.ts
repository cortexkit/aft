import { fileURLToPath } from "node:url";
import { ADAPTER_UNIMPLEMENTED_TOOLS, CANONICAL_TOOLS } from "@cortexkit/aft-bridge";
import type {
  ExtensionAPI,
  ExtensionContext,
  ToolDefinition,
} from "@earendil-works/pi-coding-agent";
import type { TSchema } from "typebox";

import type { AftConfig } from "./config.js";
import { resolveBashConfig, resolvedDisabledTools } from "./config.js";
import { detectPiHarness, type PiHarness } from "./harness.js";
import { projectAftCall, withOmpInternalUrls } from "./omp-internal-urls.js";
import {
  outsideAccessPolicyFor,
  prepareToolDefinitionForRegistration,
  recordSessionCwd,
  resolveSessionId,
  withOutsidePermissionOperation,
} from "./tools/_shared.js";
import { registerAstTools } from "./tools/ast.js";
import { registerBashCompanionTools, registerBashTool } from "./tools/bash.js";
import { registerConflictsTool } from "./tools/conflicts.js";
import { registerFsTools } from "./tools/fs.js";
import { registerHoistedTools } from "./tools/hoisted.js";
import { registerImportTools } from "./tools/imports.js";
import { registerInspectTool } from "./tools/inspect.js";
import { registerNavigateTool } from "./tools/navigate.js";
import { registerReadingTools } from "./tools/reading.js";
import { asRecord } from "./tools/render-helpers.js";
import { registerSafetyTool } from "./tools/safety.js";
import { registerSemanticTool } from "./tools/semantic.js";
import type { PluginContext } from "./types.js";

/**
 * Registration predicates for the Pi/OMP adapter. Each flag is "the canonical
 * name is not in the resolved disabled list", except that the bash companions
 * also need `bash.background`. Nothing else (index state, other runtime gates)
 * removes a registration.
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

/**
 * The surface with `restrictToProjectRoot` read from `ctx.config` at each
 * access. Tools capture the surface when they register, and a live config
 * reload replaces `ctx.config`, so the path restriction follows the file
 * while the registration flags stay as loaded.
 */
export function withLiveRestriction(surface: PiToolSurface, ctx: PluginContext): PiToolSurface {
  return Object.defineProperty({ ...surface }, "restrictToProjectRoot", {
    enumerable: true,
    get: () => ctx.config.restrict_to_project_root ?? false,
  });
}

/** Resolve the registration predicates used by Pi's production registration path. */
export function resolvePiToolSurface(config: AftConfig, pi?: ExtensionAPI): PiToolSurface {
  const disabled = new Set(resolvedDisabledTools(config));
  const ok = (name: string): boolean => !disabled.has(name);
  // The companions only act on background tasks, so they also need
  // `bash.background`; `registerBashCompanionTools` applies the same rule.
  const background = resolveBashConfig(config).background;
  const companion = (name: string): boolean => background && ok(name);
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
    bashStatus: companion("bash_status"),
    bashWatch: companion("bash_watch"),
    bashWrite: companion("bash_write"),
    bashKill: companion("bash_kill"),
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
    const hostAware = effectiveHarness === "omp" ? withOmpInternalUrls(tool, ctx.ompRouter) : tool;
    const prepared = prepareToolDefinitionForRegistration(
      hostAware,
      effectiveHarness,
      presentation,
    );
    originalRegisterTool({
      ...prepared,
      // Declared explicitly, not inferred: the binding below admits only
      // tools whose host-recorded `sourceInfo.path` equals this same constant.
      // Non-OMP hosts get no such field; they have no such contract to honor.
      ...(effectiveHarness === "omp" ? { sourcePath: TOOL_SOURCE_PATH } : {}),
      execute(toolCallId, params, signal, onUpdate, context) {
        recordSessionCwd((context as { cwd?: unknown } | undefined)?.cwd);
        return withOutsidePermissionOperation(
          () => prepared.execute(toolCallId, params, signal, onUpdate, context),
          signal,
          // Read per call: a live config reload replaces `ctx.config`.
          outsideAccessPolicyFor(ctx, effectiveHarness),
        );
      },
    } as ToolDefinition<TParams, TDetails, TState>);
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
  loadedSurface: PiToolSurface,
  harness?: PiHarness,
): void {
  const surface = withLiveRestriction(loadedSurface, ctx);
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

/**
 * Register the `powershell` slot after session start, when a modern Pi first
 * reveals that its optional built-in PowerShell tool is active. It goes through
 * the same registration funnel as the first pass, so on OMP it carries
 * `loadMode` and its approval tier like every other AFT tool.
 */
export function registerLatePowershellTool(
  pi: ExtensionAPI,
  ctx: PluginContext,
  semantic: boolean,
  harness?: PiHarness,
): void {
  const boundPi = bindToolRegistrationFunnel(pi, ctx, harness);
  registerBashTool(boundPi, ctx, semantic, "powershell", false, "powershell");
}

/**
 * Replace the `bash` slot when a live config reload flips
 * `bash.runon_enabled`: Pi copies a tool's schema into its registry, so the
 * slot is registered again with the new schema. Through the funnel, so on OMP
 * the replacement keeps `loadMode` and its approval tier.
 */
export function reregisterBashTool(
  pi: ExtensionAPI,
  ctx: PluginContext,
  semantic: boolean,
  harness?: PiHarness,
): void {
  const boundPi = bindToolRegistrationFunnel(pi, ctx, harness);
  registerBashTool(boundPi, ctx, semantic, "bash", false, "bash");
}

/** Replace Pi's captured revision-read schemas without changing the loaded tool set. */
export function refreshPiRevisionReadTools(
  pi: ExtensionAPI,
  ctx: PluginContext,
  loadedSurface: PiToolSurface,
): void {
  const surface = withLiveRestriction(
    {
      ...loadedSurface,
      hoistWrite: false,
      hoistEdit: false,
      hoistGrep: false,
    },
    ctx,
  );
  const boundPi = bindToolRegistrationFunnel(pi, ctx);
  registerHoistedTools(boundPi, ctx, surface);
  registerReadingTools(boundPi, ctx, surface);
}

/** Shared source identity for registered tools and their binding. */
export const TOOL_SOURCE_PATH: string = fileURLToPath(import.meta.url);

type AftCapability = "observation" | "mutation" | "execution" | "native-url";

const TOOL_CAPABILITIES: Readonly<Record<string, readonly AftCapability[]>> = {
  read: ["observation", "native-url"],
  write: ["mutation", "native-url"],
  edit: ["mutation", "native-url"],
  grep: ["observation", "native-url"],
  bash: ["execution", "native-url"],
  powershell: ["execution"],
  aft_outline: ["observation"],
  aft_zoom: ["observation"],
  aft_search: ["observation"],
  aft_inspect: ["observation"],
  aft_callgraph: ["observation"],
  aft_conflicts: ["observation"],
  aft_import: ["mutation"],
  aft_safety: ["observation", "mutation"],
  aft_delete: ["mutation"],
  aft_move: ["mutation"],
  ast_grep_search: ["observation"],
  ast_grep_replace: ["observation", "mutation"],
  bash_status: ["observation"],
  bash_watch: ["observation"],
  bash_write: ["execution"],
  bash_kill: ["execution"],
};

/** Answer correlated v1 requests from the live registry, regardless of extension load order. */
export function registerAftBinding(pi: ExtensionAPI, ctx: PluginContext): () => void {
  if (detectPiHarness(pi) !== "omp") return () => {};
  let latestContext: ExtensionContext | undefined;
  const capture = (_event: unknown, extCtx?: ExtensionContext): void => {
    if (extCtx) latestContext = extCtx;
  };
  // Observing tool_call would disable host speculation; capture lifecycle context instead.
  pi.on("session_start", capture);
  pi.on("session_tree", capture);
  pi.on("before_agent_start", capture);
  pi.on("agent_start", capture);
  pi.on("turn_start", capture);
  pi.on("input", capture);
  const subscription = pi.events.on("aft:binding-request", (payload: unknown) => {
    const extCtx = latestContext;
    const request = asRecord(payload);
    if (!extCtx || request?.v !== 1) return;
    const sessionId = resolveSessionId(extCtx);
    if (
      !sessionId ||
      request.sessionId !== sessionId ||
      typeof request.requestId !== "string" ||
      !request.requestId
    )
      return;
    let tools: Array<{ name: string; sourcePath: string; capabilities: AftCapability[] }>;
    try {
      const active = new Set(pi.getActiveTools());
      tools = pi
        .getAllTools()
        .filter(
          (tool) =>
            tool.sourceInfo?.path === TOOL_SOURCE_PATH &&
            active.has(tool.name) &&
            TOOL_CAPABILITIES[tool.name] !== undefined,
        )
        .map(({ name }) => ({
          name,
          sourcePath: TOOL_SOURCE_PATH,
          capabilities: [...TOOL_CAPABILITIES[name]],
        }));
    } catch {
      return;
    }
    if (tools.length === 0) return;
    pi.events.emit("aft:binding", {
      v: 1,
      sessionId,
      requestId: request.requestId,
      tools,
      project: (name: string, input: Record<string, unknown>, cwd: string) =>
        projectAftCall(ctx, name, input, cwd, extCtx),
    });
  });
  const unsubscribe = (): void => {
    latestContext = undefined;
    subscription();
  };
  pi.on("session_shutdown", unsubscribe);
  return unsubscribe;
}
