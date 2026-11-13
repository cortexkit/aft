import { describe, expect, test } from "bun:test";
import { mkdtempSync, readFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { BinaryBridge, isBridgeTransportTimeout } from "../bridge.js";
import { cachedExecutable } from "./test-utils/cached-executable.js";
import { ManualClock } from "./test-utils/manual-clock.js";

const ENGINE = `#!/usr/bin/env node
const fs = require("node:fs");
let buffer = "";
process.stdin.setEncoding("utf8");
process.stdin.on("data", chunk => {
  buffer += chunk;
  let newline;
  while ((newline = buffer.indexOf("\\n")) !== -1) {
    const req = JSON.parse(buffer.slice(0, newline));
    buffer = buffer.slice(newline + 1);
    if (req.command === "bash") {
      fs.appendFileSync("started.jsonl", JSON.stringify(req) + "\\n");
    } else {
      process.stdout.write(JSON.stringify({ id: req.id, success: true, warnings: [], version: "99.0.0" }) + "\\n");
    }
  }
});
`;

async function rig() {
  const root = mkdtempSync(join(tmpdir(), "aft-suspend-"));
  const clock = new ManualClock();
  const bridge = new BinaryBridge(cachedExecutable(ENGINE), root, {
    clock,
    maxRestarts: 0,
    // Keep the independent health probe outside the fake sleep timeline.
    hangWindowMinMs: 100_000_000,
  });
  await bridge.send("configure", { project_root: root });
  const internals = bridge as unknown as {
    pending: Map<string, unknown>;
    processStdoutLine(line: string): void;
  };
  const call = bridge.send(
    "bash",
    { command: "start once", session_id: "s" },
    {
      transportTimeoutMs: 270_000,
      keepBridgeOnTimeout: true,
    },
  );
  const outcome = call.then(
    (value) => ({ value }),
    (error) => ({ error }),
  );
  let started: { id: string } | undefined;
  const giveUpAt = Date.now() + 5_000;
  while (!started) {
    try {
      started = JSON.parse(readFileSync(join(root, "started.jsonl"), "utf8").trim());
    } catch {}
    if (Date.now() > giveUpAt) throw new Error("fake engine did not start bash");
    if (!started) await new Promise((resolve) => setTimeout(resolve, 5));
  }
  return {
    clock,
    bridge,
    internals,
    outcome,
    reply: () =>
      internals.processStdoutLine(
        JSON.stringify({ id: started.id, success: true, task_id: "bash-wake", status: "running" }),
      ),
    starts: () => readFileSync(join(root, "started.jsonl"), "utf8").trim().split("\n").length,
    cleanup: async () => {
      await bridge.shutdown();
      rmSync(root, { recursive: true, force: true });
    },
  };
}

describe("standalone bash suspend delivery", () => {
  for (const timerFirst of [true, false]) {
    test(`wake task reply resolves once with ${timerFirst ? "timer" : "I/O"} first`, async () => {
      const r = await rig();
      try {
        let settlements = 0;
        void r.outcome.then(() => settlements++);
        r.clock.advance(250_000);
        r.clock.suspend(2_000_000);
        if (timerFirst) r.clock.resume();
        r.reply();
        r.clock.resume();
        expect(await r.outcome).toMatchObject({
          value: { task_id: "bash-wake", status: "running" },
        });
        r.clock.advance(20_000);
        expect(settlements).toBe(1);
        expect(r.starts()).toBe(1);
        expect(r.internals.pending.size).toBe(0);
        expect(r.bridge.isAlive()).toBe(true);
      } finally {
        await r.cleanup();
      }
    });
  }

  test("a silent started call gets only one allowance across repeated suspend", async () => {
    const r = await rig();
    try {
      r.clock.suspend(2_000_000);
      r.clock.resume();
      expect(r.internals.pending.size).toBe(1);
      r.clock.advance(9_999);
      expect(r.internals.pending.size).toBe(1);
      r.clock.suspend(2_000_000);
      r.clock.resume();
      const result = await r.outcome;
      expect("error" in result && isBridgeTransportTimeout(result.error)).toBe(true);
      expect(r.internals.pending.size).toBe(0);
      expect(r.starts()).toBe(1);
    } finally {
      await r.cleanup();
    }
  });
});
