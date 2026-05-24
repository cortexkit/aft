/**
 * aft_transform — scope-aware structural code transformations.
 * Ops: add_member, add_derive, wrap_try_catch, add_decorator, add_struct_tags.
 */

import { StringEnum } from "@earendil-works/pi-ai";
import type { AgentToolResult, ExtensionAPI, Theme } from "@earendil-works/pi-coding-agent";
import { type Static, Type } from "typebox";
import type { PluginContext } from "../types.js";
import { bridgeFor, callBridge, textResult } from "./_shared.js";
import {
  accentPath,
  asRecord,
  asString,
  extractStructuredPayload,
  type RenderContextLike,
  renderErrorResult,
  renderSections,
  renderToolCall,
} from "./render-helpers.js";

const TransformParams = Type.Object({
  op: StringEnum(
    ["add_member", "add_derive", "wrap_try_catch", "add_decorator", "add_struct_tags"] as const,
    { description: "Transformation operation" },
  ),
  filePath: Type.String({ description: "Path to the source file" }),
  container: Type.Optional(Type.String({ description: "Class/struct/impl name for add_member" })),
  code: Type.Optional(Type.String({ description: "Member code to insert (add_member)" })),
  target: Type.Optional(Type.String({ description: "Target symbol name" })),
  derives: Type.Optional(
    Type.Array(Type.String(), { description: "Derive macro names (add_derive)" }),
  ),
  catchBody: Type.Optional(
    Type.String({ description: "Catch block body (wrap_try_catch, default: 'throw error;')" }),
  ),
  decorator: Type.Optional(
    Type.String({ description: "Decorator text without @ (add_decorator)" }),
  ),
  field: Type.Optional(Type.String({ description: "Struct field name (add_struct_tags)" })),
  tag: Type.Optional(Type.String({ description: "Tag key (add_struct_tags)" })),
  value: Type.Optional(Type.String({ description: "Tag value (add_struct_tags)" })),
  position: Type.Optional(
    Type.String({
      description:
        "Position hint: 'first', 'last' (default), 'before:name', 'after:name' for add_member",
    }),
  ),
  validate: Type.Optional(
    StringEnum(["syntax", "full"] as const, {
      description: "Validation level (default: syntax)",
    }),
  ),
});

/** Exported for renderer unit tests. */
export function buildTransformSections(
  args: Static<typeof TransformParams>,
  payload: unknown,
  theme: Theme,
): string[] {
  const response = asRecord(payload);
  if (!response) return [theme.fg("muted", "No transform result.")];

  const target =
    asString(response.target) ??
    asString(response.scope) ??
    args.target ??
    args.container ??
    args.field ??
    args.filePath;

  return [
    `${theme.fg("success", "transformed")} ${theme.fg("accent", args.op)}`,
    `${theme.fg("muted", "file")} ${theme.fg("accent", asString(response.file) ?? args.filePath)}`,
    target ? `${theme.fg("muted", "target")} ${target}` : theme.fg("muted", "No target metadata."),
  ];
}

/** Exported for renderer unit tests. */
export function renderTransformCall(
  args: Static<typeof TransformParams>,
  theme: Theme,
  context: RenderContextLike,
) {
  const target = args.target ?? args.container ?? args.field;
  const summary = [
    theme.fg("accent", args.op),
    accentPath(theme, args.filePath),
    target ? theme.fg("toolOutput", target) : undefined,
  ]
    .filter(Boolean)
    .join(" ");
  return renderToolCall("transform", summary, theme, context);
}

/** Exported for renderer unit tests. */
export function renderTransformResult(
  result: AgentToolResult<unknown>,
  args: Static<typeof TransformParams>,
  theme: Theme,
  context: RenderContextLike,
) {
  if (context.isError) return renderErrorResult(result, "transform failed", theme, context);
  return renderSections(
    buildTransformSections(args, extractStructuredPayload(result), theme),
    context,
  );
}

export function registerStructureTool(pi: ExtensionAPI, ctx: PluginContext): void {
  pi.registerTool({
    name: "aft_transform",
    label: "transform",
    description:
      "Scope-aware structural code transformations with correct indentation. Use aft_safety checkpoint/undo before risky transforms.",
    parameters: TransformParams,
    async execute(
      _toolCallId: string,
      params: Static<typeof TransformParams>,
      _signal,
      _onUpdate,
      extCtx,
    ) {
      validateTransformParams(params);

      const bridge = bridgeFor(ctx, extCtx.cwd);
      // Rust dispatch accepts the op name directly (add_member, add_derive, etc.)
      const req: Record<string, unknown> = { file: params.filePath };
      if (params.container !== undefined) req.scope = params.container;
      if (params.code !== undefined) req.code = params.code;
      if (params.target !== undefined) req.target = params.target;
      if (params.derives !== undefined) req.derives = params.derives;
      if (params.catchBody !== undefined) req.catch_body = params.catchBody;
      if (params.decorator !== undefined) req.decorator = params.decorator;
      if (params.field !== undefined) req.field = params.field;
      if (params.tag !== undefined) req.tag = params.tag;
      if (params.value !== undefined) req.value = params.value;
      if (params.position !== undefined) req.position = params.position;
      if (params.validate !== undefined) req.validate = params.validate;
      const response = await callBridge(bridge, params.op, req, extCtx);
      return textResult(JSON.stringify(response, null, 2));
    },
    renderCall(args, theme, context) {
      return renderTransformCall(args, theme, context);
    },
    renderResult(result, _options, theme, context) {
      return renderTransformResult(result, context.args, theme, context);
    },
  });
}

function validateTransformParams(params: Static<typeof TransformParams>): void {
  const op = params.op;

  if (op === "add_member") {
    if (typeof params.container !== "string") {
      throw new Error("'container' is required for 'add_member' op");
    }
    if (typeof params.code !== "string") {
      throw new Error("'code' is required for 'add_member' op");
    }
  }
  if (
    op === "add_derive" ||
    op === "wrap_try_catch" ||
    op === "add_decorator" ||
    op === "add_struct_tags"
  ) {
    if (typeof params.target !== "string") {
      throw new Error(`'target' is required for '${op}' op`);
    }
  }
  if (op === "add_derive" && !Array.isArray(params.derives)) {
    throw new Error("'derives' array is required for 'add_derive' op");
  }
  if (op === "add_decorator" && typeof params.decorator !== "string") {
    throw new Error("'decorator' is required for 'add_decorator' op");
  }
  if (op === "add_struct_tags") {
    if (typeof params.field !== "string") {
      throw new Error("'field' is required for 'add_struct_tags' op");
    }
    if (typeof params.tag !== "string") {
      throw new Error("'tag' is required for 'add_struct_tags' op");
    }
    if (typeof params.value !== "string") {
      throw new Error("'value' is required for 'add_struct_tags' op");
    }
  }
}
