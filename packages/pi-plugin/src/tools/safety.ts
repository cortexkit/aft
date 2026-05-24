/**
 * aft_safety — operation undo, per-file history, named checkpoints, restore, list.
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
  formatTimestamp,
  type RenderContextLike,
  renderErrorResult,
  renderSections,
  renderToolCall,
  shortenPath,
} from "./render-helpers.js";

const SafetyParams = Type.Object({
  op: StringEnum(["undo", "history", "checkpoint", "restore", "list"] as const, {
    description: "Safety operation",
  }),
  filePath: Type.Optional(
    Type.String({ description: "File path (required for history, optional for undo)" }),
  ),
  name: Type.Optional(
    Type.String({ description: "Checkpoint name (required for checkpoint, restore)" }),
  ),
  files: Type.Optional(
    Type.Array(Type.String(), {
      description: "Specific files for checkpoint (optional, defaults to all tracked)",
    }),
  ),
});

/** Exported for renderer unit tests. */
export function buildSafetySections(
  args: Static<typeof SafetyParams>,
  payload: unknown,
  theme: Theme,
): string[] {
  const response = asRecord(payload);
  if (!response) return [theme.fg("muted", "No safety result.")];

  if (args.op === "undo") {
    if (response.operation === true) {
      return [
        `${theme.fg("success", "restored operation")} ${theme.fg("accent", asString(response.op_id) ?? "(operation)")}`,
        `${theme.fg("muted", "files")} ${asNumber(response.restored_count) ?? asRecords(response.restored).length}`,
      ];
    }
    return [
      `${theme.fg("success", "restored")} ${theme.fg("accent", shortenPath(asString(response.path) ?? args.filePath ?? "(file)"))}`,
      `${theme.fg("muted", "backup")} ${asString(response.backup_id) ?? "—"}`,
    ];
  }

  if (args.op === "history") {
    const entries = asRecords(response.entries);
    const sections = [
      theme.fg("accent", shortenPath(asString(response.file) ?? args.filePath ?? "(file)")),
    ];
    if (entries.length === 0) {
      sections.push(theme.fg("muted", "No history entries."));
      return sections;
    }
    sections.push(
      entries
        .map((entry, index) => {
          const backupId = asString(entry.backup_id) ?? `entry-${index + 1}`;
          const timestamp = formatTimestamp(entry.timestamp) ?? "unknown time";
          const description = asString(entry.description) ?? "";
          return `${index + 1}. ${backupId} ${theme.fg("muted", timestamp)}${description ? `\n   ${description}` : ""}`;
        })
        .join("\n"),
    );
    return sections;
  }

  if (args.op === "checkpoint") {
    const skipped = asRecords(response.skipped);
    return [
      `${theme.fg("success", "checkpoint created")} ${theme.fg("accent", asString(response.name) ?? args.name ?? "(checkpoint)")}`,
      `${theme.fg("muted", "files")} ${asNumber(response.file_count) ?? 0}`,
      skipped.length > 0
        ? `${theme.fg("warning", "skipped")}\n${skipped.map((entry) => `  ↳ ${shortenPath(asString(entry.file) ?? "(file)")}: ${asString(entry.error) ?? "unknown error"}`).join("\n")}`
        : theme.fg("muted", "No skipped files."),
    ];
  }

  if (args.op === "restore") {
    return [
      `${theme.fg("success", "checkpoint restored")} ${theme.fg("accent", asString(response.name) ?? args.name ?? "(checkpoint)")}`,
      `${theme.fg("muted", "files")} ${asNumber(response.file_count) ?? 0}`,
    ];
  }

  const checkpoints = asRecords(response.checkpoints);
  const sections = [
    theme.fg("accent", `${checkpoints.length} checkpoint${checkpoints.length === 1 ? "" : "s"}`),
  ];
  if (checkpoints.length === 0) {
    sections.push(theme.fg("muted", "No checkpoints saved."));
    return sections;
  }
  sections.push(
    checkpoints
      .map((checkpoint, index) => {
        const name = asString(checkpoint.name) ?? `checkpoint-${index + 1}`;
        const count = asNumber(checkpoint.file_count) ?? 0;
        const created = formatTimestamp(checkpoint.created_at) ?? "unknown time";
        return `${index + 1}. ${name} ${theme.fg("muted", `${count} file${count === 1 ? "" : "s"} · ${created}`)}`;
      })
      .join("\n"),
  );
  return sections;
}

/** Exported for renderer unit tests. */
export function renderSafetyCall(
  args: Static<typeof SafetyParams>,
  theme: Theme,
  context: RenderContextLike,
) {
  const target = args.filePath ?? args.name;
  const summary = [theme.fg("accent", args.op), target ? accentPath(theme, target) : undefined]
    .filter(Boolean)
    .join(" ");
  return renderToolCall("safety", summary, theme, context);
}

/** Exported for renderer unit tests. */
export function renderSafetyResult(
  result: AgentToolResult<unknown>,
  args: Static<typeof SafetyParams>,
  theme: Theme,
  context: RenderContextLike,
) {
  if (context.isError) return renderErrorResult(result, "safety failed", theme, context);
  return renderSections(
    buildSafetySections(args, extractStructuredPayload(result), theme),
    context,
  );
}

export function registerSafetyTool(pi: ExtensionAPI, ctx: PluginContext): void {
  pi.registerTool({
    name: "aft_safety",
    label: "safety",
    description:
      "File safety and recovery operations. Ops: `undo` (omit filePath to undo the entire last tool call; pass filePath to pop latest snapshot for one file — irreversible), `history` (list snapshots for a file), `checkpoint` (save named snapshot), `restore` (restore named checkpoint), `list` (list checkpoints). Per-file undo stack is capped at 20.",
    parameters: SafetyParams,
    async execute(
      _toolCallId: string,
      params: Static<typeof SafetyParams>,
      _signal,
      _onUpdate,
      extCtx,
    ) {
      if (params.op === "history" && !params.filePath) {
        throw new Error(`op='${params.op}' requires 'filePath'`);
      }
      if ((params.op === "checkpoint" || params.op === "restore") && !params.name) {
        throw new Error(`op='${params.op}' requires 'name'`);
      }
      const bridge = bridgeFor(ctx, extCtx.cwd);
      const commandMap: Record<string, string> = {
        undo: "undo",
        history: "edit_history",
        checkpoint: "checkpoint",
        restore: "restore_checkpoint",
        list: "list_checkpoints",
      };
      const req: Record<string, unknown> = {};
      if (params.name) req.name = params.name;
      if (params.op === "checkpoint") {
        // For checkpoint, Rust only knows `files`. If the agent passes
        // `filePath` (a reasonable mistake — the tool schema exposes both),
        // auto-promote it into a single-entry `files` list rather than
        // silently dropping it and falling back to the whole tracked-file
        // set.
        if (params.files) {
          req.files = params.files;
        } else if (params.filePath) {
          req.files = [params.filePath];
        }
      } else {
        // undo / history / restore / list all take `file` as-is.
        if (params.filePath) req.file = params.filePath;
        if (params.files) req.files = params.files;
      }
      const response = await callBridge(bridge, commandMap[params.op], req, extCtx);
      return textResult(JSON.stringify(response, null, 2));
    },
    renderCall(args, theme, context) {
      return renderSafetyCall(args, theme, context);
    },
    renderResult(result, _options, theme, context) {
      return renderSafetyResult(result, context.args, theme, context);
    },
  });
}
