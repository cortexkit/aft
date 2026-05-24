/**
 * E2E coverage for aft_conflicts.
 * Regression for wrong Rust command name ("conflicts" → "git_conflicts").
 */

/// <reference path="../../bun-test.d.ts" />

import { afterAll, beforeAll, describe, expect, test } from "bun:test";
import { createConflictRepo, createHarness, type Harness, prepareBinary } from "./helpers.js";

const initialBinary = await prepareBinary();
const maybeDescribe = initialBinary.binaryPath ? describe : describe.skip;

maybeDescribe("aft_conflicts (real bridge)", () => {
  let harness: Harness;

  beforeAll(async () => {
    harness = await createHarness(initialBinary, { noFixtures: true });
    await createConflictRepo(harness, "conflicted.txt");
  });

  afterAll(async () => {
    if (harness) await harness.cleanup();
  });

  test("returns conflict regions for in-progress merge", async () => {
    const result = await harness.callTool("aft_conflicts", {});
    const text = harness.text(result);
    expect(text).toContain("conflicted.txt");
    expect(text).toContain("<<<<<<<");
    expect(text).toContain("=======");
    expect(text).toContain(">>>>>>>");
    expect(text).toContain("from-a");
    expect(text).toContain("from-b");
  });
});
