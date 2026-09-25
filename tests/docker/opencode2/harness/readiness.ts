import { existsSync } from "node:fs";
import { readdir, readFile } from "node:fs/promises";
import { join } from "node:path";
import { Database } from "bun:sqlite";

import { fail, type HarnessFailureCode } from "./errors.js";
import type { ReadinessGate, ScriptedTurn } from "./types.js";

/**
 * How long a row's call is held back for the callgraph store. A small fixture
 * builds in well under a second on an idle machine; the budget is for a loaded
 * runner, and running out of it is reported as a failure of its own rather than
 * letting the call through to answer "building".
 */
export const CALLGRAPH_READY_TIMEOUT_MS = 60_000;
const READINESS_POLL_MS = 250;

export type ReadinessProbe = () => Promise<boolean>;
export type ReadinessProbes = Record<ReadinessGate["subject"], ReadinessProbe>;

const NEVER_READY_CODE: Record<ReadinessGate["subject"], HarnessFailureCode> = {
  callgraph: "callgraph_never_ready",
};

export interface ReadinessRecord {
  subject: ReadinessGate["subject"];
  turn: string;
  timeout_ms: number;
  waited_ms: number;
  polls: number;
}

/**
 * Whether AFT has published a built callgraph store under `storageDir`.
 *
 * This reads the same on-disk state AFT's own read path accepts as ready: the
 * `<key>.current` pointer in `<storage>/callgraph/<key>/` names a generation
 * database, and that database's `meta` table carries `ready = 1`. A store that
 * is still being built has no pointer yet, or points at nothing marked ready.
 * The scenario owns its storage directory, so any published store there is the
 * fixture project's.
 */
export async function callgraphStorePublished(storageDir: string): Promise<boolean> {
  const root = join(storageDir, "callgraph");
  let keys: string[];
  try {
    keys = await readdir(root);
  } catch (error) {
    if ((error as NodeJS.ErrnoException).code === "ENOENT") return false;
    throw error;
  }
  for (const key of keys) {
    const directory = join(root, key);
    let entries: string[];
    try {
      entries = await readdir(directory);
    } catch {
      continue;
    }
    for (const pointer of entries.filter((entry) => entry.endsWith(".current"))) {
      const generation = (
        await readFile(join(directory, pointer), "utf8").catch(() => "")
      ).trim();
      if (!generation) continue;
      const databasePath = join(directory, generation);
      if (existsSync(databasePath) && storeMarkedReady(databasePath)) return true;
    }
  }
  return false;
}

function storeMarkedReady(databasePath: string): boolean {
  let database: Database | undefined;
  try {
    database = new Database(databasePath, { readonly: true });
    // AFT may still be writing the database; wait for that write instead of
    // failing the read with "database is locked".
    database.exec("PRAGMA busy_timeout = 5000");
    const row = database
      .query<{ v: string }, []>("SELECT v FROM meta WHERE k = 'ready'")
      .get();
    return row?.v === "1";
  } catch {
    // A generation that is mid-publish can be unreadable for a moment; the
    // next poll reads it again.
    return false;
  } finally {
    database?.close();
  }
}

/**
 * Poll `probe` until it reports ready, or fail with the gate's named reason
 * once its budget is spent. The failure is deliberate: a product that never
 * finishes building must fail the row by name instead of being waited out.
 */
export async function awaitReadiness(
  gate: ReadinessGate,
  turn: string,
  probe: ReadinessProbe,
  pollMs = READINESS_POLL_MS,
): Promise<ReadinessRecord> {
  const startedAt = Date.now();
  const deadline = startedAt + gate.timeout_ms;
  let polls = 0;
  for (;;) {
    polls += 1;
    if (await probe()) {
      return {
        subject: gate.subject,
        turn,
        timeout_ms: gate.timeout_ms,
        waited_ms: Date.now() - startedAt,
        polls,
      };
    }
    const remaining = deadline - Date.now();
    if (remaining <= 0) {
      fail(
        NEVER_READY_CODE[gate.subject],
        `${gate.subject} never became ready within ${gate.timeout_ms}ms before turn ${turn}`,
        { subject: gate.subject, turn, timeout_ms: gate.timeout_ms, polls },
      );
    }
    await Bun.sleep(Math.min(pollMs, remaining));
  }
}

/**
 * Hold a turn back until what it declared it needs is ready. A turn that
 * declares nothing returns at once without consulting any probe.
 */
export async function awaitTurnReadiness(
  turn: ScriptedTurn,
  probes: ReadinessProbes,
  pollMs = READINESS_POLL_MS,
): Promise<ReadinessRecord | undefined> {
  const gate = turn.await_ready;
  if (!gate) return undefined;
  return awaitReadiness(gate, turn.label, probes[gate.subject], pollMs);
}

/** Extra host budget a scenario needs so its readiness waits cannot outlast the host. */
export function readinessBudgetMs(turns: readonly ScriptedTurn[]): number {
  return turns.reduce((total, turn) => total + (turn.await_ready?.timeout_ms ?? 0), 0);
}
