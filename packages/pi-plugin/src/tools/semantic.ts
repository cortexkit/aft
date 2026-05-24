/**
 * aft_search — semantic (embedding-based) code search.
 * Only registered when config.semantic_search is enabled AND
 * the ONNX runtime / configured backend is available.
 */

import type { AgentToolResult, ExtensionAPI, Theme } from "@earendil-works/pi-coding-agent";
import { type Static, Type } from "typebox";
import type { PluginContext } from "../types.js";
import { bridgeFor, callBridge, textResult } from "./_shared.js";
import {
  asNumber,
  asRecord,
  asRecords,
  asString,
  extractStructuredPayload,
  groupByFile,
  type RenderContextLike,
  renderErrorResult,
  renderSections,
  renderToolCall,
  shortenPath,
} from "./render-helpers.js";

const SearchParams = Type.Object({
  query: Type.String({
    description:
      "Concept or capability to find, phrased as a programmer would describe the code. Examples: 'fuzzy match with whitespace tolerance', 'undo backup before edit', 'retry failed network request'.",
  }),
  topK: Type.Optional(
    Type.Number({ description: "Maximum number of results (default: 10, max: 100)" }),
  ),
});

/** Exported for renderer unit tests. */
export function buildSemanticSections(
  args: Static<typeof SearchParams>,
  payload: unknown,
  theme: Theme,
): string[] {
  const response = asRecord(payload);
  if (!response) return [theme.fg("muted", "No semantic search result.")];

  const status = asString(response.status) ?? "unknown";
  const sections = [
    `${theme.fg(status === "ready" ? "success" : "warning", `index: ${status}`)} ${theme.fg("muted", `query=${JSON.stringify(args.query)} topK=${args.topK ?? 10}`)}`,
  ];

  if (status !== "ready") {
    sections.push(asString(response.text) ?? theme.fg("muted", "Semantic index is not ready."));
    return sections;
  }

  const results = asRecords(response.results);
  if (results.length === 0) {
    sections.push(theme.fg("muted", "No semantic matches found."));
    return sections;
  }

  const grouped = groupByFile(results, (result) => asString(result.file));
  for (const [file, fileResults] of grouped.entries()) {
    const lines = [theme.fg("accent", shortenPath(file))];
    fileResults.forEach((result) => {
      const score = asNumber(result.score);
      const source = asString(result.source);
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
      const startLine = asNumber(result.start_line);
      const endLine = asNumber(result.end_line);
      const range =
        startLine !== undefined
          ? `${startLine}${endLine && endLine !== startLine ? `-${endLine}` : ""}`
          : "?";
      const kind = asString(result.kind) ?? "symbol";
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

  return sections;
}

/** Exported for renderer unit tests. */
export function renderSemanticCall(
  args: Static<typeof SearchParams>,
  theme: Theme,
  context: RenderContextLike,
) {
  return renderToolCall("semantic search", theme.fg("toolOutput", args.query), theme, context);
}

/** Exported for renderer unit tests. */
export function renderSemanticResult(
  result: AgentToolResult<unknown>,
  args: Static<typeof SearchParams>,
  theme: Theme,
  context: RenderContextLike,
) {
  if (context.isError) return renderErrorResult(result, "semantic search failed", theme, context);
  return renderSections(
    buildSemanticSections(args, extractStructuredPayload(result), theme),
    context,
  );
}

export function registerSemanticTool(pi: ExtensionAPI, ctx: PluginContext): void {
  pi.registerTool({
    name: "aft_search",
    label: "semantic search",
    description: [
      "Find symbols by concept using hybrid semantic + lexical search. Returns ranked code matches with similarity scores and provenance tags.",
      "",
      "When to reach for it:",
      "- Exploring an unfamiliar area: 'where is rate limiting handled', 'how does auth flow work'",
      "- Concept doesn't appear as a literal string: 'retry logic', 'cache invalidation', 'graceful shutdown'",
      "- Filename-shaped concepts: 'the bridge spawn helper', 'the session detection module'",
      "- After 2+ grep attempts that came back empty or noisy",
      "- You know roughly what the function does but not what it's named",
      "",
      "When NOT to use:",
      "- You have an error message or stack trace → use grep",
      "- You want the file/module structure → use aft_outline",
      "- You're following a call chain → use aft_navigate",
      "",
      "Each result tags `source` as one of: 'semantic' (embedding match only), 'lexical' (trigram exact-token match the embedding lane missed), or 'hybrid' (both lanes agreed — strongest signal).",
    ].join("\n"),
    parameters: SearchParams,
    async execute(
      _toolCallId: string,
      params: Static<typeof SearchParams>,
      _signal,
      _onUpdate,
      extCtx,
    ) {
      const bridge = bridgeFor(ctx, extCtx.cwd);
      const req: Record<string, unknown> = { query: params.query };
      if (params.topK !== undefined) req.top_k = params.topK;
      const response = await callBridge(bridge, "semantic_search", req, extCtx);
      return textResult((response.text as string | undefined) ?? JSON.stringify(response, null, 2));
    },
    renderCall(args, theme, context) {
      return renderSemanticCall(args, theme, context);
    },
    renderResult(result, _options, theme, context) {
      return renderSemanticResult(result, context.args, theme, context);
    },
  });
}
