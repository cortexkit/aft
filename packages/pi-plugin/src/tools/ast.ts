/**
 * ast_grep_search + ast_grep_replace — AST-aware pattern search/rewrite.
 * 9 languages: typescript, tsx, javascript, python, rust, go, pascal, r, objc.
 */

import { coerceBoolean } from "@cortexkit/aft-bridge";
import { StringEnum } from "@earendil-works/pi-ai";
import type {
  AgentToolResult,
  ExtensionAPI,
  ExtensionContext,
  Theme,
} from "@earendil-works/pi-coding-agent";
import { type Static, Type } from "typebox";
import type { PluginContext } from "../types.js";
import { bridgeFor, callToolCall, isEmptyParam, optionalInt, textResult } from "./_shared.js";
import { assertExternalDirectoryPermission, resolvePathArg } from "./hoisted.js";
import {
  asNumber,
  asRecord,
  asRecords,
  asString,
  collapsibleResult,
  collectTextContent,
  extractStructuredPayload,
  formatValue,
  groupByFile,
  type RenderContextLike,
  type RenderResultOptionsLike,
  renderErrorResult,
  renderSections,
  renderToolCall,
  renderUnifiedDiff,
  shortenPath,
} from "./render-helpers.js";

const AstLang = StringEnum(
  ["typescript", "tsx", "javascript", "python", "rust", "go", "pascal", "r", "objc"] as const,
  {
    description: "Target language",
  },
);

const SearchParams = Type.Object({
  pattern: Type.String({
    description:
      "AST pattern with meta-variables (`$VAR` matches one node, `$$$` matches many). Must be a complete AST node.",
  }),
  lang: AstLang,
  paths: Type.Optional(
    Type.Array(Type.String(), { description: "Paths to search (default: ['.'])" }),
  ),
  globs: Type.Optional(
    Type.Array(Type.String(), { description: "Include/exclude globs (prefix `!` to exclude)" }),
  ),
  contextLines: optionalInt(
    1,
    Number.MAX_SAFE_INTEGER,
    "Number of context lines around each match",
  ),
});

const ReplaceParams = Type.Object({
  pattern: Type.String({ description: "AST pattern with meta-variables" }),
  rewrite: Type.String({ description: "Replacement pattern, can reference $VAR from pattern" }),
  lang: AstLang,
  paths: Type.Optional(Type.Array(Type.String(), { description: "Paths (default: ['.'])" })),
  globs: Type.Optional(Type.Array(Type.String(), { description: "Include/exclude globs" })),
  dryRun: Type.Optional(Type.Boolean({ description: "Preview without applying (default: false)" })),
});

export interface AstSurface {
  astSearch: boolean;
  astReplace: boolean;
}

/**
 * Append the server-side `hint` field if present. Rust attaches a hint to
 * zero-match responses when the pattern looks like a common mistake (regex
 * syntax, language-specific shape, today's Rust match-arm `|` trap).
 * See `crates/aft/src/ast_grep_hints.rs` for the rules.
 */
function appendHintSection(response: Record<string, unknown>, sections: string[], theme: Theme) {
  const hint = asString(response.hint);
  if (hint && hint.length > 0) {
    sections.push(theme.fg("warning", hint));
  }
}

/** Append honest scope reporting (no_files_matched_scope + scope_warnings) when present. */
function appendScopeSections(response: Record<string, unknown>, sections: string[], theme: Theme) {
  if (response.no_files_matched_scope === true) {
    sections.push(
      theme.fg("warning", "No files matched the scope (paths/globs resolved to zero files)"),
    );
  }
  const warnings = asRecords(response.scope_warnings);
  // scope_warnings is an array of strings, not records — handle both shapes
  const warningStrings = Array.isArray(response.scope_warnings)
    ? (response.scope_warnings as unknown[]).filter((w): w is string => typeof w === "string")
    : warnings.map((w) => asString(w.warning) ?? "").filter(Boolean);
  if (warningStrings.length > 0) {
    sections.push(
      `${theme.fg("muted", "Scope warnings:")}\n${warningStrings.map((w) => `  ${w}`).join("\n")}`,
    );
  }
}
async function resolveAstPaths(
  extCtx: ExtensionContext,
  paths: unknown,
): Promise<string[] | undefined> {
  if (isEmptyParam(paths) || !Array.isArray(paths)) return undefined;
  return Promise.all(
    paths
      .filter((path): path is string => typeof path === "string")
      .map((path) => resolvePathArg(extCtx.cwd, path)),
  );
}

async function assertAstPathsPermission(
  extCtx: ExtensionContext,
  paths: string[] | undefined,
  restrictToProjectRoot: boolean,
): Promise<void> {
  if (paths === undefined || paths.length === 0) return;
  const checked = new Set<string>();
  for (const path of paths) {
    if (checked.has(path)) continue;
    checked.add(path);
    await assertExternalDirectoryPermission(extCtx, path, { restrictToProjectRoot });
  }
}

