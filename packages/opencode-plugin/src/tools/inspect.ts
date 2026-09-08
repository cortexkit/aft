import type { ToolDefinition } from "@opencode-ai/plugin";
import { tool } from "@opencode-ai/plugin";
import { resolveInspectDiagnosticsTimeoutMs } from "../config.js";
import type { PluginContext } from "../types.js";
import { callToolCall, isEmptyParam, resolvePathArg } from "./_shared.js";
import { assertExternalDirectoryPermission, permissionDeniedResponse } from "./permissions.js";

const z = tool.schema;
// `diagnostics_timeout_ms` is the server's whole-request terminal deadline.
// Transport adds egress headroom so the server always answers before the client
// gives up; headroom is never available to server-side inspect work.
const INSPECT_TRANSPORT_HEADROOM_MS = 30_000;

type ToolArg = ToolDefinition["args"][string];
type StringOrStringArray = string | string[];
export type InspectTerminalKind = "FRESH" | "INTERRUPTED" | "PHASE-FAILED";

/** A completed inspect phase, normalized once for every terminal outcome. */
export interface InspectPhaseEntry {
  id: string;
  producer?: string;
  category?: string;
  alsoSatisfied: string[];
}

export interface InspectTerminal {
  kind: InspectTerminalKind;
  phases: InspectPhaseEntry[];
  waitStampText?: string;
  failedPhase?: InspectPhaseEntry;
  failureReason?: string;
  failureDetail?: string;
}

function arg(schema: unknown): ToolArg {
  return schema as ToolArg;
}

function normalizeStringOrArray(value: unknown): StringOrStringArray | undefined {
  return isEmptyParam(value) ? undefined : (value as StringOrStringArray);
}

async function resolveAndGateScope(
  ctx: PluginContext,
  context: Parameters<ToolDefinition["execute"]>[1],
  scope: StringOrStringArray | undefined,
): Promise<{ scope: StringOrStringArray | undefined; denial?: string }> {
  if (scope === undefined) return { scope: undefined };
  const values = Array.isArray(scope) ? scope : [scope];
  const resolved = await Promise.all(
    values
      .filter((value): value is string => typeof value === "string" && value.length > 0)
      .map((value) => resolvePathArg(ctx, context, value)),
  );
  const checked = new Set<string>();
  for (const target of resolved) {
    if (checked.has(target)) continue;
    checked.add(target);
    const denial = await assertExternalDirectoryPermission(ctx, context, target);
    if (denial) return { scope: undefined, denial };
  }
  return { scope: Array.isArray(scope) ? resolved : resolved[0] };
}

function asRecord(value: unknown): Record<string, unknown> | undefined {
  return value !== null && typeof value === "object" && !Array.isArray(value)
    ? (value as Record<string, unknown>)
    : undefined;
}

function asString(value: unknown): string | undefined {
  return typeof value === "string" ? value : undefined;
}

function firstRecord(...values: unknown[]): Record<string, unknown> | undefined {
  for (const value of values) {
    const record = asRecord(value);
    if (record) return record;
  }
  return undefined;
}

function firstString(...values: unknown[]): string | undefined {
  for (const value of values) {
    const text = asString(value);
    if (text !== undefined) return text;
  }
  return undefined;
}

function terminalKind(response: Record<string, unknown>): InspectTerminalKind | undefined {
  for (const value of [
    response.inspect_terminal,
    response.terminal,
    response.outcome,
    response.inspect_outcome,
    response.status,
  ]) {
    if (typeof value !== "string") continue;
    const normalized = value.toUpperCase().replaceAll("_", "-");
    if (normalized === "FRESH" || normalized === "INTERRUPTED" || normalized === "PHASE-FAILED") {
      return normalized;
    }
  }
  return undefined;
}

/**
 * Parse phase entries shared by all terminal outcomes. If also_satisfied is
 * omitted, store it as an empty list so callers can always treat it as a list.
 */
export function parseInspectPhaseEntries(value: unknown): InspectPhaseEntry[] {
  if (!Array.isArray(value)) return [];

  return value.flatMap((candidate) => {
    const entry = asRecord(candidate);
    const id = asString(entry?.id);
    if (!id) return [];
    const alsoSatisfied = Array.isArray(entry?.also_satisfied)
      ? entry.also_satisfied.filter((category): category is string => typeof category === "string")
      : [];
    return [
      {
        id,
        producer: asString(entry?.producer),
        category: asString(entry?.category),
        alsoSatisfied,
      },
    ];
  });
}

/**
 * Use the same phase parser for every terminal outcome. Alternate discriminator
 * spellings keep older payload encodings compatible with the canonical shape.
 */
