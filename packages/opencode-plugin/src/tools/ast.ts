/**
 * Tool definitions for AST pattern search and replace using ast-grep.
 * Supports meta-variables ($VAR for single node, $$$ for multiple nodes).
 * Patterns must be complete AST nodes (valid code fragments).
 */

import { tool } from "@opencode-ai/plugin";

const z = tool.schema;

import { coerceBoolean } from "@cortexkit/aft-bridge";
import type { ToolDefinition } from "@opencode-ai/plugin";
import type { PluginContext } from "../types.js";
import { callToolCall, isEmptyParam, optionalInt, resolvePathArg } from "./_shared.js";
import {
  askEditPermission,
  assertExternalDirectoryPermission,
  permissionDeniedResponse,
  resolveRelativePatterns,
  workspacePattern,
} from "./permissions.js";

async function resolveAstPaths(
  ctx: PluginContext,
  context: Parameters<ToolDefinition["execute"]>[1],
  paths: unknown,
): Promise<string[] | undefined> {
  if (isEmptyParam(paths) || !Array.isArray(paths)) return undefined;
  const resolved = await Promise.all(
    paths
      .filter((p): p is string => typeof p === "string" && p.length > 0)
      .map((p) => resolvePathArg(ctx, context, p)),
  );
  return resolved.length > 0 ? resolved : undefined;
}

async function checkAstPathsPermission(
  ctx: PluginContext,
  context: Parameters<ToolDefinition["execute"]>[1],
  paths: string[] | undefined,
): Promise<string | undefined> {
  if (paths === undefined) return undefined;
  const uniquePaths = Array.from(new Set(paths));
  for (const p of uniquePaths) {
    const denial = await assertExternalDirectoryPermission(ctx, context, p, { kind: "directory" });
    if (denial) return denial;
  }
  return undefined;
}

const SUPPORTED_LANGS = [
  "typescript",
  "tsx",
  "javascript",
  "python",
  "rust",
  "go",
  "cuda",
  "metal",
  "pascal",
  "r",
  "objc",
] as const;

export function astTools(ctx: PluginContext): Record<string, ToolDefinition> {
  const searchTool: ToolDefinition = {
    description:
      "Search code patterns across filesystem using AST-aware matching. Supports 11 languages.\n\n" +
      "Use meta-variables: $VAR matches a single AST node, $$$ matches multiple nodes (variadic).\n" +
      "IMPORTANT: Patterns must be complete AST nodes (valid code fragments).\n" +
      "For functions, include params and body: 'export async function $NAME($$$) { $$$ }' not just 'export async function $NAME'.\n\n" +
      "Examples: pattern='console.log($MSG)' lang='typescript', pattern='async function $NAME($$$) { $$$ }' lang='javascript', pattern='def $FUNC($$$): $$$' lang='python', pattern='Writeln($MSG);' lang='pascal', pattern='$X <- $Y' lang='r', pattern='[obj foo:$ARG]' lang='objc'",
    args: {
      pattern: z
        .string()
        .describe("AST pattern with meta-variables ($VAR, $$$). Must be complete AST node."),
      lang: z.enum(SUPPORTED_LANGS).describe("Target language"),
      paths: z.array(z.string()).optional().describe("Paths to search (default: ['.'])"),
      globs: z.array(z.string()).optional().describe("Include/exclude globs (prefix ! to exclude)"),
      contextLines: optionalInt(1, Number.MAX_SAFE_INTEGER).describe(
        "Number of context lines to show around each match",
      ),
    },
    execute: async (args, context): Promise<string> => {
      const paths = await resolveAstPaths(ctx, context, args.paths);
      const externalDenied = await checkAstPathsPermission(ctx, context, paths);
      if (externalDenied) return permissionDeniedResponse(externalDenied);

      const rawArgs: Record<string, unknown> = {
        pattern: args.pattern,
        lang: args.lang,
      };
      if (paths !== undefined) rawArgs.paths = paths;
      if (args.globs !== undefined) rawArgs.globs = args.globs;
      if (args.contextLines !== undefined) rawArgs.contextLines = args.contextLines;
      const response = await callToolCall(ctx, context, "ast_search", rawArgs);

      // Error response (e.g. invalid pattern)
      if (response.success === false) {
        throw new Error((response.message as string) || "ast_search failed");
      }

      return response.text;
    },
  };

  const replaceTool: ToolDefinition = {
    description:
      "Replace code patterns across filesystem with AST-aware rewriting. Applies changes by default — set dryRun=true to preview.\n\n" +
      "Use meta-variables in the rewrite pattern to preserve matched content from the pattern.\n" +
      "IMPORTANT: Patterns must be complete AST nodes (valid code fragments).\n\n" +
      "Example: pattern='console.log($MSG)' rewrite='logger.info($MSG)' lang='typescript' — replaces all console.log calls with logger.info across TypeScript files.\n\n" +
      "**Warning: This tool modifies files directly.** Use dryRun=true to preview (shows per-file unified diff, capped at 8KB). Consider creating an aft_safety checkpoint before bulk replacements.",
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
      // Coerce at the boundary: dryRun "true" must stay preview-only (coerceBoolean).
      const isDryRun = coerceBoolean(args.dryRun);
      const paths = await resolveAstPaths(ctx, context, args.paths);

      const externalDenied = await checkAstPathsPermission(ctx, context, paths);
      if (externalDenied) return permissionDeniedResponse(externalDenied);

      if (!isDryRun) {
        // External-directory check first (mirrors opencode-native grep/glob directory checks).
        if (!Array.isArray(args.paths)) {
          const targetPath = await resolvePathArg(ctx, context, ".");
          const denial = await assertExternalDirectoryPermission(ctx, context, targetPath, {
            kind: "directory",
          });
          if (denial) return permissionDeniedResponse(denial);
        }

        const explicitPaths = paths !== undefined ? resolveRelativePatterns(context, paths) : [];
        const positiveGlobs = Array.isArray(args.globs)
          ? (args.globs as string[]).filter((glob) => !glob.startsWith("!"))
          : [];
        const patterns = [...explicitPaths, ...positiveGlobs];
        const metadata =
          explicitPaths.length === 1 && positiveGlobs.length === 0 && paths !== undefined
            ? { filepath: paths[0] }
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

      const rawArgs: Record<string, unknown> = {
        pattern: args.pattern,
        rewrite: args.rewrite,
        lang: args.lang,
      };
      if (paths !== undefined) rawArgs.paths = paths;
      if (args.globs !== undefined) rawArgs.globs = args.globs;
      rawArgs.dryRun = args.dryRun;
      const response = await callToolCall(ctx, context, "ast_replace", rawArgs);

      // Error response (e.g. invalid pattern)
      if (response.success === false) {
        throw new Error((response.message as string) || "ast_replace failed");
      }

      return response.text;
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
