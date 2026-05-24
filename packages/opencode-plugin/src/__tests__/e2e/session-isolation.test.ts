/// <reference path="../../bun-test.d.ts" />

import { afterEach, beforeAll, describe, expect, test } from "bun:test";
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

/**
 * Cover the Rust-side session isolation for issue #14.
 *
 * A single bridge now serves multiple OpenCode sessions in the same project.
 * These tests drive the bridge directly and confirm that `session_id` scopes
 * undo history and checkpoints so concurrent sessions can't see or restore
 * each other's state. Bridge-pool sharing is covered by pool.test.ts.
 */
maybeDescribe("e2e session isolation", () => {
  let preparedBinary: PreparedBinary = initialBinary;
  const harnesses: E2EHarness[] = [];

  beforeAll(async () => {
    preparedBinary = await prepareBinary();
  });

  afterEach(async () => {
    await cleanupHarnesses(harnesses);
  });

  async function harness(): Promise<E2EHarness> {
    const created = await createHarness(preparedBinary, { timeoutMs: 15_000 });
    harnesses.push(created);
    return created;
  }

  test("undo stacks are scoped per session on the same file", async () => {
    const h = await harness();
    const filePath = h.path("shared.ts");
    await writeFile(filePath, "export const v = 1;\n");

    // Session A snapshots v=1, then writes v=2 through snapshot+external write.
    const snapA = await h.bridge.send("snapshot", { file: filePath, session_id: "A" });
    expect(snapA.success).toBe(true);
    await writeFile(filePath, "export const v = 2;\n");

    // Session B hasn't snapshotted — undo must reject even though the file exists.
    const undoB = await h.bridge.send("undo", { file: filePath, session_id: "B" });
    expect(undoB.success).toBe(false);
    expect(undoB.code).toBe("no_undo_history");

    // Session A's undo still works and restores v=1.
    const undoA = await h.bridge.send("undo", { file: filePath, session_id: "A" });
    expect(undoA.success).toBe(true);
    const after = await h.bridge.send("read", { file: filePath });
    expect(after.success).toBe(true);
    expect(String(after.content ?? "")).toContain("export const v = 1;");
  });

  test("edit_history is scoped per session", async () => {
    const h = await harness();
    const filePath = h.path("history.ts");
    await writeFile(filePath, "export const a = 0;\n");

    // Session A takes two snapshots of the same file.
    await h.bridge.send("snapshot", { file: filePath, session_id: "A" });
    await writeFile(filePath, "export const a = 1;\n");
    await h.bridge.send("snapshot", { file: filePath, session_id: "A" });

    const historyA = await h.bridge.send("edit_history", {
      file: filePath,
      session_id: "A",
    });
    expect(historyA.success).toBe(true);
    expect(Array.isArray(historyA.entries)).toBe(true);
    expect((historyA.entries as unknown[]).length).toBe(2);

    const historyB = await h.bridge.send("edit_history", {
      file: filePath,
      session_id: "B",
    });
    expect(historyB.success).toBe(true);
    expect(Array.isArray(historyB.entries)).toBe(true);
    // B never snapshotted this file — its history is empty.
    expect((historyB.entries as unknown[]).length).toBe(0);
  });

  test("checkpoints with the same name don't collide across sessions", async () => {
    const h = await harness();
    const fileA = h.path("a.ts");
    const fileB = h.path("b.ts");
    await writeFile(fileA, "export const a = 0;\n");
    await writeFile(fileB, "export const b = 0;\n");

    // Both sessions create a checkpoint named "snap" but pointing at different files.
    const cpA = await h.bridge.send("checkpoint", {
      name: "snap",
      files: [fileA],
      session_id: "A",
    });
    expect(cpA.success).toBe(true);

    const cpB = await h.bridge.send("checkpoint", {
      name: "snap",
      files: [fileB],
      session_id: "B",
    });
    expect(cpB.success).toBe(true);

    // Each list shows only that session's entry.
    const listA = await h.bridge.send("list_checkpoints", { session_id: "A" });
    expect(listA.success).toBe(true);
    expect((listA.checkpoints as unknown[]).length).toBe(1);

    const listB = await h.bridge.send("list_checkpoints", { session_id: "B" });
    expect(listB.success).toBe(true);
    expect((listB.checkpoints as unknown[]).length).toBe(1);

    // Modify both files externally, then restore session A's checkpoint.
    await writeFile(fileA, "export const a = 99;\n");
    await writeFile(fileB, "export const b = 99;\n");

    const restoreA = await h.bridge.send("restore_checkpoint", {
      name: "snap",
      session_id: "A",
    });
    expect(restoreA.success).toBe(true);

    // Only file A should be restored — file B was never in session A's checkpoint.
    const afterA = await h.bridge.send("read", { file: fileA });
    expect(afterA.success).toBe(true);
    expect(String(afterA.content ?? "")).toContain("export const a = 0;");

    const afterB = await h.bridge.send("read", { file: fileB });
    expect(afterB.success).toBe(true);
    expect(String(afterB.content ?? "")).toContain("export const b = 99;");
  });

  test("restore_checkpoint from the wrong session returns checkpoint_not_found", async () => {
    const h = await harness();
    const filePath = h.path("only-a.ts");
    await writeFile(filePath, "export const a = 0;\n");

    const create = await h.bridge.send("checkpoint", {
      name: "only-a",
      files: [filePath],
      session_id: "A",
    });
    expect(create.success).toBe(true);

    const restore = await h.bridge.send("restore_checkpoint", {
      name: "only-a",
      session_id: "B",
    });
    expect(restore.success).toBe(false);
    expect(restore.code).toBe("checkpoint_not_found");
  });

  test("requests without session_id share a default namespace (backward compat)", async () => {
    const h = await harness();
    const filePath = h.path("default.ts");
    await writeFile(filePath, "export const d = 0;\n");

    // No session_id on either call — both land in the __default__ namespace.
    await h.bridge.send("snapshot", { file: filePath });
    await writeFile(filePath, "export const d = 1;\n");
    const undo = await h.bridge.send("undo", { file: filePath });
    expect(undo.success).toBe(true);

    const after = await h.bridge.send("read", { file: filePath });
    expect(String(after.content ?? "")).toContain("export const d = 0;");
  });
});
