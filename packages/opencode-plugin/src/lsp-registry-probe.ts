/**
 * Probes the npm registry for a package's available versions and selects
 * the newest one that satisfies AFT's supply-chain grace window.
 *
 * Threat model: defends against typosquat/compromise attacks where a
 * malicious version is published, then yanked within hours when caught.
 * By default, AFT only installs versions that have been on the registry
 * for at least 7 days, giving the community time to detect and respond.
 *
 * Grace days are configurable via `lsp.grace_days` (default 7). User
 * pins via `lsp.versions: { "package": "X.Y.Z" }` bypass the filter.
 */

import { warn } from "./logger.js";

const NPM_REGISTRY_BASE = "https://registry.npmjs.org";

/** Per-version publish times indexed by version string. */
type VersionTimes = Record<string, string>;

interface RegistryResponse {
  /** Map: version → ISO publish time. Also contains "created" / "modified". */
  time?: VersionTimes;
  /** dist-tags such as "latest", "next". */
  "dist-tags"?: { latest?: string };
}

export interface VersionPickResult {
  /** The chosen version, or null if none qualifies. */
  version: string | null;
  /** True when the registry has versions but none is older than `graceDays`. */
  blockedByGrace: boolean;
  /** All eligible versions sorted by publish date (newest first). For tests/logs. */
  eligible: ReadonlyArray<{ version: string; publishedAt: string }>;
}

/**
 * Pick a version from the registry response that:
 *   1. is published at least `graceDays` ago
 *   2. is the newest such version (per ISO publish time)
 *
 * Pre-release versions (semver "-" tags) are skipped.
 */
export function pickEligibleVersion(
  response: RegistryResponse,
  graceDays: number,
  now: number = Date.now(),
): VersionPickResult {
  const times = response.time || {};
  const cutoff = now - graceDays * 24 * 60 * 60 * 1000;

  // Filter out reserved keys (`created`, `modified`) and pre-releases.
  const candidates: Array<{ version: string; publishedAt: string; ts: number }> = [];
  for (const [version, publishedAt] of Object.entries(times)) {
    if (version === "created" || version === "modified") continue;
    if (version.includes("-")) continue; // skip pre-releases
    if (typeof publishedAt !== "string") continue;
    const ts = Date.parse(publishedAt);
    if (Number.isNaN(ts)) continue;
    candidates.push({ version, publishedAt, ts });
  }

  // Sort newest-first.
  candidates.sort((a, b) => b.ts - a.ts);

  const eligible = candidates.filter((c) => c.ts <= cutoff);
  const blockedByGrace = candidates.length > 0 && eligible.length === 0;

  return {
    version: eligible[0]?.version ?? null,
    blockedByGrace,
    eligible: eligible.map(({ version, publishedAt }) => ({ version, publishedAt })),
  };
}

/**
 * Fetch the registry document for `npmPackage` and apply the grace filter.
 *
 * Returns `null` on HTTP/network failure (logs a warning) so callers can
 * fall back to "use whatever's currently installed".
 */
export async function probeRegistry(
  npmPackage: string,
  graceDays: number,
  fetchImpl: typeof fetch = fetch,
): Promise<VersionPickResult | null> {
  // Scoped packages need their `/` URL-encoded once.
  const encoded = encodeURIComponent(npmPackage).replace(/^%40/, "@");
  const url = `${NPM_REGISTRY_BASE}/${encoded}`;
  try {
    const res = await fetchImpl(url, {
      headers: { accept: "application/json" },
      // Short timeout — registry probes happen at startup; don't block users.
      signal: AbortSignal.timeout(10_000),
    });
    if (!res.ok) {
      warn(`[lsp] registry probe failed for ${npmPackage}: HTTP ${res.status}`);
      return null;
    }
    const json = (await res.json()) as RegistryResponse;
    return pickEligibleVersion(json, graceDays);
  } catch (err) {
    warn(`[lsp] registry probe failed for ${npmPackage}: ${err}`);
    return null;
  }
}
