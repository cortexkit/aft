/**
 * Hoisted tools that replace opencode's built-in tools (read, write, edit, apply_patch).
 *
 * When hoist_builtin_tools is enabled (default), these tools are registered with
 * the SAME names as opencode's built-in tools, effectively overriding them.
 * When disabled, they're registered with aft_ prefix (e.g., aft_read).
 *
 * All file operations go through AFT's Rust binary for better performance,
 * backup tracking, formatting, and inline diagnostics.
 */

import * as fs from "node:fs";
import * as path from "node:path";
import type { ToolDefinition } from "@opencode-ai/plugin";
import { tool } from "@opencode-ai/plugin";
import { resolveBashConfig } from "../config.js";
import { storeToolMetadata } from "../metadata-store.js";
import { applyUpdateChunks, parsePatch } from "../patch-parser.js";
import type { PluginContext } from "../types.js";
import { callBridge } from "./_shared.js";
import { createBashKillTool, createBashStatusTool, createBashTool } from "./bash.js";
import {
  assertExternalDirectoryPermission,
  permissionDeniedResponse,
  runAsk,
} from "./permissions.js";

/** Extract callID from plugin context (exists on object but not in TS type). */
function getCallID(ctx: unknown): string | undefined {
  const c = ctx as { callID?: string; callId?: string; call_id?: string };
  return c.callID ?? c.callId ?? c.call_id;
}

/** Get relative path matching opencode's format — the desktop UI parses it to extract filename + dir. */
function relativeToWorktree(fp: string, worktree: string): string {
  return path.relative(worktree, fp);
}

/**
 * Build the navigation footer for a `read` response.
 *
 * Two cases (kept aligned with the Pi plugin's helper of the same name):
 *
 *   A. Agent did NOT specify a range
 *      → if the response is clamped (Rust's default limit / byte cap
 *        kicked in), emit a hint footer so they know more exists and
 *        how to get it. Otherwise no footer (the response IS the file).
 *
 *   B. Agent EXPLICITLY supplied startLine/endLine OR offset/limit
 *      → no footer. The agent picked the range, so:
 *         - they already have the math
 *         - if the range was clamped vs. what they asked for, they can
 *           see that from `content` length vs. their requested range
 *         - telling them "use startLine/endLine to read other sections"
 *           right after they used those exact params is patronizing
 *           and burns tokens (the user's exact dogfooding complaint
 *           when this returned the hint for read({130, 190}) on a
 *           191-line file).
 *
 * `data.truncated` from Rust means "response is a slice of the file" — TRUE
 * even when the slice matches what the agent asked for. So we cannot key
 * the hint off that flag alone; we also need to know whether the agent
 * picked the range.
 *
 * Earlier drafts emitted a compact `(Lines X-Y of Z)` when the agent
 * picked a range AND `endLine < totalLines`, on the theory that this
 * meant their range was clamped. But that condition fires whenever the
 * agent's chosen range happens not to extend to EOF (the user's exact
 * complaint case: they asked 130-190 of 191 → end_line(190) < total(191)
 * → spurious compact footer). Removed.
 */
function formatReadFooter(agentSpecifiedRange: boolean, data: Record<string, unknown>): string {
  // CASE B: agent picked the range. No footer at all. They have the math.
  if (agentSpecifiedRange) return "";

  if (!data.truncated) return "";

  const startLine = data.start_line as number | undefined;
  const endLine = data.end_line as number | undefined;
  const totalLines = data.total_lines as number | undefined;
  if (startLine === undefined || endLine === undefined || totalLines === undefined) {
    return "";
  }

  // CASE A: agent did not pick a range, response was clamped — hint is
  // useful, tell them how to read more.
  return `\n(Showing lines ${startLine}-${endLine} of ${totalLines}. Use startLine/endLine to read other sections.)`;
}

/** Test-only export. Production code uses buildUnifiedDiff directly. */
export const _buildUnifiedDiffForTest = (fp: string, before: string, after: string): string =>
  buildUnifiedDiff(fp, before, after);

/**
 * Build a unified diff string from before/after content using a proper
 * LCS-based diff algorithm with grouped hunks and 3 lines of context.
 *
 * The previous implementation compared lines by index, so any insertion
 * or deletion that shifted line numbers caused every subsequent line to
 * compare unequal — emitting the entire rest of the file as "changed"
 * (issue #22, regression introduced in v0.15.3 when apply_patch started
 * sending diffs).
 *
 * Output matches GNU diff -u style: --- /+++ headers, @@ hunk markers,
 * one hunk per change cluster (consecutive changes within 6 lines of
 * each other are merged into a single hunk).
 */
function buildUnifiedDiff(fp: string, before: string, after: string): string {
  const beforeLines = before.split("\n");
  const afterLines = after.split("\n");

  // LCS is O(n*m) in lines; a 5000x5000 matrix uses ~100 MB and ~250 ms,
  // which we accept for normal source files. Above that we skip diff
  // generation rather than block the plugin event loop on a single edit.
  // Byte-size gating misses the real cost (a 100 KB minified bundle is one
  // line; a 30 KB markdown file with 1500 lines is the expensive case).
  const LINE_CAP = 5000;
  if (beforeLines.length > LINE_CAP || afterLines.length > LINE_CAP) {
    const limit = Math.max(beforeLines.length, afterLines.length);
    return `Index: ${fp}\n(diff skipped: file has ${limit} lines, above ${LINE_CAP}-line diff cap)\n`;
  }

  const ops = diffLines(beforeLines, afterLines);

  // No changes → empty diff (caller decides whether to render the header).
  if (ops.every((op) => op.tag === "eq")) {
    return `Index: ${fp}\n===================================================================\n--- ${fp}\n+++ ${fp}\n`;
  }

  const CONTEXT = 3;
  const HUNK_GAP = CONTEXT * 2; // merge hunks closer than this
  const hunks = groupIntoHunks(ops, CONTEXT, HUNK_GAP, beforeLines.length, afterLines.length);

  let diff = `Index: ${fp}\n===================================================================\n--- ${fp}\n+++ ${fp}\n`;
  for (const hunk of hunks) {
    diff += `@@ -${hunk.beforeStart},${hunk.beforeCount} +${hunk.afterStart},${hunk.afterCount} @@\n`;
    for (const line of hunk.lines) {
      diff += `${line}\n`;
    }
  }
  return diff;
}

/**
 * Count non-empty lines in a string. Used for unambiguous addition/deletion
 * counts when one side of a diff is empty (apply_patch's add/delete hunks).
 *
 * `split("\n")` on a string with a trailing newline produces a trailing
 * empty element which we drop, so the count matches "actual content lines"
 * rather than "split slots". For empty input the count is 0.
 */
function lineCount(content: string): number {
  if (content.length === 0) return 0;
  const parts = content.split("\n");
  // Drop the trailing empty element produced by a terminating "\n".
  if (parts[parts.length - 1] === "") parts.pop();
  return parts.length;
}

/**
 * Count additions and deletions between two file contents using the same
 * LCS path that powers buildUnifiedDiff. Used for apply_patch's *move*
 * case where the Rust write diff would compare against an empty target
 * (overcounting additions). For non-move updates we use the Rust counts
 * directly.
 */
function countDiffLines(before: string, after: string): { additions: number; deletions: number } {
  const beforeLines = before.split("\n");
  const afterLines = after.split("\n");
  const ops = diffLines(beforeLines, afterLines);
  let additions = 0;
  let deletions = 0;
  for (const op of ops) {
    if (op.tag === "ins") additions++;
    else if (op.tag === "del") deletions++;
  }
  return { additions, deletions };
}

type DiffOp =
  | { tag: "eq"; beforeIdx: number; afterIdx: number; line: string }
  | { tag: "del"; beforeIdx: number; line: string }
  | { tag: "ins"; afterIdx: number; line: string };

/**
 * LCS-based line diff. Builds a length table then walks back to produce ops.
 * O(n*m) time and space — fine for the 100KB SIZE_CAP guard above.
 */
