/**
 * aft_inspect — blocking-fresh codebase health inspection.
 */

import type {
  AgentToolResult,
  ExtensionAPI,
  ExtensionContext,
  Theme,
} from "@earendil-works/pi-coding-agent";
import { type Static, Type } from "typebox";
import { resolveInspectDiagnosticsTimeoutMs } from "../config.js";
import type { PluginContext } from "../types.js";
import {
  bridgeFor,
  callToolCall,
  isEmptyParam,
  PI_TOOL_EXECUTION_TIMEOUT_MS,
  PI_TOOL_TRANSPORT_TIMEOUT_MS,
  textResult,
} from "./_shared.js";
import { assertExternalDirectoryPermission, resolvePathArg } from "./hoisted.js";
import {
  asNumber,
  asRecord,
  asRecords,
  asString,
  collapsibleResult,
  extractStructuredPayload,
  type RenderContextLike,
  type RenderResultOptionsLike,
  renderErrorResult,
  renderSections,
  renderToolCall,
} from "./render-helpers.js";

// Keep Rust's phase deadline below the Pi transport deadline so inspect can
// return an honest terminal with completed phases instead of a host timeout.
const INSPECT_DIAGNOSTICS_TIMEOUT_MS = PI_TOOL_EXECUTION_TIMEOUT_MS;

const InspectParams = Type.Object({
  sections: Type.Optional(
    Type.Union([Type.String(), Type.Array(Type.String())], {
      description:
        "Categories to include in detailed drill-down (e.g. 'todos' or ['todos', 'dead_code', 'cycles']). Use 'all' for every active category. Omit for summary-only mode. `sections` changes detail, not the categories verified.",
    }),
  ),
  scope: Type.Optional(
    Type.Union([Type.String(), Type.Array(Type.String())], {
      description:
        "Restrict returned results to paths under this scope (file or directory, absolute or relative to project root). `scope=` narrows results; it does not change the diagnostic collection work. Scoped files no producer has authoritatively analyzed are reported as named gaps (complete: false), never as a clean empty result.",
    }),
  ),
  topK: Type.Optional(
    Type.Integer({
      minimum: 1,
      maximum: 100,
      default: 20,
      description: "Max drill-down items per category. Default 20, max 100.",
    }),
  ),
});

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

function normalizeStringOrArray(value: unknown): StringOrStringArray | undefined {
  return isEmptyParam(value) ? undefined : (value as StringOrStringArray);
}

async function resolveAndGateScope(
  extCtx: ExtensionContext,
  ctx: PluginContext,
  scope: StringOrStringArray | undefined,
): Promise<StringOrStringArray | undefined> {
  if (scope === undefined) return undefined;
  const values = Array.isArray(scope) ? scope : [scope];
  const resolved = await Promise.all(
    values
      .filter((value): value is string => typeof value === "string" && value.length > 0)
      .map((value) => resolvePathArg(extCtx.cwd, value)),
  );
  const checked = new Set<string>();
  for (const target of resolved) {
    if (checked.has(target)) continue;
    checked.add(target);
    await assertExternalDirectoryPermission(extCtx, target, {
      restrictToProjectRoot: ctx.config.restrict_to_project_root ?? false,
    });
  }
  return Array.isArray(scope) ? resolved : resolved[0];
}

function validateOptionalTopK(value: unknown): number | undefined {
  if (value === undefined || value === null || value === "") return undefined;
  if (typeof value !== "number" || !Number.isInteger(value)) {
    throw new Error("topK must be an integer between 1 and 100");
  }
  if (value < 1 || value > 100) {
    throw new Error("topK must be between 1 and 100");
  }
  return value;
}

