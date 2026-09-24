/// <reference path="../bun-test.d.ts" />

import { describe, expect, test } from "bun:test";
import { readFileSync } from "node:fs";
import { resolve } from "node:path";

describe("OpenCode harness configure override", () => {
  // The flat configure overrides for both OpenCode entries live in the shared
  // bridge bootstrap, so the harness must be seeded there: in the payload the
  // pool is built with, and again on the pool once it exists.
  test("shared bridge bootstrap seeds every bridge configure payload with opencode harness", () => {
    const source = readFileSync(resolve(import.meta.dir, "../bridge-bootstrap.ts"), "utf-8");

    expect(source).toContain('harness: "opencode"');
    expect(source).toContain('pool.setConfigureOverride("harness", "opencode")');
  });

  test("both entries route through the shared bridge bootstrap", () => {
    for (const entry of ["../index.ts", "../entry/server-runtime.mjs"]) {
      const source = readFileSync(resolve(import.meta.dir, entry), "utf-8");
      expect(source).toContain("prepareBridgeEnvironment(");
      expect(source).toContain(".attach(pool)");
      expect(source).toContain("applyToolSurfaceOverrides(");
    }
  });
});
