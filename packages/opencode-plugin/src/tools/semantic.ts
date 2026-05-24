import type { ToolDefinition } from "@opencode-ai/plugin";
import { z } from "zod";
import type { PluginContext } from "../types.js";
import { callBridge } from "./_shared.js";

type ToolArg = ToolDefinition["args"][string];

function arg(schema: unknown): ToolArg {
  return schema as ToolArg;
}

export function semanticTools(ctx: PluginContext): Record<string, ToolDefinition> {
  const searchTool: ToolDefinition = {
    description: [
      "Find symbols by concept using hybrid semantic + lexical search. Returns ranked code matches with similarity scores and provenance tags.",
      "",
      "When to reach for it:",
      "- Exploring an unfamiliar area: 'where is rate limiting handled', 'how does auth flow work'",
      "- Concept doesn't appear as a literal string: 'retry logic', 'cache invalidation', 'graceful shutdown'",
      "- Filename-shaped concepts: 'the bridge spawn helper', 'the session detection module'",
      "- After 2+ grep attempts that came back empty or noisy",
      "- You know roughly what the function does but not what it's named",
      "",
      "When NOT to use:",
      "- You have an error message or stack trace → use grep",
      "- You want the file/module structure → use aft_outline",
      "- You're following a call chain → use aft_navigate",
      "",
      "Each result tags `source` as one of: 'semantic' (embedding match only), 'lexical' (trigram exact-token match the embedding lane missed), or 'hybrid' (both lanes agreed — strongest signal).",
    ].join("\n"),
    args: {
      query: arg(
        z
          .string()
          .describe(
            "Concept or capability to find, phrased as a programmer would describe the code. Examples: 'fuzzy match with whitespace tolerance', 'undo backup before edit', 'retry failed network request'.",
          ),
      ),
      topK: arg(z.number().optional().describe("Number of results (default: 10, max: 100)")),
    },
    execute: async (args, context): Promise<string> => {
      const response = await callBridge(ctx, context, "semantic_search", {
        query: args.query,
        top_k: args.topK ?? 10,
      });

      if (response.success === false) {
        if (
          response.code === "semantic_search_unavailable" &&
          typeof response.message === "string"
        ) {
          return response.message;
        }

        throw new Error((response.message as string) || "semantic_search failed");
      }

      if (typeof response.text === "string") {
        return response.text;
      }

      return JSON.stringify(response);
    },
  };

  return {
    aft_search: searchTool,
  };
}
