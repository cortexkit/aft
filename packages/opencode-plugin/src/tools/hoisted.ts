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

import * as path from "node:path";
import { coerceBoolean, coerceStringArray, toolErrorFromResponse } from "@cortexkit/aft-bridge";
import type { ToolContext, ToolDefinition, ToolResult } from "@opencode-ai/plugin";
import { tool } from "@opencode-ai/plugin";
import { resolveBashConfig } from "../config.js";
import { prepareToolMap } from "../normalize-schemas.js";
import { resolvePromptContext } from "../shared/last-assistant-model.js";
import type { PluginContext } from "../types.js";
import {
  callToolCall,
  coerceOptionalInt,
  optionalInt,
  resolvePathFromProjectRoot,
  resolveProjectRoot,
} from "./_shared.js";
import { createBashKillTool, createBashStatusTool, createBashTool } from "./bash.js";
import { createBashWatchTool } from "./bash_watch.js";
import { createBashWriteTool } from "./bash_write.js";
import {
  askEditPermission,
  assertExternalDirectoryPermission,
  classifyPermissionError,
  permissionDeniedResponse,
  permissionPath,
  permissionRuleDenial,
  runAsk,
} from "./permissions.js";

/** Get relative path matching opencode's format — the desktop UI parses it to extract filename + dir. */
function relativeToWorktree(fp: string, worktree: string): string {
  return path.relative(worktree, fp);
}

type ReadAttachment = {
  kind?: unknown;
  mime?: unknown;
  data?: unknown;
  bytes?: unknown;
  width?: unknown;
  height?: unknown;
  resized?: unknown;
};

function readAttachments(data: Record<string, unknown>): ReadAttachment[] {
  return Array.isArray(data.attachments) ? (data.attachments as ReadAttachment[]) : [];
}

const ISSUE_AND_PR_READ_DESCRIPTION =
  "GitHub issues and pull requests can be read with `issue://NUMBER` and `pr://NUMBER` (or `issue://OWNER/REPO/NUMBER` and `pr://OWNER/REPO/NUMBER`).";

/** Reuse the user-tier gh_read description gate across every GitHub-capable tool. */
export function whenGhReadEnabled(enabled: boolean, description: string): string {
  return enabled ? description : "";
}

type OpenCodeModelCatalogEntry = {
  attachment?: unknown;
  modalities?: { input?: unknown };
};

function asRecord(value: unknown): Record<string, unknown> | undefined {
  return typeof value === "object" && value !== null && !Array.isArray(value)
    ? (value as Record<string, unknown>)
    : undefined;
}

function visionCapabilityForOpenCodeModel(model: unknown): boolean | undefined {
  const entry = model as OpenCodeModelCatalogEntry | undefined;
  if (Array.isArray(entry?.modalities?.input)) {
    return entry.modalities.input.includes("image");
  }
  return typeof entry?.attachment === "boolean" ? entry.attachment : undefined;
}

function modelFromOpenCodeProvider(provider: Record<string, unknown>, modelID: string): unknown {
  const models = provider.models;
  if (Array.isArray(models)) {
    return models.find((model) => asRecord(model)?.id === modelID);
  }
  return asRecord(models)?.[modelID];
}

/**
 * Resolve the current session model for this call instead of retaining a bind-time
 * capability. A missing model catalog entry deliberately remains unspecified so
 * the server's safe text-only default applies.
 */
async function currentSessionVisionCapability(
  client: unknown,
  sessionID: string | undefined,
): Promise<boolean | undefined> {
  if (!sessionID) return undefined;
  const promptContext = await resolvePromptContext(client, sessionID);
  const currentModel = promptContext?.model;
  if (!currentModel) return undefined;

  const providerApi = (client as { provider?: { list?: () => Promise<unknown> } }).provider;
  if (typeof providerApi?.list !== "function") return undefined;

  let listed: unknown;
  try {
    listed = await providerApi.list();
  } catch {
    return undefined;
  }
  const result = asRecord(listed);
  const catalog = asRecord(result?.data) ?? result;
  const providers = Array.isArray(catalog?.all)
    ? catalog.all
    : Array.isArray(catalog?.providers)
      ? catalog.providers
      : [];
  const provider = providers.map(asRecord).find((entry) => entry?.id === currentModel.providerID);
  if (!provider) return undefined;

  return visionCapabilityForOpenCodeModel(
    modelFromOpenCodeProvider(provider, currentModel.modelID),
  );
}

/**
 * Keep OpenCode's persisted tool input compatible with its file-tool display.
 * The bridge payload remains separately constructed from the canonical path.
 */
