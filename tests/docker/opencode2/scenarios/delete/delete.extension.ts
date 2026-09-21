import { readFile } from "node:fs/promises";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import type { HarnessExtension, HarnessValidationContext } from "../../harness/types.js";
const here = dirname(fileURLToPath(import.meta.url));
const expectedIds = ["delete/T1/happy","delete/T2/invalid_arguments","delete/T2/missing_target","delete/T3/delete_ask_allow","delete/T3/delete_ask_deny","delete/T3/delete_config_deny","delete/T7/happy"];
async function validate(context: HarnessValidationContext): Promise<void> {
  const scenarios = context.scenarios.filter((scenario) => scenario.tool === "delete");
  const actual = scenarios.map((scenario) => scenario.id).sort();
  if (JSON.stringify(actual) !== JSON.stringify(expectedIds)) throw new Error("delete" + " scenario identity mismatch: " + actual.join(","));
  for (const scenario of scenarios) for (const turn of scenario.turns) if (turn.response.kind === "tool_calls") for (const call of turn.response.calls) {
    if (call.disk_effects !== undefined && call.non_mutating_evidence !== undefined) throw new Error(scenario.id + ":" + call.id + " has two disk classifications");
    if (call.disk_effects === undefined && call.non_mutating_evidence === undefined) throw new Error(scenario.id + ":" + call.id + " lacks a disk classification");
  }
  const matrix = JSON.parse(await readFile(join(here, "matrix.json"), "utf8"));
  if (JSON.stringify(matrix.rows?.[0]?.trajectories) !== JSON.stringify({"T1":"applicable","T2":"applicable","T3":"applicable","T4":"n/a:no-abortable-operation","T5":"n/a:no-background-capability","T6":"n/a:no-list-surface","T7":"expected_fail:https://github.com/anomalyco/opencode/issues/48340"})) throw new Error("delete" + " applicability mismatch");
  const controls = JSON.parse(await readFile(join(here, "mutation-controls.json"), "utf8"));
  if (!Array.isArray(controls.controls) || controls.controls.length < 3) throw new Error("delete" + " mutation controls missing");
}
const extension: HarnessExtension = { name: "delete-scenarios-v1", validate };
export default extension;