function diffLines(a: readonly string[], b: readonly string[]): DiffOp[] {
  const n = a.length;
  const m = b.length;

  // dp[i][j] = LCS length of a[0..i] and b[0..j]
  // Use a flat Uint32Array for memory efficiency on large files.
  const dp = new Uint32Array((n + 1) * (m + 1));
  const w = m + 1;
  for (let i = 1; i <= n; i++) {
    for (let j = 1; j <= m; j++) {
      if (a[i - 1] === b[j - 1]) {
        dp[i * w + j] = dp[(i - 1) * w + (j - 1)] + 1;
      } else {
        const up = dp[(i - 1) * w + j];
        const left = dp[i * w + (j - 1)];
        dp[i * w + j] = up >= left ? up : left;
      }
    }
  }

  // Walk back to produce ops in reverse, then reverse at the end.
  const ops: DiffOp[] = [];
  let i = n;
  let j = m;
  while (i > 0 && j > 0) {
    if (a[i - 1] === b[j - 1]) {
      ops.push({ tag: "eq", beforeIdx: i - 1, afterIdx: j - 1, line: a[i - 1] });
      i--;
      j--;
    } else if (dp[(i - 1) * w + j] >= dp[i * w + (j - 1)]) {
      ops.push({ tag: "del", beforeIdx: i - 1, line: a[i - 1] });
      i--;
    } else {
      ops.push({ tag: "ins", afterIdx: j - 1, line: b[j - 1] });
      j--;
    }
  }
  while (i > 0) {
    ops.push({ tag: "del", beforeIdx: i - 1, line: a[i - 1] });
    i--;
  }
  while (j > 0) {
    ops.push({ tag: "ins", afterIdx: j - 1, line: b[j - 1] });
    j--;
  }
  ops.reverse();
  return ops;
}

interface Hunk {
  beforeStart: number; // 1-based
  beforeCount: number;
  afterStart: number; // 1-based
  afterCount: number;
  lines: string[]; // each prefixed with " ", "+", or "-"
}

/**
 * Group ops into hunks. Consecutive change ops are clustered with `context`
 * lines on each side; clusters closer than `gap` are merged into one hunk.
 */
function groupIntoHunks(
  ops: DiffOp[],
  context: number,
  gap: number,
  beforeLen: number,
  afterLen: number,
): Hunk[] {
  // Find indices of change ops (ins or del).
  const changeIdx: number[] = [];
  for (let k = 0; k < ops.length; k++) {
    if (ops[k].tag !== "eq") changeIdx.push(k);
  }
  if (changeIdx.length === 0) return [];

  // Build hunk ranges in op-index space, then merge nearby ones.
  const ranges: Array<[number, number]> = [];
  for (const idx of changeIdx) {
    const start = Math.max(0, idx - context);
    const end = Math.min(ops.length - 1, idx + context);
    if (ranges.length > 0 && start <= ranges[ranges.length - 1][1] + gap) {
      ranges[ranges.length - 1][1] = Math.max(ranges[ranges.length - 1][1], end);
    } else {
      ranges.push([start, end]);
    }
  }

  // Materialize each range as a hunk. Track 1-based line numbers from the
  // first op's recorded indices.
  const hunks: Hunk[] = [];
  for (const [start, end] of ranges) {
    let beforeStart = -1;
    let afterStart = -1;
    let beforeCount = 0;
    let afterCount = 0;
    const lines: string[] = [];
    for (let k = start; k <= end; k++) {
      const op = ops[k];
      if (op.tag === "eq") {
        if (beforeStart === -1) beforeStart = op.beforeIdx + 1;
        if (afterStart === -1) afterStart = op.afterIdx + 1;
        beforeCount++;
        afterCount++;
        lines.push(` ${op.line}`);
      } else if (op.tag === "del") {
        if (beforeStart === -1) beforeStart = op.beforeIdx + 1;
        if (afterStart === -1) {
          // Pure-deletion hunk at start: position after-cursor is one past
          // the last preceding equal op. Walk forward to find the next
          // ins/eq to anchor afterStart, otherwise clamp to end.
          afterStart = inferAfterStart(ops, k, afterLen);
        }
        beforeCount++;
        lines.push(`-${op.line}`);
      } else {
        if (afterStart === -1) afterStart = op.afterIdx + 1;
        if (beforeStart === -1) {
          beforeStart = inferBeforeStart(ops, k, beforeLen);
        }
        afterCount++;
        lines.push(`+${op.line}`);
      }
    }
    // Empty file edge case: GNU diff uses 0 for line numbers when count is 0.
    if (beforeCount === 0) beforeStart = 0;
    if (afterCount === 0) afterStart = 0;
    hunks.push({ beforeStart, beforeCount, afterStart, afterCount, lines });
  }
  return hunks;
}

/** Find what afterStart should be when a hunk begins with deletions. */
function inferAfterStart(ops: DiffOp[], from: number, afterLen: number): number {
  // Look forward for any op carrying an afterIdx.
  for (let k = from; k < ops.length; k++) {
    const op = ops[k];
    if (op.tag === "eq") return op.afterIdx + 1;
    if (op.tag === "ins") return op.afterIdx + 1;
  }
  // No future after-line — point past the last line.
  return afterLen;
}

/** Find what beforeStart should be when a hunk begins with insertions. */
function inferBeforeStart(ops: DiffOp[], from: number, beforeLen: number): number {
  for (let k = from; k < ops.length; k++) {
    const op = ops[k];
    if (op.tag === "eq") return op.beforeIdx + 1;
    if (op.tag === "del") return op.beforeIdx + 1;
  }
  return beforeLen;
}

const z = tool.schema;

// ---------------------------------------------------------------------------
// Tool descriptions focus on behavior, modes, and return values.
// Parameter docs live in Zod .describe() and reach the LLM via JSON Schema.
// ---------------------------------------------------------------------------

const READ_DESCRIPTION = `Read file contents or list directory entries.

Use either startLine/endLine OR offset/limit to read a section of a file.

Behavior:
- Returns line-numbered content (e.g., "1: const x = 1")
- Lines longer than 2000 characters are truncated
- Output capped at 50KB
- Binary files are auto-detected and return a size-only message
- Image files (.png, .jpg, .gif, .webp, etc.) and PDFs return a metadata string (format, size, path) — no file content is returned
- Directories return sorted entries with trailing / for subdirectories

Examples:
  Read full file: { "filePath": "src/app.ts" }
  Read lines 50-100: { "filePath": "src/app.ts", "startLine": 50, "endLine": 100 }
  Read 30 lines from line 200: { "filePath": "src/app.ts", "offset": 200, "limit": 30 }
  List directory: { "filePath": "src/" }

Returns: Line-numbered file content string. For directories: newline-joined sorted entries. For binary files: size/message string.`;

/**
 * Creates the simple read tool. Registers as "read" when hoisted, "aft_read" when not.
 */
