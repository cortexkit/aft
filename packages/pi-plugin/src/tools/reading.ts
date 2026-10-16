/**
 * AFT reading tools: aft_outline + aft_zoom.
 * Structural overview and symbol/section inspection.
 */

import { coerceBoolean, coerceTargetParam } from "@cortexkit/aft-bridge";
import type {
  AgentToolResult,
  ExtensionAPI,
  ExtensionContext,
  Theme,
} from "@earendil-works/pi-coding-agent";
import { type Static, Type } from "typebox";
import { resolveGithubConfig, toolEnabled } from "../config.js";
import type { PluginContext } from "../types.js";
import {
  bridgeFor,
  callToolCall,
  coerceOptionalInt,
  isEmptyParam,
  optionalInt,
  textResult,
  withPathAliasPreparation,
} from "./_shared.js";
import { assertExternalDirectoryPermission, resolvePathArg, whenGhReadEnabled } from "./hoisted.js";
import {
  accentPath,
  asRecord,
  asRecordOrEmpty,
  asRecords,
  asString,
  collapsibleResult,
  collectTextContent,
  extractStructuredPayload,
  type RenderContextLike,
  type RenderResultOptionsLike,
  renderErrorResult,
  renderSections,
  renderToolCall,
  shortenPath,
} from "./render-helpers.js";

/** GitHub discussion targets are fetched by the server, not read from disk. */
function isGithubPathArg(value: unknown): boolean {
  return typeof value === "string" && (value.startsWith("issue://") || value.startsWith("pr://"));
}

const OutlineParams = Type.Object({
  target: Type.Union([Type.String(), Type.Array(Type.String())], {
    description:
      "What to outline: a file path, directory path, URL (http:// or https://), or array of file paths. The mode is auto-detected: URLs by `http://`/`https://` prefix, directories by stat, arrays as multi-file.",
  }),
  files: Type.Optional(
    Type.Boolean({
      description:
        "Directory-only mode: when true, target must be a directory or array of directories and the result is a breadth-first file tree with directory rollups plus language, symbol, and line counts.",
    }),
  ),
  includeTests: Type.Optional(
    Type.Boolean({
      description:
        "Directory outline only: include test files. Defaults to false; tests are hidden.",
    }),
  ),
});

const ZoomEntry = Type.Object({
  target: Type.String({
    description:
      "File path (absolute or project-relative) or HTTP/HTTPS URL, auto-detected; issue://N or pr://N when GitHub reads are enabled",
  }),
  symbols: Type.Union([Type.String(), Type.Array(Type.String())], {
    description:
      "Symbol name for code or heading text for Markdown/HTML; a string or array of strings",
  }),
});

const ZoomParams = Type.Object({
  batch: Type.Array(ZoomEntry, {
    minItems: 1,
    description: "One or more lookups; entries may mix file paths and URLs",
  }),
  contextLines: optionalInt(
    1,
    Number.MAX_SAFE_INTEGER,
    "Lines of context before/after each symbol (default: 3)",
  ),
  callgraph: Type.Optional(
    Type.Boolean({
      description: "Include call-graph annotations for every entry (default: false)",
    }),
  ),
});

function isUrl(s: string): boolean {
  return (
    s.startsWith("http://") ||
    s.startsWith("https://") ||
    s.startsWith("issue://") ||
    s.startsWith("pr://")
  );
}

async function assertReadPathPermissions(
  extCtx: ExtensionContext,
  ctx: PluginContext,
  paths: string | string[],
): Promise<void> {
  const targets = Array.isArray(paths) ? paths : [paths];
  const checked = new Set<string>();
  for (const target of targets) {
    if (!target || checked.has(target)) continue;
    checked.add(target);
    await assertExternalDirectoryPermission(extCtx, target, {
      restrictToProjectRoot: ctx.config.restrict_to_project_root ?? false,
    });
  }
}

function zoomTargetLabel(args: unknown): string {
  const safeArgs = asRecordOrEmpty(args);
  if (Array.isArray(safeArgs.batch)) {
    if (safeArgs.batch.length > 1) return `${safeArgs.batch.length} locations`;
    return asString(asRecord(safeArgs.batch[0])?.target) ?? "(no target)";
  }
  return (
    asString(safeArgs.target) ?? asString(safeArgs.path) ?? asString(safeArgs.url) ?? "(no target)"
  );
}