function persistFilePathAlias(args: Record<string, unknown>, context: ToolContext): void {
  if (typeof args.path === "string" && !Object.hasOwn(args, "filePath")) {
    args.filePath = args.path;
  }
  context.metadata({ metadata: {} });
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
- Supported images (PNG, JPEG, GIF, WebP) and PDFs are returned as tool attachments; range arguments are ignored for media
- Directories return sorted entries with trailing / for subdirectories

Examples:
  Read full file: { "path": "src/app.ts" }
  Read lines 50-100: { "path": "src/app.ts", "startLine": 50, "endLine": 100 }
  Read 30 lines from line 200: { "path": "src/app.ts", "offset": 200, "limit": 30 }
  List directory: { "path": "src/" }
`;

function readDescription(ghReadEnabled: boolean): string {
  const githubDescription = whenGhReadEnabled(ghReadEnabled, ISSUE_AND_PR_READ_DESCRIPTION);
  if (!githubDescription) return READ_DESCRIPTION;
  return READ_DESCRIPTION.replace("\nExamples:", `\n${githubDescription}\n\nExamples:`);
}

/**
 * Creates the simple read tool. Registers as "read" when hoisted, "aft_read" when not.
 */
export function createReadTool(ctx: PluginContext): ToolDefinition {
  return prepareToolMap({
    read: {
      description: readDescription(ctx.config.gh_read?.enabled === true),
      args: {
        // OpenCode-only exception to the canonical `path` vocabulary: the
        // hoisted read/write/edit trio overrides OpenCode's built-in tools,
        // and the host UI renders headers from the recorded model input's
        // `filePath` verbatim (no fallback, and the record is taken from the
        // raw model stream before any plugin hook). Advertising `path` here
        // blanks every file header in the OpenCode UI; `path` is still
        // accepted silently at runtime.
        filePath: z
          .string()
          .describe("Path to file or directory (absolute or relative to project root)"),
        startLine: optionalInt(1, Number.MAX_SAFE_INTEGER).describe(
          "1-based line to start reading from",
        ),
        endLine: optionalInt(1, Number.MAX_SAFE_INTEGER).describe(
          "1-based line to stop reading at (inclusive)",
        ),
        limit: optionalInt(1, Number.MAX_SAFE_INTEGER).describe(
          "Max lines to return (default: 2000)",
        ),
        offset: optionalInt(1, Number.MAX_SAFE_INTEGER).describe(
          "1-based line number to start reading from (use with limit). Ignored if startLine is provided",
        ),
      },
      execute: async (args, context): Promise<ToolResult> => {
        const file = args.path as string;
        const projectRoot = await resolveProjectRoot(ctx, context);

        // Resolve relative paths from the same session/project root used by the bridge.
        const filePath = resolvePathFromProjectRoot(projectRoot, file);
        persistFilePathAlias(args as Record<string, unknown>, context);

        // Apply OpenCode's external-directory rule first. Under AFT's project
        // restriction, reads continue to Rust so its session task registry can
        // distinguish exact bash artifacts from ordinary external paths.
        {
          const denial = await assertExternalDirectoryPermission(ctx, context, filePath, {
            serverValidatedRead: true,
          });
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
          const failure = classifyPermissionError(error);
          return permissionDeniedResponse(
            failure.kind === "rule_denied" ? permissionRuleDenial("read") : failure.message,
          );
        }

        const rawStartLine = coerceOptionalInt(
          args.startLine,
          "startLine",
          1,
          Number.MAX_SAFE_INTEGER,
        );
        const rawEndLine = coerceOptionalInt(args.endLine, "endLine", 1, Number.MAX_SAFE_INTEGER);
        const rawLimit = coerceOptionalInt(args.limit, "limit", 1, Number.MAX_SAFE_INTEGER);
        const rawOffset = coerceOptionalInt(args.offset, "offset", 1, Number.MAX_SAFE_INTEGER);

        // Normalize offset/limit to startLine/endLine (backward compat with opencode's read)
        let startLine = rawStartLine;
        let endLine = rawEndLine;
        if (startLine === undefined && rawOffset !== undefined) {
          startLine = rawOffset;
          if (rawLimit !== undefined) {
            endLine = rawOffset + rawLimit - 1;
          }
        }

        const rawArgs: Record<string, unknown> = { filePath: file };
        if (startLine !== undefined) rawArgs.startLine = startLine;
        if (endLine !== undefined) rawArgs.endLine = endLine;
        // Only send limit if we did NOT convert offset to startLine/endLine.
        if (rawLimit !== undefined && rawOffset === undefined) rawArgs.limit = rawLimit;

        const visionCapability = await currentSessionVisionCapability(
          ctx.client,
          context.sessionID,
        );
        if (visionCapability !== undefined) rawArgs.vision_capability = visionCapability;

        const response = await callToolCall(ctx, context, "read", rawArgs);

        // Error response (e.g. file not found)
        if (response.success === false) {
          throw new Error((response.message as string) || "read failed");
        }

        const dp = relativeToWorktree(filePath, projectRoot) || file;
        const output = response.text;

        const attachments = readAttachments(response);
        if (attachments.length > 0) {
          const toolAttachments = attachments
            .filter(
              (attachment) =>
                typeof attachment.mime === "string" && typeof attachment.data === "string",
            )
            .map((attachment) => ({
              type: "file" as const,
              mime: attachment.mime as string,
              url: `data:${attachment.mime};base64,${attachment.data}`,
            }));
          if (toolAttachments.length > 0) {
            const first = attachments[0];
            const firstMime = typeof first.mime === "string" ? first.mime : "";
            return {
              output,
              title: dp,
              attachments: toolAttachments,
              metadata: {
                preview: output,
                filepath: filePath,
                title: dp,
                isImage: first.kind === "image" || firstMime.startsWith("image/"),
                isPdf: first.kind === "pdf" || firstMime === "application/pdf",
              },
            };
          }
        }

        return { output, title: dp, metadata: { title: dp } };
      },
    },
  }).read;
}

// ---------------------------------------------------------------------------
// WRITE tool
// ---------------------------------------------------------------------------

function getWriteDescription(ctx: PluginContext, editToolName: string): string {
  const backupText =
    ctx.config.backup?.enabled === false
      ? "Backup capture is disabled by user config."
      : "Existing files are backed up before overwriting (undo via aft_safety).";
  return `Write content to a file, creating it and parent directories automatically. ${backupText} Auto-formats when the project has a formatter configured. Use it to create files or replace whole contents; for partial edits, use the \`${editToolName}\` tool.`;
}

