/// <reference path="../bun-test.d.ts" />

import { describe, expect, test } from "bun:test";
import { __test__ } from "../index.js";

describe("Pi tool surface", () => {
  test("bash hoisting is independent of read hoisting", () => {
    const surface = __test__.resolveToolSurface({
      disabled_tools: ["read"],
    });

    expect(surface.hoistRead).toBe(false);
    expect(surface.hoistBash).toBe(true);
  });
});
