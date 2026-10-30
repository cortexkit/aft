import { afterEach, expect, test } from "bun:test";
import { readFileSync } from "node:fs";
import { join } from "node:path";
import type { BashPatternMatchFrame } from "@cortexkit/aft-bridge";
import { Effect, Exit, PubSub, Scope, Stream } from "effect";

import { __resetBgNotificationStateForTests } from "../bg-notifications.js";
import { _resetSubagentCacheForTest } from "../shared/subagent-detect.js";
import { __resetSyncWatchAbortForTests } from "../sync-watch-abort.js";
import { createBashWatchTool } from "../tools/bash_watch.js";
import { projectV2Tool } from "../tools/definitions/v2.js";
import type { PluginContext } from "../types.js";
import { registerV2SessionLifecycle } from "../v2-session-lifecycle.js";
import { __resetV2WakeRoutesForTests, createV2RuntimeConsumer } from "../wakes/runtime-consumer.js";
import {
  MAX_ADMITTED_WAKES,
  V2SessionDelivery,
  type V2SessionSyntheticInput,
  WakeDeliveryOverflow,
} from "../wakes/session-delivery.js";

afterEach(() => {
  __resetV2WakeRoutesForTests();
  _resetSubagentCacheForTest();
  __resetBgNotificationStateForTests();
  __resetSyncWatchAbortForTests();
});

