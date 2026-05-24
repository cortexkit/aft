/**
 * Tool definitions for AST pattern search and replace using ast-grep.
 * Supports meta-variables ($VAR for single node, $$$ for multiple nodes).
 * Patterns must be complete AST nodes (valid code fragments).
 */

import { tool } from "@opencode-ai/plugin";

const z = tool.schema;

import type { ToolDefinition } from "@opencode-ai/plugin";
import type { PluginContext } from "../types.js";
import { callBridge } from "./_shared.js";
import {
  askEditPermission,
  assertExternalDirectoryPermission,
  permissionDeniedResponse,
  resolveAbsolutePath,
  resolveRelativePatterns,
  workspacePattern,
} from "./permissions.js";

/** Show output in opencode UI via metadata callback. */
function showOutputToUser(context: unknown, output: string): void {
  const ctx = context as {
    metadata?: (input: { metadata: { output: string } }) => void | Promise<void>;
  };
  ctx.metadata?.({ metadata: { output } });
}

/**
 * Pull the server-side `hint` field out of an ast-grep response. Rust now
 * attaches a hint to zero-match responses when the pattern looks like a
 * common mistake (regex syntax, language-specific shape, today's Rust
 * match-arm `|` trap). See `crates/aft/src/ast_grep_hints.rs` for the rules.
 *
 * Plugin-side rendering is intentionally thin — all detection logic lives in
 * Rust so OpenCode and Pi behave identically.
 */
function extractHint(response: Record<string, unknown>): string | null {
  const hint = response.hint;
  return typeof hint === "string" && hint.length > 0 ? hint : null;
}

async function checkAstPathsPermission(
  context: Parameters<ToolDefinition["execute"]>[1],
  paths: unknown,
): Promise<string | undefined> {
  if (!Array.isArray(paths)) return undefined;
  const uniquePaths = Array.from(
    new Set(paths.filter((p): p is string => typeof p === "string" && p.length > 0)),
  );
  for (const p of uniquePaths) {
    const denial = await assertExternalDirectoryPermission(context, p, { kind: "directory" });
    if (denial) return denial;
  }
  return undefined;
}

const SUPPORTED_LANGS = ["typescript", "tsx", "javascript", "python", "rust", "go"] as const;

