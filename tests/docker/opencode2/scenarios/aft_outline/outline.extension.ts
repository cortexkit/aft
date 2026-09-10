import { readFile, writeFile } from "node:fs/promises";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import type { HarnessExtension, HarnessValidationContext, ScenarioLifecycleContext } from "../../harness/types.js";
const here = dirname(fileURLToPath(import.meta.url));
const expectedIds = ["outline/T1/happy","outline/T2/invalid_arguments","outline/T2/missing_target","outline/T6/outline-files-payload-files/complete","outline/T6/outline-files-payload-files/incomplete","outline/T7/happy"];
async function validate(context: HarnessValidationContext): Promise<void> {
  const scenarios = context.scenarios.filter((scenario) => scenario.tool === "outline");
  const actual = scenarios.map((scenario) => scenario.id).sort();
  if (JSON.stringify(actual) !== JSON.stringify(expectedIds)) throw new Error("outline" + " scenario identity mismatch: " + actual.join(","));
  for (const scenario of scenarios) for (const turn of scenario.turns) if (turn.response.kind === "tool_calls") for (const call of turn.response.calls) {
    if (call.disk_effects !== undefined && call.non_mutating_evidence !== undefined) throw new Error(scenario.id + ":" + call.id + " has two disk classifications");
    if (call.disk_effects === undefined && call.non_mutating_evidence === undefined) throw new Error(scenario.id + ":" + call.id + " lacks a disk classification");
  }
  const matrix = JSON.parse(await readFile(join(here, "matrix.json"), "utf8"));
  if (JSON.stringify(matrix.rows?.[0]?.trajectories) !== JSON.stringify({"T1":"applicable","T2":"applicable","T3":"n/a:no-permission-operation","T4":"n/a:no-abortable-operation","T5":"n/a:no-background-capability","T6":"applicable","T7":"expected_fail:https://github.com/anomalyco/opencode/issues/48340"})) throw new Error("outline" + " applicability mismatch");
  const controls = JSON.parse(await readFile(join(here, "mutation-controls.json"), "utf8"));
  if (!Array.isArray(controls.controls) || controls.controls.length < 3) throw new Error("outline" + " mutation controls missing");
}
async function beforeScenario(context: ScenarioLifecycleContext): Promise<void> {
  if (!context.scenario.id.endsWith("_config_deny")) return;
  const path = join(dirname(context.project_root), "xdg-config", "opencode", "opencode.json");
  const config = JSON.parse(await readFile(path, "utf8"));
  config.permission = { edit: "deny" };
  await writeFile(path, JSON.stringify(config, null, 2) + "\n");
}
const extension: HarnessExtension = { name: "outline-scenarios-v1", validate, beforeScenario };
export default extension;
