import type { ToolDefinition, ToolResult } from "@opencode-ai/plugin";
import { tool } from "@opencode-ai/plugin";
import { Effect } from "effect";

const V2_TOOL_NAME = /^[A-Za-z][A-Za-z0-9_-]{0,63}$/;
const V2_PATH_HEADER_TOOLS = new Set(["read", "write", "edit"]);
const V2_BASH_TOOLS = new Set(["bash"]);
const ROOT_COMBINATORS = ["anyOf", "oneOf", "allOf"] as const;

export interface V2Location {
  directory: string;
  project?: {
    directory?: string;
    canonical?: string;
  };
}

export interface V2ExecutionContext {
  sessionID?: string;
  messageID?: string;
  agent?: string;
  /**
   * The host's identifier for this tool call. OpenCode 2 always sends it and a
   * permission request has to quote it back as its source, but it is optional
   * here because AFT's own fixtures build contexts without one.
   */
  id?: string;
  progress(update: Record<string, unknown>): Effect.Effect<void>;
}

export interface V2PermissionRequest {
  permission: string;
  patterns: string[];
  always: string[];
  metadata: Record<string, unknown>;
}

export interface V2DefinitionRuntime extends V2ExecutionContext {
  directory: string;
  effectAbort: AbortSignal;
  worktree: string;
  abort: AbortSignal;
  metadata(update: Record<string, unknown>): void;
  ask(request: V2PermissionRequest): Promise<void>;
}

export interface V2BashExecution {
  name: "bash" | "aft_bash";
  input: Record<string, unknown>;
  context: V2DefinitionRuntime;
  definition: ToolDefinition;
}

export interface V2ToolConsumers {
  /** Maps legacy tool permission requests to the V2 session permission service. */
  requestPermission?: (request: V2PermissionRequest, context: V2ExecutionContext) => Promise<void>;
  /** Runs bash through the dedicated V2 executor while preserving the shared schema. */
  executeBash?: (execution: V2BashExecution) => Promise<ToolResult>;
}

export type V2Provider = "openai" | "anthropic" | "gemini";

export interface V2ProviderTool {
  name: string;
  description: string;
  input: ReturnType<typeof tool.schema.object>;
  options: Readonly<Record<string, unknown>> & {
    codemode: false;
  };
  execute(
    input: Record<string, unknown>,
    context: V2ExecutionContext,
  ): Effect.Effect<Record<string, unknown>, unknown>;
}

function failure(error: unknown): Error {
  if (error instanceof Error) return error;
  const detail = (() => {
    try {
      return JSON.stringify(error) ?? String(error);
    } catch {
      return String(error);
    }
  })();
  return new Error(`V2 tool execution rejected with a non-Error value: ${detail}`);
}

function isPlainObject(value: unknown): value is Record<string, unknown> {
  if (typeof value !== "object" || value === null) return false;
  const prototype = Object.getPrototypeOf(value);
  return prototype === Object.prototype || prototype === null;
}

/**
 * Drop the metadata values the host has no way to store.
 *
 * A tool part's metadata is held as a record of JSON values, and `undefined`
 * is not one of them. A key that is PRESENT but undefined therefore fails the
 * whole record: the part never reaches its completed state, and the model is
 * handed the host's `Tool result missing` placeholder in place of the output.
 * The tool itself ran and its side effects landed, so nothing on either side
 * reports a problem — the output simply disappears.
 *
 * Tools assemble metadata out of optional fields, and writing
 * `description: maybeUndefined` is the obvious way to express "the caller did
 * not give me one". Cleaning that up here, at the single seam where our
 * metadata crosses into the host, fixes it for every tool at once; doing it
 * key by key in each tool would make silence the penalty for forgetting.
 *
 * An absent key is exactly what an absent optional field means, so undefined
 * keys are dropped. An array has no absent element, so an undefined entry
 * becomes null, which is what writing the same array out as JSON would give.
 * Values that are neither — a Date, say — are left alone rather than mangled
 * into something that merely looks storable.
 */
