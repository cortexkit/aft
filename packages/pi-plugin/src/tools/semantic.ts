/**
 * aft_search — unified code search.
 * Registered unless `aft_search` is in disabled_tools; index readiness and
 * backend availability are reported by the engine at call time.
 */

import { homedir } from "node:os";
import { isAbsolute, resolve } from "node:path";
import { canonicalizeProjectRoot, coerceBoolean } from "@cortexkit/aft-bridge";
import type { AgentToolResult, ExtensionAPI, Theme } from "@earendil-works/pi-coding-agent";
import { type Static, Type } from "typebox";

import type { PluginContext } from "../types.js";
import {
  bridgeFor,
  callToolCall,
  isEmptyParam,
  resolveSessionId,
  textResult,
  withPathAliasPreparation,
} from "./_shared.js";
import {
  asNumber,
  asRecord,
  asRecordOrEmpty,
  asRecords,
  asString,
  collapsibleResult,
  extractStructuredPayload,
  groupByFile,
  type RenderContextLike,
  type RenderResultOptionsLike,
  renderErrorResult,
  renderSections,
  renderToolCall,
  shortenPath,
} from "./render-helpers.js";

/** Search-status note for an answer read from files directly, with no index. */
const FILESYSTEM_SCAN_STATUS = "answered by a filesystem scan";

function semanticHonestyNote(response: Record<string, unknown>, theme: Theme): string | undefined {
  const notes: string[] = [];
  if (response.more_available === true) notes.push("more results available");
  if (response.engine_capped === true) notes.push("enumeration capped");
  if (response.fully_degraded === true) notes.push(FILESYSTEM_SCAN_STATUS);
  if (response.complete === false) notes.push("partial/incomplete");
  return notes.length > 0 ? theme.fg("warning", `Search status: ${notes.join("; ")}.`) : undefined;
}

const GENERATION_CHANGED_DISCLOSURE = "index changed - order re-derived";

type SearchContinuityBySession = Map<string, Map<string, string>>;

function normalizeContinuityQuery(query: string): string {
  return query
    .trim()
    .split(/\s+/u)
    .join(" ")
    .replace(/[A-Z]/g, (letter) => letter.toLowerCase());
}

function selectedProjectRoot(cwd: string, pathArg: string | undefined): string {
  const target = pathArg?.trim();
  if (!target) return canonicalizeProjectRoot(cwd);
  const expanded =
    target === "~"
      ? homedir()
      : /^~[/\\]/u.test(target)
        ? resolve(homedir(), target.slice(2))
        : target;
  return canonicalizeProjectRoot(isAbsolute(expanded) ? expanded : resolve(cwd, expanded));
}

function continuityKey(
  projectRoot: string,
  query: string,
  includeTests: boolean | undefined,
  topK: number | undefined,
): string {
  return JSON.stringify([
    projectRoot,
    normalizeContinuityQuery(query),
    includeTests ?? false,
    topK ?? 10,
  ]);
}

function responseGeneration(response: Record<string, unknown>): string | undefined {
  const structured = asRecord(response.structuredContent) ?? asRecord(response.structured_content);
  const plan = asRecord(response.plan) ?? asRecord(structured?.plan);
  return asString(plan?.snapshot_generation) ?? asString(response.snapshot_generation);
}

function surfaceSearchText(
  response: Record<string, unknown>,
  continuity: SearchContinuityBySession,
  sessionId: string,
  key: string,
): string {
  const text = asString(response.text) ?? "";
  const generation = responseGeneration(response);
  if (generation === undefined) return text;

  let sessionContinuity = continuity.get(sessionId);
  if (!sessionContinuity) {
    sessionContinuity = new Map();
    continuity.set(sessionId, sessionContinuity);
  }
  const previous = sessionContinuity.get(key);
  sessionContinuity.set(key, generation);
  return previous !== undefined && previous !== generation
    ? `${GENERATION_CHANGED_DISCLOSURE}\n${text}`
    : text;
}

