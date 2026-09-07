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
 *    same-named tools, which reads `path` and `edits[]` and can garble output
 *    when a renderer does not match the registered argument shape (issue #15).
 *  - Structured `details: { diff, firstChangedLine }` so the rendered diff
 *    also ends up in the agent's message stream, matching Pi's convention.
 *
 * `read` and `grep` keep the default text-only result rendering because our
 * payload (`path`, `pattern`) already aligns with Pi's built-in arg shape.
 */

import { stat } from "node:fs/promises";
import { homedir } from "node:os";
import { isAbsolute, relative, resolve, sep } from "node:path";
import {
  decodeFileUrl,
  formatEditSummary,
  formatReadFooter as formatSharedReadFooter,
  toolErrorFromResponse,
} from "@cortexkit/aft-bridge";
import {
  type AgentToolResult,
  type ExtensionAPI,
  renderDiff,
  type Theme,
} from "@earendil-works/pi-coding-agent";
import { type Component, Container, Spacer, Text } from "@earendil-works/pi-tui";
import { type Static, Type } from "typebox";
import type { PluginContext } from "../types.js";
import {
  bridgeFor,
  callToolCall,
  callToolCallForSession,
  coerceOptionalInt,
  contentResult,
  optionalInt,
  resolveSessionId,
  textResult,
  withPathAliasPreparation,
} from "./_shared.js";
import { formatDiffForPi } from "./diff-format.js";
import {
  collapsibleResult,
  normalizeTerminalText,
  type RenderResultOptionsLike,
} from "./render-helpers.js";

type ReadAttachment = {
  kind?: unknown;
  mime?: unknown;
  data?: unknown;
  bytes?: unknown;
  width?: unknown;
  height?: unknown;
  resized?: unknown;
};

function readAttachments(response: Record<string, unknown>): ReadAttachment[] {
  return Array.isArray(response.attachments) ? (response.attachments as ReadAttachment[]) : [];
}

function formatAttachmentSize(bytes: unknown): string | undefined {
  if (typeof bytes !== "number" || !Number.isFinite(bytes) || bytes < 0) return undefined;
  if (bytes >= 1024 * 1024) return `${(bytes / (1024 * 1024)).toFixed(1)} MB`;
  if (bytes >= 1024) return `${Math.ceil(bytes / 1024)} KB`;
  return `${bytes} bytes`;
}

function formatReadAttachmentText(attachment: ReadAttachment): string {
  const mime = typeof attachment.mime === "string" ? attachment.mime : "application/octet-stream";
  const size = formatAttachmentSize(attachment.bytes);
  if (attachment.kind === "image" || mime.startsWith("image/")) {
    const dimensions =
      typeof attachment.width === "number" && typeof attachment.height === "number"
        ? `, ${attachment.width}×${attachment.height}`
        : "";
    const resized = attachment.resized === true ? ", resized" : "";
    return `Read image file [${mime}]${dimensions}${resized}${size ? `, ${size}` : ""}`;
  }
  if (attachment.kind === "pdf" || mime === "application/pdf") {
    return `Read PDF file${size ? ` [${size}]` : ""}`;
  }
  return `Read attachment [${mime}]${size ? ` ${size}` : ""}`;
}

const ISSUE_AND_PR_READ_DESCRIPTION =
  "GitHub issues and pull requests can be read with `issue://NUMBER` and `pr://NUMBER` (or `issue://OWNER/REPO/NUMBER` and `pr://OWNER/REPO/NUMBER`).";

/** Reuse the user-tier gh_read description gate across every GitHub-capable tool. */
export function whenGhReadEnabled(enabled: boolean, description: string): string {
  return enabled ? description : "";
}

const READ_DESCRIPTION =
  "Read file contents with line numbers. Backed by AFT's indexed Rust reader — faster than the built-in `read` on large repos. Images are returned as attachments on vision-capable models; PDFs and non-vision models are not yet supported.";

function readDescription(ghReadEnabled: boolean): string {
  const githubDescription = whenGhReadEnabled(ghReadEnabled, ISSUE_AND_PR_READ_DESCRIPTION);
  return githubDescription ? `${READ_DESCRIPTION} ${githubDescription}` : READ_DESCRIPTION;
}

function visionCapabilityForPiModel(extCtx: { model?: { input?: unknown } }): boolean | undefined {
  return Array.isArray(extCtx.model?.input) ? extCtx.model.input.includes("image") : undefined;
}

function modelSupportsImages(extCtx: { model?: { input?: unknown } }): boolean {
  return visionCapabilityForPiModel(extCtx) === true;
}

const NON_VISION_IMAGE_NOTE =
  "[Current model does not support images. The image will be omitted from this request.]";

/**
 * Local shape for Pi's render context — the real type is exposed by
 * `@earendil-works/pi-coding-agent`'s internals but not publicly exported.
 * We only read `args`, `lastComponent`, and `isError` here; everything else is ignored.
 */
interface RenderContextLike {
  args: unknown;
  lastComponent: Component | undefined;
  isError: boolean;
}

type SearchPathArgSplit = { paths: string[]; missing: string[] };

function containsPath(parent: string, child: string): boolean {
  const rel = relative(parent, child);
  return rel === "" || (!rel.startsWith("..") && !isAbsolute(rel));
}

/**
 * Normalize a raw path argument: decode RFC 8089 `file:` URLs (models spell
 * local targets as file:///path) and expand a leading `~` to the user's home
 * directory. Runs before any filesystem stat or permission check so both see
 * the real target. The file: decode uses the shared byte-wise decoder so
 * Pi's permission check and the Rust resolver (subc_translate.rs
 * decode_file_url) judge the same filesystem path.
 */
function normalizePathInput(path: string): string {
  const decoded = decodeFileUrl(path);
  if (!decoded || !decoded.startsWith("~")) return decoded;
  if (decoded === "~") return homedir();
  if (decoded.startsWith(`~${sep}`) || decoded.startsWith("~/")) {
    return resolve(homedir(), decoded.slice(2));
  }
  return decoded;
}

function absoluteSearchPath(cwd: string, target: string): string {
  const expanded = normalizePathInput(target);
  return isAbsolute(expanded) ? expanded : resolve(cwd, expanded);
}

async function searchPathExists(cwd: string, target: string): Promise<boolean> {
  try {
    await stat(absoluteSearchPath(cwd, target));
    return true;
  } catch {
    return false;
  }
}

async function splitSearchPathArg(cwd: string, raw: string): Promise<SearchPathArgSplit> {
  if ((await searchPathExists(cwd, raw)) || !/\s/.test(raw)) {
    return { paths: [raw], missing: [] };
  }

  const fragments = raw.trim().split(/\s+/).filter(Boolean);
  if (fragments.length < 2) {
    return { paths: [raw], missing: [] };
  }

  const existing: string[] = [];
  const missing: string[] = [];
  for (const fragment of fragments) {
    if (await searchPathExists(cwd, fragment)) {
      existing.push(fragment);
    } else {
      missing.push(fragment);
    }
  }

  if (existing.length === 0) {
    return { paths: [raw], missing: [] };
  }

  return { paths: existing, missing };
}

async function bridgeSearchPathArg(cwd: string, split: SearchPathArgSplit): Promise<string> {
  if (split.paths.length === 1 && split.missing.length === 0) {
    return await resolvePathArg(cwd, split.paths[0]);
  }
  return split.paths.map((target) => absoluteSearchPath(cwd, target)).join(" ");
}

function formatSkippedSearchPaths(missing: string[]): string | undefined {
  if (missing.length === 0) return undefined;
  const noun = missing.length === 1 ? "path" : "paths";
  return `Skipped ${missing.length} ${noun} not found: ${missing.join(", ")}`;
}

function appendSkippedSearchPaths(text: string, missing: string[]): string {
  const note = formatSkippedSearchPaths(missing);
  if (!note) return text;
  return text.length > 0 ? `${text}\n\n${note}` : note;
}

/**
 * Enforce AFT's `restrict_to_project_root` isolation for an out-of-root target.
 *
 * Pi has no host-level permission/allow-list system to bubble to. So the knob
 * is binary: when `restrict_to_project_root` is false (Pi default) the path is
 * allowed (Rust accepts it); when true, the path is hard-blocked with a clear,
 * actionable error — never a prompt. A per-call grant could never override the
 * Rust-side boundary anyway, which is exactly the issue #125 footgun this
 * avoids. The thrown error surfaces as the tool result (Pi's user surface).
 */
export async function assertExternalDirectoryPermission(
  extCtx: { cwd: string },
  target: string,
  options: { restrictToProjectRoot?: boolean; serverValidatedRead?: boolean } = {},
): Promise<void> {
  if (!target) return;
  const expanded = normalizePathInput(target);
  const absoluteTarget = isAbsolute(expanded) ? expanded : resolve(extCtx.cwd, expanded);
  if (containsPath(extCtx.cwd, absoluteTarget)) return;

  // User has explicitly opted out of path restriction (the Pi default).
  // Pi has no host-level external_directory allow-list to consult, so a
  // ui.confirm prompt has no policy behind it — it would just nag the
  // user on every external path. Defer to Rust, which will accept the
  // path because `restrict_to_project_root` is false.
  if (options.restrictToProjectRoot === false) return;

  // The plugin cannot identify a session-owned bash artifact without copying
  // Rust's task registry. Forward read calls so Rust can apply that exact,
  // session-scoped exception while continuing to reject every other path.
  if (options.serverValidatedRead === true) return;

  // restrict_to_project_root is AFT's full-isolation knob — NOT a per-call
  // permission. When it's on, an out-of-root path is hard-blocked: do NOT
  // prompt (a grant could never override the Rust-side boundary anyway — that
  // produced issue #125's "approved but still fails"). Throw the clear,
  // actionable denial; Pi renders it as the tool result, which IS the user
  // surface here (no separate ignored-panel channel like OpenCode).
  throw new Error(
    `Blocked: '${absoluteTarget}' is outside the project root and restrict_to_project_root is ` +
      "enabled (AFT full isolation). Not overridable per-call; set restrict_to_project_root: false " +
      "in aft.jsonc to allow external paths.",
  );
}

// OpenAI-compatible tool calling requires a root JSON Schema object.
// TypeBox unions of object variants serialize to a bare root-level `anyOf`, so
// keep these schemas flat and validate mode-specific required fields at runtime.
// Compatibility normalization runs before host validation, allowing legacy aliases
// to be converted while explicitly provided canonical fields remain authoritative.
const ReadParams = Type.Object({
  path: Type.String({
    description: "Path to the file to read (absolute or relative to project root)",
  }),
  startLine: optionalInt(1, Number.MAX_SAFE_INTEGER, "1-based line to start reading from"),
  endLine: optionalInt(1, Number.MAX_SAFE_INTEGER, "1-based line to stop reading at (inclusive)"),
  limit: optionalInt(1, Number.MAX_SAFE_INTEGER, "Maximum number of lines to return"),
  offset: optionalInt(
    1,
    Number.MAX_SAFE_INTEGER,
    "1-based line number to start reading from (use with limit)",
  ),
});

const WriteParams = Type.Object({
  path: Type.String({
    description: "Path to the file to write (absolute or relative to project root)",
  }),
  content: Type.String({ description: "Full file contents to write" }),
});

const BatchEditParams = Type.Object({
  oldString: Type.Optional(
    Type.String({ description: "Text to find for a batch find/replace edit" }),
  ),
  newString: Type.Optional(
    Type.String({ description: "Replacement text for a batch find/replace edit" }),
  ),
  replaceAll: Type.Optional(
    Type.Boolean({ description: "Replace every occurrence for this batch item" }),
  ),
  occurrence: optionalInt(
    1,
    Number.MAX_SAFE_INTEGER,
    "1-based occurrence for this batch item (1 = first match)",
  ),
  startLine: optionalInt(
    1,
    Number.MAX_SAFE_INTEGER,
    "1-based start line for a batch line-range edit",
  ),
  endLine: optionalInt(1, Number.MAX_SAFE_INTEGER, "1-based end line for a batch line-range edit"),
  content: Type.Optional(
    Type.String({
      description: "Replacement text for a batch line-range edit (empty string deletes the lines)",
    }),
  ),
});

const EditParams = Type.Object({
  path: Type.String({
    description: "Path to the file to edit (absolute or relative to project root)",
  }),
  symbol: Type.Optional(
    Type.String({ description: "Named symbol to replace (function, class, type)" }),
  ),
  content: Type.Optional(
    Type.String({
      description:
        "Replacement content for symbol mode. For whole-file writes, use the `write` tool.",
    }),
  ),
  appendContent: Type.Optional(
    Type.String({
      description:
        "Append text to the end of the file (creates the file if missing, parent dirs auto-created). When set, other edit modes are ignored.",
    }),
  ),
  edits: Type.Optional(
    Type.Array(BatchEditParams, {
      minItems: 1,
      description:
        "Batch edits — non-empty array of { oldString, newString }, { oldString, newString, replaceAll: true }, or { startLine, endLine, content } objects applied atomically.",
    }),
  ),
});

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

const HashlineEditParams = Type.Object(
  {
    patch: Type.String({
      minLength: 1,
      description:
        "Hashline patch text with one or more [path#TAG] sections and PUT/CUT/REM/MV operations",
    }),
  },
  { additionalProperties: false },
);

function stringList(value: unknown): string[] {
  return Array.isArray(value)
    ? value.filter((item): item is string => typeof item === "string")
    : [];
}

const GrepParams = Type.Object({
  pattern: Type.String({ description: "Regex pattern to search for" }),
  path: Type.Optional(
    Type.String({
      description: "Path scope (file or directory; absolute or relative to project root)",
    }),
  ),
  include: Type.Optional(
    Type.String({ description: "Glob filter for included files (e.g. '*.ts,*.tsx')" }),
  ),
});

export interface ToolSurfaceFlags {
  /** True keeps AFT on host-native names; false registers aft_ alternatives. */
  hoistBuiltinTools?: boolean;
  hoistRead: boolean;
  hoistWrite: boolean;
  hoistEdit: boolean;
  hoistGrep: boolean;
  /**
   * Mirrors the user's `restrict_to_project_root` AFT config (Pi default
   * `false`). When false, the user has explicitly opted into "no
   * restriction" — Pi has no host-level external_directory allow-list, so
   * a `ui.confirm` prompt has no policy to consult and would only annoy
   * the user. When true, Rust hard-rejects out-of-root paths before the
   * plugin layer sees them anyway, so the prompt is also unreachable. We
   * pass this through so `assertExternalDirectoryPermission` can skip the
   * prompt in the false case (the common one) and the helper stays in
   * place as a safety net for unusual contexts that opt into restriction
   * but still want a chance to allow a one-off external write.
   */
  restrictToProjectRoot: boolean;
}

/** Details surfaced to both renderer and agent message stream. */
interface FileMutationDetails {
  diff?: string;
  firstChangedLine?: number;
  additions: number;
  deletions: number;
  replacements?: number;
  editsApplied?: number;
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

function readPathArg(args: { path?: unknown }): string | undefined {
  return typeof args.path === "string" ? args.path : undefined;
}

function mutationFilePathArg(args: { path?: unknown }): string | undefined {
  return typeof args.path === "string" ? args.path : undefined;
}

function hasOwn(record: Record<string, unknown>, key: string): boolean {
  return Object.hasOwn(record, key);
}

function validateBatchEdit(edit: unknown, index: number): void {
  if (!edit || typeof edit !== "object" || Array.isArray(edit)) {
    throw new Error(`batch: edit[${index}] must be an object`);
  }

  const record = edit as Record<string, unknown>;
  if (typeof record.oldString === "string") {
    return;
  }

  if (hasOwn(record, "startLine")) {
    if (
      typeof record.startLine !== "number" ||
      !Number.isInteger(record.startLine) ||
      record.startLine < 0
    ) {
      throw new Error(`batch: edit[${index}] 'startLine' must be a positive integer (1-based)`);
    }
    if (record.startLine === 0) {
      throw new Error(`batch: edit[${index}] 'startLine' must be >= 1 (1-based)`);
    }
    if (
      typeof record.endLine !== "number" ||
      !Number.isInteger(record.endLine) ||
      record.endLine < 0
    ) {
      throw new Error(`batch: edit[${index}] 'endLine' must be a positive integer (1-based)`);
    }
    if (record.endLine === 0) {
      throw new Error(`batch: edit[${index}] 'endLine' must be >= 1 (1-based)`);
    }
    return;
  }

  throw new Error(`batch: edit[${index}] must have either 'oldString' or 'startLine'/'endLine'`);
}

function validateBatchEdits(edits: unknown): void {
  if (edits === undefined) return;
  if (!Array.isArray(edits)) {
    throw new Error("batch: missing required param 'edits' (expected array)");
  }
  if (edits.length === 0) {
    throw new Error("batch: 'edits' array must not be empty");
  }
  edits.forEach((edit, index) => {
    validateBatchEdit(edit, index);
  });
}

function renderReadCall(
  args: { path?: unknown; filePath?: unknown } | undefined,
  theme: Theme,
  context: RenderContextLike,
): Text {
  const text = reuseText(context.lastComponent);
  const filePath = args ? readPathArg(args) : undefined;
  const pathDisplay = filePath
    ? theme.fg("accent", shortenPath(filePath))
    : theme.fg("toolOutput", "...");
  text.setText(`${theme.fg("toolTitle", theme.bold("read"))} ${pathDisplay}`);
  return text;
}

export function registerHoistedTools(
  pi: ExtensionAPI,
  ctx: PluginContext,
  surface: ToolSurfaceFlags,
): void {
  // Registered names are host-local aliases. Permission checks and bridge calls
  // use the bare command names, preserving the Rust wire protocol in both modes.
  const prefix = surface.hoistBuiltinTools === false ? "aft_" : "";
  const readName = `${prefix}read`;
  const writeName = `${prefix}write`;
  const editName = `${prefix}edit`;
  const grepName = `${prefix}grep`;

  if (surface.hoistRead) {
    pi.registerTool(
      withPathAliasPreparation({
        name: readName,
        label: readName,
        description: readDescription(ctx.config.gh_read?.enabled === true),
        promptSnippet: "Read file contents (supports offset/limit for large files)",
        promptGuidelines: [`Use ${readName} to examine files instead of cat or sed.`],
        parameters: ReadParams,
        async execute(
          _toolCallId: string,
          params: Static<typeof ReadParams>,
          _signal,
          _onUpdate,
          extCtx,
        ) {
          const bridge = bridgeFor(ctx, extCtx.cwd);
          const pathArg = readPathArg(params);
          if (typeof pathArg !== "string") {
            throw new Error("read: missing required parameter `path`");
          }
          const startLine = coerceOptionalInt(
            params.startLine,
            "startLine",
            1,
            Number.MAX_SAFE_INTEGER,
          );
          const endLine = coerceOptionalInt(params.endLine, "endLine", 1, Number.MAX_SAFE_INTEGER);
          const offset = coerceOptionalInt(params.offset, "offset", 1, Number.MAX_SAFE_INTEGER);
          const limit = coerceOptionalInt(params.limit, "limit", 1, Number.MAX_SAFE_INTEGER);
          // Resolve ~ / relative once and use the same value for the permission
          // check and the bridge. Without this, hoisted read bypassed Pi's
          // external-path prompt/deny layer while write/edit/grep were guarded.
          const filePath = await resolvePathArg(extCtx.cwd, pathArg);
          await assertExternalDirectoryPermission(extCtx, filePath, {
            restrictToProjectRoot: surface.restrictToProjectRoot,
            serverValidatedRead: true,
          });
          const rawArgs: Record<string, unknown> = { filePath: pathArg };
          if (startLine !== undefined) rawArgs.startLine = startLine;
          if (endLine !== undefined) rawArgs.endLine = endLine;
          if (offset !== undefined) rawArgs.offset = offset;
          if (limit !== undefined) rawArgs.limit = limit;
          const visionCapability = visionCapabilityForPiModel(extCtx);
          if (visionCapability !== undefined) rawArgs.vision_capability = visionCapability;
          const response = await callToolCall(bridge, "read", rawArgs, extCtx);
          if (response.success === false) {
            throw new Error(response.text || response.message || "read failed");
          }
          const agentText = response.text;
          const attachments = readAttachments(response);
          if (attachments.length > 0) {
            const first = attachments[0];
            const mime = typeof first.mime === "string" ? first.mime : "";
            const note =
              typeof agentText === "string" && agentText.length > 0
                ? agentText
                : formatReadAttachmentText(first);
            if (first.kind === "image" || mime.startsWith("image/")) {
              if (typeof first.data === "string" && modelSupportsImages(extCtx)) {
                return contentResult(
                  [
                    { type: "text", text: note },
                    { type: "image", data: first.data, mimeType: mime },
                  ],
                  response,
                );
              }
              return textResult(`${note}\n${NON_VISION_IMAGE_NOTE}`, response);
            }
            if (first.kind === "pdf" || mime === "application/pdf") {
              return textResult(`${note}\nPDFs aren't supported on the Pi harness yet.`, response);
            }
            return textResult(note, response);
          }
          return textResult(agentText, response);
        },
        renderCall(args, theme, context) {
          return renderReadCall(args, theme, context);
        },
      }),
    );
  }

  if (surface.hoistWrite) {
    const writeBackupText =
      ctx.config.backup?.enabled === false
        ? "Backup capture is disabled by user config."
        : "Existing files are backed up before overwriting (undo via aft_safety).";
    pi.registerTool<typeof WriteParams, FileMutationDetails>(
      withPathAliasPreparation({
        name: writeName,
        label: writeName,
        description: `Write content to a file, creating it and parent directories automatically. ${writeBackupText} Auto-formats when the project has a formatter configured. Uses \`path\`. For partial edits, use the \`${editName}\` tool.`,
        promptSnippet: "Create or overwrite files (uses path; auto-formats)",
        promptGuidelines: [`Use ${writeName} only for new files or complete rewrites.`],
        parameters: WriteParams,
        async execute(
          _toolCallId: string,
          params: Static<typeof WriteParams>,
          _signal,
          _onUpdate,
          extCtx,
        ) {
          const filePathArg = mutationFilePathArg(params);
          if (typeof filePathArg !== "string") {
            throw new Error("write: missing required parameter `path`");
          }
          // Resolve ~ and relative paths before the permission check. Pass the
          // original path string in the request so the path the agent receives
          // stays exactly as provided.
          const filePath = await resolvePathArg(extCtx.cwd, filePathArg);
          await assertExternalDirectoryPermission(extCtx, filePath, {
            restrictToProjectRoot: surface.restrictToProjectRoot,
          });
          const bridge = bridgeFor(ctx, extCtx.cwd);
          const rawArgs: Record<string, unknown> = {
            filePath: filePathArg,
            content: params.content,
          };
          const response = await callToolCall(bridge, "write", rawArgs, extCtx);
          if (response.success === false) {
            throw toolErrorFromResponse("write", response);
          }
          return buildMutationResult(response);
        },
        renderCall(args, theme, context) {
          return renderMutationCall("write", mutationFilePathArg(args ?? {}), theme, context);
        },
        renderResult(result, options = { expanded: false, isPartial: false }, theme, context) {
          return renderMutationResult(result, theme, context, options);
        },
      }),
    );
  }

  if (surface.hoistEdit && ctx.hashlineEffective === true) {
    pi.registerTool<typeof HashlineEditParams, FileMutationDetails>({
      name: editName,
      label: editName,
      description: HASHLINE_EDIT_DESCRIPTION,
      promptSnippet: `Apply a tagged hashline patch through the ${editName} tool`,
      promptGuidelines: [
        "Read a file to obtain its current hashline tag before editing it.",
        "Pass exactly one patch string; paths and tags belong in patch section headers.",
      ],
      parameters: HashlineEditParams,
      async execute(
        _toolCallId: string,
        params: Static<typeof HashlineEditParams>,
        _signal,
        _onUpdate,
        extCtx,
      ) {
        const bridge = bridgeFor(ctx, extCtx.cwd);
        const sessionId = resolveSessionId(extCtx);
        const rawArgs = { patch: params.patch };
        const preflight = await callToolCallForSession(
          bridge,
          "hashline_preflight",
          rawArgs,
          sessionId,
          extCtx,
        );
        if (preflight.success === false) throw toolErrorFromResponse("edit", preflight);
        for (const target of [
          ...stringList(preflight.affected_paths),
          ...stringList(preflight.mv_destinations),
        ]) {
          await assertExternalDirectoryPermission(
            extCtx,
            await resolvePathArg(extCtx.cwd, target),
            {
              restrictToProjectRoot: surface.restrictToProjectRoot,
            },
          );
        }
        const preview = await callToolCallForSession(bridge, "edit", rawArgs, sessionId, extCtx, {
          preview: true,
        });
        if (preview.success === false) throw toolErrorFromResponse("edit", preview);
        const response = await callToolCallForSession(bridge, "edit", rawArgs, sessionId, extCtx);
        if (response.success === false) throw toolErrorFromResponse("edit", response);
        return buildMutationResult(response);
      },
    });
  }

  if (surface.hoistEdit && ctx.hashlineEffective !== true) {
    pi.registerTool<typeof EditParams, FileMutationDetails>(
      withPathAliasPreparation({
        name: editName,
        label: editName,
        description:
          "Edit part of a file via `appendContent`, batch `edits[]`, or symbol plus `content`. Batch `{ oldString, newString, replaceAll: true }` replaces every match. Provide exactly one mode per call: appendContent, edits[], or symbol plus content (mixing modes is rejected).",
        promptSnippet:
          "Partial file edits via appendContent, edits[], or symbol plus content (exactly one mode per call).",
        promptGuidelines: [
          `Prefer ${editName} over ${writeName} when changing part of an existing file.`,
          `Use ${editName} with appendContent when adding text to the end of a file.`,
          "Use edits[] for multiple atomic changes in one file.",
          "Include enough surrounding context in an edits[] find/replace item to make the match unique, or set replaceAll/occurrence explicitly.",
        ],
        parameters: EditParams,
        async execute(
          _toolCallId: string,
          params: Static<typeof EditParams>,
          _signal,
          _onUpdate,
          extCtx,
        ) {
          const argsRecord = params as Record<string, unknown>;
          if (argsRecord.startLine !== undefined || argsRecord.endLine !== undefined) {
            throw new Error(
              "edit: 'startLine'/'endLine' are not top-level parameters. " +
                "For line-range edits, nest them inside the `edits` array: " +
                '`edits: [{ startLine: N, endLine: M, content: "..." }]`. ' +
                "For find/replace, use an item in `edits[]` instead.",
            );
          }

          const filePathArg = mutationFilePathArg(params);
          if (typeof filePathArg !== "string") {
            throw new Error("edit: missing required parameter `path`");
          }
          if (params.edits !== undefined) validateBatchEdits(params.edits);
          // Resolve ~ and relative paths before the permission check. Pass the
          // original path string in the request so the path the agent receives
          // stays exactly as provided.
          const filePath = await resolvePathArg(extCtx.cwd, filePathArg);
          await assertExternalDirectoryPermission(extCtx, filePath, {
            restrictToProjectRoot: surface.restrictToProjectRoot,
          });
          const bridge = bridgeFor(ctx, extCtx.cwd);
          const rawArgs: Record<string, unknown> = { path: filePathArg };
          for (const key of ["appendContent", "edits", "symbol", "content"] as const) {
            if (argsRecord[key] !== undefined) rawArgs[key] = argsRecord[key];
          }

          const response = await callToolCall(bridge, "edit", rawArgs, extCtx);
          if (response.success === false) {
            throw toolErrorFromResponse("edit", response);
          }
          return buildMutationResult(response);
        },
        renderCall(args, theme, context) {
          return renderMutationCall("edit", mutationFilePathArg(args ?? {}), theme, context);
        },
        renderResult(result, options = { expanded: false, isPartial: false }, theme, context) {
          return renderMutationResult(result, theme, context, options);
        },
      }),
    );
  }

  if (surface.hoistGrep) {
    pi.registerTool(
      withPathAliasPreparation({
        name: grepName,
        label: grepName,
        description:
          "Search for a regex pattern across files. Uses AFT's trigram index inside the project root for fast repeated queries, and falls back to ripgrep for paths outside the project root.",
        promptSnippet: "Fast regex search across files (trigram-indexed inside the project root)",
        promptGuidelines: [`Prefer ${grepName} over bash-invoked find/rg for in-project searches.`],
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
          let pathSplit: SearchPathArgSplit | undefined;
          if (params.path) {
            pathSplit = await splitSearchPathArg(extCtx.cwd, params.path);
            for (const target of pathSplit.paths) {
              await assertExternalDirectoryPermission(
                extCtx,
                absoluteSearchPath(extCtx.cwd, target),
                {
                  restrictToProjectRoot: surface.restrictToProjectRoot,
                },
              );
            }
            req.path = await bridgeSearchPathArg(extCtx.cwd, pathSplit);
          }
          if (params.include) req.include = params.include;

          const response = await callToolCall(bridge, "grep", req, extCtx);
          if (response.success === false) {
            throw new Error(response.text || response.message || "grep failed");
          }
          if (pathSplit && pathSplit.missing.length > 0) {
            response.complete = false;
          }
          const text = appendSkippedSearchPaths(
            (response.text as string | undefined) ?? "",
            pathSplit?.missing ?? [],
          );
          return textResult(text, response);
        },
      }),
    );
  }
}

// ---------------------------------------------------------------------------
// Mutation helpers — write and edit share result shape and rendering.
// ---------------------------------------------------------------------------

/**
 * Shape a bridge mutation response into an `AgentToolResult` Pi can render.
 * Exported for unit tests covering truncation, diagnostics, and batch-edit
 * summaries without spinning up a real bridge.
 */
export function buildMutationResult(
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
  const editsApplied = response.edits_applied as number | undefined;
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

  let text = response.text as string | undefined;
  if (typeof text !== "string") {
    // Fallback only for unit tests and legacy cases where response.text is
    // missing. Normally the caller has already provided the summary text.
    text = formatEditSummary(response as Record<string, unknown>);
    if (noOp) {
      text +=
        "\n\nNote: no net file change \u2014 the match was found and applied, but the file content is byte-identical to before. Likely causes: oldString and newString are identical, or a formatter normalized the change away.";
    }
    const skipNote = formatSkipReasonNote(formatSkippedReason);
    if (skipNote) text += `\n\n${skipNote}`;
    const globSkipNote = formatGlobSkipReasonsNote(globFormatSkipReasons);
    if (globSkipNote) text += `\n\n${globSkipNote}`;
    if (diagnostics && diagnostics.length > 0) {
      text += `\n\nLSP diagnostics:\n${formatDiagnosticsText(diagnostics)}`;
    }
  }

  return {
    content: [{ type: "text", text }],
    details: {
      diff: diffText,
      firstChangedLine,
      additions,
      deletions,
      replacements,
      editsApplied,
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

export function renderMutationCall(
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

export function renderMutationResult(
  result: AgentToolResult<FileMutationDetails>,
  theme: Theme,
  context: RenderContextLike,
  options: RenderResultOptionsLike = { expanded: true },
): Component {
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
    const countDetail =
      typeof details?.editsApplied === "number" && details.editsApplied > 1
        ? `, ${details.editsApplied} edits`
        : typeof details?.replacements === "number" && details.replacements > 1
          ? `, ${details.replacements} replacements`
          : "";
    const summary = theme.fg("success", `+${additions}/-${deletions}${countDetail}`);
    let suffix = "";
    if (details?.truncated) {
      suffix = ` ${theme.fg("muted", "(diff truncated)")}`;
    } else if (details?.noOp) {
      suffix = ` ${theme.fg("muted", "(no net change)")}`;
    }
    const full = reuseText(context.lastComponent);
    full.setText(`\n${summary}${suffix}`);
    return collapsibleResult({
      summary: `${summary}${suffix}`,
      full,
      expanded: options.expanded,
      context,
    });
  }

  // Diff: render using Pi's built-in renderer for colored lines + intra-line
  // highlighting, wrapped in a Container with a top spacer for breathing room.
  const container = reuseContainer(context.lastComponent);
  container.clear();
  container.addChild(new Spacer(1));
  let renderedDiff: string;
  try {
    renderedDiff = renderDiff(diff);
  } catch {
    // Renderer tests and non-interactive hosts may not have initialized Pi's theme proxy.
    renderedDiff = diff;
  }
  // Normalize before Text wraps the lines, then collapsibleResult applies the
  // same safeguard to every renderer's final terminal-bound output.
  container.addChild(new Text(normalizeTerminalText(renderedDiff), 1, 0));
  const rawPath =
    context.args &&
    typeof context.args === "object" &&
    typeof (context.args as { path?: unknown }).path === "string"
      ? (context.args as { path: string }).path
      : undefined;
  const summaryPath = rawPath ? ` ${shortenPath(rawPath)}` : "";
  const summary = `edited${summaryPath} (+${details?.additions ?? 0}/-${details?.deletions ?? 0})`;
  return collapsibleResult({
    summary,
    full: container,
    expanded: options.expanded,
    context,
  });
}

function shortenPath(path: string): string {
  const home = homedir();
  if (path.startsWith(home)) return `~${path.slice(home.length)}`;
  return path;
}

/** Resolve a path argument to an absolute path if it exists, decoding file:
 * URLs and expanding `~`. */
export async function resolvePathArg(cwd: string, path: string): Promise<string> {
  const expanded = normalizePathInput(path);
  const abs = absoluteSearchPath(cwd, path);
  try {
    await stat(abs);
    return abs;
  } catch {
    return expanded;
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
 * Build the navigation footer for a `read` response.
 *
 * The pure clamping/range logic lives in aft-bridge. Pi keeps the
 * host-specific parameter hint (`offset/limit`) here so existing agent-facing
 * output stays byte-for-byte identical.
 */
export function formatReadFooter(
  agentSpecifiedRange: boolean,
  data: Record<string, unknown>,
): string {
  return formatSharedReadFooter(agentSpecifiedRange, data, { rangeHint: "offset/limit" });
}
