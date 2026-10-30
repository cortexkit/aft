import { expect, spyOn, test } from "bun:test";
import { Effect, Exit, PubSub, Scope, Stream } from "effect";

import { sessionBgStates, trackBgTask } from "../bg-notifications.js";
import * as logger from "../logger.js";
import { _resetSubagentCacheForTest } from "../shared/subagent-detect.js";
import { isSyncWatchAborted, signalSyncWatchAbort } from "../sync-watch-abort.js";
import { createBashWatchTool } from "../tools/bash_watch.js";
import type { PluginContext } from "../types.js";
import {
  _v2SessionCountForTest,
  allowV2WatchWake,
  awaitV2ExecutionSettlement,
  noteV2ToolAbort,
  registerV2SessionLifecycle,
  trackV2BackgroundTask,
  trackV2Watch,
  v2StopEpoch,
  v2SubagentStopped,
  v2WatchWakeAllowed,
} from "../v2-session-lifecycle.js";
import { __resetV2WakeRoutesForTests, createV2RuntimeConsumer } from "../wakes/runtime-consumer.js";

async function rig(parentID: string | undefined = "parent", persistedTaskIds?: string[]) {
  _resetSubagentCacheForTest();
  const bus = await Effect.runPromise(
    PubSub.unbounded<{ type: string; data: { sessionID: string; reason?: string } }>(),
  );
  const scope = await Effect.runPromise(Scope.make());
  const calls: Array<{ name: string; args: Record<string, unknown> }> = [];
  const host = {
    location: { directory: process.cwd() },
    event: { subscribe: () => Stream.fromPubSub(bus) },
    session: { get: () => Effect.succeed({ parentID }) },
  };
  const bridge = {
    send: async (name: string, args: Record<string, unknown>) => {
      calls.push({ name, args });
      return name === "bash_owned_tasks"
        ? { success: true, task_ids: persistedTaskIds ?? [] }
        : { success: true };
    },
  };
  const ctx = {
    client: host,
    pool: { getBridge: () => bridge, getActiveBridgeForRoot: () => bridge },
    config: {},
  } as unknown as PluginContext;
  await Effect.runPromise(
    registerV2SessionLifecycle(host, persistedTaskIds === undefined ? undefined : ctx).pipe(
      Effect.provideService(Scope.Scope, scope),
    ),
  );
  return {
    host,
    ctx,
    calls,
    async event(type: string, reason?: string) {
      await Effect.runPromise(PubSub.publish(bus, { type, data: { sessionID: "child", reason } }));
      await Bun.sleep(10);
    },
    async close() {
      await Effect.runPromise(Scope.close(scope, Exit.succeed(undefined)));
    },
  };
}

for (const reason of ["user", "inactivity"] as const) {
  test(`V2 deliberate ${reason} stop cancels registered and late subagent watches`, async () => {
    const r = await rig();
    try {
      const runtime = { sessionID: "child", directory: process.cwd() };
      await trackV2Watch(r.ctx, runtime, "bash-running", "watch-1");
      noteV2ToolAbort(r.host, "child");
      let settled = false;
      const pending = awaitV2ExecutionSettlement(r.host, "child").then(() => {
        settled = true;
      });
      await Bun.sleep(10);
      expect(settled).toBe(false);
      await r.event("session.execution.interrupted", reason);
      await pending;
      expect(v2SubagentStopped(r.host, "child")).toBe(true);
      expect(v2WatchWakeAllowed(r.host, "child", "bash-running")).toBe(false);
      expect(r.calls.map((call) => call.name)).toEqual(["bash_unnotify"]);
      await trackV2Watch(r.ctx, runtime, "bash-late", "watch-2");
      expect(r.calls.map((call) => call.name)).toEqual(["bash_unnotify", "bash_unnotify"]);
      await r.event("session.execution.started");
      expect(v2SubagentStopped(r.host, "child")).toBe(false);
      expect(v2WatchWakeAllowed(r.host, "child", "bash-running")).toBe(false);
      allowV2WatchWake(r.host, "child", "bash-resumed");
      expect(v2WatchWakeAllowed(r.host, "child", "bash-resumed")).toBe(true);
    } finally {
      await r.close();
    }
  });
}

