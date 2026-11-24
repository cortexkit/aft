import { expect, test } from "bun:test";
import { Effect } from "effect";
import { sessionBgStates } from "../bg-notifications.js";
import { _resetSubagentCacheForTest } from "../shared/subagent-detect.js";
import {
  __resetV2WakeRoutesForTests,
  createV2RuntimeConsumer,
  SUBC_ACKED_WAKE_HOLD_MS,
} from "../wakes/runtime-consumer.js";
import { V2SessionDelivery } from "../wakes/session-delivery.js";

// Long-lived OpenCode 2 Locations admit Subc wakes through a delivery object
// with no size cap. A key is kept for a hold window after the daemon confirms
// its acknowledgement, so a late duplicate is still recognised, and is then
// retired so the Location does not keep one key per historical wake.

const completion = (taskId: string) => ({
  task_id: taskId,
  status: "completed",
  exit_code: 0,
  command: "sleep 1",
  output_preview: "done",
});
const match = {
  task_id: "bash-watched",
  session_id: "main",
  watch_id: "watch-1",
  match_text: "ready",
  match_offset: 0,
  context: "ready",
  once: true,
};

async function subcRig(options: { ackConfirms: boolean }) {
  _resetSubagentCacheForTest();
  __resetV2WakeRoutesForTests();
  sessionBgStates.clear();
  const synthetic: unknown[] = [];
  const host = {
    location: { directory: process.cwd() },
    session: {
      get: () => Effect.succeed({}),
      prompt: () => Effect.succeed({ id: "wake" }),
      synthetic: (input: unknown) =>
        Effect.sync(() => {
          synthetic.push(input);
          return { id: "wake" };
        }),
    },
  };
  const daemon = {
    completions: [completion("bash-done")],
    matches: [match] as Array<typeof match>,
  };
  const bridge = {
    getCwd: () => process.cwd(),
    send: async (command: string, params: Record<string, unknown>) => {
      if (command === "bash_drain_completions")
        return {
          success: true,
          bg_completions: daemon.completions,
          pending_matches: daemon.matches,
        };
      if (command === "bash_ack_completions") {
        if (!options.ackConfirms) return { success: false, message: "ack refused" };
        const acked = new Set((params.task_ids as string[] | undefined) ?? []);
        const ackedMatches = (params.matches as Array<Record<string, unknown>> | undefined) ?? [];
        daemon.completions = daemon.completions.filter((item) => !acked.has(item.task_id));
        daemon.matches = daemon.matches.filter(
          (item) => !ackedMatches.some((entry) => entry.task_id === item.task_id),
        );
        return { success: true, acked_task_ids: [...acked], acked_matches: ackedMatches };
      }
      return { success: true };
    },
  };
  // Retirements are captured instead of timed, so the test decides when the
  // hold window has passed.
  const scheduled: Array<{ run: () => void; delayMs: number; cancelled: boolean }> = [];
  // Find the delivery objects the consumer admits through, to read what they retain.
  const deliveries = new Set<V2SessionDelivery>();
  const original = V2SessionDelivery.prototype.completion;
  V2SessionDelivery.prototype.completion = function (this: V2SessionDelivery, input) {
    deliveries.add(this);
    return original.call(this, input);
  };
  const consumer = createV2RuntimeConsumer(host as never, undefined, undefined, {
    scheduleAckedWakeRetirement: (run, delayMs) => {
      const entry = { run, delayMs, cancelled: false };
      scheduled.push(entry);
      return () => {
        entry.cancelled = true;
      };
    },
  });
  const wakes = consumer.subcWakes(
    () => ({ getBridge: () => bridge, getActiveBridgeForRoot: () => bridge }) as never,
    process.cwd(),
  );
  return {
    consumer,
    wakes,
    bridge,
    daemon,
    synthetic,
    scheduled,
    retainedKeys: () =>
      [...deliveries].flatMap((delivery) => [
        ...(delivery as unknown as { admittedWakes: Map<string, string> }).admittedWakes.keys(),
      ]),
    passHoldWindow: () => {
      for (const entry of scheduled.splice(0)) if (!entry.cancelled) entry.run();
    },
    close: () => {
      V2SessionDelivery.prototype.completion = original;
      consumer.dispose();
      __resetV2WakeRoutesForTests();
      sessionBgStates.clear();
    },
  };
}

test("an acked subc wake's key holds a late duplicate inside the hold window", async () => {
  const r = await subcRig({ ackConfirms: true });
  try {
    await r.wakes.drainSession("main");
    expect(r.synthetic).toHaveLength(2);
    expect(r.scheduled.map((entry) => entry.delayMs)).toEqual([
      SUBC_ACKED_WAKE_HOLD_MS,
      SUBC_ACKED_WAKE_HOLD_MS,
    ]);

    // The daemon hands the same completion and match out again.
    r.daemon.completions = [completion("bash-done")];
    r.daemon.matches = [match];
    await r.wakes.drainSession("main");
    expect(r.synthetic).toHaveLength(2);
  } finally {
    r.close();
  }
});

test("acked subc wake keys are retired once the hold window has passed", async () => {
  const r = await subcRig({ ackConfirms: true });
  try {
    await r.wakes.drainSession("main");
    // A pushed completion is acknowledged on its own path.
    await r.consumer.bridgeOptions.onBashCompletion?.(
      { type: "bash_completed", session_id: "main", ...completion("bash-pushed") } as never,
      r.bridge as never,
    );
    expect(r.synthetic).toHaveLength(3);
    expect(r.retainedKeys()).toHaveLength(3);

    r.passHoldWindow();
    expect(r.retainedKeys()).toEqual([]);
  } finally {
    r.close();
  }
});

test("a subc wake key whose acknowledgement is not confirmed is never retired", async () => {
  const r = await subcRig({ ackConfirms: false });
  try {
    await r.wakes.drainSession("main");
    expect(r.synthetic.length).toBeGreaterThan(0);
    expect(r.scheduled).toEqual([]);
    r.passHoldWindow();
    expect(r.retainedKeys()).toHaveLength(r.synthetic.length);
  } finally {
    r.close();
  }
});
