/**
 * Hoisted tool overrides — replace Pi's built-in read/write/edit/grep with
 * AFT-backed Rust implementations. Registering a tool with the same name as
 * a built-in replaces the built-in entirely.
 *
 * Each tool provides:
 *  - `promptSnippet` / `promptGuidelines`: teach the model our argument shape
 *    in Pi's system prompt (Pi's built-ins use generic one-liners otherwise).
 *  - `renderCall` / `renderResult` for `write` and `edit`: without these,
 *    Pi's ToolExecutionComponent falls back to the *built-in* renderer for
 *    same-named tools, which reads `path` and `edits[]` and garbles our
 *    `filePath` / `oldString` / `newString` output (issue #15).
 *  - Structured `details: { diff, firstChangedLine }` so the rendered diff
 *    also ends up in the agent's message stream, matching Pi's convention.
 *
 * `read` and `grep` keep the default text-only result rendering because our
 * payload (`path`, `pattern`) already aligns with Pi's built-in arg shape.
 */

import { stat } from "node:fs/promises";
import { homedir } from "node:os";
import { isAbsolute, relative, resolve } from "node:path";
import {
  type AgentToolResult,
  type ExtensionAPI,
  renderDiff,
  type Theme,
} from "@earendil-works/pi-coding-agent";
import { type Component, Container, Spacer, Text } from "@earendil-works/pi-tui";
import { type Static, Type } from "typebox";
import type { PluginContext } from "../types.js";
import { bridgeFor, callBridge, textResult } from "./_shared.js";
import { formatDiffForPi } from "./diff-format.js";

/**
 * Local shape for Pi's render context — the real type is exposed by
 * `@earendil-works/pi-coding-agent`'s internals but not publicly exported.
 * We only read `lastComponent` and `isError` here; everything else is ignored.
 */
interface RenderContextLike {
  lastComponent: Component | undefined;
  isError: boolean;
}

function containsPath(parent: string, child: string): boolean {
  const rel = relative(parent, child);
  return rel === "" || (!rel.startsWith("..") && !isAbsolute(rel));
}

async function assertExternalDirectoryPermission(
  extCtx: { cwd: string; ui?: { confirm?: (title: string, message: string) => Promise<boolean> } },
  target: string,
  action = "modify",
): Promise<void> {
  if (!target) return;
  const absoluteTarget = isAbsolute(target) ? target : resolve(extCtx.cwd, target);
  if (containsPath(extCtx.cwd, absoluteTarget)) return;

  const confirmed = await extCtx.ui?.confirm?.(
    "Allow external directory access?",
    `AFT wants to ${action} outside the project: ${absoluteTarget}`,
  );
  if (confirmed === true) return;
  throw new Error("Permission denied: external directory access was cancelled.");
}

const ReadParams = Type.Object({
  path: Type.String({ description: "Path to the file to read (relative or absolute)" }),
  offset: Type.Optional(
    Type.Number({ description: "Line number to start reading from (1-indexed)" }),
  ),
  limit: Type.Optional(Type.Number({ description: "Maximum number of lines to read" })),
});

const WriteParams = Type.Object({
  filePath: Type.String({
    description: "Path to the file to write (absolute or project-relative)",
  }),
  content: Type.String({ description: "Full file contents to write" }),
});

const EditParams = Type.Object({
  filePath: Type.String({ description: "Path to the file to edit" }),
  oldString: Type.Optional(
    Type.String({ description: "Text to find (exact match, fuzzy fallback)" }),
  ),
  newString: Type.Optional(Type.String({ description: "Replacement text (omit to delete match)" })),
  replaceAll: Type.Optional(Type.Boolean({ description: "Replace every occurrence" })),
  occurrence: Type.Optional(
    Type.Number({ description: "0-indexed occurrence when multiple matches exist" }),
  ),
  appendContent: Type.Optional(
    Type.String({
      description:
        "Append text to the end of the file (creates the file if missing, parent dirs auto-created). When set, oldString/newString are ignored.",
    }),
  ),
});

