import { readdir, readFile } from "node:fs/promises";
import { extname, join, relative } from "node:path";
import { pathToFileURL } from "node:url";

import { fail } from "./errors.js";
import { TRAJECTORIES, type ScenarioDefinition, type ScenarioRegistration } from "./types.js";
import { asRecord } from "./util.js";

const REGISTRATION_NAMES = new Set([
  "registration.json",
  "registration.ts",
  "registration.mts",
  "scenarios.json",
  "scenarios.ts",
]);

export async function discoverScenarioRegistrationFiles(root: string): Promise<string[]> {
  const files: string[] = [];
  async function walk(directory: string): Promise<void> {
    let entries;
    try {
      entries = await readdir(directory, { withFileTypes: true });
    } catch (error) {
      if ((error as NodeJS.ErrnoException).code === "ENOENT") return;
      throw error;
    }
    entries.sort((left, right) => left.name.localeCompare(right.name));
    for (const entry of entries) {
      const path = join(directory, entry.name);
      if (entry.isDirectory()) await walk(path);
      else if (
        entry.isFile() &&
        (REGISTRATION_NAMES.has(entry.name) || entry.name.endsWith(".scenario.json"))
      ) {
        files.push(path);
      }
    }
  }
  await walk(root);
  return files.sort();
}

async function loadModuleValue(path: string): Promise<unknown> {
  if (extname(path) === ".json") return JSON.parse(await readFile(path, "utf8")) as unknown;
  const module = (await import(`${pathToFileURL(path).href}?harness=${Date.now()}`)) as Record<
    string,
    unknown
  >;
  return module.default ?? module.registration ?? module.scenarios;
}

function validateToolCall(
  scenario: ScenarioDefinition,
  turnIndex: number,
  callIndex: number,
): void {
  const call = scenario.turns[turnIndex].response;
  if (call.kind !== "tool_calls") return;
  const planned = call.calls[callIndex];
  if (!planned.id || !planned.name || !asRecord(planned.arguments)) {
    fail(
      "scenario_invalid",
      `${scenario.id}: turn ${turnIndex + 1} call ${callIndex + 1} is incomplete`,
    );
  }
  const classificationCount =
    Number(planned.disk_effects !== undefined) +
    Number(planned.non_mutating_evidence !== undefined);
  if (classificationCount > 1) {
    fail("scenario_invalid", `${scenario.id}:${planned.id} declares both disk classifications`);
  }
  if (planned.non_mutating_evidence && !planned.non_mutating_evidence.reason) {
    fail("scenario_invalid", `${scenario.id}:${planned.id} has empty non_mutating_evidence`);
  }
}

