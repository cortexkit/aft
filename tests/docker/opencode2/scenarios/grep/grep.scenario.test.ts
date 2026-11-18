import { describe, expect, test } from "bun:test";
import { mkdir, mkdtemp, readFile, readdir, rm } from "node:fs/promises";
import { join, resolve } from "node:path";
import { expectedIgnoredEntriesDisclosure } from "../../harness/ignored-entries-t6.js";
import { assertComparison, assertT6Trailer } from "../../harness/projection.js";
import { loadScenarios, materializeParityScenarios } from "../../harness/scenario-loader.js";
import type { ScenarioLifecycleContext } from "../../harness/types.js";
import extension from "./grep.extension.js";
describe("grep OpenCode 2 scenarios", () => {
  test("load through the harness loader and satisfy the slice validator", async () => {
    const scenarios = materializeParityScenarios(await loadScenarios(resolve(import.meta.dir)));
    expect(scenarios.length).toBe(10);
    await extension.validate?.({ repo_root: resolve(import.meta.dir, "../../../../../.."), platform: "linux", scenarios, matrix: {}, pinned_host_version: "2.0.3" });
  });
  for (const kind of ["complete", "incomplete"] as const) {
    test(`ignored_entries ${kind} fixture pins ignored boundaries rather than matching content`, async () => {
      const scenarios = materializeParityScenarios(await loadScenarios(resolve(import.meta.dir)));
      const scenario = scenarios.find((candidate) => candidate.id === `grep/T6/grep-payload-ignored-entries/${kind}`);
      if (!scenario) throw new Error(`ignored_entries ${kind} fixture missing`);
      const scratch = resolve(import.meta.dir, "../../../../../target/scratch");
      await mkdir(scratch, { recursive: true });
      const project = await mkdtemp(join(scratch, "grep-ignored-entries-"));
      const context: ScenarioLifecycleContext = {
        scenario, project_root: project, run_root: project, forensic_dir: project, host_generation: "v2",
      };
      try {
        expect(scenario.env).toEqual({ AFT_TEST_GREP_IGNORE_PROBE: "ready" });
        await extension.beforeScenario?.(context);
        const directory = join(project, "ignore-boundaries");
        expect(await readFile(join(directory, ".aftignore"), "utf8")).toBe("ignored-*/\n");
        expect((await readdir(directory)).filter((name) => name.startsWith("ignored-")).sort()).toEqual(
          kind === "complete" ? ["ignored-a"] : ["ignored-a", "ignored-b", "ignored-c", "ignored-d", "ignored-e", "ignored-f"],
        );
        expect(await readFile(join(directory, "ignored-a/nested/extra-not-searched.txt"), "utf8")).toBe("ignoredScope ScenarioMarker\n");
        const response = scenario.turns[0].response;
        if (response.kind !== "tool_calls" || !scenario.comparison) throw new Error("ignored_entries fixture lacks its call or comparison");
        const text = `ignore-boundaries/visible.txt\n1: ignoredScope ScenarioMarker\n\nFound 1 match in 1 file\n\n${expectedIgnoredEntriesDisclosure(kind)}`;
        expect(() => assertT6Trailer(scenario, response.calls[0], text)).not.toThrow();
        expect(() => assertComparison(text, scenario.comparison!)).not.toThrow();
        expect(() => assertComparison(text.replace("Found 1 match in 1 file", "Found 2 matches across 2 files"), scenario.comparison!)).toThrow();
        expect(() => assertComparison(text.replace("visible.txt", "ignored-a/not-searched.txt"), scenario.comparison!)).toThrow();
      } finally {
        await extension.afterScenario?.(context, { id: scenario.id, status: "passed", forensic_dir: project });
        await rm(project, { recursive: true, force: true });
      }
    });
  }
});
