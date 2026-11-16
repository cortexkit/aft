import { mkdir, open, rm, writeFile } from "node:fs/promises";
import { join } from "node:path";
import type { HarnessExtension, ScenarioLifecycleContext } from "../harness/types.js";

const MARKER = "readCeiling ScenarioMarker";
const SKIP_REASON = "Skipped: reading this file would exceed the 16 MiB in-memory read ceiling.";
const TRAILER = "shown 20 of 21 files (cap) · narrow: path";

export function readCeilingHooks(owner: "search" | "grep"): Pick<HarnessExtension, "beforeScenario" | "afterScenario"> {
  const ownsScenario = (context: ScenarioLifecycleContext) =>
    context.scenario.tool === owner &&
    context.scenario.metadata?.t6 != null &&
    (context.scenario.metadata.t6 as Record<string, unknown>).surface_id === `${owner}..payload.skipped_files`;

  return {
    async beforeScenario(context) {
      if (!ownsScenario(context)) return;
      const directory = join(context.project_root, "read-ceiling");
      await mkdir(directory, { recursive: true });
      await writeFile(join(directory, "small.txt"), `${MARKER}\n`);
      const incomplete = context.scenario.id.endsWith("/incomplete");
      if (incomplete) {
        // Grep sniffs the first 1,024 bytes for binary data before checking the
        // whole-read ceiling. Keep that prefix textual. Both registrations use
        // readCeiling\s+ScenarioMarker: \s can cross a newline, forcing a whole
        // read that rejects the logical size before the sparse, NUL-filled tail.
        const prefix = Buffer.alloc(1024, " ");
        prefix.write(`${MARKER}\n`);
        for (let index = 0; index < 21; index += 1) {
          const file = await open(join(directory, `large-${String(index).padStart(2, "0")}.txt`), "w");
          try {
            await file.writeFile(prefix);
            await file.truncate(16 * 1024 * 1024 + 1);
          } finally {
            await file.close();
          }
        }
      }

      // Paths belong to the isolated project, not the checkout. Pin every
      // disclosed name using that known root instead of weakening the assertion
      // to accept any path returned by the product.
      const skippedNames = Array.from({ length: 20 }, (_, index) =>
        join(directory, `large-${String(index).padStart(2, "0")}.txt`),
      );
      context.scenario.comparison = incomplete
        ? {
            mode: "shape",
            rules: [{
              kind: "field",
              field: "skipped_files",
              pattern: "^(?=[\\s\\S]*readCeiling ScenarioMarker)(?=[\\s\\S]*(?<value>Skipped: reading this file would exceed the 16 MiB in-memory read ceiling\\.[\\s\\S]*?shown 20 of 21 files \\(cap\\) · narrow: path)(?:\\n|$))[\\s\\S]+$",
              min_lines: 1,
              max_lines: 10000,
            }],
            expected: { skipped_files: `${SKIP_REASON}\n${skippedNames.join("\n")}\n\n${TRAILER}` },
          }
        : {
            mode: "shape",
            rules: [{
              kind: "field",
              field: "fixture_marker",
              pattern: "^(?![\\s\\S]*Skipped:)(?=[\\s\\S]*(?<value>readCeiling ScenarioMarker))[\\s\\S]+$",
              min_lines: 1,
              max_lines: 10000,
            }],
            expected: { fixture_marker: MARKER },
          };
    },
    async afterScenario(context) {
      if (ownsScenario(context)) await rm(join(context.project_root, "read-ceiling"), { recursive: true, force: true });
    },
  };
}
