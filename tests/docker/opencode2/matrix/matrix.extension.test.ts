import { describe, expect, test } from "bun:test";
import { readdir, readFile } from "node:fs/promises";
import { join } from "node:path";

import { compareApplicabilityAssembly, validateMatrixAssembly } from "./matrix.extension.js";

type JsonRecord = Record<string, unknown>;

const root = import.meta.dir;
const scenariosRoot = join(root, "../scenarios");

async function json(path: string): Promise<JsonRecord> {
  return JSON.parse(await readFile(path, "utf8")) as JsonRecord;
}

async function inputs(): Promise<{
  aggregate: JsonRecord;
  fragments: Array<{ source: string; value: JsonRecord }>;
}> {
  const aggregate = await json(join(root, "applicability.json"));
  const entries = await readdir(scenariosRoot, { withFileTypes: true });
  const fragments = await Promise.all(
    entries
      .filter((entry) => entry.isDirectory())
      .toSorted((left, right) => left.name.localeCompare(right.name))
      .map(async (entry) => {
        const source = join(scenariosRoot, entry.name, "matrix.json");
        return { source, value: await json(source) };
      }),
  );
  return { aggregate, fragments };
}

function rows(document: JsonRecord): Array<{ tool: string; trajectories: Record<string, string> }> {
  return document.rows as Array<{ tool: string; trajectories: Record<string, string> }>;
}

describe("OpenCode 2 matrix assembly", () => {
  test("assembled matrix matches every slice-owned row", async () => {
    await validateMatrixAssembly();
  });

  test("a slice row added without an aggregate row is rejected by name", async () => {
    const { aggregate, fragments } = await inputs();
    rows(aggregate).splice(
      rows(aggregate).findIndex((row) => row.tool === "write"),
      1,
    );
    expect(() => compareApplicabilityAssembly(aggregate, fragments)).toThrow(
      "slice matrix row missing from aggregate: write",
    );
  });

  test("an aggregate row whose slice row was removed is rejected by name", async () => {
    const { aggregate, fragments } = await inputs();
    const write = fragments.find(({ value }) => rows(value).some((row) => row.tool === "write"));
    if (!write) throw new Error("write slice fixture is missing");
    write.value.rows = [];
    expect(() => compareApplicabilityAssembly(aggregate, fragments)).toThrow(
      "aggregate matrix row has no owning slice: write",
    );
  });

  test("a trajectory classification drift is rejected by cell identity", async () => {
    const { aggregate, fragments } = await inputs();
    const write = rows(aggregate).find((row) => row.tool === "write");
    if (!write) throw new Error("write aggregate fixture is missing");
    write.trajectories.T6 = "applicable";
    expect(() => compareApplicabilityAssembly(aggregate, fragments)).toThrow(
      "applicability drift: write/T6",
    );
  });
});
