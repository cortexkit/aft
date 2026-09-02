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
import { assertExternalDirectoryPermission, resolvePathArg } from "./hoisted.js";
import {
  accentPath,
  asRecord,
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

const OutlineParams = Type.Object({
  target: Type.Union([Type.String(), Type.Array(Type.String())], {
    description:
      "What to outline: a file path, directory path, URL (http:// or https://), or array of file paths. The mode is auto-detected: URLs by `http://`/`https://` prefix, directories by stat, arrays as multi-file. Directory walks cap at 200 files.",
  }),
  files: Type.Optional(
    Type.Boolean({
      description:
        "Directory-only mode: when true, target must be a directory or array of directories and the result is a flat file tree with path, language, symbol count, and byte size instead of a symbol outline.",
    }),
  ),
  includeTests: Type.Optional(
    Type.Boolean({
      description:
        "Directory outline only: include test files. Defaults to false; tests are hidden.",
    }),
  ),
});

const ZoomTarget = Type.Object({
  path: Type.String({ description: "Path to file (absolute or project-relative)" }),
  symbol: Type.String({ description: "Symbol name in that file" }),
});

const ZoomParams = Type.Object({
  path: Type.Optional(Type.String({ description: "Path to file (absolute or project-relative)" })),
  url: Type.Optional(
    Type.String({
      description: "HTTP/HTTPS URL of an HTML or Markdown document to fetch and zoom into",
    }),
  ),
  symbols: Type.Optional(
    Type.Union([Type.String(), Type.Array(Type.String())], {
      description:
        "Symbol name for code, or heading text for Markdown/HTML. Pass a string or array for batched lookups in the same file/URL.",
    }),
  ),
  targets: Type.Optional(
    Type.Union([ZoomTarget, Type.Array(ZoomTarget)], {
      description:
        "Cross-file batch: `{ path, symbol }` or an array of them. Mutually exclusive with path/url/symbols.",
    }),
  ),
  contextLines: optionalInt(
    1,
    Number.MAX_SAFE_INTEGER,
    "Lines of context before/after (default: 3)",
  ),
  callgraph: Type.Optional(
    Type.Boolean({
      description:
        "Include call-graph annotations (calls-out / called-by within the same file). Default false; off keeps zoom output minimal.",
    }),
  ),
});

function isUrl(s: string): boolean {
  return s.startsWith("http://") || s.startsWith("https://");
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

/** Best-effort label for renderers when zoom is called with `path` OR `url`. */
function zoomTargetLabel(args: { path?: string; url?: string }): string {
  return args.path ?? args.url ?? "(no target)";
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
export function buildZoomSections(
  args: Static<typeof ZoomParams>,
  payload: unknown,
  theme: Theme,
): string[] {
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
        const itemTargetLabel = asString(record.targetLabel) ?? zoomTargetLabel(args);
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
      return renderZoomRecord(record, zoomTargetLabel(args), theme);
    })
    .filter(Boolean);
}

/** Exported for renderer unit tests. */
export function renderOutlineCall(
  args: Static<typeof OutlineParams>,
  theme: Theme,
  context: RenderContextLike,
) {
  const summary = Array.isArray(args.target)
    ? theme.fg("accent", `${args.target.length} ${args.files ? "directories" : "files"}`)
    : typeof args.target === "string"
      ? `${accentPath(theme, args.target)}${args.files ? " files" : ""}`
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
export function renderZoomCall(
  args: Static<typeof ZoomParams>,
  theme: Theme,
  context: RenderContextLike,
) {
  // `symbols` accepts string OR string[]; renderer adapts to both shapes.
  const symbols = args.symbols;
  const targets = args.targets;
  let summary: string;
  if (typeof symbols === "string") {
    summary = theme.fg("toolOutput", symbols);
  } else if (Array.isArray(symbols) && symbols.length > 0) {
    summary = theme.fg("toolOutput", `${symbols.length} symbols`);
  } else if (Array.isArray(targets) && targets.length > 0) {
    summary = theme.fg("toolOutput", `${targets.length} targets`);
  } else if (targets && typeof targets === "object" && !Array.isArray(targets)) {
    summary = theme.fg("toolOutput", (targets as { symbol?: string }).symbol ?? "1 target");
  } else {
    summary = theme.fg("toolOutput", "lines");
  }
  return renderToolCall(
    "zoom",
    `${accentPath(theme, zoomTargetLabel(args))} ${summary}`,
    theme,
    context,
  );
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

export function registerReadingTools(
  pi: ExtensionAPI,
  ctx: PluginContext,
  surface: ReadingSurface,
): void {
  if (surface.outline) {
    pi.registerTool(
      withPathAliasPreparation({
        name: "aft_outline",
        label: "outline",
        description:
          "Structural outline of source code, documentation files, or remote URLs. For code, returns symbols (functions, classes, types) with line ranges. For Markdown and HTML, returns heading hierarchy. Use this to explore structure before reading specific sections with aft_zoom. Set `files: true` with a directory target for a flat indexed file tree with language, symbol count, and byte metadata.\n\nFor understanding a specific feature, prefer aft_search + aft_zoom on named symbols; use aft_outline on a whole directory only for high-level structure mapping. aft_zoom with `callgraph:true` gives one-level forward calls-out; use aft_callgraph only for reverse callers or multi-level traces.\n\nPass a single `target`:\n  • file path → outline that file (with signatures)\n  • directory path → outline source files under it (recursively, up to 200 files)\n  • URL (http:// or https://) → fetch and outline a remote HTML/Markdown document\n  • array of paths → outline multiple files in one call; with files:true, every path must be a directory",
        parameters: OutlineParams,
        async execute(
          _toolCallId: string,
          params: Static<typeof OutlineParams>,
          _signal,
          onUpdate,
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

          const response = await callToolCall(bridge, "outline", rawArgs, extCtx, { onUpdate });
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
      }),
    );
  }

  if (surface.zoom) {
    pi.registerTool(
      withPathAliasPreparation({
        name: "aft_zoom",
        label: "zoom",
        description:
          "Inspect code symbols or documentation sections. For code, returns the full source of a symbol. Pass `callgraph: true` to also include call-graph annotations (calls-out / called-by within the same file). For Markdown and HTML, returns the section content under the given heading.\n\nUse exactly ONE mode: `{ path, symbols }`, `{ url, symbols }`, or `{ targets }`. `symbols` can be a string or array (one or many lookups in the same file/URL). Use `targets` for cross-file batches: `{ path, symbol }` or an array of them.",
        parameters: ZoomParams,
        async execute(
          _toolCallId: string,
          params: Static<typeof ZoomParams>,
          _signal,
          onUpdate,
          extCtx,
        ) {
          const bridge = bridgeFor(ctx, extCtx.cwd);
          // GPT-family models send empty strings / empty arrays / empty objects
          // instead of omitting optional params. Use `isEmptyParam` so e.g.
          // `targets: []` or `url: ""` don't trigger mutual-exclusion errors
          // against fields the agent didn't actually intend to provide.
          // `targets` also accepts nested object/array shapes. Only treat
          // `targets` as not-provided when EVERY entry is fully empty
          // (`[{path: "", symbol: ""}]`, `{path: "", symbol: ""}`)
          // — that's the GPT-class "I didn't intend this param" signal.
          // If any entry has even one non-empty field, the agent intends
          // targets mode; let the per-entry validation below surface the
          // specific error ("targets[0].path must be non-empty" etc).
          const hasTargetsProvided = (t: unknown): boolean => {
            if (isEmptyParam(t)) return false;
            const entryEmpty = (entry: unknown): boolean => {
              if (!entry || typeof entry !== "object") return true;
              const fp = (entry as { path?: unknown }).path;
              const sym = (entry as { symbol?: unknown }).symbol;
              const fpEmpty = typeof fp !== "string" || fp.length === 0;
              const symEmpty = typeof sym !== "string" || sym.length === 0;
              return fpEmpty && symEmpty;
            };
            if (Array.isArray(t)) return !t.every(entryEmpty);
            return !entryEmpty(t);
          };
          const hasPath = !isEmptyParam(params.path);
          const hasUrl = !isEmptyParam(params.url);
          const hasTargets = hasTargetsProvided(params.targets);
          const hasSymbols = !isEmptyParam(params.symbols);
          // Coerce stringified booleans and numbers here so they reach the server
          // in the same form the main client sends.
          const wantCallgraph = coerceBoolean(params.callgraph);
          const contextLines = coerceOptionalInt(
            params.contextLines,
            "contextLines",
            1,
            Number.MAX_SAFE_INTEGER,
          );

          // Multi-target mode (cross-file). Mutually exclusive with the other
          // modes so the agent doesn't accidentally provide overlapping inputs
          // that get silently ignored.
          if (hasTargets) {
            if (hasPath || hasUrl || hasSymbols) {
              throw new Error("'targets' is mutually exclusive with 'path', 'url', and 'symbols'");
            }
            const targets = Array.isArray(params.targets)
              ? (params.targets as Array<{ path: string; symbol: string }>)
              : ([params.targets] as Array<{ path: string; symbol: string }>);
            if (targets.length === 0) {
              throw new Error("'targets' must be a non-empty object or array");
            }
            for (const [i, entry] of targets.entries()) {
              if (!entry || typeof entry.path !== "string" || entry.path.length === 0) {
                throw new Error(`targets[${i}].path must be a non-empty string`);
              }
              if (typeof entry.symbol !== "string" || entry.symbol.length === 0) {
                throw new Error(`targets[${i}].symbol must be a non-empty string`);
              }
            }
            const resolvedTargets = await Promise.all(
              targets.map((t) => resolvePathArg(extCtx.cwd, t.path)),
            );
            await assertReadPathPermissions(extCtx, ctx, resolvedTargets);

            const rawArgs: Record<string, unknown> = {
              targets: targets.map((target) => ({ filePath: target.path, symbol: target.symbol })),
            };
            if (contextLines !== undefined) rawArgs.contextLines = contextLines;
            if (wantCallgraph) rawArgs.callgraph = true;

            const response = await callToolCall(bridge, "zoom", rawArgs, extCtx, { onUpdate });
            if (response.success === false) {
              throw new Error(response.text || response.message || "zoom failed");
            }
            return textResult(response.text, response);
          }

          if (!hasPath && !hasUrl) {
            throw new Error("Provide exactly one of 'path', 'url', or 'targets'");
          }
          if (hasPath && hasUrl) {
            throw new Error("Provide exactly ONE of 'path' or 'url' — not both");
          }

          // URL mode passes through to Rust; Rust fetches, validates, and caches.
          // File mode still resolves locally before dispatch so external-directory
          // permission checks approve the same path the server will read.
          if (!hasUrl) {
            const file = await resolvePathArg(extCtx.cwd, params.path as string);
            await assertReadPathPermissions(extCtx, ctx, file);
          }

          const rawArgs: Record<string, unknown> = hasUrl
            ? { url: params.url }
            : { filePath: params.path };
          if (hasSymbols) rawArgs.symbols = params.symbols;
          if (contextLines !== undefined) rawArgs.contextLines = contextLines;
          if (wantCallgraph) rawArgs.callgraph = true;

          const response = await callToolCall(bridge, "zoom", rawArgs, extCtx, { onUpdate });
          if (response.success === false) {
            throw new Error(response.text || response.message || "zoom failed");
          }
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
