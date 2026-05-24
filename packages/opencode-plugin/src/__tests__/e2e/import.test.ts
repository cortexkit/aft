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

maybeDescribe("e2e import commands", () => {
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

  test("adds an import", async () => {
    const h = await harness();
    const filePath = h.path("imports.ts");

    const response = await h.bridge.send("add_import", {
      file: filePath,
      module: "lodash",
      names: ["debounce"],
    });

    expect(response.success).toBe(true);
    expect(response.added).toBe(true);
    expect(await readTextFile(filePath)).toContain("import { debounce } from 'lodash';");
  });

  test("removes an import", async () => {
    const h = await harness();
    const filePath = h.path("imports.ts");

    const response = await h.bridge.send("remove_import", {
      file: filePath,
      module: "zod",
    });

    expect(response.success).toBe(true);
    expect(await readTextFile(filePath)).not.toContain('from "zod"');
  });

  test("organizes imports", async () => {
    const h = await harness();
    const filePath = h.path("imports.ts");

    await h.bridge.send("add_import", {
      file: filePath,
      module: "axios",
      default_import: "axios",
    });
    const response = await h.bridge.send("organize_imports", { file: filePath });

    expect(response.success).toBe(true);
    const content = await readTextFile(filePath);
    const axiosIndex = content.indexOf("import axios from 'axios';");
    const parseIndex = content.indexOf("import { parse } from 'jsonc-parser';");
    expect(axiosIndex).toBeGreaterThanOrEqual(0);
    expect(parseIndex).toBeGreaterThanOrEqual(0);
    expect(axiosIndex).toBeLessThan(parseIndex);
  });
});
