/**
 * aft_refactor — workspace-wide refactoring.
 * Ops: move (symbol across files), extract (lines → function), inline (call site).
 */

import { StringEnum } from "@earendil-works/pi-ai";
import type { AgentToolResult, ExtensionAPI, Theme } from "@earendil-works/pi-coding-agent";
import { type Static, Type } from "typebox";
import type { PluginContext } from "../types.js";
import {
  bridgeFor,
  callToolCall,
  coerceOptionalInt,
  isEmptyParam,
  optionalInt,
  textResult,
  withPathAliasPreparation,
} from "./_shared.js";
import { assertExternalDirectoryPermission, resolvePathArg } from "./hoisted.js";
import {
  accentPath,
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
  shortenPath,
} from "./render-helpers.js";

const RefactorParams = Type.Object({
  op: StringEnum(["move", "extract", "inline"] as const, { description: "Refactoring operation" }),
  path: Type.String({
    description: "Source file (absolute or relative to project root)",
  }),
  symbol: Type.Optional(Type.String({ description: "Symbol name (for move, inline)" })),
  destination: Type.Optional(Type.String({ description: "Target file (for move)" })),
  scope: Type.Optional(Type.String({ description: "Disambiguation scope for move op" })),
  name: Type.Optional(Type.String({ description: "New function name (for extract)" })),
  startLine: optionalInt(1, Number.MAX_SAFE_INTEGER, "1-based start line for extract"),
  endLine: optionalInt(1, Number.MAX_SAFE_INTEGER, "1-based end line (inclusive) for extract"),
  callSiteLine: optionalInt(1, Number.MAX_SAFE_INTEGER, "1-based call site line for inline"),
});

/** Exported for renderer unit tests. */
export function buildRefactorSections(
  args: Static<typeof RefactorParams>,
  payload: unknown,
  theme: Theme,
): string[] {
  const response = asRecord(payload);
  if (!response) return [theme.fg("muted", "No refactor result.")];

  if (args.op === "move") {
    const results = asRecords(response.results);
    return [
      `${theme.fg("success", "moved symbol")} ${theme.fg("toolOutput", args.symbol ?? "(symbol)")}`,
      `${theme.fg("muted", "files modified")} ${asNumber(response.files_modified) ?? results.length}`,
      `${theme.fg("muted", "consumers updated")} ${asNumber(response.consumers_updated) ?? 0}`,
      results.length > 0
        ? results
            .map((entry) => `  ↳ ${shortenPath(asString(entry.file) ?? "(unknown file)")}`)
            .join("\n")
        : theme.fg("muted", "No files reported."),
    ];
  }

  if (args.op === "extract") {
    return [
      `${theme.fg("success", "extracted")} ${theme.fg("toolOutput", asString(response.name) ?? args.name ?? "(function)")}`,
      `${theme.fg("muted", "file")} ${theme.fg("accent", shortenPath(asString(response.file) ?? args.path ?? ""))}`,
      `${theme.fg("muted", "params")} ${Array.isArray(response.parameters) ? response.parameters.join(", ") || "none" : "none"}`,
      `${theme.fg("muted", "return type")} ${asString(response.return_type) ?? "unknown"}`,
    ];
  }

  return [
    `${theme.fg("success", "inlined")} ${theme.fg("toolOutput", asString(response.symbol) ?? args.symbol ?? "(symbol)")}`,
    `${theme.fg("muted", "file")} ${theme.fg("accent", shortenPath(asString(response.file) ?? args.path ?? ""))}`,
    `${theme.fg("muted", "context")} ${asString(response.call_context) ?? "unknown"}`,
    `${theme.fg("muted", "substitutions")} ${asNumber(response.substitutions) ?? 0}`,
  ];
}

/** Exported for renderer unit tests. */
export function renderRefactorCall(
  args: Static<typeof RefactorParams>,
  theme: Theme,
  context: RenderContextLike,
) {
  const summary = [
    theme.fg("accent", args.op),
    accentPath(theme, args.path),
    args.symbol ? theme.fg("toolOutput", args.symbol) : undefined,
  ]
    .filter(Boolean)
    .join(" ");
  return renderToolCall("refactor", summary, theme, context);
}

