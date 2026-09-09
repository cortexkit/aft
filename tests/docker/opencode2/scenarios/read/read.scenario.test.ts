import { describe, expect, test } from "bun:test";
import { resolve } from "node:path";

import { loadScenarios, materializeParityScenarios } from "../../harness/scenario-loader.js";
import extension from "./read.extension.js";

describe("read OpenCode 2 scenarios", () => {
  test("load through the harness loader and satisfy the slice validator", async () => {
    const root = resolve(import.meta.dir);
    const scenarios = materializeParityScenarios(await loadScenarios(root));
    expect(scenarios.map((scenario) => scenario.id)).toEqual([
      "read/T1/happy",
      "read/T2/invalid_arguments",
      "read/T2/missing_target",
      "read/T3/read_ask_allow",
      "read/T3/read_ask_deny",
      "read/T3/read_config_deny",
      "read/T7/happy",
    ]);
    await extension.validate?.({
      repo_root: resolve(import.meta.dir, "../../../../../.."),
      platform: "linux",
      scenarios,
      matrix: {},
      pinned_host_version: "0.0.0-beta-19234",
    });
  });
});
