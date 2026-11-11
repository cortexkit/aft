/**
 * aft_delete + aft_move — filesystem ops with per-file backup.
 * Both go through Rust so backups and checkpoint rollback work the same way.
 */

import { coerceBoolean, coerceStringArray, toolErrorFromResponse } from "@cortexkit/aft-bridge";
import type { AgentToolResult, ExtensionAPI, Theme } from "@earendil-works/pi-coding-agent";
import { type Static, Type } from "typebox";
import type { PluginContext } from "../types.js";
import { bridgeFor, callToolCall, textResult, withPathAliasPreparation } from "./_shared.js";
import { assertExternalDirectoryPermission, resolvePathArg } from "./hoisted.js";
import {
  accentPath,
  asRecordOrEmpty,
  collapsibleResult,
  type RenderContextLike,
  type RenderResultOptionsLike,
  renderErrorResult,
  renderSections,
  renderToolCall,
  shortenPath,
} from "./render-helpers.js";

const DeleteParams = Type.Object({
  files: Type.Array(Type.String(), {
    description: "Paths to delete (one or more). May include directories when recursive=true.",
    minItems: 1,
  }),
  recursive: Type.Optional(
    Type.Boolean({
      description:
        "Required to delete a directory and its contents. Defaults to false; passing a directory without this returns an error.",
    }),
  ),
});

const MoveParams = Type.Object({
  path: Type.String({
    description: "Source file path to move (absolute or relative to project root)",
  }),
  destination: Type.String({
    description: "Destination file path (absolute or relative to project root)",
  }),
});

export interface FsSurface {
  delete: boolean;
  move: boolean;
}

function deletedPath(entry: unknown): string | undefined {
  if (typeof entry === "string") return entry;
  if (entry && typeof entry === "object" && !Array.isArray(entry)) {
    const file = (entry as { file?: unknown }).file;
    if (typeof file === "string") return file;
  }
  return undefined;
}

/** Exported for renderer unit tests. */
export function renderFsCall(
  toolName: "aft_delete" | "aft_move",
  args: unknown,
  theme: Theme,
  context: RenderContextLike,
) {
  const safeArgs = asRecordOrEmpty(args);
  if (toolName === "aft_delete") {
    const rawFiles = safeArgs.files;
    if (!Array.isArray(rawFiles)) return renderToolCall("delete", undefined, theme, context);
    const files = rawFiles.filter((file): file is string => typeof file === "string");
    const summary =
      files.length === 1
        ? accentPath(theme, files[0])
        : `${theme.fg("accent", String(files.length))} ${theme.fg("muted", "files")}`;
    return renderToolCall("delete", summary, theme, context);
  }

  const path = typeof safeArgs.path === "string" ? safeArgs.path : undefined;
  const destination = typeof safeArgs.destination === "string" ? safeArgs.destination : undefined;
  const summary = [
    path ? accentPath(theme, path) : undefined,
    destination ? accentPath(theme, destination) : undefined,
  ]
    .filter(Boolean)
    .join(` ${theme.fg("muted", "→")} `);
  return renderToolCall("move", summary, theme, context);
}

/** Exported for renderer unit tests. */
export function renderFsResult(
  toolName: "aft_delete" | "aft_move",
  args: unknown,
  result: AgentToolResult<unknown>,
  theme: Theme,
  context: RenderContextLike,
  options: RenderResultOptionsLike = { expanded: true },
) {
  if (context.isError) {
    return renderErrorResult(result, `${toolName} failed`, theme, context);
  }

  const safeArgs = asRecordOrEmpty(args);

  if (toolName === "aft_delete") {
    const files = Array.isArray(safeArgs.files)
      ? safeArgs.files.filter((file): file is string => typeof file === "string")
      : [];
    const data = (result?.details ?? {}) as {
      deleted?: string[];
      skipped_files?: Array<{ file: string; reason: string }>;
      complete?: boolean;
    };
    const deletedPaths = Array.isArray(data.deleted)
      ? data.deleted.map(deletedPath).filter((file): file is string => file !== undefined)
      : files;
    const skipped = data.skipped_files ?? [];
    const lines: string[] = [];
    for (const entry of deletedPaths) {
      lines.push(`${theme.fg("success", "✓ deleted")} ${theme.fg("accent", shortenPath(entry))}`);
    }
    for (const entry of skipped) {
      lines.push(
        `${theme.fg("error", "✗ skipped")} ${theme.fg("accent", shortenPath(entry.file))} ${theme.fg("muted", `(${entry.reason})`)}`,
      );
    }
    if (lines.length === 0) {
      lines.push(theme.fg("muted", "(no files deleted)"));
    }
    return collapsibleResult({
      summary: `${deletedPaths.length} deleted, ${skipped.length} skipped`,
      full: renderSections([lines.join("\n")], context),
      expanded: options.expanded,
      context,
    });
  }

  const path = typeof safeArgs.path === "string" ? safeArgs.path : "";
  const destination = typeof safeArgs.destination === "string" ? safeArgs.destination : "";
  const sections = [
    `${theme.fg("success", "✓ moved")}${path ? ` ${theme.fg("accent", shortenPath(path))}` : ""}`,
  ];
  if (destination)
    sections.push(`${theme.fg("muted", "to")} ${theme.fg("accent", shortenPath(destination))}`);
  return collapsibleResult({
    summary: `moved${path ? ` ${shortenPath(path)}` : ""}${destination ? ` to ${shortenPath(destination)}` : ""}`,
    full: renderSections(sections, context),
    expanded: options.expanded,
    context,
  });
}

