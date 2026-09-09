import { readFile } from "node:fs/promises";
import { join } from "node:path";

import { fail } from "./errors.js";
import type { ScenarioDefinition } from "./types.js";
import { asRecord, readJson } from "./util.js";

export interface PermissionSubcaseMapEntry {
  tool: string;
  subcase_prefix: string;
}

export interface PermissionOperationRow {
  operation: string;
  classification: "applicable" | `n/a:${string}`;
  issue?: string;
}

export async function readPermissionAskInventory(repoRoot: string): Promise<string[]> {
  const path = join(repoRoot, "packages", "opencode-plugin", "src", "tools", "hoisted", "v2.ts");
  const source = await readFile(path, "utf8");
  const body = source.match(/V2_PERMISSION_ASK_INVENTORY\s*=\s*\[(?<body>[\s\S]*?)\]\s*as const/)
    ?.groups?.body;
  if (!body) fail("matrix_invalid", "V2_PERMISSION_ASK_INVENTORY cannot be parsed", { path }, true);
  const operations = [...body.matchAll(/["']([^"']+)["']/g)].map((match) => match[1]);
  if (operations.length === 0 || new Set(operations).size !== operations.length) {
    fail("matrix_invalid", "V2_PERMISSION_ASK_INVENTORY is empty or duplicated", {}, true);
  }
  return operations;
}

function parseSubcaseMap(value: unknown): Map<string, PermissionSubcaseMapEntry> {
  const record = asRecord(value);
  const raw = asRecord(record?.operations ?? record);
  if (!raw) fail("matrix_invalid", "inventory-subcase-map.json must be an object");
  const parsed = new Map<string, PermissionSubcaseMapEntry>();
  const identities = new Map<string, string>();
  for (const [operation, entryValue] of Object.entries(raw)) {
    if (operation === "schema_version") continue;
    const entry = asRecord(entryValue);
    if (typeof entry?.tool !== "string" || typeof entry.subcase_prefix !== "string") {
      fail("matrix_invalid", `inventory-subcase-map entry is invalid: ${operation}`);
    }
    const identity = `${entry.tool}/${entry.subcase_prefix}`;
    const previous = identities.get(identity);
    if (previous) {
      fail(
        "matrix_invalid",
        `duplicate source operations ${previous},${operation} map to ${identity}`,
      );
    }
    identities.set(identity, operation);
    parsed.set(operation, { tool: entry.tool, subcase_prefix: entry.subcase_prefix });
  }
  return parsed;
}

function parseOperations(value: unknown): Map<string, PermissionOperationRow> {
  const record = asRecord(value);
  const rawRows = record?.operations ?? record?.rows;
  const rows = new Map<string, PermissionOperationRow>();
  if (Array.isArray(rawRows)) {
    for (const raw of rawRows) {
      const row = asRecord(raw);
      if (typeof row?.operation !== "string" || typeof row.classification !== "string") {
        fail("matrix_invalid", "operations.json row is invalid");
      }
      if (rows.has(row.operation))
        fail("matrix_invalid", `duplicate operation row: ${row.operation}`);
      rows.set(row.operation, {
        operation: row.operation,
        classification: row.classification as PermissionOperationRow["classification"],
        issue: typeof row.issue === "string" ? row.issue : undefined,
      });
    }
  } else {
    const rowMap = asRecord(rawRows ?? record);
    if (!rowMap) fail("matrix_invalid", "operations.json must contain rows");
    for (const [operation, raw] of Object.entries(rowMap)) {
      if (operation === "schema_version") continue;
      if (typeof raw === "string") {
        rows.set(operation, {
          operation,
          classification: raw as PermissionOperationRow["classification"],
        });
      } else {
        const row = asRecord(raw);
        if (typeof row?.classification !== "string")
          fail("matrix_invalid", `operation row invalid: ${operation}`);
        rows.set(operation, {
          operation,
          classification: row.classification as PermissionOperationRow["classification"],
          issue: typeof row.issue === "string" ? row.issue : undefined,
        });
      }
    }
  }
  return rows;
}

export async function validatePermissionInventory(
  repoRoot: string,
  matrixRoot: string,
  scenarios: readonly ScenarioDefinition[],
): Promise<void> {
  const inventory = await readPermissionAskInventory(repoRoot);
  const subcaseMap = parseSubcaseMap(
    await readJson(join(matrixRoot, "inventory-subcase-map.json")),
  );
  const operations = parseOperations(await readJson(join(matrixRoot, "operations.json")));
  for (const operation of inventory) {
    if (!subcaseMap.has(operation))
      fail("matrix_invalid", `permission operation missing subcase map: ${operation}`);
    if (!operations.has(operation))
      fail("matrix_invalid", `permission operation missing ledger row: ${operation}`);
  }
  for (const operation of subcaseMap.keys()) {
    if (!inventory.includes(operation))
      fail("matrix_invalid", `unknown source operation in subcase map: ${operation}`);
  }
  for (const operation of operations.keys()) {
    if (!inventory.includes(operation))
      fail("matrix_invalid", `unknown source operation in ledger: ${operation}`);
  }

  const t3Ids = new Set(
    scenarios.filter((scenario) => scenario.trajectory === "T3").map((scenario) => scenario.id),
  );
  const expected = new Set<string>();
  for (const operation of inventory) {
    const ledger = operations.get(operation) as PermissionOperationRow;
    if (ledger.classification === "applicable") {
      const mapped = subcaseMap.get(operation) as PermissionSubcaseMapEntry;
      for (const suffix of ["ask_allow", "ask_deny", "config_deny"]) {
        expected.add(`${mapped.tool}/T3/${mapped.subcase_prefix}_${suffix}`);
      }
    } else if (ledger.classification.startsWith("n/a:")) {
      if (!ledger.issue) fail("matrix_invalid", `n/a operation requires issue: ${operation}`);
    } else {
      fail("matrix_invalid", `operation classification is invalid: ${operation}`);
    }
  }
  for (const id of expected)
    if (!t3Ids.has(id)) fail("matrix_invalid", `T3 subcase missing: ${id}`);
  for (const id of t3Ids)
    if (!expected.has(id)) fail("matrix_invalid", `unexpected T3 subcase: ${id}`);

  const bashIds = [...t3Ids].filter((id) => id.startsWith("bash/T3/")).sort();
  const requiredBash = ["fallback", "loop"].flatMap((path) =>
    ["ask_allow", "ask_deny", "config_deny"].map((suffix) => `bash/T3/${path}_${suffix}`),
  );
  if (JSON.stringify(bashIds) !== JSON.stringify(requiredBash.sort())) {
    fail("matrix_invalid", `bash T3 identity set is invalid: ${bashIds.join(",")}`);
  }
  for (const scenario of scenarios.filter((candidate) => candidate.id.startsWith("bash/T3/"))) {
    const fallback = scenario.id.includes("/fallback_");
    const hostFallback = asRecord(scenario.project_config?.bash)?.host_fallback;
    if (fallback !== (hostFallback === true)) {
      fail("matrix_invalid", `${scenario.id} path must be selected by bash.host_fallback`);
    }
  }
}
