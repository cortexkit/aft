import { expect, test } from "bun:test";
import { SubcTransportPool } from "@cortexkit/aft-bridge";
import type { BindIdentity, RouteHandle } from "@cortexkit/subc-client";
import type { ExtensionAPI, ExtensionContext } from "@earendil-works/pi-coding-agent";
import { sessionBgStates } from "../bg-notifications.js";
import { registerBashTool } from "../tools/bash.js";
import type { PluginContext } from "../types.js";

const root = process.cwd();
const session = "pi-idle-background";
const taskId = "bash-release";
const handoff = `Background task started: ${taskId}. A completion reminder will be delivered automatically; don't poll bash_status.`;

function envelope(data: Record<string, unknown>) {
  return {
    content: [{ type: "text", text: data.text }],
    structuredContent: { success: true, ...data },
    isError: false,
  };
}

async function settle() {
  for (let i = 0; i < 50; i++) await Promise.resolve();
}

test("Pi background bash retains idle routes until its completion is acknowledged", async () => {
  const routes = new Map<number, BindIdentity>();
  const requests: Array<{ name: string; arguments: Record<string, unknown> }> = [];
  let nextChannel = 0;
  let unsubscribed = 0;
  let nudges = 0;
  let nudge!: () => void;
  const client = {
    async routeOpen(_target: unknown, identity: BindIdentity) {
      const channel = ++nextChannel;
      routes.set(channel, identity);
      return { channel, epoch: 1 } as RouteHandle;
    },
    async request(_route: RouteHandle, raw: unknown) {
      const body = raw as (typeof requests)[number];
      requests.push(body);
      switch (body.name) {
        case "bash":
          return envelope({
            text: handoff,
            output: handoff,
            task_id: taskId,
            status: "running",
            mode: "pipes",
          });
        case "bash_drain_completions":
          return envelope({
            text: "",
            bg_completions: [{ task_id: taskId, status: "completed" }],
            pending_matches: [],
          });
        case "bash_ack_completions":
          return envelope({ text: "", acked_task_ids: [taskId] });
        default:
          throw new Error(`Unexpected tool call: ${body.name}`);
      }
    },
    subscribe(_route: RouteHandle, _body: unknown, onEvent: (event: Uint8Array) => void) {
      let finish!: () => void;
      const closed = new Promise<void>((resolve) => {
        finish = resolve;
      });
      nudge = () => onEvent(new TextEncoder().encode(JSON.stringify({ op: "bg_events" })));
      return {
        closed,
        unsubscribe() {
          unsubscribed++;
          finish();
        },
      };
    },
    async closeRouteChannel(route: RouteHandle) {
      routes.delete(route.channel);
    },
    close() {},
  };
  const pool = new SubcTransportPool({
    connectionFile: "/fake/connection.json",
    consumerIdentity: null,
    harness: "pi",
    connect: async () => client,
    onBgEventsNudge: () => {
      nudges++;
    },
  });
  const ctx: PluginContext = {
    pool,
    config: {} as PluginContext["config"],
    storageDir: root,
  };
  let bash!: {
    execute: (
      id: string,
      args: unknown,
      signal: undefined,
      onUpdate: undefined,
      ctx: ExtensionContext,
    ) => Promise<unknown>;
  };
  const api = {
    registerTool(tool: typeof bash & { name: string }) {
      if (tool.name === "bash") bash = tool;
    },
  } as unknown as ExtensionAPI;
  registerBashTool(api, ctx);
  try {
    // A session whose start the pool observed can be reaped once its tracked work is settled.
    pool.observeSessionStart(root, session);
    const result = await bash.execute(
      "launch",
      { command: "release.sh", background: true },
      undefined,
      undefined,
      { cwd: root, sessionManager: { getSessionId: () => session } } as ExtensionContext,
    );
    await settle();
    expect(requests[0]).toMatchObject({
      name: "bash",
      arguments: { background: true, notify_on_completion: true, session_id: session },
    });
    expect(result).toMatchObject({ content: [{ text: handoff }], details: { task_id: taskId } });
    expect([...routes.values()]).toEqual([
      { project_root: root, harness: "pi", session },
      { project_root: root, harness: "pi", session },
    ]);
    const boundChannels = [...routes.keys()];
    await pool.reapIdleSessions(Date.now() + 16 * 60_000);
    expect([...routes.keys()]).toEqual(boundChannels);
    expect(unsubscribed).toBe(0);
    const beforeNudge = nudges;
    nudge();
    expect(nudges).toBe(beforeNudge + 1);

    const bridge = pool.getBridge(root);
    await bridge.send("bash_drain_completions", { session_id: session });
    await pool.reapIdleSessions(Date.now() + 16 * 60_000);
    expect([...routes.keys()]).toEqual(boundChannels);
    await bridge.send("bash_ack_completions", { session_id: session, task_ids: [taskId] });
    await pool.reapIdleSessions(Date.now() + 16 * 60_000);
    expect(routes.size).toBe(0);
    expect(unsubscribed).toBe(1);
  } finally {
    sessionBgStates.delete(session);
    await pool.shutdown();
  }
});
