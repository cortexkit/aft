import { readFile } from "node:fs/promises";
import { join } from "node:path";

import { fail } from "./errors.js";

export async function readPinnedHostVersion(repoRoot: string): Promise<string> {
  const source = join(
    repoRoot,
    "packages",
    "opencode-plugin",
    "test",
    "load-matrix",
    "load-matrix.ts",
  );
  const text = await readFile(source, "utf8");
  const match = text.match(/const\s+v2Version\s*=\s*["']([^"']+)["']/);
  if (!match?.[1])
    fail("contract_uncaptured", "pinned OpenCode 2 beta source is missing", { source }, true);
  return match[1];
}
