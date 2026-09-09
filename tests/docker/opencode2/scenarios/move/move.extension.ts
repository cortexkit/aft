import { readFile, writeFile } from "node:fs/promises";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import type { HarnessExtension, HarnessValidationContext, ScenarioLifecycleContext } from "../../harness/types.js";
const here = dirname(fileURLToPath(import.meta.url));
const expectedIds = ["move/T1/happy","move/T2/invalid_arguments","move/T2/missing_target","move/T3/move_ask_allow","move/T3/move_ask_deny","move/T3/move_config_deny","move/T7/happy"];
async function validate(context: HarnessValidationContext): Promise<void> {
  const scenarios = context.scenarios.filter((scenario) => scenario.tool === "move");
  const actual = scenarios.map((scenario) => scenario.id).sort();
  if (JSON.stringify(actual) !== JSON.stringify(expectedIds)) throw new Error("move" + " scenario identity mismatch: " + actual.join(","));
  for (const scenario of scenarios) for (const turn of scenario.turns) if (turn.response.kind === "tool_calls") for (const call of turn.response.calls) {
    if (call.disk_effects !== undefined && call.non_mutating_evidence !== undefined) throw new Error(scenario.id + ":" + call.id + " has two disk classifications");
    if (call.disk_effects === undefined && call.non_mutating_evidence === undefined) throw new Error(scenario.id + ":" + call.id + " lacks a disk classification");
  }
  const matrix = JSON.parse(await readFile(join(here, "matrix.json"), "utf8"));
  if (JSON.stringify(matrix.rows?.[0]?.trajectories) !== JSON.stringify({"T1":"applicable","T2":"applicable","T3":"applicable","T4":"n/a:no-abortable-operation","T5":"n/a:no-background-capability","T6":"n/a:no-list-surface","T7":"applicable"})) throw new Error("move" + " applicability mismatch");
  const controls = JSON.parse(await readFile(join(here, "mutation-controls.json"), "utf8"));
  if (!Array.isArray(controls.controls) || controls.controls.length < 3) throw new Error("move" + " mutation controls missing");
}
async function beforeScenario(context: ScenarioLifecycleContext): Promise<void> {
  if (!context.scenario.id.endsWith("_config_deny")) return;
  const path = join(dirname(context.project_root), "xdg-config", "opencode", "opencode.json");
  const config = JSON.parse(await readFile(path, "utf8"));
  config.permission = { edit: "deny" };
  await writeFile(path, JSON.stringify(config, null, 2) + "\n");
}
const extension: HarnessExtension = { name: "move-scenarios-v1", validate, beforeScenario };
export default extension;