for (const [type, reason] of [
  ["session.execution.interrupted", "shutdown"],
  ["session.execution.interrupted", "superseded"],
  ["session.execution.failed", undefined],
] as const) {
  test(`V2 ${type}/${reason ?? "provider error"} retains watch wakes`, async () => {
    const r = await rig();
    try {
      await trackV2Watch(
        r.ctx,
        { sessionID: "child", directory: process.cwd() },
        "bash-running",
        "watch-1",
      );
      noteV2ToolAbort(r.host, "child");
      await r.event(type, reason);
      await awaitV2ExecutionSettlement(r.host, "child");
      expect(v2SubagentStopped(r.host, "child")).toBe(false);
      expect(v2WatchWakeAllowed(r.host, "child", "bash-running")).toBe(true);
      expect(r.calls).toEqual([]);
    } finally {
      await r.close();
    }
  });
}

test("V2 main-session stop retains registered watch", async () => {
  const r = await rig("");
  try {
    await trackV2Watch(
      r.ctx,
      { sessionID: "child", directory: process.cwd() },
      "bash-running",
      "watch-1",
    );
    await r.event("session.execution.interrupted", "user");
    expect(v2SubagentStopped(r.host, "child")).toBe(false);
    expect(r.calls).toEqual([]);
  } finally {
    await r.close();
  }
});

test("cancelled worker watch does not acknowledge a racing inline completion", async () => {
  const r = await rig();
  try {
    const abort = new AbortController();
    const commands: string[] = [];
    const metadata: Array<Record<string, unknown>> = [];
    const bridge = r.ctx.pool.getBridge(process.cwd());
    bridge.send = async (command) => {
      commands.push(command);
      abort.abort();
      return {
        success: true,
        status: "completed",
        output: "racing-completion",
        task_id: "bash-race",
      };
    };
    await createBashWatchTool(r.ctx).execute({ taskId: "bash-race" }, {
      sessionID: "child",
      directory: process.cwd(),
      abort: abort.signal,
      // The bash tools send OpenCode's { title?, metadata? } update shape.
      metadata: (value: { metadata?: Record<string, unknown> }) =>
        metadata.push(value.metadata ?? {}),
    } as never);
    expect(commands).toEqual(["bash_status"]);
    expect(metadata.at(-1)?.waited).toMatchObject({ reason: "aborted" });
  } finally {
    await r.close();
  }
});

for (const reason of ["user", "inactivity"] as const) {
  test(`V2 ${reason} stop kills only its session's background tasks`, async () => {
    const r = await rig();
    try {
      for (const owner of ["parent", "sibling", "child"]) {
        await trackV2BackgroundTask(
          r.ctx,
          { sessionID: owner, directory: process.cwd() },
          `bash-${owner}`,
          v2StopEpoch(r.host, owner),
        );
      }
      await r.event("session.execution.interrupted", reason);
      const kills = r.calls.filter((call) => call.name === "bash_kill");
      expect(kills.map((call) => call.args.task_id)).toEqual(["bash-child"]);
      expect(kills.map((call) => call.args.session_id)).toEqual(["child"]);
    } finally {
      await r.close();
    }
  });
}

for (const [type, reason] of [
  ["session.execution.interrupted", "shutdown"],
  ["session.execution.interrupted", "superseded"],
  ["session.execution.failed", undefined],
  ["session.execution.succeeded", undefined],
] as const) {
  test(`V2 ${type}/${reason ?? "terminal"} kills no background tasks`, async () => {
    const r = await rig();
    try {
      await trackV2BackgroundTask(
        r.ctx,
        { sessionID: "child", directory: process.cwd() },
        "bash-child",
        v2StopEpoch(r.host, "child"),
      );
      await r.event(type, reason);
      expect(r.calls.filter((call) => call.name === "bash_kill")).toEqual([]);
      expect(v2StopEpoch(r.host, "child")).toBe(0);
    } finally {
      await r.close();
    }
  });
}

test("V2 stop kills a late background reply without killing resumed execution's task", async () => {
  const r = await rig();
  try {
    const runtime = { sessionID: "child", directory: process.cwd() };
    const oldEpoch = v2StopEpoch(r.host, "child");
    await r.event("session.execution.interrupted", "user");
    await r.event("session.execution.started");
    await trackV2BackgroundTask(r.ctx, runtime, "bash-late", oldEpoch);
    await trackV2BackgroundTask(r.ctx, runtime, "bash-resumed", v2StopEpoch(r.host, "child"));
    expect(
      r.calls.filter((call) => call.name === "bash_kill").map((call) => call.args.task_id),
    ).toEqual(["bash-late"]);
  } finally {
    await r.close();
  }
});