export function validateScenarioDefinition(
  scenario: ScenarioDefinition,
  registrationTool: string,
): void {
  if (scenario.schema_version !== 1)
    fail("scenario_invalid", `${scenario.id || "<unknown>"}: schema_version`);
  if (!scenario.id || !scenario.tool || !scenario.prompt) {
    fail("scenario_invalid", "scenario requires id, tool, and prompt");
  }
  if (scenario.tool !== registrationTool) {
    fail(
      "scenario_invalid",
      `${scenario.id}: registration tool ${registrationTool} disagrees with ${scenario.tool}`,
    );
  }
  const parts = scenario.id.split("/");
  if (parts[0] !== scenario.tool || parts[1] !== scenario.trajectory) {
    fail(
      "scenario_invalid",
      `${scenario.id}: id must start with ${scenario.tool}/${scenario.trajectory}`,
    );
  }
  if (!TRAJECTORIES.includes(scenario.trajectory)) {
    fail("scenario_invalid", `${scenario.id}: unknown trajectory ${scenario.trajectory}`);
  }
  const requiredMode = ["T3", "T4", "T5"].includes(scenario.trajectory)
    ? "shared-server"
    : "standalone";
  if (scenario.execution !== requiredMode) {
    fail("scenario_invalid", `${scenario.id}: ${scenario.trajectory} requires ${requiredMode}`);
  }
  if (scenario.trajectory === "T3" && scenario.auto) {
    fail("scenario_invalid", `${scenario.id}: permission scenarios must not enable --auto`);
  }
  if (!Array.isArray(scenario.turns) || scenario.turns.length === 0) {
    fail("scenario_invalid", `${scenario.id}: no scripted turns`);
  }
  const labels = scenario.turns.map((turn) => turn.label);
  if (new Set(labels).size !== labels.length || labels.some((label) => !label)) {
    fail("scenario_invalid", `${scenario.id}: turn labels must be unique and non-empty`);
  }
  if (
    scenario.expected_turns &&
    JSON.stringify(labels) !== JSON.stringify(scenario.expected_turns)
  ) {
    fail(
      "scenario_invalid",
      `${scenario.id}: expected_turns must exactly match scripted turn order`,
    );
  }
  for (const [turnIndex, turn] of scenario.turns.entries()) {
    if (!turn.response || !["text", "tool_calls"].includes(turn.response.kind)) {
      fail("scenario_invalid", `${scenario.id}: turn ${turnIndex + 1} response is invalid`);
    }
    if (turn.response.kind === "tool_calls") {
      if (!Array.isArray(turn.response.calls) || turn.response.calls.length === 0) {
        fail("scenario_invalid", `${scenario.id}: turn ${turnIndex + 1} has no calls`);
      }
      for (const callIndex of turn.response.calls.keys())
        validateToolCall(scenario, turnIndex, callIndex);
    }
  }
  const callIds = scenario.turns.flatMap((turn) =>
    turn.response.kind === "tool_calls" ? turn.response.calls.map((call) => call.id) : [],
  );
  if (new Set(callIds).size !== callIds.length) {
    fail("scenario_invalid", `${scenario.id}: tool call ids must be unique`);
  }
  if ((scenario.trajectory === "T1" || scenario.trajectory === "T7") && !scenario.comparison) {
    fail("scenario_invalid", `${scenario.id}: happy-path and parity scenarios require comparison`);
  }
  if (scenario.trajectory === "T2" && scenario.error_origin === "product") {
    if (
      typeof scenario.metadata?.error_code !== "string" ||
      typeof scenario.metadata?.steering_pattern !== "string"
    ) {
      fail(
        "scenario_invalid",
        `${scenario.id}: product T2 requires error_code and steering_pattern metadata`,
      );
    }
  }
  if (scenario.comparison && !scenario.compare_call_id) {
    fail("scenario_invalid", `${scenario.id}: comparison requires compare_call_id`);
  }
  if (scenario.compare_call_id && !callIds.includes(scenario.compare_call_id)) {
    fail("scenario_invalid", `${scenario.id}: compare_call_id does not name a scripted call`);
  }
  if (scenario.comparison?.mode === "shape" && scenario.comparison.rules.length === 0) {
    fail("scenario_invalid", `${scenario.id}: shape comparison has no rules`);
  }
  if (scenario.restore_evidence) {
    if (!scenario.restore_evidence.identifier || scenario.restore_evidence.calls.length === 0) {
      fail(
        "scenario_invalid",
        `${scenario.id}: restore evidence must name product calls and identifier`,
      );
    }
    if (scenario.restore_evidence.paths.length === 0) {
      fail("scenario_invalid", `${scenario.id}: restore evidence has no touched paths`);
    }
    for (const path of scenario.restore_evidence.paths) {
      if (
        (path.transition === "move_destination" ||
          path.transition === "move_overwrite_destination") &&
        !path.paired_path
      ) {
        fail("scenario_invalid", `${scenario.id}: move destination ${path.path} lacks paired_path`);
      }
      if (
        (path.transition === "create" || path.transition === "move_overwrite_destination") &&
        !path.expected_intermediate_sha256
      ) {
        fail("scenario_invalid", `${scenario.id}: ${path.path} lacks expected intermediate bytes`);
      }
    }
    const transitions = new Set(scenario.restore_evidence.paths.map((path) => path.transition));
    if (
      (transitions.has("move_source") ||
        transitions.has("move_destination") ||
        transitions.has("move_overwrite_destination")) &&
      !(
        transitions.has("move_source") &&
        (transitions.has("move_destination") || transitions.has("move_overwrite_destination"))
      )
    ) {
      fail("scenario_invalid", `${scenario.id}: move restore evidence must declare both legs`);
    }
  }
  if (scenario.quiescence_timeout_ms !== undefined) {
    if (!Number.isInteger(scenario.quiescence_timeout_ms) || scenario.quiescence_timeout_ms < 1) {
      fail("scenario_invalid", `${scenario.id}: invalid quiescence_timeout_ms`);
    }
  }
}

