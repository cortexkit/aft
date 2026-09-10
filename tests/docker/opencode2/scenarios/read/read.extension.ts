import { readFile, writeFile } from "node:fs/promises";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

import type {
  HarnessExtension,
  HarnessValidationContext,
  ScenarioLifecycleContext,
} from "../../harness/types.js";

const here = dirname(fileURLToPath(import.meta.url));
const EXPECTED_IDS = new Set([
  "read/T1/happy",
  "read/T2/invalid_arguments",
  "read/T2/missing_target",
  "read/T3/read_ask_allow",
  "read/T3/read_ask_deny",
  "read/T3/read_config_deny",
  "read/T7/happy",
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
    T3: "expected_fail:https://github.com/anomalyco/opencode/issues/37164",
    T4: "n/a:no-abortable-operation",
    T5: "n/a:no-background-capability",
    T6: "n/a:no-list-surface",
    T7: "applicable",
  };
  if (!row || JSON.stringify(row.trajectories) !== JSON.stringify(expected)) {
    throw new Error("read applicability row does not match the declared T1-T7 contract");
  }

  const controls = JSON.parse(await readFile(join(here, "mutation-controls.json"), "utf8")) as {
    controls?: Array<{ id?: string }>;
  };
  const controlIds = new Set((controls.controls ?? []).map((control) => control.id));
  for (const id of [
    "read-permission-path-identity",
    "read-permission-denial-is-non-mutating",
    "read-parity-exact",
  ]) {
    if (!controlIds.has(id)) throw new Error(`read mutation control is missing ${id}`);
  }
}

async function configureReadDenial(context: ScenarioLifecycleContext): Promise<void> {
  if (context.scenario.id !== "read/T3/read_config_deny") return;
  const hostConfig = join(dirname(context.project_root), "xdg-config", "opencode", "opencode.json");
  const config = JSON.parse(await readFile(hostConfig, "utf8")) as Record<string, unknown>;
  config.permission = { read: "deny" };
  await writeFile(hostConfig, `${JSON.stringify(config, null, 2)}\n`);
}

const extension: HarnessExtension = {
  name: "read-scenarios-v1",
  validate: validateReadScenarios,
  beforeScenario: configureReadDenial,
};

export default extension;
