import { access, readFile, readdir } from "node:fs/promises";
import { join } from "node:path";

import {
  loadHostCliContract,
  loadHostProviderConfigContract,
  loadHostSchemaRejectionContract,
} from "./contracts.js";
import { fail } from "./errors.js";
import { readPermissionAskInventory, validatePermissionInventory } from "./inventory.js";
import type {
  HarnessValidationContext,
  ScenarioDefinition,
  ToolCallPlan,
  Trajectory,
} from "./types.js";
import { TRAJECTORIES } from "./types.js";
import { asRecord, readJson } from "./util.js";

export type MatrixClassification = "applicable" | `expected_fail:${string}` | `n/a:${string}`;

export interface MatrixRow {
  tool: string;
  trajectories: Record<Trajectory, MatrixClassification>;
}

export interface ApplicabilityMatrix {
  schema_version: 1;
  platform: string;
  rows: MatrixRow[];
}

export interface ListSurface {
  id: string;
  command: string;
  mode: string;
  list_id: string;
  owner: string;
  reasons: string[];
}

export interface ParityAllowlistEntry {
  scenario: string;
  tool: string;
  field: string;
  reason: string;
}

export interface ValidatedInputs {
  context: HarnessValidationContext;
  matrix?: ApplicabilityMatrix;
  inventory: string[];
  mutatingTools: Set<string>;
  surfaces: ListSurface[];
  requiredCheckStatus: "MET" | "NOT MET (advisory only)";
  parityAllowlist: ParityAllowlistEntry[];
}

const TEST_REMOVE_MUTATING = "AFT_OPENCODE2_TEST_REMOVE_MUTATING_TOOL";
const TEST_EVIDENCE = "AFT_OPENCODE2_TEST_NON_MUTATING_EVIDENCE";

export const V2_SCHEMA_PROJECTION_EXCLUSIONS = {
  projection_only: ["bash_kill", "bash_status", "bash_watch", "bash_write"],
  schema_only: ["powershell"],
} as const;

function canonicalToolName(name: string): string {
  const aliases: Record<string, string> = {
    aft_delete: "delete",
    aft_import: "import",
    aft_move: "move",
    aft_safety: "safety",
    aft_callgraph: "callgraph",
    aft_conflicts: "conflicts",
    aft_inspect: "inspect",
    aft_outline: "outline",
    aft_search: "search",
    aft_zoom: "zoom",
  };
  return aliases[name] ?? name;
}

function parseClassification(value: unknown, label: string): MatrixClassification {
  const classification =
    typeof value === "string"
      ? value
      : typeof asRecord(value)?.classification === "string"
        ? (asRecord(value)?.classification as string)
        : undefined;
  if (
    !classification ||
    (classification !== "applicable" &&
      !classification.startsWith("n/a:") &&
      !classification.startsWith("expected_fail:"))
  ) {
    fail("matrix_invalid", `${label}: invalid classification ${String(classification)}`);
  }
  if (
    (classification.startsWith("n/a:") || classification.startsWith("expected_fail:")) &&
    !classification.split(":")[1]
  ) {
    fail("matrix_invalid", `${label}: classification needs a reason or issue`);
  }
  return classification as MatrixClassification;
}

function parseMatrix(value: unknown): ApplicabilityMatrix {
  const record = asRecord(value);
  if (record?.schema_version !== 1 || typeof record.platform !== "string") {
    fail("matrix_invalid", "applicability matrix schema or platform is invalid");
  }
  const rawRows = record.rows;
  const rows: MatrixRow[] = [];
  if (Array.isArray(rawRows)) {
    for (const raw of rawRows) {
      const row = asRecord(raw);
      const trajectories = asRecord(row?.trajectories);
      if (typeof row?.tool !== "string" || !trajectories)
        fail("matrix_invalid", "matrix row is invalid");
      rows.push({
        tool: row.tool,
        trajectories: Object.fromEntries(
          TRAJECTORIES.map((trajectory) => [
            trajectory,
            parseClassification(trajectories[trajectory], `${row.tool}/${trajectory}`),
          ]),
        ) as Record<Trajectory, MatrixClassification>,
      });
    }
  } else {
    const rowMap = asRecord(rawRows);
    if (!rowMap) fail("matrix_invalid", "matrix rows must be an array or object");
    for (const [tool, raw] of Object.entries(rowMap)) {
      const trajectories = asRecord(asRecord(raw)?.trajectories ?? raw);
      if (!trajectories) fail("matrix_invalid", `${tool}: matrix row is invalid`);
      rows.push({
        tool,
        trajectories: Object.fromEntries(
          TRAJECTORIES.map((trajectory) => [
            trajectory,
            parseClassification(trajectories[trajectory], `${tool}/${trajectory}`),
          ]),
        ) as Record<Trajectory, MatrixClassification>,
      });
    }
  }
  return { schema_version: 1, platform: record.platform, rows };
}