async function rig(parentID?: string) {
  _resetSubagentCacheForTest();
  const synthetic: V2SessionSyntheticInput[] = [];
  const task = { id: "bash-running", status: "running" };
  const statusAtWake: string[] = [];
  const bus = await Effect.runPromise(
    PubSub.unbounded<{ type: string; data: { sessionID: string; reason?: string } }>(),
  );
  const scope = await Effect.runPromise(Scope.make());
  let reverted = false;
  let failAdmission = false;
  let failAck = false;
  let ignoreAck = false;
  let loseAck = false;
  let beforeDrain: (() => Promise<void>) | undefined;
  let beforeAdmission: (() => Promise<void>) | undefined;
  const host = {
    location: { directory: process.cwd() },
    event: { subscribe: () => Stream.fromPubSub(bus) },
    session: {
      get: () =>
        Effect.sync(() => ({ parentID, revert: reverted ? { messageID: "boundary" } : undefined })),
      prompt: () => Effect.die("pattern wakes must use session.synthetic"),
      synthetic: (input: V2SessionSyntheticInput) =>
        Effect.suspend(() => {
          if (failAdmission) {
            failAdmission = false;
            return Effect.fail(new Error("host unavailable"));
          }
          const record = Effect.sync(() => {
            statusAtWake.push(task.status);
            return synthetic.push(input);
          });
          const hook = beforeAdmission;
          beforeAdmission = undefined;
          return hook ? Effect.andThen(Effect.promise(hook), record) : record;
        }),
    },
  };
  const sends: Array<{ command: string; params: Record<string, unknown> }> = [];
  let watch: Record<string, unknown> | undefined;
  let pending: BashPatternMatchFrame | undefined;
  let offset = 0;
  const bridge = {
    send: async (command: string, params: Record<string, unknown>) => {
      sends.push({ command, params });
      if (command === "bash_notify") {
        if (params.task_id !== task.id) return { success: false, message: "task not found" };
        watch = params;
        return { success: true, watch_id: "watch-ready" };
      }
      if (command === "bash_drain_completions") {
        const hook = beforeDrain;
        beforeDrain = undefined;
        if (hook) await hook();
        return {
          success: true,
          pending_matches: pending ? [pending] : [],
          bg_completions:
            task.status === "completed"
              ? [
                  {
                    task_id: task.id,
                    status: "completed",
                    command: "server",
                    exit_code: 0,
                    output_preview: "done",
                  },
                ]
              : [],
        };
      }
      if (command === "bash_ack_completions") {
        if (ignoreAck) {
          ignoreAck = false;
          return { success: true, acked_task_ids: [] };
        }
        if (failAck) {
          failAck = false;
          return { success: false, message: "ack unavailable" };
        }
        const matches = (
          (params.matches ?? []) as Array<{
            task_id: string;
            watch_id: string;
            match_offset: number;
          }>
        ).filter(
          (match) =>
            pending &&
            match.task_id === pending.task_id &&
            match.watch_id === pending.watch_id &&
            match.match_offset === pending.match_offset,
        );
        if (matches.length > 0 || ((params.task_ids ?? []) as string[]).includes(task.id)) {
          if (pending?.once) watch = undefined;
          pending = undefined;
        }
        if (loseAck) {
          loseAck = false;
          return { success: false, message: "ack response lost" };
        }
        return {
          success: true,
          acked_matches: matches,
          acked_task_ids: params.task_ids ?? [],
        };
      }
      return { success: true };
    },
  };
  const ctx = {
    client: host,
    config: { bash: { subagent_background: true } },
    pool: { getBridge: () => bridge, getActiveBridgeForRoot: () => bridge },
  } as unknown as PluginContext;
  const consumer = createV2RuntimeConsumer(
    host,
    () => ctx.config,
    () => bridge as never,
  );
  await Effect.runPromise(
    registerV2SessionLifecycle(host).pipe(Effect.provideService(Scope.Scope, scope)),
  );
  const tool = projectV2Tool(
    "bash_watch",
    createBashWatchTool(ctx),
    { directory: process.cwd() },
    consumer,
  );
  return {
    consumer,
    host,
    task,
    statusAtWake,
    synthetic,
    sends,
    bridge,
    pending: () => pending,
    setPending: (frame: BashPatternMatchFrame) => {
      pending = frame;
    },
    holdNextDrain: (hook: () => Promise<void>) => {
      beforeDrain = hook;
    },
    holdNextAdmission: (hook: () => Promise<void>) => {
      beforeAdmission = hook;
    },
    push: (frame: BashPatternMatchFrame) =>
      consumer.bridgeOptions.onBashPatternMatch?.(frame, bridge as never),
    watching: () => watch !== undefined,
    revert: (value: boolean) => {
      reverted = value;
    },
    failAdmission: () => {
      failAdmission = true;
    },
    failAck: () => {
      failAck = true;
    },
    ignoreAck: () => {
      ignoreAck = true;
    },
    loseAck: () => {
      loseAck = true;
    },
    async stop() {
      await Effect.runPromise(
        PubSub.publish(bus, {
          type: "session.execution.interrupted",
          data: { sessionID: "session-v2", reason: "user" },
        }),
      );
      await Bun.sleep(0);
    },
    async close() {
      consumer.dispose();
      await Effect.runPromise(Scope.close(scope, Exit.succeed(undefined)));
    },
    async register(once = true, deliver?: "each_line") {
      await Effect.runPromise(
        tool.execute(
          { taskId: "bash-running", background: true, pattern: "READY", once, deliver },
          { sessionID: "session-v2", progress: () => Effect.void },
        ),
      );
      expect(watch).toMatchObject({ task_id: "bash-running", pattern: "READY", once });
    },
    async print(line: string, push = true) {
      // Model the bridge's stdout push, not a host-side shortcut to session input.
      expect(task.status).toBe("running");
      if (!watch || !line.includes(String(watch.pattern))) return;
      const frame: BashPatternMatchFrame = {
        type: "bash_pattern_match",
        session_id: "session-v2",
        task_id: "bash-running",
        watch_id: "watch-ready",
        match_text: "READY",
        match_offset: offset,
        context: line,
        once: watch.once as boolean,
      };
      offset += line.length + 1;
      pending = frame;
      if (push) await consumer.bridgeOptions.onBashPatternMatch?.(frame, bridge as never);
    },
  };
}

