import { getAdapter, getAllAdapters, getInstalledAdapters } from "../adapters/index.js";
import type { HarnessAdapter, HarnessKind } from "../adapters/types.js";
import { log, selectMany, selectOne } from "./prompts.js";

function parseHarnessFlag(argv: string[]): HarnessKind | null {
  const idx = argv.indexOf("--harness");
  if (idx === -1 || idx === argv.length - 1) return null;
  const value = argv[idx + 1];
  if (value === "opencode" || value === "pi") return value;
  return null;
}

/**
 * Resolve which adapter(s) to act on.
 *   - `--harness opencode|pi` → single adapter (hard override)
 *   - otherwise: installed hosts, with interactive prompts when ambiguous
 *     - 0 installed → prompt user to pick (give install hints)
 *     - 1 installed → use it silently
 *     - 2+ installed → prompt multiselect
 */
export async function resolveAdaptersForCommand(
  argv: string[],
  options: {
    /** Allow the user to select multiple harnesses at once. Setup defaults to single. */
    allowMulti: boolean;
    /** Verb used in prompts ("setup" / "diagnose"). */
    verb: string;
  },
): Promise<HarnessAdapter[]> {
  const flag = parseHarnessFlag(argv);
  if (flag) return [getAdapter(flag)];

  const installed = getInstalledAdapters();
  if (installed.length === 0) {
    // None installed — still let the user pick one so setup can give them
    // install instructions for that harness.
    log.warn("No supported harness was detected on PATH (opencode, pi).");
    const pick = await selectOne("Which harness do you want to configure?", [
      {
        label: "OpenCode",
        value: "opencode" as HarnessKind,
        hint: "@cortexkit/aft-opencode",
      },
      {
        label: "Pi",
        value: "pi" as HarnessKind,
        hint: "@cortexkit/aft-pi",
      },
    ]);
    return [getAdapter(pick)];
  }

  if (installed.length === 1) {
    log.info(`Detected ${installed[0].displayName} — using it for ${options.verb}.`);
    return installed;
  }

  if (!options.allowMulti) {
    const pick = await selectOne(
      `Multiple harnesses detected — which one to ${options.verb}?`,
      installed.map((adapter) => ({
        label: adapter.displayName,
        value: adapter.kind,
        hint: adapter.pluginPackageName,
      })),
    );
    return [getAdapter(pick)];
  }

  const picks = await selectMany<HarnessKind>(
    `Multiple harnesses detected — ${options.verb} which ones?`,
    installed.map((adapter) => ({
      label: adapter.displayName,
      value: adapter.kind,
      hint: adapter.pluginPackageName,
    })),
    installed.map((a) => a.kind),
  );
  return picks.map((kind) => getAdapter(kind));
}

/** Return every known adapter regardless of install state (for doctor --issue). */
export function getAllRegistryAdapters(): HarnessAdapter[] {
  return getAllAdapters();
}