export interface ReadingSurface {
  outline: boolean;
  zoom: boolean;
}

function zoomExtraCallSites(call: Record<string, unknown>): string {
  const extraCount = call.extra_count;
  return typeof extraCount === "number" && Number.isInteger(extraCount) && extraCount > 0
    ? ` +${extraCount}`
    : "";
}

/** Exported for renderer unit tests. */
export function buildOutlineSections(text: string, theme: Theme): string[] {
  const trimmed = text.trim();
  if (!trimmed) return [theme.fg("muted", "No outline available.")];

  const lines = trimmed.split("\n");
  if (lines.length === 1) return [theme.fg("accent", lines[0])];
  return [theme.fg("accent", lines[0]), lines.slice(1).join("\n")];
}

function renderZoomRecord(
  record: Record<string, unknown>,
  targetLabel: string,
  theme: Theme,
): string {
  const name = asString(record.name) ?? "(unknown symbol)";
  const kind = asString(record.kind) ?? "symbol";
  const range = asRecord(record.range);
  const startLine = range && typeof range.start_line === "number" ? range.start_line : undefined;
  const endLine = range && typeof range.end_line === "number" ? range.end_line : undefined;
  const location =
    startLine !== undefined
      ? `${shortenPath(targetLabel)}:${startLine}${endLine && endLine !== startLine ? `-${endLine}` : ""}`
      : shortenPath(targetLabel);
  const lines = [`${theme.fg("accent", name)} ${theme.fg("muted", `[${kind}] ${location}`)}`];

  const content = asString(record.content);
  if (content) {
    lines.push(
      content
        .split("\n")
        .map((line) => `  ${line}`)
        .join("\n"),
    );
  }

  const annotations = asRecord(record.annotations);
  const callsOut = annotations ? asRecords(annotations.calls_out) : [];
  const calledBy = annotations ? asRecords(annotations.called_by) : [];
  if (callsOut.length > 0) {
    lines.push(
      `${theme.fg("muted", "calls out")}`,
      callsOut
        .map(
          (call) =>
            `  ↳ ${asString(call.name) ?? "(unknown)"}${typeof call.line === "number" ? `:${call.line}` : ""}${zoomExtraCallSites(call)}`,
        )
        .join("\n"),
    );
  }
  if (calledBy.length > 0) {
    lines.push(
      `${theme.fg("muted", "called by")}`,
      calledBy
        .map(
          (call) =>
            `  ↳ ${asString(call.name) ?? "(unknown)"}${typeof call.line === "number" ? `:${call.line}` : ""}${zoomExtraCallSites(call)}`,
        )
        .join("\n"),
    );
  }

  return lines.join("\n");
}

/** Exported for renderer unit tests. */
export function buildZoomSections(args: unknown, payload: unknown, theme: Theme): string[] {
  const safeArgs = asRecordOrEmpty(args);
  const batch = asRecord(payload);
  const batchItems = Array.isArray(batch?.targets)
    ? (batch.targets as unknown[])
    : Array.isArray(batch?.symbols)
      ? (batch.symbols as unknown[])
      : Array.isArray(batch?.entries)
        ? (batch.entries as unknown[])
        : null;
  if (batchItems) {
    const header =
      batch?.complete === false ? [theme.fg("warning", "Incomplete zoom results")] : [];
    return [
      ...header,
      ...batchItems.map((item) => {
        const record = asRecord(item);
        if (!record) return theme.fg("muted", "No zoom result available.");
        const response = asRecord(record.response) ?? record;
        const name = asString(record.name) ?? asString(response.name) ?? "(unknown symbol)";
        const itemTargetLabel = asString(record.targetLabel) ?? zoomTargetLabel(safeArgs);
        if (response.success === false || record.success === false) {
          const location = record.targetLabel ? ` in ${shortenPath(itemTargetLabel)}` : "";
          return theme.fg(
            "error",
            `Symbol "${name}" not found${location}: ${asString(response.message) ?? asString(record.error) ?? "zoom failed"}`,
          );
        }
        if (record.response) {
          return renderZoomRecord(
            { ...response, name: asString(response.name) ?? name },
            itemTargetLabel,
            theme,
          );
        }
        const content = asString(record.content);
        if (content) {
          return [
            `${theme.fg("accent", name)} ${theme.fg("muted", shortenPath(itemTargetLabel))}`,
            content,
          ]
            .filter(Boolean)
            .join("\n");
        }
        return renderZoomRecord(record, itemTargetLabel, theme);
      }),
    ];
  }

  const items = Array.isArray(payload) ? payload : payload ? [payload] : [];
  if (items.length === 0) return [theme.fg("muted", "No zoom result available.")];

  return items
    .map((item) => {
      const record = asRecord(item);
      if (!record) return theme.fg("muted", "No zoom result available.");
      return renderZoomRecord(record, zoomTargetLabel(safeArgs), theme);
    })
    .filter(Boolean);
}

