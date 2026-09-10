import { Database } from "bun:sqlite";
import { describe, expect, test } from "bun:test";
import { resolve } from "node:path";
import type { BridgePool } from "@cortexkit/aft-bridge";
import { Effect } from "effect";

import {
  PermissionDeniedError,
  PermissionRejectedError,
  requestPermission,
  type V2PermissionClient,
  type V2PermissionCreateInput,
  type V2PermissionReply,
} from "../../src/permissions/v2.js";
import { projectV2Tool, type V2ProviderTool } from "../../src/tools/definitions/v2.js";
import { hoistedV2ToolConsumers } from "../../src/tools/hoisted/v2.js";
import { hoistedTools } from "../../src/tools/hoisted.js";
import type { PluginContext } from "../../src/types.js";

const PROJECT_ROOT = resolve(import.meta.dir, "../../../..");
const LOCATION = {
  directory: PROJECT_ROOT,
  project: { directory: PROJECT_ROOT, canonical: PROJECT_ROOT },
};
const EXECUTION_CONTEXT = {
  sessionID: "session-v2",
  messageID: "message-v2",
  id: "call-v2",
  agent: "agent-v2",
  progress: () => Effect.succeed(undefined),
};
const REQUEST = {
  permission: "edit",
  patterns: ["/work/project/file.ts"],
  always: ["/work/project/file.ts"],
  metadata: { filepath: "/work/project/file.ts", diff: "@@ -1 +1 @@" },
};

type PermissionEffect = "allow" | "deny" | "ask";
type PermissionEvent = {
  type: "permission.replied";
  properties: { sessionID: string; requestID: string; reply: V2PermissionReply };
};

class EventStream implements AsyncIterable<unknown>, AsyncIterator<unknown> {
  private readonly queued: unknown[] = [];
  private readonly waiters: Array<(result: IteratorResult<unknown>) => void> = [];
  closed = false;

  [Symbol.asyncIterator](): AsyncIterator<unknown> {
    return this;
  }

  next(): Promise<IteratorResult<unknown>> {
    const value = this.queued.shift();
    if (value !== undefined) return Promise.resolve({ done: false, value });
    if (this.closed) return Promise.resolve({ done: true, value: undefined });
    return new Promise((resolveNext) => this.waiters.push(resolveNext));
  }

  return(): Promise<IteratorResult<unknown>> {
    this.closed = true;
    for (const resolveNext of this.waiters.splice(0)) {
      resolveNext({ done: true, value: undefined });
    }
    return Promise.resolve({ done: true, value: undefined });
  }

  push(value: unknown): void {
    const waiter = this.waiters.shift();
    if (waiter) waiter({ done: false, value });
    else this.queued.push(value);
  }
}

function permissionClient(effect: PermissionEffect = "ask") {
  const stream = new EventStream();
  const createCalls: V2PermissionCreateInput[] = [];
  let subscribed = false;
  const client: V2PermissionClient = {
    event: {
      subscribe: async () => {
        subscribed = true;
        return { stream };
      },
    },
    permission: {
      create: async (input) => {
        expect(subscribed).toBe(true);
        createCalls.push(input);
        return { data: { id: `permission-${createCalls.length}`, effect } };
      },
    },
  };
  const reply = (requestID: string, replyValue: V2PermissionReply) => {
    stream.push({
      type: "permission.replied",
      properties: { sessionID: EXECUTION_CONTEXT.sessionID, requestID, reply: replyValue },
    } satisfies PermissionEvent);
  };
  return { client, stream, createCalls, reply };
}

async function settles(promise: Promise<unknown>): Promise<boolean> {
  let settled = false;
  void promise.finally(() => {
    settled = true;
  });
  await Promise.resolve();
  await Promise.resolve();
  return settled;
}

