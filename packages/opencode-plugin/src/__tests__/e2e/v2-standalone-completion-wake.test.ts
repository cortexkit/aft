/// <reference path="../../bun-test.d.ts" />

import { afterEach, expect, test } from "bun:test";
import { Effect } from "effect";

import {
  __resetV2WakeRoutesForTests,
  createV2RuntimeConsumer,
} from "../../wakes/runtime-consumer.js";
import { cleanupHarnesses, createHarness, type E2EHarness, prepareBinary } from "./helpers.js";

const initialBinary = await prepareBinary();
const harnesses: E2EHarness[] = [];

afterEach(async () => {
  __resetV2WakeRoutesForTests();
  await cleanupHarnesses(harnesses);
});

// The standalone OpenCode 2 consumer checks each pushed completion against a
// `bash_drain_completions` reply before admitting its wake. Unit fakes build
// that reply by hand; this runs it against the real daemon, whose entries omit
// `session_id`, so a check that depends on the field fails here.
test.skipIf(!initialBinary.binaryPath)(
  "V2 standalone background completion wakes its session through the real aft binary",
  async () => {
    const synthetic: Array<{ sessionID: string; text: string }> = [];
    const consumer = createV2RuntimeConsumer({
      session: {
        get: () => Effect.succeed({}),
        prompt: () => Effect.void,
        synthetic: (input) => Effect.sync(() => void synthetic.push(input)),
      },
    });
    try {
      const h = await createHarness(initialBinary, {
        fixtureNames: [],
        bridgeOptions: { timeoutMs: 20_000, ...consumer.bridgeOptions },
        configOverrides: {
          restrict_to_project_root: false,
          bash_permissions: false,
          experimental_bash_background: true,
        },
      });
      harnesses.push(h);
      const sessionID = "v2-standalone-wake";
      consumer.registerSession?.(sessionID, h.bridge);
      const started = await h.bridge.send("bash", {
        command: "echo done",
        description: "Print a background completion marker",
        background: true,
        session_id: sessionID,
      });
      const taskId = started.task_id;
      if (typeof taskId !== "string") throw new Error(`no task id: ${JSON.stringify(started)}`);
      const deadline = Date.now() + 20_000;
      while (synthetic.length === 0 && Date.now() < deadline) await Bun.sleep(50);
      expect(synthetic).toHaveLength(1);
      expect(synthetic[0]?.sessionID).toBe(sessionID);
      expect(synthetic[0]?.text).toContain("[BACKGROUND BASH COMPLETED]");
      expect(synthetic[0]?.text).toContain(taskId);
      // The admitted wake is acknowledged after admission, so the daemon stops
      // queueing it shortly after the wake appears.
      let queued: unknown = [taskId];
      while (Date.now() < deadline) {
        queued = (await h.bridge.send("bash_drain_completions", { session_id: sessionID }))
          .bg_completions;
        if (Array.isArray(queued) && queued.length === 0) break;
        await Bun.sleep(50);
      }
      expect(queued).toEqual([]);
    } finally {
      consumer.dispose();
    }
  },
  60_000,
);