/** Exported for renderer unit tests. */
export function buildAstSearchSections(payload: unknown, theme: Theme): string[] {
  const response = asRecord(payload);
  if (!response) return [theme.fg("muted", "No AST search results.")];

  const matches = asRecords(response.matches);
  const totalMatches = asNumber(response.total_matches) ?? matches.length;
  const filesWithMatches =
    asNumber(response.files_with_matches) ??
    groupByFile(matches, (match) => asString(match.file)).size;
  const filesSearched = asNumber(response.files_searched);
  const header = [
    theme.fg("success", `${totalMatches} match${totalMatches === 1 ? "" : "es"}`),
    theme.fg("accent", `${filesWithMatches} file${filesWithMatches === 1 ? "" : "s"}`),
    filesSearched !== undefined ? theme.fg("muted", `${filesSearched} searched`) : undefined,
  ]
    .filter(Boolean)
    .join(" · ");

  if (matches.length === 0) {
    const sections = [header, theme.fg("muted", "No AST matches found.")];
    appendScopeSections(response, sections, theme);
    appendHintSection(response, sections, theme);
    return sections;
  }

  const grouped = groupByFile(matches, (match) => asString(match.file));
  const sections = [header];
  for (const [file, fileMatches] of grouped.entries()) {
    const lines = [theme.fg("accent", shortenPath(file))];
    fileMatches.forEach((match, index) => {
      const line = asNumber(match.line) ?? 0;
      const column = asNumber(match.column) ?? 0;
      const snippet = asString(match.text)?.trim() || "(empty match)";
      lines.push(`  ${index + 1}. ${theme.fg("muted", `${line}:${column}`)} ${snippet}`);

      const metaVars = asRecord(match.meta_variables);
      if (metaVars && Object.keys(metaVars).length > 0) {
        Object.entries(metaVars).forEach(([name, value]) => {
          lines.push(`     ${theme.fg("muted", `${name} =`)} ${formatValue(value)}`);
        });
      }

      const context = asRecords(match.context);
      context.forEach((ctxLine) => {
        const ctxNumber = asNumber(ctxLine.line) ?? 0;
        const prefix = ctxLine.is_match === true ? theme.fg("accent", ">") : theme.fg("muted", "|");
        lines.push(`     ${prefix} ${ctxNumber}: ${asString(ctxLine.text) ?? ""}`);
      });
    });
    sections.push(lines.join("\n"));
  }

  return sections;
}

/** Exported for renderer unit tests. */
export function buildAstReplaceSections(payload: unknown, theme: Theme): string[] {
  const response = asRecord(payload);
  if (!response) return [theme.fg("muted", "No AST replace results.")];

  const files = asRecords(response.files);
  const totalReplacements = asNumber(response.total_replacements) ?? 0;
  const totalFiles = asNumber(response.total_files) ?? files.length;
  const filesWithMatches = asNumber(response.files_with_matches);
  const dryRun = response.dry_run === true;
  const headerParts = [
    dryRun ? theme.fg("warning", "[dry run]") : theme.fg("success", "[applied]"),
    `${totalReplacements} replacement${totalReplacements === 1 ? "" : "s"}`,
    `${totalFiles} file${totalFiles === 1 ? "" : "s"}`,
    filesWithMatches !== undefined ? theme.fg("muted", `${filesWithMatches} matched`) : undefined,
  ];
  const sections = [headerParts.filter(Boolean).join(" ")];

  if (files.length === 0) {
    sections.push(theme.fg("muted", "No files changed."));
    appendScopeSections(response, sections, theme);
    appendHintSection(response, sections, theme);
    return sections;
  }

  files.forEach((fileResult) => {
    const file = shortenPath(asString(fileResult.file) ?? "(unknown file)");
    const replacements = asNumber(fileResult.replacements) ?? 0;
    const error = asString(fileResult.error);
    const diff = asString(fileResult.diff);
    const lines = [
      `${theme.fg("accent", file)} ${theme.fg("muted", `(${replacements} replacement${replacements === 1 ? "" : "s"})`)}`,
    ];

    if (error) {
      lines.push(theme.fg("error", error));
    } else if (diff) {
      const rendered = renderUnifiedDiff(diff);
      lines.push(rendered || theme.fg("muted", "No diff available."));
    } else {
      const backupId = asString(fileResult.backup_id);
      lines.push(
        backupId
          ? `${theme.fg("success", "saved")} ${theme.fg("muted", backupId)}`
          : theme.fg("success", "saved"),
      );
    }

    sections.push(lines.join("\n"));
  });

  return sections;
}

/** Exported for renderer unit tests. */
export function renderAstCall(
  toolName: "ast_grep_search" | "ast_grep_replace",
  args: Static<typeof SearchParams> | Static<typeof ReplaceParams>,
  theme: Theme,
  context: RenderContextLike,
) {
  const lang = theme.fg("accent", args.lang);
  const summary =
    toolName === "ast_grep_replace"
      ? `${lang} ${theme.fg("toolOutput", `${(args as Static<typeof ReplaceParams>).pattern} → ${(args as Static<typeof ReplaceParams>).rewrite}`)}`
      : `${lang} ${theme.fg("toolOutput", args.pattern)}`;
  return renderToolCall(
    toolName === "ast_grep_replace" ? "ast replace" : "ast search",
    summary,
    theme,
    context,
  );
}

