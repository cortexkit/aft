import { afterEach, expect, spyOn, test } from "bun:test";
import { Effect, Exit, PubSub, Scope, Stream } from "effect";

import { sessionBgStates } from "../bg-notifications.js";
import * as logger from "../logger.js";
import { releaseSessionState } from "../shared/session-cleanup.js";
import { _resetSubagentCacheForTest } from "../shared/subagent-detect.js";
import { allowV2WatchWake, registerV2SessionLifecycle } from "../v2-session-lifecycle.js";
import { registerV2ToolHooks } from "../v2-tool-hooks.js";
import { __resetV2WakeRoutesForTests, createV2RuntimeConsumer } from "../wakes/runtime-consumer.js";
import type { V2SessionSyntheticInput } from "../wakes/session-delivery.js";

const sessionID = "abort-deadlock-session";
const taskID = "bash-aborted";
const directory = process.cwd();
const completion = {
  task_id: taskID,
  status: "fate_unknown",
  exit_code: null,
  command: "sleep 60",
  output_preview: "module restarted",
};

function deferred() {
  let resolve = () => {};
  const promise = new Promise<void>((done) => {
    resolve = done;
  });
  return { promise, resolve };
}

// A deadlocked hook never returns, so a generous bound still tells the two
// apart on a loaded runner. Only the check that a call is still blocked uses a
// short bound, since waiting longer there proves nothing more.
const SETTLES_WITHIN_MS = 5_000;
const STILL_BLOCKED_AFTER_MS = 250;

async function promptly(
  work: Promise<unknown>,
  boundMs: number = SETTLES_WITHIN_MS,
): Promise<boolean> {
  let timer: ReturnType<typeof setTimeout> | undefined;
  try {
    return await Promise.race([
      work.then(() => true),
      new Promise<false>((resolve) => {
        timer = setTimeout(() => resolve(false), boundMs);
      }),
    ]);
  } finally {
    clearTimeout(timer);
  }
}

async function until(condition: () => boolean): Promise<void> {
  for (let i = 0; i < 100 && !condition(); i++) await Bun.sleep(5);
  expect(condition()).toBe(true);
}

function durableBridge() {
  const pending = new Map([[taskID, completion]]);
  const acks: string[][] = [];
  let rejectAck = false;
  return {
    pending,
    acks,
    failNextAck: () => {
      rejectAck = true;
    },
    bridge: {
      getCwd: () => directory,
      send: async (name: string, args: { task_ids?: string[] }) => {
        if (name === "bash_drain_completions") {
          return { success: true, bg_completions: [...pending.values()], pending_matches: [] };
        }
        if (name === "bash_ack_completions") {
          acks.push(args.task_ids ?? []);
          if (rejectAck) {
            rejectAck = false;
            return { success: false, message: "ack unavailable" };
          }
          return {
            success: true,
            acked_task_ids: (args.task_ids ?? []).filter((id) => pending.delete(id)),
          };
        }
        return { success: true };
      },
    },
  };
}

