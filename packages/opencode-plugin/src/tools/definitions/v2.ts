import type { ToolDefinition, ToolResult } from "@opencode-ai/plugin";
import { tool } from "@opencode-ai/plugin";
import { Effect } from "effect";

const V2_TOOL_NAME = /^[A-Za-z][A-Za-z0-9_-]{0,63}$/;
const V2_PATH_HEADER_TOOLS = new Set(["read", "write", "edit"]);
const V2_BASH_TOOLS = new Set(["bash", "aft_bash"]);
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
  /** The host rejects denied tool options before invoking the tool body. */
  nativePermissionOptions?: boolean;
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

function failure(error: unknown): Record<string, unknown> {
  return {
    _tag: "Tool.Error",
    message: error instanceof Error ? error.message : String(error),
    error,
  };
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
  const metadata = {
    ...(result.metadata ?? {}),
    ...(result.title ? { title: result.title } : {}),
  };
  return {
    content,
    ...(Object.keys(metadata).length ? { metadata } : {}),
  };
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
  permission: string | undefined,
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
        context.progress({
          ...(update.metadata ?? {}),
          ...(update.title ? { title: update.title } : {}),
        }),
      );
    },
    ask: (request) => {
      if (consumers.requestPermission) return consumers.requestPermission(request, context);
      if (consumers.nativePermissionOptions && request.permission === permission) {
        return Promise.resolve();
      }
      return Promise.reject(
        new Error("V2 permission requests require host-enforced tool permission options"),
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
          const runtime = runtimeFor(location, context, signal, consumers, permission);
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