export function parseInspectTerminal(payload: unknown): InspectTerminal | undefined {
  const response = asRecord(payload);
  if (!response) return undefined;
  const kind = terminalKind(response);
  if (!kind) return undefined;

  const waitStamp = firstRecord(
    response.wait_stamp,
    response.waitStamp,
    response.blocking_wait_stamp,
  );
  const phases = parseInspectPhaseEntries(
    kind === "FRESH"
      ? (waitStamp?.phases ??
          response.completed_phases ??
          response.completedPhases ??
          response.phases)
      : (response.completed_phases ?? response.completedPhases ?? response.phases),
  );

  if (kind !== "PHASE-FAILED") {
    return {
      kind,
      phases,
      waitStampText:
        kind === "FRESH" ? firstString(waitStamp?.text, waitStamp?.human_text) : undefined,
    };
  }

  const failedPhaseRecord = asRecord(response.failed_phase);
  const failedPhaseId = firstString(failedPhaseRecord?.id, response.failed_phase);
  const failedPhase = failedPhaseId
    ? parseInspectPhaseEntries([
        {
          id: failedPhaseId,
          producer: firstString(
            failedPhaseRecord?.producer,
            response.failed_phase_producer,
            response.producer,
          ),
          category: firstString(
            failedPhaseRecord?.category,
            response.failed_phase_category,
            response.category,
          ),
        },
      ])[0]
    : undefined;

  return {
    kind,
    phases,
    failedPhase,
    failureReason: asString(response.failure_reason),
    failureDetail: asString(response.failure_detail),
  };
}

function formatPhase(entry: InspectPhaseEntry): string {
  const details = [
    entry.producer ? `producer: ${entry.producer}` : undefined,
    entry.category ? `category: ${entry.category}` : undefined,
    entry.alsoSatisfied.length > 0
      ? `also satisfied: ${entry.alsoSatisfied.join(", ")}`
      : undefined,
  ].filter((detail): detail is string => Boolean(detail));
  return details.length > 0 ? `${entry.id} (${details.join("; ")})` : entry.id;
}

/** Render every terminal honestly without reducing failures to a generic error. */
export function renderInspectTerminal(terminal: InspectTerminal, serverText?: string): string {
  if (terminal.kind === "FRESH") {
    const lines: string[] = [
      terminal.kind,
      `wait-stamp: ${terminal.waitStampText ?? "not supplied"}`,
    ];
    lines.push(
      terminal.phases.length > 0
        ? `completed phases:\n${terminal.phases.map((phase) => `- ${formatPhase(phase)}`).join("\n")}`
        : "completed phases: none",
    );
    if (serverText?.trim()) lines.push(serverText);
    return lines.join("\n");
  }

  if (terminal.kind === "PHASE-FAILED") {
    const detail = compactTerminalField(terminal.failureDetail, "not supplied");
    const reason = compactTerminalField(terminal.failureReason, "not supplied");
    return [
      `inspect could not complete: ${detail} (${reason}).`,
      `Completed phases: ${terminal.phases.length}. Retry, or narrow with sections=...`,
    ].join("\n");
  }

  const phaseLabel = terminal.phases.length === 1 ? "phase" : "phases";
  return [
    `inspect was interrupted before it could complete (after ${terminal.phases.length} completed ${phaseLabel}); no fresh snapshot was produced.`,
    "Retry is safe; retry inspect, or narrow with sections=...",
  ].join("\n");
}

function compactTerminalField(value: string | undefined, fallback: string): string {
  const compact = value?.trim().replace(/\s+/g, " ");
  return compact || fallback;
}

export interface InspectToolConfig {
  tool_surface?: "minimal" | "recommended" | "all";
  disabled_tools?: string[];
  inspect?: {
    enabled?: boolean;
    diagnostics_timeout_ms?: number;
    tier2_idle_minutes?: number;
  };
}

export function inspectToolSurfaceEnabled(config: InspectToolConfig): boolean {
  return (config.tool_surface ?? "recommended") !== "minimal" && config.inspect?.enabled !== false;
}

export function shouldRegisterInspectTool(config: InspectToolConfig): boolean {
  return (
    inspectToolSurfaceEnabled(config) && !(config.disabled_tools ?? []).includes("aft_inspect")
  );
}

type TimerHandle = ReturnType<typeof setTimeout>;

export interface InspectTier2IdleSchedulerOptions {
  isEnabled: () => boolean;
  idleMinutes: () => number | undefined;
  run: (sessionID: string) => Promise<void>;
  warn?: (message: string) => void;
  setTimer?: (callback: () => void, delayMs: number) => TimerHandle;
  clearTimer?: (timer: TimerHandle) => void;
}