function createWriteTool(ctx: PluginContext, editToolName = "edit"): ToolDefinition {
  return {
    description: getWriteDescription(ctx, editToolName),
    args: {
      // filePath, not path: host UI header contract — see createReadTool.
      filePath: z
        .string()
        .describe("Path to the file to write (absolute or relative to project root)"),
      content: z.string().describe("The full content to write to the file"),
    },
    execute: async (args, context): Promise<ToolResult> => {
      const argsRecord = args as Record<string, unknown>;
      const file = args.path as string;
      const content = args.content as string;
      const projectRoot = await resolveProjectRoot(ctx, context);

      const filePath = resolvePathFromProjectRoot(projectRoot, file);
      persistFilePathAlias(argsRecord, context);

      const permissionPattern = permissionPath(context, filePath);

      // External-directory check first (mirrors opencode-native write.ts:43).
      {
        const denial = await assertExternalDirectoryPermission(ctx, context, filePath);
        if (denial) return permissionDeniedResponse(denial);
      }

      const rawArgs: Record<string, unknown> = { filePath: file, content };

      const preview = await callToolCall(ctx, context, "write", rawArgs, { preview: true });
      if (preview.success === false) {
        throw toolErrorFromResponse("write", preview);
      }

      const denial = await askEditPermission(context, [permissionPattern], {
        filepath: filePath,
        diff: typeof preview.preview_diff === "string" ? preview.preview_diff : "",
      });
      if (denial) return permissionDeniedResponse(denial);

      const data = await callToolCall(ctx, context, "write", rawArgs);

      // Error response (e.g. path validation failure)
      if (data.success === false) {
        throw toolErrorFromResponse("write", data);
      }

      const output = data.text;

      // Return UI metadata directly on the result. OpenCode's `fromPlugin`
      // (registry.ts) preserves a tool's returned `title`/`metadata` (since
      // v1.4.8; our floor is far past that), so there's no need for the old
      // module-level store + `tool.execute.after` merge — that workaround
      // intermittently lost the diff under duplicate plugin loads (`--port 0`
      // / Desktop) because the store Map lived in one ESM graph and the merge
      // ran in another. See GitHub #96.
      const diff = data.diff as
        | {
            before?: string;
            after?: string;
            additions?: number;
            deletions?: number;
            truncated?: boolean;
          }
        | undefined;
      if (!diff) return output;

      // See the edit tool: >512KB files return counts-only (`truncated`) with
      // no before/after — fabricating an empty `before` would render the whole
      // file as added. Fall back to the preview's hunk-scoped diff.
      const truncated = diff.truncated === true;
      const dp = relativeToWorktree(filePath, projectRoot);
      const beforeContent = diff.before ?? "";
      const afterContent = diff.after ?? content;
      const patch = truncated
        ? typeof preview.preview_diff === "string"
          ? preview.preview_diff
          : ""
        : buildUnifiedDiff(filePath, beforeContent, afterContent);
      return {
        output,
        title: dp,
        metadata: {
          diff: patch,
          ...(patch
            ? {
                filediff: {
                  file: filePath,
                  patch,
                  additions: diff.additions ?? 0,
                  deletions: diff.deletions ?? 0,
                },
              }
            : {}),
          diagnostics: {},
        },
      };
    },
  };
}

