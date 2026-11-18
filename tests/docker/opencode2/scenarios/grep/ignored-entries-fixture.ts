import { mkdir, rm, writeFile } from "node:fs/promises";
import { join } from "node:path";
import {
  expectedIgnoredEntriesDisclosure,
  IGNORED_ENTRIES_SURFACE_ID,
} from "../../harness/ignored-entries-t6.js";
import type { HarnessExtension, ScenarioLifecycleContext } from "../../harness/types.js";

const MARKER = "ignoredScope ScenarioMarker";
const IGNORED_NAMES = ["ignored-a", "ignored-b", "ignored-c", "ignored-d", "ignored-e", "ignored-f"];

export function ignoredEntriesHooks(): Pick<HarnessExtension, "beforeScenario" | "afterScenario"> {
  const ownsScenario = (context: ScenarioLifecycleContext) =>
    context.scenario.tool === "grep" &&
    (context.scenario.metadata?.t6 as Record<string, unknown> | undefined)?.surface_id === IGNORED_ENTRIES_SURFACE_ID;

  return {
    async beforeScenario(context) {
      if (!ownsScenario(context)) return;
      const incomplete = context.scenario.id.endsWith("/incomplete");
      const directory = join(context.project_root, "ignore-boundaries");
      await mkdir(directory, { recursive: true });
      await writeFile(join(directory, ".aftignore"), "ignored-*/\n");
      await writeFile(join(directory, "visible.txt"), `${MARKER}\n`);
      for (const name of incomplete ? IGNORED_NAMES : ["ignored-a"]) {
        await mkdir(join(directory, name), { recursive: true });
        await writeFile(join(directory, name, "not-searched.txt"), `${MARKER}\n`);
      }
      // More content below one boundary must not increase the boundary count.
      await mkdir(join(directory, "ignored-a", "nested"), { recursive: true });
      await writeFile(join(directory, "ignored-a", "nested", "extra-not-searched.txt"), `${MARKER}\n`);

      context.scenario.comparison = {
        mode: "shape",
        rules: [{
          kind: "field",
          field: "ignored_entries",
          pattern: "^(?![\\s\\S]*not-searched\\.txt)(?=[\\s\\S]*ignoredScope ScenarioMarker)(?=[\\s\\S]*Found 1 match in 1 file)(?=[\\s\\S]*(?<value>Not searched \\(ignore rules\\): [^\\r\\n]+)(?:\\n|$))[\\s\\S]+$",
          min_lines: 1,
          max_lines: 10000,
        }],
        expected: { ignored_entries: expectedIgnoredEntriesDisclosure(incomplete ? "incomplete" : "complete") },
      };
    },
    async afterScenario(context) {
      if (ownsScenario(context)) await rm(join(context.project_root, "ignore-boundaries"), { recursive: true, force: true });
    },
  };
}
