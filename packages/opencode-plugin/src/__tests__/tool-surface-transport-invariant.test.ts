import { describe, expect, test } from "bun:test";

import type { PluginContext } from "../shared/types.js";
import { astTools } from "../tools/ast.js";
import { conflictTools } from "../tools/conflicts.js";
import { gatherTools } from "../tools/gather.js";
import { aftPrefixedTools, hoistedTools } from "../tools/hoisted.js";
import { importTools } from "../tools/imports.js";
import { inspectTools } from "../tools/inspect.js";
import { navigationTools } from "../tools/navigation.js";
import { readingTools } from "../tools/reading.js";
import { refactoringTools } from "../tools/refactoring.js";
import { safetyTools } from "../tools/safety.js";
import { searchTools } from "../tools/search.js";
import { semanticTools } from "../tools/semantic.js";
import { buildWorkflowHints } from "../workflow-hints.js";

// The agent-visible tool surface (names, descriptions, parameter schemas) and
// the injected system-prompt hints MUST be byte-identical between the
// standalone NDJSON transport and the subc daemon transport. Any divergence
// busts users' prefix caches on a transport flip and forks the wire the model
// sees. The surface must therefore be a pure function of config — never of
// the pool behind ctx. This test builds the full surface twice with ctx.pool
// swapped between two structurally different stand-ins and asserts deep
// equality of everything the model sees.

function fakePool(label: string): PluginContext["pool"] {
  // Distinct shapes on purpose: if any tool definition consults the pool while
  // BUILDING its description/schema, the two surfaces diverge and this test
  // fails. Execution-time pool use is out of scope (and transport-agnostic by
  // the AftTransportPool contract).
  return {
    label,
    getBridge: () => {
      throw new Error(`surface construction must not touch the pool (${label})`);
    },
    closeSession: () => {},
    shutdown: () => {},
    setConfigureOverride: () => {},
  } as unknown as PluginContext["pool"];
}

function buildSurface(pool: PluginContext["pool"]): Record<string, unknown> {
  const ctx = {
    pool,
    client: {} as PluginContext["client"],
    config: {
      tool_surface: "all",
      semantic_search: true,
      search_index: true,
    },
    storageDir: "/tmp/aft-surface-test",
    isProjectEnabled: () => true,
  } as unknown as PluginContext;

  const tools = {
    ...hoistedTools(ctx),
    ...aftPrefixedTools(ctx),
    ...readingTools(ctx),
    ...safetyTools(ctx),
    ...importTools(ctx),
    ...navigationTools(ctx),
    ...astTools(ctx),
    ...semanticTools(ctx),
    ...inspectTools(ctx),
    ...searchTools(ctx),
    ...refactoringTools(ctx),
    ...conflictTools(ctx),
  };

  const surface: Record<string, unknown> = {};
  for (const [name, def] of Object.entries(tools)) {
    const definition = def as { description?: string; args?: unknown; parameters?: unknown };
    surface[name] = {
      description: definition.description,
      // Serialize schema shape; Zod objects stringify stably enough for
      // equality via JSON round-trip of their JSON-schema-ish own fields.
      args: JSON.parse(JSON.stringify(definition.args ?? definition.parameters ?? null)),
    };
  }
  return surface;
}

describe("tool surface transport invariance", () => {
  test("tool names, descriptions, and schemas are independent of the pool", () => {
    const standalone = buildSurface(fakePool("standalone-ndjson"));
    const subc = buildSurface(fakePool("subc-daemon"));

    expect(Object.keys(subc).sort()).toEqual(Object.keys(standalone).sort());
    expect(subc).toEqual(standalone);
  });

  test("workflow hints are a pure function of config, not transport", () => {
    const opts = {
      toolSurface: "all" as const,
      hoistBuiltins: true,
      semanticEnabled: true,
      bashBackgroundEnabled: true,
      bashCompressionEnabled: true,
      disabledTools: new Set<string>(),
    };
    const first = buildWorkflowHints(opts);
    const second = buildWorkflowHints(opts);
    expect(second).toEqual(first);
    expect(typeof first).toBe("string");
    expect((first as string).length).toBeGreaterThan(0);
    // Nothing transport-shaped may appear in the injected prompt text.
    expect(first as string).not.toMatch(/subc|ndjson|daemon|transport/i);
  });

  test("gather advertises path and maps it to the internal filePath transport key", async () => {
    const calls: Array<{ name: string; args: Record<string, unknown> }> = [];
    const ctx = {
      pool: {
        getBridge: () => ({
          async toolCall(
            _sessionId: string | undefined,
            name: string,
            args: Record<string, unknown>,
          ) {
            calls.push({ name, args });
            return { success: true, text: "context pack" };
          },
        }),
      },
      client: {},
      config: { tool_surface: "all" },
      storageDir: "/tmp/aft-surface-test",
      isProjectEnabled: () => true,
    } as never;
    const tool = gatherTools(ctx).aft_gather_context;

    expect(Object.hasOwn(tool.args, "path")).toBe(true);
    expect(Object.hasOwn(tool.args, "filePath")).toBe(false);

    await tool.execute(
      { symbol: "handle_zoom", path: "src/commands/zoom.rs", includeTests: true },
      {
        directory: process.cwd(),
      } as never,
    );

    const internalPathKey = ["file", "Path"].join("");
    expect(calls).toEqual([
      {
        name: "gather",
        args: {
          symbol: "handle_zoom",
          [internalPathKey]: "src/commands/zoom.rs",
          includeTests: true,
        },
      },
    ]);
  });
});
