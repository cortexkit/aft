import { describe, expect, test } from "bun:test";
import { resolve } from "node:path";

import { loadScenarios } from "../../harness/scenario-loader.js";
import extension from "./powershell.extension.js";

describe("powershell OpenCode 2 Linux applicability", () => {
  test("loads no scripted calls and validates the platform row", async () => {
    const scenarios = await loadScenarios(resolve(import.meta.dir));
    expect(scenarios).toEqual([]);
    await extension.validate?.({
      repo_root: resolve(import.meta.dir, "../../../../../.."),
      platform: "linux",
      scenarios,
      matrix: {},
      pinned_host_version: "0.0.0-beta-19234",
    });
  });
});
