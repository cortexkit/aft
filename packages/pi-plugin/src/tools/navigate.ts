/**
 * aft_callgraph — call-graph relationship queries across files.
 * Ops: call_tree, callers, trace_to, trace_to_symbol, impact, trace_data.
 */

import {
  coerceBoolean,
  formatCallgraphSections,
  PLAIN_CALLGRAPH_THEME,
} from "@cortexkit/aft-bridge";
import { StringEnum } from "@earendil-works/pi-ai";
import type { AgentToolResult, ExtensionAPI, Theme } from "@earendil-works/pi-coding-agent";
import { type Static, Type } from "typebox";
import { toolEnabled } from "../config.js";
import type { PluginContext } from "../types.js";
import {
  bridgeFor,
  callToolCall,
  coerceOptionalInt,
  formatBridgeErrorMessage,
  isEmptyParam,
  optionalInt,
  textResult,
  withPathAliasPreparation,
} from "./_shared.js";
import { assertExternalDirectoryPermission, resolvePathArg } from "./hoisted.js";
import {
  accentPath,
  collapsibleResult,
  extractStructuredPayload,
  type RenderContextLike,
  type RenderResultOptionsLike,
  renderErrorResult,
  renderSections,
  renderToolCall,
} from "./render-helpers.js";

// Read-only navigation negatives that are legitimate answers, not failures:
// the symbol isn't defined here, or the store is still building. Returned as
// plain text (no red error), matching how grep-with-no-matches reads. Mirrors
// the OpenCode plugin's set. ("no path between symbols" is already a success
// response with reason=no_path_found, never an error code.)
const CALLGRAPH_SOFT_CODES = new Set(["symbol_not_found", "callgraph_building"]);

function navigateParamsSchema() {
  return Type.Object({
    op: StringEnum(
      ["call_tree", "callers", "trace_to", "trace_to_symbol", "impact", "trace_data"] as const,
      {
        description: "Navigation operation",
      },
    ),
    path: Type.String({
      description: "Source file containing the symbol (absolute or relative to project root)",
    }),
    symbol: Type.String({ description: "Name of the symbol to analyze" }),
    depth: optionalInt(1, Number.MAX_SAFE_INTEGER, "Maximum call-graph depth to traverse"),
    expression: Type.Optional(
      Type.String({ description: "Expression to track (required for trace_data)" }),
    ),
    toSymbol: Type.Optional(
      Type.String({
        description: "Target symbol for trace_to_symbol; the returned path ends here",
      }),
    ),
    toPath: Type.Optional(
      Type.String({
        description:
          "Optional target file for trace_to_symbol; required when toSymbol exists in multiple files",
      }),
    ),
    includeTests: Type.Optional(
      Type.Boolean({
        description: "Include test files in callers/paths. Defaults to false; tests are hidden.",
      }),
    ),
    includeUnresolved: Type.Optional(
      Type.Boolean({
        description:
          "Show every unresolved external/stdlib call individually. Defaults to false; unresolved leaf calls are collapsed into one summary per parent.",
      }),
    ),
  });
}

type NavigateArgs = Static<ReturnType<typeof navigateParamsSchema>>;

/** Exported for renderer unit tests. */
export function buildNavigateSections(
  args: NavigateArgs,
  payload: unknown,
  theme: Theme,
): string[] {
  const themeAdapter = {
    fg: (role: string, s: string) => theme.fg(role as Parameters<Theme["fg"]>[0], s),
  };
  return formatCallgraphSections(args.op, payload, themeAdapter, {
    includeUnresolved: coerceBoolean(args.includeUnresolved),
  });
}

/** Exported for renderer unit tests. */
export function renderNavigateCall(args: NavigateArgs, theme: Theme, context: RenderContextLike) {
  const summary = [
    theme.fg("accent", args.op),
    accentPath(theme, args.path),
    theme.fg("toolOutput", args.symbol),
    args.toSymbol ? theme.fg("toolOutput", `→ ${args.toSymbol}`) : undefined,
  ]
    .filter(Boolean)
    .join(" ");
  return renderToolCall("callgraph", summary, theme, context);
}

/** Exported for renderer unit tests. */
export function renderNavigateResult(
  result: AgentToolResult<unknown>,
  args: NavigateArgs,
  theme: Theme,
  context: RenderContextLike,
  options: RenderResultOptionsLike = { expanded: true },
) {
  if (context.isError) return renderErrorResult(result, "navigate failed", theme, context);
  const sections = buildNavigateSections(args, extractStructuredPayload(result), theme);
  return collapsibleResult({
    summary: sections[0] ?? `${args.op} completed`,
    full: renderSections(sections, context),
    expanded: options.expanded,
    context,
  });
}