function parseRegistration(value: unknown, path: string): ScenarioRegistration {
  const record = asRecord(value);
  if (
    record?.schema_version !== 1 ||
    typeof record.tool !== "string" ||
    !Array.isArray(record.scenarios)
  ) {
    fail("scenario_invalid", `${path}: registration requires schema_version, tool, and scenarios`);
  }
  const registration = record as unknown as ScenarioRegistration;
  for (const scenario of registration.scenarios)
    validateScenarioDefinition(scenario, registration.tool);
  return registration;
}

export function addCallgraphWarmup(scenario: ScenarioDefinition): ScenarioDefinition {
  if (scenario.tool !== "callgraph") return scenario;
  const index = scenario.turns.findIndex(
    (turn) =>
      turn.response.kind === "tool_calls" &&
      turn.response.calls.some((call) => call.name === "aft_callgraph"),
  );
  if (index === -1) return scenario;
  const target = structuredClone(scenario.turns[index]);
  if (target.response.kind !== "tool_calls") return scenario;
  const warmupLabel = `${target.label}-warmup`;
  target.label = warmupLabel;
  for (const call of target.response.calls) call.id = `warmup-${call.id}`;
  const turns = scenario.turns.map((turn) => structuredClone(turn));
  turns[index] = { ...turns[index], delay_ms: Math.max(turns[index].delay_ms ?? 0, 2_000) };
  turns.splice(index, 0, target);
  const expectedTurns = [
    ...(scenario.expected_turns ?? scenario.turns.map((turn) => turn.label)),
  ];
  const expectedIndex = expectedTurns.indexOf(scenario.turns[index].label);
  expectedTurns.splice(expectedIndex === -1 ? index : expectedIndex, 0, warmupLabel);
  return { ...scenario, turns, expected_turns: expectedTurns };
}

export async function loadScenarios(root: string): Promise<ScenarioDefinition[]> {
  const files = await discoverScenarioRegistrationFiles(root);
  const loaded = await Promise.all(
    files.map(
      async (path) => [path, parseRegistration(await loadModuleValue(path), path)] as const,
    ),
  );
  const scenarios: ScenarioDefinition[] = [];
  const ids = new Map<string, string>();
  for (const [path, registration] of loaded) {
    for (const scenario of registration.scenarios) {
      const previous = ids.get(scenario.id);
      if (previous) {
        fail("scenario_invalid", `duplicate scenario ${scenario.id}`, {
          first: relative(root, previous),
          second: relative(root, path),
        });
      }
      ids.set(scenario.id, path);
      scenarios.push(addCallgraphWarmup({ ...scenario, registration_path: path }));
    }
  }
  return scenarios.sort((left, right) => left.id.localeCompare(right.id));
}

export function materializeParityScenarios(
  scenarios: readonly ScenarioDefinition[],
): ScenarioDefinition[] {
  const output = [...scenarios];
  const tools = new Set(scenarios.map((scenario) => scenario.tool));
  for (const tool of tools) {
    const happy = scenarios.filter(
      (scenario) => scenario.tool === tool && scenario.trajectory === "T1",
    );
    const explicit = scenarios.filter(
      (scenario) => scenario.tool === tool && scenario.trajectory === "T7",
    );
    if (explicit.length > 0) {
      const expectedIds = happy.map((scenario) =>
        scenario.id.replace(`/${scenario.trajectory}`, "/T7"),
      );
      const explicitIds = explicit.map((scenario) => scenario.id).sort();
      if (JSON.stringify(expectedIds.sort()) !== JSON.stringify(explicitIds)) {
        fail("scenario_invalid", `${tool}/T7 must have the same scenario identities as T1`);
      }
      continue;
    }
    output.push(
      ...happy.map((scenario) => ({
        ...structuredClone(scenario),
        id: scenario.id.replace(`/${scenario.trajectory}`, "/T7"),
        trajectory: "T7" as const,
        execution: "standalone" as const,
        controls: undefined,
        restore_evidence: undefined,
      })),
    );
  }
  return output.sort((left, right) => left.id.localeCompare(right.id));
}

export function filterScenarios(
  scenarios: readonly ScenarioDefinition[],
  selector?: string,
): ScenarioDefinition[] {
  if (!selector) return [...scenarios];
  const normalized = selector.replace(/\/$/, "");
  const selected = scenarios.filter(
    (scenario) => scenario.id === normalized || scenario.id.startsWith(`${normalized}/`),
  );
  if (selected.length === 0)
    fail("scenario_invalid", `scenario selector matched nothing: ${selector}`);
  return selected;
}
