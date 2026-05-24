/**
 * aft_import — language-aware import add/remove/organize.
 * Supports TS, JS, TSX, Python, Rust, Go.
 */

import { StringEnum } from "@earendil-works/pi-ai";
import type { AgentToolResult, ExtensionAPI, Theme } from "@earendil-works/pi-coding-agent";
import { type Static, Type } from "typebox";
import type { PluginContext } from "../types.js";
import { bridgeFor, callBridge, textResult } from "./_shared.js";
import {
  accentPath,
  asNumber,
  asRecord,
  asRecords,
  asString,
  extractStructuredPayload,
  type RenderContextLike,
  renderErrorResult,
  renderSections,
  renderToolCall,
} from "./render-helpers.js";

const ImportParams = Type.Object({
  op: StringEnum(["add", "remove", "organize"] as const, { description: "Import operation" }),
  filePath: Type.String({ description: "Path to the file" }),
  module: Type.Optional(
    Type.String({ description: "Module path (required for add/remove), e.g. 'react', './utils'" }),
  ),
  names: Type.Optional(
    Type.Array(Type.String(), { description: "Named imports to add, e.g. ['useState']" }),
  ),
  defaultImport: Type.Optional(Type.String({ description: "Default import name (e.g. 'React')" })),
  removeName: Type.Optional(
    Type.String({ description: "Named import to remove; omit to remove entire import" }),
  ),
  typeOnly: Type.Optional(Type.Boolean({ description: "Type-only import (TS only)" })),
  validate: Type.Optional(
    StringEnum(["syntax", "full"] as const, {
      description: "Post-edit validation level (default: syntax)",
    }),
  ),
});

/** Exported for renderer unit tests. */
export function buildImportSections(
  args: Static<typeof ImportParams>,
  payload: unknown,
  theme: Theme,
): string[] {
  const response = asRecord(payload);
  if (!response) return [theme.fg("muted", "No import result.")];

  if (args.op === "organize") {
    const groups = asRecords(response.groups);
    const groupText =
      groups.length > 0
        ? groups
            .map((group) => `${asString(group.name) ?? "unknown"}: ${asNumber(group.count) ?? 0}`)
            .join(" · ")
        : "No imports found";
    return [
      `${theme.fg("success", "organized")} ${theme.fg("accent", asString(response.file) ?? args.filePath)}`,
      `${theme.fg("muted", "groups")} ${groupText}`,
      `${theme.fg("muted", "duplicates removed")} ${asNumber(response.removed_duplicates) ?? 0}`,
    ];
  }

  if (args.op === "add") {
    const moduleName = asString(response.module) ?? args.module ?? "(module)";
    const status =
      response.already_present === true
        ? theme.fg("warning", "already present")
        : theme.fg("success", "added");
    return [
      `${status} ${theme.fg("accent", moduleName)}`,
      `${theme.fg("muted", "file")} ${theme.fg("accent", asString(response.file) ?? args.filePath)}`,
      `${theme.fg("muted", "group")} ${asString(response.group) ?? "—"}`,
    ];
  }

  return [
    `${theme.fg("success", "removed")} ${theme.fg("accent", asString(response.module) ?? args.module ?? "(module)")}`,
    `${theme.fg("muted", "file")} ${theme.fg("accent", asString(response.file) ?? args.filePath)}`,
    args.removeName
      ? `${theme.fg("muted", "name")} ${args.removeName}`
      : `${theme.fg("muted", "scope")} entire import`,
  ];
}

/** Exported for renderer unit tests. */
export function renderImportCall(
  args: Static<typeof ImportParams>,
  theme: Theme,
  context: RenderContextLike,
) {
  const summary = [
    theme.fg("accent", args.op),
    accentPath(theme, args.filePath),
    args.module ? theme.fg("toolOutput", args.module) : undefined,
  ]
    .filter(Boolean)
    .join(" ");
  return renderToolCall("import", summary, theme, context);
}

/** Exported for renderer unit tests. */
export function renderImportResult(
  result: AgentToolResult<unknown>,
  args: Static<typeof ImportParams>,
  theme: Theme,
  context: RenderContextLike,
) {
  if (context.isError) return renderErrorResult(result, "import failed", theme, context);
  const payload = extractStructuredPayload(result);
  return renderSections(buildImportSections(args, payload, theme), context);
}

export function registerImportTools(pi: ExtensionAPI, ctx: PluginContext): void {
  pi.registerTool({
    name: "aft_import",
    label: "import",
    description:
      "Language-aware import management. Supports TS, JS, TSX, Python, Rust, Go. Ops: `add`, `remove`, `organize`. Use aft_safety checkpoint/undo before broad cleanup.",
    parameters: ImportParams,
    async execute(
      _toolCallId: string,
      params: Static<typeof ImportParams>,
      _signal,
      _onUpdate,
      extCtx,
    ) {
      if ((params.op === "add" || params.op === "remove") && !params.module) {
        throw new Error(`op='${params.op}' requires 'module'`);
      }
      const bridge = bridgeFor(ctx, extCtx.cwd);
      const commandMap: Record<string, string> = {
        add: "add_import",
        remove: "remove_import",
        organize: "organize_imports",
      };
      const req: Record<string, unknown> = { file: params.filePath };
      if (params.module !== undefined) req.module = params.module;
      if (params.names !== undefined) req.names = params.names;
      if (params.defaultImport !== undefined) req.default_import = params.defaultImport;
      if (params.removeName !== undefined) req.name = params.removeName;
      if (params.typeOnly !== undefined) req.type_only = params.typeOnly;
      if (params.validate !== undefined) req.validate = params.validate;

      const response = await callBridge(bridge, commandMap[params.op], req, extCtx);
      return textResult(JSON.stringify(response, null, 2));
    },
    renderCall(args, theme, context) {
      return renderImportCall(args, theme, context);
    },
    renderResult(result, _options, theme, context) {
      return renderImportResult(result, context.args, theme, context);
    },
  });
}
