import { describe, expect, test } from "bun:test";
import { type ToolDefinition, tool } from "@opencode-ai/plugin";
import { Effect } from "effect";

import type { AftConfig } from "../../src/config.js";
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
  return buildAftToolDefinitions(stubContext(config), config);
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
      Object.keys(definitions),
    );
    for (const definition of registration.added) {
      expect(definition.name).toMatch(TOOL_NAME);
      expect(definition.options).toEqual({ codemode: false });
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
      expect(added.map((definition) => definition.name)).toEqual(Object.keys(definitions));
    }
  });

  test("rejects provider-invalid names before registration", () => {
    const definition = Object.values(sharedDefinitions())[0] as ToolDefinition;
    expect(() => projectV2Tool("9invalid", definition, LOCATION)).toThrow("Invalid V2 tool name");
    expect(() => projectV2Tool(`a${"x".repeat(64)}`, definition, LOCATION)).toThrow(
      "Invalid V2 tool name",
    );
  });

  test("projects the same closed inventory and changes only V2 path headers", async () => {
    const definitions = sharedDefinitions();
    const { added } = await captureRegistration();
    const projected = new Map(added.map((definition) => [definition.name, definition]));

    expect([...projected.keys()]).toEqual(Object.keys(definitions));
    for (const [name, definition] of Object.entries(definitions)) {
      const v2 = projected.get(name);
      expect(v2?.description).toBe(definition.description);
      expect(jsonSchema(v2 as V2ProviderTool)).toEqual(canonicalV1Schema(name, definition.args));
    }

    const v1Read = tool.schema.toJSONSchema(tool.schema.object(definitions.read.args), {
      io: "input",
    }) as Record<string, unknown>;
    expect(v1Read.properties).toHaveProperty("filePath");
    expect(jsonSchema(projected.get("read") as V2ProviderTool).properties).toHaveProperty("path");
    expect(jsonSchema(projected.get("read") as V2ProviderTool).properties).not.toHaveProperty(
      "filePath",
    );
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

    expect(registration.removed).toEqual(["read", "write", "edit", "apply_patch"]);
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
