/// <reference path="../bun-test.d.ts" />

import { beforeEach, describe, expect, test } from "bun:test";
import * as fs from "node:fs";
import * as os from "node:os";
import * as path from "node:path";
import type { ToolDefinition } from "@earendil-works/pi-coding-agent";
import { discoverAndLoadExtensions, ExtensionRunner } from "@earendil-works/pi-coding-agent";
import { type TSchema, Type } from "typebox";
import type { AftConfig } from "../config.js";
import {
  detectPiHarness,
  resetRecordedExtensionApi,
  setHarnessOverrideForTesting,
} from "../harness.js";
import {
  bindToolRegistrationFunnel,
  registerPiToolSurface,
  resolvePiToolSurface,
} from "../tool-registration.js";
import {
  foldToolGuidanceIntoDescription,
  prepareToolDefinitionForRegistration,
} from "../tools/_shared.js";
import { makeMockApi, makeMockBridge, makePluginContext } from "./tool-test-utils.js";

beforeEach(() => {
  resetRecordedExtensionApi();
  setHarnessOverrideForTesting(null);
});

const sampleTool: ToolDefinition<TSchema> = {
  name: "aft_sample",
  label: "aft_sample",
  description: "A sample tool for testing registration.",
  promptSnippet: "Run the sample tool (supports mode)",
  promptGuidelines: ["Use aft_sample only for testing.", "Pass valid parameters."],
  parameters: Type.Object({
    mode: Type.Optional(Type.String()),
  }),
  execute: async () => ({ content: [{ type: "text", text: "ok" }] }),
};

describe("detectPiHarness", () => {
  test("identifies OMP from arktype on ExtensionAPI", () => {
    const api = { arktype: {}, registerTool: () => {} };
    expect(detectPiHarness(api)).toBe("omp");
  });

  test("identifies OMP from registerFileWriteFallback on ExtensionAPI", () => {
    const api = { registerFileWriteFallback: () => {}, registerTool: () => {} };
    expect(detectPiHarness(api)).toBe("omp");
  });

  // Upstream Pi's ExtensionAPI carries `events: EventBus` too
  // (pi-mono core/extensions/types.ts:1499), so it is not an OMP signal; an
  // upstream host shaped like the real one must still read as Pi.
  test("identifies upstream Pi when standard API methods and events exist without OMP members", () => {
    const api = { registerTool: () => {}, registerCommand: () => {}, events: {} };
    expect(detectPiHarness(api)).toBe("pi");
  });

  test("returns unknown when object lacks registration methods", () => {
    expect(detectPiHarness({})).toBe("unknown");
    expect(detectPiHarness(null)).toBe("unknown");
    expect(detectPiHarness(undefined)).toBe("unknown");
  });
});

