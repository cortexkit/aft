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

maybeDescribe("e2e edit commands", () => {
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

  test("edit_match replaces an exact match", async () => {
    const h = await harness();
    const filePath = h.path("sample.ts");

    const response = await h.bridge.send("edit_match", {
      file: filePath,
      match: "funcA",
      replacement: "funcAlpha",
    });

    expect(response.success).toBe(true);
    expect(response.replacements).toBe(1);
    expect(await readTextFile(filePath)).toContain("funcAlpha");
  });

  test("edit_match supports fuzzy matching", async () => {
    const h = await harness();
    const filePath = h.path("with-errors.ts");

    const response = await h.bridge.send("edit_match", {
      file: filePath,
      match: "const result = value.trim();",
      replacement: "const result = value.trimStart();",
    });

    expect(response.success).toBe(true);
    expect(await readTextFile(filePath)).toContain("trimStart()");
  });

  test("edit_match replace_all updates every match", async () => {
    const h = await harness();
    const filePath = h.path("multi-match.ts");

    const response = await h.bridge.send("edit_match", {
      file: filePath,
      match: "console.log",
      replacement: "logger.info",
      replace_all: true,
    });

    expect(response.success).toBe(true);
    expect(response.replacements).toBe(5);
    const content = await readTextFile(filePath);
    expect(content.match(/logger\.info/g)?.length).toBe(5);
    expect(content).not.toContain("console.log");
  });

  test("edit_match occurrence edits only the selected match", async () => {
    const h = await harness();
    const filePath = h.path("multi-match.ts");

    const response = await h.bridge.send("edit_match", {
      file: filePath,
      match: "console.log",
      replacement: "logger.warn",
      occurrence: 2,
    });

    expect(response.success).toBe(true);
    const content = await readTextFile(filePath);
    expect(content.match(/logger\.warn/g)?.length).toBe(1);
    expect(content.match(/console\.log/g)?.length).toBe(4);
  });

  test("batch applies multiple match edits atomically", async () => {
    const h = await harness();
    const filePath = h.path("with-errors.ts");

    const response = await h.bridge.send("batch", {
      file: filePath,
      edits: [
        { match: 'return "EMPTY";', replacement: 'return "empty";' },
        { match: "return result.toLowerCase();", replacement: "return result.toUpperCase();" },
      ],
    });

    expect(response.success).toBe(true);
    const content = await readTextFile(filePath);
    expect(content).toContain('return "empty";');
    expect(content).toContain("toUpperCase");
  });

  test("batch accepts oldString/newString keys", async () => {
    const h = await harness();
    const filePath = h.path("with-errors.ts");

    const response = await h.bridge.send("batch", {
      file: filePath,
      edits: [
        {
          oldString: 'export const statusMessage = "pending";',
          newString: 'export const statusMessage = "ready";',
        },
        {
          oldString: 'export const duplicateMessage = "pending";',
          newString: 'export const duplicateMessage = "done";',
        },
      ],
    });

    expect(response.success).toBe(true);
    const content = await readTextFile(filePath);
    expect(content).toContain('statusMessage = "ready"');
    expect(content).toContain('duplicateMessage = "done"');
  });

  test("edit_symbol replaces a function by symbol name", async () => {
    const h = await harness();
    const filePath = h.path("sample.ts");

    const response = await h.bridge.send("edit_symbol", {
      file: filePath,
      symbol: "funcB",
      operation: "replace",
      content: "export function funcB(name: string): string {\n  return `updated:${name}`;\n}",
    });

    expect(response.success).toBe(true);
    expect(response.symbol).toBe("funcB");
    expect(await readTextFile(filePath)).toContain("updated:");
  });

  test("transaction updates multiple files", async () => {
    const h = await harness();
    const fileA = h.path("sample.ts");
    const fileB = h.path("with-errors.ts");

    const response = await h.bridge.send("transaction", {
      operations: [
        { file: fileA, command: "edit_match", match: "funcA", replacement: "funcATransaction" },
        {
          file: fileB,
          command: "write",
          content: 'export const transactionState = "ok";\n',
        },
      ],
    });

    expect(response.success).toBe(true);
    expect(response.files_modified).toBe(2);
    expect(await readTextFile(fileA)).toContain("funcATransaction");
    expect(await readTextFile(fileB)).toBe('export const transactionState = "ok";\n');
  });

  test("edit_match supports glob patterns across files", async () => {
    const h = await harness();

    const response = await h.bridge.send("edit_match", {
      file: `${h.path("directory")}/**/*.ts`,
      match: "OLD_VALUE",
      replacement: "NEW_VALUE",
    });

    expect(response.success).toBe(true);
    expect(response.total_files).toBe(3);
    expect(response.total_replacements).toBe(3);
    expect(await readTextFile(h.path("directory", "alpha.ts"))).toContain("NEW_VALUE");
    expect(await readTextFile(h.path("directory", "beta.ts"))).toContain("NEW_VALUE");
    expect(await readTextFile(h.path("directory", "gamma.ts"))).toContain("NEW_VALUE");
  });

  test("transaction rolls back on failure", async () => {
    const h = await harness();
    const fileA = h.path("sample.ts");
    const original = await readTextFile(fileA);

    const response = await h.bridge.send("transaction", {
      operations: [
        { file: fileA, command: "edit_match", match: "funcA", replacement: "rolledChange" },
        {
          file: h.path("with-errors.ts"),
          command: "edit_match",
          match: "missing-pattern",
          replacement: "x",
        },
      ],
    });

    expect(response.success).toBe(false);
    expect(response.code).toBe("transaction_failed");
    expect(await readTextFile(fileA)).toBe(original);
  });

  test("batch line range edits work through the binary", async () => {
    const h = await harness();
    const filePath = h.path("lines.txt");
    await writeFile(filePath, "line 1\nline 2\nline 3\nline 4\n");

    const response = await h.bridge.send("batch", {
      file: filePath,
      edits: [{ line_start: 2, line_end: 3, content: "middle\n" }],
    });

    expect(response.success).toBe(true);
    expect(await readTextFile(filePath)).toBe("line 1\nmiddle\nline 4\n");
  });
});
