import type { ToolDefinition } from "@opencode-ai/plugin";
import { tool } from "@opencode-ai/plugin";
import type { PluginContext } from "../types.js";
import { callToolCall, coerceOptionalInt, isEmptyParam, optionalInt } from "./_shared.js";
import {
  askSearchPermission,
  assertAftSearchExternalPermission,
  permissionDeniedResponse,
} from "./permissions.js";

const z = tool.schema;

type ToolArg = ToolDefinition["args"][string];

function arg(schema: unknown): ToolArg {
  return schema as ToolArg;
}

export function semanticTools(ctx: PluginContext): Record<string, ToolDefinition> {
  const searchTool: ToolDefinition = {
    // Lean and positive on purpose: this is the primary code-search tool, so
    // the description must not push agents elsewhere. The old "When NOT to
    // use: ... use grep directly" line fed the exact bash-grep reflex the
    // system prompt works to suppress, and sibling tools (aft_outline,
    // aft_callgraph) already describe themselves.
    description: [
      "Search code with one tool: concepts, identifiers, error strings, regex, literals, and filenames are auto-routed to the right engine and returned ranked. For conceptual 'how does X work' queries, phrase a full natural-language sentence — the semantic lane is NL-aware and matches intent against docstrings and comments ('how does the ORM build and execute a query', 'where is rate limiting handled'), not just keywords. Exact names, strings, and regex stay terse ('^export', 'Cargo.lock').",
    ].join("\n"),
    args: {
      query: arg(
        z
          .string()
          .describe(
            "Concept, regex, literal text, filename, or capability to find. Examples: 'fuzzy match with whitespace tolerance', '^export', 'Cargo.lock'.",
          ),
      ),
      topK: arg(optionalInt(1, 100).describe("Number of results (default: 10, max: 100)")),
      includeTests: arg(
        z
          .boolean()
          .optional()
          .describe(
            "Include test files (*.test.*, *_test.rs, __tests__/, …) plus test-support, fixture, mock, snapshot, and corpus files. Defaults to false.",
          ),
      ),
      path: arg(
        z
          .string()
          .optional()
          .describe(
            "Only set this to search a different Git project (absolute or ~ path). Omit it for the current configured workspace, including non-Git workspace roots; this is not a subdirectory filter. Unindexed external projects use a bounded lexical fallback.",
          ),
      ),
    },
    execute: async (args, context): Promise<string> => {
      if (
        isEmptyParam(args.query) ||
        typeof args.query !== "string" ||
        args.query.trim().length === 0
      ) {
        throw new Error("semantic_search: invalid params: `query` must be a non-empty string");
      }
      const query = args.query;
      const pathArg =
        typeof args.path === "string" && args.path.trim() ? args.path.trim() : undefined;

      // Auto routing may use either the indexed or grep-backed lane, so ask for
      // the aft_search permission before executing every search.
      const denied = await askSearchPermission(context, query);
      if (denied) return permissionDeniedResponse(denied);

      if (pathArg) {
        const denied = await assertAftSearchExternalPermission(ctx, context, pathArg);
        if (denied) return permissionDeniedResponse(denied);
      }

      const rawArgs: Record<string, unknown> = { query };
      const topK = coerceOptionalInt(args.topK, "topK", 1, 100);
      if (topK !== undefined) rawArgs.topK = topK;
      if (typeof args.includeTests === "boolean") rawArgs.includeTests = args.includeTests;
      if (pathArg) rawArgs.path = pathArg;
      const response = await callToolCall(ctx, context, "search", rawArgs);

      if (response.success === false) {
        const message =
          typeof response.text === "string" && response.text.length > 0
            ? response.text
            : typeof response.message === "string" && response.message.length > 0
              ? response.message
              : "semantic_search failed";
        throw new Error(message);
      }

      return response.text;
    },
  };

  return {
    aft_search: searchTool,
  };
}