/** Exported for renderer unit tests. */
export function renderOutlineCall(args: unknown, theme: Theme, context: RenderContextLike) {
  const safeArgs = asRecordOrEmpty(args);
  const target = safeArgs.target;
  const files = safeArgs.files === true;
  const summary = Array.isArray(target)
    ? theme.fg("accent", `${target.length} ${files ? "directories" : "files"}`)
    : typeof target === "string"
      ? `${accentPath(theme, target)}${files ? " files" : ""}`
      : undefined;
  return renderToolCall("outline", summary, theme, context);
}

/** Exported for renderer unit tests. */
export function renderOutlineResult(
  result: AgentToolResult<unknown>,
  theme: Theme,
  context: RenderContextLike,
  options: RenderResultOptionsLike = { expanded: true },
) {
  if (context.isError) return renderErrorResult(result, "outline failed", theme, context);
  const sections = buildOutlineSections(collectTextContent(result), theme);
  return collapsibleResult({
    summary: sections[0] ?? "outline completed",
    full: renderSections(sections, context),
    expanded: options.expanded,
    context,
  });
}

/** Exported for renderer unit tests. */
export function renderZoomCall(args: unknown, theme: Theme, context: RenderContextLike) {
  const safeArgs = asRecordOrEmpty(args);
  const entries = Array.isArray(safeArgs.batch) ? safeArgs.batch : [];
  const hasExplicitTarget =
    entries.length > 0 ||
    typeof safeArgs.target === "string" ||
    typeof safeArgs.path === "string" ||
    typeof safeArgs.url === "string";
  // `symbols` accepts string OR string[]; renderer adapts to both shapes.
  const symbols = entries.length === 1 ? asRecord(entries[0])?.symbols : safeArgs.symbols;
  const targets = entries.length > 1 ? entries : safeArgs.targets;
  let summary: string;
  if (typeof symbols === "string") {
    summary = theme.fg("toolOutput", symbols);
  } else if (Array.isArray(symbols) && symbols.length > 0) {
    summary = theme.fg("toolOutput", `${symbols.length} symbols`);
  } else if (Array.isArray(targets) && targets.length > 0) {
    summary = theme.fg("toolOutput", `${targets.length} targets`);
  } else if (targets && typeof targets === "object" && !Array.isArray(targets)) {
    summary = theme.fg("toolOutput", asString(asRecord(targets)?.symbol) ?? "1 target");
  } else if (hasExplicitTarget) {
    summary = theme.fg("toolOutput", "lines");
  } else {
    summary = "";
  }
  const hasKnownTarget =
    hasExplicitTarget ||
    typeof symbols === "string" ||
    Array.isArray(symbols) ||
    Array.isArray(targets) ||
    asRecord(targets) !== undefined;
  const display = [
    hasKnownTarget ? accentPath(theme, zoomTargetLabel(safeArgs)) : undefined,
    summary || undefined,
  ]
    .filter(Boolean)
    .join(" ");
  return renderToolCall("zoom", display, theme, context);
}