export function createInspectTier2IdleScheduler(options: InspectTier2IdleSchedulerOptions) {
  const timers = new Map<string, TimerHandle>();
  const setTimer = options.setTimer ?? ((callback, delayMs) => setTimeout(callback, delayMs));
  const clearTimer = options.clearTimer ?? ((timer) => clearTimeout(timer));

  const clear = (sessionID: string): void => {
    const timer = timers.get(sessionID);
    if (!timer) return;
    clearTimer(timer);
    timers.delete(sessionID);
  };

  const clearAll = (): void => {
    for (const timer of timers.values()) clearTimer(timer);
    timers.clear();
  };

  const schedule = (sessionID: string): void => {
    if (!options.isEnabled()) return;
    clear(sessionID);
    const idleMinutes = options.idleMinutes() ?? 4;
    const delayMs = Math.max(0, idleMinutes * 60 * 1000);
    const timer = setTimer(() => {
      timers.delete(sessionID);
      options.run(sessionID).catch((err) => {
        options.warn?.(
          `inspect_tier2_run failed: ${err instanceof Error ? err.message : String(err)}`,
        );
      });
    }, delayMs);
    timers.set(sessionID, timer);
  };

  return { schedule, clear, clearAll };
}

export function inspectTools(ctx: PluginContext): Record<string, ToolDefinition> {
  const inspectTool: ToolDefinition = {
    description:
      "Blocking-fresh codebase health inspection. Each call completes current analysis and produces exactly one terminal result: FRESH includes a wait-stamp and completed phases; INTERRUPTED and PHASE-FAILED retain completed phases, with PHASE-FAILED also reporting its phase attribution and failure reason. `sections` selects drill-down detail, not the categories verified.\n\n" +
      "Use `scope=` to narrow returned results. Scope filters rendered diagnostics and limits Rust LSP startup to Cargo workspaces owning the scoped paths; it does not trigger per-file collection work. Scoped files no producer has authoritatively analyzed are reported as named gaps (complete: false). Passive health changes use the alert channel; do not infer inspect completion from that channel.\n\n" +
      "Use when: starting work on unfamiliar code, after multi-edit batches to check diagnostics, before a refactor, before review, or to verify cleanup completeness.\n\n" +
      "Treat `dead_code` as a hint, not proof: reachability is call-based, so symbols reached only via method dispatch or referenced only in type position may be false positives — verify before deleting.\n\n" +
      "When a list is cut, the reply ends with `shown N of M <unit> (<reason>) · narrow: <knobs>`; absence of that line means the list is complete.",
    args: {
      sections: arg(
        z
          .union([z.string(), z.array(z.string())])
          .optional()
          .describe(
            "Categories to include in detailed drill-down (e.g. 'todos' or ['todos', 'dead_code', 'cycles']). Use 'all' for every active category. Omit for summary-only mode. `sections` changes detail, not the categories verified.",
          ),
      ),
      scope: arg(
        z
          .union([z.string(), z.array(z.string())])
          .optional()
          .describe(
            "Restrict returned results to paths under this scope (file or directory, absolute or relative to project root). `scope=` narrows results and limits Rust LSP startup to owning Cargo workspaces; it does not trigger per-file diagnostic collection. Scoped files no producer has authoritatively analyzed are reported as named gaps (complete: false), never as a clean empty result.",
          ),
      ),
      topK: arg(
        z
          .number()
          .int()
          .positive()
          .max(100)
          .optional()
          .describe("Max drill-down items per category. Default 20, max 100."),
      ),
    },
    execute: async (args, context): Promise<string> => {
      const sections = normalizeStringOrArray(args.sections);
      const scoped = await resolveAndGateScope(ctx, context, normalizeStringOrArray(args.scope));
      if (scoped.denial) return permissionDeniedResponse(scoped.denial);
      const rawArgs: Record<string, unknown> = {};
      if (sections !== undefined) rawArgs.sections = sections;
      if (scoped.scope !== undefined) rawArgs.scope = scoped.scope;
      if (args.topK !== undefined && args.topK !== null) rawArgs.topK = args.topK;
      const response = await callToolCall(ctx, context, "inspect", rawArgs, {
        transportTimeoutMs:
          resolveInspectDiagnosticsTimeoutMs(ctx.config) + INSPECT_TRANSPORT_HEADROOM_MS,
      });
      const terminal = parseInspectTerminal(response);
      if (terminal) return renderInspectTerminal(terminal, response.text);
      if (response.success === false)
        throw new Error((response.message as string) || "inspect failed");
      return response.text;
    },
  };

  return { aft_inspect: inspectTool };
}