test("background task tracking without V2 lifecycle registration never kills", async () => {
  const r = await rig();
  try {
    const ctx = { ...r.ctx, client: {} } as unknown as PluginContext;
    await trackV2BackgroundTask(
      ctx,
      { sessionID: "child", directory: process.cwd() },
      "bash-v1",
      0,
    );
    expect(r.calls).toEqual([]);
  } finally {
    await r.close();
  }
});

test("V2 main-session interruption kills no background tasks", async () => {
  const r = await rig("");
  try {
    await trackV2BackgroundTask(
      r.ctx,
      { sessionID: "child", directory: process.cwd() },
      "bash-main",
      0,
    );
    await r.event("session.execution.interrupted", "user");
    expect(r.calls.filter((call) => call.name === "bash_kill")).toEqual([]);
  } finally {
    await r.close();
  }
});

test("V2 stopping a watcher does not kill the task spawned by its parent", async () => {
  const r = await rig();
  try {
    await trackV2BackgroundTask(
      r.ctx,
      { sessionID: "parent", directory: process.cwd() },
      "bash-parent",
      0,
    );
    await trackV2Watch(
      r.ctx,
      { sessionID: "child", directory: process.cwd() },
      "bash-parent",
      "watch-parent",
    );
    await r.event("session.execution.interrupted", "user");
    expect(r.calls.filter((call) => call.name === "bash_unnotify")).toHaveLength(1);
    expect(r.calls.filter((call) => call.name === "bash_kill")).toEqual([]);
  } finally {
    await r.close();
  }
});

test("V2 stop enumerates persisted tasks under the stopped owner's identity after reload", async () => {
  const r = await rig("parent", ["bash-before-reload"]);
  try {
    await r.event("session.execution.interrupted", "user");
    expect(r.calls.map((call) => call.name)).toEqual(["bash_owned_tasks", "bash_kill"]);
    expect(r.calls.map((call) => call.args.session_id)).toEqual(["child", "child"]);
    expect(r.calls.at(-1)?.args.task_id).toBe("bash-before-reload");
  } finally {
    await r.close();
  }
});

test("V2 stop unions in-memory and persisted task ids without duplicate kills", async () => {
  const r = await rig("parent", ["bash-live", "bash-before-reload"]);
  try {
    await trackV2BackgroundTask(
      r.ctx,
      { sessionID: "child", directory: process.cwd() },
      "bash-live",
      0,
    );
    await r.event("session.execution.interrupted", "user");
    const killed = r.calls
      .filter((call) => call.name === "bash_kill")
      .map((call) => call.args.task_id);
    expect(killed).toEqual(["bash-live", "bash-before-reload"]);
  } finally {
    await r.close();
  }
});

test("V2 unknown role never skips stop cleanup after a failed lookup", async () => {
  const r = await rig();
  r.host.session.get = () => Effect.die(new Error("host lookup unavailable"));
  try {
    await trackV2BackgroundTask(
      r.ctx,
      { sessionID: "child", directory: process.cwd() },
      "bash-unknown",
      0,
    );
    await r.event("session.execution.interrupted", "user");
    expect(
      r.calls.filter((call) => call.name === "bash_kill").map((call) => call.args.task_id),
    ).toEqual(["bash-unknown"]);
  } finally {
    await r.close();
  }
});

test("V2 ended event subscription logs the Location once", async () => {
  const logged = spyOn(logger, "error");
  logged.mockClear();
  const scope = await Effect.runPromise(Scope.make());
  try {
    await Effect.runPromise(
      registerV2SessionLifecycle({
        location: { directory: "/work/events-ended" },
        event: { subscribe: () => Stream.empty },
      }).pipe(Effect.provideService(Scope.Scope, scope)),
    );
    await Bun.sleep(10);
    const lines = logged.mock.calls
      .map((call) => String(call[0]))
      .filter((line) => line.includes("event") && line.includes("/work/events-ended"));
    expect(lines).toHaveLength(1);
  } finally {
    await Effect.runPromise(Scope.close(scope, Exit.void));
    logged.mockRestore();
  }
});

