import { coerceBoolean, coerceTargetParam, formatZoomText } from "@cortexkit/aft-bridge";
import type { ToolContext, ToolDefinition, ToolResult } from "@opencode-ai/plugin";
import { tool } from "@opencode-ai/plugin";
import { toolEnabled } from "../config.js";
import { prepareToolMap } from "../normalize-schemas.js";
import type { PluginContext } from "../types.js";
import {
  callToolCall,
  coerceOptionalInt,
  isEmptyParam,
  optionalInt,
  resolvePathArg,
} from "./_shared.js";
import { whenGhReadEnabled } from "./hoisted.js";
import { assertExternalDirectoryPermission, permissionDeniedResponse } from "./permissions.js";

const z = tool.schema;

function buildZoomTitle(args: {
  path?: string;
  url?: string;
  symbols?: string | string[];
  targets?: { path: string; symbol: string } | Array<{ path: string; symbol: string }>;
}): string {
  const targets = args.targets;
  if (!isEmptyParam(targets)) {
    if (Array.isArray(targets)) {
      if (targets.length === 1) {
        return `${targets[0].path}#${targets[0].symbol}`;
      }
      return `${targets.length} targets across files`;
    }
    if (targets && typeof targets === "object") {
      return `${targets.path}#${targets.symbol}`;
    }
  }

  const path = args.path ?? args.url ?? "";
  if (typeof args.symbols === "string") return path ? `${path}#${args.symbols}` : args.symbols;
  if (Array.isArray(args.symbols) && args.symbols.length > 0) {
    if (args.symbols.length === 1) return path ? `${path}#${args.symbols[0]}` : args.symbols[0];
    return path ? `${path} (${args.symbols.length} symbols)` : `${args.symbols.length} symbols`;
  }
  return path || "(no target)";
}

interface ZoomBatchSymbolResult {
  name: string;
  success: boolean;
  content?: string;
  error?: string;
}

interface ZoomBatchResult {
  complete: boolean;
  symbols: ZoomBatchSymbolResult[];
  text: string;
}

/**
 * Tool definitions for code reading commands: outline + zoom.
 */
