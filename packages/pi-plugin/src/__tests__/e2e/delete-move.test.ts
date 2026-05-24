/**
 * E2E coverage for aft_delete + aft_move.
 */

/// <reference path="../../bun-test.d.ts" />

import { afterAll, beforeAll, describe, expect, test } from "bun:test";
import { constants } from "node:fs";
import { access, readFile } from "node:fs/promises";
import { createHarness, type Harness, prepareBinary, writeFixture } from "./helpers.js";

const initialBinary = await prepareBinary();
const maybeDescribe = initialBinary.binaryPath ? describe : describe.skip;

async function exists(p: string): Promise<boolean> {
  try {
    await access(p, constants.F_OK);
    return true;
  } catch {
    return false;
  }
}

maybeDescribe("aft_delete + aft_move (real bridge)", () => {
  let harness: Harness;

  beforeAll(async () => {
    harness = await createHarness(initialBinary);
  });

  afterAll(async () => {
    if (harness) await harness.cleanup();
  });

  test("aft_delete removes a file", async () => {
    await writeFixture(harness, "doomed.ts", "export const x = 1;\n");
    expect(await exists(harness.path("doomed.ts"))).toBe(true);
    await harness.callTool("aft_delete", { files: ["doomed.ts"] });
    expect(await exists(harness.path("doomed.ts"))).toBe(false);
  });

  test("aft_delete removes multiple files in one call", async () => {
    await writeFixture(harness, "bulk-a.ts", "a\n");
    await writeFixture(harness, "bulk-b.ts", "b\n");
    await writeFixture(harness, "bulk-c.ts", "c\n");
    await harness.callTool("aft_delete", {
      files: ["bulk-a.ts", "bulk-b.ts", "bulk-c.ts"],
    });
    expect(await exists(harness.path("bulk-a.ts"))).toBe(false);
    expect(await exists(harness.path("bulk-b.ts"))).toBe(false);
    expect(await exists(harness.path("bulk-c.ts"))).toBe(false);
  });

  test("aft_delete reports skipped files in partial failure", async () => {
    await writeFixture(harness, "real.ts", "x\n");
    const result = await harness.callTool("aft_delete", {
      files: ["real.ts", "does-not-exist.ts"],
    });
    expect(await exists(harness.path("real.ts"))).toBe(false);
    const text = String(result?.content?.[0]?.text ?? "");
    // Result text mentions partial completion
    expect(text).toMatch(/Deleted 1\/2/);
  });

  test("aft_move renames a file and preserves contents", async () => {
    const content = "export const greeting = 'hi';\n";
    await writeFixture(harness, "movable.ts", content);
    await harness.callTool("aft_move", {
      filePath: "movable.ts",
      destination: "moved.ts",
    });
    expect(await exists(harness.path("movable.ts"))).toBe(false);
    expect(await exists(harness.path("moved.ts"))).toBe(true);
    expect(await readFile(harness.path("moved.ts"), "utf8")).toBe(content);
  });

  test("aft_move creates parent directories for destination", async () => {
    await writeFixture(harness, "nested-src.ts", "n\n");
    await harness.callTool("aft_move", {
      filePath: "nested-src.ts",
      destination: "sub/dir/nested-dst.ts",
    });
    expect(await exists(harness.path("sub/dir/nested-dst.ts"))).toBe(true);
  });
});