// ---------------------------------------------------------------------------
// EDIT tool
// ---------------------------------------------------------------------------

function getEditDescription(ctx: PluginContext, writeToolName: string): string {
  const backupBehavior =
    ctx.config.backup?.enabled === false
      ? "- Backup capture is disabled by user config"
      : "- Backs up files before editing (recoverable via aft_safety undo)";
  return `Edit a file by finding and replacing text, or by targeting named symbols. To write or overwrite a whole file, use the \`${writeToolName}\` tool — \`edit\` requires an explicit edit mode and will not silently overwrite a file from \`content\` alone.

**Modes** (determined by which parameters you provide):

Provide exactly one mode per call: appendContent, edits[], or symbol plus content. Mixing modes or providing none is rejected — there is no implicit "write" fallback. To edit multiple files, make parallel \`edit\` calls in one response.

1. **Append** — pass \`path\` + \`appendContent\`
   Appends text to the end of a file, creating it if it does not exist.
   Example: \`{ "path": "notes.txt", "appendContent": "new line\\n" }\`

2. **Batch edits** — pass \`path\` + \`edits\` array
   Multiple edits in one file atomically. Each edit is either:
   - \`{ "oldString": "old", "newString": "new" }\` — find/replace
   - \`{ "oldString": "old", "newString": "new", "replaceAll": true }\` — replace every match
   - \`{ "startLine": 5, "endLine": 7, "content": "new lines" }\` — replace line range (1-based, both inclusive)
   Set content to empty string to delete lines.

3. **Symbol replace** — pass \`path\` + \`symbol\` + \`content\`
   Replaces an entire named symbol (function, class, type).
   Includes decorators, attributes, and doc comments in the replacement range.
   Example: \`{ "path": "src/app.ts", "symbol": "handleRequest", "content": "function handleRequest() { ... }" }\`

4. **Find and replace** — put \`oldString\` and optional \`newString\` in an item of \`edits[]\`
   Finds the exact text in \`oldString\` and replaces it with \`newString\`.
   Supports fuzzy matching (handles whitespace differences automatically).
   If multiple matches exist, specify \`occurrence\` or set \`replaceAll: true\` in that item.

5. **Replace all occurrences** — add \`replaceAll: true\` to a find/replace item.

6. **Select specific occurrence** — add \`occurrence: N\` to a find/replace item (1-based).
   When multiple matches exist, select the Nth one (1 = first, 2 = second, etc.).

**Behavior:**
${backupBehavior}
- Auto-formats using project formatter if configured
- Tree-sitter syntax validation on all edits
- Symbol replace includes decorators, attributes, and doc comments in range
- Response is a compact server-rendered summary; before/after diff details are attached as UI metadata when available.`;
}

const HASHLINE_EDIT_DESCRIPTION = [
  "Apply a hashline patch. Arguments are exactly `{patch}` where `patch` is a non-empty string. Server-owned preview control is outside this schema.",
  "",
  "Quick reference:",
  "- Header: `[path#TAG]`; TAG is exactly four hexadecimal digits from a current tagged read. Read every addressed row and gap boundary; REM and MV require a whole-file tagged read. Re-read after an edit before chaining: an edit-response tag can retain only changed context.",
  "- Same canonical path: multiple sections compose in patch order against pre-request coordinates.",
  "- Addresses: `0` (BOF), `N` (one line), `N.=M` (range; `N..=M`/`N..M` also work), `<N`/`>N` (gap before/after), `N*`/`<N*`/`>N*` (block), and `$`/`$-K` (EOF-relative). A plain `N` PUT replaces; use `<N` or `>N` to insert.",
  "- PUT text: `PUT <address>:` followed by one or more `+` body rows (`+` alone is blank). A final patch newline is allowed. PUT without `:` copies `@name` (or the anonymous register) and takes no body; names use `@` plus ASCII letters, digits, `_`, or `-`.",
  "- CUT: `CUT <address> [@name]`. REM: bare `REM` only, removing the whole file. MV: `MV <destination>` (one whitespace-free path, optional matching quotes), once and after any line operations. `*** Begin Patch`/`*** End Patch` is an optional envelope.",
  "- Only `read` (and accepted AFT `cat`/`head`/`tail` rewrites) mint hashline tags. `aft_zoom`, `aft_outline`, `grep`, `aft_search`, and conflict snippets do not. After navigation, call `read` on every file and range the patch addresses.",
].join("\n");