function terminalKind(response: Record<string, unknown>): InspectTerminalKind | undefined {
  for (const value of [
    response.terminal,
    response.outcome,
    response.inspect_outcome,
    response.inspect_terminal,
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
  const phaseSource = kind === "FRESH" ? (waitStamp ?? response) : response;
  const phases = parseInspectPhaseEntries(
    phaseSource.phases ?? response.completed_phases ?? response.completedPhases,
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

function diagnosticsSummaryPart(summary: Record<string, unknown> | undefined): string | undefined {
  const section = asRecord(summary?.diagnostics);
  if (!section) return undefined;
  const errors = asNumber(section.errors);
  const warnings = asNumber(section.warnings);
  const info = asNumber(section.info);
  const hints = asNumber(section.hints);
  if (![errors, warnings, info, hints].some((value) => value !== undefined)) return undefined;
  return `diagnostics ${errors ?? 0} errors/${warnings ?? 0} warnings/${info ?? 0} info/${hints ?? 0} hints`;
}

function diagnosticLocation(diagnostic: Record<string, unknown>): string {
  const file = asString(diagnostic.file) ?? "(unknown file)";
  const line = asNumber(diagnostic.line);
  const column = asNumber(diagnostic.column);
  if (line === undefined) return file;
  if (column === undefined) return `${file}:${line}`;
  return `${file}:${line}:${column}`;
}

function diagnosticsDetailSection(
  details: Record<string, unknown> | undefined,
): string | undefined {
  const diagnostics = asRecords(details?.diagnostics);
  if (diagnostics.length === 0) return undefined;
  return [
    "diagnostics",
    ...diagnostics.map((diagnostic) => {
      const severity = asString(diagnostic.severity) ?? "information";
      const message = asString(diagnostic.message) ?? "(no message)";
      const source = asString(diagnostic.source);
      return `- ${diagnosticLocation(diagnostic)} ${severity} ${message}${source ? ` [${source}]` : ""}`;
    }),
  ].join("\n");
}

function countFrom(summary: Record<string, unknown> | undefined, key: string): number | undefined {
  return asNumber(asRecord(summary?.[key])?.count);
}

function tier2SummaryPart(
  summary: Record<string, unknown> | undefined,
  key: string,
  label: string,
): string {
  const section = asRecord(summary?.[key]);
  const count = asNumber(section?.count);
  return count !== undefined ? `${label} ${count}` : `${label} unavailable`;
}

/** Short basename for a `path:line-line` duplicate occurrence. */
function shortDupOccurrence(entry: string): string {
  const [path] = entry.split(":");
  return path?.split("/").pop() ?? entry;
}

function tier2TopPreview(
  summary: Record<string, unknown> | undefined,
  theme: Theme,
): string | undefined {
  const lines: string[] = [];
  const dupTop = Array.isArray(asRecord(summary?.duplicates)?.top)
    ? (asRecord(summary?.duplicates)?.top as unknown[])
    : [];
  for (const group of dupTop) {
    const record = asRecord(group);
    const files = Array.isArray(record?.files) ? record.files : [];
    const cost = asNumber(record?.cost);
    if (files.length < 2) continue;
    lines.push(
      `  dup ${shortDupOccurrence(String(files[0]))} ↔ ${shortDupOccurrence(String(files[1]))}${cost !== undefined ? ` (${cost})` : ""}`,
    );
  }
  for (const [key, label] of [
    ["dead_code", "dead"],
    ["unused_exports", "unused"],
  ] as const) {
    const top = Array.isArray(asRecord(summary?.[key])?.top)
      ? (asRecord(summary?.[key])?.top as unknown[])
      : [];
    for (const item of top) {
      const record = asRecord(item);
      const file = asString(record?.file);
      const symbol = asString(record?.symbol);
      if (file && symbol) lines.push(`  ${label} ${symbol} (${file.split("/").pop()})`);
    }
  }
  return lines.length > 0
    ? `${theme.fg("muted", "top findings:")}\n${lines.join("\n")}`
    : undefined;
}

/** Exported for renderer unit tests. */
export function buildInspectSections(payload: unknown, theme: Theme): string[] {
  const terminal = parseInspectTerminal(payload);
  if (terminal) return [renderInspectTerminal(terminal, asString(asRecord(payload)?.text))];

  const response = asRecord(payload);
  if (!response) return [theme.fg("muted", "No inspect snapshot available.")];
  const summary = asRecord(response.summary);
  const metrics = asRecord(summary?.metrics);
  const parts = [
    `todos ${countFrom(summary, "todos") ?? 0}`,
    diagnosticsSummaryPart(summary),
    `metrics ${asNumber(metrics?.files) ?? 0} files/${asNumber(metrics?.symbols) ?? 0} symbols`,
    tier2SummaryPart(summary, "dead_code", "dead code"),
    tier2SummaryPart(summary, "unused_exports", "unused exports"),
    tier2SummaryPart(summary, "duplicates", "duplicates"),
    tier2SummaryPart(summary, "cycles", "cycles"),
  ].filter((part): part is string => Boolean(part));
  const sections = [theme.fg("accent", parts.join(" · "))];
  const topPreview = tier2TopPreview(summary, theme);
  if (topPreview) sections.push(topPreview);
  const details = asRecord(response.details);
  if (details) {
    const names = Object.keys(details);
    sections.push(
      names.length > 0
        ? `details: ${names.join(", ")}`
        : theme.fg("muted", "No drill-down details returned."),
    );
    const diagnosticsDetails = diagnosticsDetailSection(details);
    if (diagnosticsDetails) sections.push(diagnosticsDetails);
  }
  const text = asString(response.text);
  if (text) sections.push(text);
  return sections;
}

/** Exported for renderer unit tests. */
export function renderInspectCall(
  args: Static<typeof InspectParams>,
  theme: Theme,
  context: RenderContextLike,
) {
  const sections = Array.isArray(args.sections)
    ? `${args.sections.length} sections`
    : args.sections;
  const scope = Array.isArray(args.scope) ? `${args.scope.length} scopes` : args.scope;
  const summary = [sections, scope, args.topK ? `topK=${args.topK}` : undefined]
    .filter(Boolean)
    .join(" ");
  return renderToolCall(
    "inspect",
    summary ? theme.fg("toolOutput", summary) : undefined,
    theme,
    context,
  );
}

/** Exported for renderer unit tests. */
export function renderInspectResult(
  result: AgentToolResult<unknown>,
  theme: Theme,
  context: RenderContextLike,
  options: RenderResultOptionsLike = { expanded: true },
) {
  const payload = extractStructuredPayload(result);
  const terminal = parseInspectTerminal(payload);
  if (terminal) {
    const sections = [renderInspectTerminal(terminal, asString(asRecord(payload)?.text))];
    return collapsibleResult({
      summary: sections[0]?.split("\n")[0] ?? "inspect",
      full: renderSections(sections, context),
      expanded: options.expanded,
      context,
    });
  }
  if (context.isError) return renderErrorResult(result, "inspect failed", theme, context);
  const sections = buildInspectSections(payload, theme);
  return collapsibleResult({
    summary: sections[0] ?? "inspect completed",
    full: renderSections(sections, context),
    expanded: options.expanded,
    context,
  });
}

export function registerInspectTool(pi: ExtensionAPI, ctx: PluginContext): void {
  pi.registerTool({
    name: "aft_inspect",
    label: "inspect",
    description:
      "Blocking-fresh codebase health inspection. Each call completes current analysis and produces exactly one terminal result: FRESH includes a wait-stamp and completed phases; INTERRUPTED and PHASE-FAILED retain completed phases, with PHASE-FAILED also reporting its phase attribution and failure reason. `sections` selects drill-down detail, not the categories verified.\n\n" +
      "Use `scope=` to narrow returned results. Scope filters rendered diagnostics; it does not change collection work. Scoped files no producer has authoritatively analyzed are reported as named gaps (complete: false). Passive health changes use the alert channel; do not infer inspect completion from that channel.\n\n" +
      "Use when: starting work on unfamiliar code, after multi-edit batches to check diagnostics, before a refactor, before review, or to verify cleanup completeness.\n\n" +
      "Treat `dead_code` as a hint, not proof: reachability is call-based, so symbols reached only via method dispatch or referenced only in type position may be false positives — verify before deleting.",
    parameters: InspectParams,
    async execute(_toolCallId, params: Static<typeof InspectParams>, _signal, onUpdate, extCtx) {
      const bridge = bridgeFor(ctx, extCtx.cwd);
      const sections = normalizeStringOrArray(params.sections);
      const scope = await resolveAndGateScope(extCtx, ctx, normalizeStringOrArray(params.scope));
      const topK = validateOptionalTopK(params.topK);
      const rawArgs: Record<string, unknown> = {
        diagnostics_timeout_ms: Math.min(
          resolveInspectDiagnosticsTimeoutMs(ctx.config),
          INSPECT_DIAGNOSTICS_TIMEOUT_MS,
        ),
      };
      if (sections !== undefined) rawArgs.sections = sections;
      if (scope !== undefined) rawArgs.scope = scope;
      if (topK !== undefined) rawArgs.topK = topK;
      const response = await callToolCall(bridge, "inspect", rawArgs, extCtx, {
        transportTimeoutMs: PI_TOOL_TRANSPORT_TIMEOUT_MS,
        onUpdate,
      });
      const terminal = parseInspectTerminal(response);
      if (terminal) return textResult(renderInspectTerminal(terminal, response.text), response);
      if (response.success === false)
        throw new Error(response.text || response.message || "inspect failed");
      return textResult(response.text, response);
    },
    renderCall(args, theme, context) {
      return renderInspectCall(args, theme, context);
    },
    renderResult(result, options = { expanded: false, isPartial: false }, theme, context) {
      return renderInspectResult(result, theme, context, options);
    },
  });
}
