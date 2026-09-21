import { mkdir, readdir, rm, stat, writeFile } from "node:fs/promises";
import { join, resolve } from "node:path";

import type { RecordedMockExchange, ScenarioDefinition } from "./types.js";

/**
 * How many run roots the artifact store keeps by default, this run included.
 *
 * Enough to compare a failure with the run before it, and with the one before
 * that, which is what a reader actually reaches for.
 */
export const DEFAULT_ARTIFACT_RETENTION = 3;

export function parseArtifactRetention(value: string | undefined): number {
  if (value === undefined || value === "") return DEFAULT_ARTIFACT_RETENTION;
  const retention = Number(value);
  if (!Number.isSafeInteger(retention) || retention < 1) {
    throw new Error(`AFT_E2E_ARTIFACT_RETAIN must be a positive integer, got ${value}`);
  }
  return retention;
}

/**
 * A run root's age, taken from its own entry and its immediate children.
 *
 * A run writes into `scenarios/` and `forensics/`, not into the root itself,
 * so the root's own timestamp stops moving seconds after the run starts and a
 * long run looks older than it is. The children are where the movement shows.
 */
async function lastTouched(path: string): Promise<number> {
  const own = await stat(path).catch(() => undefined);
  if (!own) return 0;
  let newest = own.mtimeMs;
  const entries = await readdir(path, { withFileTypes: true }).catch(() => []);
  for (const entry of entries) {
    const child = await stat(join(path, entry.name)).catch(() => undefined);
    if (child && child.mtimeMs > newest) newest = child.mtimeMs;
  }
  return newest;
}

/**
 * Keep the artifact root to the newest few run roots.
 *
 * Forensics are worth keeping for the run being read and the couple before it;
 * every run before that is a few hundred megabytes of scenario trees nobody
 * opens again, and nothing else prunes them — which is how an artifact store
 * here reached 26GB. This run's own root is never a candidate, and neither is
 * one that has been written to recently, so a run happening alongside this one
 * keeps its evidence.
 */
export async function pruneOldRunRoots(options: {
  parent: string;
  /** How many run roots survive, including this run's own. */
  keep: number;
  current: string;
  now?: number;
  /** A root touched within this window belongs to a run that may still be going. */
  activeWindowMs?: number;
}): Promise<string[]> {
  const now = options.now ?? Date.now();
  const activeWindowMs = options.activeWindowMs ?? 60 * 60 * 1000;
  const current = resolve(options.current);
  const entries = await readdir(options.parent, { withFileTypes: true }).catch(() => []);
  const roots: Array<{ path: string; touched: number }> = [];
  for (const entry of entries) {
    if (!entry.isDirectory()) continue;
    const path = resolve(options.parent, entry.name);
    if (path === current) continue;
    roots.push({ path, touched: await lastTouched(path) });
  }
  roots.sort((left, right) => right.touched - left.touched);
  // This run's own root is one of the survivors, so the others keep one fewer.
  // A root inside the active window is over quota like any other, but it is
  // never the one removed: something may still be writing to it.
  const removed: string[] = [];
  for (const candidate of roots.slice(Math.max(options.keep - 1, 0))) {
    if (now - candidate.touched < activeWindowMs) continue;
    await rm(candidate.path, { recursive: true, force: true });
    removed.push(candidate.path);
  }
  return removed;
}

export class ScenarioForensics {
  readonly directory: string;

  constructor(runRoot: string, scenarioId: string) {
    this.directory = join(runRoot, "forensics", ...scenarioId.split("/"));
  }

  async initialize(scenario: ScenarioDefinition): Promise<void> {
    await mkdir(this.directory, { recursive: true });
    await this.writeJson("scenario.json", scenario);
  }

  async writeJson(name: string, value: unknown): Promise<void> {
    await writeFile(join(this.directory, name), `${JSON.stringify(value, null, 2)}\n`);
  }

  async writeText(name: string, value: string): Promise<void> {
    await writeFile(join(this.directory, name), value);
  }

  async writeExchanges(exchanges: readonly RecordedMockExchange[]): Promise<void> {
    await writeFile(
      join(this.directory, "mock-exchanges.ndjson"),
      `${exchanges.map((exchange) => JSON.stringify(exchange)).join("\n")}\n`,
    );
  }

  async recordFailure(error: unknown): Promise<void> {
    const record =
      error instanceof Error
        ? { name: error.name, message: error.message, stack: error.stack }
        : { name: "NonError", message: String(error) };
    await this.writeJson("failure.json", record);
  }
}
