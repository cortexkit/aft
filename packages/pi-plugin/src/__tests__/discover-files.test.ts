/**
 * Unit tests for source-file discovery helpers.
 */

/// <reference path="../bun-test.d.ts" />

import { afterEach, describe, expect, test } from "bun:test";
import { mkdir, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join, relative } from "node:path";
import { discoverSourceFiles } from "../shared/discover-files.js";

const roots: string[] = [];

async function makeRoot(): Promise<string> {
  const root = join(tmpdir(), `aft-pi-discover-${process.pid}-${roots.length}-${Date.now()}`);
  roots.push(root);
  await mkdir(root, { recursive: true });
  return root;
}

afterEach(async () => {
  await Promise.all(roots.splice(0).map((root) => rm(root, { recursive: true, force: true })));
});

describe("discoverSourceFiles", () => {
  test("finds supported source and docs files while skipping noise directories", async () => {
    const root = await makeRoot();
    await mkdir(join(root, "src"), { recursive: true });
    await mkdir(join(root, "node_modules", "pkg"), { recursive: true });
    await mkdir(join(root, ".git"), { recursive: true });
    await writeFile(join(root, "src", "app.ts"), "export const ok = true;\n");
    await writeFile(join(root, "README.md"), "# docs\n");
    await writeFile(join(root, "src", "image.png"), "not source\n");
    await writeFile(join(root, "node_modules", "pkg", "index.ts"), "export {};\n");
    await writeFile(join(root, ".git", "config"), "ignored\n");

    const discovered = (await discoverSourceFiles(root)).map((file) => relative(root, file));

    expect(discovered).toEqual(["README.md", "src/app.ts"]);
  });

  test("honors maxFiles to avoid unbounded project walks", async () => {
    const root = await makeRoot();
    await writeFile(join(root, "a.ts"), "export const a = 1;\n");
    await writeFile(join(root, "b.ts"), "export const b = 1;\n");
    await writeFile(join(root, "c.ts"), "export const c = 1;\n");

    const discovered = await discoverSourceFiles(root, 2);

    expect(discovered).toHaveLength(2);
    expect(discovered).toEqual([...discovered].sort());
  });

  test("unreadable or missing roots return an empty list instead of throwing", async () => {
    const root = await makeRoot();

    expect(await discoverSourceFiles(join(root, "missing"))).toEqual([]);
  });
});