test("V2 async bash_watch wakes on matching output before task completion", async () => {
  const r = await rig();
  try {
    await r.register();
    await r.print("READY: server is listening");
    expect(r.synthetic).toHaveLength(1);
    expect(r.statusAtWake).toEqual(["running"]);
    expect(r.synthetic[0]).toMatchObject({
      sessionID: "session-v2",
      delivery: "steer",
      metadata: { source: "aft", kind: "bash_pattern_match" },
    });
    expect(r.synthetic[0]?.text).toContain("[BG BASH NOTIFY]");
    expect(r.synthetic[0]?.text).toContain("READY: server is listening");
    expect(r.synthetic[0]?.text).toBe(
      '<system-reminder>\n[BG BASH NOTIFY]\n- task bash-running matched "READY" (offset 0):\n      > READY: server is listening\n</system-reminder>',
    );
    expect(r.synthetic[0]).not.toHaveProperty("resume");
    expect(r.sends.filter((send) => send.command === "bash_ack_completions")).toEqual([
      {
        command: "bash_ack_completions",
        params: {
          session_id: "session-v2",
          matches: [{ task_id: "bash-running", watch_id: "watch-ready", match_offset: 0 }],
        },
      },
    ]);
  } finally {
    await r.close();
  }
});

test("V2 sticky watch retires 10000 admitted match keys and drops an acknowledged replay", async () => {
  const r = await rig();
  const deliveries = new Set<V2SessionDelivery>();
  const original = V2SessionDelivery.prototype.patternMatch;
  V2SessionDelivery.prototype.patternMatch = function (input) {
    deliveries.add(this);
    return original.call(this, input);
  };
  try {
    await r.register(false);
    for (let i = 0; i < 10000; i++) await r.print("READY");
    expect(r.synthetic).toHaveLength(10000);
    const delivery = [...deliveries][0] as unknown as { admittedWakes: Set<string> };
    expect(delivery.admittedWakes.size).toBe(0);
    await r.consumer.bridgeOptions.onBashPatternMatch?.(
      {
        type: "bash_pattern_match",
        session_id: "session-v2",
        task_id: "bash-running",
        watch_id: "watch-ready",
        match_offset: 0,
        match_text: "READY",
        context: "READY",
        once: false,
      },
      r.bridge as never,
    );
    expect(r.synthetic).toHaveLength(10000);
  } finally {
    V2SessionDelivery.prototype.patternMatch = original;
    await r.close();
  }
});

test("V2 acknowledging one match leaves the other watch pending for the next delivery", async () => {
  const r = await rig();
  const first: BashPatternMatchFrame = {
    type: "bash_pattern_match",
    session_id: "session-v2",
    task_id: "bash-running",
    watch_id: "first",
    match_offset: 10,
    match_text: "READY",
    context: "first",
    once: true,
  };
  const second = { ...first, watch_id: "second", match_offset: 20, context: "second" };
  const pending = [first, second];
  const bridge = {
    send: async (command: string, params: Record<string, unknown>) => {
      if (command === "bash_drain_completions")
        return { success: true, pending_matches: [...pending] };
      if (command === "bash_ack_completions") {
        const matches = params.matches as
          | Array<{ watch_id: string; match_offset: number }>
          | undefined;
        for (let i = pending.length - 1; i >= 0; i--) {
          if (
            params.task_ids ||
            matches?.some(
              (m) =>
                m.watch_id === pending[i]?.watch_id && m.match_offset === pending[i]?.match_offset,
            )
          )
            pending.splice(i, 1);
        }
        return { success: true, acked_matches: params.matches ?? [] };
      }
      return { success: true };
    },
  };
  try {
    r.consumer.registerSession?.("session-v2");
    await r.consumer.bridgeOptions.onBashPatternMatch?.(first, bridge as never);
    expect(pending).toEqual([second]);
    await r.consumer.bridgeOptions.onBashPatternMatch?.(second, bridge as never);
    expect(pending).toEqual([]);
    expect(r.synthetic.map((input) => input.text)).toEqual([
      expect.stringContaining("first"),
      expect.stringContaining("second"),
    ]);
  } finally {
    await r.close();
  }
});

