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

maybeDescribe("e2e write command", () => {
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

  test("writes a new file", async () => {
    const h = await harness();
    const filePath = h.path("created.ts");
    const content = 'export const created = "hello";\n';

    const response = await h.bridge.send("write", { file: filePath, content });

    expect(response.success).toBe(true);
    expect(response.created).toBe(true);
    expect(await readTextFile(filePath)).toBe(content);
  });

  test("overwrites an existing file", async () => {
    const h = await harness();
    const filePath = h.path("overwrite.ts");
    await writeFile(filePath, 'export const before = "old";\n');

    const response = await h.bridge.send("write", {
      file: filePath,
      content: 'export const after = "new";\n',
    });

    expect(response.success).toBe(true);
    expect(response.created).toBe(false);
    expect(response.backup_id).toBeDefined();
    expect(await readTextFile(filePath)).toBe('export const after = "new";\n');
  });

  test("creates parent directories when requested", async () => {
    const h = await harness();
    const filePath = h.path("nested", "deep", "created.ts");

    const response = await h.bridge.send("write", {
      file: filePath,
      content: "export const nested = true;\n",
      create_dirs: true,
    });

    expect(response.success).toBe(true);
    expect(await readTextFile(filePath)).toBe("export const nested = true;\n");
  });

  test("records undo history after overwriting", async () => {
    const h = await harness();
    const filePath = h.path("history.ts");
    await writeFile(filePath, "export const version = 1;\n");

    const writeResponse = await h.bridge.send("write", {
      file: filePath,
      content: "export const version = 2;\n",
    });
    const historyResponse = await h.bridge.send("edit_history", { file: filePath });

    expect(writeResponse.success).toBe(true);
    expect(historyResponse.success).toBe(true);
    expect(Array.isArray(historyResponse.entries)).toBe(true);
    expect((historyResponse.entries as Array<Record<string, unknown>>).length).toBeGreaterThan(0);
    expect(
      String((historyResponse.entries as Array<Record<string, unknown>>)[0]?.description),
    ).toContain("write");
  });
});