function createHashlineEditTool(ctx: PluginContext): ToolDefinition {
  return {
    description: HASHLINE_EDIT_DESCRIPTION,
    args: {
      patch: z
        .string()
        .min(1)
        .describe(
          "Hashline patch text with one or more [path#TAG] sections and PUT/CUT/REM/MV operations",
        ),
    },
    execute: async (args, context): Promise<ToolResult> => {
      const patch = args.patch as string;
      const projectRoot = await resolveProjectRoot(ctx, context);
      const rawArgs = { patch };
      const preflight = await callToolCall(ctx, context, "hashline_preflight", rawArgs);
      if (preflight.success === false) throw toolErrorFromResponse("edit", preflight);

      const permissionPatterns = [
        ...coerceStringArray(preflight.affected_paths),
        ...coerceStringArray(preflight.affected_rel_paths),
        ...coerceStringArray(preflight.mv_destinations),
      ].filter((value, index, all) => all.indexOf(value) === index);
      for (const target of [
        ...coerceStringArray(preflight.affected_paths),
        ...coerceStringArray(preflight.mv_destinations),
      ]) {
        const absolute = resolvePathFromProjectRoot(projectRoot, target);
        const denial = await assertExternalDirectoryPermission(ctx, context, absolute);
        if (denial) return permissionDeniedResponse(denial);
      }

      const denial = await askEditPermission(context, permissionPatterns, {
        surface: "hashline",
      });
      if (denial) return permissionDeniedResponse(denial);

      const preview = await callToolCall(ctx, context, "edit", rawArgs, { preview: true });
      if (preview.success === false) throw toolErrorFromResponse("edit", preview);

      const data = await callToolCall(ctx, context, "edit", rawArgs);
      if (data.success === false) throw toolErrorFromResponse("edit", data);
      const firstPath =
        typeof data.filePath === "string"
          ? resolvePathFromProjectRoot(projectRoot, data.filePath)
          : "";
      return {
        output: data.text,
        title: firstPath ? relativeToWorktree(firstPath, projectRoot) : "edit",
        metadata:
          data.metadata && typeof data.metadata === "object"
            ? (data.metadata as Record<string, unknown>)
            : {},
      };
    },
  };
}

