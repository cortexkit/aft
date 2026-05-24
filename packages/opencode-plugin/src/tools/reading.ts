import { resolve } from "node:path";
import { fetchUrlToTempFile, formatZoomText } from "@cortexkit/aft-bridge";
import type { ToolDefinition } from "@opencode-ai/plugin";
import { tool } from "@opencode-ai/plugin";
import type { PluginContext } from "../types.js";
import { callBridge } from "./_shared.js";

const z = tool.schema;

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
  return {
    aft_outline: {
      description:
        "Structural outline of source code, documentation files, or remote URLs. For code, returns symbols (functions, classes, types) with line ranges. For Markdown and HTML, returns heading hierarchy. Use this to explore structure before reading specific sections with aft_zoom.\n\n" +
        "Pass a single `target`:\n" +
        "  • file path → outline that file (with signatures)\n" +
        "  • directory path → outline all source files under it (recursively, up to 200 files)\n" +
        "  • URL (http:// or https://) → fetch and outline a remote HTML/Markdown document\n" +
        "  • array of paths → outline multiple files in one call",
      args: {
        target: z
          .union([z.string(), z.array(z.string())])
          .describe(
            "What to outline: a file path, directory path, URL, or array of file paths. The mode is auto-detected: URLs by `http://`/`https://` prefix, directories by stat, arrays as multi-file.",
          ),
      },
      execute: async (args, context): Promise<string> => {
        const target = args.target;
        const hasUrl =
          typeof target === "string" &&
          (target.startsWith("http://") || target.startsWith("https://"));
        const isArray = Array.isArray(target) && target.length > 0;

        // URL mode: fetch to temp file, then outline the cached copy
        if (hasUrl) {
          const cachedPath = await fetchUrlToTempFile(target as string, ctx.storageDir, {
            allowPrivate: ctx.config.url_fetch_allow_private === true,
          });
          const response = await callBridge(ctx, context, "outline", { file: cachedPath });
          if (response.success === false) {
            throw new Error((response.message as string) || "outline failed");
          }
          return response.text as string;
        }

        // Multi-file mode
        if (isArray) {
          const response = await callBridge(ctx, context, "outline", {
            files: target as string[],
          });
          if (response.success === false) {
            throw new Error((response.message as string) || "outline failed");
          }
          return formatOutlineText(response);
        }

        // String mode: stat to disambiguate file vs directory
        if (typeof target !== "string" || target.length === 0) {
          throw new Error("'target' must be a non-empty string or array of strings");
        }

        let isDirectory = false;
        try {
          const { stat } = await import("node:fs/promises");
          const resolved = resolve(context.directory, target);
          const st = await stat(resolved);
          isDirectory = st.isDirectory();
        } catch {
          // Path doesn't exist locally — fall through to single-file mode and
          // let Rust report the real error with its preferred shape.
        }

        if (isDirectory) {
          const dirPath = resolve(context.directory, target);
          const response = await callBridge(ctx, context, "outline", { directory: dirPath });
          if (response.success === false) {
            throw new Error((response.message as string) || "outline failed");
          }
          return JSON.stringify(response, null, 2);
        }

        const response = await callBridge(ctx, context, "outline", { file: target });
        if (response.success === false) {
          throw new Error((response.message as string) || "outline failed");
        }
        return formatOutlineText(response);
      },
    },

    aft_zoom: {
      description:
        "Inspect code symbols or documentation sections. For code, returns the full source of a symbol with call-graph annotations (what it calls and what calls it). For Markdown and HTML, returns the section content under the given heading.\n\n" +
        "Provide exactly ONE of 'filePath' or 'url'. Pass either 'symbol' for a single lookup or 'symbols' for multiple in one call.",
      args: {
        filePath: z
          .string()
          .optional()
          .describe("Path to file (absolute or relative to project root)"),
        url: z
          .string()
          .optional()
          .describe("HTTP/HTTPS URL of an HTML or Markdown document to fetch and zoom into"),
        symbol: z
          .string()
          .optional()
          .describe("Symbol name for code, or heading text for Markdown/HTML"),
        symbols: z
          .array(z.string())
          .optional()
          .describe("Array of symbol names or heading texts for a single batched call"),
        contextLines: z
          .number()
          .optional()
          .describe("Lines of context before/after the symbol (default: 3)"),
      },
      execute: async (args, context): Promise<string> => {
        const hasFilePath = typeof args.filePath === "string" && args.filePath.length > 0;
        const hasUrl = typeof args.url === "string" && args.url.length > 0;

        if (!hasFilePath && !hasUrl) {
          throw new Error("Provide exactly one of 'filePath' or 'url'");
        }
        if (hasFilePath && hasUrl) {
          throw new Error("Provide exactly ONE of 'filePath' or 'url' — not both");
        }

        // URL mode: fetch to temp file, then zoom into the cached copy
        const file = hasUrl
          ? await fetchUrlToTempFile(args.url as string, ctx.storageDir, {
              allowPrivate: ctx.config.url_fetch_allow_private === true,
            })
          : (args.filePath as string);

        // Header label — what the agent typed, not the on-disk cache path.
        const targetLabel = (hasUrl ? (args.url as string) : (args.filePath as string)) ?? file;

        // Multi-symbol mode: make separate zoom calls in parallel and combine results
        if (Array.isArray(args.symbols) && args.symbols.length > 0) {
          const results = await Promise.all(
            (args.symbols as string[]).map((sym) => {
              const params: Record<string, unknown> = { file, symbol: sym };
              if (args.contextLines !== undefined) params.context_lines = args.contextLines;
              return callBridge(ctx, context, "zoom", params).catch((err) => ({
                success: false,
                message: err instanceof Error ? err.message : String(err),
              }));
            }),
          );
          return formatZoomBatchResult(targetLabel, args.symbols as string[], results).text;
        }

        // Single symbol mode
        const params: Record<string, unknown> = { file };
        if (typeof args.symbol === "string") params.symbol = args.symbol;
        if (args.contextLines !== undefined) params.context_lines = args.contextLines;

        const data = await callBridge(ctx, context, "zoom", params);
        if (data.success === false) {
          throw new Error((data.message as string) || "zoom failed");
        }
        return formatZoomText(targetLabel, data);
      },
    },
  };
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

/**
 * Format an outline response into agent-readable text, appending honest skip
 * reporting when files were intentionally skipped (parse error, unsupported
 * language, file not found, too large). Without this, agents only see the tree
 * and assume all input files were processed.
 */
interface SkippedOutlineFile {
  file: string;
  reason: string;
}

function formatOutlineText(response: Record<string, unknown>): string {
  const text = (response.text as string | undefined) ?? "";
  const skipped = response.skipped_files as SkippedOutlineFile[] | undefined;
  if (!skipped || skipped.length === 0) {
    return text;
  }
  const lines = skipped.map(({ file, reason }) => `  ${file} — ${reason}`).join("\n");
  const header = text.length > 0 ? `${text}\n\n` : "";
  return `${header}Skipped ${skipped.length} file(s):\n${lines}`;
}