const SearchParams = Type.Object(
  {
    query: Type.Optional(
      Type.String({
        description:
          "Concept, regex, literal text, filename, or capability to find. Examples: 'fuzzy match with whitespace tolerance', '^export', 'Cargo.lock'. Optional when `pattern` is given.",
      }),
    ),
    pattern: Type.Optional(
      Type.String({
        description:
          "Regex (grep syntax, case-sensitive) for names or text that must appear, e.g. 'load_config|ConfigLoader'. Ranked with `query` when both are given.",
      }),
    ),
    topK: Type.Optional(
      Type.Integer({
        minimum: 1,
        maximum: 50,
        default: 10,
        description: "Maximum number of results (default: 10, max: 50)",
      }),
    ),
    offset: Type.Optional(
      Type.Integer({
        minimum: 0,
        maximum: 100000,
        default: 0,
        description: "Zero-based result offset (default: 0, max: 100000).",
      }),
    ),
    includeTests: Type.Optional(
      Type.Boolean({
        default: false,
        description:
          "Include test files (*.test.*, *_test.rs, __tests__/, …) plus test-support, fixture, mock, snapshot, and corpus files. Defaults to false.",
      }),
    ),
    path: Type.Optional(
      Type.String({
        description:
          "Only set this to search a different project or plain folder (absolute or ~ path). Omit it for the current configured workspace, including non-Git workspace roots; this is not a subdirectory filter. Unindexed external folders use a bounded lexical fallback; filesystem roots, home and its ancestors are refused.",
      }),
    ),
  },
  // No explicit additionalProperties: JSON Schema tolerates unknown keys by
  // default, which is what lets a legacy `hint` argument arrive and be
  // ignored — and the registration-parity projection stays byte-identical
  // with the OpenCode schema.
);

