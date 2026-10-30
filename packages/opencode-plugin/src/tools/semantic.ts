import { canonicalizeProjectRoot, coerceBoolean } from "@cortexkit/aft-bridge";
import type { ToolDefinition } from "@opencode-ai/plugin";
import { tool } from "@opencode-ai/plugin";
import type { PluginContext } from "../types.js";
import {
  callToolCall,
  coerceOptionalInt,
  isEmptyParam,
  optionalInt,
  projectRootFor,
  resolvePathFromProjectRoot,
} from "./_shared.js";
import {
  askSearchPermission,
  assertAftSearchExternalPermission,
  permissionDeniedResponse,
} from "./permissions.js";

function hasMeaningfulProjectSearchFallback(
  query: string,
  pathArg: string,
  externalRoot: string,
): boolean {
  const trimmedQuery = query.trim();
  // A non-empty code query is useful in the current project unless the caller
  // used the query solely as the external path itself. There is no separate
  // "external-only" flag in the public tool schema, so this is the only
  // unambiguous shape where dropping `path` would lose the request's purpose.
  return trimmedQuery.length > 0 && trimmedQuery !== pathArg && trimmedQuery !== externalRoot;
}

/**
 * Ends the refusal of a rule-denied external `path`: that root was not
 * searched, and how to search the current project instead.
 */
const DENIED_PATH_NOT_SEARCHED =
  "{root} was not searched. To search the current project instead, call aft_search again without `path`.";

function deniedPathNotSearched(root: string): string {
  return DENIED_PATH_NOT_SEARCHED.replace("{root}", () => root);
}

const GENERATION_CHANGED_DISCLOSURE = "index changed - order re-derived";

type SearchContinuityBySession = Map<string, Map<string, string>>;

function asRecord(value: unknown): Record<string, unknown> | undefined {
  return typeof value === "object" && value !== null && !Array.isArray(value)
    ? (value as Record<string, unknown>)
    : undefined;
}

function normalizeContinuityQuery(query: string): string {
  return query
    .trim()
    .split(/\s+/u)
    .join(" ")
    .replace(/[A-Z]/g, (letter) => letter.toLowerCase());
}

function continuityKey(
  projectRoot: string,
  query: string,
  includeTests: boolean | undefined,
  topK: number | undefined,
): string {
  return JSON.stringify([
    projectRoot,
    normalizeContinuityQuery(query),
    includeTests ?? false,
    topK ?? 10,
  ]);
}

function responseGeneration(response: unknown): string | undefined {
  const envelope = asRecord(response);
  const structured =
    asRecord(envelope?.structuredContent) ?? asRecord(envelope?.structured_content);
  const plan = asRecord(envelope?.plan) ?? asRecord(structured?.plan);
  const generation = plan?.snapshot_generation ?? envelope?.snapshot_generation;
  return typeof generation === "string" ? generation : undefined;
}

function surfaceSearchText(
  response: unknown,
  continuity: SearchContinuityBySession,
  sessionId: string,
  key: string,
): string {
  const envelope = asRecord(response);
  const text = typeof envelope?.text === "string" ? envelope.text : "";
  const generation = responseGeneration(response);
  if (generation === undefined) return text;

  let sessionContinuity = continuity.get(sessionId);
  if (!sessionContinuity) {
    sessionContinuity = new Map();
    continuity.set(sessionId, sessionContinuity);
  }
  const previous = sessionContinuity.get(key);
  sessionContinuity.set(key, generation);
  return previous !== undefined && previous !== generation
    ? `${GENERATION_CHANGED_DISCLOSURE}\n${text}`
    : text;
}

const z = tool.schema;

type ToolArg = ToolDefinition["args"][string];

function arg(schema: unknown): ToolArg {
  return schema as ToolArg;
}

