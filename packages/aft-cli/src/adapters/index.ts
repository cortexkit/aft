import { OmpAdapter } from "./omp.js";
import { OpenCodeAdapter } from "./opencode.js";
import { PiAdapter } from "./pi.js";
import type { HarnessAdapter, HarnessKind } from "./types.js";

export type { HarnessAdapter, HarnessKind } from "./types.js";
export { OmpAdapter, OpenCodeAdapter, PiAdapter };

const ALL: HarnessAdapter[] = [new OpenCodeAdapter(), new PiAdapter(), new OmpAdapter()];

export function getAllAdapters(): HarnessAdapter[] {
  return ALL;
}

export function getAdapter(kind: HarnessKind): HarnessAdapter {
  const found = ALL.find((a) => a.kind === kind);
  if (!found) throw new Error(`Unknown harness: ${kind}`);
  return found;
}

/** Adapters whose host is installed, regardless of whether AFT is registered. */
export function getInstalledAdapters(): HarnessAdapter[] {
  return ALL.filter((a) => a.isInstalled());
}