describe("Registration funnel: 3 harnesses x 2 presentation modes", () => {
  test("registration funnel: omp x top_level attaches loadMode essential", () => {
    const prepared = prepareToolDefinitionForRegistration(sampleTool, "omp", "top_level");
    expect(prepared.loadMode).toBe("essential");
    expect(prepared.description).toBe(
      "A sample tool for testing registration.\n\nRun the sample tool (supports mode)\n- Use aft_sample only for testing.\n- Pass valid parameters.",
    );
    expect(prepared.promptSnippet).toBe(sampleTool.promptSnippet);
    expect(prepared.promptGuidelines).toEqual(sampleTool.promptGuidelines);
  });

  test("registration funnel: omp x host_default passes no loadMode", () => {
    const prepared = prepareToolDefinitionForRegistration(sampleTool, "omp", "host_default");
    expect(prepared.loadMode).toBeUndefined();
    expect(prepared.description).toBe(
      "A sample tool for testing registration.\n\nRun the sample tool (supports mode)\n- Use aft_sample only for testing.\n- Pass valid parameters.",
    );
    expect(prepared.promptSnippet).toBe(sampleTool.promptSnippet);
    expect(prepared.promptGuidelines).toEqual(sampleTool.promptGuidelines);
  });

  test("registration funnel: pi no double render (does not fold guidelines into description)", () => {
    const prepared = prepareToolDefinitionForRegistration(sampleTool, "pi", "top_level");
    expect(prepared.loadMode).toBeUndefined();
    expect(prepared.description).toBe("A sample tool for testing registration.");
    expect(prepared.promptSnippet).toBe(sampleTool.promptSnippet);
    expect(prepared.promptGuidelines).toEqual(sampleTool.promptGuidelines);
  });

  test("registration funnel: pi x host_default passes original definition", () => {
    const prepared = prepareToolDefinitionForRegistration(sampleTool, "pi", "host_default");
    expect(prepared.loadMode).toBeUndefined();
    expect(prepared.description).toBe("A sample tool for testing registration.");
    expect(prepared.promptSnippet).toBe(sampleTool.promptSnippet);
    expect(prepared.promptGuidelines).toEqual(sampleTool.promptGuidelines);
  });

  test("registration funnel: unknown x top_level behaves as pi", () => {
    const prepared = prepareToolDefinitionForRegistration(sampleTool, "unknown", "top_level");
    expect(prepared.loadMode).toBeUndefined();
    expect(prepared.description).toBe("A sample tool for testing registration.");
    expect(prepared.promptSnippet).toBe(sampleTool.promptSnippet);
    expect(prepared.promptGuidelines).toEqual(sampleTool.promptGuidelines);
  });

  test("registration funnel: unknown x host_default behaves as pi", () => {
    const prepared = prepareToolDefinitionForRegistration(sampleTool, "unknown", "host_default");
    expect(prepared.loadMode).toBeUndefined();
    expect(prepared.description).toBe("A sample tool for testing registration.");
    expect(prepared.promptSnippet).toBe(sampleTool.promptSnippet);
    expect(prepared.promptGuidelines).toEqual(sampleTool.promptGuidelines);
  });
});

describe("OMP description fold golden", () => {
  test("matches expected golden format: description, blank line, snippet, guidelines as - lines", () => {
    const description = "Execute shell commands.";
    const snippet = "Run shell commands (timeout in milliseconds)";
    const guidelines = [
      "DO NOT use bash for code search.",
      "Set compressed: false when you need raw output.",
    ];

    const folded = foldToolGuidanceIntoDescription(description, snippet, guidelines);
    const expectedGolden = [
      "Execute shell commands.",
      "",
      "Run shell commands (timeout in milliseconds)",
      "- DO NOT use bash for code search.",
      "- Set compressed: false when you need raw output.",
    ].join("\n");

    expect(folded).toBe(expectedGolden);
  });

  test("omits blank line if snippet and guidelines are absent", () => {
    expect(foldToolGuidanceIntoDescription("base description", undefined, undefined)).toBe(
      "base description",
    );
    expect(foldToolGuidanceIntoDescription("base description", "", [])).toBe("base description");
  });

  test("handles snippet without guidelines", () => {
    expect(foldToolGuidanceIntoDescription("base description", "a snippet", [])).toBe(
      "base description\n\na snippet",
    );
  });

  test("handles guidelines without snippet", () => {
    expect(
      foldToolGuidanceIntoDescription("base description", undefined, ["rule 1", "rule 2"]),
    ).toBe("base description\n\n- rule 1\n- rule 2");
  });
});

