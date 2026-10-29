import { describe, expect, test } from "bun:test";
import { DEFAULT_DISABLED_TOOLS, translateConfigDocument } from "@cortexkit/aft-bridge";
import { type ToolDefinition, tool } from "@opencode-ai/plugin";
import { Effect } from "effect";

import type { AftConfig } from "../../src/config.js";
import { normalizeToolArgSchemas } from "../../src/normalize-schemas.js";
import {
  buildAftToolDefinitions,
  registerAftTools,
  type V2ToolEditor,
} from "../../src/tool-registration.js";
import {
  projectV2Tool,
  providerDefinitionBytes,
  type V2ProviderTool,
  type V2ToolConsumers,
} from "../../src/tools/definitions/v2.js";
import type { PluginContext } from "../../src/types.js";
import { renameV2ToolReferences, v2HostToolName } from "../../src/v2-host-tool-names.js";

const ALL_TOOLS_CONFIG: AftConfig = {
  tool_surface: "all",
  hoist_builtin_tools: true,
  backup: { enabled: true },
  bash: true,
  search_index: true,
  semantic_search: true,
};
const LOCATION = {
  directory: "/work/project",
  project: { directory: "/work/project", canonical: "/work/canonical" },
};
const TOOL_NAME = /^[A-Za-z][A-Za-z0-9_-]{0,63}$/;
// V1 byte-identity of the governed artifacts is proven by the committed
// subc_tool_schemas.json drift guard and the surface audit, which compare the
// artifacts against a fresh generation; an in-process registration never
// touches those files, so re-reading them around it would assert nothing.

/**
 * Resolve a profile written with retired keys through the same
 * translation the loaders use, so registration sees a resolved config with a
 * present disabled_tools list, exactly as it does in production.
 */
function resolved(config: AftConfig): AftConfig {
  const doc = structuredClone(config) as Record<string, unknown>;
  translateConfigDocument(doc, "user");
  doc.disabled_tools ??= [...DEFAULT_DISABLED_TOOLS];
  return doc as AftConfig;
}

function stubContext(config: AftConfig = ALL_TOOLS_CONFIG): PluginContext {
  return {
    pool: {
      getBridge: () => {
        throw new Error("tool projection must not start a bridge");
      },
    },
    client: {},
    config,
    hashlineEffective: false,
    storageDir: "/isolated/storage",
  } as never;
}

function sharedDefinitions(config: AftConfig = ALL_TOOLS_CONFIG) {
  const resolvedConfig = resolved(config);
  return buildAftToolDefinitions(stubContext(resolvedConfig), resolvedConfig);
}

async function captureRegistration(config: AftConfig = ALL_TOOLS_CONFIG) {
  const added: V2ProviderTool[] = [];
  const removed: string[] = [];
  let updates = 0;
  let beforeHooks = 0;
  let afterHooks = 0;
  const context = {
    tool: {
      transform: (transform: (editor: V2ToolEditor) => void) =>
        Effect.sync(() => {
          transform({
            add: (definition) => added.push(definition),
            remove: (name) => removed.push(name),
            update: () => {
              updates += 1;
            },
          } as V2ToolEditor);
        }),
      execute: {
        before: () => {
          beforeHooks += 1;
        },
        after: () => {
          afterHooks += 1;
        },
      },
    },
  };

  await Effect.runPromise(
    registerAftTools(context, LOCATION, sharedDefinitions(config)) as Effect.Effect<void>,
  );
  return { added, removed, updates, beforeHooks, afterHooks };
}

function jsonSchema(definition: V2ProviderTool): Record<string, unknown> {
  return tool.schema.toJSONSchema(definition.input, { io: "input" }) as Record<string, unknown>;
}

