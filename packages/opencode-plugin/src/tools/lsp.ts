import type { ToolDefinition } from "@opencode-ai/plugin";
import { tool } from "@opencode-ai/plugin";
import type { PluginContext } from "../types.js";
import { callBridge } from "./_shared.js";

const z = tool.schema;
/**
 * Tool definitions for LSP commands: diagnostics.
 */
export function lspTools(ctx: PluginContext): Record<string, ToolDefinition> {
  const diagnosticsTool: ToolDefinition = {
    description:
      "On-demand LSP file/scope check. Spawns the relevant language server (if " +
      "registered for the file's extension), opens the document, prefers LSP 3.17 " +
      "pull diagnostics when supported, and falls back to push + waitMs otherwise. " +
      "NOT a project-wide type checker — for full coverage run `tsc --noEmit`, " +
      "`cargo check`, `pyright`, etc.\n" +
      "\n" +
      "Returns: {\n" +
      "  diagnostics: Array<{file, line, column, end_line, end_column, severity, message, code, source}>,\n" +
      "  total: number,\n" +
      "  files_with_errors: number,\n" +
      "  complete: boolean,                  // true = trustable absence; false = partial\n" +
      "  lsp_servers_used: Array<{           // honest per-server status\n" +
      "    server_id, scope: 'file'|'workspace', status: 'pull_ok'|'pull_unchanged'|'push_only'|'no_root_marker (...)' |\n" +
      "    'binary_not_installed: <name>'|'spawn_failed: ...'|'pull_failed: ...'|'workspace_pull_unsupported'|...\n" +
      "  }>,\n" +
      "  unchecked_files?: string[],         // directory mode only — files we have no info for\n" +
      "  walk_truncated?: boolean,           // directory walk hit the 200-file cap\n" +
      "  note?: string                       // present when no LSP server is registered for the file's extension\n" +
      "}\n" +
      "\n" +
      "**Reading the response honestly:**\n" +
      "- `total: 0, complete: true, lsp_servers_used: [{status: 'pull_ok'}]` → file is genuinely clean.\n" +
      "- `total: 0, lsp_servers_used: []` → **nothing was checked** (no server registered for this extension). Tell the user, don't claim 'no errors'.\n" +
      "- `lsp_servers_used: [{status: 'binary_not_installed: <name>'}]` → server matched the extension but its binary isn't on PATH. Tell the user to install it.\n" +
      "- `lsp_servers_used: [{status: 'no_root_marker (...)'}]` → server is registered but couldn't find a workspace root marker walking up from this file. The user's project layout doesn't match what the server expects.\n" +
      "- `complete: false` (directory mode) → some files in the directory weren't checked; see `unchecked_files`.\n" +
      "\n" +
      "**When this tool gives an unhelpful answer**, run `npx @cortexkit/aft doctor lsp <filePath>` from a terminal to get a full per-server breakdown (registered servers, binary resolution, root-marker resolution, spawn outcome).",
    args: {
      filePath: z
        .string()
        .optional()
        .describe(
          "Path to a file to check. Mutually exclusive with 'directory'. Omit both to dump all cached diagnostics.",
        ),
      directory: z
        .string()
        .optional()
        .describe(
          "Path to a directory. Returns cached diagnostics + workspace pull from active servers; lists files we have no info for in 'unchecked_files'. Capped at 200 walked files.",
        ),
      severity: z
        .enum(["error", "warning", "information", "hint", "all"])
        .optional()
        .describe("Filter by severity (default: 'all')."),
      waitMs: z
        .number()
        .optional()
        .describe(
          "Wait up to N ms (max 10000, default 0) for push diagnostics to arrive. Only matters for servers that don't support LSP 3.17 pull (bash-language-server, yaml-language-server). Use after an edit to let the server re-analyze.",
        ),
    },
    execute: async (args, context): Promise<string> => {
      const filePath = args.filePath || undefined; // treat empty string as absent
      const directory = args.directory || undefined;
      if (filePath !== undefined && directory !== undefined) {
        throw new Error(
          "'filePath' and 'directory' are mutually exclusive — provide one or neither",
        );
      }
      const params: Record<string, unknown> = {};
      if (filePath !== undefined) params.file = filePath;
      if (directory !== undefined) params.directory = directory;
      if (args.severity !== undefined) params.severity = args.severity;
      if (args.waitMs !== undefined) params.wait_ms = args.waitMs;
      const result = await callBridge(ctx, context, "lsp_diagnostics", params);
      if (result.success === false) {
        throw new Error((result.message as string) || "lsp_diagnostics failed");
      }
      return JSON.stringify(result);
    },
  };

  // When hoisting: register as lsp_diagnostics (override oh-my-opencode's)
  // When not hoisting: register as aft_lsp_diagnostics
  const hoisting = ctx.config.hoist_builtin_tools !== false;
  return {
    [hoisting ? "lsp_diagnostics" : "aft_lsp_diagnostics"]: diagnosticsTool,
  };
}