test("V2 unretired wake overflow is typed and does not evict duplicate protection", async () => {
  let admitted = 0;
  const delivery = new V2SessionDelivery({
    get: () => Effect.succeed({}),
    prompt: () => Effect.void,
    synthetic: () => Effect.sync(() => admitted++),
  });
  const wake = {
    sessionID: "s",
    taskIDs: ["task"],
    watchID: "sticky",
    matchOffset: 0,
    text: "READY",
  };
  for (let i = 0; i < MAX_ADMITTED_WAKES; i++) {
    expect(await Effect.runPromise(delivery.patternMatch({ ...wake, matchOffset: i }))).toBe(
      "delivered",
    );
  }
  const overflow = await Effect.runPromise(
    delivery
      .patternMatch({ ...wake, matchOffset: MAX_ADMITTED_WAKES })
      .pipe(Effect.catch((error) => Effect.succeed(error))),
  );
  expect(overflow).toBeInstanceOf(WakeDeliveryOverflow);
  expect(overflow).toMatchObject({ type: "wake_delivery_overflow" });
  expect(await Effect.runPromise(delivery.patternMatch(wake))).toBe("already-delivered");
  expect(admitted).toBe(MAX_ADMITTED_WAKES);
  delivery.dispose();
});

test("V2 once matches remain pending until admission and acknowledgement succeed", async () => {
  const r = await rig();
  try {
    await r.register();
    r.failAdmission();
    await r.print("READY");
    expect(r.synthetic).toEqual([]);
    expect(r.pending()).toBeDefined();
    expect(r.watching()).toBe(true);
    r.failAck();
    r.consumer.registerSession?.("session-v2");
    await Bun.sleep(0);
    expect(r.synthetic).toHaveLength(1);
    expect(r.pending()).toBeDefined();
    r.consumer.registerSession?.("session-v2");
    await Bun.sleep(0);
    expect(r.synthetic).toHaveLength(1);
    expect(r.pending()).toBeUndefined();
    expect(r.watching()).toBe(false);
  } finally {
    await r.close();
  }
});

test("V2 unconfirmed match acknowledgement retains admission until durable retirement", async () => {
  const r = await rig();
  try {
    await r.register(false);
    r.ignoreAck();
    await r.print("READY");
    expect(r.synthetic).toHaveLength(1);
    expect(r.pending()).toBeDefined();
    r.consumer.registerSession?.("session-v2");
    await Bun.sleep(0);
    expect(r.pending()).toBeUndefined();
    expect(r.synthetic).toHaveLength(1);
  } finally {
    await r.close();
  }
});

test("V2 sticky matches clear on ack, can wake again, and do not consume completion", async () => {
  const r = await rig();
  try {
    await r.register(false);
    await r.print("READY first");
    const first = {
      type: "bash_pattern_match" as const,
      session_id: "session-v2",
      task_id: "bash-running",
      watch_id: "watch-ready",
      match_text: "READY",
      match_offset: 0,
      context: "READY first",
      once: false,
    };
    await r.consumer.bridgeOptions.onBashPatternMatch?.(first, r.bridge as never);
    expect(r.synthetic).toHaveLength(1);
    expect(r.pending()).toBeUndefined();
    expect(r.watching()).toBe(true);
    await r.print("READY second");
    expect(r.synthetic).toHaveLength(2);
    r.task.status = "completed";
    await r.consumer.bridgeOptions.onBashCompletion?.(
      {
        type: "bash_completed",
        session_id: "session-v2",
        task_id: "bash-running",
        status: "completed",
        command: "server",
        exit_code: 0,
        output_preview: "done",
      },
      r.bridge as never,
    );
    expect(r.synthetic).toHaveLength(3);
    expect(r.synthetic[2]?.metadata?.kind).toBe("bash_completion");
  } finally {
    await r.close();
  }
});