test("V2 session deletion releases watches, tasks, settlement and auxiliary session maps", async () => {
  const r = await rig();
  try {
    const runtime = { sessionID: "child", directory: process.cwd() };
    await trackV2BackgroundTask(r.ctx, runtime, "bash-deleted", 0);
    await trackV2Watch(r.ctx, runtime, "bash-deleted", "watch-deleted");
    noteV2ToolAbort(r.host, "child");
    let settled = false;
    void awaitV2ExecutionSettlement(r.host, "child").then(() => {
      settled = true;
    });
    signalSyncWatchAbort("child");
    signalSyncWatchAbort("sibling");
    trackBgTask("child", "bash-deleted");
    trackBgTask("sibling", "bash-retained");
    await r.event("session.deleted");
    expect(settled).toBe(true);
    expect(isSyncWatchAborted("child")).toBe(false);
    expect(isSyncWatchAborted("sibling")).toBe(true);
    expect(sessionBgStates.has("child")).toBe(false);
    expect(sessionBgStates.has("sibling")).toBe(true);
    expect(r.calls.map((call) => call.name).sort()).toEqual(["bash_kill", "bash_unnotify"]);
    expect(v2WatchWakeAllowed(r.host, "child", "bash-deleted")).toBe(false);
    expect(_v2SessionCountForTest(r.host)).toBe(0);
  } finally {
    await r.close();
    sessionBgStates.clear();
  }
});

test("V2 replies arriving after session deletion cannot recreate session state", async () => {
  const r = await rig();
  try {
    const runtime = { sessionID: "child", directory: process.cwd() };
    const epoch = v2StopEpoch(r.host, "child");
    await r.event("session.deleted");
    await trackV2BackgroundTask(r.ctx, runtime, "bash-late-deleted", epoch);
    await trackV2Watch(r.ctx, runtime, "bash-late-deleted", "watch-late-deleted");
    expect(r.calls.map((call) => call.name)).toEqual(["bash_kill", "bash_unnotify"]);
    expect(_v2SessionCountForTest(r.host)).toBe(0);
    expect(sessionBgStates.has("child")).toBe(false);
  } finally {
    await r.close();
    sessionBgStates.clear();
  }
});

test("V2 session deletion after a reload kills only the tasks the bridge says that session owns", async () => {
  _resetSubagentCacheForTest();
  const bus = await Effect.runPromise(
    PubSub.unbounded<{ type: string; data: { sessionID: string } }>(),
  );
  const scope = await Effect.runPromise(Scope.make());
  const calls: Array<{ name: string; args: Record<string, unknown> }> = [];
  // Persisted task ownership as the engine keeps it; after a plugin reload
  // nothing about these tasks is held in memory.
  const owned: Record<string, string[]> = {
    "deleted-child": ["bash-deleted-owned"],
    "live-sibling": ["bash-sibling-owned"],
  };
  const bridge = {
    // Mirrors the engine's bash_owned_tasks reply: the running tasks of the
    // session the request is bound to, never another session's.
    send: async (name: string, args: Record<string, unknown>) => {
      calls.push({ name, args });
      if (name === "bash_owned_tasks") {
        const session = String(args.session_id ?? "");
        return { success: true, task_ids: owned[session] ?? [], skipped_missing_owner: 0 };
      }
      return { success: true };
    },
  };
  const host = {
    location: { directory: process.cwd() },
    event: { subscribe: () => Stream.fromPubSub(bus) },
    session: { get: () => Effect.succeed({ parentID: "parent" }) },
  };
  const ctx = {
    client: host,
    pool: { getBridge: () => bridge, getActiveBridgeForRoot: () => bridge },
    config: {},
  } as unknown as PluginContext;
  try {
    await Effect.runPromise(
      registerV2SessionLifecycle(host, ctx).pipe(Effect.provideService(Scope.Scope, scope)),
    );
    await Effect.runPromise(
      PubSub.publish(bus, { type: "session.deleted", data: { sessionID: "deleted-child" } }),
    );
    await Bun.sleep(20);
    const kills = calls.filter((call) => call.name === "bash_kill");
    expect(kills.map((call) => call.args.task_id)).toEqual(["bash-deleted-owned"]);
    expect(kills.every((call) => call.args.session_id === "deleted-child")).toBe(true);
    expect(
      calls.some(
        (call) => call.name === "bash_owned_tasks" && call.args.session_id === "deleted-child",
      ),
    ).toBe(true);
  } finally {
    await Effect.runPromise(Scope.close(scope, Exit.void));
    sessionBgStates.clear();
  }
});