export function createReadTool(ctx: PluginContext): ToolDefinition {
  return {
    description: READ_DESCRIPTION,
    args: {
      filePath: z
        .string()
        .describe("Path to file or directory (absolute or relative to project root)"),
      startLine: z.number().optional().describe("1-based line to start reading from"),
      endLine: z.number().optional().describe("1-based line to stop reading at (inclusive)"),
      limit: z.number().optional().describe("Max lines to return (default: 2000)"),
      offset: z
        .number()
        .optional()
        .describe(
          "1-based line number to start reading from (use with limit). Ignored if startLine is provided",
        ),
    },
    execute: async (args, context): Promise<string> => {
      const file = args.filePath as string;

      // Resolve relative paths
      const filePath = path.isAbsolute(file) ? file : path.resolve(context.directory, file);

      // External-directory check first (mirrors opencode-native ordering in
      // tool/read.ts:175). Out-of-project paths prompt the user via the
      // separate `external_directory` permission rule.
      {
        const denial = await assertExternalDirectoryPermission(context, filePath);
        if (denial) return permissionDeniedResponse(denial);
      }

      // Permission check
      try {
        await runAsk(
          context.ask({
            permission: "read",
            patterns: [filePath],
            always: ["*"],
            metadata: {},
          }),
        );
      } catch (error) {
        if (error instanceof Error && error.message) return permissionDeniedResponse(error.message);
        return permissionDeniedResponse("Permission denied.");
      }

      // Image/PDF detection — return metadata for UI preview
      const ext = path.extname(filePath).toLowerCase();
      const mimeMap: Record<string, string> = {
        ".png": "image/png",
        ".jpg": "image/jpeg",
        ".jpeg": "image/jpeg",
        ".gif": "image/gif",
        ".webp": "image/webp",
        ".bmp": "image/bmp",
        ".ico": "image/x-icon",
        ".tiff": "image/tiff",
        ".tif": "image/tiff",
        ".avif": "image/avif",
        ".heic": "image/heic",
        ".heif": "image/heif",
        ".pdf": "application/pdf",
      };
      const mime = mimeMap[ext];
      if (mime) {
        const isImage = mime.startsWith("image/");
        const label = isImage ? "Image" : "PDF";
        let fileSize = 0;
        try {
          const stat = await import("node:fs/promises").then((fs) => fs.stat(filePath));
          fileSize = stat.size;
        } catch {
          /* ignore */
        }
        const sizeStr =
          fileSize > 1024 * 1024
            ? `${(fileSize / (1024 * 1024)).toFixed(1)}MB`
            : fileSize > 1024
              ? `${(fileSize / 1024).toFixed(0)}KB`
              : `${fileSize} bytes`;
        const msg = `${label} read successfully`;
        const imgCallID = getCallID(context);
        if (imgCallID) {
          storeToolMetadata(context.sessionID, imgCallID, {
            title: path.relative(context.worktree, filePath),
            metadata: {
              preview: msg,
              filepath: filePath,
              isImage,
              isPdf: mime === "application/pdf",
            },
          });
        }
        return `${msg} (${ext.slice(1).toUpperCase()}, ${sizeStr}). File: ${filePath}`;
      }

      // Normalize offset/limit to startLine/endLine (backward compat with opencode's read)
      let startLine = args.startLine;
      let endLine = args.endLine;
      if (startLine === undefined && args.offset !== undefined) {
        startLine = args.offset;
        if (args.limit !== undefined) {
          endLine = Number(args.offset) + Number(args.limit) - 1;
        }
      }

      // Always use Rust read command — simple file reading only
      const params: Record<string, unknown> = { file: filePath };
      if (startLine !== undefined) params.start_line = startLine;
      if (endLine !== undefined) params.end_line = endLine;
      // Only send limit if we did NOT convert offset to startLine/endLine
      if (args.limit !== undefined && args.offset === undefined) params.limit = args.limit;

      const data = await callBridge(ctx, context, "read", params);

      // Error response (e.g. file not found)
      if (data.success === false) {
        throw new Error((data.message as string) || "read failed");
      }

      const readCallID = getCallID(context);

      // Directory response
      if (data.entries) {
        if (readCallID) {
          const dp = relativeToWorktree(filePath, context.worktree) || file;
          storeToolMetadata(context.sessionID, readCallID, { title: dp, metadata: { title: dp } });
        }
        return (data.entries as string[]).join("\n");
      }

      // Binary response
      if (data.binary) {
        if (readCallID) {
          const dp = relativeToWorktree(filePath, context.worktree) || file;
          storeToolMetadata(context.sessionID, readCallID, { title: dp, metadata: { title: dp } });
        }
        return data.message as string;
      }

      // File content — already line-numbered from Rust
      if (readCallID) {
        const dp = relativeToWorktree(filePath, context.worktree) || file;
        storeToolMetadata(context.sessionID, readCallID, { title: dp, metadata: { title: dp } });
      }
      let output = data.content as string;

      // Three-case footer: see formatReadFooter() doc.
      const agentSpecifiedRange =
        args.startLine !== undefined ||
        args.endLine !== undefined ||
        args.offset !== undefined ||
        args.limit !== undefined;
      const footer = formatReadFooter(agentSpecifiedRange, data);
      if (footer) output += footer;

      return output;
    },
  };
}

// ---------------------------------------------------------------------------
// WRITE tool
// ---------------------------------------------------------------------------

function getWriteDescription(editToolName: string): string {
  return `Write content to a file, creating it (and parent directories) if needed.

Automatically creates parent directories. Backs up existing files before overwriting.
If the project has a formatter configured (biome, prettier, rustfmt, etc.), the file
is auto-formatted after writing. Returns inline LSP diagnostics when available.

**Behavior:**
- Creates parent directories automatically (no need to mkdir first)
- Existing files are backed up before overwriting (recoverable via aft_safety undo)
- Auto-formats using project formatter if configured (biome.json, .prettierrc, etc.)
- Returns LSP error-level diagnostics inline if type errors are introduced
- Use this for creating new files or completely replacing file contents
- For partial edits (find/replace), use the \`${editToolName}\` tool instead

Returns: Status message string (for example: "Created new file. Auto-formatted.") with optional inline LSP error lines.`;
}

function createWriteTool(ctx: PluginContext, editToolName = "edit"): ToolDefinition {
  return {
    description: getWriteDescription(editToolName),
    args: {
      filePath: z
        .string()
        .describe("Path to the file to write (absolute or relative to project root)"),
      content: z.string().describe("The full content to write to the file"),
    },
    execute: async (args, context): Promise<string> => {
      const file = args.filePath as string;
      const content = args.content as string;

      const filePath = path.isAbsolute(file) ? file : path.resolve(context.directory, file);

      const relPath = path.relative(context.worktree, filePath);

      // External-directory check first (mirrors opencode-native write.ts:43).
      {
        const denial = await assertExternalDirectoryPermission(context, filePath);
        if (denial) return permissionDeniedResponse(denial);
      }

      // Permission check
      await runAsk(
        context.ask({
          permission: "edit",
          patterns: [relPath],
          always: ["*"],
          metadata: { filepath: filePath },
        }),
      );

      const data = await callBridge(ctx, context, "write", {
        file: filePath,
        content,
        create_dirs: true,
        diagnostics: true,
        include_diff: true,
      });

      // Error response (e.g. path validation failure)
      if (data.success === false) {
        throw new Error((data.message as string) || "write failed");
      }

      let output = data.created ? "Created new file." : "File updated.";
      if (data.formatted) output += " Auto-formatted.";
      // v0.27.1: Rust returns `no_op: true` when post-write content is
      // byte-identical to the pre-write state (e.g. agent wrote the same
      // bytes that were already there, or a formatter normalized the
      // change away). Surface this so the agent doesn't see "File updated"
      // and assume real bytes changed. See GitHub #45.
      if (data.no_op === true) {
        output +=
          " No net change — the written content is byte-identical to what was already on disk.";
      }

      // Append inline diagnostics if present
      const diags = data.lsp_diagnostics as Array<Record<string, unknown>> | undefined;
      if (diags && diags.length > 0) {
        const errors = diags.filter((d) => d.severity === "error");
        if (errors.length > 0) {
          output += "\n\nLSP errors detected, please fix:\n";
          for (const d of errors) {
            output += `  Line ${d.line}: ${d.message}\n`;
          }
        }
      }

      // v0.17.3 honest reporting: when an LSP server didn't respond in time
      // or its process exited mid-edit, surface that to the agent so they
      // know diagnostics may be incomplete (rather than assuming silence
      // means "clean").
      const pendingServers = data.lsp_pending_servers as string[] | undefined;
      const exitedServers = data.lsp_exited_servers as string[] | undefined;
      if (pendingServers && pendingServers.length > 0) {
        output += `\n\nNote: LSP server(s) did not respond in time: ${pendingServers.join(", ")}. Diagnostics may be incomplete; rerun lsp_diagnostics later for a fresh check.`;
      }
      if (exitedServers && exitedServers.length > 0) {
        output += `\n\nNote: LSP server(s) exited during this edit: ${exitedServers.join(", ")}. Their diagnostics could not be collected.`;
      }

      // Store metadata for tool.execute.after hook (fromPlugin overwrites context.metadata)
      const diff = data.diff as
        | { before?: string; after?: string; additions?: number; deletions?: number }
        | undefined;
      const callID = getCallID(context);
      if (callID) {
        const dp = relativeToWorktree(filePath, context.worktree);
        const beforeContent = diff?.before ?? "";
        const afterContent = diff?.after ?? content;
        storeToolMetadata(context.sessionID, callID, {
          title: dp,
          metadata: {
            diff: buildUnifiedDiff(filePath, beforeContent, afterContent),
            filediff: {
              file: filePath,
              before: beforeContent,
              after: afterContent,
              additions: diff?.additions ?? 0,
              deletions: diff?.deletions ?? 0,
            },
            diagnostics: {},
          },
        });
      }

      return output;
    },
  };
}

// ---------------------------------------------------------------------------
// EDIT tool
// ---------------------------------------------------------------------------