function jsonSafeMetadata(value: unknown): unknown {
  if (Array.isArray(value)) {
    return value.map((entry) => (entry === undefined ? null : jsonSafeMetadata(entry)));
  }
  if (!isPlainObject(value)) return value;
  const cleaned: Record<string, unknown> = {};
  for (const [key, entry] of Object.entries(value)) {
    if (entry === undefined) continue;
    cleaned[key] = jsonSafeMetadata(entry);
  }
  return cleaned;
}

function resultContent(result: ToolResult): Record<string, unknown> {
  if (typeof result === "string") return { content: result };

  const attachments = (result.attachments ?? []).map((attachment) => ({
    type: "file",
    uri: attachment.url,
    mime: attachment.mime,
    ...(attachment.filename ? { name: attachment.filename } : {}),
  }));
  const content = attachments.length
    ? [{ type: "text", text: result.output }, ...attachments]
    : result.output;
  const metadata = jsonSafeMetadata({
    ...(result.metadata ?? {}),
    ...(result.title ? { title: result.title } : {}),
  }) as Record<string, unknown>;
  return {
    content,
    ...(Object.keys(metadata).length ? { metadata } : {}),
  };
}

/**
 * Pull the human-readable message out of a serialized failure envelope.
 *
 * Several tools report a refusal by *returning* `{"success": false, ...}` as
 * their text instead of throwing. Left alone that reaches the host as a
 * successful call whose content is raw JSON, so a transcript shows a check mark
 * over a result that does not exist, and the reader has to parse JSON to learn
 * it failed. Recognising the envelope here turns it back into a failure that
 * carries only the rendered sentence.
 *
 * The match is deliberately narrow — a top-level object with `success: false`
 * — so ordinary tool output that merely happens to be JSON is untouched.
 */
function failureEnvelopeMessage(result: ToolResult): string | undefined {
  const output = typeof result === "string" ? result : result.output;
  if (typeof output !== "string") return undefined;
  const trimmed = output.trim();
  if (!trimmed.startsWith("{") || !trimmed.includes('"success"')) return undefined;

  let parsed: unknown;
  try {
    parsed = JSON.parse(trimmed);
  } catch {
    return undefined;
  }
  if (!parsed || typeof parsed !== "object" || Array.isArray(parsed)) return undefined;

  const envelope = parsed as Record<string, unknown>;
  if (envelope.success !== false) return undefined;
  for (const field of [envelope.message, envelope.error, envelope.code]) {
    if (typeof field === "string" && field.length > 0) return field;
  }
  return "The tool call failed.";
}

function bareToolName(name: string): string {
  return name.startsWith("aft_") ? name.slice(4) : name;
}

/**
 * V1 keeps OpenCode's historical `filePath` header key. V2's renderers read the
 * canonical `path` key, so the V2 root argument map renames that field while
 * reusing its schema node. The shared definition itself remains unchanged.
 */
function projectArguments(name: string, definition: ToolDefinition): ToolDefinition["args"] {
  if (!V2_PATH_HEADER_TOOLS.has(bareToolName(name)) || !("filePath" in definition.args)) {
    return { ...definition.args };
  }

  const { filePath, ...rest } = definition.args;
  return { path: filePath, ...rest };
}

function executionArguments(name: string, input: Record<string, unknown>): Record<string, unknown> {
  if (!V2_PATH_HEADER_TOOLS.has(bareToolName(name)) || !("path" in input)) return input;
  const { path, ...rest } = input;
  return { filePath: path, ...rest };
}

function hostPermission(name: string): string | undefined {
  const bare = bareToolName(name);
  if (new Set(["read", "glob", "grep"]).has(bare)) return bare;
  if (V2_BASH_TOOLS.has(name)) return "bash";
  if (
    new Set([
      "write",
      "edit",
      "apply_patch",
      "delete",
      "move",
      "ast_grep_replace",
      "import",
      "safety",
    ]).has(bare)
  ) {
    return "edit";
  }
  return undefined;
}