/** Exported for renderer unit tests. */
export function renderRefactorResult(
  result: AgentToolResult<unknown>,
  args: Static<typeof RefactorParams>,
  theme: Theme,
  context: RenderContextLike,
  options: RenderResultOptionsLike = { expanded: true },
) {
  if (context.isError) return renderErrorResult(result, "refactor failed", theme, context);
  const sections = buildRefactorSections(args, extractStructuredPayload(result), theme);
  return collapsibleResult({
    summary: sections[0] ?? `${args.op} completed`,
    full: renderSections(sections, context),
    expanded: options.expanded,
    context,
  });
}

export function registerRefactorTool(pi: ExtensionAPI, ctx: PluginContext): void {
  pi.registerTool(
    withPathAliasPreparation({
      name: "aft_refactor",
      label: "refactor",
      description:
        "Workspace-wide refactoring that updates imports and references across files. `move` relocates a top-level symbol (not nested functions or class methods) to another file, rewriting imports workspace-wide; a checkpoint is created first. To move/rename a whole file, use aft_move. `extract` pulls a line range into a new function (TS/JS/TSX, Python). `inline` replaces a call with the function's body.",
      parameters: RefactorParams,
      async execute(
        _toolCallId: string,
        params: Static<typeof RefactorParams>,
        _signal,
        onUpdate,
        extCtx,
      ) {
        // Per-op required-field validation using isEmptyParam so empty strings
        // ("") sent by GPT-family models trigger the proper "required" error
        // instead of being passed through to Rust as a valid empty value.
        if ((params.op === "move" || params.op === "inline") && isEmptyParam(params.symbol)) {
          throw new Error(`'symbol' is required for '${params.op}' op`);
        }
        if (params.op === "move" && isEmptyParam(params.destination)) {
          throw new Error("'destination' is required for 'move' op");
        }
        if (params.op === "extract" && isEmptyParam(params.name)) {
          throw new Error("'name' is required for 'extract' op");
        }
        const startLine = coerceOptionalInt(
          params.startLine,
          "startLine",
          1,
          Number.MAX_SAFE_INTEGER,
        );
        const endLine = coerceOptionalInt(params.endLine, "endLine", 1, Number.MAX_SAFE_INTEGER);
        const callSiteLine = coerceOptionalInt(
          params.callSiteLine,
          "callSiteLine",
          1,
          Number.MAX_SAFE_INTEGER,
        );
        if (params.op === "extract") {
          if (startLine === undefined) throw new Error("'startLine' is required for 'extract' op");
          if (endLine === undefined) throw new Error("'endLine' is required for 'extract' op");
        }
        if (params.op === "inline" && callSiteLine === undefined) {
          throw new Error("'callSiteLine' is required for 'inline' op");
        }

        const filePath = await resolvePathArg(extCtx.cwd, params.path as string);
        const destination = !isEmptyParam(params.destination)
          ? await resolvePathArg(extCtx.cwd, params.destination as string)
          : undefined;
        const permissionTargets =
          params.op === "move" && destination !== undefined ? [filePath, destination] : [filePath];
        const checked = new Set<string>();
        for (const target of permissionTargets) {
          if (checked.has(target)) continue;
          checked.add(target);
          await assertExternalDirectoryPermission(extCtx, target, {
            restrictToProjectRoot: ctx.config.restrict_to_project_root ?? false,
          });
        }

        const bridge = bridgeFor(ctx, extCtx.cwd);
        const rawArgs: Record<string, unknown> = { op: params.op, filePath };
        // Use isEmptyParam everywhere so "" / [] / null don't slip through as
        // valid string params that Rust then has to deal with.
        if (!isEmptyParam(params.symbol)) rawArgs.symbol = params.symbol;
        if (destination !== undefined) rawArgs.destination = destination;
        if (!isEmptyParam(params.scope)) rawArgs.scope = params.scope;
        if (!isEmptyParam(params.name)) rawArgs.name = params.name;
        if (startLine !== undefined) rawArgs.startLine = startLine;
        if (endLine !== undefined) rawArgs.endLine = endLine;
        if (callSiteLine !== undefined) rawArgs.callSiteLine = callSiteLine;
        const response = await callToolCall(bridge, "refactor", rawArgs, extCtx, { onUpdate });
        if (response.success === false) {
          throw new Error(response.text || response.message || `${params.op} failed`);
        }
        return textResult(response.text, response);
      },
      renderCall(args, theme, context) {
        return renderRefactorCall(args, theme, context);
      },
      renderResult(result, options = { expanded: false, isPartial: false }, theme, context) {
        return renderRefactorResult(result, context.args, theme, context, options);
      },
    }),
  );
}
