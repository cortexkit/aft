import { coerceBoolean } from "@cortexkit/aft-bridge";
import type { ToolDefinition } from "@opencode-ai/plugin";
import { tool } from "@opencode-ai/plugin";
import { toolEnabled } from "../config.js";
import { prepareToolMap } from "../normalize-schemas.js";
import type { PluginContext } from "../types.js";
import {
  callToolCall,
  coerceOptionalInt,
  isEmptyParam,
  optionalInt,
  resolvePathArg,
} from "./_shared.js";
import { assertExternalDirectoryPermission, permissionDeniedResponse } from "./permissions.js";

const z = tool.schema;

// Read-only navigation outcomes that are legitimate negative/transient answers,
// not failures: the symbol isn't defined here, or the store is still building.
// These return as plain text (no red error) so the agent reads them as "no
// result" the same way grep-with-no-matches does. Everything else
// (invalid_request, path_outside_project_root, ambiguous_target, or any unknown
// code) still throws so real errors stay visible. ("no path between symbols" is
// already a success response with reason=no_path_found, never an error code.)
const CALLGRAPH_SOFT_CODES = new Set(["symbol_not_found", "callgraph_building"]);

/**
 * Tool definitions for call-graph navigation: call_tree, callers, trace_to, trace_to_symbol, impact, and trace_data.
 */
export function navigationTools(ctx: PluginContext): Record<string, ToolDefinition> {
  const zoomEnabled = toolEnabled(ctx.config, "aft_zoom");
  const tools = prepareToolMap({
    aft_callgraph: {
      description:
        "Answer code-relationship questions from a real call graph — instead of grep + read chains. Reach for this whenever the question is about how symbols connect: who calls X, what X calls, what breaks if X changes, how execution reaches X, or how a value flows.\n\n" +
        (zoomEnabled
          ? "Use aft_zoom with `callgraph:true` for one-level forward calls-out while reading source. Use aft_callgraph only for reverse callers or multi-level traces so you do not double-fetch the same relationships.\n\n"
          : "Use aft_callgraph call_tree (depth 1) for one-level forward calls-out; use it for reverse callers or multi-level traces.\n\n") +
        "Ops:\n" +
        "- 'callers': Find all call sites of a symbol. Use before renaming or changing a function's signature.\n" +
        "- 'impact': What breaks if a symbol changes — affected callers with signatures and entry-point status (blast radius). Use before a risky edit.\n" +
        "- 'call_tree': What a function calls (forward traversal). Use to understand a function's dependencies before modifying it.\n" +
        "- 'trace_to': How execution reaches a function from entry points (routes, exports, main). Use to understand context around deeply-nested code.\n" +
        "- 'trace_to_symbol': Shortest call path from one symbol to another. Requires 'toSymbol'. If multiple targets match, the error returns candidate files; retry with 'toPath' to disambiguate.\n" +
        "- 'trace_data': Follow a value through variable assignments and function parameters across files. Requires 'symbol' (scope to trace from) and 'expression'.\n\n" +
        "All ops require both 'path' and 'symbol'. 'expression' is additionally required for trace_data; 'toSymbol' for trace_to_symbol.\n\n" +
        "Markers: ~ = edge resolved by name only (may point at the wrong same-named symbol); [unresolved] = callee not resolved to a definition, so the location shown is the call site. Unmarked edges are resolved exactly. By default, unresolved external/stdlib leaf calls in call_tree are collapsed into one summary per parent; pass includeUnresolved=true to show every unresolved edge individually.\n",
      // Parameters are Zod-optional because different ops need different subsets.
      // Runtime guards below validate per-op requirements and give clear errors.
      args: {
        op: z
          .enum(["call_tree", "callers", "trace_to", "trace_to_symbol", "impact", "trace_data"])
          .describe("Navigation operation"),
        path: z
          .string()
          .describe(
            "Path to the source file containing the symbol (absolute or relative to project root)",
          ),
        symbol: z.string().describe("Name of the symbol to analyze"),
        depth: optionalInt(1, Number.MAX_SAFE_INTEGER).describe(
          "Max traversal depth (default: call_tree=5, callers=1, trace_to=10, trace_to_symbol=10 capped at 16, impact=5, trace_data=5)",
        ),
        expression: z
          .string()
          .optional()
          .describe("Expression to track through data flow (required for trace_data op)"),
        toSymbol: z
          .string()
          .optional()
          .describe(
            "Target symbol name for trace_to_symbol; the returned path ends at this symbol",
          ),
        toPath: z
          .string()
          .optional()
          .describe(
            "Optional target file for trace_to_symbol; required when toSymbol exists in multiple files",
          ),
        includeTests: z
          .boolean()
          .optional()
          .describe("Include test files in callers/paths. Defaults to false; tests are hidden."),
        includeUnresolved: z
          .boolean()
          .optional()
          .describe(
            "Show every unresolved external/stdlib call individually. Defaults to false; unresolved leaf calls are collapsed into one summary per parent.",
          ),
      },
      execute: async (args, context): Promise<string> => {
        if (isEmptyParam(args.path)) {
          throw new Error("'path' is required");
        }
        if (isEmptyParam(args.symbol)) {
          throw new Error("'symbol' is required");
        }
        if (args.op === "trace_data" && isEmptyParam(args.expression)) {
          throw new Error("'expression' is required for 'trace_data' op");
        }
        if (args.op === "trace_to_symbol" && isEmptyParam(args.toSymbol)) {
          throw new Error("'toSymbol' is required for 'trace_to_symbol' op");
        }

        const filePath = await resolvePathArg(ctx, context, args.path as string);
        const toFile = !isEmptyParam(args.toPath)
          ? await resolvePathArg(ctx, context, args.toPath as string)
          : undefined;

        const checked = new Set<string>();
        for (const target of [filePath, ...(toFile !== undefined ? [toFile] : [])]) {
          if (checked.has(target)) continue;
          checked.add(target);
          const denial = await assertExternalDirectoryPermission(ctx, context, target);
          if (denial) return permissionDeniedResponse(denial);
        }

        const rawArgs: Record<string, unknown> = {
          op: args.op,
          filePath: args.path,
          symbol: args.symbol,
        };
        const depth = coerceOptionalInt(args.depth, "depth", 1, Number.MAX_SAFE_INTEGER);
        if (depth !== undefined) rawArgs.depth = depth;
        if (!isEmptyParam(args.expression)) rawArgs.expression = args.expression;
        if (!isEmptyParam(args.toSymbol)) rawArgs.toSymbol = args.toSymbol;
        if (!isEmptyParam(args.toPath)) rawArgs.toFile = args.toPath;
        if (!isEmptyParam(args.includeTests))
          rawArgs.includeTests = coerceBoolean(args.includeTests);
        if (!isEmptyParam(args.includeUnresolved))
          rawArgs.includeUnresolved = coerceBoolean(args.includeUnresolved);
        const response = await callToolCall(ctx, context, "callgraph", rawArgs);
        if (response.success === false) {
          const code = typeof response.code === "string" ? response.code : "";
          // Read-only navigation negatives ("symbol isn't here", "no path between
          // them", "store still building") are legitimate answers, not failures —
          // return them as plain text so the UI doesn't paint them red. Genuine
          // errors (invalid_request, boundary violations, anything unknown) still
          // throw so they surface as errors.
          if (CALLGRAPH_SOFT_CODES.has(code)) {
            return response.text;
          }
          throw new Error(response.text || response.message || "callgraph failed");
        }
        return response.text;
      },
    },
  });
  return tools;
}
