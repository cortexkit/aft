import { describe, expect, test } from "bun:test";

import { resolve } from "node:path";
import type { BridgePool } from "@cortexkit/aft-bridge";
import { prepareCanonicalPathArguments } from "@cortexkit/aft-bridge";
import { Error as ToolError } from "@opencode/plugin/promise/tool";
import { Cause, Effect } from "effect";

import { makeServerEffect } from "../entry/server-runtime.mjs";
import { requestPermission, type V2PermissionHostContext } from "../permissions/v2.js";
import { astTools } from "../tools/ast.js";
import { projectV2Tool, type V2DefinitionRuntime } from "../tools/definitions/v2.js";
import { hoistedV2ToolConsumers } from "../tools/hoisted/v2.js";
import { hoistedTools } from "../tools/hoisted.js";
import { inspectTools } from "../tools/inspect.js";
import { navigationTools } from "../tools/navigation.js";
import { classifyPermissionError } from "../tools/permissions.js";
import { readingTools } from "../tools/reading.js";
import { searchTools } from "../tools/search.js";
import type { PluginContext } from "../types.js";
import { engineOutsideAsk } from "./helpers/engine-outside-ask.js";
import { permissionHost, type V2PermissionRule } from "./v2-permission-host.js";

const request = {
  permission: "edit",
  patterns: ["src/a.ts"],
  always: ["src/*"],
  metadata: { diff: "-old\n+new", reason: "Narrow the edit" },
};
const context = {
  sessionID: "ses_test",
  messageID: "msg_test",
  id: "call_test",
  agent: "build",
  progress: () => Effect.void,
};

const defaultRules: readonly V2PermissionRule[] = [
  { action: "*", resource: "*", effect: "allow" },
  { action: "external_directory", resource: "*", effect: "ask" },
  { action: "read", resource: "*.env", effect: "ask" },
  { action: "read", resource: "*.env.*", effect: "ask" },
  { action: "read", resource: "*.env.example", effect: "allow" },
];
const ruleRequest = {
  permission: "edit",
  patterns: ["file.ts"],
  always: ["file.ts"],
  metadata: { filepath: "/work/project/file.ts", diff: "@@ -1 +1 @@" },
};

function fixture(options: Parameters<typeof permissionHost>[0] = {}) {
  return permissionHost(options);
}

function pluginContext(
  bridgeResponse: (
    name: string,
    preview: boolean,
    args: Record<string, unknown>,
  ) => Record<string, unknown>,
  bridgeSend?: (command: string, params: Record<string, unknown>) => Record<string, unknown>,
): PluginContext {
  const bridge = {
    send: async (command: string, params: Record<string, unknown>) => {
      if (!bridgeSend) throw new Error(`unexpected ${command}`);
      return bridgeSend(command, params);
    },
    toolCall: async (
      _sessionID: string | undefined,
      name: string,
      args: Record<string, unknown>,
      options?: { preview?: boolean },
    ) => ({ content_files: [], ...bridgeResponse(name, options?.preview === true, args) }),
  };
  return {
    pool: { getBridge: () => bridge } as unknown as BridgePool,
    client: {},
    config: { tool_surface: "all", hoist_builtin_tools: true },
    hashlineEffective: false,
    storageDir: "/isolated/storage",
  } as PluginContext;
}

function projectedFilesystemTools(
  host: V2PermissionHostContext,
  bridgeResponse: (name: string, preview: boolean) => Record<string, unknown>,
) {
  const definitions = hoistedTools(pluginContext(bridgeResponse));
  const consumers = hoistedV2ToolConsumers(host);
  const location = { directory: process.cwd() };
  return {
    read: projectV2Tool("read", definitions.read!, location, consumers),
    write: projectV2Tool("write", definitions.write!, location, consumers),
    edit: projectV2Tool("edit", definitions.edit!, location, consumers),
    aft_delete: projectV2Tool("aft_delete", definitions.aft_delete!, location, consumers),
    aft_move: projectV2Tool("aft_move", definitions.aft_move!, location, consumers),
  };
}

async function executeProjected(
  tool: ReturnType<typeof projectV2Tool>,
  input: Record<string, unknown>,
) {
  return await Effect.runPromise(tool.execute(input, context));
}

