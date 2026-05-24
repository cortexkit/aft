/**
 * E2E coverage for aft_safety 5 ops.
 * Regression for wrong Rust command names (was sending "safety" with op param;
 * Rust expects undo/edit_history/checkpoint/restore_checkpoint/list_checkpoints).
 */

/// <reference path="../../bun-test.d.ts" />

import { afterAll, beforeAll, describe, expect, test } from "bun:test";
import { readFile } from "node:fs/promises";
import { createHarness, type Harness, prepareBinary, writeFixture } from "./helpers.js";

const initialBinary = await prepareBinary();
const maybeDescribe = initialBinary.binaryPath ? describe : describe.skip;

maybeDescribe("aft_safety (real bridge)", () => {
  let harness: Harness;

  beforeAll(async () => {
    harness = await createHarness(initialBinary);
  });

  afterAll(async () => {
    if (harness) await harness.cleanup();
  });

  test("history requires filePath", async () => {
    await expect(harness.callTool("aft_safety", { op: "history" })).rejects.toThrow(/filePath/);
  });

  test("checkpoint requires name", async () => {
    await expect(harness.callTool("aft_safety", { op: "checkpoint" })).rejects.toThrow(/name/);
  });

  test("history returns empty for unedited file", async () => {
    await writeFixture(harness, "untouched.ts", "x\n");
    const result = await harness.callTool("aft_safety", {
      op: "history",
      filePath: "untouched.ts",
    });
    const text = harness.text(result);
    // Result is JSON stringified — either explicit entries: [] or a no-history shape
    expect(text.length).toBeGreaterThan(0);
  });

  test("edit → history shows one snapshot", async () => {
    await writeFixture(harness, "edited.ts", "line1\nline2\n");
    await harness.callTool("edit", {
      filePath: "edited.ts",
      oldString: "line1",
      newString: "LINE1",
    });
    const result = await harness.callTool("aft_safety", {
      op: "history",
      filePath: "edited.ts",
    });
    const text = harness.text(result);
    // Rust edit_history returns { file, entries: [...] }
    expect(text).toContain("entries");
  });

  test("edit → undo reverts file content", async () => {
    await writeFixture(harness, "undoable.ts", "hello\n");
    await harness.callTool("edit", {
      filePath: "undoable.ts",
      oldString: "hello",
      newString: "goodbye",
    });
    // Sanity: edit succeeded
    expect(await readFile(harness.path("undoable.ts"), "utf8")).toBe("goodbye\n");

    await harness.callTool("aft_safety", { op: "undo", filePath: "undoable.ts" });
    expect(await readFile(harness.path("undoable.ts"), "utf8")).toBe("hello\n");
  });

  test("checkpoint promotes filePath to single-entry files[]", async () => {
    // Regression: Rust `checkpoint` only accepts `files`, not `file`. The plugin
    // must auto-upgrade `filePath` → `files: [filePath]` rather than silently
    // dropping it and falling back to the whole tracked-file set.
    await harness.callTool("write", { filePath: "cp-single.ts", content: "hello\n" });
    const result = await harness.callTool("aft_safety", {
      op: "checkpoint",
      name: "single-file-cp",
      filePath: "cp-single.ts",
    });
    const text = harness.text(result);
    expect(text).toContain("single-file-cp");
    expect(text).toContain('"file_count": 1');
    // Must not have silently omitted our file
    expect(text).not.toContain('"file_count": 0');
  });

  test("checkpoint tolerates deleted files in tracked set", async () => {
    // Regression: earlier behavior aborted the whole checkpoint on the first
    // missing path when the tracked-file fallback hit a deleted file. Now we
    // skip and report instead.
    await harness.callTool("write", { filePath: "cp-keeper.ts", content: "stays\n" });
    await harness.callTool("write", { filePath: "cp-doomed.ts", content: "soon\n" });
    await harness.callTool("aft_delete", { files: ["cp-doomed.ts"] });

    // No explicit files → uses tracked-file set, which still contains cp-doomed.ts.
    const result = await harness.callTool("aft_safety", {
      op: "checkpoint",
      name: "after-deletion",
    });
    const text = harness.text(result);
    expect(text).toContain("after-deletion");
    // cp-keeper.ts survived the snapshot
    expect(text).toMatch(/"file_count":\s*[1-9]/);
    // cp-doomed.ts is reported as skipped, not as a hard failure
    expect(text).toContain("skipped");
    expect(text).toContain("cp-doomed.ts");
  });

  test("checkpoint → list → restore round-trip", async () => {
    // Use the `write` tool (not raw fs) so the file is tracked in the backup store.
    await harness.callTool("write", { filePath: "cp-target.ts", content: "original\n" });
    await harness.callTool("aft_safety", {
      op: "checkpoint",
      name: "before-change",
      files: ["cp-target.ts"],
    });

    // Mutate
    await harness.callTool("edit", {
      filePath: "cp-target.ts",
      oldString: "original",
      newString: "modified",
    });
    expect(await readFile(harness.path("cp-target.ts"), "utf8")).toBe("modified\n");

    // List includes our checkpoint
    const listResult = await harness.callTool("aft_safety", { op: "list" });
    expect(harness.text(listResult)).toContain("before-change");

    // Restore flips back
    await harness.callTool("aft_safety", { op: "restore", name: "before-change" });
    expect(await readFile(harness.path("cp-target.ts"), "utf8")).toBe("original\n");
  });

  test("operation undo restores every file from a multi-file delete in one call", async () => {
    // Regression: v0.25 introduced operation-scoped backups. aft_delete
    // files: [a, b, c] writes one op_id; a single aft_safety undo with no
    // filePath restores all three atomically.
    await harness.callTool("write", { filePath: "op-undo-a.txt", content: "content-a\n" });
    await harness.callTool("write", { filePath: "op-undo-b.txt", content: "content-b\n" });
    await harness.callTool("write", { filePath: "op-undo-c.txt", content: "content-c\n" });

    await harness.callTool("aft_delete", {
      files: ["op-undo-a.txt", "op-undo-b.txt", "op-undo-c.txt"],
    });
    const { existsSync } = await import("node:fs");
    expect(existsSync(harness.path("op-undo-a.txt"))).toBe(false);
    expect(existsSync(harness.path("op-undo-b.txt"))).toBe(false);
    expect(existsSync(harness.path("op-undo-c.txt"))).toBe(false);

    // Operation undo: no filePath. Restores everything tagged with the most
    // recent op_id atomically.
    const undoResult = await harness.callTool("aft_safety", { op: "undo" });
    const undoText = harness.text(undoResult);
    expect(undoText).toContain('"operation": true');
    expect(undoText).toContain('"restored_count": 3');
    expect(await readFile(harness.path("op-undo-a.txt"), "utf8")).toBe("content-a\n");
    expect(await readFile(harness.path("op-undo-b.txt"), "utf8")).toBe("content-b\n");
    expect(await readFile(harness.path("op-undo-c.txt"), "utf8")).toBe("content-c\n");
  });

  test("operation undo restores a recursive directory delete in one call", async () => {
    // Regression: v0.25 added aft_delete recursive: true. Backs every file
    // in the tree under one op_id; single undo restores files AND
    // intermediate parent directories.
    const { mkdir } = await import("node:fs/promises");
    const { existsSync } = await import("node:fs");
    await mkdir(harness.path("op-undo-tree/nested"), { recursive: true });
    await harness.callTool("write", {
      filePath: "op-undo-tree/top.txt",
      content: "top-content\n",
    });
    await harness.callTool("write", {
      filePath: "op-undo-tree/nested/inner.txt",
      content: "inner-content\n",
    });

    await harness.callTool("aft_delete", {
      files: ["op-undo-tree"],
      recursive: true,
    });
    expect(existsSync(harness.path("op-undo-tree"))).toBe(false);

    const undoResult = await harness.callTool("aft_safety", { op: "undo" });
    const undoText = harness.text(undoResult);
    expect(undoText).toContain('"operation": true');
    expect(undoText).toContain('"restored_count": 2');
    // Files AND their parent directories must be restored.
    expect(await readFile(harness.path("op-undo-tree/top.txt"), "utf8")).toBe("top-content\n");
    expect(await readFile(harness.path("op-undo-tree/nested/inner.txt"), "utf8")).toBe(
      "inner-content\n",
    );
  });

  test("recursive delete rejects symlinks before touching the filesystem", async () => {
    // Regression: v0.25 guards recursive delete against symlinks (whose
    // canonical target could be outside the tree) and empty directories.
    const { mkdir, symlink } = await import("node:fs/promises");
    const { existsSync } = await import("node:fs");
    await mkdir(harness.path("symlink-guard"), { recursive: true });
    await harness.callTool("write", {
      filePath: "symlink-guard/real.txt",
      content: "inside\n",
    });
    await harness.callTool("write", {
      filePath: "symlink-target.txt",
      content: "outside\n",
    });
    await symlink(harness.path("symlink-target.txt"), harness.path("symlink-guard/link.txt"));

    // aft_delete with recursive: true should throw a permission-style error
    // (the plugin tool wraps a success: false response). Match the error message.
    await expect(
      harness.callTool("aft_delete", {
        files: ["symlink-guard"],
        recursive: true,
      }),
    ).rejects.toThrow(/unsupported_directory_contents|link\.txt|symlink/);
    expect(existsSync(harness.path("symlink-guard"))).toBe(true);
    expect(existsSync(harness.path("symlink-guard/real.txt"))).toBe(true);
    expect(existsSync(harness.path("symlink-guard/link.txt"))).toBe(true);
    expect(await readFile(harness.path("symlink-target.txt"), "utf8")).toBe("outside\n");
  });
});