function createEditTool(ctx: PluginContext, writeToolName = "write"): ToolDefinition {
  if (ctx.hashlineEffective === true) return createHashlineEditTool(ctx);
  return {
    description: getEditDescription(ctx, writeToolName),
    args: {
      // filePath, not path: host UI header contract — see createReadTool.
      filePath: z
        .string()
        .describe("Path to the file to edit (absolute or relative to project root)"),
      symbol: z.string().optional().describe("Named symbol to replace (function, class, type)"),
      content: z
        .string()
        .optional()
        .describe(
          "Replacement content for symbol mode. For whole-file writes, use the `write` tool.",
        ),
      appendContent: z
        .string()
        .optional()
        .describe("Text to append to the end of path; creates the file if needed"),
      edits: z
        .array(
          z.object({
            oldString: z.string().optional().describe("Text to find for a batch find/replace edit"),
            newString: z
              .string()
              .optional()
              .describe("Replacement text for a batch find/replace edit"),
            replaceAll: z
              .boolean()
              .optional()
              .describe("Replace every occurrence for this batch item"),
            occurrence: optionalInt(1, Number.MAX_SAFE_INTEGER).describe(
              "1-based occurrence for this batch item (1 = first match)",
            ),
            startLine: optionalInt(1, Number.MAX_SAFE_INTEGER).describe(
              "1-based start line for a batch line-range edit",
            ),
            endLine: optionalInt(1, Number.MAX_SAFE_INTEGER).describe(
              "1-based end line for a batch line-range edit",
            ),
            content: z.string().optional().describe("Replacement text for a batch line-range edit"),
          }),
        )
        .min(1)
        .optional()
        .describe(
          "Batch edits — non-empty array of { oldString, newString }, { oldString, newString, replaceAll: true }, or { startLine, endLine, content } objects",
        ),
    },
    execute: async (args, context): Promise<ToolResult> => {
      // Footgun guard: top-level startLine/endLine are not valid params on
      // edit. They only exist nested inside `edits[]` for batch line-range
      // mode. Without this guard, OpenCode schema handling can strip the
      // unknown keys before the request reaches the server, producing an
      // unrelated mode-resolution error instead of a useful batch-edit hint.
      const argsRecord = args as Record<string, unknown>;
      if (argsRecord.startLine !== undefined || argsRecord.endLine !== undefined) {
        throw new Error(
          "edit: 'startLine'/'endLine' are not top-level parameters. " +
            "For line-range edits, nest them inside the `edits` array: " +
            '`edits: [{ startLine: N, endLine: M, content: "..." }]`. ' +
            "For find/replace, use an item in `edits[]` instead.",
        );
      }

      const file = args.path as string;
      if (!file) throw new Error("'path' parameter is required");
      const projectRoot = await resolveProjectRoot(ctx, context);

      const filePath = resolvePathFromProjectRoot(projectRoot, file);
      persistFilePathAlias(argsRecord, context);

      const permissionPattern = permissionPath(context, filePath);

      // External-directory check first (mirrors opencode-native edit.ts:68).
      {
        const denial = await assertExternalDirectoryPermission(ctx, context, filePath);
        if (denial) return permissionDeniedResponse(denial);
      }

      const rawArgs: Record<string, unknown> = { path: file };
      for (const key of ["appendContent", "symbol", "content", "edits"] as const) {
        if (argsRecord[key] !== undefined) rawArgs[key] = argsRecord[key];
      }

      const preview = await callToolCall(ctx, context, "edit", rawArgs, { preview: true });
      if (preview.success === false) {
        throw toolErrorFromResponse("edit", preview);
      }

      const denial = await askEditPermission(context, [permissionPattern], {
        filepath: filePath,
        diff: typeof preview.preview_diff === "string" ? preview.preview_diff : "",
      });
      if (denial) return permissionDeniedResponse(denial);

      const data = await callToolCall(ctx, context, "edit", rawArgs);

      // tool_call returns `{ success: false }` responses as data, so failed
      // edits (match-not-found, ambiguous, syntax rollback, or glob with zero
      // matches) must still be surfaced as thrown tool errors.
      if (data.success === false) {
        throw toolErrorFromResponse("edit", data);
      }

      const output = data.text;
      const diff = data.diff as
        | {
            before?: string;
            after?: string;
            additions?: number;
            deletions?: number;
            truncated?: boolean;
          }
        | undefined;
      if (!diff) return output;

      // UI metadata returned directly on the result (see write tool for the
      // rationale; replaces the old metadata-store + after-hook merge that
      // intermittently lost the diff under duplicate plugin loads — GitHub #96).
      //
      // Files over Rust's 512KB diff cap return counts-only (`truncated: true`)
      // with no before/after. Fabricating empty contents here rendered a blank
      // diff in the UI — use the preview's hunk-scoped unified diff instead
      // (it scales with the edit, not the file).
      const truncated = diff.truncated === true;
      const beforeContent = diff.before ?? "";
      const afterContent = diff.after ?? "";
      const patch = truncated
        ? typeof preview.preview_diff === "string"
          ? preview.preview_diff
          : ""
        : buildUnifiedDiff(filePath, beforeContent, afterContent);
      const uiMeta = {
        diff: patch,
        ...(patch
          ? {
              filediff: {
                file: filePath,
                patch,
                additions: diff.additions ?? 0,
                deletions: diff.deletions ?? 0,
              },
            }
          : {}),
        diagnostics: {},
      };
      return { output, title: relativeToWorktree(filePath, projectRoot), metadata: uiMeta };
    },
  };
}

// ---------------------------------------------------------------------------
// APPLY_PATCH tool
// ---------------------------------------------------------------------------

function applyPatchDescription(ctx: PluginContext): string {
  const backupBehavior =
    ctx.config.backup?.enabled === false
      ? "- Backup capture is disabled by user config; applied file changes are not recorded in the undo stack."
      : "- Per-file commit: each file's edits apply independently. If a later file fails, earlier successful changes are kept. Use `aft_safety` undo if you need to revert the applied changes.\n- Files are backed up before modification";
  return `Use the \`apply_patch\` tool to edit files. Your patch language is a stripped‑down, file‑oriented diff format designed to be easy to parse and safe to apply. You can think of it as a high‑level envelope:

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
${backupBehavior}
- Parent directories are created automatically for new files
- Fuzzy matching for context anchors (handles whitespace and Unicode differences)

**It is important to remember:**

- You must include a header with your intended action (Add/Delete/Update)
- You must prefix new lines with \`+\` even when creating a new file

Edits return as soon as the write completes unless \`lsp.diagnostics_on_edit\` requests legacy sync-wait behavior. Call \`aft_inspect\` afterward to check diagnostics across a batch of edits.`;
}