export function semanticTools(ctx: PluginContext): Record<string, ToolDefinition> {
  const continuity: SearchContinuityBySession = new Map();
  const searchTool: ToolDefinition = {
    // Lean and positive on purpose: this is the primary code-search tool, so
    // the description must not push agents elsewhere. The old "When NOT to
    // use: ... use grep directly" line fed the exact bash-grep reflex the
    // system prompt works to suppress, and sibling tools (aft_outline,
    // aft_callgraph) already describe themselves.
    description: [
      "Search code with one tool: concepts, identifiers, error strings, regex, literals, and filenames are auto-routed to the right engine and returned ranked. For conceptual 'how does X work' queries, phrase a full natural-language sentence — the semantic lane is NL-aware and matches intent against docstrings and comments ('how does the ORM build and execute a query', 'where is rate limiting handled'), not just keywords. Exact names, strings, and regex stay terse ('^export', 'Cargo.lock').",
      "When you know a name or string that must appear, put it in `pattern` (a regex, grep syntax, case-sensitive) and keep the question in `query`: both are ranked together, and the reply's first line says how many files the pattern matched and where it is defined. Example: query 'how is the config file loaded', pattern 'load_config|ConfigLoader'. `pattern` alone ranks the files it matches, definitions first.",
      "Use `offset` to continue a ranked result list from a zero-based position.",
      "When a list is cut, the reply ends with `shown N of M <unit> (<reason>) · narrow: <knobs>`; absence of that line means the list is complete.",
    ].join("\n\n"),
    args: {
      query: arg(
        z
          .string()
          .optional()
          .describe(
            "Concept, regex, literal text, filename, or capability to find. Examples: 'fuzzy match with whitespace tolerance', '^export', 'Cargo.lock'. Optional when `pattern` is given.",
          ),
      ),
      pattern: arg(
        z
          .string()
          .optional()
          .describe(
            "Regex (grep syntax, case-sensitive) for names or text that must appear, e.g. 'load_config|ConfigLoader'. Ranked with `query` when both are given.",
          ),
      ),
      topK: arg(optionalInt(1, 50).describe("Number of results (default: 10, max: 50)")),
      offset: arg(
        optionalInt(0, 100000).describe("Zero-based result offset (default: 0, max: 100000)."),
      ),
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
            "Only set this to search a different project or plain folder (absolute or ~ path). Omit it for the current configured workspace, including non-Git workspace roots; this is not a subdirectory filter. Unindexed external folders use a bounded lexical fallback; filesystem roots, home and its ancestors are refused.",
          ),
      ),
    },
    execute: async (args, context): Promise<string> => {
      const hasQuery = typeof args.query === "string" && args.query.trim().length > 0;
      // A blank pattern counts as absent; a real one is sent as written,
      // because whitespace inside a regex is part of what it matches.
      if (!isEmptyParam(args.pattern) && typeof args.pattern !== "string") {
        throw new Error("semantic_search: invalid params: `pattern` must be a string");
      }
      const pattern =
        typeof args.pattern === "string" && args.pattern.trim().length > 0
          ? args.pattern
          : undefined;
      if (!hasQuery && pattern === undefined) {
        if (!isEmptyParam(args.query) && typeof args.query !== "string") {
          throw new Error("semantic_search: invalid params: `query` must be a non-empty string");
        }
        throw new Error(
          args.pattern === undefined || args.pattern === null
            ? "semantic_search: invalid params: `query` must be a non-empty string"
            : "semantic_search: invalid params: at least one of `query` or `pattern` must be a non-empty string",
        );
      }
      const query = hasQuery ? (args.query as string) : "";
      const permissionSubject = hasQuery ? query : (pattern as string);
      // The continuity key of a query-only search is unchanged; a pattern
      // makes a different search, so it gets its own key.
      const continuitySubject = pattern === undefined ? query : `${query}\u0000pattern:${pattern}`;
      const pathArg =
        typeof args.path === "string" && args.path.trim() ? args.path.trim() : undefined;
      const sessionProjectRoot = projectRootFor(context);

      // Auto routing may use either the indexed or grep-backed lane, so ask for
      // the aft_search permission before executing every search.
      const denied = await askSearchPermission(context, permissionSubject);
      if (denied) return permissionDeniedResponse(denied, context);

      const rawArgs: Record<string, unknown> = {};
      if (hasQuery) rawArgs.query = query;
      if (pattern !== undefined) rawArgs.pattern = pattern;
      const topK = coerceOptionalInt(args.topK, "topK", 1, 50);
      const offset = coerceOptionalInt(args.offset, "offset", 0, 100000);
      const includeTests = !isEmptyParam(args.includeTests)
        ? coerceBoolean(args.includeTests)
        : undefined;
      if (topK !== undefined) rawArgs.topK = topK;
      if (offset !== undefined) rawArgs.offset = offset;
      if (includeTests !== undefined) rawArgs.includeTests = includeTests;

      if (pathArg) {
        const externalDenied = await assertAftSearchExternalPermission(ctx, context, pathArg);
        if (externalDenied) {
          // A session rule denied this root, so it is refused, never answered
          // from another root: a count from the current project would read as
          // an answer about the path the caller named. When the query is
          // meaningful in the current project too, the refusal says how to
          // search there instead.
          if (
            externalDenied.kind === "rule_denied" &&
            hasMeaningfulProjectSearchFallback(permissionSubject, pathArg, externalDenied.root)
          ) {
            return permissionDeniedResponse(
              `${externalDenied.message}. ${deniedPathNotSearched(externalDenied.root)}`,
              context,
            );
          }
          return permissionDeniedResponse(externalDenied.message, context);
        }
        rawArgs.path = pathArg;
      }

      const response = await callToolCall(ctx, context, "search", rawArgs, {
        abortSignal: context.abort,
      });

      if (response.success === false) {
        const message =
          typeof response.text === "string" && response.text.length > 0
            ? response.text
            : typeof response.message === "string" && response.message.length > 0
              ? response.message
              : "semantic_search failed";
        throw new Error(message);
      }

      const selectedRoot = pathArg
        ? canonicalizeProjectRoot(resolvePathFromProjectRoot(sessionProjectRoot, pathArg))
        : sessionProjectRoot;
      return surfaceSearchText(
        response,
        continuity,
        context.sessionID ?? "unknown-session",
        continuityKey(selectedRoot, continuitySubject, includeTests, topK),
      );
    },
  };

  return {
    aft_search: searchTool,
  };
}
