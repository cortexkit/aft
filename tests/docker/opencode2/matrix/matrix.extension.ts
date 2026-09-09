import { readdir, readFile } from "node:fs/promises";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

import { TRAJECTORIES, type HarnessExtension } from "../harness/types.js";

interface MatrixRow {
  tool: string;
  trajectories: Record<string, string>;
}

interface MatrixDocument {
  schema_version: number;
  platform: string;
  rows: MatrixRow[];
}

interface MatrixFragment {
  source: string;
  document: MatrixDocument;
}

const matrixRoot = dirname(fileURLToPath(import.meta.url));
const scenarioRoot = join(matrixRoot, "../scenarios");

function parseDocument(value: unknown, source: string): MatrixDocument {
  if (typeof value !== "object" || value === null) {
    throw new Error(`${source}: applicability document must be an object`);
  }
  const document = value as Partial<MatrixDocument>;
  if (document.schema_version !== 1 || document.platform !== "linux" || !Array.isArray(document.rows)) {
    throw new Error(`${source}: applicability document header is invalid`);
  }
  for (const row of document.rows) {
    if (typeof row?.tool !== "string" || typeof row.trajectories !== "object") {
      throw new Error(`${source}: applicability row is invalid`);
    }
    for (const trajectory of TRAJECTORIES) {
      if (typeof row.trajectories[trajectory] !== "string") {
        throw new Error(`${source}: ${row.tool}/${trajectory} is missing`);
      }
    }
  }
  return document as MatrixDocument;
}

function rowsByTool(rows: readonly MatrixRow[], source: string): Map<string, MatrixRow> {
  const result = new Map<string, MatrixRow>();
  for (const row of rows) {
    if (result.has(row.tool)) throw new Error(`${source}: duplicate applicability row: ${row.tool}`);
    result.set(row.tool, row);
  }
  return result;
}

export function compareApplicabilityAssembly(
  aggregateValue: unknown,
  fragmentValues: readonly { source: string; value: unknown }[],
): void {
  const aggregate = parseDocument(aggregateValue, "matrix/applicability.json");
  const aggregateRows = rowsByTool(aggregate.rows, "matrix/applicability.json");
  const fragmentRows = new Map<string, { row: MatrixRow; source: string }>();

  for (const fragmentValue of fragmentValues) {
    const fragment = parseDocument(fragmentValue.value, fragmentValue.source);
    for (const row of fragment.rows) {
      const previous = fragmentRows.get(row.tool);
      if (previous) {
        throw new Error(
          `duplicate slice applicability row: ${row.tool} (${previous.source}, ${fragmentValue.source})`,
        );
      }
      fragmentRows.set(row.tool, { row, source: fragmentValue.source });
    }
  }

  for (const tool of [...fragmentRows.keys()].sort()) {
    if (!aggregateRows.has(tool)) throw new Error(`slice matrix row missing from aggregate: ${tool}`);
  }
  for (const tool of [...aggregateRows.keys()].sort()) {
    if (!fragmentRows.has(tool)) throw new Error(`aggregate matrix row has no owning slice: ${tool}`);
  }

  for (const [tool, { row: fragmentRow }] of [...fragmentRows.entries()].sort(([left], [right]) =>
    left.localeCompare(right),
  )) {
    const aggregateRow = aggregateRows.get(tool) as MatrixRow;
    for (const trajectory of TRAJECTORIES) {
      if (aggregateRow.trajectories[trajectory] !== fragmentRow.trajectories[trajectory]) {
        throw new Error(
          `applicability drift: ${tool}/${trajectory} aggregate=${aggregateRow.trajectories[trajectory]} slice=${fragmentRow.trajectories[trajectory]}`,
        );
      }
    }
  }
}

async function readJson(path: string): Promise<unknown> {
  return JSON.parse(await readFile(path, "utf8")) as unknown;
}

async function loadFragments(): Promise<MatrixFragment[]> {
  const entries = await readdir(scenarioRoot, { withFileTypes: true });
  const fragments: MatrixFragment[] = [];
  for (const entry of entries.toSorted((left, right) => left.name.localeCompare(right.name))) {
    if (!entry.isDirectory()) continue;
    const source = join(scenarioRoot, entry.name, "matrix.json");
    fragments.push({ source, document: parseDocument(await readJson(source), source) });
  }
  return fragments;
}

export async function validateMatrixAssembly(): Promise<void> {
  const fragments = await loadFragments();
  compareApplicabilityAssembly(
    await readJson(join(matrixRoot, "applicability.json")),
    fragments.map(({ source, document }) => ({ source, value: document })),
  );
}

const extension: HarnessExtension = {
  name: "opencode2-matrix-assembly-v1",
  validate: validateMatrixAssembly,
};

export default extension;