export function astTools(ctx: PluginContext): Record<string, ToolDefinition> {
  const searchTool: ToolDefinition = {
    description:
      "Search code patterns across filesystem using AST-aware matching. Supports 6 languages.\n\n" +
      "Use meta-variables: $VAR matches a single AST node, $$$ matches multiple nodes (variadic).\n" +
      "IMPORTANT: Patterns must be complete AST nodes (valid code fragments).\n" +
      "For functions, include params and body: 'export async function $NAME($$$) { $$$ }' not just 'export async function $NAME'.\n\n" +
      "Examples: pattern='console.log($MSG)' lang='typescript', pattern='async function $NAME($$$) { $$$ }' lang='javascript', pattern='def $FUNC($$$): $$$' lang='python'\n\n" +
      "Returns: Text summary — 'Found N match(es) across M file(s)' followed by file:line blocks with matched text and captured meta-variables.",
    args: {
      pattern: z
        .string()
        .describe("AST pattern with meta-variables ($VAR, $$$). Must be complete AST node."),
      lang: z.enum(SUPPORTED_LANGS).describe("Target language"),
      paths: z.array(z.string()).optional().describe("Paths to search (default: ['.'])"),
      globs: z.array(z.string()).optional().describe("Include/exclude globs (prefix ! to exclude)"),
      contextLines: z
        .number()
        .optional()
        .describe("Number of context lines to show around each match"),
    },
    execute: async (args, context): Promise<string> => {
      const externalDenied = await checkAstPathsPermission(context, args.paths);
      if (externalDenied) return permissionDeniedResponse(externalDenied);

      const params: Record<string, unknown> = {
        pattern: args.pattern,
        lang: args.lang,
      };
      if (args.paths) params.paths = args.paths;
      if (args.globs) params.globs = args.globs;
      if (args.contextLines !== undefined) params.context = Number(args.contextLines);
      const response = await callBridge(ctx, context, "ast_search", params);

      // Error response (e.g. invalid pattern)
      if (response.success === false) {
        throw new Error((response.message as string) || "ast_search failed");
      }

      // Format output for readability
      const data = response as {
        ok?: boolean;
        matches?: Array<{
          file?: string;
          line?: number;
          text?: string;
          meta_variables?: Record<string, string>;
        }>;
        total_matches?: number;
        files_with_matches?: number;
        files_searched?: number;
        no_files_matched_scope?: boolean;
        scope_warnings?: string[];
      };

      const matchCount = data.total_matches ?? data.matches?.length ?? 0;
      const filesSearched = data.files_searched ?? 0;
      const filesWithMatches = data.files_with_matches ?? filesSearched;

      let output: string;
      if (data.no_files_matched_scope) {
        // Scope (paths/globs) was syntactically valid but matched zero files — say so
        // explicitly so agents don't read this as "I searched everywhere and found nothing."
        output = "No files matched the scope (paths/globs resolved to zero files)";
        if (data.scope_warnings && data.scope_warnings.length > 0) {
          output += `\n\nScope warnings:\n${data.scope_warnings.map((w) => `  ${w}`).join("\n")}`;
        }
      } else if (matchCount === 0) {
        // Zero-match format is intentionally not documented in the description — it's
        // self-explanatory text and documenting it would bloat the Returns section.
        output = `No matches found (searched ${filesSearched} files)`;
        if (data.scope_warnings && data.scope_warnings.length > 0) {
          output += `\n\nScope warnings:\n${data.scope_warnings.map((w) => `  ${w}`).join("\n")}`;
        }
        // Server-side hint for common pattern mistakes (attached by Rust
        // when the pattern looks like regex syntax, language-specific shape
        // mistake, or today's Rust match-arm `|` trap).
        const hint = extractHint(response as Record<string, unknown>);
        if (hint) {
          output += `\n\n${hint}`;
        }
      } else {
        output = `Found ${matchCount} match(es) in ${filesWithMatches} file(s) (${filesSearched} searched)\n\n`;
        if (data.matches) {
          for (const m of data.matches) {
            const relFile = m.file ?? "unknown";
            const line = m.line ?? 0;
            output += `${relFile}:${line}\n`;
            if (m.text) {
              output += `  ${m.text.trim()}\n`;
            }
            if (m.meta_variables && Object.keys(m.meta_variables).length > 0) {
              for (const [k, v] of Object.entries(m.meta_variables)) {
                output += `  ${k}: ${v}\n`;
              }
            }
            output += "\n";
          }
        }
      }

      // Show output in UI
      showOutputToUser(context, output);
      return output;
    },
  };

  const replaceTool: ToolDefinition = {
    description:
      "Replace code patterns across filesystem with AST-aware rewriting. Applies changes by default — set dryRun=true to preview.\n\n" +
      "Use meta-variables in the rewrite pattern to preserve matched content from the pattern.\n" +
      "IMPORTANT: Patterns must be complete AST nodes (valid code fragments).\n\n" +
      "Example: pattern='console.log($MSG)' rewrite='logger.info($MSG)' lang='typescript' — replaces all console.log calls with logger.info across TypeScript files.\n\n" +
      "**Warning: This tool modifies files directly.** Use dryRun=true to preview. Consider creating an aft_safety checkpoint before bulk replacements.\n\n" +
      "Returns: Text summary — 'Replaced N match(es) across M file(s)' (or '[DRY RUN] Would replace...') followed by per-file output. In dry-run mode, each file is shown with its unified diff so you can verify the rewrite before applying (e.g. catch literal $$$ from anonymous-variadic typos). Diff preview is capped at 8KB total; remaining files are summarized.",
    args: {
      pattern: z
        .string()
        .describe("AST pattern with meta-variables ($VAR, $$$). Must be complete AST node."),
      rewrite: z.string().describe("Replacement pattern (can use $VAR from pattern)"),
      lang: z.enum(SUPPORTED_LANGS).describe("Target language"),
      paths: z.array(z.string()).optional().describe("Paths to search (default: ['.'])"),
      globs: z.array(z.string()).optional().describe("Include/exclude globs (prefix ! to exclude)"),
      dryRun: z.boolean().optional().describe("Preview changes without applying (default: false)"),
    },
    execute: async (args, context): Promise<string> => {
      const isDryRun = args.dryRun === true;

      const externalDenied = await checkAstPathsPermission(context, args.paths);
      if (externalDenied) return permissionDeniedResponse(externalDenied);

      if (!isDryRun) {
        const paths = Array.isArray(args.paths) ? (args.paths as string[]) : ["."];
        // External-directory check first (mirrors opencode-native grep/glob directory checks).
        if (!Array.isArray(args.paths)) {
          const asked = new Set<string>();
          for (const targetPath of paths) {
            const absPath = resolveAbsolutePath(context, targetPath);
            if (asked.has(absPath)) continue;
            asked.add(absPath);
            const denial = await assertExternalDirectoryPermission(context, absPath, {
              kind: "directory",
            });
            if (denial) return permissionDeniedResponse(denial);
          }
        }

        const explicitPaths = Array.isArray(args.paths)
          ? resolveRelativePatterns(context, args.paths as string[])
          : [];
        const positiveGlobs = Array.isArray(args.globs)
          ? (args.globs as string[]).filter((glob) => !glob.startsWith("!"))
          : [];
        const patterns = [...explicitPaths, ...positiveGlobs];
        const metadata =
          explicitPaths.length === 1 && positiveGlobs.length === 0 && Array.isArray(args.paths)
            ? { filepath: resolveAbsolutePath(context, (args.paths as string[])[0] as string) }
            : {};
        const permissionError = await askEditPermission(
          context,
          patterns.length > 0 ? patterns : [workspacePattern(context)],
          metadata,
        );
        if (permissionError) {
          return permissionDeniedResponse(permissionError);
        }
      }

      const params: Record<string, unknown> = {
        pattern: args.pattern,
        rewrite: args.rewrite,
        lang: args.lang,
      };
      if (args.paths) params.paths = args.paths;
      if (args.globs) params.globs = args.globs;
      params.dry_run = args.dryRun === true;
      const response = await callBridge(ctx, context, "ast_replace", params);

      // Error response (e.g. invalid pattern)
      if (response.success === false) {
        throw new Error((response.message as string) || "ast_replace failed");
      }

      const data = response as {
        ok?: boolean;
        // Apply-mode shape (Rust commands/ast_replace.rs returns these in
        // `matches[]` only on the `ast_search`-shaped path; `ast_replace`
        // itself emits per-file results in `files[]`).
        matches?: Array<{ file?: string; line?: number; text?: string; replacement?: string }>;
        // Per-file results carry a unified diff string in dry-run mode and
        // a write outcome in apply mode. See crates/aft/src/commands/ast_replace.rs.
        files?: Array<{
          file?: string;
          replacements?: number;
          diff?: string; // present in dry-run only
          backup_id?: string; // present in apply mode when snapshot succeeded
          ok?: boolean; // false on per-file write failure
          error?: string;
        }>;
        total_matches?: number;
        total_replacements?: number;
        total_files?: number;
        files_with_matches?: number;
        files_searched?: number;
        no_files_matched_scope?: boolean;
        scope_warnings?: string[];
      };

      const matchCount = data.total_replacements ?? data.total_matches ?? data.matches?.length ?? 0;
      const filesSearched = data.files_searched ?? data.total_files ?? 0;
      const filesWithMatches = data.files_with_matches ?? data.total_files ?? filesSearched;

      let output: string;
      if (data.no_files_matched_scope) {
        output = "No files matched the scope (paths/globs resolved to zero files)";
        if (data.scope_warnings && data.scope_warnings.length > 0) {
          output += `\n\nScope warnings:\n${data.scope_warnings.map((w) => `  ${w}`).join("\n")}`;
        }
      } else if (matchCount === 0) {
        output = `No matches found (searched ${filesSearched} files)`;
        if (data.scope_warnings && data.scope_warnings.length > 0) {
          output += `\n\nScope warnings:\n${data.scope_warnings.map((w) => `  ${w}`).join("\n")}`;
        }
        // Server-side hint when zero replacements happened. Especially
        // important here: "0 replacements" looks like a clean no-op but
        // can mean silent corruption (today's `|` bug). The hint tells
        // the agent why the pattern matched nothing.
        const hint = extractHint(response as Record<string, unknown>);
        if (hint) {
          output += `\n\n${hint}`;
        }
      } else {
        output = isDryRun
          ? `[DRY RUN] Would replace ${matchCount} match(es) in ${filesWithMatches} file(s) (${filesSearched} searched)\n\n`
          : `Replaced ${matchCount} match(es) in ${filesWithMatches} file(s) (${filesSearched} searched)\n\n`;

        // Dry-run: render per-file unified diff so the agent can SEE the
        // proposed change (catches anonymous-`$$$` and other rewrite bugs
        // BEFORE applying). The Rust handler caps each diff at a sane size,
        // and we additionally cap total diff bytes here so a 1000-file
        // rewrite doesn't dump a megabyte of diff into the agent's context.
        if (isDryRun && data.files && data.files.length > 0) {
          const MAX_DIFF_BYTES = 8 * 1024; // 8 KB total preview budget
          let used = 0;
          let filesShown = 0;
          for (const f of data.files) {
            const relFile = f.file ?? "unknown";
            const reps = f.replacements ?? 0;
            const diff = f.diff ?? "";
            if (used + diff.length > MAX_DIFF_BYTES) {
              const remaining = data.files.length - filesShown;
              if (remaining > 0) {
                output += `\n... (${remaining} more file(s) omitted from preview to stay under ${MAX_DIFF_BYTES / 1024}KB; total ${matchCount} replacements across ${filesWithMatches} files)\n`;
              }
              break;
            }
            output += `${relFile} (${reps} replacement${reps === 1 ? "" : "s"}):\n`;
            output += diff;
            if (!diff.endsWith("\n")) output += "\n";
            output += "\n";
            used += diff.length;
            filesShown += 1;
          }
        } else if (data.matches) {
          // Apply-mode legacy path: render per-match before/after (this
          // path is reached when the response carries `matches[]` rather
          // than `files[]` — currently the search shape, kept for
          // forward-compat with handler refactors).
          for (const m of data.matches) {
            const relFile = m.file ?? "unknown";
            const line = m.line ?? 0;
            output += `${relFile}:${line}\n`;
            if (m.text && m.replacement) {
              output += `  - ${m.text.trim()}\n`;
              output += `  + ${m.replacement.trim()}\n`;
            }
            output += "\n";
          }
        } else if (data.files && data.files.length > 0) {
          // Apply-mode + files[] only: list files with replacement counts
          // (no diff in apply mode — the file is already on disk).
          for (const f of data.files) {
            const relFile = f.file ?? "unknown";
            const reps = f.replacements ?? 0;
            output += `  ${relFile}: ${reps} replacement${reps === 1 ? "" : "s"}\n`;
          }
        }
      }

      showOutputToUser(context, output);
      return output;
    },
  };

  // When hoisting: register as ast_grep_search/ast_grep_replace (override oh-my-opencode's)
  // When not hoisting: register as aft_ast_search/aft_ast_replace
  const hoisting = ctx.config.hoist_builtin_tools !== false;
  return {
    [hoisting ? "ast_grep_search" : "aft_ast_search"]: searchTool,
    [hoisting ? "ast_grep_replace" : "aft_ast_replace"]: replaceTool,
  };
}