async function firstExisting(paths: string[]): Promise<string | undefined> {
  for (const path of paths) {
    if (
      await access(path)
        .then(() => true)
        .catch(() => false)
    )
      return path;
  }
  return undefined;
}

export async function loadApplicabilityMatrix(
  matrixRoot: string,
  required = true,
): Promise<ApplicabilityMatrix | undefined> {
  const path = await firstExisting([
    join(matrixRoot, "applicability.json"),
    join(matrixRoot, "matrix.json"),
  ]);
  if (!path) {
    if (required) fail("matrix_absent", `applicability matrix missing under ${matrixRoot}`);
    return undefined;
  }
  return parseMatrix(await readJson(path));
}

export async function loadToolSchemas(
  repoRoot: string,
): Promise<Record<string, Record<string, unknown>>> {
  const path = join(repoRoot, "crates", "aft", "src", "subc_tool_schemas.json");
  const record = asRecord(await readJson(path));
  if (!record) fail("matrix_invalid", "tool schema artifact is invalid", { path }, true);
  return record as Record<string, Record<string, unknown>>;
}

export function deriveV2HarnessProjection(
  scenarios: readonly ScenarioDefinition[],
  platform: NodeJS.Platform,
): string[] {
  const projection = new Set(scenarios.map((scenario) => canonicalToolName(scenario.tool)));
  if (platform !== "win32") projection.delete("powershell");
  return [...projection].sort();
}

/// The V2 projection derived from the committed schema artifact alone: every
/// direct schema key plus the projection-only decomposed bash tools, minus the
/// platform-absent keys. An observation-only run (one tool's scenarios during
/// debugging) validates against this so a scenario subset is never read as
/// inventory drift; a full run derives the projection from the scenarios so
/// that a tool with no scenarios is drift.
export function deriveSchemaProjection(
  schemas: Record<string, Record<string, unknown>>,
  platform: NodeJS.Platform,
): string[] {
  const projection = new Set(Object.keys(schemas).map(canonicalToolName));
  for (const tool of V2_SCHEMA_PROJECTION_EXCLUSIONS.projection_only) projection.add(tool);
  for (const tool of V2_SCHEMA_PROJECTION_EXCLUSIONS.schema_only) projection.delete(tool);
  if (platform === "win32") projection.add("powershell");
  return [...projection].sort();
}

function difference(left: ReadonlySet<string>, right: ReadonlySet<string>): string[] {
  return [...left].filter((tool) => !right.has(tool)).sort();
}

function assertExactExclusions(label: string, actual: string[], expected: readonly string[]): void {
  if (JSON.stringify(actual) !== JSON.stringify([...expected].sort())) {
    fail("matrix_invalid", `${label} do not match the explicit exclusion table`, {
      actual,
      expected,
    });
  }
}

function inventoryFromProjection(
  schemas: Record<string, Record<string, unknown>>,
  projectedTools: readonly string[],
): string[] {
  const schemaInventory = new Set(Object.keys(schemas).map(canonicalToolName));
  const projection = new Set(projectedTools.map(canonicalToolName));
  assertExactExclusions(
    "projection-only tools",
    difference(projection, schemaInventory),
    V2_SCHEMA_PROJECTION_EXCLUSIONS.projection_only,
  );
  assertExactExclusions(
    "schema-only tools",
    difference(schemaInventory, projection),
    V2_SCHEMA_PROJECTION_EXCLUSIONS.schema_only,
  );
  return [...new Set([...projection, ...V2_SCHEMA_PROJECTION_EXCLUSIONS.schema_only])].sort();
}

