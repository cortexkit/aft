/**
 * aft_conflicts — one-call merge conflict inspection.
 */

import type { ExtensionAPI } from "@earendil-works/pi-coding-agent";
import { Type } from "typebox";
import type { PluginContext } from "../types.js";
import { bridgeFor, callToolCall, textResult, withPathAliasPreparation } from "./_shared.js";
import { assertExternalDirectoryPermission, resolvePathArg } from "./hoisted.js";
import {
  collapsibleResult,
  collectTextContent,
  type RenderContextLike,
  type RenderResultOptionsLike,
  renderErrorResult,
  renderSections,
  renderToolCall,
} from "./render-helpers.js";

const ConflictsParams = Type.Object({
  path: Type.Optional(
    Type.String({
      description:
        "Optional path inside the git repository or worktree to inspect (absolute or relative to project root). Conflicts are discovered from that repository's top level. Defaults to the session project root.",
    }),
  ),
});

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
  options: RenderResultOptionsLike = { expanded: true },
) {
  const sections = buildConflictSections(text).map((section, index) =>
    index === 0 ? theme.fg("warning", section) : section,
  );
  return collapsibleResult({
    summary: sections[0] ?? "No merge conflicts found.",
    full: renderSections(sections, context),
    expanded: options.expanded,
    context,
  });
}

/** Exported for renderer unit tests. */
export function renderConflictToolResult(
  result: Parameters<typeof renderErrorResult>[0],
  theme: Parameters<typeof renderToolCall>[2],
  context: RenderContextLike,
  options: RenderResultOptionsLike = { expanded: true },
) {
  if (context.isError) return renderErrorResult(result, "conflicts failed", theme, context);
  return renderConflictResult(collectTextContent(result), theme, context, options);
}

export function registerConflictsTool(pi: ExtensionAPI, ctx: PluginContext): void {
  pi.registerTool(
    withPathAliasPreparation({
      name: "aft_conflicts",
      label: "conflicts",
      description:
        "Show all git merge conflicts across the repository — returns line-numbered conflict regions with context for every conflicted file in a single call.",
      parameters: ConflictsParams,
      async execute(_toolCallId: string, params, _signal, onUpdate, extCtx) {
        const bridge = bridgeFor(ctx, extCtx.cwd);
        const reqParams: Record<string, unknown> = {};
        const path = (params as { path?: unknown })?.path;
        if (typeof path === "string" && path.trim() !== "") {
          // Gate the repo/worktree path like Pi's other path-taking tools (only
          // prompts when restrict_to_project_root is true), and resolve tilde/
          // relative the same way Rust will.
          await assertExternalDirectoryPermission(extCtx, path, {
            restrictToProjectRoot: ctx.config.restrict_to_project_root ?? false,
          });
          reqParams.path = await resolvePathArg(extCtx.cwd, path);
        }
        const response = await callToolCall(bridge, "conflicts", reqParams, extCtx, { onUpdate });
        if (response.success === false) {
          throw new Error(response.text || response.message || "conflicts failed");
        }
        return textResult(response.text, response);
      },
      renderCall(_args, theme, context) {
        return renderConflictCall(theme, context);
      },
      renderResult(result, options = { expanded: false, isPartial: false }, theme, context) {
        return renderConflictToolResult(result, theme, context, options);
      },
    }),
  );
}