function getEditDescription(writeToolName: string): string {
  return `Edit a file by finding and replacing text, or by targeting named symbols. To write or overwrite a whole file, use the \`${writeToolName}\` tool — \`edit\` requires an explicit edit mode and will not silently overwrite a file from \`content\` alone.

**Modes** (determined by which parameters you provide):

Mode priority: operations > appendContent > edits > symbol (without oldString) > oldString (find/replace). If none match, the call is rejected — there is no implicit "write" fallback.

1. **Multi-file transaction** — pass \`operations\` array
   Edits across multiple files with checkpoint-based rollback on failure.
   Each operation: \`{ "file": "path", "command": "edit_match" | "write", ... }\`.
   For \`edit_match\`: include \`match\`, \`replacement\`. For \`write\`: include \`content\`.
   Example: \`{ "operations": [{ "file": "a.ts", "command": "edit_match", "match": "old", "replacement": "new" }, { "file": "b.ts", "command": "write", "content": "..." }] }\`

2. **Append** — pass \`filePath\` + \`appendContent\`
   Appends text to the end of a file, creating the file if it does not exist.
   Example: \`{ "filePath": "notes.txt", "appendContent": "new line\\n" }\`

3. **Batch edits** — pass \`filePath\` + \`edits\` array
   Multiple edits in one file atomically. Each edit is either:
   - \`{ "oldString": "old", "newString": "new" }\` — find/replace
   - \`{ "startLine": 5, "endLine": 7, "content": "new lines" }\` — replace line range (1-based, both inclusive)
   Set content to empty string to delete lines.

4. **Symbol replace** — pass \`filePath\` + \`symbol\` + \`content\`
   Replaces an entire named symbol (function, class, type) with new content.
   Includes decorators, attributes, and doc comments in the replacement range.
   **Important:** You must NOT provide \`oldString\` when using symbol mode — if present, the tool silently falls back to find/replace mode.
   Example: \`{ "filePath": "src/app.ts", "symbol": "handleRequest", "content": "function handleRequest() { ... }" }\`

5. **Find and replace** — pass \`filePath\` + \`oldString\` + \`newString\`
   Finds the exact text in \`oldString\` and replaces it with \`newString\`.
   Supports fuzzy matching (handles whitespace differences automatically).
   If multiple matches exist, specify which one with \`occurrence\` or use \`replaceAll: true\`.
   Example: \`{ "filePath": "src/app.ts", "oldString": "const x = 1", "newString": "const x = 2" }\`

6. **Replace all occurrences** — add \`replaceAll: true\`
   Replaces every occurrence of \`oldString\` in the file.
   Example: \`{ "filePath": "src/app.ts", "oldString": "oldName", "newString": "newName", "replaceAll": true }\`

7. **Select specific occurrence** — add \`occurrence: N\` (0-indexed)
   When multiple matches exist, select the Nth one (0 = first, 1 = second, etc.).
   Example: \`{ "filePath": "src/app.ts", "oldString": "TODO", "newString": "DONE", "occurrence": 0 }\`

Note: Modes 6 and 7 are options on mode 5 (find/replace) — they require \`oldString\`.

**Behavior:**
- Backs up files before editing (recoverable via aft_safety undo)
- Auto-formats using project formatter if configured
- Tree-sitter syntax validation on all edits
- Symbol replace includes decorators, attributes, and doc comments in range
- LSP error-level diagnostics are returned automatically after edits

Returns: JSON string for the selected edit mode. Edits may append inline LSP error lines.

Common response fields: success (boolean), diff (object with before/after), backup_id (string), syntax_valid (boolean). Exact fields vary by mode.`;
  // Note: The Returns section intentionally stays high-level because per-mode JSON shapes
  // vary by Rust command and documenting each would bloat the description for minimal gain.
  // Agents can parse the JSON response generically — key fields include 'success' and 'diff'.
}

function createEditTool(ctx: PluginContext, writeToolName = "write"): ToolDefinition {
  return {
    description: getEditDescription(writeToolName),
    args: {
      filePath: z
        .string()
        .optional()
        .describe(
          "Path to the file to edit (absolute or relative to project root). Required for all modes except 'operations' multi-file transactions",
        ),
      oldString: z.string().optional().describe("Text to find (exact match, with fuzzy fallback)"),
      newString: z
        .string()
        .optional()
        .describe("Text to replace with (omit or set to empty string to delete the matched text)"),
      replaceAll: z.boolean().optional().describe("Replace all occurrences"),
      occurrence: z
        .number()
        .optional()
        .describe("0-indexed occurrence to replace when multiple matches exist"),
      symbol: z.string().optional().describe("Named symbol to replace (function, class, type)"),
      content: z.string().optional().describe("New content for symbol replace or file write"),
      appendContent: z
        .string()
        .optional()
        .describe("Text to append to the end of filePath; creates the file if needed"),
      edits: z
        .array(z.record(z.string(), z.unknown()))
        .optional()
        .describe(
          "Batch edits — array of { oldString: string, newString: string } or { startLine: number (1-based), endLine: number (1-based, inclusive), content: string }",
        ),
      operations: z
        .array(z.record(z.string(), z.unknown()))
        .optional()
        .describe(
          "Transaction — array of { file: string, command: 'edit_match' | 'write', match?: string, replacement?: string, content?: string } for multi-file edits with rollback. Note: uses 'file'/'match'/'replacement' (not filePath/oldString/newString)",
        ),
    },
    execute: async (args, context): Promise<string> => {
      // Footgun guard: top-level startLine/endLine are not valid params on
      // edit. They only exist nested inside `edits[]` for batch line-range
      // mode. Without this guard, Zod silently strips the unknown keys and
      // the call falls through mode resolution to the content-only-write
      // branch, overwriting the entire file. Reject with a helpful pointer.
      const argsRecord = args as Record<string, unknown>;
      if (argsRecord.startLine !== undefined || argsRecord.endLine !== undefined) {
        throw new Error(
          "edit: 'startLine'/'endLine' are not top-level parameters. " +
            "For line-range edits, nest them inside the `edits` array: " +
            '`edits: [{ startLine: N, endLine: M, content: "..." }]`. ' +
            "For find/replace, use `oldString`/`newString` instead.",
        );
      }

      // Transaction mode — multi-file
      if (Array.isArray(args.operations)) {
        const ops = args.operations as Array<Record<string, unknown>>;
        const files = ops.map((op) => op.file as string).filter(Boolean);

        // External-directory check first (mirrors opencode-native edit.ts:68).
        {
          const asked = new Set<string>();
          for (const file of files) {
            const absPath = path.isAbsolute(file) ? file : path.resolve(context.directory, file);
            if (asked.has(absPath)) continue;
            asked.add(absPath);
            const denial = await assertExternalDirectoryPermission(context, absPath);
            if (denial) return permissionDeniedResponse(denial);
          }
        }

        await runAsk(
          context.ask({
            permission: "edit",
            patterns: files.map((f) =>
              path.relative(context.worktree, path.resolve(context.directory, f)),
            ),
            always: ["*"],
            metadata: {},
          }),
        );

        const resolvedOps = ops.map((op) => ({
          ...op,
          file: path.isAbsolute(op.file as string)
            ? op.file
            : path.resolve(context.directory, op.file as string),
        }));

        const response = await callBridge(ctx, context, "transaction", { operations: resolvedOps });
        if (response.success === false) {
          throw new Error((response.message as string | undefined) ?? "transaction failed");
        }
        return JSON.stringify(response);
      }

      const file = args.filePath as string;
      if (!file) throw new Error("'filePath' parameter is required");

      const filePath = path.isAbsolute(file) ? file : path.resolve(context.directory, file);

      const relPath = path.relative(context.worktree, filePath);

      // External-directory check first (mirrors opencode-native edit.ts:68).
      {
        const denial = await assertExternalDirectoryPermission(context, filePath);
        if (denial) return permissionDeniedResponse(denial);
      }

      await runAsk(
        context.ask({
          permission: "edit",
          patterns: [relPath],
          always: ["*"],
          metadata: { filepath: filePath },
        }),
      );

      const params: Record<string, unknown> = { file: filePath };

      // Route to appropriate Rust command
      let command: string;

      if (typeof args.appendContent === "string") {
        command = "edit_match";
        params.op = "append";
        params.append_content = args.appendContent;
        params.create_dirs = true;
      } else if (Array.isArray(args.edits)) {
        // Batch mode — translate camelCase to snake_case for Rust
        command = "batch";
        params.edits = (args.edits as Array<Record<string, unknown>>).map((edit) => {
          const translated: Record<string, unknown> = {};
          for (const [key, value] of Object.entries(edit)) {
            if (key === "oldString") translated.match = value;
            else if (key === "newString") translated.replacement = value;
            else if (key === "startLine") translated.line_start = value;
            else if (key === "endLine") translated.line_end = value;
            else translated[key] = value;
          }
          return translated;
        });
      } else if (
        typeof args.symbol === "string" &&
        typeof args.oldString !== "string" &&
        args.content !== undefined
      ) {
        // Symbol replace — only when content is provided and oldString is NOT present
        // (agents often pass symbol as "what to search for", not "replace whole symbol")
        command = "edit_symbol";
        params.symbol = args.symbol;
        params.operation = "replace";
        params.content = args.content;
      } else if (typeof args.oldString === "string") {
        // Find/replace mode — default newString to "" (deletion) if not provided
        command = "edit_match";
        params.match = args.oldString;
        params.replacement = args.newString ?? "";
        if (args.replaceAll !== undefined) params.replace_all = args.replaceAll;
        if (args.occurrence !== undefined) params.occurrence = args.occurrence;
      } else {
        // No mode-selecting parameter matched. We deliberately do NOT fall
        // through to a content-only "write" mode here, even when `content` is
        // present: that fallback was the disaster path (a typo or misnamed
        // param like top-level startLine could silently overwrite the whole
        // file). For full-file writes, use the dedicated `${writeToolName}`
        // tool, which is unambiguous about its destructive intent.
        const hint =
          typeof args.content === "string"
            ? ` To write the whole file, use the '${writeToolName}' tool. To edit existing content, provide 'oldString' (and optionally 'newString'), 'symbol' + 'content', or an 'edits' array.`
            : " Provide 'oldString' (+ optional 'newString'), 'symbol' + 'content', 'edits' array, or 'operations' array.";
        throw new Error(`edit: no edit mode resolved from arguments.${hint}`);
      }

      params.diagnostics = true;
      // Request diff from Rust for UI metadata (avoids extra file reads in TS)
      params.include_diff = true;

      const data = await callBridge(ctx, context, command, params);

      // Store metadata for tool.execute.after hook (fromPlugin overwrites context.metadata)
      if (data.success && data.diff) {
        const diff = data.diff as {
          before?: string;
          after?: string;
          additions?: number;
          deletions?: number;
        };
        const callID = getCallID(context);
        if (callID) {
          const dp = relativeToWorktree(filePath, context.worktree);
          const beforeContent = diff.before ?? "";
          const afterContent = diff.after ?? "";
          storeToolMetadata(context.sessionID, callID, {
            title: dp,
            metadata: {
              diff: buildUnifiedDiff(filePath, beforeContent, afterContent),
              filediff: {
                file: filePath,
                before: beforeContent,
                after: afterContent,
                additions: diff.additions ?? 0,
                deletions: diff.deletions ?? 0,
              },
              diagnostics: {},
            },
          });
        }
      }

      let result = JSON.stringify(data);

      const globSkipNote = formatGlobSkipReasonsNote(data.format_skip_reasons as unknown);
      if (globSkipNote) result += `\n\n${globSkipNote}`;

      // v0.27.1: surface `no_op: true` honestly. Rust sets this when the
      // post-write file content is byte-identical to the pre-write state —
      // either oldString === newString, a formatter normalized the change
      // away, or the replacement matched what was already in the file.
      // The match was satisfied (replacements > 0) but no net file change
      // landed. Without this note, agents see `+0/-0` and assume the tool
      // failed silently. See GitHub #45.
      if (data.no_op === true) {
        result +=
          "\n\nNote: no net file change — the match was found and applied, but the file content is byte-identical to before. Likely causes: oldString and newString are identical, or a formatter normalized the change away.";
      }

      // Append inline diagnostics to output (matching write tool pattern)
      const diags = data.lsp_diagnostics as Array<Record<string, unknown>> | undefined;
      if (diags && diags.length > 0) {
        const errors = diags.filter((d) => d.severity === "error");
        if (errors.length > 0) {
          const diagLines = errors.map((d) => `  Line ${d.line}: ${d.message}`).join("\n");
          result += `\n\nLSP errors detected, please fix:\n${diagLines}`;
        }
      }
      // v0.17.3 honest reporting: surface pending/exited servers so the
      // agent doesn't mistake silence for "all clear" when an LSP server
      // simply didn't respond before our wait_ms deadline.
      const pendingServers = data.lsp_pending_servers as string[] | undefined;
      const exitedServers = data.lsp_exited_servers as string[] | undefined;
      if (pendingServers && pendingServers.length > 0) {
        result += `\n\nNote: LSP server(s) did not respond in time: ${pendingServers.join(", ")}. Diagnostics may be incomplete; rerun lsp_diagnostics later for a fresh check.`;
      }
      if (exitedServers && exitedServers.length > 0) {
        result += `\n\nNote: LSP server(s) exited during this edit: ${exitedServers.join(", ")}. Their diagnostics could not be collected.`;
      }

      return result;
    },
  };
}