function applyPatchErrorMessage(response: Record<string, unknown>, fallback: string): string {
  for (const key of ["text", "output", "message"] as const) {
    const value = response[key];
    if (typeof value === "string" && value.length > 0) return value;
  }
  return fallback;
}

function stringArray(value: unknown): string[] {
  return Array.isArray(value)
    ? value.filter((entry): entry is string => typeof entry === "string")
    : [];
}

function createApplyPatchTool(ctx: PluginContext): ToolDefinition {
  return {
    description: applyPatchDescription(ctx),
    args: {
      patchText: z.string().describe("The full patch text including Begin/End markers"),
    },
    execute: async (args, context): Promise<ToolResult> => {
      const patchText = args.patchText as string;
      if (!patchText) throw new Error("'patchText' is required");

      const preview = await callToolCall(
        ctx,
        context,
        "apply_patch",
        { patchText },
        { preview: true },
      );
      if (preview.success === false) {
        throw new Error(applyPatchErrorMessage(preview, "apply_patch preview failed"));
      }

      const askedExternalPaths = new Set<string>();
      for (const filePath of stringArray(preview.affected_paths)) {
        if (askedExternalPaths.has(filePath)) continue;
        askedExternalPaths.add(filePath);
        const denial = await assertExternalDirectoryPermission(ctx, context, filePath);
        if (denial) return permissionDeniedResponse(denial);
      }

      const affectedRelPaths = stringArray(preview.affected_rel_paths);
      const affectedPaths = stringArray(preview.affected_paths);
      const permissionPatterns = (affectedPaths.length > 0 ? affectedPaths : affectedRelPaths).map(
        (filePath) => permissionPath(context, filePath),
      );
      const denial = await askEditPermission(context, permissionPatterns, {
        diff: typeof preview.preview_diff === "string" ? preview.preview_diff : "",
        filepath:
          typeof preview.filepath === "string"
            ? preview.filepath
            : (affectedPaths[0] ?? affectedRelPaths[0]),
      });
      if (denial) return permissionDeniedResponse(denial);

      const response = await callToolCall(ctx, context, "apply_patch", { patchText });
      if (response.success === false) {
        throw new Error(applyPatchErrorMessage(response, "apply_patch failed"));
      }

      const metadata =
        response.metadata &&
        typeof response.metadata === "object" &&
        !Array.isArray(response.metadata)
          ? (response.metadata as Record<string, unknown>)
          : {};
      const result: {
        output: string;
        title?: string;
        metadata: { diff: unknown; files: unknown };
      } = {
        output:
          typeof response.text === "string" ? response.text : applyPatchErrorMessage(response, ""),
        metadata: {
          diff: typeof metadata.diff === "string" ? metadata.diff : "",
          files: Array.isArray(metadata.files) ? metadata.files : [],
        },
      };
      if (typeof response.title === "string" && response.title.length > 0) {
        result.title = response.title;
      }
      return result;
    },
  };
}

// ---------------------------------------------------------------------------
// Delete
// ---------------------------------------------------------------------------

function deleteDescription(ctx: PluginContext): string {
  const backupText =
    ctx.config.backup?.enabled === false
      ? "Backup capture is disabled by user config, so this tool does not create undo snapshots. "
      : "Each file is backed up before deletion — use aft_safety undo to recover any of them. For directories, every file inside is individually backed up before the tree is removed. ";
  return (
    "Delete one or more files (or directories).\n\n" +
    backupText +
    "Directory deletion requires recursive: true. Without it, passing a directory returns an error.\n\n" +
    "Partial success is allowed: deletable files are deleted; failed ones are reported in `skipped_files` with `complete: false`."
  );
}