test("V2 completion arriving after session deletion creates no session state and is acknowledged", async () => {
  _resetSubagentCacheForTest();
  __resetV2WakeRoutesForTests();
  const bus = await Effect.runPromise(
    PubSub.unbounded<{ type: string; data: { sessionID: string } }>(),
  );
  const scope = await Effect.runPromise(Scope.make());
  const synthetic: unknown[] = [];
  const host = {
    location: { directory: process.cwd() },
    event: { subscribe: () => Stream.fromPubSub(bus) },
    session: {
      get: () => Effect.succeed({ parentID: "parent" }),
      prompt: () => Effect.succeed({ id: "wake" }),
      synthetic: (input: unknown) =>
        Effect.sync(() => {
          synthetic.push(input);
          return { id: "status" };
        }),
    },
  };
  const sends: Array<{ command: string; params: Record<string, unknown> }> = [];
  const bridge = {
    getCwd: () => process.cwd(),
    send: async (command: string, params: Record<string, unknown>) => {
      sends.push({ command, params });
      return { success: true };
    },
  };
  const consumer = createV2RuntimeConsumer(host as never);
  try {
    await Effect.runPromise(
      registerV2SessionLifecycle(host).pipe(Effect.provideService(Scope.Scope, scope)),
    );
    consumer.registerSession?.("deleted-child");
    await Effect.runPromise(
      PubSub.publish(bus, { type: "session.deleted", data: { sessionID: "deleted-child" } }),
    );
    await Bun.sleep(10);
    await consumer.bridgeOptions.onBashCompletion?.(
      {
        type: "bash_completed",
        session_id: "deleted-child",
        task_id: "bash-late-child",
        status: "completed",
        exit_code: 0,
        command: "sleep 1",
        output_preview: "done",
      } as never,
      bridge as never,
    );
    expect(sessionBgStates.has("deleted-child")).toBe(false);
    expect(synthetic).toEqual([]);
    expect(sends).toEqual([
      {
        command: "bash_ack_completions",
        params: { session_id: "deleted-child", task_ids: ["bash-late-child"] },
      },
    ]);
  } finally {
    consumer.dispose();
    __resetV2WakeRoutesForTests();
    await Effect.runPromise(Scope.close(scope, Exit.void));
    sessionBgStates.clear();
  }
});

test("V2 watch match arriving after session deletion is acknowledged without a wake", async () => {
  _resetSubagentCacheForTest();
  __resetV2WakeRoutesForTests();
  const bus = await Effect.runPromise(
    PubSub.unbounded<{ type: string; data: { sessionID: string } }>(),
  );
  const scope = await Effect.runPromise(Scope.make());
  const synthetic: unknown[] = [];
  const host = {
    location: { directory: process.cwd() },
    event: { subscribe: () => Stream.fromPubSub(bus) },
    session: {
      get: () => Effect.succeed({ parentID: "parent" }),
      prompt: () => Effect.succeed({ id: "wake" }),
      synthetic: (input: unknown) =>
        Effect.sync(() => {
          synthetic.push(input);
          return { id: "status" };
        }),
    },
  };
  const sends: Array<{ command: string; params: Record<string, unknown> }> = [];
  const bridge = {
    getCwd: () => process.cwd(),
    send: async (command: string, params: Record<string, unknown>) => {
      sends.push({ command, params });
      return { success: true };
    },
  };
  const consumer = createV2RuntimeConsumer(host as never);
  try {
    await Effect.runPromise(
      registerV2SessionLifecycle(host).pipe(Effect.provideService(Scope.Scope, scope)),
    );
    consumer.registerSession?.("deleted-child");
    await Effect.runPromise(
      PubSub.publish(bus, { type: "session.deleted", data: { sessionID: "deleted-child" } }),
    );
    await Bun.sleep(10);
    await consumer.bridgeOptions.onBashPatternMatch?.(
      {
        type: "bash_pattern_match",
        session_id: "deleted-child",
        task_id: "bash-late-child",
        watch_id: "watch-late",
        match_text: "ready",
        match_offset: 12,
        context: "server ready",
        once: false,
        reason: "pattern_match",
      } as never,
      bridge as never,
    );
    expect(sessionBgStates.has("deleted-child")).toBe(false);
    expect(synthetic).toEqual([]);
    expect(sends).toEqual([
      {
        command: "bash_ack_completions",
        params: {
          session_id: "deleted-child",
          matches: [{ task_id: "bash-late-child", watch_id: "watch-late", match_offset: 12 }],
        },
      },
    ]);
  } finally {
    consumer.dispose();
    __resetV2WakeRoutesForTests();
    await Effect.runPromise(Scope.close(scope, Exit.void));
    sessionBgStates.clear();
  }
});