function formatGlobSkipReasonsNote(reasons: unknown): string | undefined {
  if (!Array.isArray(reasons)) return undefined;
  const actionable = reasons
    .filter((reason): reason is string => typeof reason === "string")
    .filter((reason) =>
      ["formatter_not_installed", "formatter_excluded_path", "timeout", "error"].includes(reason),
    );
  if (actionable.length === 0) return undefined;
  return `Note: formatter skipped some glob edit result file(s): ${[...new Set(actionable)].sort().join(", ")}. See per-file format_skipped_reason values for details.`;
}

// ---------------------------------------------------------------------------
// APPLY_PATCH tool
// ---------------------------------------------------------------------------

const APPLY_PATCH_DESCRIPTION = `Use the \`apply_patch\` tool to edit files. Your patch language is a stripped‑down, file‑oriented diff format designed to be easy to parse and safe to apply. You can think of it as a high‑level envelope:

*** Begin Patch
[ one or more file sections ]
*** End Patch

Within that envelope, you get a sequence of file operations.
You MUST include a header to specify the action you are taking.
Each operation starts with one of three headers:

*** Add File: <path> - create a new file. Every following line is a + line (the initial contents).
*** Delete File: <path> - remove an existing file. Nothing follows.
*** Update File: <path> - patch an existing file in place (optionally with a rename).
*** Move to: <path> - after update file header, renames the file.


Example patch:

\`\`\`
*** Begin Patch
*** Add File: hello.txt
+Hello world
*** Update File: src/app.py
*** Move to: src/main.py
@@ def greet():
-print("Hi")
+print("Hello, world!")
*** Delete File: obsolete.txt
*** End Patch
\`\`\`

**Behavior:**
- All file changes are applied with checkpoint-based rollback — if any file fails, previous changes are rolled back (best-effort)
- Files are backed up before modification
- Parent directories are created automatically for new files
- Fuzzy matching for context anchors (handles whitespace and Unicode differences)

**It is important to remember:**

- You must include a header with your intended action (Add/Delete/Update)
- You must prefix new lines with \`+\` even when creating a new file

Returns: Status message string listing created, updated, moved, deleted, or failed file operations. May include inline LSP errors if type errors are introduced by the patch.`;

