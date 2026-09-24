/// <reference path="../bun-test.d.ts" />
import { describe, expect, test } from "bun:test";
import type { BridgePool } from "@cortexkit/aft-bridge";
import type { ToolDefinition } from "@opencode-ai/plugin";
import { tool } from "@opencode-ai/plugin";
import { astTools } from "../tools/ast.js";
import { createBashKillTool, createBashStatusTool, createBashTool } from "../tools/bash.js";
import { createBashWatchTool } from "../tools/bash_watch.js";
import { conflictTools } from "../tools/conflicts.js";
import { createReadTool, hoistedTools } from "../tools/hoisted.js";
import { navigationTools } from "../tools/navigation.js";
import { readingTools } from "../tools/reading.js";
import { safetyTools } from "../tools/safety.js";
import { searchTools } from "../tools/search.js";
import { semanticTools } from "../tools/semantic.js";
import type { PluginContext } from "../types.js";

const z = tool.schema;

/**
 * Contract test: every plugin-exported tool's `args` MUST be convertible
 * to JSON Schema via the host's `z.toJSONSchema()` call.
 *
 * This is the contract OpenCode actually exercises at session start —
 * `packages/opencode/src/tool/registry.ts` does:
 *
 *   z.toJSONSchema(z.object(args), { io: "input" })
 *
 * If any tool's args contain a Zod node the host's Zod can't represent
 * (`.transform()`, `.preprocess()`, certain coerce shapes), this call
 * throws "Transforms cannot be represented in JSON Schema" and OpenCode
 * fails to load the plugin entirely. Every session start dies, no tools
 * are registered.
 *
 * Historical regression this guards against: v0.30.1 commit 76583f5
 * introduced `optionalInt = z.any().transform(...).optional()` to handle
 * empty-sentinel coercion. Unit tests of the schema's runtime parse
 * behavior passed cleanly, but no test exercised the host-side JSON
 * Schema conversion, so the broken plugin shipped to dev. The user had
 * to debug with OpenCode's own agent before we caught it.
 *
 * If this test fails, the offending tool's args schema contains a node
 * that can't be represented in JSON Schema. Replace transforms with
 * plain schemas and move coercion into the tool handler.
 */

function makeStubCtx(): PluginContext {
  return {
    pool: {
      getBridge: () =>
        ({
          send: async () => ({ success: true }),
        }) as unknown as ReturnType<BridgePool["getBridge"]>,
    } as unknown as BridgePool,
    client: { lsp: {}, find: {} } as PluginContext["client"],
    config: { disabled_tools: [] } as PluginContext["config"],
    storageDir: "/tmp/aft-schema-test",
  };
}

function collectAllTools(ctx: PluginContext): Record<string, ToolDefinition> {
  return {
    ...astTools(ctx),
    ...conflictTools(ctx),
    bash: createBashTool(ctx),
    bash_status: createBashStatusTool(ctx),
    bash_kill: createBashKillTool(ctx),
    bash_watch: createBashWatchTool(ctx),
    read: createReadTool(ctx),
    ...hoistedTools(ctx),
    ...navigationTools(ctx),
    ...readingTools(ctx),
    ...safetyTools(ctx),
    ...searchTools(ctx),
    ...semanticTools(ctx),
  };
}

function expectRootObjectSchema(schema: unknown): void {
  const record = schema as Record<string, unknown>;
  expect(record.type).toBe("object");
  expect(record.anyOf).toBeUndefined();
  expect(record.oneOf).toBeUndefined();
  expect(record.allOf).toBeUndefined();
}

describe("tool args MUST be JSON-Schema-convertible by host Zod", () => {
  const ctx = makeStubCtx();
  const allTools = collectAllTools(ctx);
  const hoistedToolNames = new Set([...Object.keys(hoistedTools(ctx))]);
  const entries = Object.entries(allTools);

  test(`registers at least 10 tools (sanity)`, () => {
    expect(entries.length).toBeGreaterThanOrEqual(10);
  });

  for (const [toolName, def] of entries) {
    test(`${toolName} args convert to JSON Schema without throwing`, () => {
      // Mirror exactly what packages/opencode/src/tool/registry.ts does:
      // wrap args in a host z.object() and convert with io: "input".
      const wrapped = z.object(def.args);
      let jsonSchema: unknown;
      expect(() => {
        jsonSchema = z.toJSONSchema(wrapped, { io: "input" });
      }).not.toThrow();
      expect(jsonSchema).toBeDefined();
      // Sanity: conversion should produce an object schema.
      const schema = jsonSchema as Record<string, unknown>;
      expect(schema.type).toBe("object");
      if (hoistedToolNames.has(toolName)) {
        expectRootObjectSchema(schema);
      }
    });
  }
});