export function validateInventory(
  matrix: ApplicabilityMatrix,
  schemas: Record<string, Record<string, unknown>>,
  platform: NodeJS.Platform,
  projectedTools: readonly string[],
): string[] {
  const inventory = inventoryFromProjection(schemas, projectedTools);
  const rows = new Map<string, MatrixRow>();
  for (const row of matrix.rows) {
    if (rows.has(row.tool)) fail("matrix_invalid", `duplicate matrix row: ${row.tool}`);
    rows.set(row.tool, row);
  }
  for (const tool of inventory) {
    if (!rows.has(tool)) fail("matrix_invalid", `tool missing matrix row: ${tool}`);
  }
  for (const tool of rows.keys()) {
    if (!inventory.includes(tool)) fail("matrix_invalid", `matrix row names removed tool: ${tool}`);
  }
  if (platform === "linux") {
    const powershell = rows.get("powershell");
    for (const trajectory of TRAJECTORIES) {
      if (powershell?.trajectories[trajectory] !== "n/a:platform") {
        fail("matrix_invalid", `powershell/${trajectory} must be n/a:platform on Linux`);
      }
    }
  }
  return inventory;
}

function validateScenarioRows(
  matrix: ApplicabilityMatrix,
  scenarios: readonly ScenarioDefinition[],
  schemas: Record<string, Record<string, unknown>>,
  observationOnly = false,
): void {
  // An observation-only run carries one tool's scenarios; only the rows of
  // tools present in that set are checked for coverage. A full run checks
  // every row, so an applicable row with no scenario is drift.
  const coveredTools = new Set(scenarios.map((scenario) => scenario.tool));
  const byParent = new Map<string, ScenarioDefinition[]>();
  for (const scenario of scenarios) {
    const key = `${scenario.tool}/${scenario.trajectory}`;
    byParent.set(key, [...(byParent.get(key) ?? []), scenario]);
  }
  for (const row of matrix.rows) {
    if (observationOnly && !coveredTools.has(row.tool)) continue;
    for (const trajectory of TRAJECTORIES) {
      const key = `${row.tool}/${trajectory}`;
      const classification = row.trajectories[trajectory];
      const fixtures = byParent.get(key) ?? [];
      if (
        (classification === "applicable" || classification.startsWith("expected_fail:")) &&
        fixtures.length === 0
      ) {
        fail("matrix_invalid", `applicable row has no scenario: ${key}`);
      }
      if (classification === "n/a:platform" && fixtures.length > 0) {
        fail("matrix_invalid", `platform-absent row has scripted scenarios: ${key}`);
      }
    }
  }
  for (const scenario of scenarios) {
    const row = matrix.rows.find((candidate) => candidate.tool === scenario.tool);
    if (!row) fail("matrix_invalid", `scenario names tool outside matrix: ${scenario.id}`);
    if (row.trajectories[scenario.trajectory].startsWith("n/a:")) {
      fail("matrix_invalid", `scenario exists for non-applicable row: ${scenario.id}`);
    }
  }

  for (const row of matrix.rows) {
    if (!schemas[row.tool]) continue;
    if (observationOnly && !coveredTools.has(row.tool)) continue;
    // A tool absent on the run's platform is outside the tool universe: every
    // trajectory cell is n/a:platform and no T2 subcase can exist for it.
    if (row.trajectories.T2.startsWith("n/a:platform")) continue;
    const t2 = byParent.get(`${row.tool}/T2`) ?? [];
    if (!t2.some((scenario) => scenario.subcase === "invalid_arguments")) {
      fail("matrix_invalid", `${row.tool}/T2 missing invalid_arguments`);
    }
    const required = schemas[row.tool].required;
    const needsTarget = Array.isArray(required) && required.length > 0;
    const hasMissingTarget = t2.some((scenario) => scenario.subcase === "missing_target");
    if (needsTarget !== hasMissingTarget) {
      fail(
        "matrix_invalid",
        needsTarget
          ? `${row.tool}/T2 missing missing_target`
          : `${row.tool}/T2 argument-free tool must use n/a:no-target-argument`,
      );
    }
  }
}

