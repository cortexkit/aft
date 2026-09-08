import * as fs from "node:fs";
import * as path from "node:path";
import type { ToolDefinition } from "@opencode-ai/plugin";
import { z } from "zod";
import type { PluginContext } from "../types.js";
import {
  callToolCall,
  expandTilde,
  resolvePathFromProjectRoot,
  resolveProjectRoot,
} from "./_shared.js";
import {
  askGlobPermission,
  askGrepPermission,
  assertExternalDirectoryPermission,
  permissionDeniedResponse,
} from "./permissions.js";

type ToolArg = ToolDefinition["args"][string];
type SearchPathKind = "file" | "directory";
type SearchPathTarget = { target: string; kind: SearchPathKind };
type SearchPathArgSplit = { paths: string[]; missing: string[] };

function arg(schema: unknown): ToolArg {
  return schema as ToolArg;
}

function absoluteSearchPath(projectRoot: string, target: string): string {
  return resolvePathFromProjectRoot(projectRoot, expandTilde(target));
}

function searchPathExists(projectRoot: string, target: string): boolean {
  return fs.existsSync(absoluteSearchPath(projectRoot, target));
}

function splitSearchPathArg(projectRoot: string, raw: string): SearchPathArgSplit {
  if (searchPathExists(projectRoot, raw) || !/\s/.test(raw)) {
    return { paths: [raw], missing: [] };
  }

  const fragments = raw.trim().split(/\s+/).filter(Boolean);
  if (fragments.length < 2) {
    return { paths: [raw], missing: [] };
  }

  const existing: string[] = [];
  const missing: string[] = [];
  for (const fragment of fragments) {
    if (searchPathExists(projectRoot, fragment)) {
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

function bridgeSearchPathArg(projectRoot: string, split: SearchPathArgSplit): string {
  return split.paths.map((target) => absoluteSearchPath(projectRoot, target)).join(" ");
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

function searchPathKind(
  projectRoot: string,
  target: string,
  defaultKind: SearchPathKind,
): SearchPathKind {
  try {
    const stat = fs.lstatSync(absoluteSearchPath(projectRoot, target));
    if (defaultKind === "file") {
      return stat.isDirectory() ? "directory" : "file";
    }
    return stat.isFile() ? "file" : "directory";
  } catch {
    return defaultKind;
  }
}

function searchPathTargets(
  projectRoot: string,
  split: SearchPathArgSplit,
  defaultKind: SearchPathKind,
): SearchPathTarget[] {
  return split.paths.map((target) => {
    const absoluteTarget = absoluteSearchPath(projectRoot, target);
    return {
      target: absoluteTarget,
      kind: searchPathKind(projectRoot, target, defaultKind),
    };
  });
}

/**
 * Brace-aware comma split. Allows users to type either of:
 *
 *   - "*.ts,*.tsx"            (multiple OpenCode-style includes)
 *   - "**\/*.{vue,ts,tsx}"    (a single glob with a brace alternation)
 *   - "*.ts,**\/*.{vue,tsx}"  (mix of both)
 *
 * Without brace awareness the naive `String#split(",")` chops the brace
 * group apart and the resulting `**\/*.{vue` glob fails parsing in
 * ripgrep / globset with `unclosed alternate group; missing '}'`.
 */
export function splitIncludeArg(raw: string): string[] {
  const out: string[] = [];
  let depth = 0;
  let buf = "";
  for (const ch of raw) {
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
 * Tool definitions for indexed search-backed grep and glob.
 */
export function searchTools(ctx: PluginContext): Record<string, ToolDefinition> {
  const grepTool: ToolDefinition = {
    description:
      "Search file contents using regular expressions. Returns matching lines with file paths and line numbers (no surrounding context lines — use `read` for that). Always case-sensitive. Capped at 100 matches; if you hit the cap, narrow with `path` or `include` and re-run. When a list is cut, the reply ends with `shown N of M <unit> (<reason>) · narrow: <knobs>`; absence of that line means the list is complete.",
    args: {
      pattern: arg(z.string().describe("Regular expression pattern to search for")),
      include: arg(
        z.string().optional().describe("File pattern to include (e.g. '*.ts', '*.{ts,tsx}')"),
      ),
      path: arg(
        z
          .string()
          .optional()
          .describe("Directory to search (absolute or relative to project root)"),
      ),
    },
    execute: async (args, context): Promise<string> => {
      const projectRoot = await resolveProjectRoot(ctx, context);
      const pattern = String(args.pattern);
      const includeArg = args.include ? String(args.include) : undefined;
      const pathArg = args.path ? String(args.path) : undefined;
      const pathSplit = pathArg ? splitSearchPathArg(projectRoot, pathArg) : undefined;
      const bridgePath = pathSplit ? bridgeSearchPathArg(projectRoot, pathSplit) : undefined;

      // Match OpenCode native ordering: grep permission first (on the raw
      // pattern + path the agent typed), then external_directory check on
      // the resolved search target if it points outside the project.
      const grepDenied = await askGrepPermission(context, pattern, {
        path: bridgePath,
        include: includeArg,
      });
      if (grepDenied) return permissionDeniedResponse(grepDenied);

      if (pathSplit) {
        for (const target of searchPathTargets(projectRoot, pathSplit, "file")) {
          const externalDenied = await assertExternalDirectoryPermission(
            ctx,
            context,
            target.target,
            {
              kind: target.kind,
            },
          );
          if (externalDenied) return permissionDeniedResponse(externalDenied);
        }
      }

      const rawArgs: Record<string, unknown> = { pattern };
      if (includeArg !== undefined) rawArgs.include = includeArg;
      if (bridgePath !== undefined) rawArgs.path = bridgePath;
      const response = await callToolCall(ctx, context, "grep", rawArgs);

      if (response.success === false) {
        throw new Error((response.message as string) || "grep failed");
      }

      if (pathSplit && pathSplit.missing.length > 0) {
        response.complete = false;
      }

      return appendSkippedSearchPaths(response.text, pathSplit?.missing ?? []);
    },
  };

  const globTool: ToolDefinition = {
    description:
      "Find files matching a glob pattern. Returns matching file paths sorted by modification time. When a list is cut, the reply ends with `shown N of M <unit> (<reason>) · narrow: <knobs>`; absence of that line means the list is complete.",
    args: {
      pattern: arg(
        z.string().describe("Glob pattern to match (e.g. '**/*.ts', 'src/**/*.test.*')"),
      ),
      path: arg(
        z
          .string()
          .optional()
          .describe("Directory to search (absolute or relative to project root)"),
      ),
    },
    execute: async (args, context): Promise<string> => {
      const projectRoot = await resolveProjectRoot(ctx, context);
      // Handle absolute paths embedded in the pattern (e.g. "/abs/path/src/**/*.ts")
      // Split into path (directory prefix) and pattern (glob suffix)
      let globPattern = expandTilde(String(args.pattern));
      let globPath = args.path ? String(args.path) : undefined;

      if (!globPath && globPattern.startsWith("/")) {
        // Find the last directory component before any glob metacharacters.
        // Exact absolute paths need the same split because the bridge matches
        // glob patterns relative to the search path.
        const metaIdx = globPattern.search(/[*?{}[\]]/);
        if (metaIdx > 0) {
          const lastSlash = globPattern.lastIndexOf("/", metaIdx);
          if (lastSlash > 0) {
            globPath = globPattern.slice(0, lastSlash);
            globPattern = `**/${globPattern.slice(lastSlash + 1)}`;
          }
        } else if (metaIdx === -1) {
          globPath = path.dirname(globPattern);
          globPattern = path.basename(globPattern);
        }
      }

      const pathSplit = globPath ? splitSearchPathArg(projectRoot, globPath) : undefined;
      const bridgePath = pathSplit ? bridgeSearchPathArg(projectRoot, pathSplit) : undefined;

      // Match OpenCode native ordering: glob permission first, then
      // external_directory check on the resolved search root if it's
      // outside the project.
      const globDenied = await askGlobPermission(context, globPattern, { path: bridgePath });
      if (globDenied) return permissionDeniedResponse(globDenied);

      if (pathSplit) {
        for (const target of searchPathTargets(projectRoot, pathSplit, "directory")) {
          const externalDenied = await assertExternalDirectoryPermission(
            ctx,
            context,
            target.target,
            {
              kind: target.kind,
            },
          );
          if (externalDenied) return permissionDeniedResponse(externalDenied);
        }
      }

      const rawArgs: Record<string, unknown> = { pattern: globPattern };
      if (bridgePath !== undefined) rawArgs.path = bridgePath;
      const response = await callToolCall(ctx, context, "glob", rawArgs);

      if (response.success === false) {
        throw new Error((response.message as string) || "glob failed");
      }

      if (pathSplit && pathSplit.missing.length > 0) {
        response.complete = false;
      }

      return appendSkippedSearchPaths(response.text, pathSplit?.missing ?? []);
    },
  };

  const hoisting = ctx.config.hoist_builtin_tools !== false;
  return {
    [hoisting ? "grep" : "aft_grep"]: grepTool,
    [hoisting ? "glob" : "aft_glob"]: globTool,
  };
}
