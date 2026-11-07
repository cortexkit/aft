/// <reference path="../bun-test.d.ts" />

import { afterEach, beforeEach, describe, expect, test } from "bun:test";
import * as fs from "node:fs";
import * as os from "node:os";
import * as path from "node:path";
import type { ExtensionAPI, ToolDefinition } from "@earendil-works/pi-coding-agent";
import { discoverAndLoadExtensions, ExtensionRunner } from "@earendil-works/pi-coding-agent";
import { type TSchema, Type } from "typebox";
import type { AftConfig } from "../config.js";
import { registerPiConfigErrorState } from "../config-error-state.js";
import {
  detectPiHarness,
  resetRecordedExtensionApi,
  setHarnessOverrideForTesting,
} from "../harness.js";
import { OMP_TOOL_TIERS } from "../omp-approval.js";
import { type OmpInternalUrlRouter, projectAftCall } from "../omp-internal-urls.js";
import {
  bindToolRegistrationFunnel,
  registerAftBinding,
  registerLatePowershellTool,
  registerPiToolSurface,
  reregisterBashTool,
  resolvePiToolSurface,
  TOOL_SOURCE_PATH,
} from "../tool-registration.js";
import {
  foldToolGuidanceIntoDescription,
  prepareToolDefinitionForRegistration,
} from "../tools/_shared.js";
import type { PluginContext } from "../types.js";
import {
  makeExtContext,
  makeMockApi,
  makeMockBridge,
  makePluginContext,
} from "./tool-test-utils.js";

beforeEach(() => {
  resetRecordedExtensionApi();
  setHarnessOverrideForTesting(null);
});

const temporaryProjects: string[] = [];

afterEach(() => {
  setHarnessOverrideForTesting(null);
  for (const root of temporaryProjects.splice(0)) fs.rmSync(root, { recursive: true, force: true });
});

function tmpProjectDir(): string {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), "aft-projection-test-"));
  temporaryProjects.push(root);
  return root;
}

/** PluginContext over a mock bridge; most projections only need the context. */
function projectCtx(
  handler?: Parameters<typeof makeMockBridge>[0],
  overrides: Partial<PluginContext> = {},
) {
  return makePluginContext(makeMockBridge(handler).bridge, overrides);
}

interface MockHostApi {
  registerTool: () => void;
  on: (event: string, handler: (...args: unknown[]) => void) => void;
  events: {
    on: (event: string, handler: (payload: unknown) => void) => () => void;
    emit: (event: string, payload: unknown) => void;
  };
  getAllTools?: () => Array<{ name: string; sourceInfo?: { path?: string } }>;
  getActiveTools?: () => string[];
}

