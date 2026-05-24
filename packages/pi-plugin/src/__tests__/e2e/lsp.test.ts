/**
 * E2E coverage for lsp_diagnostics.
 *
 * Requires a working TypeScript LSP (tsserver). If none is available in the
 * environment, the bridge may return an empty diagnostics list instead of
 * populated findings — we only assert that the tool dispatches cleanly.
 */

/// <reference path="../../bun-test.d.ts" />

import { afterAll, beforeAll, describe, expect, test } from "bun:test";
import { createHarness, type Harness, prepareBinary, writeFixture } from "./helpers.js";

const initialBinary = await prepareBinary();
const maybeDescribe = initialBinary.binaryPath ? describe : describe.skip;

maybeDescribe("lsp_diagnostics (real bridge)", () => {
  let harness: Harness;

  beforeAll(async () => {
    harness = await createHarness(initialBinary);
  });

  afterAll(async () => {
    if (harness) await harness.cleanup();
  });

  test("returns valid structure for a clean file", async () => {
    await writeFixture(harness, "clean.ts", `export const ok = 1;\n`);
    const result = await harness.callTool("lsp_diagnostics", { filePath: "clean.ts" });
    const text = harness.text(result);
    const parsed = JSON.parse(text);
    expect(parsed).toHaveProperty("diagnostics");
    expect(Array.isArray(parsed.diagnostics)).toBe(true);
    expect(parsed).toHaveProperty("total");
  });

  test("rejects both filePath and directory", async () => {
    await expect(
      harness.callTool("lsp_diagnostics", { filePath: "a.ts", directory: "b" }),
    ).rejects.toThrow(/mutually exclusive/);
  });

  test("empty strings treated as absent — directory mode routes correctly", async () => {
    // Per earlier bugfix: filePath=""/directory="" should not count as present.
    const result = await harness.callTool("lsp_diagnostics", {
      filePath: "",
      directory: ".",
    });
    const text = harness.text(result);
    const parsed = JSON.parse(text);
    expect(parsed).toHaveProperty("diagnostics");
  });

  test("severity filter dispatches", async () => {
    await writeFixture(harness, "severity.ts", `export const x = 1;\n`);
    const result = await harness.callTool("lsp_diagnostics", {
      filePath: "severity.ts",
      severity: "error",
    });
    const text = harness.text(result);
    expect(text).toContain("diagnostics");
  });
});
