/**
 * E2E coverage for aft_import (add/remove/organize).
 * Regression for wrong Rust command names (was sending "import"; Rust expects
 * add_import / remove_import / organize_imports).
 */

/// <reference path="../../bun-test.d.ts" />

import { afterAll, beforeAll, describe, expect, test } from "bun:test";
import { readFile } from "node:fs/promises";
import { createHarness, type Harness, prepareBinary, writeFixture } from "./helpers.js";

const initialBinary = await prepareBinary();
const maybeDescribe = initialBinary.binaryPath ? describe : describe.skip;

maybeDescribe("aft_import (real bridge)", () => {
  let harness: Harness;

  beforeAll(async () => {
    harness = await createHarness(initialBinary);
  });

  afterAll(async () => {
    if (harness) await harness.cleanup();
  });

  test("add/remove require module", async () => {
    await expect(
      harness.callTool("aft_import", { op: "add", filePath: "imports.ts" }),
    ).rejects.toThrow(/module/);
    await expect(
      harness.callTool("aft_import", { op: "remove", filePath: "imports.ts" }),
    ).rejects.toThrow(/module/);
  });

  test("add new named import", async () => {
    await writeFixture(
      harness,
      "add-target.ts",
      `import { existing } from "./existing";\n\nexport const x = existing;\n`,
    );
    await harness.callTool("aft_import", {
      op: "add",
      filePath: "add-target.ts",
      module: "./utils",
      names: ["helper"],
    });
    const after = await readFile(harness.path("add-target.ts"), "utf8");
    // Quote style may differ from existing imports (AFT doesn't infer style).
    expect(after).toMatch(/from ['"]\.\/utils['"]/);
    expect(after).toContain("helper");
  });

  test("remove entire import when removeName is omitted", async () => {
    await writeFixture(
      harness,
      "remove-all.ts",
      `import { a, b } from "./mod";\n\nexport const z = a + b;\n`,
    );
    await harness.callTool("aft_import", {
      op: "remove",
      filePath: "remove-all.ts",
      module: "./mod",
    });
    const after = await readFile(harness.path("remove-all.ts"), "utf8");
    expect(after).not.toContain('from "./mod"');
  });

  test("remove single named import via removeName", async () => {
    await writeFixture(
      harness,
      "remove-one.ts",
      `import { a, b } from "./mod";\n\nexport const z = a;\n`,
    );
    await harness.callTool("aft_import", {
      op: "remove",
      filePath: "remove-one.ts",
      module: "./mod",
      removeName: "b",
    });
    const after = await readFile(harness.path("remove-one.ts"), "utf8");
    expect(after).toMatch(/from ['"]\.\/mod['"]/);
    expect(after).toContain("a");
    expect(after).not.toMatch(/\bb\b/);
  });

  test("organize returns success for valid file", async () => {
    await writeFixture(
      harness,
      "organize-target.ts",
      `import { z } from "./z";\nimport { a } from "./a";\n\nexport const v = a + z;\n`,
    );
    const result = await harness.callTool("aft_import", {
      op: "organize",
      filePath: "organize-target.ts",
    });
    const text = harness.text(result);
    // Response structure includes groups/file keys — any of those proves dispatch worked
    expect(text).toMatch(/groups|file/i);
  });
});