const GrepParams = Type.Object({
  pattern: Type.String({ description: "Regex pattern to search for" }),
  path: Type.Optional(Type.String({ description: "Path scope (file or directory)" })),
  include: Type.Optional(
    Type.String({ description: "Glob filter for included files (e.g. '*.ts,*.tsx')" }),
  ),
  caseSensitive: Type.Optional(Type.Boolean({ description: "Case-sensitive matching" })),
  contextLines: Type.Optional(
    Type.Number({ description: "Lines of context before/after each match" }),
  ),
});

export interface ToolSurfaceFlags {
  hoistRead: boolean;
  hoistWrite: boolean;
  hoistEdit: boolean;
  hoistGrep: boolean;
}

/** Details surfaced to both renderer and agent message stream. */
interface FileMutationDetails {
  diff?: string;
  firstChangedLine?: number;
  additions: number;
  deletions: number;
  replacements?: number;
  diagnostics?: unknown[];
  /**
   * True when Rust returned `diff.truncated = true` — the before/after strings
   * were omitted because the file exceeded the diff size cap, so we have no
   * line-level diff to render. Both the agent-facing text and the TUI renderer
   * surface this explicitly rather than silently showing a summary.
   */
  truncated?: boolean;
  /**
   * Whether AFT's auto-formatter ran on the post-write content. Mirrors the
   * `data.formatted` field from the Rust write/edit response. When true,
   * the file content on disk is what the formatter produced; when false,
   * `formatSkippedReason` explains why.
   */
  formatted?: boolean;
  /**
   * Reason the formatter was skipped, when `formatted=false`. One of the
   * documented values from `crates/aft/src/format.rs::auto_format`:
   * `"unsupported_language"`, `"no_formatter_configured"`,
   * `"formatter_not_installed"`, `"formatter_excluded_path"`, `"timeout"`,
   * `"error"`. Pi agents read this to decide whether to retry, fix config,
   * or accept the unformatted result.
   */
  formatSkippedReason?: string;
  /**
   * v0.27.1: Rust returns `no_op: true` when the post-write file content
   * is byte-identical to the pre-write state. This separates "matched but
   * produced no change" from a real `+0/-0` failure mode in the UI.
   * See GitHub #45.
   */
  noOp?: boolean;
}