function backgroundCapableTools(schemas: Record<string, Record<string, unknown>>): Set<string> {
  return new Set(
    Object.entries(schemas)
      .filter(([, schema]) => Object.hasOwn(asRecord(schema.properties) ?? {}, "background"))
      .map(([tool]) => canonicalToolName(tool)),
  );
}

async function validateT5Inventory(
  matrixRoot: string,
  matrix: ApplicabilityMatrix,
  schemas: Record<string, Record<string, unknown>>,
): Promise<void> {
  const path = join(matrixRoot, "t5-capability.json");
  const record = asRecord(await readJson(path).catch(() => undefined));
  const rawTools = record?.tools ?? record?.capable_tools;
  if (!Array.isArray(rawTools) || rawTools.some((tool) => typeof tool !== "string")) {
    fail("matrix_invalid", "t5-capability.json is missing or invalid");
  }
  const declared = new Set(rawTools as string[]);
  const derived = backgroundCapableTools(schemas);
  for (const tool of derived)
    if (!declared.has(tool)) fail("matrix_invalid", `${tool}/T5 missing capability`);
  for (const tool of declared)
    if (!derived.has(tool)) fail("matrix_invalid", `${tool}/T5 lacks background parameter`);
  for (const row of matrix.rows) {
    if (row.tool === "powershell" && matrix.platform === "linux") continue;
    const classification = row.trajectories.T5;
    if (derived.has(row.tool) && classification === "n/a:no-background-capability") {
      fail("matrix_invalid", `${row.tool}/T5 incorrectly classified n/a:no-background-capability`);
    }
    if (!derived.has(row.tool) && classification !== "n/a:no-background-capability") {
      fail("matrix_invalid", `${row.tool}/T5 must derive n/a:no-background-capability`);
    }
  }
}

function ownerForCommand(command: string): string {
  if (["callers", "impact", "call_tree", "trace_to", "trace_data", "callgraph"].includes(command)) {
    return "callgraph";
  }
  return canonicalToolName(command === "semantic_search" ? "search" : command);
}

export async function deriveListSurfaces(repoRoot: string): Promise<ListSurface[]> {
  const path = join(repoRoot, "crates", "aft", "src", "list_surfaces.rs");
  const source = await readFile(path, "utf8");
  const registry = source.match(/pub static LIST_SURFACES[^=]*=\s*&\[(?<body>[\s\S]*?)\n\];/)
    ?.groups?.body;
  if (!registry) fail("matrix_invalid", "LIST_SURFACES registry cannot be parsed", { path }, true);
  const entries = [
    ...registry.matchAll(/SurfaceEntry\s*\{(?<body>[\s\S]*?)(?=\n\s*SurfaceEntry\s*\{|$)/g),
  ];
  return entries.map((entry) => {
    const body = entry.groups?.body ?? "";
    const command = body.match(/command:\s*"([^"]+)"/)?.[1];
    const mode = body.match(/mode:\s*"([^"]*)"/)?.[1];
    const listId = body.match(/list_id:\s*"([^"]+)"/)?.[1];
    if (command === undefined || mode === undefined || listId === undefined) {
      fail(
        "matrix_invalid",
        "LIST_SURFACES entry is incomplete",
        { entry: body.slice(0, 200) },
        true,
      );
    }
    const reasons = [...body.matchAll(/reason:\s*Reason::(Cap|Depth|Budget|Walk)/g)].map((match) =>
      match[1].toLowerCase(),
    );
    return {
      id: `${command}.${mode}.${listId}`,
      command,
      mode,
      list_id: listId,
      owner: ownerForCommand(command),
      reasons,
    };
  });
}