function createApplyPatchTool(ctx: PluginContext): ToolDefinition {
  return {
    description: APPLY_PATCH_DESCRIPTION,
    args: {
      patchText: z.string().describe("The full patch text including Begin/End markers"),
    },
    execute: async (args, context): Promise<string> => {
      const patchText = args.patchText as string;
      if (!patchText) throw new Error("'patchText' is required");

      // Parse the patch
      let hunks: import("../patch-parser.js").Hunk[];
      try {
        hunks = parsePatch(patchText);
      } catch (e) {
        throw new Error(`Patch parse error: ${e instanceof Error ? e.message : e}`);
      }

      if (hunks.length === 0) {
        throw new Error("Empty patch: no file operations found");
      }

      // Resolve every path this patch touches — SOURCES (h.path) and
      // DESTINATIONS (h.move_path for move hunks). Move destinations have to
      // be tracked because the old code only checkpointed sources; a partial
      // move that succeeded at the destination but failed at source deletion
      // left orphan files behind that rollback never cleaned up (audit #8).
      const affectedAbs = new Set<string>();
      // Files that did NOT exist before this patch — add targets plus move
      // destinations whose path was empty. On rollback we delete these
      // instead of restoring content that was never there.
      const newlyCreatedAbs = new Set<string>();

      for (const h of hunks) {
        const srcAbs = path.resolve(context.directory, h.path);
        affectedAbs.add(srcAbs);
        if (h.type === "add") {
          newlyCreatedAbs.add(srcAbs);
        }
        if (h.type === "update" && h.move_path) {
          const dstAbs = path.resolve(context.directory, h.move_path);
          affectedAbs.add(dstAbs);
          // Snapshot the destination if it exists so rollback restores the
          // original contents. If it doesn't exist, track it as newly
          // created so rollback removes it.
          if (!fs.existsSync(dstAbs)) {
            newlyCreatedAbs.add(dstAbs);
          }
        }
      }

      const relPaths = Array.from(affectedAbs).map((abs) => path.relative(context.worktree, abs));
      const multiFileWritePaths = Array.from(affectedAbs);

      // External-directory check first (mirrors opencode-native patch.ts:298).
      {
        const asked = new Set<string>();
        for (const filePath of multiFileWritePaths) {
          if (asked.has(filePath)) continue;
          asked.add(filePath);
          const denial = await assertExternalDirectoryPermission(context, filePath);
          if (denial) return permissionDeniedResponse(denial);
        }
      }

      await runAsk(
        context.ask({
          permission: "edit",
          patterns: relPaths,
          always: ["*"],
          metadata: {},
        }),
      );

      // Pre-patch checkpoint covers files that exist pre-patch (so the
      // agent can `aft_safety` undo if they want to abort after seeing a
      // partial result). Newly-created targets are deleted to revert.
      // Checkpoint failure is non-fatal — agent can still inspect partial
      // results and proceed.
      const checkpointPaths = Array.from(affectedAbs).filter((abs) => !newlyCreatedAbs.has(abs));
      const checkpointName = `apply_patch_${Date.now()}`;
      let checkpointCreated = false;
      if (checkpointPaths.length > 0) {
        try {
          await callBridge(ctx, context, "checkpoint", {
            name: checkpointName,
            files: checkpointPaths,
          });
          checkpointCreated = true;
        } catch {
          // Checkpoint failure: agent loses the easy `aft_safety` undo
          // path but the patch still attempts each hunk independently.
        }
      }

      // Process each hunk, track per-file diffs for metadata.
      // additions/deletions come from the Rust-side `similar`-crate diff
      // (returned via `include_diff: true` on the write call) — same source
      // as the edit/write tools, which produce correct counts. Avoid
      // recomputing via TS-side LCS to keep one source of truth (issue: the
      // `apply_patch` UI was reporting +N/-N≈filesize counts because the
      // local count was diverging from the Rust truth).
      //
      // PER-FILE COMMIT MODEL (BUG-6a, dogfooding fix): each hunk commits
      // independently. A failure on one file no longer rolls back the
      // others. The pre-patch checkpoint is still created so the agent can
      // use `aft_safety` to revert successful files manually if they want
      // to abort the whole patch after seeing a partial result.
      //
      // Why this changed: an agent submitted a 3-file patch where 2 files
      // patched cleanly and the 3rd hit a fuzzy-match drift. The old
      // atomic-rollback discarded the 2 successes, so the agent had to
      // re-issue the same patch with the failing file removed — exactly
      // the per-file commit semantics, just done by hand. The ergonomic
      // fix is to give them per-file commit out of the box.
      const results: string[] = [];
      const failures: string[] = [];
      const perFileDiffs: Array<{
        filePath: string;
        before: string;
        after: string;
        additions: number;
        deletions: number;
      }> = [];

      for (const hunk of hunks) {
        const filePath = path.resolve(context.directory, hunk.path);

        switch (hunk.type) {
          case "add": {
            // *** Add File: <path> means CREATE; refuse to overwrite an existing
            // file. The unified `write` bridge command silently overwrites by
            // design (it's the back-end for both `write` and `apply_patch`'s
            // create-or-overwrite flow), so the existence check has to happen
            // here, in the apply_patch wrapper. Without it, an Add hunk against
            // a path that already exists would clobber the file's contents and
            // the agent would see a misleading "Created <path>" success.
            if (fs.existsSync(filePath)) {
              const msg = `Failed to create ${hunk.path}: file already exists. Use *** Update File: to modify, or *** Delete File: first if you want to replace it entirely.`;
              results.push(msg);
              failures.push(hunk.path);
              break;
            }
            try {
              const content = hunk.contents.endsWith("\n") ? hunk.contents : `${hunk.contents}\n`;
              const writeResult = await callBridge(ctx, context, "write", {
                file: filePath,
                content,
                create_dirs: true,
                diagnostics: true,
                include_diff: true,
                multi_file_write_paths: multiFileWritePaths,
              });
              const wrDiff = writeResult.diff as
                | { before?: string; after?: string; additions?: number; deletions?: number }
                | undefined;
              perFileDiffs.push({
                filePath,
                before: "",
                after: hunk.contents,
                // For a brand-new file, additions = total lines, deletions = 0.
                // Prefer Rust counts; fall back to a content line count if the
                // bridge didn't include a diff (e.g. older binary).
                additions: wrDiff?.additions ?? lineCount(content),
                deletions: wrDiff?.deletions ?? 0,
              });
              results.push(`Created ${hunk.path}`);
            } catch (e) {
              const msg = `Failed to create ${hunk.path}: ${e instanceof Error ? e.message : e}`;
              results.push(msg);
              failures.push(hunk.path);
              // The write may have left a partial file on disk for an `add`
              // hunk. Best-effort cleanup so we don't leave orphan partials.
              // (Failures here are tolerated: the agent will see the
              // creation failure in `results` either way.)
              const filePath = path.resolve(context.directory, hunk.path);
              if (fs.existsSync(filePath)) {
                try {
                  await callBridge(ctx, context, "delete_file", { file: filePath });
                } catch {
                  // ignore — surfaced through the parent failure already
                }
              }
            }
            break;
          }

          case "delete": {
            try {
              const before = await fs.promises.readFile(filePath, "utf-8").catch(() => "");
              await callBridge(ctx, context, "delete_file", { file: filePath });
              // delete_file doesn't return a diff. The counts are unambiguous:
              // every prior line is a deletion; nothing is added.
              perFileDiffs.push({
                filePath,
                before,
                after: "",
                additions: 0,
                deletions: lineCount(before),
              });
              results.push(`Deleted ${hunk.path}`);
            } catch (e) {
              results.push(`Failed to delete ${hunk.path}: ${e instanceof Error ? e.message : e}`);
              failures.push(hunk.path);
            }
            break;
          }

          case "update": {
            try {
              // Read original, apply chunks, write back
              const original = await fs.promises.readFile(filePath, "utf-8");
              const newContent = applyUpdateChunks(original, filePath, hunk.chunks);

              const targetPath = hunk.move_path
                ? path.resolve(context.directory, hunk.move_path)
                : filePath;

              const writeResult = await callBridge(ctx, context, "write", {
                file: targetPath,
                content: newContent,
                create_dirs: true,
                diagnostics: true,
                include_diff: true,
                multi_file_write_paths: multiFileWritePaths,
              });

              // Collect diagnostics from this file
              const diags = writeResult.lsp_diagnostics as
                | Array<Record<string, unknown>>
                | undefined;
              if (diags && diags.length > 0) {
                const errors = diags.filter((d) => d.severity === "error");
                if (errors.length > 0) {
                  const relPath = path.relative(context.worktree, targetPath);
                  const diagLines = errors.map((d) => `  Line ${d.line}: ${d.message}`).join("\n");
                  results.push(`\nLSP errors detected in ${relPath}, please fix:\n${diagLines}`);
                }
              }

              // Track per-file diff for metadata. For a regular update the
              // Rust write diff compares disk-before vs new content, which
              // matches what we want. For a *move*, write goes to a fresh
              // target (no prior content), so Rust would report the whole
              // file as additions; we recompute via TS-side LCS instead.
              // For non-move updates we still recompute as a fallback when
              // the bridge didn't include a diff (older binary or a test
              // mock without diff support).
              const wrDiff = writeResult.diff as
                | { before?: string; after?: string; additions?: number; deletions?: number }
                | undefined;
              const isMove = Boolean(hunk.move_path);
              const { additions, deletions } =
                isMove || wrDiff?.additions === undefined || wrDiff.deletions === undefined
                  ? countDiffLines(original, newContent)
                  : {
                      additions: wrDiff.additions,
                      deletions: wrDiff.deletions,
                    };
              perFileDiffs.push({
                filePath,
                before: original,
                after: newContent,
                additions,
                deletions,
              });

              if (hunk.move_path) {
                try {
                  const deleteResult = await callBridge(ctx, context, "delete_file", {
                    file: filePath,
                  });
                  if (deleteResult.success === false) {
                    throw new Error(
                      (deleteResult.message as string | undefined) ?? "delete failed",
                    );
                  }
                } catch (deleteError) {
                  try {
                    if (!checkpointCreated) {
                      throw new Error("pre-patch checkpoint was not created");
                    }
                    const rollbackResult = await callBridge(ctx, context, "restore_checkpoint", {
                      name: checkpointName,
                    });
                    if (rollbackResult.success === false) {
                      throw new Error(
                        (rollbackResult.message as string | undefined) ??
                          "checkpoint restore failed",
                      );
                    }
                    if (newlyCreatedAbs.has(targetPath) && fs.existsSync(targetPath)) {
                      const cleanupResult = await callBridge(ctx, context, "delete_file", {
                        file: targetPath,
                      });
                      if (cleanupResult.success === false) {
                        throw new Error(
                          (cleanupResult.message as string | undefined) ??
                            "new destination cleanup failed",
                        );
                      }
                    }
                  } catch (rollbackError) {
                    throw new Error(
                      `success: false; code: move_partial_failure; files: [${filePath}, ${targetPath}]; wrote destination ${targetPath}, but failed to delete source ${filePath} (${formatError(deleteError)}) and failed to restore pre-patch checkpoint ${checkpointName} (${formatError(rollbackError)}). Both copies may exist or destination content may be changed: ${filePath}, ${targetPath}`,
                    );
                  }
                  throw new Error(
                    `source delete failed after writing move destination; restored pre-patch checkpoint ${checkpointName}: ${formatError(deleteError)}`,
                  );
                }
                results.push(`Updated and moved ${hunk.path} → ${hunk.move_path}`);
              } else {
                results.push(`Updated ${hunk.path}`);
              }
            } catch (e) {
              results.push(`Failed to update ${hunk.path}: ${e instanceof Error ? e.message : e}`);
              failures.push(hunk.path);
              break;
            }
            break;
          }
        }
      }

      // PER-FILE COMMIT (BUG-6a): no atomic rollback. The pre-patch
      // checkpoint stays available so the agent can `aft_safety` revert
      // successful files manually if they want to abort the whole patch
      // after seeing a partial outcome.
      //
      // Each hunk type self-recovers cleanly on failure:
      //   - add: the partial file (if any) is deleted in the catch block
      //          above so we don't leave orphan partials
      //   - update: applyUpdateChunks throws BEFORE write when fuzzy match
      //             can't find the lines, so the original file is intact
      //             on disk. write failures are also pre-commit at the
      //             bridge level (bridge does its own backup).
      //   - delete: failed delete leaves the file in place — no cleanup
      //             needed
      //
      // Surface a clear failure summary at the end so the agent can see
      // which hunks failed and decide whether to retry just those, without
      // scanning the per-hunk lines.
      if (failures.length > 0) {
        const partial = failures.length < hunks.length;
        const summary = partial
          ? `Patch partially applied — ${hunks.length - failures.length} of ${hunks.length} hunk(s) succeeded. Failed: ${failures.join(", ")}. Successful changes are kept; use \`aft_safety\` to revert if you want to abort.`
          : `Patch failed — none of the ${hunks.length} hunk(s) applied: ${failures.join(", ")}.`;
        results.push(summary);
        // Total-failure case: throw so OpenCode marks the tool call as errored
        // in the UI (state.status = "error") and the agent's retry loop sees
        // a real failure. Returning the failure summary as a normal string
        // makes OpenCode classify the call as completed/successful — the
        // agent only sees the failure in the output text, and the UI shows
        // a green check next to a red error message. This matches OpenCode's
        // native apply_patch which uses Effect.fail() on every error path
        // (packages/opencode/src/tool/apply_patch.ts).
        //
        // Partial successes still return the string: real changes landed on
        // disk, the agent needs to see exactly which hunks worked, and the
        // tool genuinely did do work. Treating it as an error would obscure
        // the partial outcome.
        if (!partial) {
          throw new Error(results.join("\n"));
        }
      }

      // Store metadata for tool.execute.after hook (match opencode built-in format)
      const callID = getCallID(context);
      if (callID) {
        // Index per-file diffs by absolute filePath for fast lookup when
        // building the metadata.files array. Each entry NEEDS to carry the
        // per-file `patch` string plus `additions`/`deletions` counts —
        // OpenCode's UI patchFile() at packages/ui/src/components/apply-patch-file.ts
        // returns undefined for any file metadata that lacks all of `patch`,
        // `before`, and `after`. Without this enrichment, the UI silently
        // dropped every file entry and rendered no diffs (v0.15.2 fix for
        // the "apply_patch shows no diff in TUI/UI" report).
        const diffByPath = new Map(perFileDiffs.map((d) => [d.filePath, d]));

        // Build per-file metadata. OpenCode's apply_patch shape (see
        // packages/opencode/src/tool/apply_patch.ts:188) per file:
        //   { filePath, relativePath, type, patch, additions, deletions, movePath? }
        // `type` is normalised to "move" when an update hunk has a move target,
        // so the UI can label the row correctly.
        //
        // additions/deletions come from perFileDiffs, which were populated
        // from the Rust-side `similar`-crate diff (via include_diff:true on
        // each write call). This matches edit/write tool counts exactly.
        // The TS-side LCS via buildUnifiedDiff is still used to build the
        // *display* `patch` text — the diff is correct visually; only the
        // line-count derivation through countAddDel was producing wrong
        // numbers (e.g. +399/-400 for a single-line removal). See
        // perFileDiffs population above for how counts are derived per
        // hunk type.
        const files = hunks.map((h) => {
          const filePath = path.resolve(context.directory, h.path);
          // `move_path` only exists on UpdateHunk variants — narrow first.
          const rawMovePath = h.type === "update" ? h.move_path : undefined;
          const movePath = rawMovePath ? path.resolve(context.directory, rawMovePath) : undefined;
          // For moved files, render the destination path as the visible
          // location (matches OpenCode's apply_patch behaviour).
          const displayPath = movePath ?? filePath;
          const relPath = path.relative(context.worktree, displayPath);

          const diffEntry = diffByPath.get(filePath);
          const patch = diffEntry
            ? buildUnifiedDiff(displayPath, diffEntry.before, diffEntry.after)
            : "";
          const additions = diffEntry?.additions ?? 0;
          const deletions = diffEntry?.deletions ?? 0;

          // Normalise type for UI: an "update" hunk with a move target is a
          // move, otherwise keep the parsed type as-is.
          const uiType: "add" | "update" | "delete" | "move" =
            h.type === "update" && rawMovePath ? "move" : h.type;

          return {
            filePath,
            relativePath: relPath,
            type: uiType,
            patch,
            additions,
            deletions,
            ...(movePath ? { movePath } : {}),
          };
        });

        // Build title matching built-in: "Success. Updated the following files:\nM path/to/file.ts"
        const fileList = files
          .map((f) => {
            const prefix = f.type === "add" ? "A" : f.type === "delete" ? "D" : "M";
            return `${prefix} ${f.relativePath}`;
          })
          .join("\n");
        const title = `Success. Updated the following files:\n${fileList}`;

        // Aggregate unified diff for the top-level metadata.diff field
        // (OpenCode's renderer also uses this for some views).
        const diffText = files
          .map((f) => f.patch)
          .filter(Boolean)
          .join("\n");

        storeToolMetadata(context.sessionID, callID, {
          title,
          metadata: {
            diff: diffText,
            files,
          },
        });
      }

      return results.join("\n");
    },
  };
}