function canonicalV1Schema(name: string, args: Record<string, unknown>): Record<string, unknown> {
  const schema = tool.schema.toJSONSchema(tool.schema.object(args), { io: "input" }) as Record<
    string,
    unknown
  >;
  const bare = name.startsWith("aft_") ? name.slice(4) : name;
  if (!["read", "write", "edit"].includes(bare)) return schema;
  const properties = schema.properties as Record<string, unknown>;
  if (!("filePath" in properties)) return schema;
  const { filePath, ...rest } = properties;
  return {
    ...schema,
    properties: { path: filePath, ...rest },
    required: (schema.required as string[] | undefined)?.map((key) =>
      key === "filePath" ? "path" : key,
    ),
  };
}

describe("OpenCode V2 tool surface", () => {
  test("registers every selected AFT definition as one non-CodeMode provider tool", async () => {
    const definitions = sharedDefinitions();
    const registration = await captureRegistration();

    expect(registration.added.map((definition) => definition.name)).toEqual(
      Object.keys(definitions).map(v2HostToolName),
    );
    for (const definition of registration.added) {
      expect(definition.name).toMatch(TOOL_NAME);
      expect(definition.options?.codemode).toBe(false);
      const schema = jsonSchema(definition);
      expect(schema.type).toBe("object");
      expect(schema).not.toHaveProperty("anyOf");
      expect(schema).not.toHaveProperty("oneOf");
      expect(schema).not.toHaveProperty("allOf");
    }
  });

  test("applies identical V1 and V2 config gates to the closed inventory", async () => {
    const profiles: AftConfig[] = [
      ALL_TOOLS_CONFIG,
      {
        tool_surface: "recommended",
        disabled_tools: ["aft_zoom"],
        backup: { enabled: false },
        bash: false,
        search_index: false,
        semantic_search: false,
        inspect: { enabled: false },
      },
      { tool_surface: "minimal", backup: { enabled: true }, bash: false },
    ];

    for (const config of profiles) {
      const definitions = sharedDefinitions(config);
      const { added } = await captureRegistration(config);
      expect(added.map((definition) => definition.name)).toEqual(
        Object.keys(definitions).map(v2HostToolName),
      );
    }
  });

  test("rejects provider-invalid names before registration", () => {
    const definition = Object.values(sharedDefinitions())[0] as ToolDefinition;
    expect(() => projectV2Tool("9invalid", definition, LOCATION)).toThrow("Invalid V2 tool name");
    expect(() => projectV2Tool(`a${"x".repeat(64)}`, definition, LOCATION)).toThrow(
      "Invalid V2 tool name",
    );
  });

  test("projects the same closed inventory and changes only V2 names and path headers", async () => {
    const definitions = sharedDefinitions();
    const { added } = await captureRegistration();
    const projected = new Map(added.map((definition) => [definition.name, definition]));

    // Besides the path header, OpenCode 2 differs only in naming: bash and
    // apply_patch register as shell and patch, and the text that refers to
    // them as tools says so (the rewrite itself is pinned in
    // v2-host-tool-names.test.ts).
    expect([...projected.keys()]).toEqual(Object.keys(definitions).map(v2HostToolName));
    for (const [name, definition] of Object.entries(definitions)) {
      const v2 = projected.get(v2HostToolName(name));
      const description = ["read", "write", "edit"].includes(name)
        ? definition.description.replaceAll("filePath", "path")
        : definition.description;
      expect(v2?.description).toBe(renameV2ToolReferences(description));
      expect(jsonSchema(v2 as V2ProviderTool)).toEqual(
        JSON.parse(
          renameV2ToolReferences(JSON.stringify(canonicalV1Schema(name, definition.args))),
        ),
      );
    }

    const v1Read = tool.schema.toJSONSchema(tool.schema.object(definitions.read.args), {
      io: "input",
    }) as Record<string, unknown>;
    expect(v1Read.properties).toHaveProperty("filePath");
    expect(jsonSchema(projected.get("read") as V2ProviderTool).properties).toHaveProperty("path");
    expect(jsonSchema(projected.get("read") as V2ProviderTool).properties).not.toHaveProperty(
      "filePath",
    );
    expect(projected.get("read")?.options?.permission).toBe("read");
    expect(projected.get("glob")?.options?.permission).toBe("glob");
    expect(projected.get("patch")?.options?.permission).toBe("edit");
    // OpenCode 2 evaluates shell-command rules under `shell`.
    expect(projected.get("shell")?.options?.permission).toBe("shell");
  });

  test("exposes described V2 arguments through Standard JSON Schema without changing validation", async () => {
    const definition: ToolDefinition = normalizeToolArgSchemas({
      description: "Read a filePath",
      args: {
        filePath: tool.schema
          .string()
          .min(1)
          .describe("File to read")
          .meta({
            examples: ["sample.txt"],
          }),
        limit: tool.schema.number().int().positive().optional().describe("Maximum lines"),
      },
      execute: async () => "ok",
    });
    const projected = projectV2Tool("read", definition, LOCATION);
    const input = projected.input;
    const standard = input["~standard"];

    expect(input).toBeInstanceOf(tool.schema.ZodObject);
    expect(standard.version).toBe(1);
    expect(standard.vendor).toBe("zod");
    const schema = standard.jsonSchema.input({ target: "draft-2020-12" });
    expect(schema.$schema).toBe("https://json-schema.org/draft/2020-12/schema");
    expect(schema.properties).toMatchObject({
      path: { type: "string", minLength: 1, description: "File to read", examples: ["sample.txt"] },
      limit: { type: "integer", description: "Maximum lines" },
    });
    expect(schema.properties).not.toHaveProperty("filePath");
    expect(schema.required).toEqual(["path"]);
    expect(schema).not.toHaveProperty("additionalProperties");
    expect(standard.jsonSchema.output({ target: "draft-2020-12" })).toEqual({
      ...schema,
      additionalProperties: false,
    });
    for (const target of ["draft-07", "openapi-3.0", "unsupported"]) {
      expect(() => standard.jsonSchema.input({ target })).toThrow(
        `Unsupported JSON Schema target: ${target}`,
      );
      expect(() => standard.jsonSchema.output({ target })).toThrow(
        `Unsupported JSON Schema target: ${target}`,
      );
    }

    const valid = { path: "sample.txt", limit: 3 };
    expect(input.parse(valid)).toEqual(valid);
    expect(await standard.validate(valid)).toEqual({ value: valid });
    for (const invalid of [{}, { path: "" }, { path: "sample.txt", limit: "3" }]) {
      expect(input.safeParse(invalid).success).toBe(false);
      expect(await standard.validate(invalid)).toHaveProperty("issues");
    }
    expect(definition.args).toHaveProperty("filePath");
    expect(definition.args).not.toHaveProperty("path");

    definition.args.limit = tool.schema.number().int().positive().describe("Required lines");
    const updated = projected.input["~standard"].jsonSchema.input({ target: "draft-2020-12" });
    expect(updated.required).toEqual(["path", "limit"]);
    expect(updated.properties).toHaveProperty("limit.description", "Required lines");
    expect(schema.required).toEqual(["path"]);
  });

  test("maps canonical V2 path input back to the shared executor field", async () => {
    let executedInput: Record<string, unknown> | undefined;
    const definition = sharedDefinitions().read;
    const projected = projectV2Tool(
      "read",
      {
        ...definition,
        execute: async (input) => {
          executedInput = input;
          return "ok";
        },
      },
      LOCATION,
    );

    await Effect.runPromise(
      projected.execute(
        { path: "sample.txt" },
        {
          sessionID: "session",
          messageID: "message",
          id: "call",
          agent: "agent",
          progress: () => Effect.void,
        },
      ),
    );
    expect(executedInput).toMatchObject({ filePath: "sample.txt" });
    expect(executedInput).not.toHaveProperty("path");
  });

  test("rejects permission asks when no evaluator is bound", async () => {
    let executed = false;
    const definition = {
      ...sharedDefinitions().read,
      execute: async (_input: unknown, context: { ask(request: unknown): Promise<void> }) => {
        await context.ask({ permission: "read", patterns: ["sample.txt"], always: ["*"] });
        executed = true;
        return "ok";
      },
    } as ToolDefinition;
    const executionContext = {
      sessionID: "session",
      messageID: "message",
      id: "call",
      agent: "agent",
      progress: () => Effect.void,
    };

    await expect(
      Effect.runPromise(
        projectV2Tool("read", definition, LOCATION).execute(
          { path: "sample.txt" },
          executionContext,
        ),
      ),
    ).rejects.toThrow(
      'The "read" operation was refused because this AFT runtime has no permission evaluator bound, so the host\'s permission rules for it could not be consulted.',
    );
    expect(executed).toBe(false);
  });

  test("keeps provider definition bytes stable without model-aware filtering", () => {
    const definitions = sharedDefinitions();
    const providers = ["openai", "anthropic", "gemini"] as const;
    const turnBytes = (provider: (typeof providers)[number]) =>
      Object.entries(definitions).map(([name, definition]) =>
        providerDefinitionBytes(provider, projectV2Tool(name, definition, LOCATION)),
      );

    for (const provider of providers) {
      const first = turnBytes(provider);
      const second = turnBytes(provider);
      expect(first, `${provider} first turn`).toEqual(second);
      expect(first.length, `${provider} inventory`).toBe(Object.keys(definitions).length);
    }
  });

  test("transform replacement only removes built-ins and never installs execution hooks", async () => {
    const registration = await captureRegistration();

    // apply_patch replaces the host's built-in under the name both use, `patch`.
    expect(registration.removed).toEqual(["read", "write", "edit", "patch"]);
    expect(registration.updates).toBe(0);
    expect(registration.beforeHooks).toBe(0);
    expect(registration.afterHooks).toBe(0);
  });

  test("routes permission and bash execution through the consumer seams", async () => {
    const permissionRequests: unknown[] = [];
    const permissionDefinition = {
      description: "permission probe",
      options: { permission: "read" },
      args: { value: tool.schema.string() },
      execute: async (input, context) => {
        await context.ask({
          permission: "read",
          patterns: [String(input.value)],
          always: ["*"],
          metadata: {},
        });
        return `allowed:${String(input.value)}`;
      },
    } as ToolDefinition & { options: { permission: string } };
    const bashDefinition: ToolDefinition = {
      description: "bash consumer probe",
      args: { command: tool.schema.string() },
      execute: async () => {
        throw new Error("the V1 bash executor must not run when the V2 consumer is installed");
      },
    };
    const executionContext = {
      sessionID: "session-v2",
      messageID: "message-v2",
      agent: "agent-v2",
      progress: () => Effect.succeed(undefined),
    };
    const consumers: V2ToolConsumers = {
      requestPermission: async (request) => {
        permissionRequests.push(request);
      },
      executeBash: async ({ input }) => `host-bash:${String(input.command)}`,
    };

    const permissionTool = projectV2Tool("aft_probe", permissionDefinition, LOCATION, consumers);
    const permissionResult = await Effect.runPromise(
      permissionTool.execute({ value: "src/index.ts" }, executionContext),
    );
    const bashResult = await Effect.runPromise(
      projectV2Tool("bash", bashDefinition, LOCATION, consumers).execute(
        { command: "pwd" },
        executionContext,
      ),
    );

    expect(permissionTool.options).toEqual({ permission: "read", codemode: false });
    expect(permissionRequests).toEqual([
      { permission: "read", patterns: ["src/index.ts"], always: ["*"], metadata: {} },
    ]);
    expect(permissionResult).toEqual({ content: "allowed:src/index.ts" });
    expect(bashResult).toEqual({ content: "host-bash:pwd" });
  });
});