async function rig(
  parentID: string | undefined = "parent",
  durable = durableBridge(),
  wakeAllowed = true,
) {
  const bus = await Effect.runPromise(
    PubSub.unbounded<{ type: string; data: { sessionID: string; reason?: string } }>(),
  );
  const scope = await Effect.runPromise(Scope.make());
  const hooks = new Map<string, Array<(event: any) => Effect.Effect<unknown>>>();
  const synthetic: V2SessionSyntheticInput[] = [];
  const admissionEntered = deferred();
  let admissionGate: Promise<void> | undefined;
  const host = {
    location: { directory },
    event: { subscribe: () => Stream.fromPubSub(bus) },
    session: {
      get: () => Effect.succeed({ parentID }),
      prompt: () => Effect.void,
      synthetic: (input: V2SessionSyntheticInput) =>
        Effect.promise(async () => {
          admissionEntered.resolve();
          await admissionGate;
          synthetic.push(input);
          return { id: "wake" };
        }),
    },
    tool: {
      hook: (name: string, callback: (event: any) => Effect.Effect<unknown>) =>
        Effect.sync(() => hooks.set(name, [...(hooks.get(name) ?? []), callback])),
    },
  };
  const pool = { getBridge: () => durable.bridge, getActiveBridgeForRoot: () => durable.bridge };
  const consumer = createV2RuntimeConsumer(host);
  const wakes = consumer.subcWakes(() => pool as never, directory);
  await Effect.runPromise(
    registerV2SessionLifecycle(host).pipe(Effect.provideService(Scope.Scope, scope)),
  );
  await Effect.runPromise(
    registerV2ToolHooks(
      host,
      { pool: pool as never, drainBgNotifications: wakes.drainSession },
      new Set(["bash"]),
    ),
  );
  consumer.registerSession(sessionID, durable.bridge as never);
  if (wakeAllowed) allowV2WatchWake(host, sessionID, taskID);
  return {
    consumer,
    durable,
    synthetic,
    admissionEntered: admissionEntered.promise,
    gateAdmission: (gate: Promise<void>) => {
      admissionGate = gate;
    },
    nudge: () => Promise.resolve(wakes.handlers.onBgEventsNudge(directory, sessionID)),
    after: async () => {
      const event = {
        tool: "shell",
        sessionID,
        status: "completed",
        result: { content: "second tool finished" },
      };
      for (const hook of hooks.get("execute.after") ?? []) await Effect.runPromise(hook(event));
      return event;
    },
    event: async (type: string) => {
      await Effect.runPromise(PubSub.publish(bus, { type, data: { sessionID } }));
    },
    close: async () => {
      consumer.dispose();
      await Effect.runPromise(Scope.close(scope, Exit.succeed(undefined)));
    },
  };
}

async function parkCompletion(r: Awaited<ReturnType<typeof rig>>) {
  r.consumer.onToolAbort?.(sessionID);
  const handling = r.nudge();
  await until(() => sessionBgStates.get(sessionID)?.deliveringTaskIds.has(taskID) === true);
  expect(r.synthetic).toHaveLength(0);
  expect(r.durable.acks).toHaveLength(0);
  return { handling };
}

afterEach(() => {
  releaseSessionState(sessionID);
  __resetV2WakeRoutesForTests();
  _resetSubagentCacheForTest();
});

test("V2 abort settlement: a second subagent tool finishes while a drained completion is parked", async () => {
  const r = await rig("parent", durableBridge(), false);
  let handling: Promise<void> | undefined;
  try {
    ({ handling } = await parkCompletion(r));
    expect(await promptly(r.after())).toBe(true);
    expect(r.synthetic).toHaveLength(0);
    expect(r.durable.pending.has(taskID)).toBe(true);
    await r.event("session.execution.succeeded");
    await handling;
    expect(r.durable.acks).toHaveLength(0);
    await r.event("session.execution.started");
    const next = await r.after();
    expect(next.result.content).toContain(taskID);
    expect((await r.after()).result.content).not.toContain(taskID);
    expect(r.synthetic).toHaveLength(0);
    expect(r.durable.acks).toEqual([[taskID]]);
    expect(r.durable.pending.size).toBe(0);
  } finally {
    await r.close();
    await handling;
  }
});

test("V2 abort settlement: a parked completion is delivered once after execution end despite replay and ack failure", async () => {
  const r = await rig();
  const handling: Promise<unknown>[] = [];
  const warn = spyOn(logger, "sessionWarn").mockImplementation(() => {});
  try {
    handling.push((await parkCompletion(r)).handling);
    handling.push(r.nudge());
    expect(await promptly(r.after())).toBe(true);
    expect(r.synthetic).toHaveLength(0);
    r.durable.failNextAck();
    await r.event("session.execution.failed");
    await Promise.all(handling);
    expect(r.synthetic).toHaveLength(1);
    expect(r.synthetic[0]?.metadata).toMatchObject({ task_ids: [taskID], kind: "bash_completion" });
    expect(r.durable.pending.has(taskID)).toBe(true);
    await r.nudge();
    await r.nudge();
    await r.after();
    expect(r.synthetic).toHaveLength(1);
    expect(r.durable.acks).toEqual([[taskID], [taskID]]);
    expect(r.durable.pending.size).toBe(0);
  } finally {
    await r.close();
    await Promise.all(handling);
    warn.mockRestore();
  }
});