/** Exported for renderer unit tests. */
export function renderZoomResult(
  result: AgentToolResult<unknown>,
  args: Static<typeof ZoomParams>,
  theme: Theme,
  context: RenderContextLike,
  options: RenderResultOptionsLike = { expanded: true },
) {
  if (context.isError) return renderErrorResult(result, "zoom failed", theme, context);
  const sections = buildZoomSections(args, extractStructuredPayload(result), theme);
  return collapsibleResult({
    summary: sections[0] ?? "zoom completed",
    full: renderSections(sections, context),
    expanded: options.expanded,
    context,
  });
}

/**
 * The outline description's steer toward focused reading. It names
 * `aft_search`, `aft_zoom` and `aft_callgraph` only when each is registered,
 * falling back to `read` for symbol reading. Kept identical to the OpenCode copy.
 */
function outlineFocusSteer(config: PluginContext["config"]): string {
  const zoomEnabled = toolEnabled(config, "aft_zoom");
  const reader = zoomEnabled ? "aft_zoom" : "read";
  const locate = toolEnabled(config, "aft_search") ? `aft_search + ${reader}` : reader;
  const callgraph = zoomEnabled
    ? toolEnabled(config, "aft_callgraph")
      ? " aft_zoom with `callgraph:true` gives one-level forward calls-out; use aft_callgraph only for reverse callers or multi-level traces."
      : " aft_zoom with `callgraph:true` gives one-level forward calls-out."
    : "";
  return `For understanding a specific feature, prefer ${locate} on named symbols; use aft_outline on a whole directory only for high-level structure mapping.${callgraph}`;
}

