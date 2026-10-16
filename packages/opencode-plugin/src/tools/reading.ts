import { coerceBoolean, coerceTargetParam, formatZoomText } from "@cortexkit/aft-bridge";
import type { ToolContext, ToolDefinition, ToolResult } from "@opencode-ai/plugin";
import { tool } from "@opencode-ai/plugin";
import { resolveGithubConfig, toolEnabled } from "../config.js";
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

/** GitHub discussion targets are fetched by the server, not read from disk. */
function isGithubPathArg(value: unknown): boolean {
  return typeof value === "string" && (value.startsWith("issue://") || value.startsWith("pr://"));
}

function isUrl(value: string): boolean {
  return value.startsWith("http://") || value.startsWith("https://");
}

function buildZoomTitle(args: {
  batch: Array<{ target: string; symbols: string | string[] }>;
}): string {
  if (args.batch.length !== 1) return `${args.batch.length} locations`;
  const entry = args.batch[0];
  return `${entry.target}#${Array.isArray(entry.symbols) ? entry.symbols.join(", ") : entry.symbols}`;
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
/**
 * The outline description's steer toward focused reading. It names
 * `aft_search`, `aft_zoom` and `aft_callgraph` only when each is registered,
 * falling back to `read` for symbol reading.
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

export function readingTools(ctx: PluginContext): Record<string, ToolDefinition> {
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
  const tools = prepareToolMap({
    aft_outline: {
      description:
        "Structural outline of source code, documentation files, or remote URLs. For code, returns symbols (functions, classes, types) with line ranges. For Markdown and HTML, returns heading hierarchy. Use this to explore structure before reading specific sections with " +
        (zoomEnabled ? "aft_zoom" : "read") +
        ". With `files: true`, the outline is breadth-first with directory rollups; drill in by outlining a subdirectory. Rows show language, symbol count, and line count.\n\n" +
        outlineFocusSteer(ctx.config) +
        "\n\n" +
        "Pass a single `target`:\n" +
        "  • file path → outline that file (with signatures)\n" +
        "  • directory path → outline source files under it\n" +
        "  • URL (http:// or https://) → fetch and outline a remote HTML/Markdown document\n" +
        "  • array of paths → outline multiple files in one call; with files:true, every path must be a directory" +
        (githubOutlineDescription ? `\n\n${githubOutlineDescription}` : "") +
        "\n\nWhen a list is cut, the reply ends with `shown N of M <unit> (<reason>) · narrow: <knobs>`; absence of that line means the list is complete.",
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
        'Inspect code symbols or documentation sections. Use `batch: [{ target, symbols }]` for a single lookup. Mix files and URLs in one batch, for example `batch: [{ target: "src/app.ts", symbols: "run" }, { target: "https://example.com/docs", symbols: "Overview" }]`. Each target is auto-detected as a file path or an HTTP/HTTPS URL. `symbols` is a string or string array. Markdown and HTML lookups return the section under the heading. Top-level `contextLines` and `callgraph` apply to every entry; callgraph adds calls-out and called-by annotations. Each failed lookup reports its own error line.' +
        (githubZoomDescription ? `\n\n${githubZoomDescription}` : ""),
      args: {
        batch: z
          .array(
            z.object({
              target: z
                .string()
                .describe(
                  "File path (absolute or project-relative) or HTTP/HTTPS URL, auto-detected; issue://N or pr://N when GitHub reads are enabled",
                ),
              symbols: z
                .union([z.string(), z.array(z.string())])
                .describe(
                  "Symbol name for code or heading text for Markdown/HTML; a string or array of strings",
                ),
            }),
          )
          .min(1)
          .describe("One or more lookups; entries may mix file paths and URLs"),
        contextLines: optionalInt(1, Number.MAX_SAFE_INTEGER).describe(
          "Lines of context before/after each symbol (default: 3)",
        ),
        callgraph: z
          .boolean()
          .optional()
          .describe("Include call-graph annotations for every entry (default: false)"),
      },
      execute: async (args, context): Promise<ToolResult> => {
        if (!Array.isArray(args.batch) || args.batch.length === 0)
          throw new Error("'batch' must be a non-empty array of {target, symbols}");
        // Remote targets are fetched by Rust; only filesystem locations need
        // external-directory approval before dispatch.
        const paths = args.batch
          .map((entry) => entry.target)
          .filter((target) => target && !isGithubPathArg(target) && !isUrl(target));
        const resolved = await Promise.all(
          paths.map((target) => resolvePathArg(ctx, context, target)),
        );
        const permissionDenied = await assertPathExternalPermissions(ctx, context, resolved);
        if (permissionDenied) return permissionDeniedResponse(permissionDenied);
        const rawArgs: Record<string, unknown> = { batch: args.batch };
        const contextLines = coerceOptionalInt(
          args.contextLines,
          "contextLines",
          1,
          Number.MAX_SAFE_INTEGER,
        );
        if (contextLines !== undefined) rawArgs.contextLines = contextLines;
        if (coerceBoolean(args.callgraph)) rawArgs.callgraph = true;
        const response = await callToolCall(ctx, context, "zoom", rawArgs);
        if (response.success === false)
          throw new Error(response.text || response.message || "zoom failed");
        const title = buildZoomTitle({ batch: args.batch });
        return {
          output: response.text,
          title,
          metadata: { title, ...rawArgs, batch: JSON.stringify(args.batch) },
        };
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