export function readingTools(ctx: PluginContext): Record<string, ToolDefinition> {
  const zoomEnabled = toolEnabled(ctx.config, "aft_zoom");
  const ghReadEnabled = ctx.config.gh_read?.enabled === true;
  const githubOutlineDescription = whenGhReadEnabled(
    ghReadEnabled,
    "GitHub issues and pull requests can be outlined with `issue://NUMBER` or `pr://NUMBER` (including `OWNER/REPO` forms).",
  );
  const githubZoomDescription = whenGhReadEnabled(
    ghReadEnabled,
    "GitHub issue and pull-request discussion ordinals can be zoomed with `path: issue://…` or `path: pr://…` and `symbols`.",
  );
  const tools = prepareToolMap({
    aft_outline: {
      description:
        "Structural outline of source code, documentation files, or remote URLs. For code, returns symbols (functions, classes, types) with line ranges. For Markdown and HTML, returns heading hierarchy. Use this to explore structure before reading specific sections with " +
        (zoomEnabled ? "aft_zoom" : "read") +
        ". With `files: true`, the outline is breadth-first with directory rollups; drill in by outlining a subdirectory. Rows show language, symbol count, and line count.\n\n" +
        (zoomEnabled
          ? "For understanding a specific feature, prefer aft_search + aft_zoom on named symbols; use aft_outline on a whole directory only for high-level structure mapping. aft_zoom with `callgraph:true` gives one-level forward calls-out; use aft_callgraph only for reverse callers or multi-level traces.\n\n"
          : "For understanding a specific feature, prefer aft_search + read on named symbols; use aft_outline on a whole directory only for high-level structure mapping.\n\n") +
        "Pass a single `target`:\n" +
        "  • file path → outline that file (with signatures)\n" +
        "  • directory path → outline source files under it\n" +
        "  • URL (http:// or https://) → fetch and outline a remote HTML/Markdown document\n" +
        "  • array of paths → outline multiple files in one call; with files:true, every path must be a directory" +
        (githubOutlineDescription ? `\n\n${githubOutlineDescription}` : ""),
      args: {
        target: z
          .union([z.string(), z.array(z.string())])
          .describe(
            "What to outline: a file path, directory path, URL, or array of paths. The mode is auto-detected: URLs by `http://`/`https://` prefix, directories by stat, arrays as multi-file.",
          ),
        files: z
          .boolean()
          .optional()
          .describe(
            "Directory-only mode: when true, target must be a directory or array of directories and the result is a breadth-first file tree with directory rollups plus language, symbol, and line counts.",
          ),
        includeTests: z
          .boolean()
          .optional()
          .describe(
            "Directory outline only: include test files. Defaults to false; tests are hidden.",
          ),
      },
      execute: async (args, context): Promise<string> => {
        // Coerce at the boundary: a host may deliver the string|array `target` as
        // a JSON-stringified array, which would otherwise be treated as one
        // literal path (coerceTargetParam). And a stringified "true" must enable
        // files mode (coerceBoolean).
        const target = coerceTargetParam(args.target);
        const filesMode = coerceBoolean(args.files);
        const hasIncludeTests = !isEmptyParam(args.includeTests);
        const includeTests = coerceBoolean(args.includeTests);
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
            target.map((entry) => resolvePathArg(ctx, context, entry)),
          );
          const permissionDenied = await assertPathExternalPermissions(
            ctx,
            context,
            resolvedTargets,
            filesMode ? "directory" : "file",
          );
          if (permissionDenied) return permissionDeniedResponse(permissionDenied);
        } else {
          if (typeof target !== "string" || target.length === 0) {
            throw new Error("'target' must be a non-empty string or array of strings");
          }

          const hasUrl =
            !filesMode &&
            (target.startsWith("http://") ||
              target.startsWith("https://") ||
              target.startsWith("issue://") ||
              target.startsWith("pr://"));
          if (!hasUrl) {
            const resolvedTarget = await resolvePathArg(ctx, context, target);
            const permissionDenied = await assertPathExternalPermissions(
              ctx,
              context,
              resolvedTarget,
              await permissionKindForPath(resolvedTarget),
            );
            if (permissionDenied) return permissionDeniedResponse(permissionDenied);
          }
        }

        const response = await callToolCall(ctx, context, "outline", rawArgs);
        if (response.success === false) {
          throw new Error((response.message as string) || "outline failed");
        }
        return response.text;
      },
    },

    aft_zoom: {
      description:
        "Inspect code symbols or documentation sections. For code, returns the full source of a symbol. Pass `callgraph: true` to also include call-graph annotations (calls-out / called-by within the same file). For Markdown and HTML, returns the section content under the given heading.\n\nUse exactly ONE mode: `{ path, symbols }`, `{ url, symbols }`, or `{ targets }`. `symbols` can be a string or array (one or many lookups in the same file/URL). Use `targets` for cross-file batches: `{ path, symbol }` or an array of them." +
        (githubZoomDescription ? `\n\n${githubZoomDescription}` : ""),
      args: {
        path: z.string().optional().describe("Path to file (absolute or relative to project root)"),
        url: z
          .string()
          .optional()
          .describe("HTTP/HTTPS URL of an HTML or Markdown document to fetch and zoom into"),
        symbols: z
          .union([z.string(), z.array(z.string())])
          .optional()
          .describe(
            "Symbol name for code, or heading text for Markdown/HTML. Pass a string for one lookup or an array for batched lookups in the same file/URL.",
          ),
        targets: z
          .union([
            z.object({
              path: z.string().describe("Path to file (absolute or relative to project root)"),
              symbol: z.string().describe("Symbol name in that file"),
            }),
            z.array(
              z.object({
                path: z.string().describe("Path to file (absolute or relative to project root)"),
                symbol: z.string().describe("Symbol name in that file"),
              }),
            ),
          ])
          .optional()
          .describe(
            "Cross-file batch: `{ path, symbol }` or an array of them. Mutually exclusive with path/url/symbols.",
          ),
        contextLines: optionalInt(1, Number.MAX_SAFE_INTEGER).describe(
          "Lines of context before/after the symbol (default: 3)",
        ),
        callgraph: z
          .boolean()
          .optional()
          .describe(
            "Include call-graph annotations (calls-out / called-by within the same file). Default false; off keeps zoom output minimal.",
          ),
      },
      execute: async (args, context): Promise<ToolResult> => {
        // GPT-family models send empty strings / empty arrays / empty objects
        // instead of omitting optional params. Use `isEmptyParam` so e.g.
        // `targets: []` or `url: ""` don't trigger mutual-exclusion errors
        // against fields the agent didn't actually intend to provide.
        // `targets` also accepts nested object/array shapes. Treat `targets` as
        // omitted only when every entry is empty, such as
        // `[{path: "", symbol: ""}]` or `{path: "", symbol: ""}`.
        // When any target entry contains a non-empty field, treat `targets` as
        // supplied and let per-entry validation report an error such as
        // `targets[0].path must be non-empty`.
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
        const hasFilePath = !isEmptyParam(args.path);
        const hasUrl = !isEmptyParam(args.url);
        const hasTargets = hasTargetsProvided(args.targets);
        const hasSymbols = !isEmptyParam(args.symbols);
        // Coerce at the boundary: stringified "true" must request callgraph (coerceBoolean).
        const wantCallgraph = coerceBoolean(args.callgraph);
        const contextLines = coerceOptionalInt(
          args.contextLines,
          "contextLines",
          1,
          Number.MAX_SAFE_INTEGER,
        );

        // TUI title + scalar metadata for the tool-call header. OpenCode's UI
        // only auto-renders SCALAR args (strings, numbers, booleans) — arrays
        // and objects are dropped from the `[key=value, ...]` line. Stringify
        // collection-shaped args here so `targets`/`symbols` stay visible.
        // Attached to every successful return via `withMeta`. (Error paths
        // can't carry a title: OpenCode skips `tool.execute.after` when execute
        // throws, and the plugin `context.metadata()` callback is unbridged, so
        // the return value is the only channel that survives.)
        const zoomTitle = buildZoomTitle(args);
        const zoomDisplay: Record<string, unknown> = { title: zoomTitle };
        if (hasFilePath) zoomDisplay.path = args.path;
        if (hasUrl) zoomDisplay.url = args.url;
        if (hasSymbols) {
          zoomDisplay.symbols =
            typeof args.symbols === "string" ? args.symbols : JSON.stringify(args.symbols);
        }
        if (hasTargets) zoomDisplay.targets = JSON.stringify(args.targets);
        if (contextLines !== undefined) zoomDisplay.contextLines = contextLines;
        if (wantCallgraph) zoomDisplay.callgraph = true;
        const withMeta = (output: string): ToolResult => ({
          output,
          title: zoomTitle,
          metadata: zoomDisplay,
        });

        // Multi-target mode (cross-file). Mutually exclusive with the other
        // modes so the agent doesn't accidentally provide overlapping inputs
        // that get silently ignored.
        if (hasTargets) {
          if (hasFilePath || hasUrl || hasSymbols) {
            throw new Error("'targets' is mutually exclusive with 'path', 'url', and 'symbols'");
          }
          const targets = Array.isArray(args.targets)
            ? (args.targets as Array<{ path: string; symbol: string }>)
            : ([args.targets] as Array<{ path: string; symbol: string }>);
          if (targets.length === 0) {
            throw new Error("'targets' must be a non-empty object or array");
          }
          for (const [i, entry] of targets.entries()) {
            const targetPath = entry?.path;
            if (typeof targetPath !== "string" || targetPath.length === 0) {
              throw new Error(`targets[${i}].path must be a non-empty string`);
            }
            if (typeof entry.symbol !== "string" || entry.symbol.length === 0) {
              throw new Error(`targets[${i}].symbol must be a non-empty string`);
            }
          }
          const resolvedTargets = await Promise.all(
            targets.map((t) => resolvePathArg(ctx, context, t.path)),
          );
          const permissionDenied = await assertPathExternalPermissions(
            ctx,
            context,
            resolvedTargets,
          );
          if (permissionDenied) return permissionDeniedResponse(permissionDenied);

          const rawArgs: Record<string, unknown> = {
            targets: targets.map((target) => ({
              filePath: target.path,
              symbol: target.symbol,
            })),
          };
          if (contextLines !== undefined) rawArgs.contextLines = contextLines;
          if (wantCallgraph) rawArgs.callgraph = true;

          const response = await callToolCall(ctx, context, "zoom", rawArgs);
          if (response.success === false) {
            throw new Error(response.text || response.message || "zoom failed");
          }
          return withMeta(response.text);
        }

        if (!hasFilePath && !hasUrl) {
          throw new Error("Provide exactly one of 'path', 'url', or 'targets'");
        }
        if (hasFilePath && hasUrl) {
          throw new Error("Provide exactly ONE of 'path' or 'url' — not both");
        }

        // URL mode passes through to Rust; Rust fetches, validates, and caches.
        // File mode still resolves locally before dispatch so external-directory
        // permission checks approve the same path the server will read.
        const githubPath =
          (hasFilePath &&
            (String(args.path).startsWith("issue://") || String(args.path).startsWith("pr://"))) ||
          (hasUrl &&
            (String(args.url).startsWith("issue://") || String(args.url).startsWith("pr://")));
        if (!hasUrl && !githubPath) {
          const file = await resolvePathArg(ctx, context, args.path as string);
          const permissionDenied = await assertPathExternalPermissions(ctx, context, file);
          if (permissionDenied) return permissionDeniedResponse(permissionDenied);
        }

        const rawArgs: Record<string, unknown> = hasUrl
          ? { url: args.url }
          : { filePath: args.path };
        if (hasSymbols) rawArgs.symbols = args.symbols;
        if (contextLines !== undefined) rawArgs.contextLines = contextLines;
        if (wantCallgraph) rawArgs.callgraph = true;

        const response = await callToolCall(ctx, context, "zoom", rawArgs);
        if (response.success === false) {
          throw new Error(response.text || response.message || "zoom failed");
        }
        return withMeta(response.text);
      },
    },
  });
  return tools;
}

