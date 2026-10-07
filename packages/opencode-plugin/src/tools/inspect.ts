import { coerceJsonCollectionParam } from "@cortexkit/aft-bridge";
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
export type InspectTerminalKind = "FRESH" | "PARTIAL" | "INTERRUPTED" | "PHASE-FAILED";

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
  /** Set for PARTIAL: which diagnostics producers are unknown. */
  partialReason?: string;
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
    if (
      normalized === "FRESH" ||
      normalized === "PARTIAL" ||
      normalized === "INTERRUPTED" ||
      normalized === "PHASE-FAILED"
    ) {
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
  // PARTIAL is a completed result like FRESH (it carries the same wait
  // stamp); only its diagnostics are not authoritative.
  const completed = kind === "FRESH" || kind === "PARTIAL";
  const phases = parseInspectPhaseEntries(
    completed
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
      waitStampText: completed ? firstString(waitStamp?.text, waitStamp?.human_text) : undefined,
      partialReason:
        kind === "PARTIAL"
          ? (asString(response.partial_reason) ?? "diagnostics unknown")
          : undefined,
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

/**
 * One line per phase id, with a count and a per-producer (or per-category)
 * breakdown, largest first: `lsp_start ×25 (typescript 12, bash 6)`. A project
 * with two dozen language servers otherwise printed two dozen near-identical
 * lines. Mirrors `collapse_phase_entries` in the Rust phase log.
 */
export function collapseInspectPhases(phases: InspectPhaseEntry[]): string[] {
  const groups: { id: string; count: number; labels: Map<string, number>; also: Set<string> }[] =
    [];
  for (const phase of phases) {
    let group = groups.find((candidate) => candidate.id === phase.id);
    if (!group) {
      group = { id: phase.id, count: 0, labels: new Map(), also: new Set() };
      groups.push(group);
    }
    group.count += 1;
    const label = phase.producer ?? phase.category;
    if (label) group.labels.set(label, (group.labels.get(label) ?? 0) + 1);
    for (const category of phase.alsoSatisfied) group.also.add(category);
  }
  return groups.map((group) => {
    // Array.prototype.sort is stable, so equal counts keep first-seen order.
    const labels = [...group.labels.entries()].sort((left, right) => right[1] - left[1]);
    const details: string[] = [];
    if (group.count === 1 && labels.length === 1) details.push(labels[0][0]);
    else if (labels.length > 0)
      details.push(labels.map(([label, n]) => `${label} ${n}`).join(", "));
    if (group.also.size > 0) details.push(`also satisfied: ${[...group.also].join(", ")}`);
    const head = group.count > 1 ? `${group.id} ×${group.count}` : group.id;
    return details.length > 0 ? `${head} (${details.join("; ")})` : head;
  });
}

/** Render every terminal honestly without reducing failures to a generic error. */
export function renderInspectTerminal(terminal: InspectTerminal, serverText?: string): string {
  // The server owns the status and findings; do not prepend a second status.
  if (serverText?.match(/^(FRESH\n|PARTIAL — |INTERRUPTED — |PHASE-FAILED — )/)) return serverText;
  if (terminal.kind === "FRESH" || terminal.kind === "PARTIAL") {
    // FRESH never heads a result whose diagnostics are unknown: that result
    // is PARTIAL, and the header names the producers.
    const header =
      terminal.kind === "PARTIAL"
        ? `PARTIAL — ${terminal.partialReason ?? "diagnostics unknown; retry aft_inspect."}`
        : terminal.kind;
    const lines: string[] = [header];
    if (serverText?.trim()) lines.push(serverText);
    return lines.join("\n");
  }

  if (terminal.kind === "PHASE-FAILED") {
    const detail = compactTerminalField(terminal.failureDetail, "not supplied");
    const reason = compactTerminalField(terminal.failureReason, "not supplied");
    return [
      `PHASE-FAILED — inspect could not complete: ${detail} (${reason}). Retry aft_inspect, or narrow the scope.`,
    ].join("\n");
  }

  return [
    "INTERRUPTED — inspect stopped before it could complete; no fresh snapshot was produced. Retry aft_inspect, or narrow the scope.",
  ].join("\n");
}

function compactTerminalField(value: string | undefined, fallback: string): string {
  let compact = value?.trim().replace(/\s+/g, " ");
  for (const [internal, plain] of [
    ["lsp_start", "starting language servers"],
    ["lsp_quiescence", "waiting for language servers"],
    ["stat_verification", "verifying files are unchanged"],
    ["callgraph_ready", "preparing call analysis"],
    ["tier2_rescan", "running code analysis"],
  ])
    compact = compact?.replaceAll(internal, plain);
  return compact || fallback;
}

export interface InspectToolConfig {
  disabled_tools?: string[];
}

/**
 * `aft_inspect` registers exactly when it is not disabled. `inspect.enabled:
 * false` is a runtime gate (the engine answers `inspect_disabled`), not a
 * registration predicate.
 */
export function shouldRegisterInspectTool(config: InspectToolConfig): boolean {
  return !(config.disabled_tools ?? []).includes("aft_inspect");
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
      "Codebase health inspection that waits for current analysis. FRESH means the reported analysis is current. PARTIAL means some diagnostics are unknown; the header names the analyzer, reason, and retry guidance. INTERRUPTED means the request stopped without a fresh snapshot; retry aft_inspect. PHASE-FAILED means inspection could not finish; address the reported reason and retry, or narrow the scope. `sections` selects drill-down detail, not the categories verified. Categories switched off in `inspect.categories` are not computed or refreshed, render as off, and never make the header PARTIAL.\n\n" +
      "Use `scope=` to narrow all findings, counts, and examples to those paths. Cross-boundary duplicates are labeled as groups touching the scope. Scope also limits Rust analyzer startup to the Cargo workspaces owning those paths. Files without an authoritative diagnostic report remain named gaps (complete: false), not a clean result.\n\n" +
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
            "Restrict returned results to paths under this scope (one path, or an array of paths — not a space-separated list; file or directory; absolute or relative to project root). `scope=` narrows results and limits Rust LSP startup to owning Cargo workspaces; it does not trigger per-file diagnostic collection. Scoped files no producer has authoritatively analyzed are reported as named gaps (complete: false), never as a clean empty result.",
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
      const sections = normalizeStringOrArray(coerceJsonCollectionParam(args.sections, "sections"));
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
