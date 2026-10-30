import { expect, spyOn, test } from "bun:test";
import type { AftTransportPool } from "@cortexkit/aft-bridge";
import { runDetachedBgHandler } from "../bg-notifications.js";
import { closeDeletedSessionRoutes } from "../shared/session-directory.js";
import { schedulePassiveTimer } from "../shutdown-hooks.js";

test("deleted session cleanup uses every previously bound root rather than init cwd", async () => {
  const closed: string[] = [];
  const pool = {
    activeBridges: () => [
      { getCwd: () => "/resumed-worktree" },
      { getCwd: () => "/early-fallback" },
    ],
    closeSession: async (root: string, session: string) => {
      expect(session).toBe("deleted");
      closed.push(root);
    },
  } as unknown as AftTransportPool;
  await closeDeletedSessionRoutes(pool, "deleted", "/launch-cwd");
  expect(closed).toContain("/resumed-worktree");
  expect(closed).toContain("/early-fallback");
});

test("detached background callback catches and reports outer handler rejection", async () => {
  const rejection = new Error("pre-scheduling failed");
  const errors: unknown[] = [];
  await expect(
    runDetachedBgHandler(
      async () => {
        throw rejection;
      },
      (error) => {
        errors.push(error);
      },
    ),
  ).resolves.toBeUndefined();
  expect(errors).toEqual([rejection]);
});

test("announcement timer is passive and its owner can cancel it", () => {
  let unrefs = 0;
  const timer = {
    unref: () => {
      unrefs++;
    },
  } as unknown as ReturnType<typeof setTimeout>;
  const set = spyOn(globalThis, "setTimeout").mockReturnValue(timer);
  const clear = spyOn(globalThis, "clearTimeout").mockImplementation(() => {});
  try {
    const cancel = schedulePassiveTimer(() => {}, 8000);
    expect(unrefs).toBe(1);
    cancel();
    expect(clear).toHaveBeenCalledWith(timer);
  } finally {
    set.mockRestore();
    clear.mockRestore();
  }
});
