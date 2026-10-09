/**
 * aft_inspect — blocking-fresh codebase health inspection.
 */

import { coerceJsonCollectionParam } from "@cortexkit/aft-bridge";
import type {
  AgentToolResult,
  ExtensionAPI,
  ExtensionContext,
  Theme,
} from "@earendil-works/pi-coding-agent";
import { type Static, Type } from "typebox";
import { resolveInspectDiagnosticsTimeoutMs } from "../config.js";
import type { PluginContext } from "../types.js";
import { bridgeFor, callToolCall, isEmptyParam, textResult } from "./_shared.js";
import { assertExternalDirectoryPermission, resolvePathArg } from "./hoisted.js";
import {
  asNumber,
  asRecord,
  asRecordOrEmpty,
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

// `diagnostics_timeout_ms` is the server's whole-request terminal deadline.
// Transport adds egress headroom so the server always answers before the client
// gives up; headroom is never available to server-side inspect work.
const INSPECT_TRANSPORT_HEADROOM_MS = 30_000;

const InspectParams = Type.Object({
  sections: Type.Optional(
    Type.Union([Type.String(), Type.Array(Type.String())], {
      description:
        "Categories to include in detailed drill-down (e.g. 'todos' or ['todos', 'dead_code', 'cycles']). Use 'all' for every active category. Omit for summary-only mode. With scope, diagnostics run only when sections includes 'diagnostics' or 'all'; other categories are verified regardless of sections.",
    }),
  ),
  scope: Type.Optional(
    Type.Union([Type.String(), Type.Array(Type.String())], {
      description:
        "Restrict returned results to paths under this scope (one path, or an array of paths — not a space-separated list; file or directory; absolute or relative to project root). `scope=` narrows results. Scoped requests do no diagnostics work unless sections includes 'diagnostics' or 'all'; when requested, diagnostics collect scoped files and report coverage gaps.",
    }),
  ),
  offset: Type.Optional(
    Type.Integer({
      minimum: 0,
      maximum: Number.MAX_SAFE_INTEGER,
      default: 0,
      description:
        "Rows to skip independently in each drill-down list. Default 0; follow next_offset in each list envelope.",
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
  // PARTIAL is a completed result like FRESH (it carries the same wait
  // stamp); only its diagnostics are not authoritative.
  const completed = kind === "FRESH" || kind === "PARTIAL";
  const phaseSource = completed ? (waitStamp ?? response) : response;
  const phases = parseInspectPhaseEntries(
    phaseSource.phases ?? response.completed_phases ?? response.completedPhases,
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
export function renderInspectCall(args: unknown, theme: Theme, context: RenderContextLike) {
  const safeArgs = asRecordOrEmpty(args);
  const sectionsValue = safeArgs.sections;
  const scopeValue = safeArgs.scope;
  const sections = Array.isArray(sectionsValue)
    ? `${sectionsValue.filter((section) => typeof section === "string").length} sections`
    : asString(sectionsValue);
  const scope = Array.isArray(scopeValue)
    ? `${scopeValue.filter((entry) => typeof entry === "string").length} scopes`
    : asString(scopeValue);
  const topK = asNumber(safeArgs.topK);
  const summary = [sections, scope, topK ? `topK=${topK}` : undefined].filter(Boolean).join(" ");
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
      "Codebase health inspection that waits for current analysis. FRESH means the reported analysis is current. PARTIAL means some diagnostics are unknown; the header names the analyzer, reason, and retry guidance. INTERRUPTED means the request stopped without a fresh snapshot; retry aft_inspect. PHASE-FAILED means inspection could not finish; address the reported reason and retry, or narrow the scope. `sections` selects drill-down detail. Scoped requests skip diagnostics unless sections includes 'diagnostics' or 'all'; unscoped requests keep warm diagnostics. Categories switched off in `inspect.categories` are not computed or refreshed, render as off, and never make the header PARTIAL.\n\n" +
      "Use `scope=` to narrow all findings, counts, and examples to those paths. Cross-boundary duplicates are labeled as groups touching the scope. Scope also limits Rust analyzer startup to the Cargo workspaces owning those paths when diagnostics are requested. Files without an authoritative diagnostic report remain named gaps (complete: false), not a clean result.\n\n" +
      "Use when: starting work on unfamiliar code, after multi-edit batches to check diagnostics, before a refactor, before review, or to verify cleanup completeness.\n\n" +
      "Treat `dead_code` as a hint, not proof: reachability is call-based, so symbols reached only via method dispatch or referenced only in type position may be false positives — verify before deleting.\n\n" +
      "When a list is cut, the reply ends with `shown N of M <unit> (<reason>) · narrow: <knobs>`; absence of that line means the list is complete.",
    parameters: InspectParams,
    async execute(_toolCallId, params: Static<typeof InspectParams>, _signal, _onUpdate, extCtx) {
      const bridge = bridgeFor(ctx, extCtx.cwd);
      const sections = normalizeStringOrArray(
        coerceJsonCollectionParam(params.sections, "sections"),
      );
      const scope = await resolveAndGateScope(extCtx, ctx, normalizeStringOrArray(params.scope));
      const topK = validateOptionalTopK(params.topK);
      const rawArgs: Record<string, unknown> = {};
      if (sections !== undefined) rawArgs.sections = sections;
      if (scope !== undefined) rawArgs.scope = scope;
      if (topK !== undefined) rawArgs.topK = topK;
      if (params.offset !== undefined) rawArgs.offset = params.offset;
      const response = await callToolCall(bridge, "inspect", rawArgs, extCtx, {
        transportTimeoutMs:
          resolveInspectDiagnosticsTimeoutMs(ctx.config) + INSPECT_TRANSPORT_HEADROOM_MS,
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