test("V2 missed pushes drain into pattern steer wakes and drop foreign-session matches", async () => {
  const r = await rig();
  try {
    await r.register();
    await r.print("READY recovered", false);
    expect(r.synthetic).toEqual([]);
    r.consumer.transportOptions.onBgEventsNudge?.(process.cwd(), "session-v2");
    await Bun.sleep(0);
    expect(r.synthetic).toHaveLength(1);
    expect(r.synthetic[0]?.text).toContain("READY recovered");
    expect(r.pending()).toBeUndefined();
    const count = r.sends.length;
    await r.consumer.bridgeOptions.onBashPatternMatch?.(
      {
        type: "bash_pattern_match",
        session_id: "foreign",
        task_id: "bash-other",
        watch_id: "watch-other",
        match_text: "READY",
        match_offset: 0,
        context: "foreign",
        once: true,
      },
      r.bridge as never,
    );
    expect(r.synthetic).toHaveLength(1);
    expect(r.sends).toHaveLength(count);
  } finally {
    await r.close();
  }
});

test("V2 staged revert defers pattern admission and ack until the next registration", async () => {
  const r = await rig();
  try {
    await r.register();
    r.revert(true);
    await r.print("READY");
    expect(r.synthetic).toEqual([]);
    expect(r.pending()).toBeDefined();
    expect(r.sends.some((send) => send.command === "bash_ack_completions")).toBe(false);
    r.revert(false);
    r.consumer.registerSession?.("session-v2");
    await Bun.sleep(0);
    expect(r.synthetic).toHaveLength(1);
    expect(r.pending()).toBeUndefined();
  } finally {
    await r.close();
  }
});

test("V2 session registration recovers a pending match after consumer restart", async () => {
  const r = await rig();
  let restarted: ReturnType<typeof createV2RuntimeConsumer> | undefined;
  try {
    await r.register();
    await r.print("READY after restart", false);
    r.consumer.dispose();
    restarted = createV2RuntimeConsumer(
      r.host,
      () => ({}),
      () => r.bridge as never,
    );
    restarted.registerSession?.("session-v2");
    await Bun.sleep(0);
    expect(r.synthetic).toHaveLength(1);
    expect(r.synthetic[0]?.text).toContain("READY after restart");
    expect(r.pending()).toBeUndefined();
  } finally {
    restarted?.dispose();
    await r.close();
  }
});

test("V2 subagent pattern wakes require an eligible watch and respect deliberate stop", async () => {
  const r = await rig("parent");
  try {
    r.consumer.registerSession?.("session-v2");
    await r.consumer.bridgeOptions.onBashPatternMatch?.(
      {
        type: "bash_pattern_match",
        session_id: "session-v2",
        task_id: "not-watched",
        watch_id: "other",
        match_text: "READY",
        match_offset: 0,
        context: "READY",
        once: true,
      },
      r.bridge as never,
    );
    expect(r.synthetic).toEqual([]);
    await r.register(false);
    await r.print("READY allowed");
    expect(r.synthetic).toHaveLength(1);
    await r.stop();
    const acks = r.sends.filter((send) => send.command === "bash_ack_completions").length;
    await r.print("READY stopped");
    expect(r.synthetic).toHaveLength(1);
    expect(r.sends.filter((send) => send.command === "bash_ack_completions")).toHaveLength(acks);
  } finally {
    await r.close();
  }
});

test("V2 erased watch delivers its live-only failure notice without a durable match", async () => {
  const r = await rig();
  try {
    await r.register();
    const before = r.sends.length;
    await r.consumer.bridgeOptions.onBashPatternMatch?.(
      {
        type: "bash_pattern_match",
        session_id: "session-v2",
        task_id: "bash-running",
        watch_id: "watch-ready",
        match_offset: 0,
        match_text: "watch target erased",
        context:
          "watch target erased: the background task row was erased before the watch reached a normal terminal result",
        once: true,
        reason: "task_exit",
      },
      r.bridge as never,
    );
    expect(r.synthetic).toHaveLength(1);
    expect(r.synthetic[0]?.text).toContain("watch target erased");
    expect(r.sends.slice(before).filter((send) => send.command === "bash_ack_completions")).toEqual(
      [
        {
          command: "bash_ack_completions",
          params: { session_id: "session-v2", task_ids: ["bash-running"] },
        },
      ],
    );
  } finally {
    await r.close();
  }
});