function pluginContext(
  bridgeResponse: (name: string, preview: boolean) => Record<string, unknown>,
): PluginContext {
  const bridge = {
    toolCall: async (
      _sessionID: string | undefined,
      name: string,
      _args: Record<string, unknown>,
      options?: { preview?: boolean },
    ) => bridgeResponse(name, options?.preview === true),
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
  client: V2PermissionClient,
  bridgeResponse: (name: string, preview: boolean) => Record<string, unknown>,
) {
  const definitions = hoistedTools(pluginContext(bridgeResponse));
  const consumers = hoistedV2ToolConsumers({ client });
  const project = (name: keyof typeof definitions) =>
    projectV2Tool(name, definitions[name], LOCATION, consumers);
  return {
    read: project("read"),
    write: project("write"),
    edit: project("edit"),
    aft_delete: project("aft_delete"),
    aft_move: project("aft_move"),
  };
}

async function execute(tool: V2ProviderTool, input: Record<string, unknown>) {
  return await Effect.runPromise(tool.execute(input, EXECUTION_CONTEXT));
}

function parsedDenied(result: Record<string, unknown>) {
  return JSON.parse(String(result.content)) as Record<string, unknown>;
}

describe("OpenCode V2 permission consumer", () => {
  test("creates the session-scoped request and waits for its permission.replied event", async () => {
    const host = permissionClient();
    const pending = requestPermission({ client: host.client }, REQUEST, EXECUTION_CONTEXT);

    expect(await settles(pending)).toBe(false);
    expect(host.createCalls).toEqual([
      {
        sessionID: "session-v2",
        action: "edit",
        resources: ["/work/project/file.ts"],
        save: ["/work/project/file.ts"],
        metadata: { filepath: "/work/project/file.ts", diff: "@@ -1 +1 @@" },
        source: { type: "tool", messageID: "message-v2", id: "call-v2" },
      },
    ]);

    host.stream.push({
      type: "permission.replied",
      properties: { sessionID: "another-session", requestID: "permission-1", reply: "once" },
    });
    expect(await settles(pending)).toBe(false);
    host.reply("permission-1", "once");
    await pending;
    expect(host.stream.closed).toBe(true);
  });

  test("maps host deny and reject outcomes to distinct failures", async () => {
    const denied = permissionClient("deny");
    await expect(
      requestPermission({ client: denied.client }, REQUEST, EXECUTION_CONTEXT),
    ).rejects.toBeInstanceOf(PermissionDeniedError);

    const rejected = permissionClient();
    const pending = requestPermission({ client: rejected.client }, REQUEST, EXECUTION_CONTEXT);
    await Promise.resolve();
    rejected.reply("permission-1", "reject");
    await expect(pending).rejects.toBeInstanceOf(PermissionRejectedError);
  });

  test("persists always grants by action and resource and reuses a V1-written row", async () => {
    const db = new Database(":memory:");
    db.exec(
      "CREATE TABLE permission_saved (action TEXT NOT NULL, resource TEXT NOT NULL);" +
        "INSERT INTO permission_saved VALUES ('edit', '/work/project/from-v1.ts');",
    );
    const streams: EventStream[] = [];
    const creates: V2PermissionCreateInput[] = [];
    const client: V2PermissionClient = {
      event: {
        subscribe: async () => {
          const stream = new EventStream();
          streams.push(stream);
          return { stream };
        },
      },
      permission: {
        create: async (input) => {
          creates.push(input);
          const saved = db
            .query(
              "SELECT 1 FROM permission_saved WHERE action = ? AND (resource = ? OR resource = '*')",
            )
            .get(input.action, input.resources[0]) as unknown;
          return { data: { id: `permission-${creates.length}`, effect: saved ? "allow" : "ask" } };
        },
      },
    };
    const v1Request = { ...REQUEST, patterns: ["/work/project/from-v1.ts"] };
    await requestPermission({ client }, v1Request, EXECUTION_CONTEXT);
    expect(streams[0]?.closed).toBe(true);

    const first = requestPermission({ client }, REQUEST, EXECUTION_CONTEXT);
    await Promise.resolve();
    db.query("INSERT INTO permission_saved VALUES (?, ?)").run("edit", REQUEST.patterns[0]);
    streams[1]?.push({
      type: "permission.replied",
      properties: { sessionID: "session-v2", requestID: "permission-2", reply: "always" },
    } satisfies PermissionEvent);
    await first;

    const row = db
      .query("SELECT action, resource FROM permission_saved WHERE resource = ?")
      .get(REQUEST.patterns[0]);
    expect(row).toEqual({ action: "edit", resource: REQUEST.patterns[0] });
    await requestPermission({ client }, REQUEST, EXECUTION_CONTEXT);
    expect(creates).toHaveLength(3);
    expect(streams[2]?.closed).toBe(true);
    db.close();
  });

  test("hoisted edit sends PatchDiff metadata through the registration consumer seam", async () => {
    const host = permissionClient("allow");
    const tools = projectedFilesystemTools(host.client, (_name, preview) =>
      preview
        ? { success: true, preview_diff: "@@ -1 +1 @@\n-old\n+new" }
        : { success: true, text: "edited" },
    );

    await execute(tools.edit, {
      path: "file.ts",
      edits: [{ oldString: "old", newString: "new" }],
    });

    expect(host.createCalls[0]).toMatchObject({
      action: "edit",
      resources: ["file.ts"],
      metadata: {
        filepath: resolve(PROJECT_ROOT, "file.ts"),
        diff: "@@ -1 +1 @@\n-old\n+new",
      },
    });
  });

  test("read, write, and edit return permission_denied responses", async () => {
    for (const name of ["read", "write", "edit"] as const) {
      const host = permissionClient("deny");
      const tools = projectedFilesystemTools(host.client, (_tool, preview) =>
        preview ? { success: true, preview_diff: "diff" } : { success: true, text: "ok" },
      );
      const input =
        name === "read"
          ? { path: "file.ts" }
          : name === "write"
            ? { path: "file.ts", content: "new" }
            : { path: "file.ts", edits: [{ oldString: "old", newString: "new" }] };
      const result = await execute(tools[name], input);
      expect(parsedDenied(result)).toMatchObject({ success: false, code: "permission_denied" });
    }
  });

  test("aft_delete and aft_move reject instead of returning permission_denied", async () => {
    for (const name of ["aft_delete", "aft_move"] as const) {
      const host = permissionClient("deny");
      const tools = projectedFilesystemTools(host.client, () => ({ success: true, text: "ok" }));
      const input =
        name === "aft_delete"
          ? { files: ["file.ts"] }
          : { path: "file.ts", destination: "moved.ts" };
      await expect(execute(tools[name], input)).rejects.toMatchObject({
        _tag: "Tool.Error",
        message: "Permission denied.",
      });
      expect(host.createCalls[0]).toMatchObject({
        action: "edit",
        metadata: { action: name === "aft_delete" ? "delete" : "move" },
      });
      expect(host.createCalls[0]?.action).not.toBe(name);
    }
  });
});