function artifactReadTool(
  owned: boolean,
  name = "read",
  artifact = "/aft-v2-artifact-fixture/opencode/bash-tasks/bgb-0123abcd/stdout",
) {
  const ownershipQueries: Record<string, unknown>[] = [];
  const plugin = pluginContext(
    (tool, _preview, args) => {
      const rawTarget =
        prepareCanonicalPathArguments(tool, args).path ??
        args.path ??
        args.target ??
        args.scope ??
        (args.paths as unknown[] | undefined)?.[0] ??
        // Zoom sends only a batch of { target, ... } entries.
        (args.batch as Array<{ target?: unknown }> | undefined)?.[0]?.target;
      const target = String(Array.isArray(rawTarget) ? rawTarget[0] : rawTarget);
      if (!owned || tool === "write" || target !== artifact) {
        const ask = engineOutsideAsk(target, args);
        if (ask) return ask;
      }
      return { success: true, text: "task output" };
    },
    (command, params) => {
      if (command !== "bash_artifact_owned") throw new Error(`unexpected ${command}`);
      ownershipQueries.push(params);
      return {
        success: true,
        owned: owned && params.path === artifact && params.session_id === "ses_test",
      };
    },
  );
  const { host, calls } = fixture({ agentRules: defaultRules });
  const definitions = {
    ...hoistedTools(plugin),
    ...searchTools(plugin),
    ...readingTools(plugin),
    ...navigationTools(plugin),
    ...inspectTools(plugin),
    ...astTools(plugin),
  };
  const definition = definitions[name];
  if (!definition) throw new Error(`the ${name} tool definition was not created`);
  const read = projectV2Tool(
    name,
    definition,
    { directory: process.cwd() },
    hoistedV2ToolConsumers(host),
  );
  return { read, artifact, calls, ownershipQueries };
}

