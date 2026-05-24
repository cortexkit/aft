/// <reference path="../../bun-test.d.ts" />

import { afterEach, beforeAll, describe, expect, test } from "bun:test";
import { writeFile } from "node:fs/promises";
import {
  __resetBgNotificationStateForTests,
  cleanupIdleSessionStates,
  ingestBgCompletions,
  SESSION_BG_STATE_IDLE_TTL_MS,
  sessionBgStates,
  trackBgTask,
} from "../../bg-notifications.js";
import { createHarness, type Harness, type PreparedBinary, prepareBinary } from "./helpers.js";

const initialBinary = await prepareBinary();
const maybeDescribe = initialBinary.binaryPath ? describe : describe.skip;

maybeDescribe("e2e Pi parity features", () => {
  let preparedBinary: PreparedBinary = initialBinary;
  let harnesses: Harness[] = [];

  beforeAll(async () => {
    preparedBinary = await prepareBinary();
  });

  afterEach(async () => {
    __resetBgNotificationStateForTests();
    await Promise.allSettled(harnesses.map((harness) => harness.cleanup()));
    harnesses = [];
  });

  async function harness(): Promise<Harness> {
    const created = await createHarness(preparedBinary, {
      fixtureNames: [],
      config: { search_index: false },
      timeoutMs: 10_000,
    });
    harnesses.push(created);
    return created;
  }

  test("idle session notification state is evicted and new sessions still work", () => {
    const sessionA = "session-a";
    const taskId = "task-a";
    trackBgTask(sessionA, taskId);
    ingestBgCompletions(sessionA, [
      { task_id: taskId, status: "done", exit_code: 0, command: "echo a" },
    ]);
    expect(sessionBgStates.has(sessionA)).toBe(true);

    const staleState = sessionBgStates.get(sessionA);
    if (!staleState) throw new Error("session A state was not created");
    const staleSeenAt = Date.now() - SESSION_BG_STATE_IDLE_TTL_MS - 1_000;
    staleState.lastSeenAt = staleSeenAt;
    cleanupIdleSessionStates(staleSeenAt + SESSION_BG_STATE_IDLE_TTL_MS + 2_000);
    expect(sessionBgStates.has(sessionA)).toBe(false);

    const sessionB = "session-b";
    trackBgTask(sessionB, "task-b");
    expect(sessionBgStates.has(sessionB)).toBe(true);
    expect(sessionBgStates.has(sessionA)).toBe(false);
  });

  test("reserved id guard rejects user id without corrupting following bridge requests", async () => {
    const h = await harness();
    await writeFile(h.path("sample.txt"), "alpha\nbeta\n", "utf8");

    await expect(
      h.bridge.send("read", { id: "user-id", file: h.path("sample.txt") }),
    ).rejects.toThrow("params cannot contain reserved key 'id'");

    const after = await h.bridge.send("read", { file: h.path("sample.txt") });
    expect(after.success).toBe(true);
    expect(String(after.content ?? "")).toContain("alpha");
  });
});