export function registerReadingTools(
  pi: ExtensionAPI,
  ctx: PluginContext,
  surface: ReadingSurface,
): void {
  const zoomEnabled = toolEnabled(ctx.config, "aft_zoom");
  const ghReadEnabled = resolveGithubConfig(ctx.config).read;
  const githubOutlineDescription = whenGhReadEnabled(
    ghReadEnabled,
    "GitHub issues and pull requests can be outlined with `issue://NUMBER` or `pr://NUMBER` (including `OWNER/REPO` forms).",
  );
  const githubZoomDescription = whenGhReadEnabled(
    ghReadEnabled,
    "Batch targets may use `issue://NUMBER` or `pr://NUMBER` (including `OWNER/REPO` forms) to inspect GitHub discussion ordinals.",
  );
  if (surface.outline) {
    const outlineTool = withPathAliasPreparation({
      name: "aft_outline",
      label: "outline",
      description: `Structural outline of source code, documentation files, or remote URLs. For code, returns symbols (functions, classes, types) with line ranges. For Markdown and HTML, returns heading hierarchy. Use this to explore structure before reading specific sections with ${zoomEnabled ? "aft_zoom" : "read"}. With \`files: true\`, the outline is breadth-first with directory rollups; drill in by outlining a subdirectory. Rows show language, symbol count, and line count.\n\n${outlineFocusSteer(ctx.config)}\n\nPass a single \`target\`:\n  • file path → outline that file (with signatures)\n  • directory path → outline source files under it\n  • URL (http:// or https://) → fetch and outline a remote HTML/Markdown document\n  • array of paths → outline multiple files in one call; with files:true, every path must be a directory${githubOutlineDescription ? `\n\n${githubOutlineDescription}` : ""}\n\nWhen a list is cut, the reply ends with \`shown N of M <unit> (<reason>) · narrow: <knobs>\`; absence of that line means the list is complete.`,
      parameters: OutlineParams,
      async execute(
        _toolCallId: string,
        params: Static<typeof OutlineParams>,
        _signal,
        _onUpdate,
        extCtx,
      ) {
        const bridge = bridgeFor(ctx, extCtx.cwd);
        // Coerce at the boundary: a host may deliver the string|array `target` as
        // a JSON-stringified array, which would otherwise be treated as one
        // literal path (coerceTargetParam). And a stringified "true" must enable
        // files mode (coerceBoolean).
        const target = coerceTargetParam(params.target);
        const filesMode = coerceBoolean(params.files);
        const hasIncludeTests = !isEmptyParam(params.includeTests);
        const includeTests = coerceBoolean(params.includeTests);
        const rawArgs: Record<string, unknown> = {
          target,
          ...(filesMode ? { files: true } : {}),
          ...(hasIncludeTests ? { includeTests } : {}),
        };

        if (Array.isArray(target)) {
          if (target.length === 0) {
            throw new Error("'target' must be a non-empty string or array of strings");
          }
          const resolvedTargets = await Promise.all(
            (target as string[]).map((entry) => resolvePathArg(extCtx.cwd, entry)),
          );
          await assertReadPathPermissions(extCtx, ctx, resolvedTargets);
        } else {
          if (typeof target !== "string" || target.length === 0) {
            throw new Error("'target' must be a non-empty string or array of strings");
          }
          if (!(!filesMode && isUrl(target))) {
            const resolvedTarget = await resolvePathArg(extCtx.cwd, target);
            await assertReadPathPermissions(extCtx, ctx, resolvedTarget);
          }
        }

        const response = await callToolCall(bridge, "outline", rawArgs, extCtx);
        if (response.success === false) {
          throw new Error(response.text || response.message || "outline failed");
        }
        let text = typeof response.text === "string" ? response.text : "";
        const trimmed = text.trim();
        if (trimmed.startsWith("{") && trimmed.endsWith("}")) {
          try {
            const parsed = JSON.parse(text);
            if (typeof parsed.text === "string") {
              text = parsed.text;
            }
          } catch {
            // keep original text
          }
        }
        return textResult(text, response);
      },
      renderCall(args, theme, context) {
        return renderOutlineCall(args, theme, context);
      },
      renderResult(result, options = { expanded: false, isPartial: false }, theme, context) {
        return renderOutlineResult(result, theme, context, options);
      },
    });
    pi.registerTool(outlineTool);
  }

  if (surface.zoom) {
    pi.registerTool(
      withPathAliasPreparation({
        name: "aft_zoom",
        label: "zoom",
        description:
          'Inspect code symbols or documentation sections. Use `batch: [{ target, symbols }]` for a single lookup. Mix files and URLs in one batch, for example `batch: [{ target: "src/app.ts", symbols: "run" }, { target: "https://example.com/docs", symbols: "Overview" }]`. Each target is auto-detected as a file path or an HTTP/HTTPS URL. `symbols` is a string or string array. Markdown and HTML lookups return the section under the heading. Top-level `contextLines` and `callgraph` apply to every entry; callgraph adds calls-out and called-by annotations. Each failed lookup reports its own error line.' +
          (githubZoomDescription ? `\n\n${githubZoomDescription}` : ""),
        parameters: ZoomParams,
        async execute(
          _toolCallId: string,
          params: Static<typeof ZoomParams>,
          _signal,
          _onUpdate,
          extCtx,
        ) {
          const bridge = bridgeFor(ctx, extCtx.cwd);
          if (!Array.isArray(params.batch) || params.batch.length === 0)
            throw new Error("'batch' must be a non-empty array of {target, symbols}");
          // Remote targets are fetched by Rust; only filesystem locations need
          // external-directory approval before dispatch.
          const paths = params.batch
            .map((entry) => entry.target)
            .filter((target) => target && !isGithubPathArg(target) && !isUrl(target));
          const resolved = await Promise.all(
            paths.map((target) => resolvePathArg(extCtx.cwd, target)),
          );
          await assertReadPathPermissions(extCtx, ctx, resolved);
          const rawArgs: Record<string, unknown> = { batch: params.batch };
          const contextLines = coerceOptionalInt(
            params.contextLines,
            "contextLines",
            1,
            Number.MAX_SAFE_INTEGER,
          );
          if (contextLines !== undefined) rawArgs.contextLines = contextLines;
          if (coerceBoolean(params.callgraph)) rawArgs.callgraph = true;
          const response = await callToolCall(bridge, "zoom", rawArgs, extCtx);
          if (response.success === false)
            throw new Error(response.text || response.message || "zoom failed");
          return textResult(response.text, response);
        },
        renderCall(args, theme, context) {
          return renderZoomCall(args, theme, context);
        },
        renderResult(result, options = { expanded: false, isPartial: false }, theme, context) {
          return renderZoomResult(result, context.args, theme, context, options);
        },
      }),
    );
  }
}
