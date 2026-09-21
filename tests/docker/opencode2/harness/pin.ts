import { readFile } from "node:fs/promises";
import { join } from "node:path";

import { fail } from "./errors.js";

/**
 * The V1 host version the image installs, read from the file the runner reads.
 *
 * `run-opencode2-test.sh` builds the image with this same file as
 * `OPENCODE1_VERSION`, so reading it here keeps the contract check and the
 * installed binary on one source instead of two that can drift apart.
 */
export async function readPinnedV1HostVersion(repoRoot: string): Promise<string> {
  const source = join(repoRoot, ".github", "opencode-version.txt");
  const text = await readFile(source, "utf8").catch(() => "");
  const version = text.trim();
  if (!version) fail("contract_uncaptured", "pinned OpenCode 1 version is missing", { source }, true);
  return version;
}

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
    fail("contract_uncaptured", "pinned OpenCode 2 GA source is missing", { source }, true);
  return match[1];
}