async function validateParityAllowlist(
  matrixRoot: string,
  scenarios: readonly ScenarioDefinition[],
): Promise<ParityAllowlistEntry[]> {
  const value = await readJson(join(matrixRoot, "parity-allowlist.json")).catch(() => undefined);
  if (value === undefined) fail("matrix_invalid", "parity-allowlist.json is missing");
  const record = asRecord(value);
  const entries: unknown[] = Array.isArray(record?.entries)
    ? record.entries
    : Array.isArray(value)
      ? value
      : Object.entries(record ?? {})
          .filter(([key]) => key !== "schema_version")
          .map(([key, raw]) => {
            const entry = asRecord(raw);
            const separator = key.indexOf(".");
            return {
              ...entry,
              tool: entry?.tool ?? (separator === -1 ? undefined : key.slice(0, separator)),
              field: entry?.field ?? (separator === -1 ? undefined : key.slice(separator + 1)),
            };
          });
  const validated: ParityAllowlistEntry[] = [];
  for (const raw of entries) {
    const entry = asRecord(raw);
    if (
      typeof entry?.scenario !== "string" ||
      typeof entry.tool !== "string" ||
      typeof entry.field !== "string" ||
      typeof entry.reason !== "string" ||
      entry.reason.length === 0
    ) {
      fail("matrix_invalid", "parity allowlist entry is invalid");
    }
    const scenario = scenarios.find((candidate) => candidate.id === entry.scenario);
    if (!scenario) {
      fail("matrix_invalid", `parity allowlist names unknown scenario: ${entry.scenario}`);
    }
    if (scenario.tool !== entry.tool) {
      fail("matrix_invalid", `parity allowlist tool mismatch: ${entry.scenario}.${entry.field}`);
    }
    if (scenario.comparison?.mode !== "shape") {
      fail("matrix_invalid", `exact comparison cannot carry parity allowlist: ${entry.scenario}`);
    }
    if (
      !scenario.comparison.rules.some(
        (rule) =>
          rule.kind !== "ignore" &&
          (rule.kind === "trailer" ? (rule.field ?? "trailer") : rule.field) === entry.field,
      )
    ) {
      fail(
        "matrix_invalid",
        `parity allowlist field is not projected: ${entry.tool}.${entry.field}`,
      );
    }
    validated.push({
      scenario: entry.scenario,
      tool: entry.tool,
      field: entry.field,
      reason: entry.reason,
    });
  }
  return validated;
}

async function validateRequiredCheckRecord(
  matrixRoot: string,
): Promise<"MET" | "NOT MET (advisory only)"> {
  const value = await readJson(join(matrixRoot, "required-check.json")).catch(() => undefined);
  if (value === undefined) return "NOT MET (advisory only)";
  const record = asRecord(value);
  if (!record) fail("matrix_invalid", "required-check.json must be an object");
  if (
    typeof record.context !== "string" ||
    record.context.length === 0 ||
    typeof record.observed_run_id !== "number" ||
    !Number.isInteger(record.observed_run_id) ||
    typeof record.observed_sha !== "string"
  ) {
    fail("matrix_invalid", "required-check.json lacks an emitted context, numeric run id, or SHA");
  }
  const complete =
    typeof record.required_since_run === "number" &&
    Number.isInteger(record.required_since_run) &&
    typeof record.protection_evidence_sha === "string" &&
    typeof record.probe_absent_sha === "string" &&
    typeof record.probe_failed_sha === "string" &&
    typeof record.observed_refusal === "string" &&
    /(GH006|required check|pending)/i.test(record.observed_refusal) &&
    !/(pull request|auto-merge)/i.test(record.observed_refusal);
  return complete ? "MET" : "NOT MET (advisory only)";
}

