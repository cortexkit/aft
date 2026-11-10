import { resolve } from "node:path";

import { fail, type HarnessFailureCode } from "./errors.js";
import type { ReadinessGate, ScriptedTurn } from "./types.js";
import { asRecord } from "./util.js";

/**
 * How long a row's call is held back for the callgraph store. A small fixture
 * builds in well under a second on an idle machine; the budget is for a loaded
 * runner, and running out of it is reported as a failure of its own rather than
 * letting the call through to answer "building".
 */
export const CALLGRAPH_READY_TIMEOUT_MS = 60_000;
const READINESS_POLL_MS = 250;

export interface ReadinessObservation {
  ready: boolean;
  status: unknown;
}

export type ReadinessProbe = (remainingMs: number) => Promise<ReadinessObservation>;
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
  last_status: unknown;
}

/**
 * Poll the product's raw status response, not an index's storage layout.
 * A ready graph for another root must not release this scenario's turn.
 */
export function callgraphReadinessProbe(
  projectRoot: string,
  readStatus: (remainingMs: number) => Promise<unknown>,
): ReadinessProbe {
  return async (remainingMs) => {
    try {
      const response = asRecord(await readStatus(remainingMs));
      const callgraph = asRecord(response?.callgraph_store);
      const rootMatches = [response?.project_root, response?.canonical_root].some(
        (root) => typeof root === "string" && resolve(root) === resolve(projectRoot),
      );
      return {
        ready: response?.success === true && rootMatches && callgraph?.status === "ready",
        status: {
          success: response?.success,
          project_root: response?.project_root,
          canonical_root: response?.canonical_root,
          callgraph_store: callgraph ?? null,
          status: response?.status,
          code: response?.code,
          message: response?.message,
        },
      };
    } catch (error) {
      return {
        ready: false,
        status: {
          status: "unavailable",
          reason: "status_request_failed",
          error: error instanceof Error ? error.message : String(error),
        },
      };
    }
  };
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
    const observation = await probe(Math.max(1, deadline - Date.now()));
    if (observation.ready) {
      return {
        subject: gate.subject,
        turn,
        timeout_ms: gate.timeout_ms,
        waited_ms: Date.now() - startedAt,
        polls,
        last_status: observation.status,
      };
    }
    const remaining = deadline - Date.now();
    if (remaining <= 0) {
      fail(
        NEVER_READY_CODE[gate.subject],
        `${gate.subject} never became ready within ${gate.timeout_ms}ms before turn ${turn}; last status: ${JSON.stringify(observation.status)}`,
        {
          subject: gate.subject,
          turn,
          timeout_ms: gate.timeout_ms,
          polls,
          last_status: observation.status,
        },
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
