import * as fs from "node:fs";
import * as path from "node:path";
import type { ToolDefinition } from "@opencode-ai/plugin";
import { z } from "zod";
import type { PluginContext } from "../types.js";
import { callBridge } from "./_shared.js";
import {
  askGlobPermission,
  askGrepPermission,
  assertExternalDirectoryPermission,
  permissionDeniedResponse,
} from "./permissions.js";

type ToolArg = ToolDefinition["args"][string];

type GrepMatch = {
  file?: string;
  line?: number;
  line_text?: string;
  text?: string;
};

type GrepResponse = {
  text?: string;
  matches?: GrepMatch[];
  total_matches?: number;
  files_with_matches?: number;
};

function arg(schema: unknown): ToolArg {
  return schema as ToolArg;
}

function formatGrepOutput(response: GrepResponse): string {
  if (typeof response.text === "string") {
    return response.text;
  }

  const matches = Array.isArray(response.matches) ? response.matches : [];
  const totalMatches = response.total_matches ?? matches.length;
  const filesWithMatches = response.files_with_matches ?? new Set(matches.map((m) => m.file)).size;

  if (matches.length === 0) {
    return `Found ${totalMatches} match(es) in ${filesWithMatches} file(s).`;
  }

  const body = matches
    .map((match) => {
      const file = match.file ?? "unknown";
      const line = match.line ?? 0;
      const text = match.line_text ?? match.text ?? "";
      return `${file}:${line}: ${text}`;
    })
    .join("\n");

  return `${body}\n\nFound ${totalMatches} match(es) in ${filesWithMatches} file(s).`;
}

/** Ensure glob patterns match files in subdirectories — prefix with **\/ if no path separator. */
function normalizeGlob(pattern: string): string {
  if (!pattern.includes("/") && !pattern.startsWith("**/")) {
    return `**/${pattern}`;
  }
  return pattern;
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
      "Search file contents using regular expressions. Returns matching lines with file paths, line numbers, and context.",
    args: {
      pattern: arg(z.string().describe("Regular expression pattern to search for")),
      include: arg(
        z.string().optional().describe("File pattern to include (e.g. '*.ts', '*.{ts,tsx}')"),
      ),
      path: arg(z.string().optional().describe("Directory to search in, relative to project root")),
    },
    execute: async (args, context): Promise<string> => {
      const pattern = String(args.pattern);
      const includeArg = args.include ? String(args.include) : undefined;
      const pathArg = args.path ? String(args.path) : undefined;

      // Match OpenCode native ordering: grep permission first (on the raw
      // pattern + path the agent typed), then external_directory check on
      // the resolved search target if it points outside the project.
      const grepDenied = await askGrepPermission(context, pattern, {
        path: pathArg,
        include: includeArg,
      });
      if (grepDenied) return permissionDeniedResponse(grepDenied);

      if (pathArg) {
        let kind: "file" | "directory" = "file";
        try {
          const abs = path.isAbsolute(pathArg) ? pathArg : path.resolve(context.directory, pathArg);
          if (fs.lstatSync(abs).isDirectory()) kind = "directory";
        } catch {
          // Stat failed; conservative default "file" already set.
        }
        const externalDenied = await assertExternalDirectoryPermission(context, pathArg, {
          kind,
        });
        if (externalDenied) return permissionDeniedResponse(externalDenied);
      }

      const response = await callBridge(ctx, context, "grep", {
        pattern,
        case_sensitive: true,
        include: includeArg
          ? splitIncludeArg(includeArg).map(normalizeGlob).filter(Boolean)
          : undefined,
        path: pathArg,
        max_results: 100,
      });

      if (response.success === false) {
        throw new Error((response.message as string) || "grep failed");
      }

      return formatGrepOutput(response as GrepResponse);
    },
  };

  const globTool: ToolDefinition = {
    description:
      "Find files matching a glob pattern. Returns matching file paths sorted by modification time.",
    args: {
      pattern: arg(
        z.string().describe("Glob pattern to match (e.g. '**/*.ts', 'src/**/*.test.*')"),
      ),
      path: arg(z.string().optional().describe("Directory to search in, relative to project root")),
    },
    execute: async (args, context): Promise<string> => {
      // Handle absolute paths embedded in the pattern (e.g. "/abs/path/src/**/*.ts")
      // Split into path (directory prefix) and pattern (glob suffix)
      let globPattern = String(args.pattern);
      let globPath = args.path ? String(args.path) : undefined;

      if (!globPath && globPattern.startsWith("/")) {
        // Find the last directory component before any glob metacharacters
        const metaIdx = globPattern.search(/[*?{}[\]]/);
        if (metaIdx > 0) {
          const lastSlash = globPattern.lastIndexOf("/", metaIdx);
          if (lastSlash > 0) {
            globPath = globPattern.slice(0, lastSlash);
            globPattern = `**/${globPattern.slice(lastSlash + 1)}`;
          }
        }
      }

      // Match OpenCode native ordering: glob permission first, then
      // external_directory check on the resolved search root if it's
      // outside the project.
      const globDenied = await askGlobPermission(context, globPattern, { path: globPath });
      if (globDenied) return permissionDeniedResponse(globDenied);

      if (globPath) {
        let kind: "file" | "directory" = "directory";
        try {
          const abs = path.isAbsolute(globPath)
            ? globPath
            : path.resolve(context.directory, globPath);
          if (fs.lstatSync(abs).isFile()) kind = "file";
        } catch {
          // Stat failed; keep "directory" as conservative default for glob.
        }
        const externalDenied = await assertExternalDirectoryPermission(context, globPath, {
          kind,
        });
        if (externalDenied) return permissionDeniedResponse(externalDenied);
      }

      const response = await callBridge(ctx, context, "glob", {
        pattern: globPattern,
        path: globPath,
      });

      if (response.success === false) {
        throw new Error((response.message as string) || "glob failed");
      }

      if (typeof response.text === "string") {
        return response.text;
      }

      if (Array.isArray(response.files)) {
        return response.files.join("\n");
      }

      return (response.text as string) || JSON.stringify(response);
    },
  };

  const hoisting = ctx.config.hoist_builtin_tools !== false;
  return {
    [hoisting ? "grep" : "aft_grep"]: grepTool,
    [hoisting ? "glob" : "aft_glob"]: globTool,
  };
}
