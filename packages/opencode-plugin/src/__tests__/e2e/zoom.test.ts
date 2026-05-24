/// <reference path="../../bun-test.d.ts" />

import { afterEach, beforeAll, describe, expect, test } from "bun:test";
import {
  cleanupHarnesses,
  createHarness,
  type E2EHarness,
  type PreparedBinary,
  prepareBinary,
  sendZoomMultiSymbolLikePlugin,
} from "./helpers.js";

const initialBinary = await prepareBinary();
const maybeDescribe = describe.skipIf(!initialBinary.binaryPath);

maybeDescribe("e2e zoom command", () => {
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

  test("zooms a symbol with annotations", async () => {
    const h = await harness();

    const response = await h.bridge.send("zoom", { file: h.path("sample.ts"), symbol: "funcB" });

    expect(response.success).toBe(true);
    expect(response.name).toBe("funcB");
    expect(String(response.content)).toContain("export function funcB");
    expect(Array.isArray((response.annotations as Record<string, unknown>).calls_out)).toBe(true);
  });

  test("zooms multiple symbols like the plugin helper", async () => {
    const h = await harness();

    const responses = await sendZoomMultiSymbolLikePlugin(h.bridge, h.path("sample.ts"), [
      "funcA",
      "SampleService",
    ]);

    expect(responses).toHaveLength(2);
    expect(responses[0]?.name).toBe("funcA");
    expect(responses[1]?.name).toBe("SampleService");
  });

  test("follows a re-export through the barrel file", async () => {
    const h = await harness();

    const response = await h.bridge.send("zoom", { file: h.path("barrel.ts"), symbol: "funcA" });

    expect(response.success).toBe(true);
    expect(String(response.content)).toContain("export function funcA");
    expect(response.name).toBe("funcA");
  });

  test("returns symbol_not_found for unknown symbols", async () => {
    const h = await harness();

    const response = await h.bridge.send("zoom", {
      file: h.path("sample.ts"),
      symbol: "missingSymbol",
    });

    expect(response.success).toBe(false);
    expect(response.code).toBe("symbol_not_found");
  });

  test("supports line-range zooming", async () => {
    const h = await harness();

    const response = await h.bridge.send("zoom", {
      file: h.path("sample.md"),
      start_line: 1,
      end_line: 3,
    });

    expect(response.success).toBe(true);
    expect(response.kind).toBe("lines");
    expect(String(response.content)).toContain("# Project Title");
  });
});