describe("OpenCode 2 in-process permission assertion", () => {
  test("route replacement requests a second host assertion before retrying", async () => {
    const target = "/aft-v2-engine-fixture/narrow/file.ts";
    const { host, calls, prompts } = fixture({
      agentRules: [
        { action: "*", resource: "*", effect: "allow" },
        { action: "external_directory", resource: "*", effect: "ask" },
      ],
    });
    const sends: Record<string, unknown>[] = [];
    const definitions = hoistedTools(
      pluginContext((_name, _preview, args) => {
        sends.push(structuredClone(args));
        if (sends.length === 3) return { success: true, text: "freshly approved contents" };
        return {
          success: false,
          code: "permission_required",
          asks: [
            {
              kind: "external_directory",
              grant_id: sends.length === 1 ? "outside:old-route" : "outside:new-route",
              path: target,
              operation: "read",
              patterns: [target],
              always: [target],
              ...(sends.length === 2 ? { reason: "approval_route_replaced" } : {}),
            },
          ],
        };
      }),
    );
    const read = projectV2Tool(
      "read",
      definitions.read!,
      { directory: process.cwd() },
      hoistedV2ToolConsumers(host),
    );
    const result = await executeProjected(read, { path: target });
    expect(JSON.stringify(result)).toContain("freshly approved contents");
    expect(sends[1]?.permissions_granted).toEqual(["outside:old-route"]);
    expect(sends[2]?.permissions_granted).toEqual(["outside:new-route"]);
    expect(calls.filter((call) => call.action === "external_directory")).toHaveLength(2);
    expect(prompts.filter((call) => call.action === "external_directory")).toHaveLength(2);
  });

  for (const decision of ["allow", "ask", "deny", "hook-deny"] as const) {
    test(`engine outside asks use host assert with exact patterns: ${decision}`, async () => {
      const target = "/aft-v2-engine-fixture/narrow/file.ts";
      const patterns = ["/aft-v2-engine-fixture/narrow/*", "/aft-v2-engine-fixture/other/*"];
      const always = ["/aft-v2-engine-fixture/approved/*"];
      const { host, calls, prompts } = fixture({
        agentRules: [
          { action: "*", resource: "*", effect: "allow" },
          {
            action: "external_directory",
            resource: "*",
            effect: decision === "hook-deny" ? "allow" : decision,
          },
        ],
        evaluate:
          decision === "hook-deny"
            ? (input, effect) => (input.action === "external_directory" ? "deny" : effect)
            : undefined,
      });
      const bridgeCalls: Record<string, unknown>[] = [];
      const definitions = hoistedTools(
        pluginContext((_name, _preview, args) => {
          bridgeCalls.push(structuredClone(args));
          if (
            !(args.permissions_granted as string[] | undefined)?.includes("outside:engine-wire")
          ) {
            return {
              success: false,
              code: "permission_required",
              text: "outside permission needed",
              asks: [
                {
                  kind: "external_directory",
                  grant_id: "outside:engine-wire",
                  path: target,
                  parentDir: "/aft-v2-engine-fixture/narrow",
                  patterns,
                  always,
                },
              ],
            };
          }
          return { success: true, text: "approved contents" };
        }),
      );
      const read = projectV2Tool(
        "read",
        definitions.read!,
        { directory: process.cwd() },
        hoistedV2ToolConsumers(host),
      );
      if (decision === "deny" || decision === "hook-deny") {
        await expect(executeProjected(read, { path: target })).rejects.toThrow(
          "Permission denied: external_directory",
        );
        expect(bridgeCalls).toHaveLength(1);
      } else {
        await executeProjected(read, { path: target });
        expect(bridgeCalls).toHaveLength(2);
        expect(bridgeCalls[1]?.permissions_granted).toEqual(["outside:engine-wire"]);
      }
      expect(calls.map((call) => call.action)).toEqual(["read", "external_directory"]);
      expect(calls[1]).toMatchObject({
        action: "external_directory",
        resources: patterns,
        save: always,
        source: { type: "tool", messageID: "msg_test", id: "call_test" },
      });
      expect(prompts.map((call) => call.action)).toEqual(
        decision === "ask" ? ["external_directory"] : [],
      );
    });
  }

  test("ask awaits assert and preserves edit diff and source mapping", async () => {
    const { host, calls } = fixture();
    let finish!: () => void;
    host.permission.assert = (input) =>
      Effect.promise(async () => {
        calls.push(input);
        await new Promise<void>((resolve) => {
          finish = resolve;
        });
      });
    let completed = false;
    const pending = requestPermission(host, request, context).then(() => {
      completed = true;
    });
    for (let turn = 0; turn < 20 && !finish; turn++) await Promise.resolve();
    expect(completed).toBe(false);
    expect(calls).toEqual([
      {
        sessionID: "ses_test",
        agent: "build",
        action: "edit",
        resources: ["src/a.ts"],
        save: ["src/*"],
        metadata: request.metadata,
        source: { type: "tool", messageID: "msg_test", id: "call_test" },
      },
    ]);
    finish();
    await pending;
    expect(completed).toBe(true);
  });

  test("asserts once with every resource and lets the host resolve session rules", async () => {
    const { host, calls, prompts } = fixture({
      agentRules: defaultRules,
      sessionRules: [{ action: "edit", resource: "file.ts", effect: "deny" }],
    });
    await expect(requestPermission(host, ruleRequest, context)).rejects.toThrow(
      "Permission denied: edit",
    );
    expect(calls).toHaveLength(1);
    expect(calls[0]?.resources).toEqual(["file.ts"]);
    expect(prompts).toEqual([]);
  });
  test("hoisted edit preserves its generated PatchDiff metadata for the permission consumer", async () => {
    const seen: unknown[] = [];
    const { host } = fixture({ agentRules: defaultRules });
    const definitions = hoistedTools(
      pluginContext((_name, preview) =>
        preview
          ? { success: true, preview_diff: "@@ -1 +1 @@\n-old\n+new" }
          : { success: true, text: "edited" },
      ),
    );
    const definition = definitions.edit;
    if (!definition) throw new Error("the edit tool definition was not created");
    const consumers = hoistedV2ToolConsumers(host);
    const edit = projectV2Tool(
      "edit",
      definition,
      { directory: process.cwd() },
      {
        requestPermission: (permissionRequest, executionContext) => {
          seen.push(permissionRequest);
          return (
            consumers.requestPermission?.(permissionRequest, executionContext) ?? Promise.resolve()
          );
        },
      },
    );

    await executeProjected(edit, {
      path: "file.ts",
      edits: [{ oldString: "old", newString: "new" }],
    });

    expect(seen[0]).toMatchObject({
      permission: "edit",
      patterns: ["file.ts"],
      metadata: {
        filepath: resolve(process.cwd(), "file.ts"),
        diff: "@@ -1 +1 @@\n-old\n+new",
      },
    });
  });

  test("a session-owned bash artifact skips external_directory but still evaluates read", async () => {
    const { read, artifact, calls, ownershipQueries } = artifactReadTool(true);

    await executeProjected(read, { path: artifact });

    expect(calls.map((call) => (call as { action: string }).action)).toEqual(["read"]);
    expect(ownershipQueries).toEqual([]);
  });

  test("an artifact this session does not own evaluates external_directory and read", async () => {
    const { read, artifact, calls } = artifactReadTool(false);

    await executeProjected(read, { path: artifact });

    expect(calls.map((call) => (call as { action: string }).action)).toEqual([
      "read",
      "external_directory",
    ]);
  });

  const readers = [
    { name: "grep", args: (p: string) => ({ path: p, pattern: "task" }) },
    { name: "glob", args: (p: string) => ({ path: p, pattern: "*" }) },
    { name: "aft_outline", args: (p: string) => ({ target: p }) },
    { name: "aft_zoom", args: (p: string) => ({ path: p, symbols: "task" }) },
    { name: "aft_callgraph", args: (p: string) => ({ path: p, op: "callers", symbol: "task" }) },
    { name: "aft_inspect", args: (p: string) => ({ scope: p, sections: ["todos"] }) },
    {
      name: "ast_grep_search",
      args: (p: string) => ({ paths: [p], lang: "rust", pattern: "fn $F() { $$$ }" }),
    },
  ];
  for (const scenario of ["own", "foreign", "directory", "pattern"] as const) {
    test(`exact-file readers ${scenario} artifact permission boundary`, async () => {
      const observations: Record<string, number> = {};
      for (const reader of readers) {
        const { read, artifact, calls } = artifactReadTool(scenario !== "foreign", reader.name);
        const target =
          scenario === "directory"
            ? artifact.slice(0, artifact.lastIndexOf("/"))
            : scenario === "pattern"
              ? artifact.replace("stdout", "*")
              : artifact;
        await executeProjected(read, reader.args(target));
        observations[reader.name] = calls.filter(
          (call) => (call as { action: string }).action === "external_directory",
        ).length;
      }
      expect(observations).toEqual(
        Object.fromEntries(readers.map((reader) => [reader.name, scenario === "own" ? 0 : 1])),
      );
    });
  }

  for (const owned of [true, false]) {
    for (const target of [
      "/aft-v2-scratch-fixture/.sandbox-tmp-task/scratch.rs",
      "/aft-v2-scratch-fixture/.sandbox-tmp-task",
    ]) {
      test(`scratch read capability ${owned ? "owned" : "foreign"} ${target}`, async () => {
        for (const reader of [{ name: "read", args: (p: string) => ({ path: p }) }, ...readers]) {
          const { read, calls, ownershipQueries } = artifactReadTool(owned, reader.name, target);
          await executeProjected(read, reader.args(target));
          expect(ownershipQueries).toEqual([]);
          expect(
            calls.filter((call) => (call as { action: string }).action === "external_directory"),
          ).toHaveLength(owned ? 0 : 1);
        }
      });
    }
  }

  test("write to own artifact still asserts external_directory", async () => {
    const { read: write, artifact, calls } = artifactReadTool(true, "write");
    await executeProjected(write, { path: artifact, content: "overwrite" });
    expect(calls.map((call) => (call as { action: string }).action)).toContain(
      "external_directory",
    );
  });

  test("read, write, and edit surface a rule denial instead of a JSON envelope", async () => {
    for (const name of ["read", "write", "edit"] as const) {
      const { host } = fixture({
        agentRules: [{ action: "*", resource: "*", effect: "deny" }],
      });
      const tools = projectedFilesystemTools(host, (_tool, preview) =>
        preview ? { success: true, preview_diff: "diff" } : { success: true, text: "ok" },
      );
      const input =
        name === "read"
          ? { path: "file.ts" }
          : name === "write"
            ? { path: "file.ts", content: "new" }
            : { path: "file.ts", edits: [{ oldString: "old", newString: "new" }] };

      const failure = await executeProjected(tools[name], input).then(
        (result) => result,
        (error: unknown) => error as Error,
      );

      expect(failure, `${name} must fail the call`).toBeInstanceOf(Error);
      expect((failure as Error).message).toContain("permission_denied");
      expect((failure as Error).message).not.toContain('"success"');
    }
  });

  test("aft_delete and aft_move preserve the host's denial reason", async () => {
    for (const name of ["aft_delete", "aft_move"] as const) {
      const { host } = fixture({
        agentRules: [{ action: "edit", resource: "*", effect: "deny" }],
      });
      const tools = projectedFilesystemTools(host, () => ({ success: true, text: "ok" }));
      const input =
        name === "aft_delete"
          ? { files: ["file.ts"] }
          : { path: "file.ts", destination: "moved.ts" };
      const denied = await executeProjected(tools[name], input).catch(
        (error: unknown) => error as Error,
      );

      expect(denied).toBeInstanceOf(Error);
      expect((denied as Error).message).toContain("Permission denied: edit");
    }
  });

  for (const [label, message] of [
    ["deny", "Denied by host rule"],
    ["feedback", "Use a narrower path"],
  ]) {
    test(`host Tool.Error ${label} preserves deny-with-feedback`, async () => {
      const { host, calls } = fixture({ rejection: new ToolError({ message }) });
      const error = await requestPermission(host, request, context).catch((error) => error);
      expect(classifyPermissionError(error)).toEqual({ kind: "feedback", message });
      expect(calls).toHaveLength(1);
    });
  }

  test("plain host rejection escapes the projected tool unchanged even if shared code catches it", async () => {
    const abort = new Error("stop this step");
    const { host } = fixture({ rejection: abort });
    const projected = projectV2Tool(
      "edit",
      {
        description: "edit",
        args: {},
        execute: async (_input, runtime) => {
          try {
            await (runtime as unknown as V2DefinitionRuntime).ask(request);
          } catch {
            return "caught by shared tool";
          }
          return "incorrectly allowed";
        },
      },
      { directory: "/project" },
      hoistedV2ToolConsumers(host),
    );
    const outcome = await Effect.runPromiseExit(projected.execute({}, context));
    expect(outcome._tag).toBe("Failure");
    if (outcome._tag === "Failure") {
      expect(Cause.findDefect(outcome.cause)).toMatchObject({ _tag: "Success", success: abort });
    }
  });

  test("old V2 context refuses startup with the required version instead of disabling gated tools", () => {
    const { host } = fixture();
    const { permission: _permission, ...oldHost } = host;
    expect(() => hoistedV2ToolConsumers(oldHost)).toThrow("OpenCode 2.0.29");
    expect(() => makeServerEffect()({ ...oldHost, location: { directory: "/project" } })).toThrow(
      "OpenCode 2.0.29",
    );
  });

  test("capability probe accepts assert without a host version string", () => {
    expect(typeof hoistedV2ToolConsumers(fixture().host).requestPermission).toBe("function");
  });

  test("caller cancellation interrupts the host assertion rather than leaving a pending prompt", async () => {
    const { host } = fixture();
    const controller = new AbortController();
    let started = false;
    let interrupted = false;
    host.permission.assert = () =>
      Effect.suspend(() => {
        started = true;
        return Effect.never.pipe(
          Effect.onInterrupt(() =>
            Effect.sync(() => {
              interrupted = true;
            }),
          ),
        );
      });
    const projected = projectV2Tool(
      "edit",
      {
        description: "edit",
        args: {},
        execute: async (_input, runtime) => {
          await (runtime as unknown as V2DefinitionRuntime).ask(request);
          return "allowed";
        },
      },
      { directory: "/project" },
      hoistedV2ToolConsumers(host),
    );
    const pending = Effect.runPromiseExit(projected.execute({}, context), {
      signal: controller.signal,
    });
    for (let turn = 0; turn < 20 && !started; turn++) await Promise.resolve();
    expect(started).toBe(true);
    controller.abort();
    await pending;
    expect(interrupted).toBe(true);
  });
});