/** Mock OMP host: `pi.on` lifecycle hooks plus the synchronous extension event bus. */
function makeMockHost(overrides: Partial<MockHostApi> = {}) {
  const busHandlers = new Map<string, Array<(payload: unknown) => void>>();
  const lifecycleHandlers = new Map<string, Array<(...args: unknown[]) => void>>();
  const api: MockHostApi = {
    registerTool: () => {},
    getAllTools: () => [],
    getActiveTools: () => (overrides.getAllTools?.() ?? []).map(({ name }) => name),
    on: (event, handler) => {
      const list = lifecycleHandlers.get(event) ?? [];
      list.push(handler);
      lifecycleHandlers.set(event, list);
    },
    events: {
      on: (event, handler) => {
        const list = busHandlers.get(event) ?? [];
        list.push(handler);
        busHandlers.set(event, list);
        return () =>
          busHandlers.set(
            event,
            (busHandlers.get(event) ?? []).filter((h) => h !== handler),
          );
      },
      emit: (event, payload) => {
        for (const handler of busHandlers.get(event) ?? []) handler(payload);
      },
    },
    ...overrides,
  };
  return {
    api: api as unknown as ExtensionAPI,
    emitLifecycle: (event: string, ...args: unknown[]) => {
      for (const handler of lifecycleHandlers.get(event) ?? []) handler(...args);
    },
    emitBus: (event: string, payload: unknown) => api.events.emit(event, payload),
    registeredLifecycleEvents: () => [...lifecycleHandlers.keys()],
  };
}

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
    config: AftConfig = { disabled_tools: [], bash: true },
  ) {
    const { api, tools } = makeMockApi();
    const { bridge, calls } = makeMockBridge(() => ({ success: true, text: "ok" }));
    const ctx = makePluginContext(bridge, { config });
    registerPiToolSurface(api, ctx, resolvePiToolSurface(config), harness);
    return { tools, calls };
  }

  test("OMP top_level registers every surface tool with loadMode essential and folded description", () => {
    const config: AftConfig = {
      disabled_tools: [],
      bash: true,
      pi: { tool_presentation: "top_level" },
    };
    const { api, tools } = makeMockApi();
    // Shaped like OMP's ExtensionAPI, so a registration that detects the host
    // itself (as the late session_start one does) sees OMP.
    Object.assign(api, { arktype: {} });
    const ctx = makePluginContext(makeMockBridge(() => ({ success: true, text: "ok" })).bridge, {
      config,
    });
    registerPiToolSurface(api, ctx, resolvePiToolSurface(config), "omp");
    // What index.ts does at session_start once Pi reveals PowerShell.
    expect(tools.has("powershell")).toBe(false);
    registerLatePowershellTool(api, ctx, false);
    expect(tools.has("powershell")).toBe(true);

    expect(tools.size).toBeGreaterThan(0);
    for (const [name, tool] of tools) {
      const registered = tool as unknown as {
        loadMode?: string;
        approval?: (args: unknown) => unknown;
      };
      expect(registered.loadMode).toBe(
        "essential",
        `Tool ${name} must carry loadMode: "essential" on OMP under top_level presentation`,
      );
      expect(typeof registered.approval).toBe("function");
      expect(registered.approval?.({})).toBe(OMP_TOOL_TIERS[name]);
    }
    expect((tools.get("powershell") as unknown as { approval: () => unknown }).approval()).toBe(
      "exec",
    );

    // Check bash tool description has folded guidance
    const bashTool = tools.get("bash");
    expect(bashTool).toBeDefined();
    expect(bashTool?.description).toContain("Run shell commands (timeout in milliseconds");
    expect(bashTool?.description).toContain("- DO NOT use bash for code search or exploration");
  });

  test("OMP keeps loadMode essential and the exec tier on bash re-registered by a live config reload", () => {
    const config: AftConfig = { disabled_tools: [], bash: true };
    const { api, tools } = makeMockApi();
    Object.assign(api, { arktype: {} });
    const ctx = makePluginContext(makeMockBridge(() => ({ success: true, text: "ok" })).bridge, {
      config,
    });
    registerPiToolSurface(api, ctx, resolvePiToolSurface(config), "omp");
    const first = tools.get("bash");
    // What index.ts does when a reload flips bash.runon_enabled.
    reregisterBashTool(api, ctx, false);
    const replaced = tools.get("bash") as unknown as {
      loadMode?: string;
      approval?: (args: unknown) => unknown;
    };
    expect(replaced).not.toBe(first);
    expect(replaced.loadMode).toBe("essential");
    expect(replaced.approval?.({ command: "ls" })).toBe("exec");
  });

  test("OMP config-error-state tools still go through the funnel", () => {
    const config: AftConfig = { disabled_tools: [], bash: true };
    const { api, tools } = makeMockApi();
    Object.assign(api, { arktype: {}, on: () => {} });
    registerPiConfigErrorState(api, config, "broken config");
    expect(tools.size).toBeGreaterThan(0);
    for (const [name, tool] of tools) {
      const registered = tool as unknown as {
        loadMode?: string;
        approval?: (args: unknown) => unknown;
      };
      expect(registered.loadMode).toBe("essential", name);
      expect(registered.approval?.({})).toBe(OMP_TOOL_TIERS[name]);
    }
  });

  test("OMP host_default leaves loadMode undefined on all tools", () => {
    const { tools } = registerWithHarness("omp", {
      disabled_tools: [],
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
      disabled_tools: [],
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

describe("registerAftBinding — harness gating", () => {
  test("does not subscribe outside the OMP harness", () => {
    setHarnessOverrideForTesting("pi");
    const { api, emitLifecycle, emitBus } = makeMockHost();
    let answered = false;
    api.events.on("aft:binding", () => {
      answered = true;
    });
    registerAftBinding(api, projectCtx());
    emitLifecycle("session_start", undefined, makeExtContext(process.cwd(), "s1"));
    emitBus("aft:binding-request", { v: 1, sessionId: "s1", requestId: "r1" });
    expect(answered).toBe(false);
  });

  test("never registers a tool_call handler (it would disable OMP's speculative batch reads/tasks)", () => {
    setHarnessOverrideForTesting("omp");
    const { api, registeredLifecycleEvents } = makeMockHost();
    registerAftBinding(api, projectCtx());
    expect(registeredLifecycleEvents()).not.toContain("tool_call");
  });
});

describe("registerAftBinding — request admission (OMP)", () => {
  function setup(toolEntries: Array<{ name: string; sourceInfo?: { path?: string } }>) {
    setHarnessOverrideForTesting("omp");
    const { api, emitLifecycle, emitBus } = makeMockHost({
      getAllTools: () => toolEntries,
      getActiveTools: () => toolEntries.map((entry) => entry.name),
    });
    const responses: unknown[] = [];
    api.events.on("aft:binding", (payload) => responses.push(payload));
    registerAftBinding(api, projectCtx());
    emitLifecycle("session_start", undefined, makeExtContext(process.cwd(), "session-1"));
    return { emitBus, responses };
  }

  test("ignores a request with the wrong version", () => {
    const { emitBus, responses } = setup([
      { name: "read", sourceInfo: { path: TOOL_SOURCE_PATH } },
    ]);
    emitBus("aft:binding-request", { v: 2, sessionId: "session-1", requestId: "r1" });
    expect(responses).toEqual([]);
  });

  test("ignores a request for a different session", () => {
    const { emitBus, responses } = setup([
      { name: "read", sourceInfo: { path: TOOL_SOURCE_PATH } },
    ]);
    emitBus("aft:binding-request", { v: 1, sessionId: "other-session", requestId: "r1" });
    expect(responses).toEqual([]);
  });

  test("ignores a request missing requestId", () => {
    const { emitBus, responses } = setup([
      { name: "read", sourceInfo: { path: TOOL_SOURCE_PATH } },
    ]);
    emitBus("aft:binding-request", { v: 1, sessionId: "session-1" });
    expect(responses).toEqual([]);
  });

  test("answers a well-formed request with only the live, exact-source tools", () => {
    const { emitBus, responses } = setup([
      { name: "read", sourceInfo: { path: TOOL_SOURCE_PATH } },
      { name: "aft_outline", sourceInfo: { path: TOOL_SOURCE_PATH } },
      // A same-named tool another extension registered later: excluded,
      // even though the name is one AFT would otherwise report.
      { name: "edit", sourceInfo: { path: "/some/other/extension/index.js" } },
      // Not a tool AFT ever registers: excluded regardless of source path.
      { name: "not_an_aft_tool", sourceInfo: { path: TOOL_SOURCE_PATH } },
    ]);
    emitBus("aft:binding-request", { v: 1, sessionId: "session-1", requestId: "r1" });
    expect(responses).toHaveLength(1);
    const response = responses[0] as {
      v: number;
      sessionId: string;
      requestId: string;
      tools: Array<{ name: string; sourcePath: string; capabilities: string[] }>;
      project: unknown;
    };
    expect(response.v).toBe(1);
    expect(response.sessionId).toBe("session-1");
    expect(response.requestId).toBe("r1");
    expect(typeof response.project).toBe("function");
    expect(response.tools.map((t) => t.name).sort()).toEqual(["aft_outline", "read"]);
    const read = response.tools.find((t) => t.name === "read")!;
    expect(read.sourcePath).toBe(TOOL_SOURCE_PATH);
    expect(read.capabilities.sort()).toEqual(["native-url", "observation"].sort());
  });

  test("registry read failures emit no binding and a later readable request recovers", () => {
    setHarnessOverrideForTesting("omp");
    for (const failing of ["getAllTools", "getActiveTools"] as const) {
      let unreadable = true;
      const { api, emitLifecycle, emitBus } = makeMockHost({
        getAllTools: () => {
          if (unreadable && failing === "getAllTools") throw new Error("Registry not bound");
          return [{ name: "read", sourceInfo: { path: TOOL_SOURCE_PATH } }];
        },
        getActiveTools: () => {
          if (unreadable && failing === "getActiveTools") throw new Error("Registry not bound");
          return ["read"];
        },
      });
      const responses: unknown[] = [];
      api.events.on("aft:binding", (payload) => responses.push(payload));
      registerAftBinding(api, projectCtx());
      emitLifecycle("session_start", undefined, makeExtContext(process.cwd(), "session-1"));
      emitBus("aft:binding-request", { v: 1, sessionId: "session-1", requestId: "r1" });
      expect(responses).toEqual([]);
      unreadable = false;
      emitBus("aft:binding-request", { v: 1, sessionId: "session-1", requestId: "r2" });
      expect(responses).toMatchObject([{ requestId: "r2", tools: [{ name: "read" }] }]);
    }
  });

  test("an initially empty registry can publish a later registration", () => {
    setHarnessOverrideForTesting("omp");
    const entries: Array<{ name: string; sourceInfo: { path: string } }> = [];
    const { api, emitLifecycle, emitBus } = makeMockHost({
      getAllTools: () => entries,
      getActiveTools: () => ["read"],
    });
    const responses: unknown[] = [];
    api.events.on("aft:binding", (payload) => responses.push(payload));
    registerAftBinding(api, projectCtx());
    emitLifecycle("session_start", undefined, makeExtContext(process.cwd(), "session-1"));
    emitBus("aft:binding-request", { v: 1, sessionId: "session-1", requestId: "r1" });
    expect(responses).toEqual([]);
    entries.push({ name: "read", sourceInfo: { path: TOOL_SOURCE_PATH } });
    emitBus("aft:binding-request", { v: 1, sessionId: "session-1", requestId: "r2" });
    expect(responses).toMatchObject([{ requestId: "r2", tools: [{ name: "read" }] }]);
  });
});

describe("projectAftCall — filesystem-mutating tools", () => {
  test("edit (non-hashline): exact single-file mutation target", async () => {
    const dir = tmpProjectDir();
    const projection = await projectAftCall(
      projectCtx(),
      "edit",
      { path: "a.ts", edits: [{ oldString: "x", newString: "y" }] },
      dir,
    );
    expect(projection.kind).toBe("mutation");
    expect(projection.paths).toEqual([path.join(dir, "a.ts")]);
    expect(projection.broad).toBe(false);
  });

  test("edit (hashline): reuses the real preflight parser for exact multi-file targets", async () => {
    const dir = tmpProjectDir();
    const ctx = projectCtx(
      (command) =>
        command === "hashline_preflight"
          ? { success: true, affected_paths: ["a.ts"], mv_destinations: ["b.ts"] }
          : { success: true },
      { hashlineEffective: true },
    );
    const projection = await projectAftCall(ctx, "edit", { patch: "[a.ts#AAAA]\nREM\n" }, dir);
    expect(projection.kind).toBe("mutation");
    expect(projection.paths.sort()).toEqual(
      [path.join(dir, "a.ts"), path.join(dir, "b.ts")].sort(),
    );
    expect(projection.broad).toBe(false);
  });

  test("edit (hashline): a patch the preflight parser rejects is unknown, never empty-but-safe", async () => {
    const dir = tmpProjectDir();
    const ctx = projectCtx(() => ({ success: false, text: "malformed patch" }), {
      hashlineEffective: true,
    });
    const projection = await projectAftCall(ctx, "edit", { patch: "garbage" }, dir);
    expect(projection.kind).toBe("unknown");
    expect(projection.paths).toEqual([]);
  });

  test("write: a GitHub comment target has no filesystem path", async () => {
    const projection = await projectAftCall(
      projectCtx(),
      "write",
      { path: "issue://42", content: "x" },
      "/repo",
    );
    expect(projection.kind).toBe("mutation");
    expect(projection.paths).toEqual([]);
  });

  test("aft_delete: recursive delete is a broad (ancestor) scope", async () => {
    const dir = tmpProjectDir();
    const ctx = projectCtx();
    const recursive = await projectAftCall(
      ctx,
      "aft_delete",
      { files: ["sub"], recursive: true },
      dir,
    );
    expect(recursive.kind).toBe("mutation");
    expect(recursive.broad).toBe(true);
    const nonRecursive = await projectAftCall(ctx, "aft_delete", { files: ["a.ts"] }, dir);
    expect(nonRecursive.broad).toBe(false);
  });

  test("aft_delete: no files is unknown, never an empty-but-safe mutation", async () => {
    const projection = await projectAftCall(projectCtx(), "aft_delete", { files: [] }, "/repo");
    expect(projection.kind).toBe("unknown");
  });

  test("aft_move: destination is a real mutation target alongside the source", async () => {
    const dir = tmpProjectDir();
    const projection = await projectAftCall(
      projectCtx(),
      "aft_move",
      { path: "a.ts", destination: "sub/b.ts" },
      dir,
    );
    expect(projection.kind).toBe("mutation");
    expect(projection.paths).toEqual([path.join(dir, "a.ts"), path.join(dir, "sub/b.ts")]);
  });

  test("aft_move: moving a directory is a broad (ancestor) scope", async () => {
    const dir = tmpProjectDir();
    const projection = await projectAftCall(
      projectCtx(),
      "aft_move",
      { path: ".", destination: "renamed" },
      dir,
    );
    expect(projection.broad).toBe(true);
  });
});

describe("projectAftCall — aft_safety", () => {
  test("list/history are observation, never mutation", async () => {
    const ctx = projectCtx();
    const list = await projectAftCall(ctx, "aft_safety", { op: "list" }, "/repo");
    expect(list.kind).toBe("observation");
    const history = await projectAftCall(
      ctx,
      "aft_safety",
      { op: "history", path: "a.ts" },
      "/repo",
    );
    expect(history.kind).toBe("observation");
  });

  test("restore without explicit targets reuses checkpoint_paths for exact targets, not unknown", async () => {
    const dir = tmpProjectDir();
    const ctx = projectCtx((command) =>
      command === "checkpoint_paths"
        ? { success: true, paths: ["a.ts", "b.ts"] }
        : { success: true },
    );
    const projection = await projectAftCall(ctx, "aft_safety", { op: "restore", name: "cp1" }, dir);
    expect(projection.kind).toBe("mutation");
    expect(projection.paths.sort()).toEqual(
      [path.join(dir, "a.ts"), path.join(dir, "b.ts")].sort(),
    );
    expect(projection.broad).toBe(false);
  });

  test("restore with no checkpoint name is unknown", async () => {
    const projection = await projectAftCall(projectCtx(), "aft_safety", { op: "restore" }, "/repo");
    expect(projection.kind).toBe("unknown");
  });

  test("checkpoint without files/path is unknown (uninspectable full-project scope)", async () => {
    const projection = await projectAftCall(
      projectCtx(),
      "aft_safety",
      { op: "checkpoint", name: "cp1" },
      "/repo",
    );
    expect(projection.kind).toBe("unknown");
  });

  test("checkpoint with explicit files is a mutation, never an observation", async () => {
    const projection = await projectAftCall(
      projectCtx(),
      "aft_safety",
      { op: "checkpoint", name: "cp1", files: ["a.ts"] },
      "/repo",
    );
    expect(projection.kind).toBe("mutation");
    expect(projection.paths).toEqual(["/repo/a.ts"]);
  });

  test("undo: a failed preview is unknown, never a partial guess from the explicit path", async () => {
    const dir = tmpProjectDir();
    const ctx = projectCtx(() => ({ success: false, text: "preview unavailable" }));
    const projection = await projectAftCall(ctx, "aft_safety", { op: "undo", path: "a.ts" }, dir);
    expect(projection.kind).toBe("unknown");
    expect(projection.paths).toEqual([]);
  });

  test("undo: an engine-confirmed empty scope is an honest empty mutation, not unknown", async () => {
    const dir = tmpProjectDir();
    const ctx = projectCtx((command) =>
      command === "undo_preview" ? { success: true, paths: [] } : { success: true },
    );
    const projection = await projectAftCall(ctx, "aft_safety", { op: "undo" }, dir);
    expect(projection.kind).toBe("mutation");
    expect(projection.paths).toEqual([]);
  });

  test("restore: a failed preview is unknown, never a partial guess from explicit path/files", async () => {
    const dir = tmpProjectDir();
    const ctx = projectCtx(() => ({ success: false, text: "preview unavailable" }));
    const projection = await projectAftCall(
      ctx,
      "aft_safety",
      { op: "restore", name: "cp1", path: "maybe-a-directory" },
      dir,
    );
    expect(projection.kind).toBe("unknown");
    expect(projection.paths).toEqual([]);
  });

  test("purge is a broad mutation on the purged path", async () => {
    const projection = await projectAftCall(
      projectCtx(),
      "aft_safety",
      { op: "purge", path: "secret.env" },
      "/repo",
    );
    expect(projection.kind).toBe("mutation");
    expect(projection.broad).toBe(true);
  });
});

describe("projectAftCall — AST dry-run versus apply", () => {
  test("ast_grep_replace with dryRun:true is observation", async () => {
    const dir = tmpProjectDir();
    fs.writeFileSync(path.join(dir, "a.ts"), "const x = 1;");
    const projection = await projectAftCall(
      projectCtx(),
      "ast_grep_replace",
      { pattern: "$A", rewrite: "$A", lang: "typescript", paths: ["a.ts"], dryRun: true },
      dir,
    );
    expect(projection.kind).toBe("observation");
  });

  test("ast_grep_replace without dryRun applies, and is mutation", async () => {
    const dir = tmpProjectDir();
    fs.writeFileSync(path.join(dir, "a.ts"), "const x = 1;");
    const projection = await projectAftCall(
      projectCtx(),
      "ast_grep_replace",
      { pattern: "$A", rewrite: "$A", lang: "typescript", paths: ["a.ts"] },
      dir,
    );
    expect(projection.kind).toBe("mutation");
  });

  test("ast_grep_search is always observation, regardless of scope breadth", async () => {
    const projection = await projectAftCall(
      projectCtx(),
      "ast_grep_search",
      { pattern: "$A", lang: "typescript" },
      "/repo",
    );
    expect(projection.kind).toBe("observation");
    expect(projection.broad).toBe(true);
  });
});

describe("projectAftCall — observation scope", () => {
  test("grep without a path scope is broad over cwd", async () => {
    const projection = await projectAftCall(projectCtx(), "grep", { pattern: "foo" }, "/repo");
    expect(projection.kind).toBe("observation");
    expect(projection.paths).toEqual(["/repo"]);
    expect(projection.broad).toBe(true);
  });

  test("aft_search path never narrows scope: always broad", async () => {
    const projection = await projectAftCall(projectCtx(), "aft_search", { query: "foo" }, "/repo");
    expect(projection.kind).toBe("observation");
    expect(projection.broad).toBe(true);
  });

  test("aft_callgraph requires a path; without one it is unknown", async () => {
    const projection = await projectAftCall(
      projectCtx(),
      "aft_callgraph",
      { op: "callers", symbol: "foo" },
      "/repo",
    );
    expect(projection.kind).toBe("unknown");
  });
});

describe("projectAftCall — native-URL delegation and unknown tools", () => {
  test("an OMP internal-URL read target is classified as native, not observation", async () => {
    const router: OmpInternalUrlRouter = {
      canHandle: (input: string) => input.startsWith("proc://"),
      canResolve: (input: string) => input.startsWith("proc://"),
      writeTarget: () => undefined,
    };
    const projection = await projectAftCall(
      projectCtx(undefined, { ompRouter: router }),
      "read",
      { path: "proc://123" },
      "/repo",
    );
    expect(projection.kind).toBe("native");
    expect(projection.paths).toEqual([]);
  });

  test("an unrecognized tool name is unknown", async () => {
    const projection = await projectAftCall(
      projectCtx(),
      "not_a_real_tool",
      { path: "a.ts" },
      "/repo",
    );
    expect(projection.kind).toBe("unknown");
  });
});

describe("session-owned mutation scope", () => {
  test("undo projects the issuing session backup stack, not another session scope", async () => {
    const dir = tmpProjectDir();
    const ctx = projectCtx((command, params) =>
      command === "undo_preview"
        ? {
            success: true,
            paths: [params.session_id === "session-a" ? "docs/adr/private.md" : "ordinary.ts"],
          }
        : { success: true },
    );
    const first = await projectAftCall(
      ctx,
      "aft_safety",
      { op: "undo" },
      dir,
      makeExtContext(dir, "session-a"),
    );
    const second = await projectAftCall(
      ctx,
      "aft_safety",
      { op: "undo" },
      dir,
      makeExtContext(dir, "session-b"),
    );
    expect(first.paths).toEqual([path.join(dir, "docs/adr/private.md")]);
    expect(second.paths).toEqual([path.join(dir, "ordinary.ts")]);
  });

  test("a binding projects hashline targets in the session that requested it", async () => {
    setHarnessOverrideForTesting("omp");
    const dir = tmpProjectDir();
    const { bridge } = makeMockBridge();
    bridge.toolCall = async (sessionId) => ({
      success: true,
      text: "",
      affected_paths: [sessionId === "session-1" ? "docs/adr/private.md" : "ordinary.ts"],
      mv_destinations: [],
    });
    const ctx = makePluginContext(bridge, { hashlineEffective: true });
    const { api, emitLifecycle, emitBus } = makeMockHost({
      getAllTools: () => [{ name: "edit", sourceInfo: { path: TOOL_SOURCE_PATH } }],
      getActiveTools: () => ["edit"],
    });
    let response:
      | {
          project: (
            name: string,
            input: Record<string, unknown>,
            cwd: string,
          ) => Promise<{ paths: string[] }>;
        }
      | undefined;
    api.events.on("aft:binding", (payload) => {
      response = payload as typeof response;
    });
    registerAftBinding(api, ctx);
    emitLifecycle("session_start", undefined, makeExtContext(dir, "session-1"));
    emitBus("aft:binding-request", { v: 1, sessionId: "session-1", requestId: "r1" });
    const projection = await response?.project("edit", { patch: "[a.ts#AAAA]\nREM\n" }, dir);
    expect(projection?.paths).toEqual([path.join(dir, "docs/adr/private.md")]);
  });
});

describe("AST compound mutation scopes", () => {
  test("whitespace scopes include every real root, including a protected directory", async () => {
    const root = tmpProjectDir();
    fs.mkdirSync(path.join(root, "src"));
    fs.mkdirSync(path.join(root, "docs/adr"), { recursive: true });
    const projected = await projectAftCall(
      projectCtx(),
      "ast_grep_replace",
      { paths: ["src docs/adr"] },
      root,
    );
    expect(projected).toEqual({
      kind: "mutation",
      paths: [path.join(root, "src"), path.join(root, "docs/adr")],
      broad: true,
    });
  });

  test("an existing literal path with spaces stays one scope even when its fragments exist", async () => {
    const root = tmpProjectDir();
    fs.mkdirSync(path.join(root, "src"));
    fs.mkdirSync(path.join(root, "docs"));
    fs.mkdirSync(path.join(root, "src docs"));
    const projected = await projectAftCall(
      projectCtx(),
      "ast_grep_replace",
      { paths: ["src docs"] },
      root,
    );
    expect(projected).toEqual({
      kind: "mutation",
      paths: [path.join(root, "src docs")],
      broad: true,
    });
  });

  test("partial and wholly missing compound roots are unknown, never partial safe scopes", async () => {
    const root = tmpProjectDir();
    fs.mkdirSync(path.join(root, "src"));
    for (const paths of [["src absent"], ["absent missing"], ["src", "absent missing"]]) {
      const projected = await projectAftCall(
        projectCtx(),
        "ast_grep_replace",
        { paths, globs: ["*.ts"] },
        root,
      );
      expect(projected.kind).toBe("unknown");
    }
  });
});