function validateT6(
  matrix: ApplicabilityMatrix,
  scenarios: readonly ScenarioDefinition[],
  surfaces: readonly ListSurface[],
): void {
  const byOwner = new Map<string, ListSurface[]>();
  for (const surface of surfaces)
    byOwner.set(surface.owner, [...(byOwner.get(surface.owner) ?? []), surface]);
  for (const row of matrix.rows) {
    if (row.tool === "powershell" && matrix.platform === "linux") continue;
    const owned = byOwner.get(row.tool) ?? [];
    const classification = row.trajectories.T6;
    if (owned.length === 0 && classification !== "n/a:no-list-surface") {
      fail("matrix_invalid", `${row.tool}/T6 must derive n/a:no-list-surface`);
    }
    if (owned.length > 0 && classification === "n/a:no-list-surface") {
      fail("matrix_invalid", `${row.tool}/T6 owns registered list surfaces`);
    }
  }
  const t6Metadata = scenarios
    .filter((scenario) => scenario.trajectory === "T6")
    .map((scenario) => ({ scenario, t6: asRecord(scenario.metadata?.t6) }));
  for (const surface of surfaces) {
    const fixtures = t6Metadata.filter(({ t6 }) => t6?.surface_id === surface.id);
    for (const fixture of fixtures) {
      if (
        fixture.scenario.tool !== surface.owner ||
        (fixture.t6?.owner !== undefined && fixture.t6.owner !== surface.owner)
      ) {
        fail(
          "matrix_invalid",
          `${surface.id} owner is ${surface.owner}, not ${fixture.scenario.tool}`,
          {
            surface: surface.id,
            derived_owner: surface.owner,
            declared_owner: fixture.scenario.tool,
          },
        );
      }
      const fixtureCallId = fixture.t6?.call_id ?? fixture.scenario.compare_call_id;
      if (typeof fixtureCallId !== "string") {
        fail("matrix_invalid", `${fixture.scenario.id} must identify its T6 subject call`);
      }
      const fixtureCall = callsForScenario(fixture.scenario).find(
        (call) => call.id === fixtureCallId,
      );
      if (!fixtureCall || canonicalToolName(fixtureCall.name) !== surface.owner) {
        fail(
          "matrix_invalid",
          `${surface.id} fixture call owner is ${fixtureCall?.name ?? "missing"}, not ${surface.owner}`,
        );
      }
      const reason = fixture.t6?.triggered_reason;
      if (
        reason !== undefined &&
        (typeof reason !== "string" || !surface.reasons.includes(reason))
      ) {
        fail("matrix_invalid", `${surface.id} does not support reason ${String(reason)}`);
      }
    }
    const kinds = new Set(fixtures.map(({ t6 }) => t6?.fixture));
    if (!kinds.has("complete") || !kinds.has("incomplete")) {
      fail("matrix_invalid", `${surface.id} requires complete and incomplete fixtures`);
    }
  }
  for (const { t6 } of t6Metadata) {
    if (
      typeof t6?.surface_id === "string" &&
      !surfaces.some((surface) => surface.id === t6.surface_id)
    ) {
      fail("matrix_invalid", `unknown T6 surface: ${t6.surface_id}`);
    }
  }
}

export function applyMutatingTestOverride(
  tools: Set<string>,
  env: NodeJS.ProcessEnv,
  testMode: boolean,
): { tools: Set<string>; fabricatedEvidence?: { tool: string; reason: string } } {
  const remove = env[TEST_REMOVE_MUTATING];
  const evidence = env[TEST_EVIDENCE];
  if (!remove && !evidence) return { tools: new Set(tools) };
  if (!testMode || env.AFT_OPENCODE2_HARNESS_SELF_TEST !== "1") {
    fail(
      "matrix_invalid",
      "test-local mutating override is refused outside the harness self-test",
      {},
      true,
    );
  }
  if (!remove || !tools.has(remove) || !evidence) {
    fail(
      "matrix_invalid",
      "test-local mutating override requires one known tool and fabricated evidence",
      {},
      true,
    );
  }
  const overridden = new Set(tools);
  overridden.delete(remove);
  return { tools: overridden, fabricatedEvidence: { tool: remove, reason: evidence } };
}

function callsForScenario(scenario: ScenarioDefinition): ToolCallPlan[] {
  return scenario.turns.flatMap((turn) =>
    turn.response.kind === "tool_calls" ? turn.response.calls : [],
  );
}

