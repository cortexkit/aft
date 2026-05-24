/// <reference path="../../bun-test.d.ts" />

import { afterEach, beforeAll, describe, expect, test } from "bun:test";
import { writeFile } from "node:fs/promises";
import {
  cleanupHarnesses,
  createHarness,
  type E2EHarness,
  type PreparedBinary,
  prepareBinary,
  readTextFile,
} from "./helpers.js";

const initialBinary = await prepareBinary();
const maybeDescribe = describe.skipIf(!initialBinary.binaryPath);

maybeDescribe("e2e safety commands", () => {
  let preparedBinary: PreparedBinary = initialBinary;
  const harnesses: E2EHarness[] = [];

  beforeAll(async () => {
    preparedBinary = await prepareBinary();
  });

  afterEach(async () => {
    await cleanupHarnesses(harnesses);
  });

  async function harness(): Promise<E2EHarness> {
    const created = await createHarness(preparedBinary);
    harnesses.push(created);
    return created;
  }

  test("creates and restores a checkpoint", async () => {
    const h = await harness();
    const filePath = h.path("sample.ts");
    const original = await readTextFile(filePath);

    const checkpoint = await h.bridge.send("checkpoint", { name: "safe-point", files: [filePath] });
    await h.bridge.send("write", { file: filePath, content: "export const changed = true;\n" });
    const restore = await h.bridge.send("restore_checkpoint", { name: "safe-point" });

    expect(checkpoint.success).toBe(true);
    expect(restore.success).toBe(true);
    expect(await readTextFile(filePath)).toBe(original);
  });

  test("undo reverts an edit", async () => {
    const h = await harness();
    const filePath = h.path("with-errors.ts");
    const original = await readTextFile(filePath);

    const edit = await h.bridge.send("edit_match", {
      file: filePath,
      match: "pending",
      replacement: "ready",
      occurrence: 0,
    });
    const undo = await h.bridge.send("undo", { file: filePath });

    expect(edit.success).toBe(true);
    expect(undo.success).toBe(true);
    expect(await readTextFile(filePath)).toBe(original);
  });

  test("history lists prior snapshots", async () => {
    const h = await harness();
    const filePath = h.path("history.txt");
    await writeFile(filePath, "v1\n");

    await h.bridge.send("write", { file: filePath, content: "v2\n" });
    await h.bridge.send("write", { file: filePath, content: "v3\n" });
    const history = await h.bridge.send("edit_history", { file: filePath });

    expect(history.success).toBe(true);
    expect((history.entries as Array<Record<string, unknown>>).length).toBeGreaterThanOrEqual(2);
  });

  test("multiple undos walk back the stack", async () => {
    const h = await harness();
    const filePath = h.path("undo-stack.txt");
    await writeFile(filePath, "v1\n");

    await h.bridge.send("write", { file: filePath, content: "v2\n" });
    await h.bridge.send("write", { file: filePath, content: "v3\n" });
    await h.bridge.send("undo", { file: filePath });
    expect(await readTextFile(filePath)).toBe("v2\n");

    await h.bridge.send("undo", { file: filePath });
    expect(await readTextFile(filePath)).toBe("v1\n");
  });

  test("list_checkpoints returns created checkpoints", async () => {
    const h = await harness();
    const filePath = h.path("sample.ts");

    await h.bridge.send("checkpoint", { name: "one", files: [filePath] });
    await h.bridge.send("checkpoint", { name: "two", files: [filePath] });
    const response = await h.bridge.send("list_checkpoints");

    expect(response.success).toBe(true);
    const checkpoints = response.checkpoints as Array<Record<string, unknown>>;
    expect(checkpoints.some((checkpoint) => checkpoint.name === "one")).toBe(true);
    expect(checkpoints.some((checkpoint) => checkpoint.name === "two")).toBe(true);
  });

  test("operation undo restores every file from a multi-file delete in one call", async () => {
    // Regression: v0.25 introduced operation-scoped backups. aft_delete
    // files: [a, b, c] writes one op_id; a single `undo` with no `file`
    // param restores all three atomically.
    const h = await harness();
    const fileA = h.path("op-undo-a.txt");
    const fileB = h.path("op-undo-b.txt");
    const fileC = h.path("op-undo-c.txt");

    await writeFile(fileA, "content-a\n");
    await writeFile(fileB, "content-b\n");
    await writeFile(fileC, "content-c\n");

    const deleteResp = await h.bridge.send("delete_file", {
      files: [fileA, fileB, fileC],
    });
    expect(deleteResp.success).toBe(true);
    expect(deleteResp.complete).toBe(true);
    const { existsSync } = await import("node:fs");
    expect(existsSync(fileA)).toBe(false);
    expect(existsSync(fileB)).toBe(false);
    expect(existsSync(fileC)).toBe(false);

    // Operation undo: no `file` param. Restores everything tagged with the
    // most recent op_id atomically.
    const undoResp = await h.bridge.send("undo");
    expect(undoResp.success).toBe(true);
    expect(undoResp.operation).toBe(true);
    expect(undoResp.restored_count).toBe(3);
    expect(await readTextFile(fileA)).toBe("content-a\n");
    expect(await readTextFile(fileB)).toBe("content-b\n");
    expect(await readTextFile(fileC)).toBe("content-c\n");
  });

  test("operation undo restores a recursive directory delete in one call", async () => {
    // Regression: v0.25 added aft_delete recursive: true. Backs every file
    // in the tree under one op_id; single undo restores files AND
    // intermediate parent directories.
    const h = await harness();
    const dir = h.path("op-undo-tree");
    const { mkdir } = await import("node:fs/promises");
    const { existsSync } = await import("node:fs");
    await mkdir(`${dir}/nested`, { recursive: true });
    await writeFile(`${dir}/top.txt`, "top-content\n");
    await writeFile(`${dir}/nested/inner.txt`, "inner-content\n");

    const deleteResp = await h.bridge.send("delete_file", {
      file: dir,
      recursive: true,
    });
    expect(deleteResp.success).toBe(true);
    expect(deleteResp.is_directory).toBe(true);
    expect(deleteResp.files_deleted).toBe(2);
    expect(existsSync(dir)).toBe(false);

    const undoResp = await h.bridge.send("undo");
    expect(undoResp.success).toBe(true);
    expect(undoResp.operation).toBe(true);
    expect(undoResp.restored_count).toBe(2);
    // Both files AND their parent directories must be restored.
    expect(await readTextFile(`${dir}/top.txt`)).toBe("top-content\n");
    expect(await readTextFile(`${dir}/nested/inner.txt`)).toBe("inner-content\n");
  });

  test("recursive delete rejects symlinks before touching the filesystem", async () => {
    // Regression: v0.25 guards recursive delete against symlinks (whose
    // canonical target could be outside the tree) and empty directories
    // (which the backup format can't currently restore).
    const h = await harness();
    const dir = h.path("symlink-guard");
    const outside = h.path("symlink-target.txt");
    const { mkdir, symlink } = await import("node:fs/promises");
    const { existsSync } = await import("node:fs");
    await mkdir(dir, { recursive: true });
    await writeFile(`${dir}/real.txt`, "inside\n");
    await writeFile(outside, "outside\n");
    await symlink(outside, `${dir}/link.txt`);

    const resp = await h.bridge.send("delete_file", {
      file: dir,
      recursive: true,
    });
    expect(resp.success).toBe(false);
    expect(resp.code).toBe("unsupported_directory_contents");
    expect(resp.message as string).toContain("link.txt");
    // The whole tree, the symlink, and the outside target must be untouched.
    expect(existsSync(dir)).toBe(true);
    expect(existsSync(`${dir}/real.txt`)).toBe(true);
    expect(existsSync(`${dir}/link.txt`)).toBe(true);
    expect(await readTextFile(outside)).toBe("outside\n");
  });
});
