/// <reference path="../../bun-test.d.ts" />

import { afterEach, beforeAll, describe, expect, test } from "bun:test";
import { writeFile } from "node:fs/promises";
import {
  cleanupHarnesses,
  createHarness,
  type E2EHarness,
  lineNumberRangeText,
  lineNumberText,
  type PreparedBinary,
  prepareBinary,
  readTextFile,
  sendReadLikePlugin,
} from "./helpers.js";

const initialBinary = await prepareBinary();
const maybeDescribe = describe.skipIf(!initialBinary.binaryPath);

maybeDescribe("e2e read command", () => {
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

  test("reads a full file with line numbers", async () => {
    const h = await harness();
    const filePath = h.path("sample.ts");

    const response = await h.bridge.send("read", { file: filePath });

    expect(response.success).toBe(true);
    expect(response.content).toBe(lineNumberText(await readTextFile(filePath)));
  });

  test("reads a line range", async () => {
    const h = await harness();
    const filePath = h.path("sample.ts");
    const source = await readTextFile(filePath);

    const response = await h.bridge.send("read", {
      file: filePath,
      start_line: 4,
      end_line: 7,
    });

    expect(response.success).toBe(true);
    expect(response.content).toBe(lineNumberRangeText(source, 4, 7));
    expect(response.start_line).toBe(4);
    expect(response.end_line).toBe(7);
  });

  test("reads with offset and limit pagination semantics", async () => {
    const h = await harness();
    const filePath = h.path("sample.ts");
    const source = await readTextFile(filePath);

    const response = await sendReadLikePlugin(h.bridge, filePath, {
      offset: 2,
      limit: 3,
    });

    expect(response.success).toBe(true);
    expect(response.content).toBe(lineNumberRangeText(source, 2, 4));
    expect(response.start_line).toBe(2);
    expect(response.end_line).toBe(4);
  });

  test("reads a directory and returns sorted entries", async () => {
    const h = await harness();

    const response = await h.bridge.send("read", { file: h.path("directory") });

    expect(response.success).toBe(true);
    expect(response.entries).toEqual(["alpha.ts", "beta.ts", "gamma.ts"]);
  });

  test("detects binary files", async () => {
    const h = await harness();

    const response = await h.bridge.send("read", { file: h.path("binary.bin") });

    expect(response.success).toBe(true);
    expect(response.binary).toBe(true);
    expect(response.message).toBe("Binary file (8 bytes), cannot display as text");
  });

  test("returns an error for a missing file", async () => {
    const h = await harness();

    const response = await h.bridge.send("read", { file: h.path("missing.ts") });

    expect(response.success).toBe(false);
    expect(response.code).toBe("not_found");
  });

  test("truncates very large reads with a hint", async () => {
    const h = await harness();
    const filePath = h.path("large.txt");
    const largeContent = Array.from(
      { length: 2000 },
      (_, index) => `line-${index}-${"x".repeat(80)}`,
    ).join("\n");
    await writeFile(filePath, `${largeContent}\n`);

    const response = await h.bridge.send("read", { file: filePath });

    expect(response.success).toBe(true);
    expect(response.truncated).toBe(true);
    expect(String(response.content)).toContain("output truncated at 50KB");
  });
});
