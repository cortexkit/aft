import { afterEach, describe, expect, test } from "bun:test";
import { mkdtemp, readFile, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { markProject } from "../../../../tests/helpers/project.js";

const roots: string[] = [];

async function fixture(): Promise<string> {
  const root = await mkdtemp(join(tmpdir(), "aft-project-fixture-"));
  roots.push(root);
  return root;
}

afterEach(async () => {
  await Promise.all(roots.splice(0).map((root) => rm(root, { recursive: true, force: true })));
});

describe("shared project fixture evidence", () => {
  test("creates an empty root build file", async () => {
    const root = await fixture();
    await markProject(root);
    expect(await readFile(join(root, "Makefile"), "utf8")).toBe("");
  });

  test("preserves an existing build file and can be called repeatedly", async () => {
    const root = await fixture();
    const content = "all:\n\t@echo fixture\n";
    await writeFile(join(root, "Makefile"), content);
    await markProject(root);
    await markProject(root);
    expect(await readFile(join(root, "Makefile"), "utf8")).toBe(content);
  });

  test("does not hide setup errors", async () => {
    const root = await fixture();
    await expect(markProject(join(root, "missing"))).rejects.toThrow();
  });
});
