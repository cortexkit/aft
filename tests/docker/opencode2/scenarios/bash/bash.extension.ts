import { readFile } from "node:fs/promises";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { bashDeadTransportDeclaration } from "../../harness/permission-plan.js";
import type {
  HarnessExtension,
  HarnessRuntimeEvent,
  HarnessValidationContext,
  RecordedMockExchange,
  ScenarioDefinition,
  ScenarioLifecycleContext,
  ScenarioResult,
} from "../../harness/types.js";
import {
  isLinkedWorktreeScenario,
  judgeLinkedWorktreeResults,
  prepareLinkedWorktree,
} from "./linked-worktree.js";
const here = dirname(fileURLToPath(import.meta.url));
const expectedIds = ["bash/T1/happy","bash/T1/linked_worktree","bash/T2/invalid_arguments","bash/T2/missing_target","bash/T3/fallback_ask_allow","bash/T3/fallback_ask_deny","bash/T3/fallback_config_deny","bash/T3/loop_ask_allow","bash/T3/loop_ask_deny","bash/T3/loop_config_deny","bash/T4/abort","bash/T5/completion_wake","bash/T5/message_aborts_sync_watch","bash/T5/message_detaches_wait","bash/T5/message_queue_keeps_wait","bash/T5/watch_pattern_once","bash/T6/bash-bash-output/complete","bash/T6/bash-bash-output/incomplete","bash/T6/bash-changed-files/complete","bash/T6/bash-changed-files/incomplete","bash/T6/bash-diff-lines/complete","bash/T6/bash-diff-lines/incomplete","bash/T7/happy","bash/T7/linked_worktree"];
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
  for (const scenario of scenarios) {
    const plan = messageDetachPlan(scenario);
    if (!plan) continue;
    const control = scenario.controls?.find((candidate) => candidate.id === plan.control_id);
    if (control?.purpose !== "message") {
      throw new Error(scenario.id + " message_detach names no message control " + plan.control_id);
    }
    if (scenario.compare_call_id !== plan.call_id) {
      throw new Error(scenario.id + " message_detach must judge the compared call " + String(scenario.compare_call_id));
    }
  }
}

/**
 * A row that sends a message while a call is waiting declares which control
 * sends it, which call it must end, and how soon. The row's result comparison
 * alone proves the call returned the interrupted-wait text; this adds that the
 * result came back promptly after the message, so a wait that ran to its
 * natural end and only then reported cannot pass.
 */
export interface MessageDetachPlan {
  control_id: string;
  call_id: string;
  budget_ms: number;
}

export function messageDetachPlan(scenario: ScenarioDefinition): MessageDetachPlan | undefined {
  const raw = scenario.metadata?.message_detach as Partial<MessageDetachPlan> | undefined;
  if (!raw) return undefined;
  if (
    typeof raw.control_id !== "string" ||
    typeof raw.call_id !== "string" ||
    !Number.isInteger(raw.budget_ms) ||
    (raw.budget_ms ?? 0) < 1
  ) {
    throw new Error(scenario.id + " has an invalid message_detach declaration");
  }
  return raw as MessageDetachPlan;
}

/**
 * Judge one runtime event of a message-detach row. `sentAt` holds, per running
 * row, when its message control started; a result seen before that, or later
 * than the budget after it, was not ended by the message.
 */
export function judgeMessageDetachEvent(
  scenario: ScenarioDefinition,
  event: HarnessRuntimeEvent,
  sentAt: Map<string, number>,
  key: string,
): void {
  const plan = messageDetachPlan(scenario);
  if (!plan) return;
  if (event.kind === "control_started" && event.control.id === plan.control_id) {
    sentAt.set(key, event.at);
    return;
  }
  if (event.kind !== "tool_result_observed" || event.call.id !== plan.call_id) return;
  const sent = sentAt.get(key);
  if (sent === undefined) {
    throw new Error(
      `${scenario.id}: the ${plan.call_id} result came back before the message was sent, so no message ended that wait`,
    );
  }
  const elapsed = event.at - sent;
  if (elapsed > plan.budget_ms) {
    throw new Error(
      `${scenario.id}: the ${plan.call_id} result came back ${elapsed}ms after the message, over the ${plan.budget_ms}ms budget, so the message did not end the wait`,
    );
  }
}

// Keyed by the row's forensic directory, which is unique per row and host run,
// because rows run concurrently and share this extension.
const messageSentAt = new Map<string, number>();

// Every model request the host sent during a linked-worktree row, keyed by the
// row's forensic directory like messageSentAt. The host hands each tool result
// back inside the next model request, so these hold the results the row judges.
const linkedWorktreeExchanges = new Map<string, RecordedMockExchange[]>();

async function beforeScenario(context: ScenarioLifecycleContext): Promise<void> {
  if (!isLinkedWorktreeScenario(context.scenario)) return;
  linkedWorktreeExchanges.set(context.forensic_dir, []);
  await prepareLinkedWorktree(context.project_root);
}

function observe(context: ScenarioLifecycleContext, event: HarnessRuntimeEvent): void {
  judgeMessageDetachEvent(context.scenario, event, messageSentAt, context.forensic_dir);
  if (event.kind === "host_exit") messageSentAt.delete(context.forensic_dir);
  if (event.kind === "mock_exchange") {
    linkedWorktreeExchanges.get(context.forensic_dir)?.push(event.exchange);
  }
}

async function afterScenario(
  context: ScenarioLifecycleContext,
  result: ScenarioResult,
): Promise<void> {
  const exchanges = linkedWorktreeExchanges.get(context.forensic_dir);
  if (!exchanges) return;
  linkedWorktreeExchanges.delete(context.forensic_dir);
  // Skip the full-path check when the row has already failed, so the failure
  // that happened first stays the one the report names.
  if (result.status !== "passed") return;
  await judgeLinkedWorktreeResults(context.project_root, exchanges);
}

const extension: HarnessExtension = {
  name: "bash-scenarios-v1",
  validate,
  beforeScenario,
  observe,
  afterScenario,
};
export default extension;
