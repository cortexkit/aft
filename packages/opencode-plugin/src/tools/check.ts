import type { ToolDefinition } from "@opencode-ai/plugin";
import { tool } from "@opencode-ai/plugin";
import type { PluginContext } from "../types.js";
import { callToolCall, resolvePathArg } from "./_shared.js";
import { assertExternalDirectoryPermission } from "./permissions.js";

const z = tool.schema;

type ToolArg = ToolDefinition["args"][string];

function arg(schema: unknown): ToolArg {
  return schema as ToolArg;
}

export function createCheckTool(ctx: PluginContext): ToolDefinition {
  return {
    description:
      "Run the configured type checker (e.g. ruff, tsc, pyright) on a single file and return validation errors. " +
      "Use this AFTER completing a batch of edits to verify correctness, not after every intermediate edit. " +
      "The checker is determined by the `checker` config or auto-detected from project config files. " +
      "Returns error count and per-error details (line, message).",
    args: {
      filePath: arg(z.string().describe("Absolute or project-relative path to the file to check.")),
    },
    execute: async (args, context) => {
      const resolved = await resolvePathArg(ctx, context, args.filePath as string);
      const denial = await assertExternalDirectoryPermission(ctx, context, resolved);
      if (denial) return denial;
      const result = await callToolCall(ctx, context, "check", { filePath: resolved });
      if (result.success === false) {
        throw new Error((result.message as string) || "check failed");
      }
      return result.text;
    },
  };
}