function assertV2Contract(name: string, input: ReturnType<typeof tool.schema.object>): void {
  if (!V2_TOOL_NAME.test(name)) {
    throw new Error(
      `Invalid V2 tool name ${JSON.stringify(name)}; expected ${V2_TOOL_NAME.source}`,
    );
  }

  const schema = tool.schema.toJSONSchema(input, { io: "input" }) as Record<string, unknown>;
  if (schema.type !== "object") {
    throw new Error(`V2 tool ${name} must have an object-rooted input schema`);
  }
  for (const keyword of ROOT_COMBINATORS) {
    if (keyword in schema) {
      throw new Error(`V2 tool ${name} input schema cannot use root ${keyword}`);
    }
  }
}

function runtimeFor(
  location: V2Location,
  context: V2ExecutionContext,
  signal: AbortSignal,
  consumers: V2ToolConsumers,
): V2DefinitionRuntime {
  const directory = location.directory;
  const worktree = location.project?.canonical ?? location.project?.directory ?? directory;
  return {
    sessionID: context.sessionID,
    messageID: context.messageID,
    agent: context.agent,
    directory,
    worktree,
    abort: signal,
    effectAbort: signal,
    metadata: (update) => {
      void Effect.runPromise(
        context.progress(
          jsonSafeMetadata({
            ...(update.metadata ?? {}),
            ...(update.title ? { title: update.title } : {}),
          }) as Record<string, unknown>,
        ),
      );
    },
    ask: (request) => {
      if (consumers.requestPermission) return consumers.requestPermission(request, context);
      // Only our own wiring is observable here, so say that and nothing more:
      // the previous wording blamed the host for a missing endpoint it was
      // never asked for.
      return Promise.reject(
        new Error(
          `The "${request.permission}" operation was refused because this AFT runtime has no permission evaluator bound, so the host's permission rules for it could not be consulted.`,
        ),
      );
    },
    progress: context.progress,
  };
}

/** Project one shared V1 definition into the V2 provider-tool contract. */
export function projectV2Tool(
  name: string,
  definition: ToolDefinition,
  location: V2Location,
  consumers: V2ToolConsumers = {},
): V2ProviderTool {
  const input = tool.schema.object(projectArguments(name, definition));
  assertV2Contract(name, input);
  const sharedOptions = (definition as ToolDefinition & { options?: Record<string, unknown> })
    .options;
  const permission = hostPermission(name);

  return {
    name,
    description: definition.description,
    input,
    options: {
      ...sharedOptions,
      ...(permission ? { permission } : {}),
      codemode: false,
    },
    execute: (rawInput, context) =>
      Effect.tryPromise({
        try: async (signal) => {
          const runtime = runtimeFor(location, context, signal, consumers);
          const input = executionArguments(name, rawInput);
          const result =
            consumers.executeBash && V2_BASH_TOOLS.has(name)
              ? await consumers.executeBash({
                  name: name as "bash" | "aft_bash",
                  input,
                  context: runtime,
                  definition,
                })
              : await definition.execute(input, runtime as never);
          const refusal = failureEnvelopeMessage(result);
          if (refusal) throw new Error(refusal);
          return resultContent(result);
        },
        catch: failure,
      }),
  };
}

/** Serialize provider-specific JSON envelopes for deterministic fixture comparisons. */
export function providerDefinitionBytes(provider: V2Provider, definition: V2ProviderTool): string {
  const input = tool.schema.toJSONSchema(definition.input, { io: "input" });
  if (provider === "openai") {
    return JSON.stringify({
      type: "function",
      function: {
        name: definition.name,
        description: definition.description,
        parameters: input,
      },
    });
  }
  if (provider === "anthropic") {
    return JSON.stringify({
      name: definition.name,
      description: definition.description,
      input_schema: input,
    });
  }
  return JSON.stringify({
    functionDeclarations: [
      { name: definition.name, description: definition.description, parameters: input },
    ],
  });
}
