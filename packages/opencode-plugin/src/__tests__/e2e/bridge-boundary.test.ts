/// <reference path="../../bun-test.d.ts" />

import { afterEach, beforeAll, describe, expect, test } from "bun:test";
import {
  cleanupHarnesses,
  createHarness,
  type E2EHarness,
  type PreparedBinary,
  prepareBinary,
} from "./helpers.js";

const initialBinary = await prepareBinary();
const maybeDescribe = describe.skipIf(!initialBinary.binaryPath);

maybeDescribe("e2e bridge boundary behavior", () => {
  let preparedBinary: PreparedBinary = initialBinary;
  const harnesses: E2EHarness[] = [];

  beforeAll(async () => {
    preparedBinary = await prepareBinary();
  });

  afterEach(async () => {
    await cleanupHarnesses(harnesses);
  });

  async function harness(): Promise<E2EHarness> {
    const created = await createHarness(preparedBinary, { timeoutMs: 10_000 });
    harnesses.push(created);
    return created;
  }

  test("handles concurrent requests through one bridge", async () => {
    const h = await harness();

    const responses = await Promise.all([
      h.bridge.send("ping"),
      h.bridge.send("read", { file: h.path("sample.ts") }),
      h.bridge.send("outline", { file: h.path("sample.ts") }),
    ]);

    expect(responses[0]?.command).toBe("pong");
    expect(responses[1]?.success).toBe(true);
    expect(responses[2]?.success).toBe(true);
    expect(new Set(responses.map((response) => response.id)).size).toBe(3);
  });

  test("recovers via lazy respawn after external SIGKILL", async () => {
    // Issue #14: SIGKILL/SIGTERM from OS or host process teardown is NOT a real
    // crash — we explicitly do NOT auto-restart on signal exits to avoid
    // process avalanches when many bridges receive SIGTERM simultaneously.
    // Recovery still works: the next send() lazy-spawns a fresh bridge.
    const h = await harness();

    const first = await h.bridge.send("ping");
    expect(first.success).toBe(true);

    const proc = (h.bridge as unknown as { process?: { kill(signal: string): void; pid?: number } })
      .process;
    proc?.kill("SIGKILL");
    await waitForBridgeProcessExit(h.bridge, proc?.pid);

    const second = await h.bridge.send("ping");
    expect(second.success).toBe(true);
    // No auto-restart counter bump — signal kills aren't counted as crashes.
    expect(h.bridge.restartCount).toBe(0);
  });
});

async function waitForBridgeProcessExit(
  bridge: E2EHarness["bridge"],
  pid: number | undefined,
  timeoutMs = 5_000,
): Promise<void> {
  if (pid === undefined) throw new Error("bridge child process is missing a pid");
  const started = Date.now();
  while (true) {
    const current = (bridge as unknown as { process?: { pid?: number } | null }).process;
    if (current?.pid !== pid || !isProcessAlive(pid)) return;
    if (Date.now() - started > timeoutMs) {
      throw new Error(`timed out waiting for bridge child ${pid} to exit`);
    }
    await new Promise((resolve) => setTimeout(resolve, 25));
  }
}

function isProcessAlive(pid: number): boolean {
  try {
    process.kill(pid, 0);
    return true;
  } catch {
    return false;
  }
}
