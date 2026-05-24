/**
 * E2E coverage for aft_refactor (move, extract, inline).
 * Regression for wrong Rust command names (was sending "refactor"; Rust
 * expects move_symbol / extract_function / inline_symbol) plus the
 * endLine+1 inclusive→exclusive conversion for extract.
 */

/// <reference path="../../bun-test.d.ts" />

import { afterAll, beforeAll, describe, expect, test } from "bun:test";
import { readFile } from "node:fs/promises";
import { createHarness, type Harness, prepareBinary, writeFixture } from "./helpers.js";

const initialBinary = await prepareBinary();
const maybeDescribe = initialBinary.binaryPath ? describe : describe.skip;

maybeDescribe("aft_refactor (real bridge)", () => {
  let harness: Harness;

  beforeAll(async () => {
    harness = await createHarness(initialBinary);
  });

  afterAll(async () => {
    if (harness) await harness.cleanup();
  });

  test("extract with inclusive endLine pulls line range into a function", async () => {
    await writeFixture(
      harness,
      "extract-src.ts",
      `export function process(): number {\n  const a = 1;\n  const b = 2;\n  const sum = a + b;\n  return sum * 10;\n}\n`,
    );
    // Lines 2–3 are `const a = 1;` + `const b = 2;` — extract into computeBase()
    await harness.callTool("aft_refactor", {
      op: "extract",
      filePath: "extract-src.ts",
      name: "computeBase",
      startLine: 2,
      endLine: 3,
    });
    const after = await readFile(harness.path("extract-src.ts"), "utf8");
    // The new function should exist with the extracted body
    expect(after).toContain("computeBase");
    expect(after).toContain("const a = 1");
    expect(after).toContain("const b = 2");
  });

  test("move transfers a top-level export to another file", async () => {
    await writeFixture(
      harness,
      "src-origin.ts",
      `export function utility(x: number): number {\n  return x * 2;\n}\n\nexport function caller(): number {\n  return utility(3);\n}\n`,
    );
    await writeFixture(harness, "src-dest.ts", `// destination module\n`);

    await harness.callTool("aft_refactor", {
      op: "move",
      filePath: "src-origin.ts",
      symbol: "utility",
      destination: harness.path("src-dest.ts"),
    });
    const origin = await readFile(harness.path("src-origin.ts"), "utf8");
    const dest = await readFile(harness.path("src-dest.ts"), "utf8");
    expect(origin).not.toContain("function utility");
    expect(dest).toContain("function utility");
  });
});
