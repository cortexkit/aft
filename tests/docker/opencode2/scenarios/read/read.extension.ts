import { mkdir, readFile, writeFile } from "node:fs/promises";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { assertRequestToolSurface } from "../../harness/mock-server.js";

import type { HarnessExtension, HarnessValidationContext, ScenarioLifecycleContext } from "../../harness/types.js";

const here = dirname(fileURLToPath(import.meta.url));
const EXPECTED_IDS = new Set([
  "read/T1/happy",
  "read/T1/edit-family/non-gpt",
  "read/T1/edit-family/gpt",
  "read/T2/invalid_arguments",
  "read/T2/missing_target",
  "read/T3/read_ask_allow",
  "read/T3/read_ask_deny",
  "read/T3/read_config_deny",
  "read/T6/read-directory-payload-entries/complete",
  "read/T6/read-directory-payload-entries/incomplete",
  "read/T7/happy",
  "read/T7/edit-family/non-gpt",
  "read/T7/edit-family/gpt",
]);

async function validateReadScenarios(context: HarnessValidationContext): Promise<void> {
  const scenarios = context.scenarios.filter((scenario) => scenario.tool === "read");
  const ids = new Set(scenarios.map((scenario) => scenario.id));
  for (const id of EXPECTED_IDS) {
    if (!ids.has(id)) throw new Error(`read scenario registration is missing ${id}`);
  }
  for (const scenario of scenarios) {
    if (!EXPECTED_IDS.has(scenario.id)) throw new Error(`unexpected read scenario ${scenario.id}`);
    for (const turn of scenario.turns) {
      if (turn.response.kind !== "tool_calls") continue;
      for (const call of turn.response.calls) {
        if (call.name !== "read") throw new Error(`${scenario.id} calls ${call.name}, not read`);
        if (!call.non_mutating_evidence || call.disk_effects !== undefined) {
          throw new Error(`${scenario.id}:${call.id} must carry read-only evidence`);
        }
      }
    }
  }

  const matrix = JSON.parse(await readFile(join(here, "matrix.json"), "utf8")) as {
    rows: Array<{ tool: string; trajectories: Record<string, string> }>;
  };
  const row = matrix.rows.find((candidate) => candidate.tool === "read");
  const expected = {
    T1: "applicable",
    T2: "applicable",
    T3: "applicable",
    T4: "n/a:no-abortable-operation",
    T5: "n/a:no-background-capability",
    T6: "applicable",
    T7: "applicable",
  };
  if (!row || JSON.stringify(row.trajectories) !== JSON.stringify(expected)) {
    throw new Error("read applicability row does not match the declared T1-T7 contract");
  }


}

async function beforeScenario(context: ScenarioLifecycleContext): Promise<void> {
  if (context.scenario.tool !== "read" || context.scenario.trajectory !== "T6") return;
  const directory = join(context.project_root, "entries");
  await mkdir(directory, { recursive: true });
  // Exceed read's 1000-entry display cap without checking in 1001 tiny files.
  const count = context.scenario.id === "read/T6/read-directory-payload-entries/incomplete" ? 1001 : 1;
  for (let index = 0; index < count; index++) {
    await writeFile(join(directory, `entry-${String(index).padStart(4, "0")}.txt`), "fixture\n");
  }
}

const extension: HarnessExtension = {
  name: "read-scenarios-v1",
  validate: validateReadScenarios,
  beforeScenario,
  observe: async (context, event) => {
    if (context.host_generation !== "v2" || event.kind !== "mock_exchange") return;
    const expected = context.scenario.metadata?.tool_surface as
      { model: string; present: string[]; absent: string[] } | undefined;
    if (!expected) return;
    const names = assertRequestToolSurface(event.exchange, expected);
    await writeFile(
      join(context.forensic_dir, `tool-surface-${event.exchange.label}.json`),
      `${JSON.stringify({ model: expected.model, tools: names }, null, 2)}\n`,
    );
  },
};

export default extension;
