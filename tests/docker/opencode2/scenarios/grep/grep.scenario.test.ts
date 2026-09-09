import { describe, expect, test } from "bun:test";
import { resolve } from "node:path";
import { loadScenarios, materializeParityScenarios } from "../../harness/scenario-loader.js";
import extension from "./grep.extension.js";
describe("grep OpenCode 2 scenarios", () => {
  test("load through the harness loader and satisfy the slice validator", async () => {
    const scenarios = materializeParityScenarios(await loadScenarios(resolve(import.meta.dir)));
    expect(scenarios.length).toBe(6);
    await extension.validate?.({ repo_root: resolve(import.meta.dir, "../../../../../.."), platform: "linux", scenarios, matrix: {}, pinned_host_version: "0.0.0-beta-19234" });
  });
});