// ---------------------------------------------------------------------------
// Delete
// ---------------------------------------------------------------------------

const DELETE_DESCRIPTION =
  "Delete one or more files (or directories) with backup.\n\n" +
  "Each file is backed up before deletion — use aft_safety undo to recover any of them. " +
  "For directories, every file inside is individually backed up before the tree is removed.\n\n" +
  "Directory deletion requires recursive: true. Without it, passing a directory returns an error.\n\n" +
  "Returns: { success, complete, deleted: [paths], skipped_files: [{file, reason}] }. " +
  "Partial success is allowed: files that can be deleted are deleted; files that fail " +
  "(missing, permission denied, etc.) are reported in skipped_files. " +
  "`complete: false` indicates at least one file was skipped.";

function createDeleteTool(ctx: PluginContext): ToolDefinition {
  return {
    description: DELETE_DESCRIPTION,
    args: {
      files: z
        .array(z.string())
        .min(1)
        .describe("Paths to delete (one or more). May include directories when recursive=true."),
      recursive: z
        .boolean()
        .optional()
        .describe(
          "Required to delete a directory and its contents. Defaults to false; passing a directory without this returns an error.",
        ),
    },
    execute: async (args, context): Promise<string> => {
      const inputs = args.files as string[];
      const recursive = args.recursive === true;
      const absolutePaths = inputs.map((f) =>
        path.isAbsolute(f) ? f : path.resolve(context.directory, f),
      );

      // External-directory check first (mirrors opencode-native edit.ts:68).
      {
        const asked = new Set<string>();
        for (const filePath of absolutePaths) {
          if (asked.has(filePath)) continue;
          asked.add(filePath);
          const denial = await assertExternalDirectoryPermission(context, filePath);
          if (denial) return permissionDeniedResponse(denial);
        }
      }

      await runAsk(
        context.ask({
          permission: "edit",
          patterns: absolutePaths,
          always: ["*"],
          metadata: { action: "delete", count: absolutePaths.length },
        }),
      );

      // Single batched call so every file shares one op_id; one `aft_safety
      // undo` then restores the whole delete atomically.
      const response = await callBridge(ctx, context, "delete_file", {
        files: absolutePaths,
        recursive,
      });

      if (response.success === false) {
        throw new Error((response.message as string | undefined) ?? "delete failed");
      }

      const deletedEntries = (response.deleted as Array<{ file: string }> | undefined) ?? [];
      const skipped =
        (response.skipped_files as Array<{ file: string; reason: string }> | undefined) ?? [];
      const deleted = deletedEntries.map((entry) => entry.file);

      // Refuse a fully-failed batch with a real error so the agent surface
      // doesn't silently render "completed" for nothing-actually-deleted.
      if (deleted.length === 0 && skipped.length > 0) {
        throw new Error(
          `delete failed for all ${skipped.length} file(s):\n` +
            skipped.map((entry) => `  ${entry.file}: ${entry.reason}`).join("\n"),
        );
      }

      return JSON.stringify({
        success: true,
        complete: skipped.length === 0,
        deleted,
        skipped_files: skipped,
      });
    },
  };
}

