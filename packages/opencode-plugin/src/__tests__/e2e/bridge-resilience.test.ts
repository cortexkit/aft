/// <reference path="../../bun-test.d.ts" />

import { afterEach, beforeAll, describe, expect, test } from "bun:test";
import type { ChildProcess } from "node:child_process";
import { writeFile } from "node:fs/promises";
import {
  cleanupHarnesses,
  createHarness,
  type E2EHarness,
  type PreparedBinary,
  prepareBinary,
} from "./helpers.js";

const initialBinary = await prepareBinary();
const maybeDescribe = describe.skipIf(!initialBinary.binaryPath);

function childPid(bridge: E2EHarness["bridge"]): number {
  const child = (bridge as unknown as { process: ChildProcess | null }).process;
  const pid = child?.pid;
  if (pid === undefined) throw new Error("bridge child process is not spawned");
  return pid;
}

async function waitForExitHandler(
  bridge: { process?: ChildProcess | null },
  pid: number,
  timeoutMs = 5_000,
): Promise<void> {
  const started = Date.now();
  while (true) {
    if (bridge.process?.pid !== pid || !isProcessAlive(pid)) return;
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

maybeDescribe("e2e bridge transport resilience (OpenCode)", () => {
  let preparedBinary: PreparedBinary = initialBinary;
  const harnesses: E2EHarness[] = [];

  beforeAll(async () => {
    preparedBinary = await prepareBinary();
  });

  afterEach(async () => {
    await cleanupHarnesses(harnesses);
  });

  async function harness(): Promise<E2EHarness> {
    const created = await createHarness(preparedBinary, {
      fixtureNames: [],
      timeoutMs: 10_000,
      bridgeOptions: { maxRestarts: 0 },
    });
    harnesses.push(created);
    await writeFile(created.path("sample.txt"), "alpha\nbeta\n", "utf8");
    return created;
  }

  test("foreground bash returns before a short transport timeout and leaves following requests healthy", async () => {
    const h = await harness();
    await h.bridge.send("ping");
    const firstPid = childPid(h.bridge);

    // transportTimeoutMs must be shorter than the bash command duration
    // while leaving enough headroom for Rust spawn/registry/protocol work
    // on contended CI runners. Keep this mirrored with the Pi test.
    const launched = await h.bridge.send(
      "bash",
      { command: "sleep 1 && echo slow", timeout: 5_000, compressed: false },
      { transportTimeoutMs: 500 },
    );

    expect(launched.success).toBe(true);
    expect(launched.status).toBe("running");
    expect(h.bridge.isAlive()).toBe(true);

    const after = await h.bridge.send("read", { file: h.path("sample.txt") });
    expect(after.success).toBe(true);
    expect(String(after.content ?? "")).toContain("alpha");
    expect(h.bridge.isAlive()).toBe(true);
    expect(childPid(h.bridge)).toBe(firstPid);
  });

  test("recovers with a fresh bridge after external SIGKILL", async () => {
    const h = await harness();

    const before = await h.bridge.send("read", { file: h.path("sample.txt") });
    expect(before.success).toBe(true);
    const killedPid = childPid(h.bridge);

    process.kill(killedPid, "SIGKILL");
    await waitForExitHandler(h.bridge as unknown as { process?: ChildProcess | null }, killedPid);

    const after = await h.bridge.send("read", { file: h.path("sample.txt") });
    expect(after.success).toBe(true);
    expect(String(after.content ?? "")).toContain("beta");
    expect(childPid(h.bridge)).not.toBe(killedPid);
  });

  test("reserved command/method/session/lsp params route to the intended command", async () => {
    const h = await harness();

    const commandCollision = await h.bridge.send("bash", {
      command: "printf collision-ok",
      method: "not-a-bridge-method",
      session_id: "reserved-session",
      lsp_hints: { completions: ["test"] },
      timeout: 1_000,
      compressed: false,
    });
    expect(commandCollision.success).toBe(true);
    expect(commandCollision.status).toBe("running");
    let status: Record<string, unknown> = {};
    const started = Date.now();
    while (Date.now() - started < 5_000) {
      status = await h.bridge.send("bash_status", {
        task_id: commandCollision.task_id,
        session_id: "reserved-session",
      });
      if (status.status !== "running") break;
      await new Promise((resolve) => setTimeout(resolve, 50));
    }
    expect(status.output_preview).toBe("collision-ok");

    const sessionSnapshot = await h.bridge.send("snapshot", {
      file: h.path("sample.txt"),
      session_id: "reserved-session",
    });
    expect(sessionSnapshot.success).toBe(true);

    const defaultHistory = await h.bridge.send("edit_history", { file: h.path("sample.txt") });
    expect(defaultHistory.success).toBe(true);
    expect(defaultHistory.entries).toEqual([]);

    const sessionHistory = await h.bridge.send("edit_history", {
      file: h.path("sample.txt"),
      session_id: "reserved-session",
    });
    expect(sessionHistory.success).toBe(true);
    expect((sessionHistory.entries as unknown[]).length).toBe(1);
  });

  test("reserved id params are rejected before corrupting bridge state", async () => {
    const h = await harness();

    await expect(h.bridge.send("read", { id: "1", file: h.path("sample.txt") })).rejects.toThrow(
      "params cannot contain reserved key 'id'",
    );

    const after = await h.bridge.send("read", { file: h.path("sample.txt") });
    expect(after.success).toBe(true);
    expect(String(after.content ?? "")).toContain("alpha");
  });
});