test("V2 abort settlement: a disappearing Location leaves its parked completion for its replacement", async () => {
  const r = await rig();
  const { handling } = await parkCompletion(r);
  let replacement: Awaited<ReturnType<typeof rig>> | undefined;
  try {
    expect(await promptly(r.after())).toBe(true);
    await r.close();
    await handling;
    expect(r.synthetic).toHaveLength(0);
    expect(r.durable.acks).toHaveLength(0);
    expect(r.durable.pending.has(taskID)).toBe(true);
    releaseSessionState(sessionID);
    replacement = await rig("parent", r.durable);
    await replacement.nudge();
    await replacement.nudge();
    expect(replacement.synthetic).toHaveLength(1);
    expect(r.durable.acks).toEqual([[taskID]]);
    expect(r.durable.pending.size).toBe(0);
  } finally {
    await r.close();
    await handling;
    await replacement?.close();
  }
});

test("V2 abort settlement: deletion retires a parked completion without recreating deleted session state", async () => {
  const r = await rig();
  const { handling } = await parkCompletion(r);
  try {
    await r.event("session.deleted");
    await handling;
    expect(r.synthetic).toHaveLength(0);
    expect(r.durable.acks).toEqual([[taskID]]);
    expect(r.durable.pending.size).toBe(0);
    expect(sessionBgStates.has(sessionID)).toBe(false);
  } finally {
    await r.close();
    await handling;
  }
});

test("V2 abort settlement: a primary tool still joins admission and ack without waiting for execution end", async () => {
  const r = await rig("");
  const gate = deferred();
  let handling: Promise<void> | undefined;
  let after: Promise<unknown> | undefined;
  try {
    r.gateAdmission(gate.promise);
    r.consumer.onToolAbort?.(sessionID);
    handling = r.nudge();
    await r.admissionEntered;
    after = r.after();
    expect(await promptly(after, STILL_BLOCKED_AFTER_MS)).toBe(false);
    gate.resolve();
    expect(await promptly(after)).toBe(true);
    await handling;
    expect(r.synthetic).toHaveLength(1);
    expect(r.durable.acks).toEqual([[taskID]]);
    await r.nudge();
    expect(r.synthetic).toHaveLength(1);
  } finally {
    gate.resolve();
    await r.close();
    await handling;
    await after;
  }
});

test("V2 abort settlement: an unknown session role does not join a drain and detached failures are observed", async () => {
  const warn = spyOn(logger, "sessionWarn").mockImplementation(() => {});
  const drain = deferred();
  const hooks: Array<(event: any) => Effect.Effect<unknown>> = [];
  const host = {
    location: { directory },
    session: { get: () => Effect.fail(new Error("session lookup unavailable")) },
    tool: {
      hook: (name: string, callback: (event: any) => Effect.Effect<unknown>) =>
        Effect.sync(() => {
          if (name === "execute.after") hooks.push(callback);
        }),
    },
  };
  let called = 0;
  try {
    await Effect.runPromise(
      registerV2ToolHooks(
        host,
        {
          drainBgNotifications: async () => {
            called += 1;
            await drain.promise;
            throw new Error("drain unavailable");
          },
        },
        new Set(),
      ),
    );
    expect(
      await promptly(
        Effect.runPromise(
          hooks[0]!({ tool: "shell", sessionID, status: "completed", result: { content: "ok" } }),
        ),
      ),
    ).toBe(true);
    expect(called).toBe(1);
    drain.resolve();
    await until(() =>
      warn.mock.calls.some((call) => String(call[1]).includes("drain unavailable")),
    );
  } finally {
    drain.resolve();
    warn.mockRestore();
  }
});
