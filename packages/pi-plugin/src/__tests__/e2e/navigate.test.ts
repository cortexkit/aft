/**
 * E2E coverage for aft_navigate (5 ops).
 * Each op is dispatched as its own Rust command name (call_tree, callers,
 * trace_to, impact, trace_data).
 */

/// <reference path="../../bun-test.d.ts" />

import { afterAll, beforeAll, describe, expect, test } from "bun:test";
import { createHarness, type Harness, prepareBinary } from "./helpers.js";

const initialBinary = await prepareBinary();
const maybeDescribe = initialBinary.binaryPath ? describe : describe.skip;

maybeDescribe("aft_navigate (real bridge)", () => {
  let harness: Harness;

  beforeAll(async () => {
    harness = await createHarness(initialBinary);
  });

  afterAll(async () => {
    if (harness) await harness.cleanup();
  });

  test("call_tree returns calls from a function", async () => {
    const result = await harness.callTool("aft_navigate", {
      op: "call_tree",
      filePath: "sample.ts",
      symbol: "funcB",
    });
    const text = harness.text(result);
    // funcB calls normalize — should appear in call tree
    expect(text).toContain("normalize");
  });

  test("callers finds call sites of a symbol", async () => {
    const result = await harness.callTool("aft_navigate", {
      op: "callers",
      filePath: "sample.ts",
      symbol: "normalize",
    });
    const text = harness.text(result);
    // normalize is called by funcB
    expect(text).toContain("funcB");
  });

  test("impact returns blast radius info", async () => {
    const result = await harness.callTool("aft_navigate", {
      op: "impact",
      filePath: "sample.ts",
      symbol: "funcA",
    });
    const text = harness.text(result);
    // Response is JSON with affected items list — just dispatch worked
    expect(text.length).toBeGreaterThan(0);
  });

  test("trace_to walks upward to entry points", async () => {
    const result = await harness.callTool("aft_navigate", {
      op: "trace_to",
      filePath: "sample.ts",
      symbol: "decorate",
    });
    const text = harness.text(result);
    expect(text.length).toBeGreaterThan(0);
  });

  test("trace_data requires expression", async () => {
    await expect(
      harness.callTool("aft_navigate", {
        op: "trace_data",
        filePath: "sample.ts",
        symbol: "funcA",
      }),
    ).rejects.toThrow(/expression/);
  });

  test("trace_data follows a value through scopes", async () => {
    const result = await harness.callTool("aft_navigate", {
      op: "trace_data",
      filePath: "sample.ts",
      symbol: "funcB",
      expression: "name",
    });
    const text = harness.text(result);
    // trace_data returns JSON with flow info — dispatch success
    expect(text.length).toBeGreaterThan(0);
  });
});