function eachLineFrame(offset: number, text: string): BashPatternMatchFrame {
  return {
    type: "bash_pattern_match",
    session_id: "session-v2",
    task_id: "bash-running",
    watch_id: "watch-ready",
    match_text: "matching lines",
    match_offset: offset,
    context: "unused: the batch is the rendered source",
    once: false,
    reason: "pattern_match",
    batch: {
      lines: [
        {
          stream: "stdout",
          offset,
          end_offset: offset + Buffer.byteLength(text) + 1,
          text,
          count: 1,
        },
      ],
      dropped_lines: 0,
      first_dropped_offset: null,
      first_dropped_stream: null,
      skipped_long_lines: 0,
    },
  };
}

function eachLineSnapshot(id: string, ...frames: BashPatternMatchFrame[]): BashPatternMatchFrame {
  const last = frames.at(-1)!;
  const batch = {
    ...last.batch!,
    pending_id: id,
    snapshot: true,
    lines: frames.flatMap((frame) => frame.batch!.lines.map((line) => ({ ...line }))),
  };
  return { ...last, batch };
}

function latch() {
  let release!: () => void;
  const promise = new Promise<void>((resolve) => {
    release = resolve;
  });
  return { promise, release };
}

function nativeTerminalFrame(): BashPatternMatchFrame {
  // Checked-in native emitter output (see each-line-terminal-frame.json).
  // Spawning cargo from a host test hangs offline builders; the Rust suite
  // asserts the live frame equals this pinned file.
  return JSON.parse(
    readFileSync(join(import.meta.dir, "fixtures/each-line-terminal-frame.json"), "utf8"),
  ) as BashPatternMatchFrame;
}

test("V2 each_line terminal cap summary survives the durable version fence", async () => {
  const native = nativeTerminalFrame();
  const r = await rig();
  try {
    await r.register(false, "each_line");
    const pending = eachLineSnapshot("terminal-pending", eachLineFrame(0, "READY first"));
    pending.match_offset = 123;
    pending.batch!.dropped_lines = 7;
    pending.batch!.first_dropped_offset = 123;
    pending.batch!.first_dropped_stream = "stderr";
    r.setPending(pending);
    await r.push(native);
    expect(r.synthetic).toHaveLength(1);
    expect(r.synthetic[0]?.text).toContain(
      '7 matching lines dropped. Read them with bash_status({ taskId: "bash-running", stderrOffset: 123 }).',
    );
    expect(r.synthetic[0]?.text).toContain("stdout byte 0: READY first");
    expect(
      r.sends.filter((send) => send.command === "bash_ack_completions").at(-1)?.params,
    ).toEqual({
      session_id: "session-v2",
      matches: [{ task_id: "bash-running", watch_id: "watch-ready", match_offset: 123 }],
    });
    expect(r.pending()).toBeUndefined();
  } finally {
    await r.close();
  }
});

test("V2 each_line superseded push delivers every durable line once", async () => {
  const r = await rig();
  const entered = latch();
  const resume = latch();
  let firstPush: Promise<void> | void;
  try {
    await r.register(false, "each_line");
    const first = eachLineFrame(0, "READY first");
    const second = eachLineFrame(12, "READY second");
    const third = eachLineFrame(25, "READY third");
    r.setPending(eachLineSnapshot("pending-a", first));
    r.holdNextDrain(async () => {
      entered.release();
      await resume.promise;
    });
    firstPush = r.push(first);
    await entered.promise;
    r.setPending(eachLineSnapshot("pending-a", first, second));
    await r.push(second);
    r.setPending(eachLineSnapshot("pending-a", first, second, third));
    await r.push(third);
    resume.release();
    await firstPush;
    expect(r.synthetic).toHaveLength(1);
    expect(r.synthetic[0]?.text.split("\n").filter((line) => line.includes("stdout byte"))).toEqual(
      [
        "      > stdout byte 0: READY first",
        "      > stdout byte 12: READY second",
        "      > stdout byte 25: READY third",
      ],
    );
    expect(
      r.sends.filter((send) => send.command === "bash_ack_completions").map((send) => send.params),
    ).toEqual([
      {
        session_id: "session-v2",
        matches: [{ task_id: "bash-running", watch_id: "watch-ready", match_offset: 25 }],
      },
    ]);
    expect(r.pending()).toBeUndefined();
  } finally {
    resume.release();
    await firstPush;
    await r.close();
  }
});

