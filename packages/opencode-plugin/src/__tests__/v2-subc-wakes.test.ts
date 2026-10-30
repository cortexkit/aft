import { afterEach, expect, spyOn, test } from "bun:test";
import { mkdtempSync, realpathSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import {
  type AftTransportFactoryOptions,
  type AftTransportPool,
  acquireBridge,
  type BashPatternMatchAck,
  releaseBridge,
  SubcTransportPool,
} from "@cortexkit/aft-bridge";
import type { BindIdentity, RouteHandle } from "@cortexkit/subc-client";
import { DateTime, Effect, Exit, Scope } from "effect";
import {
  __resetBgNotificationStateForTests,
  appendInTurnBgCompletions,
  ingestBgCompletions,
  markExplicitControl,
  trackBgTask,
} from "../bg-notifications.js";
import { makeServerEffect } from "../entry/server-runtime.mjs";
import * as logger from "../logger.js";
import { __resetV2WakeRoutesForTests, type V2RuntimeConsumer } from "../wakes/runtime-consumer.js";
import { V2SessionDelivery, type V2SessionSyntheticInput } from "../wakes/session-delivery.js";
import { recoverV2SubcSessions } from "../wakes/subc-recovery.js";

const roots: string[] = [];
afterEach(() => {
  __resetV2WakeRoutesForTests();
  __resetBgNotificationStateForTests();
  for (const root of roots.splice(0)) rmSync(root, { recursive: true, force: true });
});
function workspace() {
  // The transport binds the canonical root (on macOS, /var is a link to
  // /private/var), so the module double must key its queues by the same path.
  const root = realpathSync(mkdtempSync(join(tmpdir(), "aft-v2-subc-")));
  roots.push(root);
  return root;
}
const tick = () => new Promise<void>((resolve) => setTimeout(resolve, 5));
async function until(predicate: () => boolean) {
  for (let i = 0; i < 100 && !predicate(); i++) await tick();
  expect(predicate()).toBe(true);
}

/** Only durable drain/ack carries notifications; the subscription is a payload-free hint. */
class ModuleDouble {
  pending = new Map<string, { bg_completions?: unknown[]; pending_matches?: unknown[] }>();
  events: string[] = [];
  clients: ReturnType<ModuleDouble["connect"]>[] = [];
  admissionGate: Promise<void> | undefined;
  acks: BashPatternMatchAck[][] = [];
  key(root: string, session: string) {
    return `${root}\0${session}`;
  }
  connect = () => {
    const routes = new Map<number, BindIdentity>();
    const subscriptions: Array<{
      session: string;
      root: string;
      emit(): void;
      unsubscribe(): void;
    }> = [];
    let channel = 0;
    let closed = false;
    const client = {
      subscriptions,
      routeOpen: async (_target: unknown, identity: BindIdentity) => {
        if (closed) throw new Error("client closed");
        routes.set(++channel, identity);
        return { channel, epoch: channel } as RouteHandle;
      },
      request: async (route: RouteHandle, raw: unknown) => {
        if (closed) throw new Error("client closed");
        const identity = routes.get(route.channel)!;
        const body = raw as {
          name: string;
          arguments: { task_ids?: string[]; matches?: BashPatternMatchAck[] };
        };
        const key = this.key(identity.project_root, identity.session);
        this.events.push(`${body.name}:${identity.session}`);
        if (body.name === "bash_drain_completions") {
          const snapshot = { ...this.pending.get(key) };
          return { structuredContent: { text: "", success: true, ...snapshot }, content: [] };
        }
        if (body.name === "bash_ack_completions") {
          expect(this.events).toContain(`admitted:${identity.session}`);
          const taskIds = new Set(body.arguments.task_ids);
          const exact = (left: BashPatternMatchAck, right: BashPatternMatchAck) =>
            left.task_id === right.task_id &&
            left.watch_id === right.watch_id &&
            left.match_offset === right.match_offset;
          const acked = (body.arguments.matches ?? []).filter((target) =>
            this.pending
              .get(key)
              ?.pending_matches?.some((item) => exact(item as BashPatternMatchAck, target)),
          );
          this.acks.push(body.arguments.matches ?? []);
          const retained = {
            bg_completions: this.pending
              .get(key)
              ?.bg_completions?.filter(
                (item) => !taskIds.has((item as { task_id: string }).task_id),
              ),
            pending_matches: this.pending
              .get(key)
              ?.pending_matches?.filter(
                (item) =>
                  !taskIds.has((item as { task_id: string }).task_id) &&
                  !acked.some((target) => exact(item as BashPatternMatchAck, target)),
              ),
          };
          if (retained.bg_completions?.length || retained.pending_matches?.length)
            this.pending.set(key, retained);
          else this.pending.delete(key);
          return {
            structuredContent: {
              text: "",
              success: true,
              acked_task_ids: body.arguments.task_ids,
              acked_matches: acked,
            },
            content: [],
          };
        }
        return { structuredContent: { success: true, text: "ok" }, content: [] };
      },
      subscribe: (route: RouteHandle, _body: unknown, onEvent: (event: Uint8Array) => void) => {
        const identity = routes.get(route.channel)!;
        let close!: () => void;
        const closed = new Promise<void>((resolve) => {
          close = resolve;
        });
        let active = true;
        const sub = {
          session: identity.session,
          root: identity.project_root,
          emit: () => {
            if (!active) return;
            this.events.push(`nudge:${identity.session}`);
            onEvent(new TextEncoder().encode('{"op":"bg_events"}'));
          },
          unsubscribe: () => {
            active = false;
            close();
          },
          closed,
        };
        subscriptions.push(sub);
        return sub;
      },
      closeRoute: () => {},
      close: async () => {
        closed = true;
        for (const sub of subscriptions) sub.unsubscribe();
      },
    };
    this.clients.push(client);
    return client;
  };
  nudge(session: string) {
    for (const client of this.clients) {
      for (const sub of client.subscriptions) if (sub.session === session) sub.emit();
    }
  }
}
function completion(task = "task") {
  return {
    task_id: task,
    status: "completed",
    exit_code: 0,
    command: "printf done",
    output_preview: "done",
  };
}
function match() {
  return {
    task_id: "watched",
    session_id: "s",
    watch_id: "watch",
    match_text: "ready",
    match_offset: 3,
    context: "ready",
    once: true,
  };
}

async function start(
  root: string,
  module: ModuleDouble,
  recent: unknown[] = [],
  sessionState: { revert?: unknown } = {},
) {
  const synthetic: V2SessionSyntheticInput[] = [];
  let pool!: AftTransportPool;
  let consumer!: V2RuntimeConsumer;
  const afterHooks: Array<(event: unknown) => Effect.Effect<unknown, unknown>> = [];
  const listQueries: unknown[] = [];
  const scope = await Effect.runPromise(Scope.make());
  const context = {
    location: { directory: root, project: { canonical: root } },
    permission: { assert: () => Effect.void },
    tool: {
      hook: (name: string, handler: (event: unknown) => Effect.Effect<unknown, unknown>) =>
        Effect.sync(() => {
          if (name === "execute.after") afterHooks.push(handler);
        }),
    },
    session: {
      get: () => Effect.sync(() => sessionState),
      prompt: () => Effect.void,
      list: (input: unknown) =>
        Effect.sync(() => {
          listQueries.push(input);
          return { data: recent, cursor: undefined };
        }),
      synthetic: (input: V2SessionSyntheticInput) =>
        Effect.promise(async () => {
          await module.admissionGate;
          synthetic.push(input);
          module.events.push(`admitted:${input.sessionID}`);
        }),
    },
  };
  const effect = makeServerEffect({
    loadConfig: () => ({ subc: { connection_file: root } }),
    configLoadErrors: () => [],
    configLoadSources: () => [],
    configLoadTexts: () => new Map(),
    deliverLoadNotices: () => {},
    migrateConfigLocations: () => [],
    ensureStorageMigrated: async () => {},
    ensureOnnxRuntime: async () => null,
    startLspAutoInstall: () => null,
    pushLspPaths: async () => {},
    resolveStorageRoot: () => root,
    buildConfigureParams: () => ({}),
    resolveVersion: () => "0.61.0",
    resolvePoolOptions: () => ({}),
    acquireBridge: async (directory: string, options: AftTransportFactoryOptions) => {
      pool = await acquireBridge(directory, options, {
        createPool: async () =>
          new SubcTransportPool({
            harness: "opencode",
            connectionFile: "unused",
            connect: async () => module.connect(),
            onBgEventsNudge: options.onBgEventsNudge,
            onBgEventsNudgeRef: options.onBgEventsNudgeRef,
          }),
      });
      return pool;
    },
    releaseBridge,
    buildToolMap: () => ({}),
    registerTools: (
      _context: unknown,
      _location: unknown,
      _tools: unknown,
      consumers: V2RuntimeConsumer,
    ) =>
      Effect.sync(() => {
        consumer = consumers;
      }),
    registerRpc: () => Effect.succeed({ dispose: async () => {} }),
    registerPromptHook: () => Effect.void,
    registerWorkflowHints: () => Effect.void,
    startLiveConfigReload: () => ({ stop: () => {} }),
  });
  await Effect.runPromise(effect(context).pipe(Effect.provideService(Scope.Scope, scope)));
  return {
    pool,
    consumer,
    synthetic,
    listQueries,
    toolAfter: async () => {
      const event = {
        status: "completed",
        sessionID: "s",
        tool: "read",
        result: { content: "tool result" },
      };
      for (const hook of afterHooks) await Effect.runPromise(hook(event));
      return event;
    },
    stop: () => Effect.runPromise(Scope.close(scope, Exit.succeed(undefined))),
  };
}

test("OC2 subc idle completion drains after a payload-free nudge and acks only after admission", async () => {
  const root = workspace();
  const module = new ModuleDouble();
  const runtime = await start(root, module);
  try {
    runtime.consumer.registerSession?.("s");
    await runtime.pool.getBridge(root).send("status", { session_id: "s" });
    await tick();
    module.events.length = 0;
    module.pending.set(module.key(root, "s"), { bg_completions: [completion()] });
    let admit!: () => void;
    module.admissionGate = new Promise((resolve) => {
      admit = resolve;
    });
    module.nudge("s");
    await until(() => module.events.includes("bash_drain_completions:s"));
    expect(module.events).not.toContain("bash_ack_completions:s");
    admit();
    await until(() => module.events.includes("bash_ack_completions:s"));
    module.nudge("s");
    await tick();
    expect(runtime.synthetic).toHaveLength(1);
    expect(module.events.indexOf("nudge:s")).toBeLessThan(
      module.events.indexOf("bash_drain_completions:s"),
    );
    expect(module.events.indexOf("admitted:s")).toBeLessThan(
      module.events.indexOf("bash_ack_completions:s"),
    );
  } finally {
    await runtime.stop();
  }
});

test("OC2 subc pattern match wakes once and is acknowledged", async () => {
  const root = workspace();
  const module = new ModuleDouble();
  const runtime = await start(root, module);
  try {
    runtime.consumer.registerSession?.("s");
    await runtime.pool.getBridge(root).send("status", { session_id: "s" });
    await tick();
    module.pending.set(module.key(root, "s"), { pending_matches: [match()] });
    module.nudge("s");
    await until(() => module.events.includes("bash_ack_completions:s"));
    module.nudge("s");
    await tick();
    expect(runtime.synthetic).toHaveLength(1);
    expect(runtime.synthetic[0]?.text).toContain("ready");
    expect(runtime.synthetic[0]?.metadata?.kind).toBe("bash_pattern_match");
    module.pending.set(module.key(root, "s"), { bg_completions: [completion("watched")] });
    module.nudge("s");
    await until(() => runtime.synthetic.length === 2);
    await until(() => !module.pending.has(module.key(root, "s")));
    expect(runtime.synthetic[1]?.metadata?.kind).toBe("bash_completion");
  } finally {
    await runtime.stop();
  }
});

test("OC2 subc Location restart restores an idle recent session without a tool call", async () => {
  const root = workspace();
  const module = new ModuleDouble();
  const first = await start(root, module);
  first.consumer.registerSession?.("s");
  await first.pool.getBridge(root).send("status", { session_id: "s" });
  await first.stop();
  __resetV2WakeRoutesForTests();
  __resetBgNotificationStateForTests();
  module.pending.set(module.key(root, "s"), { bg_completions: [completion()] });
  const second = await start(root, module, [
    { id: "s", location: { directory: root }, time: { updated: DateTime.makeUnsafe(Date.now()) } },
  ]);
  try {
    await until(() => second.synthetic.length === 1);
    await until(() => !module.pending.has(module.key(root, "s")));
    expect(first.synthetic).toHaveLength(0);
    expect(second.synthetic[0]?.sessionID).toBe("s");
    expect(second.listQueries).toEqual([{ directory: root, order: "desc", limit: 20 }]);
  } finally {
    await second.stop();
  }
});

test("OC2 subc nudge and tool-call drain racing admission deliver exactly once", async () => {
  for (const nudgeFirst of [true, false]) {
    const root = workspace();
    const module = new ModuleDouble();
    const runtime = await start(root, module);
    try {
      runtime.consumer.registerSession?.("s");
      await runtime.pool.getBridge(root).send("status", { session_id: "s" });
      await tick();
      module.events.length = 0;
      module.pending.set(module.key(root, "s"), {
        bg_completions: [completion(nudgeFirst ? "task-first" : "task-second")],
      });
      let admit!: () => void;
      module.admissionGate = new Promise((resolve) => {
        admit = resolve;
      });
      let toolCall: ReturnType<typeof runtime.toolAfter> | undefined;
      if (nudgeFirst) module.nudge("s");
      else toolCall = runtime.toolAfter();
      await until(() => module.events.includes("bash_drain_completions:s"));
      if (nudgeFirst) toolCall = runtime.toolAfter();
      else module.nudge("s");
      await tick();
      expect(module.events).not.toContain("bash_ack_completions:s");
      admit();
      expect((await toolCall!).result.content).toBe("tool result");
      await until(() => module.events.includes("bash_ack_completions:s"));
      expect(runtime.synthetic).toHaveLength(1);
      expect(module.events.filter((event) => event === "bash_ack_completions:s")).toHaveLength(1);
    } finally {
      await runtime.stop();
    }
  }
});

test("OC2 subc held completion is delivered once on the replacement Location transport", async () => {
  const root = workspace();
  const module = new ModuleDouble();
  const first = await start(root, module);
  await first.pool.getBridge(root).send("status", { session_id: "s" });
  await tick();
  module.pending.set(module.key(root, "s"), { bg_completions: [completion()] });
  module.nudge("s");
  await until(() => module.events.includes("bash_drain_completions:s"));
  await tick();
  expect(first.synthetic).toHaveLength(0);
  expect(module.events).not.toContain("bash_ack_completions:s");
  await first.stop();
  const second = await start(root, module, [
    { id: "s", location: { directory: root }, time: { updated: DateTime.makeUnsafe(Date.now()) } },
  ]);
  try {
    await until(() => !module.pending.has(module.key(root, "s")));
    expect(second.synthetic).toHaveLength(1);
    module.nudge("s");
    second.consumer.registerSession?.("s");
    await tick();
    expect(second.synthetic).toHaveLength(1);
    expect(module.events.filter((event) => event === "bash_ack_completions:s")).toHaveLength(1);
  } finally {
    await second.stop();
  }
});

for (const notice of ["completion", "async-watch exit"] as const) {
  test(
    notice === "completion"
      ? "OC2 subc a completion admitted by a later nudge is not replayed after reload"
      : "OC2 subc a held completion delivered as an async-watch exit is not replayed after reload",
    async () => {
      const root = workspace();
      const module = new ModuleDouble();
      const state: { revert?: unknown } = {};
      const first = await start(root, module, [], state);
      const warnings = spyOn(logger, "warn").mockImplementation(() => {});
      try {
        first.consumer.registerSession?.("s");
        await first.pool.getBridge(root).send("status", { session_id: "s" });
        await tick();
        state.revert = { messageID: "boundary" };
        module.pending.set(module.key(root, "s"), { bg_completions: [completion()] });
        module.nudge("s");
        await until(() =>
          warnings.mock.calls.some(([message]) =>
            String(message).includes("delivery of task task"),
          ),
        );
        expect(first.synthetic).toHaveLength(0);
        expect(module.events).not.toContain("bash_ack_completions:s");
        state.revert = undefined;
        if (notice === "async-watch exit") markExplicitControl("s", "task");
        module.nudge("s");
        await until(() => !module.pending.has(module.key(root, "s")));
        expect(first.synthetic).toHaveLength(1);
        expect(first.synthetic[0]?.metadata?.kind).toBe(
          notice === "completion" ? "bash_completion" : "bash_pattern_match",
        );
      } finally {
        warnings.mockRestore();
        await first.stop();
      }
      const drains = module.events.filter((event) => event === "bash_drain_completions:s").length;
      const second = await start(root, module, [
        {
          id: "s",
          location: { directory: root },
          time: { updated: DateTime.makeUnsafe(Date.now()) },
        },
      ]);
      try {
        await until(
          () =>
            module.events.filter((event) => event === "bash_drain_completions:s").length > drains,
        );
        await tick();
        expect(second.synthetic).toHaveLength(0);
        expect(module.events.filter((event) => event === "bash_ack_completions:s")).toHaveLength(1);
      } finally {
        await second.stop();
      }
    },
  );
}

test("OC2 subc next tool call registers a session outside the startup recovery window", async () => {
  const root = workspace();
  const module = new ModuleDouble();
  const first = await start(root, module);
  first.consumer.registerSession?.("s");
  await first.stop();
  module.pending.set(module.key(root, "s"), { bg_completions: [completion()] });
  const second = await start(root, module, [
    {
      id: "s",
      location: { directory: root },
      time: { updated: DateTime.makeUnsafe(Date.now() - 25 * 3600_000) },
    },
  ]);
  try {
    await until(() => second.listQueries.length === 1);
    expect(module.clients).toHaveLength(0);
    await second.toolAfter();
    await until(() => !module.pending.has(module.key(root, "s")));
    expect(second.synthetic).toHaveLength(1);
    expect(second.synthetic[0]?.sessionID).toBe("s");
    module.nudge("s");
    await tick();
    expect(second.synthetic).toHaveLength(1);
    expect(module.events.filter((event) => event === "bash_ack_completions:s")).toHaveLength(1);
  } finally {
    await second.stop();
  }
});

test("OC2 subc locally converted async-watch exit wakes and acks once", async () => {
  const root = workspace();
  const module = new ModuleDouble();
  const runtime = await start(root, module);
  try {
    runtime.consumer.registerSession?.("s");
    await runtime.pool.getBridge(root).send("status", { session_id: "s" });
    await tick();
    markExplicitControl("s", "watched");
    module.pending.set(module.key(root, "s"), {
      bg_completions: [completion("watched")],
      pending_matches: [{ ...match(), watch_id: "exit", match_offset: 0, reason: "task_exit" }],
    });
    module.nudge("s");
    await until(() => !module.pending.has(module.key(root, "s")));
    expect(runtime.synthetic).toHaveLength(1);
    expect(runtime.synthetic[0]?.sessionID).toBe("s");
    expect(runtime.synthetic[0]?.text).toContain("task watched exited:");
    expect(runtime.synthetic[0]?.metadata).toMatchObject({
      kind: "bash_pattern_match",
      task_ids: ["watched"],
      watch_id: "exit",
    });
    module.nudge("s");
    await runtime.toolAfter();
    expect(runtime.synthetic).toHaveLength(1);
    expect(module.events.filter((event) => event === "bash_ack_completions:s")).toHaveLength(1);
  } finally {
    await runtime.stop();
  }
});

test("OC2 subc delivers beyond the standalone wake retention limit", async () => {
  const root = workspace();
  const module = new ModuleDouble();
  const runtime = await start(root, module);
  const wakes = runtime.consumer.subcWakes(() => runtime.pool, root);
  try {
    for (let index = 0; index < 4097; index++) {
      module.pending.set(module.key(root, "s"), {
        bg_completions: [completion(`task-${index}`)],
      });
      await wakes.drainSession("s");
    }
    expect(runtime.synthetic).toHaveLength(4097);
    expect(module.pending.has(module.key(root, "s"))).toBe(false);
    expect(module.events.filter((event) => event === "bash_ack_completions:s")).toHaveLength(4097);
  } finally {
    await runtime.stop();
  }
}, 30_000);

test("OC2 subc retroactive async-watch exit retains its session across admission retry", async () => {
  const root = workspace();
  const module = new ModuleDouble();
  const state: { revert?: unknown } = { revert: { messageID: "boundary" } };
  const runtime = await start(root, module, [], state);
  const wakes = runtime.consumer.subcWakes(() => runtime.pool, root);
  try {
    runtime.consumer.registerSession?.("s");
    trackBgTask("s", "watched");
    ingestBgCompletions("s", [completion("watched")]);
    markExplicitControl("s", "watched", false);
    module.pending.set(module.key(root, "s"), { bg_completions: [completion("watched")] });
    await wakes.drainSession("s").catch(() => {});
    expect(runtime.synthetic).toHaveLength(0);
    expect(module.events).not.toContain("bash_ack_completions:s");
    state.revert = undefined;
    module.pending.set(module.key(root, "s"), {
      bg_completions: [completion("watched")],
      pending_matches: [{ ...match(), watch_id: "exit", match_offset: 0, reason: "task_exit" }],
    });
    await wakes.drainSession("s");
    await until(() => !module.pending.has(module.key(root, "s")));
    module.nudge("s");
    await runtime.toolAfter();
    expect(runtime.synthetic).toHaveLength(1);
    expect(runtime.synthetic[0]?.sessionID).toBe("s");
    expect(runtime.synthetic[0]?.text).toContain("task watched exited:");
    expect(module.events.filter((event) => event === "bash_ack_completions:s")).toHaveLength(1);
  } finally {
    await runtime.stop();
  }
});

for (const { nudgeFirst, registered } of [
  { nudgeFirst: true, registered: true },
  { nudgeFirst: false, registered: true },
  { nudgeFirst: true, registered: false },
]) {
  test(`OC2 subc ${registered ? "held" : "unregistered"} completion and async-watch exit racing a tool call wake and ack once (${nudgeFirst ? "nudge first" : "tool first"})`, async () => {
    const root = workspace();
    const module = new ModuleDouble();
    const state: { revert?: unknown } = {};
    const runtime = await start(root, module, [], state);
    const warnings = spyOn(logger, "warn").mockImplementation(() => {});
    let admit!: () => void;
    let toolCall: ReturnType<typeof runtime.toolAfter> | undefined;
    try {
      if (registered) runtime.consumer.registerSession?.("s");
      await runtime.pool.getBridge(root).send("status", { session_id: "s" });
      await tick();
      if (registered) state.revert = { messageID: "boundary" };
      module.pending.set(module.key(root, "s"), { bg_completions: [completion()] });
      module.nudge("s");
      await until(() =>
        warnings.mock.calls.some(([message]) =>
          String(message).includes(registered ? "delivery of task task" : "no OpenCode Location"),
        ),
      );
      expect(runtime.synthetic).toHaveLength(0);
      expect(module.events).not.toContain("bash_ack_completions:s");

      // The first completion could not be admitted. Mark the task as explicitly
      // controlled by an async watch before allowing the tool-call retry.
      markExplicitControl("s", "task");
      state.revert = undefined;
      module.admissionGate = new Promise((resolve) => {
        admit = resolve;
      });
      const drains = module.events.filter((event) => event === "bash_drain_completions:s").length;
      if (nudgeFirst) module.nudge("s");
      else toolCall = runtime.toolAfter();
      await until(
        () => module.events.filter((event) => event === "bash_drain_completions:s").length > drains,
      );
      if (nudgeFirst) toolCall = runtime.toolAfter();
      else module.nudge("s");
      await tick();
      expect(runtime.synthetic).toHaveLength(0);
      expect(module.events).not.toContain("bash_ack_completions:s");
      admit();
      expect((await toolCall!).result.content).toBe("tool result");
      await until(() => !module.pending.has(module.key(root, "s")));
      await tick();
      expect(runtime.synthetic).toHaveLength(1);
      expect(runtime.synthetic[0]?.metadata?.kind).toBe("bash_pattern_match");
      expect(module.events.filter((event) => event === "bash_ack_completions:s")).toHaveLength(1);
      module.nudge("s");
      await runtime.toolAfter();
      expect(runtime.synthetic).toHaveLength(1);
      expect(module.events.filter((event) => event === "bash_ack_completions:s")).toHaveLength(1);
    } finally {
      admit?.();
      await toolCall;
      warnings.mockRestore();
      await runtime.stop();
    }
  });
}

test("OC2 subc recovery is root-scoped, recent, and capped at twenty newest host sessions", async () => {
  const root = workspace();
  const foreign = workspace();
  const recent = Array.from({ length: 25 }, (_, i) => ({
    id: `s${i}`,
    location: { directory: i === 0 ? foreign : root },
    time: { updated: DateTime.makeUnsafe(Date.now() - (i === 1 ? 25 * 3600_000 : i * 1000)) },
  }));
  const queries: unknown[] = [];
  const restored: string[] = [];
  await Effect.runPromise(
    recoverV2SubcSessions(
      {
        session: {
          list: (query: unknown) =>
            Effect.sync(() => {
              queries.push(query);
              return { data: recent, cursor: undefined };
            }),
        },
      },
      root,
      async (sessionID) => {
        restored.push(sessionID);
      },
    ),
  );
  expect(queries).toEqual([{ directory: root, order: "desc", limit: 20 }]);
  expect(restored).toHaveLength(18);
  expect(restored).not.toContain("s0");
  expect(restored).not.toContain("s1");
  expect(restored).not.toContain("s20");
});

test("OC2 subc recovery continues after a rejected restore and wakes the next session", async () => {
  const root = workspace();
  const module = new ModuleDouble();
  module.pending.set(module.key(root, "s"), { bg_completions: [completion()] });
  const runtime = await start(root, module);
  const warnings = spyOn(logger, "warn").mockImplementation(() => {});
  const attempts: string[] = [];
  const recent = ["unavailable", "s"].map((id) => ({
    id,
    location: { directory: root },
    time: { updated: DateTime.makeUnsafe(Date.now()) },
  }));
  const wakes = runtime.consumer.subcWakes(() => runtime.pool, root);
  try {
    await Effect.runPromise(
      recoverV2SubcSessions(
        {
          session: { list: () => Effect.succeed({ data: recent }) },
        },
        root,
        async (sessionID) => {
          attempts.push(sessionID);
          if (sessionID === "unavailable") throw new Error("restore unavailable");
          await wakes.restoreSession(sessionID);
        },
      ),
    );
    await until(() => module.events.includes("bash_ack_completions:s"));
    expect(attempts).toEqual(["unavailable", "s"]);
    expect(runtime.synthetic).toHaveLength(1);
    expect(runtime.synthetic[0]?.sessionID).toBe("s");
    expect(module.pending.has(module.key(root, "s"))).toBe(false);
    expect(
      warnings.mock.calls.some(
        ([message]) =>
          String(message).includes("session recovery failed:") &&
          String(message).includes("restore unavailable"),
      ),
    ).toBe(true);
  } finally {
    warnings.mockRestore();
    await runtime.stop();
  }
});

test("OC2 subc recovery ignores host-listed sessions from another directory without subscribing or draining", async () => {
  const root = workspace();
  const foreign = workspace();
  const module = new ModuleDouble();
  module.pending.set(module.key(foreign, "foreign"), {
    bg_completions: [completion("foreign-task")],
  });
  module.pending.set(module.key(root, "s"), { bg_completions: [completion()] });
  const recent = [
    { id: "foreign", directory: foreign },
    { id: "s", directory: root },
  ].map(({ id, directory }) => ({
    id,
    location: { directory },
    time: { updated: DateTime.makeUnsafe(Date.now()) },
  }));
  const runtime = await start(root, module, recent);
  try {
    await until(() => module.events.includes("bash_ack_completions:s"));
    expect(runtime.synthetic).toHaveLength(1);
    expect(runtime.synthetic[0]?.sessionID).toBe("s");
    expect(
      module.clients
        .flatMap((client) => client.subscriptions)
        .some((sub) => sub.session === "foreign"),
    ).toBe(false);
    expect(module.events).not.toContain("bash_drain_completions:foreign");
    expect(module.events).not.toContain("bash_ack_completions:foreign");
    expect(module.pending.has(module.key(foreign, "foreign"))).toBe(true);
  } finally {
    await runtime.stop();
  }
});

test("OC2 subc admission reservation fences a concurrent in-turn completion drain", async () => {
  const root = workspace();
  const module = new ModuleDouble();
  const runtime = await start(root, module);
  let admit!: () => void;
  try {
    runtime.consumer.registerSession?.("s");
    await runtime.pool.getBridge(root).send("status", { session_id: "s" });
    await tick();
    module.events.length = 0;
    module.pending.set(module.key(root, "s"), { bg_completions: [completion()] });
    module.admissionGate = new Promise((resolve) => {
      admit = resolve;
    });
    module.nudge("s");
    await until(() => module.events.includes("bash_drain_completions:s"));
    trackBgTask("s", "task");
    const output = { output: "tool result" };
    await appendInTurnBgCompletions(
      { ctx: { pool: runtime.pool } as never, directory: root, sessionID: "s" },
      output,
    );
    expect(output.output).toBe("tool result");
    expect(module.events).not.toContain("bash_ack_completions:s");
    admit();
    await until(() => module.events.includes("bash_ack_completions:s"));
    expect(runtime.synthetic).toHaveLength(1);
  } finally {
    admit?.();
    await runtime.stop();
  }
});

test("OC2 subc each_line stale ack at version one does not repeat admitted rows or counters", async () => {
  const root = workspace();
  const module = new ModuleDouble();
  const runtime = await start(root, module);
  let admit!: () => void;
  let firstDrain: Promise<void> | undefined;
  const deliveries = new Set<V2SessionDelivery>();
  const original = V2SessionDelivery.prototype.completion;
  V2SessionDelivery.prototype.completion = function (input) {
    if (input.notificationID?.startsWith("match\0")) deliveries.add(this);
    return original.call(this, input);
  };
  try {
    runtime.consumer.registerSession?.("s");
    await runtime.pool.getBridge(root).send("status", { session_id: "s" });
    await tick();
    module.events.length = 0;
    const firstLine = {
      stream: "stdout",
      offset: 0,
      end_offset: 12,
      text: "READY first",
      count: 1,
    };
    const first = {
      ...match(),
      once: false,
      reason: "pattern_match",
      match_offset: 0,
      batch: {
        pending_id: "subc-pending",
        version: 0,
        snapshot: true,
        lines: [firstLine],
        dropped_lines: 7,
        skipped_long_lines: 0,
        first_dropped_offset: 100,
        first_dropped_stream: "stdout",
      },
    };
    module.pending.set(module.key(root, "s"), { pending_matches: [first] });
    module.admissionGate = new Promise<void>((resolve) => {
      admit = resolve;
    });
    const receiver = runtime.consumer.subcWakes(() => runtime.pool, root);
    firstDrain = receiver.drainSession("s");
    await until(() => module.events.includes("bash_drain_completions:s"));
    module.pending.set(module.key(root, "s"), {
      pending_matches: [
        {
          ...first,
          match_offset: 1,
          batch: {
            ...first.batch,
            version: 1,
            dropped_lines: 10,
            lines: [
              { ...firstLine },
              { stream: "stderr", offset: 0, end_offset: 13, text: "READY second", count: 1 },
            ],
          },
        },
      ],
    });
    admit();
    module.admissionGate = undefined;
    await firstDrain;
    await receiver.drainSession("s");
    expect(runtime.synthetic).toHaveLength(2);
    expect(runtime.synthetic[0]?.text).toContain("stdout byte 0: READY first");
    expect(runtime.synthetic[0]?.text).toContain("7 matching lines dropped.");
    expect(runtime.synthetic[1]?.text).toContain("stderr byte 0: READY second");
    expect(runtime.synthetic[1]?.text).not.toContain("stdout byte 0: READY first");
    expect(runtime.synthetic[1]?.text).toContain("3 matching lines dropped.");
    expect(runtime.synthetic[1]?.text).not.toContain("10 matching lines dropped.");
    expect(module.acks).toEqual([
      [{ task_id: "watched", watch_id: "watch", match_offset: 0 }],
      [{ task_id: "watched", watch_id: "watch", match_offset: 1 }],
    ]);
    expect(module.pending.has(module.key(root, "s"))).toBe(false);
    expect(deliveries.size).toBe(1);
    for (const delivery of deliveries) {
      expect((delivery as unknown as { admittedWakes: Set<string> }).admittedWakes.size).toBe(0);
    }
    await receiver.drainSession("s");
    expect(runtime.synthetic).toHaveLength(2);
  } finally {
    V2SessionDelivery.prototype.completion = original;
    admit?.();
    await firstDrain;
    await runtime.stop();
  }
});