test("V2 subc drain for a deleted session acknowledges its completion and watch match and leaves no session state", async () => {
  _resetSubagentCacheForTest();
  __resetV2WakeRoutesForTests();
  const bus = await Effect.runPromise(
    PubSub.unbounded<{ type: string; data: { sessionID: string } }>(),
  );
  const scope = await Effect.runPromise(Scope.make());
  const synthetic: unknown[] = [];
  const host = {
    location: { directory: process.cwd() },
    event: { subscribe: () => Stream.fromPubSub(bus) },
    session: {
      get: () => Effect.succeed({ parentID: "parent" }),
      prompt: () => Effect.succeed({ id: "wake" }),
      synthetic: (input: unknown) =>
        Effect.sync(() => {
          synthetic.push(input);
          return { id: "status" };
        }),
    },
  };
  const sends: Array<{ command: string; params: Record<string, unknown> }> = [];
  let pending = [
    {
      task_id: "bash-drained-late",
      status: "completed",
      exit_code: 0,
      command: "sleep 1",
      output_preview: "done",
    },
  ];
  let matches = [
    {
      task_id: "bash-watched-late",
      session_id: "drained-child",
      watch_id: "watch-late",
      match_text: "ready",
      match_offset: 0,
      context: "ready",
      once: true,
    },
  ];
  const bridge = {
    getCwd: () => process.cwd(),
    send: async (command: string, params: Record<string, unknown>) => {
      sends.push({ command, params });
      if (command === "bash_drain_completions")
        return { success: true, bg_completions: pending, pending_matches: matches };
      if (command === "bash_ack_completions") {
        const acked = new Set((params.task_ids as string[] | undefined) ?? []);
        const ackedMatches = new Set(
          ((params.matches as Array<{ task_id: string }> | undefined) ?? []).map(
            (match) => match.task_id,
          ),
        );
        pending = pending.filter((item) => !acked.has(item.task_id));
        matches = matches.filter(
          (item) => !acked.has(item.task_id) && !ackedMatches.has(item.task_id),
        );
      }
      return { success: true };
    },
  };
  const consumer = createV2RuntimeConsumer(host as never);
  try {
    await Effect.runPromise(
      registerV2SessionLifecycle(host).pipe(Effect.provideService(Scope.Scope, scope)),
    );
    await Effect.runPromise(
      PubSub.publish(bus, { type: "session.deleted", data: { sessionID: "drained-child" } }),
    );
    await Bun.sleep(10);
    const wakes = consumer.subcWakes(
      () => ({ getBridge: () => bridge, getActiveBridgeForRoot: () => bridge }) as never,
      process.cwd(),
    );
    await wakes.drainSession("drained-child");
    expect(synthetic).toEqual([]);
    // The completion is acked by task id so the bridge never redelivers it.
    expect(
      sends.some(
        (send) =>
          send.command === "bash_ack_completions" &&
          ((send.params.task_ids as string[] | undefined) ?? []).includes("bash-drained-late"),
      ),
    ).toBe(true);
    // The watch match is acked in the matches shape (task, watch, offset),
    // not as a completion, so the deleted session's match is retired
    // without queueing a wake for a session that no longer exists.
    const matchAcks = sends.filter(
      (send) => send.command === "bash_ack_completions" && Array.isArray(send.params.matches),
    );
    expect(matchAcks).toHaveLength(1);
    expect(matchAcks[0]?.params).toEqual({
      session_id: "drained-child",
      matches: [{ task_id: "bash-watched-late", watch_id: "watch-late", match_offset: 0 }],
    });
    expect(sessionBgStates.has("drained-child")).toBe(false);
  } finally {
    consumer.dispose();
    __resetV2WakeRoutesForTests();
    await Effect.runPromise(Scope.close(scope, Exit.void));
    sessionBgStates.clear();
  }
});