export function registerNavigateTool(pi: ExtensionAPI, ctx: PluginContext): void {
  const zoomEnabled = toolEnabled(ctx.config, "aft_zoom");
  const navigateTool = withPathAliasPreparation({
    name: "aft_callgraph",
    label: "callgraph",
    description: `Answer code-relationship questions from a real call graph — instead of grep + read chains. Reach for this whenever the question is about how symbols connect. ${zoomEnabled ? "Use aft_zoom with `callgraph:true` for one-level forward calls-out while reading source; use aft_callgraph only for reverse callers or multi-level traces so you do not double-fetch the same relationships." : "Use aft_callgraph call_tree (depth 1) for one-level forward calls-out; use it for reverse callers or multi-level traces."} All ops require both \`path\` and \`symbol\`. Use \`callers\` for call sites (before renaming/signature changes), \`impact\` for blast radius (what breaks if a symbol changes), \`call_tree\` for what a function calls, \`trace_to\` for how execution reaches a symbol from entry points, \`trace_to_symbol\` for the shortest path from one symbol to another (requires \`toSymbol\`; if ambiguous, the error returns candidate files — retry with \`toPath\`), \`trace_data\` to follow a value across assignments/params. Markers: ~ = edge resolved by name only (may point at the wrong same-named symbol); [unresolved] = callee not resolved to a definition, so the location shown is the call site. Unmarked edges are resolved exactly. By default, unresolved external/stdlib leaf calls in call_tree are collapsed into one summary per parent; pass includeUnresolved=true to show every unresolved edge individually.`,
    parameters: navigateParamsSchema(),
    async execute(_toolCallId: string, params: NavigateArgs, _signal, _onUpdate, extCtx) {
      if (isEmptyParam(params.path)) {
        throw new Error(`op='${params.op}' requires a \`path\``);
      }
      if (isEmptyParam(params.symbol)) {
        throw new Error(`op='${params.op}' requires a \`symbol\``);
      }
      if (params.op === "trace_data" && isEmptyParam(params.expression)) {
        throw new Error("op='trace_data' requires an `expression`");
      }
      if (params.op === "trace_to_symbol" && isEmptyParam(params.toSymbol)) {
        throw new Error("op='trace_to_symbol' requires a `toSymbol`");
      }
      const filePath = await resolvePathArg(extCtx.cwd, params.path as string);
      const toFile = !isEmptyParam(params.toPath)
        ? await resolvePathArg(extCtx.cwd, params.toPath as string)
        : undefined;
      const checked = new Set<string>();
      for (const target of [filePath, ...(toFile !== undefined ? [toFile] : [])]) {
        if (checked.has(target)) continue;
        checked.add(target);
        await assertExternalDirectoryPermission(extCtx, target, {
          restrictToProjectRoot: ctx.config.restrict_to_project_root ?? false,
        });
      }

      const bridge = bridgeFor(ctx, extCtx.cwd);
      const rawArgs: Record<string, unknown> = {
        op: params.op,
        filePath: params.path,
        symbol: params.symbol,
      };
      const depth = coerceOptionalInt(params.depth, "depth", 1, Number.MAX_SAFE_INTEGER);
      if (depth !== undefined) rawArgs.depth = depth;
      if (!isEmptyParam(params.expression)) rawArgs.expression = params.expression;
      if (!isEmptyParam(params.toSymbol)) rawArgs.toSymbol = params.toSymbol;
      if (!isEmptyParam(params.toPath)) rawArgs.toFile = params.toPath;
      if (!isEmptyParam(params.includeTests))
        rawArgs.includeTests = coerceBoolean(params.includeTests);
      if (!isEmptyParam(params.includeUnresolved))
        rawArgs.includeUnresolved = coerceBoolean(params.includeUnresolved);
      const response = await callToolCall(bridge, "callgraph", rawArgs, extCtx);
      if (response.success === false) {
        const code = typeof response.code === "string" ? response.code : "";
        const text = response.text || formatBridgeErrorMessage(params.op, response, rawArgs);
        if (CALLGRAPH_SOFT_CODES.has(code)) {
          return textResult(text, response);
        }
        throw new Error(text || response.message || "callgraph failed");
      }
      return textResult(
        response.text ||
          formatCallgraphSections(params.op, response, PLAIN_CALLGRAPH_THEME, {
            includeUnresolved: coerceBoolean(params.includeUnresolved),
          }).join("\n"),
        response,
      );
    },
    renderCall(args, theme, context) {
      return renderNavigateCall(args, theme, context);
    },
    renderResult(result, options = { expanded: false, isPartial: false }, theme, context) {
      return renderNavigateResult(result, context.args, theme, context, options);
    },
  });
  if (!toolEnabled(ctx.config, "aft_zoom")) {
    navigateTool.description = navigateTool.description.replaceAll("aft_zoom", "read");
  }
  pi.registerTool(navigateTool);
}
