import { describe, expect, test } from "bun:test";
import { mkdir, mkdtemp, open, readdir, rm, stat } from "node:fs/promises";
import { join, resolve } from "node:path";
import { assertComparison, assertT6Trailer } from "../../harness/projection.js";
import { loadScenarios, materializeParityScenarios } from "../../harness/scenario-loader.js";
import type { ScenarioLifecycleContext, ScenarioResult } from "../../harness/types.js";
import grepExtension from "../grep/grep.extension.js";
import extension from "./search.extension.js";
describe("search OpenCode 2 scenarios", () => {
  test("load through the harness loader and satisfy the slice validator", async () => {
    const scenarios = materializeParityScenarios(await loadScenarios(resolve(import.meta.dir)));
    expect(scenarios.length).toBe(8);
    await extension.validate?.({ repo_root: resolve(import.meta.dir, "../../../../../.."), platform: "linux", scenarios, matrix: {}, pinned_host_version: "2.0.3" });
  });

  for (const owner of ["search", "grep"] as const) {
    for (const fixture of ["complete", "incomplete"] as const) {
      test(`${owner} ${fixture} read-ceiling fixture pins disclosure and cleans up`, async () => {
        const scenarios = await loadScenarios(resolve(import.meta.dir, owner === "search" ? "." : "../grep"));
        const scenario = scenarios.find((row) => row.id === `${owner}/T6/${owner}-payload-skipped-files/${fixture}`);
        if (!scenario) throw new Error(`${owner} ${fixture} skipped-files fixture missing`);
        const scratch = resolve(import.meta.dir, "../../../../..", "target/scratch");
        await mkdir(scratch, { recursive: true });
        const root = await mkdtemp(join(scratch, "read-ceiling-"));
        const context: ScenarioLifecycleContext = {
          scenario,
          project_root: root,
          run_root: root,
          forensic_dir: root,
          host_generation: "v2",
        };
        const directory = join(root, "read-ceiling");
        const result: ScenarioResult = { id: scenario.id, status: "passed", forensic_dir: root };
        try {
          for (const hooks of [extension, grepExtension]) await hooks.beforeScenario?.(context);
          const names = (await readdir(directory)).sort();
          expect(names.length).toBe(fixture === "incomplete" ? 22 : 1);
          expect(names).toContain("small.txt");
          const turn = scenario.turns[0].response;
          if (turn.kind !== "tool_calls") throw new Error("read-ceiling fixture must call its tool");
          expect(turn.calls[0].arguments.pattern).toBe("readCeiling\\s+ScenarioMarker");
          expect(turn.calls[0].arguments.path).toBe("read-ceiling");
          if (!scenario.comparison) throw new Error("read-ceiling comparison missing");
          const marker = "readCeiling ScenarioMarker";
          let text = `small.txt:\n  1: ${marker}`;
          if (fixture === "incomplete") {
            for (const name of names.filter((name) => name.startsWith("large-"))) {
              const info = await stat(join(directory, name));
              expect(info.size).toBe(16 * 1024 * 1024 + 1);
              if (process.platform !== "win32") expect(info.blocks * 512).toBeLessThan(info.size);
            }
            const file = await open(join(directory, "large-00.txt"), "r");
            try {
              const prefix = Buffer.alloc(1024);
              await file.read(prefix, 0, prefix.length, 0);
              expect(prefix.includes(0)).toBe(false);
              expect(new RegExp(turn.calls[0].arguments.pattern as string).test(prefix.toString())).toBe(true);
              const tail = Buffer.alloc(1, 1);
              await file.read(tail, 0, 1, 16 * 1024 * 1024);
              expect(tail[0]).toBe(0);
            } finally {
              await file.close();
            }
            const disclosed = Array.from({ length: 20 }, (_, index) =>
              join(directory, `large-${String(index).padStart(2, "0")}.txt`),
            );
            text += `\n\nSkipped: reading this file would exceed the 16 MiB in-memory read ceiling.\n${disclosed.join("\n")}\n\nshown 20 of 21 files (cap) · narrow: path`;
            expect(() => assertComparison(text.replace("16 MiB", "32 MiB"), scenario.comparison!)).toThrow();
            expect(() => assertComparison(text.replace("large-00.txt", "other.txt"), scenario.comparison!)).toThrow();
            expect(() => assertComparison(text.replace(marker, "missing"), scenario.comparison!)).toThrow();
            expect(() => assertT6Trailer(scenario, turn.calls[0], text.replace("of 21", "of 22"))).toThrow();
          } else {
            expect(() => assertComparison(`${text}\nSkipped: unexpected omission`, scenario.comparison!)).toThrow();
          }
          expect(() => assertComparison(text, scenario.comparison!)).not.toThrow();
          expect(() => assertT6Trailer(scenario, turn.calls[0], text)).not.toThrow();
        } finally {
          try {
            for (const hooks of [extension, grepExtension]) await hooks.afterScenario?.(context, result);
            expect(await stat(directory).catch(() => null)).toBeNull();
          } finally {
            await rm(root, { recursive: true, force: true });
          }
        }
      });
    }
  }
});
