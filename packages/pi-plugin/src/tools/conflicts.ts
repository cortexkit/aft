/**
 * aft_conflicts — one-call merge conflict inspection.
 */

import type { ExtensionAPI } from "@earendil-works/pi-coding-agent";
import { Type } from "typebox";
import type { PluginContext } from "../types.js";
import { bridgeFor, callBridge, textResult } from "./_shared.js";
import {
  collectTextContent,
  type RenderContextLike,
  renderErrorResult,
  renderSections,
  renderToolCall,
} from "./render-helpers.js";

const ConflictsParams = Type.Object({});

/** Exported for renderer unit tests. */
export function renderConflictCall(
  theme: Parameters<typeof renderToolCall>[2],
  context: RenderContextLike,
) {
  return renderToolCall("conflicts", undefined, theme, context);
}

/** Exported for renderer unit tests. */
export function buildConflictSections(text: string): string[] {
  const trimmed = text.trim();
  if (!trimmed) return ["No merge conflicts found."];

  const [header, ...rest] = trimmed.split(/\n\n+/);
  const match = header.match(/^(\d+)\s+files?,\s+(\d+)\s+conflicts?/i);
  const sections = [
    match
      ? `${match[1]} conflicted file${match[1] === "1" ? "" : "s"} · ${match[2]} region${match[2] === "1" ? "" : "s"}`
      : header,
  ];

  if (rest.length === 0) return sections;
  sections.push(...rest.map((section) => section.trim()).filter(Boolean));
  return sections;
}

/** Exported for renderer unit tests. */
export function renderConflictResult(
  text: string,
  theme: Parameters<typeof renderToolCall>[2],
  context: RenderContextLike,
) {
  const sections = buildConflictSections(text).map((section, index) =>
    index === 0 ? theme.fg("warning", section) : section,
  );
  return renderSections(sections, context);
}

/** Exported for renderer unit tests. */
export function renderConflictToolResult(
  result: Parameters<typeof renderErrorResult>[0],
  theme: Parameters<typeof renderToolCall>[2],
  context: RenderContextLike,
) {
  if (context.isError) return renderErrorResult(result, "conflicts failed", theme, context);
  return renderConflictResult(collectTextContent(result), theme, context);
}

export function registerConflictsTool(pi: ExtensionAPI, ctx: PluginContext): void {
  pi.registerTool({
    name: "aft_conflicts",
    label: "conflicts",
    description:
      "Show all git merge conflicts across the repository — returns line-numbered conflict regions with context for every conflicted file in a single call.",
    parameters: ConflictsParams,
    async execute(_toolCallId: string, _params, _signal, _onUpdate, extCtx) {
      const bridge = bridgeFor(ctx, extCtx.cwd);
      const response = await callBridge(bridge, "git_conflicts", {}, extCtx);
      return textResult((response.text as string | undefined) ?? JSON.stringify(response, null, 2));
    },
    renderCall(_args, theme, context) {
      return renderConflictCall(theme, context);
    },
    renderResult(result, _options, theme, context) {
      return renderConflictToolResult(result, theme, context);
    },
  });
}