export function registerHoistedTools(
  pi: ExtensionAPI,
  ctx: PluginContext,
  surface: ToolSurfaceFlags,
): void {
  if (surface.hoistRead) {
    pi.registerTool({
      name: "read",
      label: "read",
      description:
        "Read file contents with line numbers. Backed by AFT's indexed Rust reader — faster than the built-in `read` on large repos and correctly handles images/PDFs as attachments.",
      promptSnippet: "Read file contents (supports offset/limit for large files)",
      promptGuidelines: ["Use read to examine files instead of cat or sed."],
      parameters: ReadParams,
      async execute(
        _toolCallId: string,
        params: Static<typeof ReadParams>,
        _signal,
        _onUpdate,
        extCtx,
      ) {
        const bridge = bridgeFor(ctx, extCtx.cwd);
        const req: Record<string, unknown> = { file: params.path };
        if (params.offset !== undefined) {
          req.start_line = params.offset;
          if (params.limit !== undefined) {
            req.end_line = params.offset + params.limit - 1;
          }
        } else if (params.limit !== undefined) {
          req.end_line = params.limit;
        }
        const response = await callBridge(bridge, "read", req, extCtx);
        if (Array.isArray(response.entries)) {
          return textResult((response.entries as string[]).join("\n"));
        }
        let text = (response.content as string | undefined) ?? "";

        // Two-case footer (kept aligned with the OpenCode plugin's
        // formatReadFooter — see docs there for case A/B rationale).
        // Pi previously discarded `truncated`/`total_lines` entirely, so
        // an agent that read a 500-line file with no range got back
        // default-clamped 100 lines with NO signal that 400 more lines
        // existed. This restores Case A (hint when agent didn't choose)
        // while avoiding the patronizing hint when the agent already
        // chose a range (Case B → no footer).
        const agentSpecifiedRange = params.offset !== undefined || params.limit !== undefined;
        const footer = formatReadFooter(agentSpecifiedRange, response);
        if (footer) text += footer;
        return textResult(text);
      },
    });
  }

  if (surface.hoistWrite) {
    pi.registerTool<typeof WriteParams, FileMutationDetails>({
      name: "write",
      label: "write",
      description:
        "Write a file atomically with per-file backup, optional auto-format, and inline LSP diagnostics. Parent directories are created automatically. Overwrites existing files. Uses `filePath` (not `path`).",
      promptSnippet:
        "Create or overwrite files (uses filePath; auto-formats; returns LSP diagnostics inline)",
      promptGuidelines: ["Use write only for new files or complete rewrites."],
      parameters: WriteParams,
      async execute(
        _toolCallId: string,
        params: Static<typeof WriteParams>,
        _signal,
        _onUpdate,
        extCtx,
      ) {
        await assertExternalDirectoryPermission(extCtx, params.filePath, "modify");
        const bridge = bridgeFor(ctx, extCtx.cwd);
        const response = await callBridge(
          bridge,
          "write",
          {
            file: params.filePath,
            content: params.content,
            diagnostics: true,
            include_diff: true,
          },
          extCtx,
        );
        return buildMutationResult(params.filePath, response);
      },
      renderCall(args, theme, context) {
        return renderMutationCall("write", args?.filePath, theme, context);
      },
      renderResult(result, _options, theme, context) {
        return renderMutationResult(result, theme, context);
      },
    });
  }

  if (surface.hoistEdit) {
    pi.registerTool<typeof EditParams, FileMutationDetails>({
      name: "edit",
      label: "edit",
      description:
        "Find-and-replace edit with progressive fuzzy matching (handles whitespace and Unicode drift). Uses `filePath`, `oldString`, `newString`. Errors on multiple matches — use `occurrence` to pick one, or `replaceAll: true`. Always returns LSP diagnostics inline.",
      promptSnippet:
        "Targeted find-and-replace (uses filePath/oldString/newString; occurrence or replaceAll for disambiguation; fuzzy whitespace matching). Pass appendContent to append to a file (creates if missing).",
      promptGuidelines: [
        "Prefer edit over write when changing part of an existing file.",
        "Include enough surrounding context in oldString to make the match unique, or set replaceAll/occurrence explicitly.",
        "Use appendContent (instead of read+write) when adding text to the end of a file.",
      ],
      parameters: EditParams,
      async execute(
        _toolCallId: string,
        params: Static<typeof EditParams>,
        _signal,
        _onUpdate,
        extCtx,
      ) {
        await assertExternalDirectoryPermission(extCtx, params.filePath, "modify");
        const bridge = bridgeFor(ctx, extCtx.cwd);

        // Append mode: explicitly route through the Rust `append` op, which
        // creates the file (and parent dirs) when missing and appends without
        // reading the whole file first. oldString/newString are ignored when
        // appendContent is set, matching the OpenCode-side hoisted edit shape.
        if (typeof params.appendContent === "string") {
          const req: Record<string, unknown> = {
            op: "append",
            file: params.filePath,
            append_content: params.appendContent,
            diagnostics: true,
            include_diff: true,
          };
          const response = await callBridge(bridge, "edit_match", req, extCtx);
          return buildMutationResult(params.filePath, response);
        }

        const req: Record<string, unknown> = {
          file: params.filePath,
          match: params.oldString ?? "",
          replacement: params.newString ?? "",
          diagnostics: true,
          include_diff: true,
        };
        if (params.replaceAll === true) req.replace_all = true;
        if (params.occurrence !== undefined) req.occurrence = params.occurrence;

        const response = await callBridge(bridge, "edit_match", req, extCtx);
        return buildMutationResult(params.filePath, response);
      },
      renderCall(args, theme, context) {
        return renderMutationCall("edit", args?.filePath, theme, context);
      },
      renderResult(result, _options, theme, context) {
        return renderMutationResult(result, theme, context);
      },
    });
  }

  if (surface.hoistGrep) {
    pi.registerTool({
      name: "grep",
      label: "grep",
      description:
        "Search for a regex pattern across files. Uses AFT's trigram index inside the project root for fast repeated queries, and falls back to ripgrep for paths outside the project root.",
      promptSnippet: "Fast regex search across files (trigram-indexed inside the project root)",
      promptGuidelines: ["Prefer grep over bash-invoked find/rg for in-project searches."],
      parameters: GrepParams,
      async execute(
        _toolCallId: string,
        params: Static<typeof GrepParams>,
        _signal,
        _onUpdate,
        extCtx,
      ) {
        const bridge = bridgeFor(ctx, extCtx.cwd);
        const req: Record<string, unknown> = { pattern: params.pattern };
        if (params.path) {
          await assertExternalDirectoryPermission(extCtx, params.path, "search");
          req.path = await resolvePathArg(extCtx.cwd, params.path);
        }
        if (params.include) req.include = splitIncludeGlobs(params.include);
        if (params.caseSensitive !== undefined) req.case_sensitive = params.caseSensitive;
        if (params.contextLines !== undefined) req.context_lines = params.contextLines;

        const response = await callBridge(bridge, "grep", req, extCtx);
        const text = (response.text as string | undefined) ?? "";
        return textResult(text);
      },
    });
  }
}