/** Exported for renderer unit tests. */
export function renderAstResult(
  toolName: "ast_grep_search" | "ast_grep_replace",
  result: AgentToolResult<unknown>,
  theme: Theme,
  context: RenderContextLike,
  options: RenderResultOptionsLike = { expanded: true },
) {
  if (context.isError) {
    return renderErrorResult(result, `${toolName} failed`, theme, context);
  }

  const payload = extractStructuredPayload(result);
  if (!payload) {
    const text = collectTextContent(result) || theme.fg("muted", "No result.");
    return collapsibleResult({
      summary: text,
      full: renderSections([text], context),
      expanded: options.expanded,
      context,
    });
  }

  const sections =
    toolName === "ast_grep_replace"
      ? buildAstReplaceSections(payload, theme)
      : buildAstSearchSections(payload, theme);
  return collapsibleResult({
    summary: sections[0] ?? `${toolName} completed`,
    full: renderSections(sections, context),
    expanded: options.expanded,
    context,
  });
}

export function registerAstTools(pi: ExtensionAPI, ctx: PluginContext, surface: AstSurface): void {
  if (surface.astSearch) {
    pi.registerTool({
      name: "ast_grep_search",
      label: "ast search",
      description:
        "Search code patterns across the filesystem using AST-aware matching. Use `$VAR` to match a single AST node, `$$$` for multiple. Pattern must be a complete, valid code fragment (include braces, params, etc.).",
      parameters: SearchParams,
      async execute(
        _toolCallId: string,
        params: Static<typeof SearchParams>,
        _signal,
        onUpdate,
        extCtx,
      ) {
        const paths = await resolveAstPaths(extCtx, params.paths);
        await assertAstPathsPermission(extCtx, paths, ctx.config.restrict_to_project_root ?? false);

        const bridge = bridgeFor(ctx, extCtx.cwd);
        const rawArgs: Record<string, unknown> = {
          pattern: params.pattern,
          lang: params.lang,
        };
        // Use isEmptyParam so empty arrays sent by GPT-family models don't
        // get forwarded to Rust as "scope present" — let Rust default to whole
        // project_root instead of round-tripping a useless empty scope.
        if (!isEmptyParam(paths)) rawArgs.paths = paths;
        if (!isEmptyParam(params.globs)) rawArgs.globs = params.globs;
        if (params.contextLines !== undefined) rawArgs.contextLines = params.contextLines;
        const response = await callToolCall(bridge, "ast_search", rawArgs, extCtx, { onUpdate });
        if (response.success === false) {
          throw new Error(response.text || response.message || "ast_search failed");
        }
        return textResult(response.text, response);
      },
      renderCall(args, theme, context) {
        return renderAstCall("ast_grep_search", args, theme, context);
      },
      renderResult(result, options = { expanded: false, isPartial: false }, theme, context) {
        return renderAstResult("ast_grep_search", result, theme, context, options);
      },
    });
  }

  if (surface.astReplace) {
    pi.registerTool({
      name: "ast_grep_replace",
      label: "ast replace",
      description:
        "Replace code patterns across the filesystem with AST-aware rewriting. Applies by default — pass `dryRun: true` to preview. Use meta-variables in `rewrite` to preserve captured content from the pattern.",
      parameters: ReplaceParams,
      async execute(
        _toolCallId: string,
        params: Static<typeof ReplaceParams>,
        _signal,
        onUpdate,
        extCtx,
      ) {
        const paths = await resolveAstPaths(extCtx, params.paths);
        await assertAstPathsPermission(extCtx, paths, ctx.config.restrict_to_project_root ?? false);

        const bridge = bridgeFor(ctx, extCtx.cwd);
        const rawArgs: Record<string, unknown> = {
          pattern: params.pattern,
          rewrite: params.rewrite,
          lang: params.lang,
        };
        // Use isEmptyParam — see ast_search above for rationale.
        if (!isEmptyParam(paths)) rawArgs.paths = paths;
        if (!isEmptyParam(params.globs)) rawArgs.globs = params.globs;
        // Coerce at the boundary: dryRun "true" must stay preview-only (coerceBoolean).
        rawArgs.dryRun = coerceBoolean(params.dryRun);
        const response = await callToolCall(bridge, "ast_replace", rawArgs, extCtx, { onUpdate });
        if (response.success === false) {
          throw new Error(response.text || response.message || "ast_replace failed");
        }
        return textResult(response.text, response);
      },
      renderCall(args, theme, context) {
        return renderAstCall("ast_grep_replace", args, theme, context);
      },
      renderResult(result, options = { expanded: false, isPartial: false }, theme, context) {
        return renderAstResult("ast_grep_replace", result, theme, context, options);
      },
    });
  }
}