export function registerFsTools(pi: ExtensionAPI, ctx: PluginContext, surface: FsSurface): void {
  const backupsDisabled = ctx.config.backup?.enabled === false;
  if (surface.delete) {
    pi.registerTool(
      withPathAliasPreparation({
        name: "aft_delete",
        label: "delete",
        description:
          "Delete one or more files (or directories). " +
          (backupsDisabled
            ? "Backup capture is disabled by user config, so this tool does not create undo snapshots. A directory tree containing a mount point of another filesystem is still refused. "
            : "Each file is backed up before deletion — use `aft_safety undo` to recover any of them. A recursive delete backs up the whole tree: directories (empty ones too, with their permissions), file contents, hard links (relinked by undo) and symlinks (the link itself, never its target; restored exactly, even when dangling). Sockets are deleted but undo does not restore them, and a file hard-linked to paths outside the tree comes back as an independent copy; both are reported as warnings. Refused before anything is deleted: mount points of another filesystem, named pipes, device nodes, and symlinks undo cannot recreate exactly (non-UTF-8 target, or on Windows). Paths under the system temp directory are never backed up, so only mount points are refused there. The deleted file's contents stay in the undo store until its retention expires. A recursive delete whose backup would record more than 2,000 entries (files, directories and links) or copy 100 MiB in one call is refused before anything is deleted; delete such a tree in smaller pieces, or use bash `rm -rf` when no undo is needed. ") +
          "Directory deletion requires recursive: true. " +
          "Returns { success, complete, deleted, skipped_files }: partial success is allowed; files that fail are reported in skipped_files.",
        parameters: DeleteParams,
        async execute(
          _toolCallId: string,
          params: Static<typeof DeleteParams>,
          _signal,
          _onUpdate,
          extCtx,
        ) {
          // Coerce at the boundary: some hosts deliver `files` as a bare string
          // or a JSON-stringified array despite the schema, which would crash the
          // unchecked `.map` below before any validation runs.
          const inputs = coerceStringArray(params.files);
          if (inputs.length === 0) {
            throw new Error("delete: `files` must be a non-empty array of paths");
          }
          const files = await Promise.all(inputs.map((file) => resolvePathArg(extCtx.cwd, file)));
          // One value for every file of this call, even if a live config
          // reload replaces `ctx.config` while the checks run.
          const restrictToProjectRoot = ctx.config.restrict_to_project_root ?? false;
          const checked = new Set<string>();
          for (const file of files) {
            if (checked.has(file)) continue;
            checked.add(file);
            await assertExternalDirectoryPermission(extCtx, file, { restrictToProjectRoot });
          }

          const bridge = bridgeFor(ctx, extCtx.cwd);
          // Single batched call so every file shares one op_id; one
          // `aft_safety undo` then restores the whole delete atomically.
          const response = await callToolCall(
            bridge,
            "delete",
            {
              files,
              // Coerce at the boundary, like `files`: a stringified "true" from the
              // model must not silently drop the flag (see coerceBoolean).
              recursive: coerceBoolean(params.recursive),
            },
            extCtx,
          );
          if (response.success === false) {
            throw toolErrorFromResponse("delete", response);
          }
          const deletedEntries = (response.deleted as Array<{ file: string }> | undefined) ?? [];
          const skipped =
            (response.skipped_files as Array<{ file: string; reason: string }> | undefined) ?? [];
          const deleted = deletedEntries.map((entry) => entry.file);
          // Refuse a fully-failed batch with an error so renderers don't show
          // "completed" for nothing-actually-deleted.
          if (deleted.length === 0 && skipped.length > 0) {
            throw new Error(
              `delete failed for all ${skipped.length} file(s):\n` +
                skipped.map((entry) => `  ${entry.file}: ${entry.reason}`).join("\n"),
            );
          }
          return textResult(response.text, response);
        },
        renderCall(args, theme, context) {
          return renderFsCall("aft_delete", args, theme, context);
        },
        renderResult(result, options = { expanded: false, isPartial: false }, theme, context) {
          return renderFsResult("aft_delete", context.args, result, theme, context, options);
        },
      }),
    );
  }

  if (surface.move) {
    pi.registerTool(
      withPathAliasPreparation({
        name: "aft_move",
        label: "move",
        description:
          "Move or rename a file. " +
          (backupsDisabled
            ? "Backup capture is disabled by user config. "
            : "Creates an undo backup before moving. ") +
          "Creates parent directories for the destination automatically. This operates on whole files at the OS level; it does not relocate an individual symbol or rewrite imports.",
        parameters: MoveParams,
        async execute(
          _toolCallId: string,
          params: Static<typeof MoveParams>,
          _signal,
          _onUpdate,
          extCtx,
        ) {
          const filePath = await resolvePathArg(extCtx.cwd, params.path as string);
          const destination = await resolvePathArg(extCtx.cwd, params.destination);
          const checked = new Set([filePath, destination]);
          const restrictToProjectRoot = ctx.config.restrict_to_project_root ?? false;
          for (const file of checked) {
            await assertExternalDirectoryPermission(extCtx, file, { restrictToProjectRoot });
          }

          const bridge = bridgeFor(ctx, extCtx.cwd);
          const response = await callToolCall(
            bridge,
            "move",
            {
              filePath: params.path,
              destination: params.destination,
            },
            extCtx,
          );
          if (response.success === false) {
            throw new Error(response.text || response.message || "move failed");
          }
          return textResult(response.text, response);
        },
        renderCall(args, theme, context) {
          return renderFsCall("aft_move", args, theme, context);
        },
        renderResult(result, options = { expanded: false, isPartial: false }, theme, context) {
          return renderFsResult("aft_move", context.args, result, theme, context, options);
        },
      }),
    );
  }
}
