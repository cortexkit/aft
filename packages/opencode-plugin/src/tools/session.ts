import type { ToolContext, ToolDefinition } from "@opencode-ai/plugin";
import { tool } from "@opencode-ai/plugin";
import type { PluginContext } from "../types.js";
import { callBridge, optionalInt } from "./_shared.js";

const z = tool.schema;

/**
 * Tool definitions for session-aware file history.
 */
export function sessionTools(ctx: PluginContext): Record<string, ToolDefinition> {
  return {
    aft_session: {
      description:
        "Show recent files accessed in this session, ordered by recency (newest first). " +
        "Tracks `read`, `zoom`, `edit`, `write`, `delete`, and `move` operations. " +
        "Use this when you need to recall what you were working on, find a file you " +
        "just edited, or re-orient after a context drop. Results include file path, " +
        "operation type, and timestamp.",
      args: {
        limit: optionalInt(1, 200).describe(
          "Max entries to return (default 50, max 200).",
        ),
      },
      handler: async (args: Record<string, unknown>, runtime: ToolContext) => {
        const response = await callBridge(
          ctx,
          runtime,
          "session_history",
          {
            limit: args.limit ?? 50,
          },
        );

        return JSON.stringify(response, null, 2);
      },
    },
  };
}
