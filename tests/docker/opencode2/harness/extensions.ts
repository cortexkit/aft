import { readdir } from "node:fs/promises";
import { join } from "node:path";
import { pathToFileURL } from "node:url";

import type { HarnessExtension } from "./types.js";

export async function loadHarnessExtensions(root: string): Promise<HarnessExtension[]> {
  const paths: string[] = [];
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
        (entry.name.endsWith(".extension.ts") || entry.name.endsWith(".extension.mts"))
      ) {
        paths.push(path);
      }
    }
  }
  await walk(root);
  const extensions = await Promise.all(
    paths.sort().map(async (path) => {
      const loaded = (await import(pathToFileURL(path).href)) as Record<string, unknown>;
      const extension = (loaded.default ?? loaded.extension) as HarnessExtension | undefined;
      if (!extension?.name) throw new Error(`harness extension has no name: ${path}`);
      return extension;
    }),
  );
  const names = extensions.map((extension) => extension.name);
  if (new Set(names).size !== names.length) throw new Error("duplicate harness extension name");
  return extensions;
}
