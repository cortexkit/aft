import { readFile } from "node:fs/promises";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import type { HarnessExtension, HarnessValidationContext } from "../../harness/types.js";
const here = dirname(fileURLToPath(import.meta.url));
const expectedIds = ["search/T1/happy","search/T2/invalid_arguments","search/T2/missing_target","search/T6/search-payload-results/complete","search/T6/search-payload-results/incomplete","search/T7/happy"];
async function validate(context: HarnessValidationContext): Promise<void> {
  const scenarios = context.scenarios.filter((scenario) => scenario.tool === "search");
  const actual = scenarios.map((scenario) => scenario.id).sort();
  if (JSON.stringify(actual) !== JSON.stringify(expectedIds)) throw new Error("search" + " scenario identity mismatch: " + actual.join(","));
  for (const scenario of scenarios) for (const turn of scenario.turns) if (turn.response.kind === "tool_calls") for (const call of turn.response.calls) {
    if (call.disk_effects !== undefined && call.non_mutating_evidence !== undefined) throw new Error(scenario.id + ":" + call.id + " has two disk classifications");
    if (call.disk_effects === undefined && call.non_mutating_evidence === undefined) throw new Error(scenario.id + ":" + call.id + " lacks a disk classification");
  }
  const matrix = JSON.parse(await readFile(join(here, "matrix.json"), "utf8"));
  if (JSON.stringify(matrix.rows?.[0]?.trajectories) !== JSON.stringify({"T1":"applicable","T2":"applicable","T3":"n/a:no-permission-operation","T4":"n/a:no-abortable-operation","T5":"n/a:no-background-capability","T6":"applicable","T7":"expected_fail:https://github.com/anomalyco/opencode/issues/48340"})) throw new Error("search" + " applicability mismatch");
  const controls = JSON.parse(await readFile(join(here, "mutation-controls.json"), "utf8"));
  if (!Array.isArray(controls.controls) || controls.controls.length < 3) throw new Error("search" + " mutation controls missing");
}
const extension: HarnessExtension = { name: "search-scenarios-v1", validate };
export default extension;