function validateRestoreCoverage(
  scenarios: readonly ScenarioDefinition[],
  mutatingTools: ReadonlySet<string>,
): void {
  for (const scenario of scenarios) {
    const requiresRestore =
      scenario.id === "safety/T1/checkpoint_restore" ||
      (scenario.trajectory === "T3" && scenario.id.endsWith("_ask_allow"));
    if (!requiresRestore) continue;
    const mutatingCalls = callsForScenario(scenario).filter((call) =>
      mutatingTools.has(canonicalToolName(call.name)),
    );
    if (mutatingCalls.length === 0) continue;
    if (!scenario.restore_evidence) {
      fail("matrix_invalid", `${scenario.id} lacks three-state product restore evidence`);
    }
    const expectedPaths = new Set(
      scenario.restore_evidence.paths.map((expectation) => expectation.path),
    );
    for (const call of mutatingCalls) {
      for (const effect of call.disk_effects ?? []) {
        const path = typeof effect === "string" ? effect : effect.path;
        if (!expectedPaths.has(path)) {
          fail("matrix_invalid", `${scenario.id} restore evidence omits changed path ${path}`);
        }
      }
    }
  }
}

export function validateMutatingDeclarations(
  scenarios: readonly ScenarioDefinition[],
  mutatingTools: Set<string>,
  fabricatedEvidence?: { tool: string; reason: string },
): void {
  for (const scenario of scenarios) {
    for (const call of callsForScenario(scenario)) {
      const tool = canonicalToolName(call.name);
      const mutating = mutatingTools.has(tool);
      const hasEffects = call.disk_effects !== undefined;
      const hasEvidence =
        call.non_mutating_evidence !== undefined || fabricatedEvidence?.tool === tool;
      if (mutating && Number(hasEffects) + Number(hasEvidence) !== 1) {
        fail("matrix_invalid", `mutating_classification_invalid:${tool}`, {
          scenario: scenario.id,
          call: call.id,
        });
      }
    }
  }
}

