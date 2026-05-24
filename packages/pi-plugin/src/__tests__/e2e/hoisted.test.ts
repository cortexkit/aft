/**
 * E2E coverage for Pi's hoisted read/write/edit/grep tools.
 *
 * These are registered with Pi's built-in tool names so registerTool replaces
 * Pi's default implementation. Each routes through BinaryBridge to the Rust
 * aft binary.
 */

/// <reference path="../../bun-test.d.ts" />

import { afterAll, beforeAll, describe, expect, test } from "bun:test";
import { readFile } from "node:fs/promises";
import { createHarness, type Harness, prepareBinary } from "./helpers.js";

const initialBinary = await prepareBinary();
const maybeDescribe = initialBinary.binaryPath ? describe : describe.skip;

maybeDescribe("hoisted tools (real bridge)", () => {
  let harness: Harness;

  beforeAll(async () => {
    harness = await createHarness(initialBinary);
  });

  afterAll(async () => {
    if (harness) await harness.cleanup();
  });

  test("read returns file content", async () => {
    const result = await harness.callTool("read", { path: "sample.ts" });
    const text = harness.text(result);
    expect(text).toContain("DEFAULT_SUFFIX");
    expect(text).toContain("funcA");
    // Line-numbered output
    expect(text).toMatch(/\d+:\s/);
  });

  test("read honors offset/limit (Pi-style paging)", async () => {
    const result = await harness.callTool("read", {
      path: "sample.ts",
      offset: 4,
      limit: 2,
    });
    const text = harness.text(result);
    expect(text).toContain("DEFAULT_SUFFIX");
    expect(text).toContain("LOCAL_SEPARATOR");
    expect(text).not.toContain("readFileSync"); // line 1 excluded
  });

  test("read directory lists entries", async () => {
    const result = await harness.callTool("read", { path: "directory" });
    const text = harness.text(result);
    expect(text.length).toBeGreaterThan(0);
  });

  test("write creates a new file and reports diff + details", async () => {
    const rel = "written.ts";
    const content = "export const hello = 'world';\n";
    const result = await harness.callTool("write", { filePath: rel, content });

    // Agent-facing text: summary header.
    const text = harness.text(result);
    expect(text).toMatch(/Wrote .*written\.ts \(\+\d+\/-\d+\)/);

    // Structured details: matches Pi's result-renderer contract.
    const details = result.details as { additions: number; deletions: number } | undefined;
    expect(details).toBeDefined();
    expect(typeof details?.additions).toBe("number");
    expect(typeof details?.deletions).toBe("number");

    // File actually written.
    const actual = await readFile(harness.path(rel), "utf8");
    expect(actual).toBe(content);
  });

  test("edit with oldString/newString replaces exact match and returns diff text", async () => {
    await harness.callTool("write", {
      filePath: "edit-target.ts",
      content: "export const suffix = '!';\n",
    });
    const result = await harness.callTool("edit", {
      filePath: "edit-target.ts",
      oldString: "'!'",
      newString: "'?'",
    });

    // Summary header mentions replacements.
    const text = harness.text(result);
    expect(text).toMatch(/Edited .*edit-target\.ts \(\+\d+\/-\d+, 1 replacement\)/);

    // Diff text uses Pi's line-numbered format: "+NN content" / "-NN content".
    expect(text).toMatch(/^[+-]\s*\d+ /m);

    // Details carry the diff and firstChangedLine for the renderer.
    const details = result.details as
      | {
          diff?: string;
          firstChangedLine?: number;
          replacements?: number;
        }
      | undefined;
    expect(details?.diff).toBeDefined();
    expect(typeof details?.firstChangedLine).toBe("number");
    expect(details?.replacements).toBe(1);

    const actual = await readFile(harness.path("edit-target.ts"), "utf8");
    expect(actual).toBe("export const suffix = '?';\n");
  });

  test("edit with replaceAll rewrites every occurrence and reports count", async () => {
    await harness.callTool("write", {
      filePath: "edit-all.ts",
      content: "a\na\na\n",
    });
    const result = await harness.callTool("edit", {
      filePath: "edit-all.ts",
      oldString: "a",
      newString: "b",
      replaceAll: true,
    });
    const text = harness.text(result);
    expect(text).toMatch(/3 replacements/);

    const actual = await readFile(harness.path("edit-all.ts"), "utf8");
    expect(actual).toBe("b\nb\nb\n");
  });

  test("grep finds literal patterns", async () => {
    const result = await harness.callTool("grep", { pattern: "funcA" });
    const text = harness.text(result);
    expect(text).toContain("sample.ts");
    expect(text).toContain("funcA");
  });

  test("grep honors include glob", async () => {
    const result = await harness.callTool("grep", {
      pattern: "console.log",
      include: "*.ts",
    });
    const text = harness.text(result);
    expect(text).toContain("multi-match.ts");
  });
});
