/**
 * aft_import — language-aware import add/remove/organize.
 * Supports TS, JS, TSX, Python, Rust, Go, Solidity, Java, C#, PHP, Kotlin, Scala, Swift, Ruby, Lua, C, C++, Perl, Vue.
 */

import { StringEnum } from "@earendil-works/pi-ai";
import type { AgentToolResult, ExtensionAPI, Theme } from "@earendil-works/pi-coding-agent";
import { type Static, Type } from "typebox";
import type { PluginContext } from "../types.js";
import {
  bridgeFor,
  callToolCall,
  isEmptyParam,
  textResult,
  withPathAliasPreparation,
} from "./_shared.js";
import { assertExternalDirectoryPermission, resolvePathArg } from "./hoisted.js";
import {
  accentPath,
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

const ImportParams = Type.Object({
  op: StringEnum(["add", "remove", "organize"] as const, { description: "Import operation" }),
  path: Type.String({
    description: "Path to the file (absolute or relative to project root)",
  }),
  module: Type.Optional(
    Type.String({ description: "Module path (required for add/remove), e.g. 'react', './utils'" }),
  ),
  names: Type.Optional(
    Type.Array(Type.String(), {
      description:
        "Named imports to add, using native named-import text with per-name `as` aliasing where supported, e.g. ['useState'], Solidity ['ERC20', 'IERC20 as IToken']",
    }),
  ),
  defaultImport: Type.Optional(
    Type.String({ description: "Default import name, ES only (e.g. 'React')" }),
  ),
  namespace: Type.Optional(
    Type.String({
      description:
        "Namespace binding: `import * as ns from 'mod'` (ES), `* as N from \"./X.sol\"` (Solidity)",
    }),
  ),
  alias: Type.Optional(
    Type.String({ description: 'Whole-module alias. Solidity: `import "./X.sol" as X`' }),
  ),
  modifiers: Type.Optional(
    Type.Array(Type.String(), {
      description:
        "Statement-level modifiers, language-validated: Java/C# 'static', C# 'global'/'unsafe', Java/Kotlin/Scala 'wildcard', Swift '@testable'",
    }),
  ),
  importKind: Type.Optional(
    Type.String({
      description:
        "Symbol-kind import: PHP 'function'/'const', Swift 'struct'/'class'/'enum', Scala 'given'",
    }),
  ),
  removeName: Type.Optional(
    Type.String({ description: "Named import to remove; omit to remove entire import" }),
  ),
  typeOnly: Type.Optional(Type.Boolean({ description: "Type-only import (TS only)" })),
  // Pi's StringEnum returns plain JSON; Type.Optional needs a TypeBox schema
  // here to keep its optional marker out of the serialized tool parameters.
  validate: Type.Optional(
    Type.String({
      enum: ["syntax", "full"],
      description: "Post-edit validation level (default: syntax)",
    }),
  ),
});

/** Exported for renderer unit tests. */
export function buildImportSections(args: unknown, payload: unknown, theme: Theme): string[] {
  const safeArgs = asRecordOrEmpty(args);
  const op = asString(safeArgs.op);
  const path = asString(safeArgs.path);
  const moduleNameArg = asString(safeArgs.module);
  const response = asRecord(payload);
  if (!response) return [theme.fg("muted", "No import result.")];

  if (op === "organize") {
    const groups = asRecords(response.groups);
    const groupText =
      groups.length > 0
        ? groups
            .map((group) => `${asString(group.name) ?? "unknown"}: ${asNumber(group.count) ?? 0}`)
            .join(" · ")
        : "No imports found";
    return [
      `${theme.fg("success", "organized")} ${theme.fg("accent", asString(response.file) ?? path ?? "")}`,
      `${theme.fg("muted", "groups")} ${groupText}`,
      `${theme.fg("muted", "duplicates removed")} ${asNumber(response.removed_duplicates) ?? 0}`,
    ];
  }

  if (op === "add") {
    const moduleName = asString(response.module) ?? moduleNameArg ?? "(module)";
    const status =
      response.already_present === true
        ? theme.fg("warning", "already present")
        : theme.fg("success", "added");
    return [
      `${status} ${theme.fg("accent", moduleName)}`,
      `${theme.fg("muted", "file")} ${theme.fg("accent", asString(response.file) ?? path ?? "")}`,
      `${theme.fg("muted", "group")} ${asString(response.group) ?? "—"}`,
    ];
  }

  if (op !== "remove") {
    const partial = [
      asString(response.file) ?? path,
      asString(response.module) ?? moduleNameArg,
    ].filter((value): value is string => Boolean(value));
    return [
      theme.fg("muted", "Import result available."),
      ...partial.map((value) => theme.fg("accent", value)),
    ];
  }

  const moduleName = asString(response.module) ?? moduleNameArg ?? "(module)";
  const didRemove = response.removed !== false;
  const removeStatus = didRemove
    ? `${theme.fg("success", "removed")} ${theme.fg("accent", moduleName)}`
    : `${theme.fg("warning", "not present")} ${theme.fg("accent", moduleName)}`;
  return [
    removeStatus,
    `${theme.fg("muted", "file")} ${theme.fg("accent", asString(response.file) ?? path ?? "")}`,
    asString(safeArgs.removeName)
      ? `${theme.fg("muted", "name")} ${asString(safeArgs.removeName)}`
      : `${theme.fg("muted", "scope")} entire import`,
  ];
}

/** Exported for renderer unit tests. */
export function renderImportCall(args: unknown, theme: Theme, context: RenderContextLike) {
  const safeArgs = asRecordOrEmpty(args);
  const op = asString(safeArgs.op);
  const path = asString(safeArgs.path);
  const moduleName = asString(safeArgs.module);
  const summary = [
    op ? theme.fg("accent", op) : undefined,
    path ? accentPath(theme, path) : undefined,
    moduleName ? theme.fg("toolOutput", moduleName) : undefined,
  ]
    .filter(Boolean)
    .join(" ");
  return renderToolCall("import", summary, theme, context);
}

/** Exported for renderer unit tests. */
export function renderImportResult(
  result: AgentToolResult<unknown>,
  args: unknown,
  theme: Theme,
  context: RenderContextLike,
  options: RenderResultOptionsLike = { expanded: true },
) {
  if (context.isError) return renderErrorResult(result, "import failed", theme, context);
  const payload = extractStructuredPayload(result);
  const sections = buildImportSections(args, payload, theme);
  const op = asString(asRecordOrEmpty(args).op) ?? "import";
  return collapsibleResult({
    summary: sections[0] ?? `${op} import completed`,
    full: renderSections(sections, context),
    expanded: options.expanded,
    context,
  });
}

export function registerImportTools(pi: ExtensionAPI, ctx: PluginContext): void {
  const organizeRecovery =
    ctx.config.backup?.enabled === false
      ? "Backup capture is disabled by user config; review broad cleanup changes before proceeding."
      : "Use aft_safety checkpoint/undo before broad cleanup.";
  pi.registerTool(
    withPathAliasPreparation({
      name: "aft_import",
      label: "import",
      description: `Language-aware import management. Supports TS, JS, TSX, Python, Rust, Go, Solidity, Java, C#, PHP, Kotlin, Scala, Swift, Ruby, Lua, C, C++, Perl, Vue. Ops: \`add\`, \`remove\`, \`organize\`. ${organizeRecovery}`,
      parameters: ImportParams,
      async execute(
        _toolCallId: string,
        params: Static<typeof ImportParams>,
        _signal,
        _onUpdate,
        extCtx,
      ) {
        if ((params.op === "add" || params.op === "remove") && isEmptyParam(params.module)) {
          throw new Error(`op='${params.op}' requires 'module'`);
        }
        const filePath = await resolvePathArg(extCtx.cwd, params.path as string);
        await assertExternalDirectoryPermission(extCtx, filePath, {
          restrictToProjectRoot: ctx.config.restrict_to_project_root ?? false,
        });
        const bridge = bridgeFor(ctx, extCtx.cwd);
        const rawArgs: Record<string, unknown> = { op: params.op, filePath };
        if (params.module !== undefined) rawArgs.module = params.module;
        if (params.names !== undefined) rawArgs.names = params.names;
        if (params.defaultImport !== undefined) rawArgs.defaultImport = params.defaultImport;
        if (params.namespace !== undefined) rawArgs.namespace = params.namespace;
        if (params.alias !== undefined) rawArgs.alias = params.alias;
        if (params.modifiers !== undefined) rawArgs.modifiers = params.modifiers;
        if (params.importKind !== undefined) rawArgs.importKind = params.importKind;
        if (params.removeName !== undefined) rawArgs.removeName = params.removeName;
        if (params.typeOnly !== undefined) rawArgs.typeOnly = params.typeOnly;
        if (params.validate !== undefined) rawArgs.validate = params.validate;

        const response = await callToolCall(bridge, "import", rawArgs, extCtx);
        if (response.success === false) {
          throw new Error(response.text || response.message || `${params.op} failed`);
        }
        return textResult(response.text, response);
      },
      renderCall(args, theme, context) {
        return renderImportCall(args, theme, context);
      },
      renderResult(result, options = { expanded: false, isPartial: false }, theme, context) {
        return renderImportResult(result, context.args, theme, context, options);
      },
    }),
  );
}