describe("Full surface registration funnel", () => {
  function registerWithHarness(
    harness: "pi" | "omp" | "unknown",
    config: AftConfig = { tool_surface: "recommended", search_index: true, bash: true },
  ) {
    const { api, tools } = makeMockApi();
    const { bridge, calls } = makeMockBridge(() => ({ success: true, text: "ok" }));
    const ctx = makePluginContext(bridge, { config });
    registerPiToolSurface(api, ctx, resolvePiToolSurface(config), harness);
    return { tools, calls };
  }

  test("OMP top_level registers every surface tool with loadMode essential and folded description", () => {
    const { tools } = registerWithHarness("omp", {
      tool_surface: "recommended",
      search_index: true,
      bash: true,
      pi: { tool_presentation: "top_level" },
    });

    expect(tools.size).toBeGreaterThan(0);
    for (const [name, tool] of tools) {
      const toolWithLoadMode = tool as unknown as { loadMode?: string };
      expect(toolWithLoadMode.loadMode).toBe(
        "essential",
        `Tool ${name} must carry loadMode: "essential" on OMP under top_level presentation`,
      );
    }

    // Check bash tool description has folded guidance
    const bashTool = tools.get("bash");
    expect(bashTool).toBeDefined();
    expect(bashTool?.description).toContain("Run shell commands (timeout in milliseconds");
    expect(bashTool?.description).toContain("- DO NOT use bash for code search or exploration");
  });

  test("OMP host_default leaves loadMode undefined on all tools", () => {
    const { tools } = registerWithHarness("omp", {
      tool_surface: "recommended",
      search_index: true,
      bash: true,
      pi: { tool_presentation: "host_default" },
    });

    expect(tools.size).toBeGreaterThan(0);
    for (const [name, tool] of tools) {
      const toolWithLoadMode = tool as unknown as { loadMode?: string };
      expect(toolWithLoadMode.loadMode).toBeUndefined(
        `Tool ${name} must not carry loadMode on OMP under host_default presentation`,
      );
    }
  });

  test("Pi harness leaves loadMode undefined and description unfolded", () => {
    const { tools } = registerWithHarness("pi", {
      tool_surface: "recommended",
      search_index: true,
      bash: true,
      pi: { tool_presentation: "top_level" },
    });

    expect(tools.size).toBeGreaterThan(0);
    for (const [, tool] of tools) {
      const toolWithLoadMode = tool as unknown as { loadMode?: string };
      expect(toolWithLoadMode.loadMode).toBeUndefined();
    }

    const bashTool = tools.get("bash");
    expect(bashTool?.description).not.toContain("- DO NOT use bash for code search or exploration");
  });
});

describe("bindToolRegistrationFunnel helper", () => {
  test("wraps pi.registerTool and is idempotent", () => {
    const { api, tools } = makeMockApi();
    const ctx = makePluginContext(makeMockBridge(() => ({ success: true, text: "ok" })).bridge);
    const bound = bindToolRegistrationFunnel(api, ctx, "omp");
    expect(bindToolRegistrationFunnel(bound, ctx, "omp")).toBe(bound);
    bound.registerTool(sampleTool);
    const registered = tools.get("aft_sample") as unknown as { loadMode?: string };
    expect(registered?.loadMode).toBe("essential");
  });
});

describe("Upstream Pi ExtensionAPI runtime tolerance", () => {
  test("upstream Pi ExtensionAPI registers tool carrying loadMode without rejection", async () => {
    const tmpDir = fs.mkdtempSync(path.join(os.tmpdir(), "pi-runtime-test-"));
    const extFile = path.join(tmpDir, "ext.js");
    fs.writeFileSync(
      extFile,
      `
export default function(api) {
  api.registerTool({
    name: 'test_omp_compat_tool',
    description: 'Tool carrying loadMode field',
    loadMode: 'essential',
    parameters: { type: 'object', properties: {} },
    execute: async () => ({ content: [{ type: 'text', text: 'success' }] }),
  });
}
`,
    );

    try {
      const { extensions, runtime } = await discoverAndLoadExtensions([extFile], tmpDir);
      const runner = new ExtensionRunner(extensions, runtime, tmpDir);
      const toolDef = runner.getToolDefinition("test_omp_compat_tool") as unknown as {
        name: string;
        loadMode?: string;
      };
      expect(toolDef).toBeDefined();
      expect(toolDef.name).toBe("test_omp_compat_tool");
      expect(toolDef.loadMode).toBe("essential");
    } finally {
      fs.rmSync(tmpDir, { recursive: true, force: true });
    }
  });
});