// ---------------------------------------------------------------------------
// Move / Rename
// ---------------------------------------------------------------------------

const MOVE_DESCRIPTION =
  "Move or rename a file with backup. Creates parent directories for destination automatically\n" +
  "Note: This moves/renames files at the OS level.";

function createMoveTool(ctx: PluginContext): ToolDefinition {
  return {
    description: MOVE_DESCRIPTION,
    args: {
      filePath: z.string().describe("Source file path to move"),
      destination: z.string().describe("Destination file path"),
    },
    execute: async (args, context): Promise<string> => {
      const filePath = path.isAbsolute(args.filePath as string)
        ? (args.filePath as string)
        : path.resolve(context.directory, args.filePath as string);
      const destPath = path.isAbsolute(args.destination as string)
        ? (args.destination as string)
        : path.resolve(context.directory, args.destination as string);

      // External-directory check first (mirrors opencode-native edit.ts:68).
      {
        const sourceDenial = await assertExternalDirectoryPermission(context, filePath, {
          kind: "file",
        });
        if (sourceDenial) return permissionDeniedResponse(sourceDenial);
        if (destPath !== filePath) {
          const destDenial = await assertExternalDirectoryPermission(context, destPath);
          if (destDenial) return permissionDeniedResponse(destDenial);
        }
      }

      await runAsk(
        context.ask({
          permission: "edit",
          patterns: [filePath, destPath],
          always: ["*"],
          metadata: { action: "move" },
        }),
      );

      const result = await callBridge(ctx, context, "move_file", {
        file: filePath,
        destination: destPath,
      });
      if (result.success === false) {
        throw new Error((result.message as string) || "move failed");
      }
      return JSON.stringify(result);
    },
  };
}

// ---------------------------------------------------------------------------
// Exports
// ---------------------------------------------------------------------------

/**
 * Returns hoisted tools keyed by opencode's built-in names.
 * Overrides: read, write, edit, apply_patch (always when hoisting is on).
 *
 * Bash hoisting is opt-in: `bash`, `bash_status`, and `bash_kill` are
 * registered together when at least one `experimental.bash.*` flag is
 * enabled (rewrite, compress, or background). When all flags are off,
 * opencode's native bash stays in place — users without bash experimentals
 * get zero AFT code in their bash path.
 *
 * `bash_status` and `bash_kill` ride alongside `bash` regardless of which
 * experimental flag enabled it: foreground bash auto-promotes long-running
 * tasks to background after a short wait-window (v0.20+), so the agent
 * always needs a way to inspect or kill those promoted tasks. The
 * `experimental.bash.background` flag only gates explicit
 * `bash({ background: true })` spawning, not promotion.
 */
export function hoistedTools(ctx: PluginContext): Record<string, ToolDefinition> {
  const tools: Record<string, ToolDefinition> = {
    read: createReadTool(ctx),
    write: createWriteTool(ctx, "edit"),
    edit: createEditTool(ctx, "write"),
    apply_patch: createApplyPatchTool(ctx),
    aft_delete: createDeleteTool(ctx),
    aft_move: createMoveTool(ctx),
  };

  // Bash hoisting is gated by the single resolved bash config — see
  // `resolveBashConfig` in config.ts for the precedence rules (top-level
  // `bash` wins over legacy `experimental.bash.*`, surface defaults fill in
  // when neither is set). When enabled, `bash_status` and `bash_kill`
  // register alongside `bash` so the agent can always inspect and kill
  // auto-promoted background tasks regardless of which sub-feature was
  // actually requested.
  if (resolveBashConfig(ctx.config).enabled) {
    tools.bash = createBashTool(ctx);
    tools.bash_status = createBashStatusTool(ctx);
    tools.bash_kill = createBashKillTool(ctx);
  }

  return tools;
}

function formatError(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}

/**
 * Returns the same tools with aft_ prefix (for when hoisting is disabled).
 */
export function aftPrefixedTools(ctx: PluginContext): Record<string, ToolDefinition> {
  const aftEditTool = createEditTool(ctx, "aft_write");

  const tools: Record<string, ToolDefinition> = {
    aft_read: createReadTool(ctx),
    aft_write: createWriteTool(ctx, "aft_edit"),
    aft_edit: {
      ...aftEditTool,
      // Returns the inner aft_edit tool's result OR a JSON envelope string for
      // the legacy mode:"write" shim. Newer @opencode-ai/plugin versions
      // widened ToolResult from `string` to `string | { output, metadata? }`,
      // so we accept both shapes here; the OpenCode runtime handles both.
      execute: async (args, context) => {
        const argRecord = args as Record<string, unknown>;
        // Legacy back-compat: callers (mostly older tests/integrations) used
        // `{ mode, file, ... }` instead of the current schema. Translate
        // `file` -> `filePath` so the rest of the wrapper sees the modern
        // shape. The current edit tool ignores the `mode` field; we keep it
        // in the args object only so the explicit `mode: "write"` branch
        // below can detect it.
        const normalizedArgs: Record<string, unknown> =
          argRecord.mode !== undefined &&
          argRecord.filePath === undefined &&
          typeof argRecord.file === "string"
            ? { ...argRecord, filePath: argRecord.file }
            : { ...argRecord };

        // Explicit legacy `mode: "write"` — route directly to the Rust
        // `write` command. We do NOT fall through to the modern edit tool
        // here, because the modern tool deliberately rejects content-only
        // calls (the v0.17.2 footgun fix). Legacy `mode: "write"` is an
        // *explicit* whole-file write request, which is fine; the danger is
        // *implicit* whole-file writes where a typo in another mode-selecting
        // param silently degrades into overwrite. Returns the same JSON
        // envelope shape the legacy callers expect (success / file /
        // syntax_valid / etc.), not the human-readable string the modern
        // `write` tool returns.
        if (
          normalizedArgs.mode === "write" &&
          typeof normalizedArgs.filePath === "string" &&
          typeof normalizedArgs.content === "string"
        ) {
          const file = normalizedArgs.filePath as string;
          const filePath = path.isAbsolute(file) ? file : path.resolve(context.directory, file);
          const relPath = path.relative(context.worktree, filePath);

          // External-directory check first (mirrors opencode-native write.ts:43).
          {
            const denial = await assertExternalDirectoryPermission(context, filePath);
            if (denial) return permissionDeniedResponse(denial);
          }

          await runAsk(
            context.ask({
              permission: "edit",
              patterns: [relPath],
              always: ["*"],
              metadata: { filepath: filePath },
            }),
          );
          const writeParams: Record<string, unknown> = {
            file: filePath,
            content: normalizedArgs.content as string,
            create_dirs: normalizedArgs.create_dirs !== false,
            diagnostics: true,
          };
          const response = await callBridge(ctx, context, "write", writeParams);
          if (response.success === false) {
            throw new Error((response.message as string | undefined) ?? "write failed");
          }
          return JSON.stringify(response);
        }

        return aftEditTool.execute(normalizedArgs, context);
      },
    },
    aft_apply_patch: createApplyPatchTool(ctx),
    aft_delete: createDeleteTool(ctx),
    aft_move: createMoveTool(ctx),
  };

  // Hoist-off mode: same gating as hoisted mode but with the aft_ prefix on
  // the primary bash tool so it doesn't override OpenCode's native bash.
  // The sibling status/kill tools keep their unprefixed names because they
  // refer to AFT-spawned task IDs that the native bash doesn't know about.
  if (resolveBashConfig(ctx.config).enabled) {
    tools.aft_bash = createBashTool(ctx);
    tools.bash_status = createBashStatusTool(ctx);
    tools.bash_kill = createBashKillTool(ctx);
  }

  return tools;
}
