/// <reference path="../../bun-test.d.ts" />

import { afterEach, beforeAll, describe, expect, test } from "bun:test";
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

maybeDescribe("e2e ast commands", () => {
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

  test("ast_search finds a pattern with locations", async () => {
    const h = await harness();

    const response = await h.bridge.send("ast_search", {
      pattern: "console.log($MSG)",
      lang: "typescript",
      paths: [h.path("multi-match.ts")],
    });

    expect(response.success).toBe(true);
    expect(response.total_matches).toBe(5);
    expect(response.files_with_matches).toBe(1);
  });

  test("ast_search captures meta-variables", async () => {
    const h = await harness();

    const response = await h.bridge.send("ast_search", {
      pattern: "export const $NAME = $VALUE",
      lang: "typescript",
      paths: [h.path("sample.ts")],
    });

    expect(response.success).toBe(true);
    const matches = response.matches as Array<Record<string, unknown>>;
    expect(matches.length).toBeGreaterThan(0);
    expect((matches[0]?.meta_variables as Record<string, unknown>)?.$NAME).toBe("DEFAULT_SUFFIX");
    expect((matches[0]?.meta_variables as Record<string, unknown>)?.$VALUE).toBe('"!"');
  });

  test("ast_search reports clean empty results", async () => {
    const h = await harness();

    const response = await h.bridge.send("ast_search", {
      pattern: "console.log($MSG)",
      lang: "typescript",
      paths: [h.path("sample.ts")],
    });

    expect(response.success).toBe(true);
    expect(response.total_matches).toBe(0);
  });

  test("ast_search rejects invalid patterns without crashing", async () => {
    const h = await harness();

    const response = await h.bridge.send("ast_replace", {
      pattern: "catch ($ERR) { $$$ }",
      rewrite: "noop()",
      lang: "typescript",
      dry_run: true,
    });

    expect(response.success).toBe(false);
    expect(response.code).toBe("invalid_pattern");
  });

  test("ast_replace updates a single file", async () => {
    const h = await harness();
    const filePath = h.path("multi-match.ts");

    const response = await h.bridge.send("ast_replace", {
      pattern: "console.log($MSG)",
      rewrite: "logger.info($MSG)",
      lang: "typescript",
      paths: [filePath],
      dry_run: false,
    });

    expect(response.success).toBe(true);
    expect(response.total_replacements).toBe(5);
    const content = await readTextFile(filePath);
    expect(content.match(/logger\.info/g)?.length).toBe(5);
  });

  test("ast_replace dry run leaves files unchanged", async () => {
    const h = await harness();
    const filePath = h.path("multi-match.ts");
    const original = await readTextFile(filePath);

    const response = await h.bridge.send("ast_replace", {
      pattern: "console.log($MSG)",
      rewrite: "logger.debug($MSG)",
      lang: "typescript",
      paths: [filePath],
      dry_run: true,
    });

    expect(response.success).toBe(true);
    expect(response.dry_run).toBe(true);
    expect(await readTextFile(filePath)).toBe(original);
  });

  test("ast_replace preserves meta-variables in rewrite", async () => {
    const h = await harness();
    const filePath = h.path("multi-match.ts");

    const response = await h.bridge.send("ast_replace", {
      pattern: "console.log($MSG)",
      rewrite: "report($MSG)",
      lang: "typescript",
      paths: [filePath],
      dry_run: false,
    });

    expect(response.success).toBe(true);
    const content = await readTextFile(filePath);
    expect(content).toContain('report("alpha")');
    expect(content).toContain('report("epsilon")');
  });

  test("ast_search works for python fixtures too", async () => {
    const h = await harness();

    const response = await h.bridge.send("ast_search", {
      pattern: "return $VALUE",
      lang: "python",
      paths: [h.path("sample.py")],
    });

    expect(response.success).toBe(true);
    expect(Number(response.total_matches)).toBeGreaterThanOrEqual(4);
  });
});
