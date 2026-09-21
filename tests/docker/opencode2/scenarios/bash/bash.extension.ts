import { readFile } from "node:fs/promises";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { bashDeadTransportDeclaration } from "../../harness/permission-plan.js";
import type { HarnessExtension, HarnessValidationContext } from "../../harness/types.js";
const here = dirname(fileURLToPath(import.meta.url));
const expectedIds = ["bash/T1/happy","bash/T2/invalid_arguments","bash/T2/missing_target","bash/T3/fallback_ask_allow","bash/T3/fallback_ask_deny","bash/T3/fallback_config_deny","bash/T3/loop_ask_allow","bash/T3/loop_ask_deny","bash/T3/loop_config_deny","bash/T4/abort","bash/T5/completion_wake","bash/T5/watch_pattern_once","bash/T6/bash-bash-output/complete","bash/T6/bash-bash-output/incomplete","bash/T7/happy"];
async function validate(context: HarnessValidationContext): Promise<void> {
  const scenarios = context.scenarios.filter((scenario) => scenario.tool === "bash");
  const actual = scenarios.map((scenario) => scenario.id).sort();
  if (JSON.stringify(actual) !== JSON.stringify(expectedIds)) throw new Error("bash" + " scenario identity mismatch: " + actual.join(","));
  for (const scenario of scenarios) for (const turn of scenario.turns) if (turn.response.kind === "tool_calls") for (const call of turn.response.calls) {
    if (call.disk_effects !== undefined && call.non_mutating_evidence !== undefined) throw new Error(scenario.id + ":" + call.id + " has two disk classifications");
    if (call.disk_effects === undefined && call.non_mutating_evidence === undefined) throw new Error(scenario.id + ":" + call.id + " lacks a disk classification");
  }
  const matrix = JSON.parse(await readFile(join(here, "matrix.json"), "utf8"));
  if (JSON.stringify(matrix.rows?.[0]?.trajectories) !== JSON.stringify({"T1":"applicable","T2":"applicable","T3":"applicable","T4":"applicable","T5":"applicable","T6":"applicable","T7":"applicable"})) throw new Error("bash" + " applicability mismatch");
  const controls = JSON.parse(await readFile(join(here, "mutation-controls.json"), "utf8"));
  if (!Array.isArray(controls.controls) || controls.controls.length < 3) throw new Error("bash" + " mutation controls missing");
  // Every fallback row says what its dead transport produces, and a row that
  // refuses instead of falling back has to record where the break-glass
  // coverage went. Read at validation time so a row that drops the record
  // fails before a run, rather than passing quietly with one capability less
  // than the slice claims.
  for (const scenario of scenarios.filter((candidate) => candidate.id.startsWith("bash/T3/fallback_"))) {
    const declared = bashDeadTransportDeclaration(scenario);
    if (declared.outcome === "refusal" && !declared.hostFallbackCoverage) {
      throw new Error(scenario.id + " does not record its break-glass coverage");
    }
  }
}
const extension: HarnessExtension = { name: "bash-scenarios-v1", validate };
export default extension;