/** Exported for renderer unit tests. */
export function buildSemanticSections(args: unknown, payload: unknown, theme: Theme): string[] {
  const safeArgs = asRecordOrEmpty(args);
  const query = asString(safeArgs.query);
  const pattern = asString(safeArgs.pattern);
  const topK = asNumber(safeArgs.topK) ?? 10;
  const response = asRecord(payload);
  if (!response) return [theme.fg("muted", "No search result.")];

  const status = asString(response.status) ?? "unknown";
  const semanticStatus = asString(response.semantic_status) ?? status;
  const interpretedAs = asString(response.interpreted_as) ?? "unknown";
  const queryKind = asString(response.query_kind);
  const responseText = asString(response.text);
  const hasGenerationDisclosure =
    responseText === GENERATION_CHANGED_DISCLOSURE ||
    responseText?.startsWith(`${GENERATION_CHANGED_DISCLOSURE}\n`) === true;
  const backendText = hasGenerationDisclosure
    ? responseText?.slice(GENERATION_CHANGED_DISCLOSURE.length + 1)
    : responseText;
  const sections = [
    ...(hasGenerationDisclosure ? [theme.fg("warning", GENERATION_CHANGED_DISCLOSURE)] : []),
    `${theme.fg(semanticStatus === "ready" ? "success" : "warning", `semantic: ${semanticStatus}`)} ${theme.fg("muted", `mode=${interpretedAs}${queryKind ? ` kind=${queryKind}` : ""}${query !== undefined ? ` query=${JSON.stringify(query)}` : ""}${pattern !== undefined ? ` pattern=${JSON.stringify(pattern)}` : ""} topK=${topK}`)}`,
  ];

  const warnings = Array.isArray(response.warnings)
    ? response.warnings.filter((warning): warning is string => typeof warning === "string")
    : [];
  if (warnings.length > 0) {
    sections.push(warnings.map((warning) => theme.fg("warning", `⚠ ${warning}`)).join("\n"));
  }

  const honestyNote = semanticHonestyNote(response, theme);
  if (honestyNote) sections.push(honestyNote);

  // An identifier searched in another project is answered by an exact sweep
  // whose coverage (complete, or stopped at its time limit) the agent text
  // ends with; the human view shows the same sentence.
  const sweepCoverage = asString(asRecord(response.exact_sweep)?.coverage);

  const results = asRecords(response.results);
  if (status !== "ready" && results.length === 0) {
    sections.push(backendText ?? theme.fg("muted", "Semantic index is not ready."));
    return sections;
  }

  if (results.length === 0) {
    sections.push(theme.fg("muted", "No matches found."));
    if (sweepCoverage) sections.push(theme.fg("muted", sweepCoverage));
    return sections;
  }

  const grouped = groupByFile(results, (result) => asString(result.file));
  for (const [file, fileResults] of grouped.entries()) {
    const lines = [theme.fg("accent", shortenPath(file))];
    fileResults.forEach((result) => {
      if (asString(result.kind) === "GrepLine") {
        const line = asNumber(result.line);
        const column = asNumber(result.column);
        const lineText = asString(result.line_text) ?? "";
        const location =
          line !== undefined ? `${line}${column !== undefined ? `:${column}` : ""}` : "?";
        // Sweep lines say whether they hold the identifier itself or only a
        // spelling variant of it; plain regex and literal lines carry no
        // `exact` field and get no marker.
        const marker =
          result.exact === true
            ? ` ${theme.fg("muted", "[exact]")}`
            : result.exact === false
              ? ` ${theme.fg("muted", `[variant: ${asString(result.match_text) ?? "?"}]`)}`
              : "";
        lines.push(`  ↳ ${theme.fg("muted", `line ${location}`)}${marker} ${lineText}`);
        return;
      }

      const score = asNumber(result.score);
      const source = asString(result.source);
      const kind = asString(result.kind) ?? "symbol";
      const location = asString(result.location);
      if (source === "lexical") {
        lines.push(
          `  ↳ ${theme.fg("muted", `[lexical match${score !== undefined ? ` — score ${score.toFixed(3)}` : ""}]`)}`,
        );
        const snippet = asString(result.snippet);
        if (snippet) {
          lines.push(...snippet.split("\n").map((line) => `     ${line}`));
        }
        return;
      }
      if (kind === "file_summary" || location === "[file summary]") {
        const summary = asString(result.snippet) ?? asString(result.name) ?? "(no summary)";
        lines.push(
          `  ↳ ${summary} ${theme.fg("muted", `[file summary${score !== undefined ? ` score ${score.toFixed(3)}` : ""}]`)}`,
        );
        return;
      }
      const startLine = asNumber(result.start_line);
      const endLine = asNumber(result.end_line);
      const range =
        startLine !== undefined
          ? `${startLine}${endLine && endLine !== startLine ? `-${endLine}` : ""}`
          : "?";
      const name = asString(result.name) ?? "(unknown)";
      lines.push(
        `  ↳ ${name} ${theme.fg("muted", `[${kind}] lines ${range}${score !== undefined ? ` score ${score.toFixed(3)}` : ""}`)}`,
      );
      const snippet = asString(result.snippet);
      if (snippet) {
        lines.push(...snippet.split("\n").map((line) => `     ${line}`));
      }
    });
    sections.push(lines.join("\n"));
  }
  if (sweepCoverage) sections.push(theme.fg("muted", sweepCoverage));

  return sections;
}

/** Exported for renderer unit tests. */
export function renderSemanticCall(args: unknown, theme: Theme, context: RenderContextLike) {
  const safeArgs = asRecordOrEmpty(args);
  const query = asString(safeArgs.query);
  const pattern = asString(safeArgs.pattern);
  return renderToolCall(
    "search",
    theme.fg("toolOutput", query ?? (pattern ? `pattern ${pattern}` : "")),
    theme,
    context,
  );
}

/** Exported for renderer unit tests. */
export function renderSemanticResult(
  result: AgentToolResult<unknown>,
  args: Static<typeof SearchParams>,
  theme: Theme,
  context: RenderContextLike,
  options: RenderResultOptionsLike = { expanded: true },
) {
  if (context.isError) return renderErrorResult(result, "search failed", theme, context);
  const sections = buildSemanticSections(args, extractStructuredPayload(result), theme);
  return collapsibleResult({
    summary: sections[0] ?? "search completed",
    full: renderSections(sections, context),
    expanded: options.expanded,
    context,
  });
}