export async function deriveMutatingTools(repoRoot: string): Promise<Set<string>> {
  const toolsRoot = join(repoRoot, "packages", "opencode-plugin", "src", "tools");
  const files: string[] = [];
  async function walk(directory: string): Promise<void> {
    const entries = await readdir(directory, { withFileTypes: true });
    for (const entry of entries) {
      const path = join(directory, entry.name);
      if (entry.isDirectory()) await walk(path);
      else if (entry.isFile() && entry.name.endsWith(".ts")) files.push(path);
    }
  }
  await walk(toolsRoot);
  const derived = new Set<string>();
  for (const path of files.sort()) {
    const source = await readFile(path, "utf8");
    for (const match of source.matchAll(/callToolCall\(\s*ctx,\s*context,\s*["']([a-z_]+)["']/g)) {
      const functionStart = Math.max(
        source.lastIndexOf("execute:", match.index),
        source.lastIndexOf("execute(", match.index),
      );
      const executionPrefix = source.slice(Math.max(0, functionStart), match.index);
      if (/askEditPermission\s*\(|permission:\s*["']edit["']/.test(executionPrefix)) {
        derived.add(canonicalToolName(match[1]));
      }
    }
  }
  const permissionInventory = await readPermissionAskInventory(repoRoot);
  if (permissionInventory.some((operation) => operation.startsWith("bash:"))) derived.add("bash");
  return derived;
}

async function loadMutatingTools(matrixRoot: string): Promise<Set<string>> {
  const record = asRecord(
    await readJson(join(matrixRoot, "mutating-tools.json")).catch(() => undefined),
  );
  const raw = record?.tools ?? record?.mutating_tools;
  if (!Array.isArray(raw) || raw.some((tool) => typeof tool !== "string")) {
    fail("matrix_invalid", "mutating-tools.json is missing or invalid");
  }
  return new Set((raw as string[]).map(canonicalToolName));
}

export async function validateHarnessInputs(options: {
  repoRoot: string;
  scenarios: readonly ScenarioDefinition[];
  pinnedHostVersion: string;
  platform?: NodeJS.Platform;
  env?: NodeJS.ProcessEnv;
  testMode?: boolean;
  observationOnly?: boolean;
  fullRun?: boolean;
}): Promise<ValidatedInputs> {
  const platform = options.platform ?? process.platform;
  const matrixRoot = join(options.repoRoot, "tests", "docker", "opencode2", "matrix");
  const contractRoot = join(options.repoRoot, "tests", "docker", "opencode2", "contract");
  const matrix = await loadApplicabilityMatrix(matrixRoot, options.fullRun === true);
  if (
    matrix &&
    matrix.platform !== platform &&
    !(platform === "darwin" && matrix.platform === "linux")
  ) {
    fail("matrix_invalid", `matrix platform ${matrix.platform} does not match ${platform}`);
  }
  const schemas = await loadToolSchemas(options.repoRoot);
  const projection = options.observationOnly
    ? deriveSchemaProjection(schemas, platform)
    : deriveV2HarnessProjection(options.scenarios, platform);
  const inventory = matrix
    ? validateInventory(matrix, schemas, matrix.platform as NodeJS.Platform, projection)
    : projection;
  const surfaces = await deriveListSurfaces(options.repoRoot);
  let parityAllowlist: ParityAllowlistEntry[] = [];
  let requiredCheckStatus: ValidatedInputs["requiredCheckStatus"] = "NOT MET (advisory only)";
  let configuredMutatingTools: Set<string> | undefined;
  if (matrix) {
    const observationOnly = options.observationOnly === true;
    validateScenarioRows(matrix, options.scenarios, schemas, observationOnly);
    await validateT5Inventory(matrixRoot, matrix, schemas);
    // The set-wide drift guards (every permission operation has its T3
    // triple, every list surface has both T6 fixtures, the parity allowlist
    // is complete) assume the whole scenario set; an observation-only run
    // carries a subset and is not drift.
    if (!observationOnly) {
      await validatePermissionInventory(options.repoRoot, matrixRoot, options.scenarios);
      validateT6(matrix, options.scenarios, surfaces);
      parityAllowlist = await validateParityAllowlist(matrixRoot, options.scenarios);
    }
    requiredCheckStatus = await validateRequiredCheckRecord(matrixRoot);
    configuredMutatingTools = await loadMutatingTools(matrixRoot);
  }
  const derivedMutatingTools = await deriveMutatingTools(options.repoRoot);
  configuredMutatingTools ??= new Set(derivedMutatingTools);
  for (const tool of derivedMutatingTools) {
    if (!configuredMutatingTools.has(tool)) {
      fail("matrix_invalid", `mutating-tools.json omits product mutation permission: ${tool}`);
    }
  }
  for (const tool of configuredMutatingTools) {
    if (!derivedMutatingTools.has(tool)) {
      fail("matrix_invalid", `mutating-tools.json adds non-mutating tool: ${tool}`);
    }
  }
  const override = applyMutatingTestOverride(
    configuredMutatingTools,
    options.env ?? process.env,
    options.testMode === true,
  );
  validateMutatingDeclarations(options.scenarios, override.tools, override.fabricatedEvidence);
  validateRestoreCoverage(options.scenarios, derivedMutatingTools);

  if (!options.observationOnly) {
    await loadHostCliContract(contractRoot, options.pinnedHostVersion);
    await loadHostProviderConfigContract(contractRoot, options.pinnedHostVersion);
    if (options.scenarios.some((scenario) => scenario.error_origin === "host")) {
      await loadHostSchemaRejectionContract(contractRoot, options.pinnedHostVersion);
    }
  }
  const context: HarnessValidationContext = {
    repo_root: options.repoRoot,
    platform: (matrix?.platform ?? platform) as NodeJS.Platform,
    scenarios: options.scenarios,
    matrix,
    pinned_host_version: options.pinnedHostVersion,
  };
  return {
    context,
    matrix,
    inventory,
    mutatingTools: override.tools,
    surfaces,
    requiredCheckStatus,
    parityAllowlist,
  };
}
