import type { ToolDefinition } from "@opencode-ai/plugin";
import { tool } from "@opencode-ai/plugin";
import type { PluginContext } from "../types.js";
import { callToolCall, coerceOptionalInt, isEmptyParam } from "./_shared.js";

const z = tool.schema;

/**
 * Tool definition for aft_gather_context — deterministic context-pack builder.
 *
 * Replaces a multi-turn search → outline → zoom → callgraph read chain
 * with ONE call that returns verbatim code evidence, not conclusions.
 */
export function gatherTools(_ctx: PluginContext): Record<string, ToolDefinition> {
  return {
    aft_gather_context: {
      description:
        "Assemble a deterministic 'context pack' — ranked, deduped, budgeted verbatim code evidence — in ONE call instead of a multi-turn search→outline→zoom→callgraph chain. " +
        "Returns code bodies with file:line headers, not conclusions.\n\n" +
        "Two modes (mutually exclusive):\n" +
        '- question mode: `{ question: "how does X work?" }` — semantic-search-seeded. Ranks seeds by search score.\n' +
        '- symbol mode: `{ symbol: "handle_zoom", path: "src/commands/zoom.rs" }` — impact-seeded (blast-radius callers + callees).\n\n' +
        "Optional: `budget` (default 400 lines, max 800). When the budget is exhausted, remaining candidates appear as one-line stubs under '## Beyond budget (zoom to expand)'.\n\n" +
        "Use when: the agent would otherwise need 4-6 serial tools to gather code context around a question or symbol. NOT for quick single-symbol reads (use aft_zoom).",
      args: {
        question: z
          .string()
          .optional()
          .describe(
            "Natural-language question to seed the pack via semantic search. Mutually exclusive with 'symbol'+'path'.",
          ),
        symbol: z
          .string()
          .optional()
          .describe(
            "Symbol name for impact-seeded mode. Requires 'path'. Mutually exclusive with 'question'.",
          ),
        path: z
          .string()
          .optional()
          .describe(
            "Path to the source file for impact-seeded mode. Required when 'symbol' is provided. Mutually exclusive with 'question'.",
          ),
        budget: z
          .number()
          .int()
          .min(1)
          .max(800)
          .optional()
          .describe(
            "Output line budget for the pack (default 400, max 800). Budget-excluded candidates are listed as stubs.",
          ),
        includeTests: z
          .boolean()
          .optional()
          .describe("Include test files in callers/paths. Defaults to false; tests are hidden."),
      },
      execute: async (args, context): Promise<string> => {
        const hasQuestion = !isEmptyParam(args.question);
        const hasSymbol = !isEmptyParam(args.symbol);
        const hasPath = !isEmptyParam(args.path);

        // Mode validation — same rules as Rust-side translate_gather.
        if (hasQuestion && (hasSymbol || hasPath)) {
          throw new Error(
            "aft_gather_context: provide exactly ONE mode — either 'question' OR 'symbol'+'path'",
          );
        }
        if (hasSymbol !== hasPath) {
          throw new Error("aft_gather_context: 'symbol' and 'path' must be provided together");
        }
        if (!hasQuestion && !hasSymbol && !hasPath) {
          throw new Error("aft_gather_context: provide either 'question' or 'symbol'+'path'");
        }

        const rawArgs: Record<string, unknown> = {};
        if (hasQuestion) rawArgs.question = args.question;
        if (hasSymbol) rawArgs.symbol = args.symbol;
        if (hasPath) rawArgs.filePath = args.path;

        const budget = coerceOptionalInt(args.budget, "budget", 1, 800);
        if (budget !== undefined) rawArgs.budget = budget;
        if (!isEmptyParam(args.includeTests)) rawArgs.includeTests = args.includeTests;

        const response = await callToolCall(_ctx, context, "gather", rawArgs);
        if (response.success === false) {
          throw new Error((response.message as string) || response.text || "gather failed");
        }
        return response.text;
      },
    },
  };
}