function createDeleteTool(ctx: PluginContext): ToolDefinition {
  return {
    description: deleteDescription(ctx),
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
      // Coerce at the boundary: some hosts deliver `files` as a bare string or
      // a JSON-stringified array despite the schema, which would crash the
      // unchecked `.map` below before any validation runs.
      const inputs = coerceStringArray(args.files);
      if (inputs.length === 0) {
        throw new Error("delete: `files` must be a non-empty array of paths");
      }
      // Coerce at the boundary: hosts deliver this boolean as the model's raw
      // emitted value (e.g. the string "true") despite the declared schema, same
      // as `files` above. A strict `=== true` then drops a stringified flag and
      // an agent's `recursive: true` is silently lost (see coerceBoolean).
      const recursive = coerceBoolean(args.recursive);
      const projectRoot = await resolveProjectRoot(ctx, context);
      const absolutePaths = inputs.map((f) => resolvePathFromProjectRoot(projectRoot, f));

      // External-directory check first (mirrors opencode-native edit.ts:68).
      {
        const asked = new Set<string>();
        for (const filePath of absolutePaths) {
          if (asked.has(filePath)) continue;
          asked.add(filePath);
          const denial = await assertExternalDirectoryPermission(ctx, context, filePath);
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
      const response = await callToolCall(ctx, context, "delete", {
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

      return response.text;
    },
  };
}

// ---------------------------------------------------------------------------
// Move / Rename
// ---------------------------------------------------------------------------

function moveDescription(ctx: PluginContext): string {
  const backupText =
    ctx.config.backup?.enabled === false
      ? "Backup capture is disabled by user config. "
      : "Creates an undo backup before moving. ";
  return (
    `Move or rename a file. ${backupText}Creates parent directories for destination automatically\n` +
    "Note: This moves/renames whole files at the OS level; it does not relocate an individual symbol or rewrite imports."
  );
}

function createMoveTool(ctx: PluginContext): ToolDefinition {
  return {
    description: moveDescription(ctx),
    args: {
      path: z.string().describe("Source file path to move (absolute or relative to project root)"),
      destination: z
        .string()
        .describe("Destination file path (absolute or relative to project root)"),
    },
    execute: async (args, context): Promise<string> => {
      const projectRoot = await resolveProjectRoot(ctx, context);
      const filePath = resolvePathFromProjectRoot(projectRoot, args.path as string);
      const destPath = resolvePathFromProjectRoot(projectRoot, args.destination as string);

      // External-directory check first (mirrors opencode-native edit.ts:68).
      {
        const sourceDenial = await assertExternalDirectoryPermission(ctx, context, filePath, {
          kind: "file",
        });
        if (sourceDenial) return permissionDeniedResponse(sourceDenial);
        if (destPath !== filePath) {
          const destDenial = await assertExternalDirectoryPermission(ctx, context, destPath);
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

      const result = await callToolCall(ctx, context, "move", {
        filePath: args.path as string,
        destination: args.destination as string,
      });
      if (result.success === false) {
        throw new Error((result.message as string) || "move failed");
      }
      return result.text;
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
 * Bash hoisting follows the resolved `bash` config. When bash is enabled, the
 * primary `bash` tool is registered. Background control tools (`bash_status`,
 * `bash_write`, `bash_watch`, and `bash_kill`) are registered only when
 * `bash.background` resolves true. With `bash.background: false`, foreground
 * bash runs to completion inline and no background surface is exposed.
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
  // `resolveBashConfig` in config.ts for the precedence rules. `bash` itself
  // registers whenever bash is enabled; the background control tools register
  // only when `bash.background` is enabled.
  const bashCfg = resolveBashConfig(ctx.config);
  if (bashCfg.enabled) {
    tools.bash = createBashTool(ctx);
    if (bashCfg.background) {
      tools.bash_status = createBashStatusTool(ctx);
      tools.bash_write = createBashWriteTool(ctx);
      tools.bash_watch = createBashWatchTool(ctx);
      tools.bash_kill = createBashKillTool(ctx);
    }
  }

  return prepareToolMap(tools, { hashlineEffective: ctx.hashlineEffective });
}

/**
 * Returns the same tools with aft_ prefix (for when hoisting is disabled).
 */
export function aftPrefixedTools(ctx: PluginContext): Record<string, ToolDefinition> {
  const aftEditTool = createEditTool(ctx, "aft_write");

  const tools: Record<string, ToolDefinition> = {
    aft_read: createReadTool(ctx),
    aft_write: createWriteTool(ctx, "aft_edit"),
    aft_edit: aftEditTool,
    aft_apply_patch: createApplyPatchTool(ctx),
    aft_delete: createDeleteTool(ctx),
    aft_move: createMoveTool(ctx),
  };

  // Hoist-off mode: same gating as hoisted mode but with the aft_ prefix on
  // the primary bash tool so it doesn't override OpenCode's native bash. The
  // background control tools keep their unprefixed names because they refer to
  // AFT-spawned task IDs that the native bash doesn't know about.
  const bashCfg = resolveBashConfig(ctx.config);
  if (bashCfg.enabled) {
    tools.aft_bash = createBashTool(ctx);
    if (bashCfg.background) {
      tools.bash_status = createBashStatusTool(ctx);
      tools.bash_write = createBashWriteTool(ctx);
      tools.bash_watch = createBashWatchTool(ctx);
      tools.bash_kill = createBashKillTool(ctx);
    }
  }

  return prepareToolMap(tools, { hashlineEffective: ctx.hashlineEffective });
}