// ---------------------------------------------------------------------------
// Mutation helpers — write and edit share result shape and rendering.
// ---------------------------------------------------------------------------

/**
 * Shape the bridge `edit_match` / `write` response into an `AgentToolResult`
 * Pi can render. Exported for unit tests covering truncation and diagnostics
 * behavior without spinning up a real bridge.
 */
export function buildMutationResult(
  filePath: string,
  response: Record<string, unknown>,
): AgentToolResult<FileMutationDetails> {
  const diffObj = response.diff as
    | {
        before?: string;
        after?: string;
        additions?: number;
        deletions?: number;
        truncated?: boolean;
      }
    | undefined;
  const additions = diffObj?.additions ?? 0;
  const deletions = diffObj?.deletions ?? 0;
  const replacements = response.replacements as number | undefined;
  const diagnostics = response.lsp_diagnostics as unknown[] | undefined;
  const truncated = diffObj?.truncated === true;
  // Rust v0.27.1: `no_op: true` when the file content is byte-identical to
  // the pre-write state — either the agent passed `oldString === newString`,
  // a formatter normalized the change away, or the replacement matched the
  // existing content. The match was satisfied (replacements > 0) but no net
  // file change landed. See GitHub #45.
  const noOp = response.no_op === true;
  // Format outcome — Rust writes return `formatted: bool` and, when
  // skipped, `format_skipped_reason: "<reason>"`. Forward both into
  // `details` so Pi agents can act on them (retry with different config,
  // accept the unformatted result, etc). The OpenCode plugin surfaces
  // these the same way; this is the Pi parity fix.
  const formatted = response.formatted as boolean | undefined;
  const formatSkippedReason = response.format_skipped_reason as string | undefined;
  const globFormatSkipReasons = response.format_skip_reasons as unknown;

  // Generate the Pi-style line-numbered diff when Rust gave us before/after
  // and the diff wasn't truncated. Truncated diffs carry `additions`/`deletions`
  // counts but no before/after strings, so we surface that explicitly in both
  // the agent-facing text and the TUI renderer instead of silently collapsing
  // to a summary-only output.
  let diffText: string | undefined;
  let firstChangedLine: number | undefined;
  if (
    diffObj &&
    !truncated &&
    typeof diffObj.before === "string" &&
    typeof diffObj.after === "string"
  ) {
    const piDiff = formatDiffForPi(diffObj.before, diffObj.after);
    diffText = piDiff.diff;
    firstChangedLine = piDiff.firstChangedLine;
  }

  // Agent-facing text: summary header + diff (if present) + truncation
  // notice + no-op notice + format-skip notice (non-benign reasons only)
  // + diagnostics.
  const summaryHeader =
    replacements !== undefined
      ? `Edited ${filePath} (+${additions}/-${deletions}, ${replacements} replacement${replacements === 1 ? "" : "s"})`
      : `Wrote ${filePath} (+${additions}/-${deletions})`;
  let text = summaryHeader;
  if (diffText) text += `\n\n${diffText}`;
  if (truncated) {
    text += "\n\n(diff truncated \u2014 file too large to include before/after content)";
  }
  if (noOp) {
    // Surface the no-op signal explicitly so the agent can distinguish "the
    // tool failed silently" from "the edit matched but produced no net change".
    // Common causes: oldString equals newString, or a formatter normalized
    // the replacement back to the original.
    text +=
      "\n\nNote: no net file change \u2014 the match was found and applied, but the file content is byte-identical to before. Likely causes: oldString and newString are identical, or a formatter normalized the change away.";
  }
  // Surface non-benign format-skip reasons in agent-facing text. Benign
  // reasons (no formatter configured for the language, language unsupported)
  // are silent because the agent can't act on them. The actionable reasons
  // — formatter binary missing, formatter timed out, formatter crashed,
  // formatter excluded the path via project config — get a one-line note
  // pointing at the right remediation.
  const skipNote = formatSkipReasonNote(formatSkippedReason);
  if (skipNote) text += `\n\n${skipNote}`;
  const globSkipNote = formatGlobSkipReasonsNote(globFormatSkipReasons);
  if (globSkipNote) text += `\n\n${globSkipNote}`;
  if (diagnostics && diagnostics.length > 0) {
    text += `\n\nLSP diagnostics:\n${formatDiagnosticsText(diagnostics)}`;
  }

  return {
    content: [{ type: "text", text }],
    details: {
      diff: diffText,
      firstChangedLine,
      additions,
      deletions,
      replacements,
      diagnostics,
      truncated: truncated || undefined,
      formatted,
      formatSkippedReason,
      noOp: noOp || undefined,
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

/**
 * Build a one-line agent-facing note for a non-benign format skip reason.
 * Returns undefined for benign reasons (no message worth surfacing) so the
 * caller can skip emitting a section header.
 */
function formatSkipReasonNote(reason: string | undefined): string | undefined {
  switch (reason) {
    case "formatter_not_installed":
      return "Note: formatter binary not installed; file written unformatted.";
    case "timeout":
      return "Note: formatter timed out; file written unformatted. Raise formatter_timeout_secs or check the formatter for hangs.";
    case "formatter_excluded_path":
      return "Note: formatter is configured to ignore this path (e.g. biome.json files.includes, .prettierignore). File written unformatted.";
    case "error":
      return "Note: formatter exited with an unrecognized error; file written unformatted.";
    default:
      // unsupported_language, no_formatter_configured, undefined → silent
      return undefined;
  }
}

function formatDiagnosticsText(diagnostics: unknown[]): string {
  // Diagnostics come back as an array of { line, severity, message, ... }.
  // Keep the format compact and human-readable; fall back to JSON if shape
  // is unexpected.
  try {
    return diagnostics
      .map((d) => {
        if (d && typeof d === "object") {
          const obj = d as Record<string, unknown>;
          const line = obj.line ?? obj.startLine ?? "?";
          const severity = obj.severity ?? "info";
          const msg = obj.message ?? JSON.stringify(obj);
          return `  [${severity}] line ${line}: ${msg}`;
        }
        return `  ${String(d)}`;
      })
      .join("\n");
  } catch {
    return JSON.stringify(diagnostics, null, 2);
  }
}

/**
 * Reuse a compatible `Text` from `lastComponent`, or create a fresh one.
 * The runtime `instanceof` guard prevents a cross-branch re-render from
 * trying to use a `Container` as a `Text` (or vice versa) — today Pi keeps
 * call/result slots separate and each slot's branch is stable per call, so
 * this is defensive hardening rather than a current-bug fix.
 */
function reuseText(last: Component | undefined): Text {
  return last instanceof Text ? last : new Text("", 0, 0);
}

function reuseContainer(last: Component | undefined): Container {
  return last instanceof Container ? last : new Container();
}

function renderMutationCall(
  toolName: "write" | "edit",
  filePath: string | undefined,
  theme: Theme,
  context: RenderContextLike,
): Text {
  const text = reuseText(context.lastComponent);
  const pathDisplay = filePath
    ? theme.fg("accent", shortenPath(filePath))
    : theme.fg("toolOutput", "...");
  text.setText(`${theme.fg("toolTitle", theme.bold(toolName))} ${pathDisplay}`);
  return text;
}

function renderMutationResult(
  result: AgentToolResult<FileMutationDetails>,
  theme: Theme,
  context: RenderContextLike,
): Container | Text {
  // Errors: red text.
  if (context.isError) {
    const errorText = result.content
      .filter((c) => c.type === "text")
      .map((c) => (c as { text?: string }).text ?? "")
      .join("\n")
      .trim();
    const text = reuseText(context.lastComponent);
    text.setText(`\n${theme.fg("error", errorText || "edit failed")}`);
    return text;
  }

  const details = result.details;
  const diff = typeof details?.diff === "string" ? details.diff : undefined;

  // No diff (no-op edit or truncated diff): one-line summary. Truncation is
  // surfaced explicitly in muted text so the user isn't misled into thinking
  // a tiny summary reflects a tiny change. v0.27.1: when Rust signaled
  // `no_op: true`, attach a clear "no net change" suffix instead of a bare
  // `+0/-0` so the user can tell the agent's edit matched but produced no
  // file change (oldString === newString, or formatter reverted the diff).
  // See GitHub #45.
  if (!diff) {
    const additions = details?.additions ?? 0;
    const deletions = details?.deletions ?? 0;
    const text = reuseText(context.lastComponent);
    const summary = theme.fg("success", `+${additions}/-${deletions}`);
    let suffix = "";
    if (details?.truncated) {
      suffix = ` ${theme.fg("muted", "(diff truncated)")}`;
    } else if (details?.noOp) {
      suffix = ` ${theme.fg("muted", "(no net change)")}`;
    }
    text.setText(`\n${summary}${suffix}`);
    return text;
  }

  // Diff: render using Pi's built-in renderer for colored lines + intra-line
  // highlighting, wrapped in a Container with a top spacer for breathing room.
  const container = reuseContainer(context.lastComponent);
  container.clear();
  container.addChild(new Spacer(1));
  container.addChild(new Text(renderDiff(diff), 1, 0));
  return container;
}

function shortenPath(path: string): string {
  const home = homedir();
  if (path.startsWith(home)) return `~${path.slice(home.length)}`;
  return path;
}

/** Resolve a path argument to an absolute path if it exists. */
async function resolvePathArg(cwd: string, path: string): Promise<string> {
  const abs = resolve(cwd, path);
  try {
    await stat(abs);
    return abs;
  } catch {
    return path;
  }
}

/**
 * Brace-aware split for OpenCode-style include args.
 *
 * Accepts:
 *   - "*.ts,*.tsx"            (comma-separated includes)
 *   - "**\/*.{vue,ts,tsx}"    (single glob with brace alternation)
 *   - "*.ts,**\/*.{vue,tsx}"  (mix of both)
 *
 * A naive split-by-`,` would chop `*.{vue,ts}` into `*.{vue` + `ts}`,
 * which then fails downstream globbing with
 * `unclosed alternate group; missing '}'`.
 */
export function splitIncludeGlobs(include: string): string[] {
  const out: string[] = [];
  let depth = 0;
  let buf = "";
  for (const ch of include) {
    if (ch === "{") {
      depth++;
      buf += ch;
      continue;
    }
    if (ch === "}") {
      if (depth > 0) depth--;
      buf += ch;
      continue;
    }
    if (ch === "," && depth === 0) {
      const trimmed = buf.trim();
      if (trimmed.length > 0) out.push(trimmed);
      buf = "";
      continue;
    }
    buf += ch;
  }
  const tail = buf.trim();
  if (tail.length > 0) out.push(tail);
  return out;
}

/**
 * Build the navigation footer for a `read` response. Mirrors the OpenCode
 * plugin's helper of the same name. See packages/opencode-plugin/src/tools/
 * hoisted.ts::formatReadFooter for the case rationale; the two are kept in
 * sync deliberately. (Not factored into a shared package because there is no
 * cross-plugin shared module yet and ~40 lines doesn't justify creating one.)
 */
export function formatReadFooter(
  agentSpecifiedRange: boolean,
  data: Record<string, unknown>,
): string {
  // CASE B: agent picked the range. No footer at all. They have the math.
  if (agentSpecifiedRange) return "";

  if (!data.truncated) return "";

  const startLine = data.start_line as number | undefined;
  const endLine = data.end_line as number | undefined;
  const totalLines = data.total_lines as number | undefined;
  if (startLine === undefined || endLine === undefined || totalLines === undefined) {
    return "";
  }

  // CASE A: agent did not pick a range, response was clamped — hint
  // is useful, tell them how to read more.
  return `\n(Showing lines ${startLine}-${endLine} of ${totalLines}. Use offset/limit to read other sections.)`;
}
