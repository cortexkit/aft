import { OpenCodeAdapter } from "./opencode.js";
import { PiAdapter } from "./pi.js";
import type { HarnessAdapter, HarnessKind } from "./types.js";

export type { HarnessAdapter, HarnessKind } from "./types.js";
export { OpenCodeAdapter, PiAdapter };

const ALL: HarnessAdapter[] = [new OpenCodeAdapter(), new PiAdapter()];

export function getAllAdapters(): HarnessAdapter[] {
  return ALL;
}

export function getAdapter(kind: HarnessKind): HarnessAdapter {
  const found = ALL.find((a) => a.kind === kind);
  if (!found) throw new Error(`Unknown harness: ${kind}`);
  return found;
}

/** Adapters whose host binary is on PATH. Order: installed first, then rest. */
export function getAdaptersPreferInstalled(): HarnessAdapter[] {
  return [...ALL].sort((a, b) => {
    const aa = a.isInstalled() ? 0 : 1;
    const bb = b.isInstalled() ? 0 : 1;
    return aa - bb;
  });
}

/** Adapters that have AFT registered (either via plugin entry or `pi install`). */
export function getAdaptersWithPluginRegistered(): HarnessAdapter[] {
  return ALL.filter((a) => a.hasPluginEntry());
}

/** Adapters whose host is installed, regardless of whether AFT is registered. */
export function getInstalledAdapters(): HarnessAdapter[] {
  return ALL.filter((a) => a.isInstalled());
}