export function registerSemanticTool(pi: ExtensionAPI, ctx: PluginContext): void {
  const continuity: SearchContinuityBySession = new Map();
  pi.registerTool(
    withPathAliasPreparation({
      name: "aft_search",
      label: "search",
      // Lean and positive on purpose (parity with OpenCode): this is the
      // primary code-search tool, so the description must not push agents
      // elsewhere. The old "When NOT to use: ... use grep directly" line fed
      // the exact bash-grep reflex the system prompt works to suppress.
      description: [
        "Search code with one tool: concepts, identifiers, error strings, regex, literals, and filenames are auto-routed to the right engine and returned ranked. For conceptual 'how does X work' queries, phrase a full natural-language sentence — the semantic lane is NL-aware and matches intent against docstrings and comments ('how does the ORM build and execute a query', 'where is rate limiting handled'), not just keywords. Exact names, strings, and regex stay terse ('^export', 'Cargo.lock').",
        "When you know a name or string that must appear, put it in `pattern` (a regex, grep syntax, case-sensitive) and keep the question in `query`: both are ranked together, and the reply's first line says how many files the pattern matched and where it is defined. Example: query 'how is the config file loaded', pattern 'load_config|ConfigLoader'. `pattern` alone ranks the files it matches, definitions first.",
        "Use `offset` to continue a ranked result list from a zero-based position.",
        "When a list is cut, the reply ends with `shown N of M <unit> (<reason>) · narrow: <knobs>`; absence of that line means the list is complete.",
      ].join("\n\n"),
      parameters: SearchParams,
      async execute(
        _toolCallId: string,
        params: Static<typeof SearchParams>,
        signal,
        _onUpdate,
        extCtx,
      ) {
        const rawPattern: unknown = params.pattern;
        const hasQuery = typeof params.query === "string" && params.query.trim().length > 0;
        // A blank pattern counts as absent; a real one is sent as written,
        // because whitespace inside a regex is part of what it matches.
        if (!isEmptyParam(rawPattern) && typeof rawPattern !== "string") {
          throw new Error("semantic_search: invalid params: `pattern` must be a string");
        }
        const pattern =
          typeof rawPattern === "string" && rawPattern.trim().length > 0 ? rawPattern : undefined;
        if (!hasQuery && pattern === undefined) {
          throw new Error(
            rawPattern === undefined || rawPattern === null
              ? "semantic_search: invalid params: `query` must be a non-empty string"
              : "semantic_search: invalid params: at least one of `query` or `pattern` must be a non-empty string",
          );
        }
        const query = hasQuery ? (params.query as string) : "";
        // The continuity key of a query-only search is unchanged; a pattern
        // makes a different search, so it gets its own key.
        const continuitySubject =
          pattern === undefined ? query : `${query}\u0000pattern:${pattern}`;

        const bridge = bridgeFor(ctx, extCtx.cwd);
        const req: Record<string, unknown> = {};
        if (hasQuery) req.query = query;
        if (pattern !== undefined) req.pattern = pattern;
        const includeTests = !isEmptyParam(params.includeTests)
          ? coerceBoolean(params.includeTests)
          : undefined;
        if (params.topK !== undefined) req.topK = params.topK;
        if (params.offset !== undefined) req.offset = params.offset;
        if (includeTests !== undefined) req.includeTests = includeTests;
        if (params.path !== undefined) req.path = params.path;
        const response = await callToolCall(
          bridge,
          "search",
          req,
          extCtx,
          signal ? { abortSignal: signal } : undefined,
        );
        if (response.success === false) {
          throw new Error(response.text || response.message || "search failed");
        }
        const text = surfaceSearchText(
          response,
          continuity,
          resolveSessionId(extCtx) ?? "unknown-session",
          continuityKey(
            selectedProjectRoot(extCtx.cwd, params.path),
            continuitySubject,
            includeTests,
            params.topK,
          ),
        );
        return textResult(text, text === response.text ? response : { ...response, text });
      },
      renderCall(args, theme, context) {
        return renderSemanticCall(args, theme, context);
      },
      renderResult(result, options = { expanded: false, isPartial: false }, theme, context) {
        return renderSemanticResult(result, context.args, theme, context, options);
      },
    }),
  );
}
