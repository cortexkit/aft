/// <reference path="../bun-test.d.ts" />

import { describe, expect, test } from "bun:test";
import { readFileSync } from "node:fs";
import { resolve } from "node:path";

describe("OpenCode harness configure override", () => {
  test("plugin seeds every bridge configure payload with opencode harness", () => {
    const source = readFileSync(resolve(import.meta.dir, "../index.ts"), "utf-8");

    expect(source).toContain('pool.setConfigureOverride("harness", "opencode")');
    expect(source.indexOf("new BridgePool(")).toBeLessThan(
      source.indexOf('pool.setConfigureOverride("harness", "opencode")'),
    );
  });
});