/**
 * Format multi-symbol zoom results as plain text. Successful entries use
 * `formatZoomText` (line-numbered, no JSON escapes); failures render as
 * `Symbol "name" not found: <reason>`. Sections are blank-line separated.
 *
 * Exported for regression tests.
 */
export function formatZoomBatchResult(
  targetLabel: string,
  symbols: string[],
  responses: Record<string, unknown>[],
): ZoomBatchResult {
  const entries = symbols.map((name, index): ZoomBatchSymbolResult => {
    const response = responses[index] ?? { success: false, message: "missing zoom response" };
    if (response.success === false) {
      const message =
        typeof response.message === "string" && response.message.length > 0
          ? response.message
          : "zoom failed";
      return { name, success: false, error: message };
    }
    return { name, success: true, content: formatZoomText(targetLabel, response) };
  });
  const complete = entries.every((entry) => entry.success);
  const sections: string[] = [];
  if (!complete) {
    sections.push("Incomplete zoom results: one or more symbols failed.");
  }
  for (const entry of entries) {
    if (entry.success) {
      sections.push(entry.content ?? "");
    } else {
      sections.push(`Symbol "${entry.name}" not found: ${entry.error ?? "zoom failed"}`);
    }
  }
  return { complete, symbols: entries, text: sections.join("\n\n") };
}

async function permissionKindForPath(resolvedPath: string): Promise<"file" | "directory"> {
  try {
    const { stat } = await import("node:fs/promises");
    const st = await stat(resolvedPath);
    return st.isDirectory() ? "directory" : "file";
  } catch {
    // If stat fails, keep the tool call moving so the server can report the
    // real path error. Use a file label because a missing path cannot be a
    // directory that the permission prompt could grant.
    return "file";
  }
}

async function assertPathExternalPermissions(
  ctx: PluginContext,
  context: ToolContext,
  target: string | string[],
  kind: "file" | "directory" = "file",
): Promise<string | undefined> {
  const targets = Array.isArray(target) ? target : [target];
  const checked = new Set<string>();

  for (const resolvedPath of targets) {
    if (typeof resolvedPath !== "string" || resolvedPath.length === 0) continue;
    const key = `${kind}:${resolvedPath}`;
    if (checked.has(key)) continue;
    checked.add(key);

    const denial = await assertExternalDirectoryPermission(ctx, context, resolvedPath, { kind });
    if (denial) return denial;
  }

  return undefined;
}
