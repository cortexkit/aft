/**
 * E2E coverage for ast_grep_search + ast_grep_replace.
 */

/// <reference path="../../bun-test.d.ts" />

import { afterAll, beforeAll, describe, expect, test } from "bun:test";
import { readFile } from "node:fs/promises";
import { createHarness, type Harness, prepareBinary, writeFixture } from "./helpers.js";

const initialBinary = await prepareBinary();
const maybeDescribe = initialBinary.binaryPath ? describe : describe.skip;

maybeDescribe("ast_grep_search + ast_grep_replace (real bridge)", () => {
  let harness: Harness;

  beforeAll(async () => {
    harness = await createHarness(initialBinary);
  });

  afterAll(async () => {
    if (harness) await harness.cleanup();
  });

  test("ast_grep_search finds TS patterns", async () => {
    const result = await harness.callTool("ast_grep_search", {
      pattern: "console.log($MSG)",
      lang: "typescript",
      paths: [harness.tempDir],
      globs: ["multi-match.ts"],
    });
    const text = harness.text(result);
    expect(text).toContain("multi-match.ts");
    // At least one console.log argument should be captured
    expect(text).toMatch(/alpha|beta|gamma/);
  });

  test("ast_grep_search Python meta-variable parsing", async () => {
    await writeFixture(
      harness,
      "py-target.py",
      `def greet(name):\n    return f"hello {name}"\n\ndef ignore():\n    pass\n`,
    );
    const result = await harness.callTool("ast_grep_search", {
      pattern: "def $FUNC($$$): $$$",
      lang: "python",
      paths: [harness.path("py-target.py")],
    });
    const text = harness.text(result);
    expect(text).toContain("greet");
    expect(text).toContain("ignore");
  });

  test("ast_grep_replace rewrites every match in a file", async () => {
    await writeFixture(
      harness,
      "replace-target.ts",
      `function log(msg: string) {\n  console.log(msg);\n  console.log("extra");\n  console.log("more");\n}\n`,
    );
    const replaceResp = await harness.callTool("ast_grep_replace", {
      pattern: "console.log($ARG)",
      rewrite: "logger.info($ARG)",
      lang: "typescript",
      paths: [harness.path("replace-target.ts")],
    });
    void replaceResp;
    const after = await readFile(harness.path("replace-target.ts"), "utf8");
    expect(after).not.toContain("console.log");
    expect(after).toContain("logger.info");
    // All three logs should be replaced
    const matches = after.match(/logger\.info/g);
    expect(matches?.length).toBe(3);
  });

  test("ast_grep_replace dryRun does not mutate", async () => {
    await writeFixture(harness, "dry-ast.ts", `function f() { console.log(1); }\n`);
    const before = await readFile(harness.path("dry-ast.ts"), "utf8");
    await harness.callTool("ast_grep_replace", {
      pattern: "console.log($A)",
      rewrite: "logger.info($A)",
      lang: "typescript",
      paths: [harness.path("dry-ast.ts")],
      dryRun: true,
    });
    const after = await readFile(harness.path("dry-ast.ts"), "utf8");
    expect(after).toBe(before);
  });
});