test("V2 each_line concurrent admission dedups folded rows until exact ack", async () => {
  const r = await rig();
  const entered = latch();
  const resume = latch();
  let firstPush: Promise<void> | void;
  try {
    await r.register(false, "each_line");
    const first = eachLineFrame(0, "READY first");
    const second = eachLineFrame(12, "READY first");
    r.setPending(eachLineSnapshot("pending-a", first));
    r.holdNextAdmission(async () => {
      entered.release();
      await resume.promise;
    });
    firstPush = r.push(first);
    await entered.promise;
    const folded = eachLineSnapshot("pending-a", first);
    folded.match_offset = second.match_offset;
    folded.batch!.lines[0].count = 2;
    r.setPending(folded);
    await r.push(second);
    resume.release();
    await firstPush;
    expect(r.synthetic).toHaveLength(2);
    expect(r.synthetic[0]?.text).toContain("stdout byte 0: READY first");
    expect(r.synthetic[1]?.text).toContain("stdout byte 12: READY first");
    expect(r.synthetic[1]?.text).not.toContain("stdout byte 0");
    expect(r.synthetic[1]?.text).not.toContain("×2");
    expect(r.pending()).toBeUndefined();
  } finally {
    resume.release();
    await firstPush;
    await r.close();
  }
});

test("V2 each_line new pending generation resets admitted drop counters", async () => {
  const r = await rig();
  try {
    await r.register(false, "each_line");
    const first = eachLineFrame(0, "READY first");
    first.batch!.dropped_lines = 7;
    first.batch!.first_dropped_offset = 12;
    first.batch!.first_dropped_stream = "stdout";
    r.setPending(eachLineSnapshot("pending-a", first));
    r.failAck();
    await r.push(first);
    const second = eachLineFrame(24, "READY second");
    second.batch!.dropped_lines = 10;
    second.batch!.first_dropped_offset = 37;
    second.batch!.first_dropped_stream = "stdout";
    r.setPending(eachLineSnapshot("pending-b", second));
    await r.push(second);
    expect(r.synthetic).toHaveLength(2);
    expect(r.synthetic[0]?.text).toContain("7 matching lines dropped.");
    expect(r.synthetic[1]?.text).toContain("10 matching lines dropped.");
    expect(r.synthetic[1]?.text).not.toContain("3 matching lines dropped.");
  } finally {
    await r.close();
  }
});

test("V2 each_line lost ack redelivery preserves drop totals without duplicate delivery", async () => {
  for (const committed of [false, true]) {
    const r = await rig();
    try {
      await r.register(false, "each_line");
      const first = eachLineFrame(0, "READY first");
      first.batch!.dropped_lines = 7;
      first.batch!.first_dropped_offset = 12;
      first.batch!.first_dropped_stream = "stdout";
      r.setPending(eachLineSnapshot("pending-a", first));
      if (committed) r.loseAck();
      else r.failAck();
      await r.push(first);
      await r.push(first);
      expect(r.synthetic).toHaveLength(1);
      expect(r.synthetic[0]?.text).toContain("7 matching lines dropped.");
      const next = eachLineFrame(24, "READY second");
      next.batch!.dropped_lines = 10;
      next.batch!.first_dropped_offset = 37;
      next.batch!.first_dropped_stream = "stdout";
      r.setPending(eachLineSnapshot("pending-b", next));
      await r.push(next);
      expect(r.synthetic).toHaveLength(2);
      expect(r.synthetic[1]?.text).toContain("10 matching lines dropped.");
      expect(r.synthetic[1]?.text).not.toContain("READY first");
      expect(r.synthetic[1]?.text).not.toContain("3 matching lines dropped.");
      expect(r.pending()).toBeUndefined();
    } finally {
      await r.close();
    }
  }
});
